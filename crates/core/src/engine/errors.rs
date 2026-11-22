//! Shared storage-error → protocol-error mapping for engine verbs.
//!
//! Write verbs funnel storage failures through [`map_write_storage_error`]
//! so one taxonomy decides which failures are caller-fixable.

use crate::error::ProtocolError;
use crate::publication::PublicationError;
use crate::{RuleClass, StorageError, StorageRule};

/// Map a refused publication capture onto the public protocol surface.
///
/// - an oversized export, a failed export and a listenable write on the
///   receipt-only route are things the CALLER (or the spiice author) can
///   fix, and read as `invalid_argument`;
/// - an exhausted outbox is backpressure on a legal write, and reads as
///   `capacity_exhausted` (MCP `-32000`, REST 503) so a caller backs off
///   instead of opening an incident (docs/host-integration.md);
/// - an unbound source is the deployment's, and reads as `internal`.
#[must_use]
pub(crate) fn map_publication_error(err: &PublicationError) -> ProtocolError {
    match err {
        PublicationError::PayloadTooLarge { .. }
        | PublicationError::ExportFailed(_)
        | PublicationError::UntypedListenableWrite { .. } => {
            ProtocolError::invalid_argument("publication", err.to_string())
        }
        PublicationError::CapacityExhausted { .. } => {
            ProtocolError::capacity_exhausted(err.to_string())
        }
        PublicationError::SourceUnbound { .. } => ProtocolError::internal(err.to_string()),
    }
}

/// Map a write-verb storage failure onto the public protocol surface.
/// `field` labels the caller-fixable `ConstraintViolation`/`Conflict`
/// messages (`invalid_argument`); `not_found_message` names the missing
/// row without echoing storage internals.
/// [`map_write_storage_error`] for a Fact ingest: the field and the not-found message every Fact
/// write path answers with.
pub(in crate::engine) fn map_fact_ingest_error(err: StorageError) -> ProtocolError {
    map_write_storage_error(err, "fact", "fact ingest referenced row not found")
}

pub(crate) fn map_write_storage_error(
    err: StorageError,
    field: &str,
    not_found_message: &str,
) -> ProtocolError {
    match err {
        StorageError::NotFound => ProtocolError::not_found(not_found_message),
        StorageError::ScopeMissing { kind, id } => ProtocolError::scope_not_registered(&kind, id),
        StorageError::IdempotencyConflict {
            request_id,
            existing,
        } => ProtocolError::idempotency_conflict(request_id, existing),
        StorageError::ReplayChanged(drift) => ProtocolError::replay_changed(drift),
        StorageError::ConstraintViolation(message) | StorageError::Conflict(message) => {
            ProtocolError::invalid_argument(field, message)
        }
        StorageError::GoalChildrenOpen { .. } => {
            ProtocolError::invalid_argument(field, err.to_string())
        }
        StorageError::Refused { rule, detail } => match rule.class() {
            RuleClass::Invalid | RuleClass::Conflict => {
                let field = if rule == StorageRule::ReadCommittedRequired {
                    "transaction_isolation"
                } else {
                    field
                };
                ProtocolError::invalid_argument(field, detail)
            }
            RuleClass::Forbidden => ProtocolError::forbidden(detail),
            RuleClass::Deployment => ProtocolError::internal(detail),
        },
        StorageError::Forbidden(message) => ProtocolError::forbidden(message),
        StorageError::UnreadableRead { .. } => ProtocolError::forbidden(err.to_string()),
        StorageError::Suppressed(message) => ProtocolError::suppressed(message),
        StorageError::PublicationRefused(ref publication) => map_publication_error(publication),
        // A transient deadlock/serialization failure that outlived the
        // bounded storage retry surfaces as an internal (retry-later)
        // fault.
        StorageError::Retryable(message)
        | StorageError::Unavailable(message)
        | StorageError::Internal(message) => ProtocolError::internal(message),
        StorageError::SchemaResetRequired { details }
        | StorageError::MigrationRequired { details } => ProtocolError::internal(details),
        StorageError::InvalidStoredInterpretation { .. } => {
            ProtocolError::internal(err.to_string())
        }
        StorageError::MemoryIdCollision => ProtocolError::internal("memory UUID collision"),
    }
}

/// Read-path collapse: reads surface every storage failure as `Internal`
/// with the verb name as context (a read has no caller-fixable storage
/// failure class to distinguish).
pub(in crate::engine) fn internal_storage_error(
    context: &str,
    err: &StorageError,
) -> ProtocolError {
    ProtocolError::internal(format!("{context}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn storage_errors_match_ledger_vectors() {
        #[derive(serde::Deserialize)]
        struct Vector {
            #[serde(rename = "in")]
            input: [String; 2],
            out: String,
        }

        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/ledger_vectors.json"))
                .expect("ledger vectors are valid JSON");
        let rows: Vec<Vector> = serde_json::from_value(
            vectors["Proxiima.DatabaseAuthority.storageErrorCategory"].clone(),
        )
        .expect("the ledger emits the storage error decision");

        assert_eq!(rows.len(), 4);
        for Vector {
            input: [verb, outcome],
            out,
        } in rows
        {
            let errors = match outcome.as_str() {
                "conflictOrConstraint" => vec![
                    StorageError::Conflict("conflict".into()),
                    StorageError::ConstraintViolation("constraint".into()),
                ],
                "exhaustedTransient" => vec![StorageError::Retryable("transient".into())],
                _ => panic!("unknown storage outcome {outcome}"),
            };
            let expected = match out.as_str() {
                "invalidArgument" => ErrorCode::InvalidArgument,
                "internal" => ErrorCode::Internal,
                _ => panic!("unknown protocol failure {out}"),
            };
            for error in errors {
                let actual = match verb.as_str() {
                    "read" => internal_storage_error("read", &error),
                    "write" => map_write_storage_error(error, "write", "missing"),
                    _ => panic!("unknown storage verb {verb}"),
                };
                assert_eq!(actual.code, expected, "{verb} {outcome}");
            }
        }
    }
}
