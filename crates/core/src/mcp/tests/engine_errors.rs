use crate::ToolError;
use crate::engine::errors::{map_publication_error, map_write_storage_error};
use crate::error::ErrorCode;
use crate::mcp::ToolErrorKind;
use crate::publication::PublicationError;
use crate::{ReplayDrift, StorageError};

/// Every shipped write reaches the client through `Engine`, so a
/// refusal must read the same there as on the direct storage path:
/// one class per refusal, whichever channel carries it.
#[test]
fn engine_and_storage_paths_classify_each_publication_refusal_alike() {
    for refusal in [
        PublicationError::PayloadTooLarge { bytes: 9, max: 8 },
        PublicationError::CapacityExhausted { pending: 3, max: 3 },
        PublicationError::ExportFailed("nope".into()),
        PublicationError::UntypedListenableWrite {
            schema_id: "x/y-v1".into(),
        },
        PublicationError::SourceUnbound {
            schema_id: "x/y-v1".into(),
        },
    ] {
        let engine = ToolError::Protocol(map_publication_error(&refusal));
        let storage = ToolError::Storage(StorageError::PublicationRefused(refusal));
        assert_eq!(engine.kind(), storage.kind(), "for {storage}");
    }
}

/// A replay that declared something else is an idempotency conflict on
/// both channels, whichever drift it was.
#[test]
fn a_changed_replay_is_an_idempotency_conflict_on_both_paths() {
    for drift in [
        ReplayDrift::FactRefs,
        ReplayDrift::DerivedRefs,
        ReplayDrift::DerivedIndexRows,
        ReplayDrift::DerivedExternalEdges,
    ] {
        let engine = map_write_storage_error(StorageError::ReplayChanged(drift), "f", "nf");
        assert_eq!(engine.code, ErrorCode::IdempotencyConflict, "{drift}");
        assert!(engine.message.starts_with(&drift.to_string()), "{engine}");
        let storage = ToolError::Storage(StorageError::ReplayChanged(drift));
        assert_eq!(
            ToolError::Protocol(engine).kind(),
            storage.kind(),
            "{drift}"
        );
    }
}

/// Backpressure is retry-later, not an incident, and the caller keeps
/// the backlog depth it needs to decide how long to wait.
#[test]
fn exhausted_outbox_reads_as_capacity_exhausted_with_its_depth() {
    let err = map_publication_error(&PublicationError::CapacityExhausted { pending: 3, max: 3 });
    assert_eq!(err.code, ErrorCode::CapacityExhausted);

    let err = ToolError::Protocol(err);
    assert_eq!(err.kind(), ToolErrorKind::CapacityExhausted);
    assert!(err.client_message().contains("3 pending, limit 3"), "{err}");
}
