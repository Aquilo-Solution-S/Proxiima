//! The dispatch seam's errors as [`Problem`] documents: which status,
//! `type` and `title` each one answers.
//!
//! Two rules carry the whole module and neither is negotiable:
//!
//! - `detail` is [`ToolError::client_message`], never `Display` and never
//!   a formatted source chain. That single call is what keeps an
//!   internal-kind error redacted to `internal server error` before the HTTP
//!   layer can see the storage DSN inside it, and what lets `Unavailable`
//!   through verbatim because it is a caller-actionable precondition.
//! - The status map matches on **variants**, exhaustively, with no wildcard
//!   arm. `ToolErrorKind` is too coarse for HTTP — its `InvalidRequest`
//!   bucket holds authorization, conflict and capability failures that must
//!   not share a status — and [`ToolInvocationError::kind`] maps
//!   `ToolNotFound` to `InvalidInput`, a `400` where HTTP wants `404`.
//!   Adding an error variant is therefore a compile error until someone
//!   chooses its status.

use axum::http::StatusCode;
use proxiima_core::ToolError;
use proxiima_core::error::ErrorCode;
use proxiima_core::{RuleClass, StorageError};

use crate::problem::Problem;
use crate::server::ToolInvocationError;

/// Map one dispatch failure onto its problem document.
///
/// `ToolInvocationError` carries no `client_message` of its own: its two
/// non-`Tool` variants are produced by this crate from a name the caller
/// supplied, so there is no internal detail to redact. The `Tool` arm — the
/// only one that can carry a storage or protocol fault — goes through
/// [`ToolError::client_message`].
#[must_use]
pub fn problem_for(err: &ToolInvocationError, instance: &str) -> Problem {
    match err {
        ToolInvocationError::NotAuthorized(name) => Problem::new(
            StatusCode::FORBIDDEN,
            "not-authorized",
            "Not authorized",
            ToolError::NotAuthorized(name.clone()).client_message(),
            instance,
        ),
        ToolInvocationError::ToolNotFound(name) => Problem::new(
            StatusCode::NOT_FOUND,
            "tool-not-found",
            "Tool not found",
            format!("tool {name} not found"),
            instance,
        ),
        ToolInvocationError::Tool(inner) => tool_problem(inner, instance),
    }
}

fn tool_problem(err: &ToolError, instance: &str) -> Problem {
    let (status, slug, title) = match err {
        ToolError::InvalidInput(_) => (StatusCode::BAD_REQUEST, "invalid-input", "Invalid input"),
        ToolError::NotFound(_) => (StatusCode::NOT_FOUND, "not-found", "Not found"),
        ToolError::NotAuthorized(_) => (StatusCode::FORBIDDEN, "not-authorized", "Not authorized"),
        ToolError::LayeringViolation(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "layering-violation",
            "Layering violation",
        ),
        ToolError::Unavailable(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Capability unavailable",
        ),
        ToolError::StoreUnavailable { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "store-unavailable",
            "Store unavailable",
        ),
        ToolError::Failure(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "tool-failure",
            "Tool failed",
        ),
        ToolError::Protocol(inner) => protocol_status(inner.code),
        ToolError::Storage(inner) => storage_status(inner),
        ToolError::Other(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Internal error",
        ),
    };
    // The redaction seam. `Other` and the internal storage variants collapse
    // to a generic string here; nothing else in this module may reach for
    // `Display`.
    let mut problem = Problem::new(status, slug, title, err.client_message(), instance);
    if let Some(existing) = idempotency_existing(err) {
        problem = problem.with_existing(existing);
    }
    problem
}

fn idempotency_existing(err: &ToolError) -> Option<uuid::Uuid> {
    match err {
        ToolError::Protocol(inner) => inner.existing,
        ToolError::Storage(StorageError::IdempotencyConflict { existing, .. }) => *existing,
        _ => None,
    }
}

const fn protocol_status(code: ErrorCode) -> (StatusCode, &'static str, &'static str) {
    match code {
        ErrorCode::AuthRequired => (
            StatusCode::UNAUTHORIZED,
            "auth-required",
            "Authentication required",
        ),
        ErrorCode::Forbidden => (StatusCode::FORBIDDEN, "forbidden", "Forbidden"),
        ErrorCode::UnknownSchema => (StatusCode::BAD_REQUEST, "unknown-schema", "Unknown schema"),
        ErrorCode::InvalidArgument => (
            StatusCode::BAD_REQUEST,
            "invalid-argument",
            "Invalid argument",
        ),
        ErrorCode::NotFound => (StatusCode::NOT_FOUND, "not-found", "Not found"),
        ErrorCode::ToolNotRegistered => (
            StatusCode::NOT_FOUND,
            "tool-not-registered",
            "Tool not registered",
        ),
        ErrorCode::AlreadyIngested => {
            (StatusCode::CONFLICT, "already-ingested", "Already ingested")
        }
        ErrorCode::IdempotencyConflict => (
            StatusCode::CONFLICT,
            "idempotency-conflict",
            "Idempotency conflict",
        ),
        ErrorCode::TriggerConflict => {
            (StatusCode::CONFLICT, "trigger-conflict", "Trigger conflict")
        }
        ErrorCode::DuplicateTriggerInRequest => (
            StatusCode::CONFLICT,
            "duplicate-trigger-in-request",
            "Duplicate trigger in request",
        ),
        // 409, not 451. Suppression is a erasure primitive (13) and is not
        // necessarily a legal hold; 451 would overclaim.
        ErrorCode::Suppressed => (StatusCode::CONFLICT, "suppressed", "Suppressed"),
        // 503, as for the storage-level refusal below: retry later.
        ErrorCode::CapacityExhausted => (
            StatusCode::SERVICE_UNAVAILABLE,
            "capacity-exhausted",
            "Capacity exhausted",
        ),
        ErrorCode::Internal => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Internal error",
        ),
    }
}

fn storage_status(err: &StorageError) -> (StatusCode, &'static str, &'static str) {
    match err {
        StorageError::ConstraintViolation(_)
        | StorageError::GoalChildrenOpen { .. }
        | StorageError::GoalChildrenAllAbandoned => {
            (StatusCode::BAD_REQUEST, "invalid-input", "Invalid input")
        }
        StorageError::Refused { rule, .. } => match rule.class() {
            RuleClass::Invalid => (StatusCode::BAD_REQUEST, "invalid-input", "Invalid input"),
            RuleClass::Conflict => (StatusCode::CONFLICT, "conflict", "Conflict"),
            RuleClass::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Refused by a storage rule",
            ),
            RuleClass::Deployment => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal error",
            ),
        },
        StorageError::Forbidden(_) => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "Membership authorization refused",
        ),
        StorageError::UnreadableRead { .. } => {
            (StatusCode::FORBIDDEN, "forbidden", "Read set not readable")
        }
        StorageError::NotFound => (StatusCode::NOT_FOUND, "not-found", "Not found"),
        StorageError::ScopeMissing { .. } => (
            StatusCode::NOT_FOUND,
            "scope-not-registered",
            "Scope not registered",
        ),
        StorageError::Conflict(_) => (StatusCode::CONFLICT, "conflict", "Conflict"),
        StorageError::IdempotencyConflict { .. } | StorageError::ReplayChanged(_) => (
            StatusCode::CONFLICT,
            "idempotency-conflict",
            "Idempotency conflict",
        ),
        StorageError::Suppressed(_) => (StatusCode::CONFLICT, "suppressed", "Suppressed"),
        // A refused publication capture keeps the write path's own split:
        // an unexportable or oversized payload and a listenable write on
        // the receipt-only route are the caller's to fix; an unbound
        // source and an exhausted outbox are the deployment's.
        StorageError::PublicationRefused(publication) => match publication {
            proxiima_core::publication::PublicationError::PayloadTooLarge { .. }
            | proxiima_core::publication::PublicationError::ExportFailed(_)
            | proxiima_core::publication::PublicationError::UntypedListenableWrite { .. } => {
                (StatusCode::BAD_REQUEST, "invalid-input", "Invalid input")
            }
            // 503, not 500: the write was legal and the publisher is
            // behind, so this is "retry later", which is what `503` means
            // and what `500` does not. Distinct `type` slug so a client
            // switches on the code rather than on the status alone, and
            // `detail` keeps the backlog depth because `CapacityExhausted`
            // is its own `ToolErrorKind` and is therefore not redacted.
            proxiima_core::publication::PublicationError::CapacityExhausted { .. } => (
                StatusCode::SERVICE_UNAVAILABLE,
                "capacity-exhausted",
                "Capacity exhausted",
            ),
            proxiima_core::publication::PublicationError::SourceUnbound { .. } => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Internal error",
            ),
        },
        StorageError::Retryable(_)
        | StorageError::Unavailable(_)
        | StorageError::Internal(_)
        | StorageError::MemoryIdCollision
        | StorageError::SchemaResetRequired { .. }
        | StorageError::MigrationRequired { .. } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Internal error",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use proxiima_core::StorageRule;
    use proxiima_core::error::ProtocolError;

    fn status_of(err: &ToolInvocationError) -> StatusCode {
        problem_for(err, "/v1/tools/x").into_response().status()
    }

    fn refused(rule: StorageRule) -> StorageError {
        StorageError::Refused {
            rule,
            detail: "x".into(),
        }
    }

    fn protocol(code: ErrorCode) -> ToolInvocationError {
        ToolInvocationError::Tool(ToolError::Protocol(ProtocolError {
            code,
            message: "boom".into(),
            request_id: None,
            existing: None,
        }))
    }

    #[test]
    fn protocol_codes_match_the_documented_table() {
        for (code, expected) in [
            (ErrorCode::AuthRequired, StatusCode::UNAUTHORIZED),
            (ErrorCode::Forbidden, StatusCode::FORBIDDEN),
            (ErrorCode::UnknownSchema, StatusCode::BAD_REQUEST),
            (ErrorCode::InvalidArgument, StatusCode::BAD_REQUEST),
            (ErrorCode::NotFound, StatusCode::NOT_FOUND),
            (ErrorCode::ToolNotRegistered, StatusCode::NOT_FOUND),
            (ErrorCode::AlreadyIngested, StatusCode::CONFLICT),
            (ErrorCode::IdempotencyConflict, StatusCode::CONFLICT),
            (ErrorCode::TriggerConflict, StatusCode::CONFLICT),
            (ErrorCode::DuplicateTriggerInRequest, StatusCode::CONFLICT),
            (ErrorCode::Suppressed, StatusCode::CONFLICT),
            (
                ErrorCode::CapacityExhausted,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (ErrorCode::Internal, StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            assert_eq!(status_of(&protocol(code)), expected, "protocol {code:?}");
        }
    }

    #[test]
    fn a_refused_rule_maps_by_its_class() {
        for (err, expected) in [
            (
                refused(StorageRule::DerivedMemoryPinsNothing),
                StatusCode::BAD_REQUEST,
            ),
            (refused(StorageRule::GroupCycle), StatusCode::CONFLICT),
            (
                refused(StorageRule::MigrationLaneRequired),
                StatusCode::FORBIDDEN,
            ),
            (
                refused(StorageRule::SidecarNotDeclared),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            assert_eq!(
                status_of(&ToolInvocationError::Tool(ToolError::Storage(err))),
                expected
            );
        }
    }

    #[test]
    fn variant_map_matches_the_documented_table() {
        assert_eq!(
            status_of(&ToolInvocationError::ToolNotFound("core_nope".into())),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&ToolInvocationError::NotAuthorized("core_transfer".into())),
            StatusCode::FORBIDDEN
        );
        for (err, expected) in [
            (ToolError::InvalidInput("x".into()), StatusCode::BAD_REQUEST),
            (ToolError::NotFound("x".into()), StatusCode::NOT_FOUND),
            (ToolError::NotAuthorized("x".into()), StatusCode::FORBIDDEN),
            (
                ToolError::LayeringViolation("x".into()),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                ToolError::Unavailable("x".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (ToolError::NotFound("x".into()), StatusCode::NOT_FOUND),
            (
                ToolError::Unavailable("x".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ToolError::Failure("x".into()),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                ToolError::Other("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            assert_eq!(
                status_of(&ToolInvocationError::Tool(err)),
                expected,
                "variant map drifted"
            );
        }

        for (err, expected) in [
            (
                StorageError::ConstraintViolation("x".into()),
                StatusCode::BAD_REQUEST,
            ),
            (StorageError::NotFound, StatusCode::NOT_FOUND),
            (StorageError::Conflict("x".into()), StatusCode::CONFLICT),
            (
                StorageError::IdempotencyConflict {
                    request_id: "r".into(),
                    existing: None,
                },
                StatusCode::CONFLICT,
            ),
            (StorageError::Suppressed("x".into()), StatusCode::CONFLICT),
            (
                StorageError::Retryable("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                StorageError::Unavailable("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                StorageError::Internal("x".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                StorageError::MemoryIdCollision,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                StorageError::SchemaResetRequired {
                    details: "x".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                StorageError::MigrationRequired {
                    details: "x".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            assert_eq!(
                status_of(&ToolInvocationError::Tool(ToolError::Storage(err))),
                expected
            );
        }
    }

    /// Declared backpressure is a `503` with its own `type`, and its
    /// `detail` keeps the backlog depth: a client that reads `500
    /// internal` cannot tell a full outbox from a broken substrate, and
    /// retries either the same way.
    /// #480: an other store that did not answer is a retryable 503 with its
    /// own type, not a 404.
    #[test]
    fn an_unanswered_store_is_a_503_with_its_own_type() {
        let err = ToolInvocationError::Tool(ToolError::StoreUnavailable { store: 1 });
        let problem = problem_for(&err, "/v1/tools/core_get_memory");
        assert_eq!(status_of(&err), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            problem.to_json()["type"],
            "https://proxiima.dev/errors/store-unavailable"
        );
    }

    #[test]
    fn an_exhausted_outbox_is_a_503_with_its_own_type() {
        let err = ToolInvocationError::Tool(ToolError::Storage(StorageError::PublicationRefused(
            proxiima_core::publication::PublicationError::CapacityExhausted {
                pending: 100_000,
                max: 100_000,
            },
        )));
        let problem = problem_for(&err, "/v1/tools/core_remember").to_json();
        assert_eq!(status_of(&err), StatusCode::SERVICE_UNAVAILABLE);
        let detail = problem["detail"].as_str().expect("detail is a string");
        assert!(detail.contains("100000"), "{detail}");
        assert_eq!(
            problem["type"],
            "https://proxiima.dev/errors/capacity-exhausted"
        );

        // The other four publication refusals keep the statuses they had.
        for (refusal, expected) in [
            (
                proxiima_core::publication::PublicationError::PayloadTooLarge { bytes: 9, max: 8 },
                StatusCode::BAD_REQUEST,
            ),
            (
                proxiima_core::publication::PublicationError::ExportFailed("nope".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                proxiima_core::publication::PublicationError::UntypedListenableWrite {
                    schema_id: "x/y-v1".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                proxiima_core::publication::PublicationError::SourceUnbound {
                    schema_id: "x/y-v1".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            assert_eq!(
                status_of(&ToolInvocationError::Tool(ToolError::Storage(
                    StorageError::PublicationRefused(refusal)
                ))),
                expected
            );
        }
    }

    #[test]
    fn document_carries_the_rfc_9457_members() {
        let problem = problem_for(
            &ToolInvocationError::NotAuthorized("core_transfer:to_owner".into()),
            "/v1/tools/core_transfer/to_owner",
        );
        let json = problem.to_json();
        assert_eq!(json["type"], "https://proxiima.dev/errors/not-authorized");
        assert_eq!(json["title"], "Not authorized");
        assert_eq!(json["status"], 403);
        assert_eq!(
            json["detail"],
            "tool core_transfer:to_owner not authorized for this MCP token"
        );
        assert_eq!(json["instance"], "/v1/tools/core_transfer/to_owner");
    }
}
