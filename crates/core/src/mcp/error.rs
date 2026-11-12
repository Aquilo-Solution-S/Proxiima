use crate::ToolError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorKind {
    InvalidInput,
    /// Well-formed reference to a missing (or invisible) entity.
    NotFound,
    InvalidRequest,
    /// Declared backpressure: the request was well-formed and legal, the
    /// deployment is momentarily unable to accept it, and the caller should
    /// slow down and retry. Its own kind so a client's retry and alerting
    /// policy can tell "the outbox is full" from "the substrate is broken";
    /// [`Self::Internal`] would page an operator for a queue depth.
    ///
    /// NOT the class of [`ToolError::Unavailable`], which is a
    /// PERMANENT missing-capability precondition (no embedding client is
    /// configured) that retrying will never resolve — that stays
    /// [`Self::InvalidRequest`].
    CapacityExhausted,
    /// Another store did not answer. Retryable, like
    /// [`Self::CapacityExhausted`], and never "not found": the memory may
    /// well exist. NOT the class of [`ToolError::Unavailable`], which is
    /// permanent.
    StoreUnavailable,
    Internal,
}

impl ToolError {
    #[must_use]
    pub fn kind(&self) -> ToolErrorKind {
        match self {
            Self::InvalidInput(_) => ToolErrorKind::InvalidInput,
            Self::NotFound(_) => ToolErrorKind::NotFound,
            Self::NotAuthorized(_) | Self::LayeringViolation(_) | Self::Unavailable(_) => {
                ToolErrorKind::InvalidRequest
            }
            Self::Protocol(e) => match e.code {
                crate::error::ErrorCode::InvalidArgument => ToolErrorKind::InvalidInput,
                crate::error::ErrorCode::NotFound => ToolErrorKind::NotFound,
                crate::error::ErrorCode::Internal => ToolErrorKind::Internal,
                crate::error::ErrorCode::CapacityExhausted => ToolErrorKind::CapacityExhausted,
                crate::error::ErrorCode::AuthRequired
                | crate::error::ErrorCode::Forbidden
                | crate::error::ErrorCode::UnknownSchema
                | crate::error::ErrorCode::AlreadyIngested
                | crate::error::ErrorCode::IdempotencyConflict
                | crate::error::ErrorCode::ToolNotRegistered
                | crate::error::ErrorCode::TriggerConflict
                | crate::error::ErrorCode::DuplicateTriggerInRequest
                | crate::error::ErrorCode::Suppressed => ToolErrorKind::InvalidRequest,
            },
            Self::Storage(storage) => match storage {
                crate::StorageError::ConstraintViolation(_)
                | crate::StorageError::GoalChildrenOpen { .. }
                | crate::StorageError::GoalChildrenAllAbandoned => ToolErrorKind::InvalidInput,
                crate::StorageError::Refused { rule, .. } => match rule.class() {
                    crate::RuleClass::Invalid => ToolErrorKind::InvalidInput,
                    crate::RuleClass::Conflict | crate::RuleClass::Forbidden => {
                        ToolErrorKind::InvalidRequest
                    }
                    crate::RuleClass::Deployment => ToolErrorKind::Internal,
                },
                crate::StorageError::NotFound | crate::StorageError::ScopeMissing { .. } => {
                    ToolErrorKind::NotFound
                }
                crate::StorageError::Forbidden(_)
                | crate::StorageError::UnreadableRead { .. }
                | crate::StorageError::Conflict(_)
                | crate::StorageError::Suppressed(_)
                | crate::StorageError::IdempotencyConflict { .. }
                | crate::StorageError::ReplayChanged(_) => ToolErrorKind::InvalidRequest,
                // A refused publication capture keeps the split the write
                // path already draws: an oversized or unexportable payload
                // and a listenable write on the receipt-only route are the
                // caller's (or the spiice author's) to fix; an unbound
                // source and an exhausted outbox are the deployment's.
                crate::StorageError::PublicationRefused(publication) => match publication {
                    crate::publication::PublicationError::PayloadTooLarge { .. }
                    | crate::publication::PublicationError::ExportFailed(_)
                    | crate::publication::PublicationError::UntypedListenableWrite { .. } => {
                        ToolErrorKind::InvalidInput
                    }
                    crate::publication::PublicationError::SourceUnbound { .. } => {
                        ToolErrorKind::Internal
                    }
                    // Backpressure, not a fault: the write was legal and the
                    // publisher is behind. A caller told "internal server
                    // error" learns nothing and retries immediately.
                    crate::publication::PublicationError::CapacityExhausted { .. } => {
                        ToolErrorKind::CapacityExhausted
                    }
                },
                crate::StorageError::Retryable(_)
                | crate::StorageError::Unavailable(_)
                | crate::StorageError::Internal(_)
                | crate::StorageError::MemoryIdCollision
                | crate::StorageError::SchemaResetRequired { .. }
                | crate::StorageError::MigrationRequired { .. } => ToolErrorKind::Internal,
            },
            Self::StoreUnavailable { .. } => ToolErrorKind::StoreUnavailable,
            Self::Failure(_) => ToolErrorKind::InvalidRequest,
            Self::Other(_) => ToolErrorKind::Internal,
        }
    }

    #[must_use]
    pub fn client_message(&self) -> String {
        if let Self::NotAuthorized(name) = self {
            return format!("tool {name} not authorized for this MCP token");
        }
        match self.kind() {
            ToolErrorKind::InvalidInput
            | ToolErrorKind::NotFound
            | ToolErrorKind::InvalidRequest
            // Verbatim: the message carries the backlog depth and the
            // configured bound, which is exactly what a caller needs to
            // decide how long to wait.
            | ToolErrorKind::CapacityExhausted
            // Verbatim: names the store number and nothing from the port.
            | ToolErrorKind::StoreUnavailable => self.to_string(),
            ToolErrorKind::Internal => "internal server error".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolError, ToolErrorKind};

    #[test]
    fn unavailable_message_reaches_caller_verbatim() {
        let err = ToolError::Unavailable(
            "semantic search unavailable: no embedding client is configured for this host".into(),
        );
        // Precondition faults classify as a well-formed-but-illegal request,
        // NOT an internal fault (which would redact the message).
        assert_eq!(err.kind(), ToolErrorKind::InvalidRequest);
        assert_eq!(
            err.client_message(),
            "semantic search unavailable: no embedding client is configured for this host"
        );
        assert_ne!(err.client_message(), "internal server error");
    }

    /// The other four refusals keep the classes they already had, so the
    /// new kind is one condition rather than a bucket.
    #[test]
    fn the_other_publication_refusals_keep_their_classes() {
        use crate::publication::PublicationError;

        for (refusal, expected) in [
            (
                PublicationError::PayloadTooLarge { bytes: 9, max: 8 },
                ToolErrorKind::InvalidInput,
            ),
            (
                PublicationError::ExportFailed("nope".into()),
                ToolErrorKind::InvalidInput,
            ),
            (
                PublicationError::UntypedListenableWrite {
                    schema_id: "x/y-v1".into(),
                },
                ToolErrorKind::InvalidInput,
            ),
            (
                PublicationError::SourceUnbound {
                    schema_id: "x/y-v1".into(),
                },
                ToolErrorKind::Internal,
            ),
        ] {
            let err = ToolError::Storage(crate::StorageError::PublicationRefused(refusal));
            assert_eq!(err.kind(), expected, "for {err}");
        }
    }

    /// Another store that did not answer is its own retryable kind,
    /// distinct from `NotFound` (an answer) and `Unavailable` (permanent).
    #[test]
    fn an_unanswered_store_is_store_unavailable_and_verbatim() {
        let err = ToolError::StoreUnavailable { store: 7 };
        assert_eq!(err.kind(), ToolErrorKind::StoreUnavailable);
        assert_eq!(err.client_message(), "store 7 unavailable");
    }

    #[test]
    fn not_found_and_unavailable_reach_the_caller_verbatim() {
        let not_found = ToolError::NotFound("repo not found".into());
        assert_eq!(not_found.kind(), ToolErrorKind::NotFound);
        assert_eq!(not_found.client_message(), "repo not found");

        let unavailable = ToolError::Unavailable("semantic search unavailable".into());
        assert_eq!(unavailable.kind(), ToolErrorKind::InvalidRequest);
        assert_eq!(unavailable.client_message(), "semantic search unavailable");
        assert_ne!(unavailable.client_message(), "internal server error");
    }
}
