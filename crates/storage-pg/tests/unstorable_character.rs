//! #687: PostgreSQL stores U+0000 in neither `text` nor `jsonb`. A write
//! that carries one is the caller's input, refused as a constraint
//! violation without the driver's sentence, and writes nothing.
#![allow(clippy::missing_errors_doc)]

use proxiima_core::storage_ports::{FactIngestPort, MemoryReadPort, OwnerWritePermit};
use proxiima_core::test_fixtures::owner_scope_for_tests;
use proxiima_core::verbs::fact_ingest::{AuthorizedFactWrite, FactWriteCommand};
use proxiima_core::{MemoryKind, OwnerId, SchemaId, SchemaVersion, StorageError, UserId};
use proxiima_storage_pg::test_fixtures::fresh_pg;
use uuid::Uuid;

#[tokio::test]
async fn a_nul_in_stored_text_is_refused_and_writes_nothing()
-> Result<(), Box<dyn std::error::Error>> {
    let (pg, _db) = fresh_pg("proxiima_unstorable_character").await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let draft = FactWriteCommand {
        schema_id: SchemaId::new("core/test-fact-v1".into()),
        schema_version: SchemaVersion::new(1),
        handle: None,
        source_id: None,
        ingest_key: None,
        payload: Vec::new(),
        rendered_text: Some("page one\u{0}page two".into()),
        lexical_language: None,
        receipt: None,
        occurred_at: None,
        citation: None,
        additional_references: Vec::new(),
        refs: Vec::new(),
        blob_id: None,
        kind: MemoryKind::Fact,
        request: None,
    };
    let authorized = AuthorizedFactWrite::new_for_tests(
        OwnerWritePermit::new_for_tests(owner, MemoryKind::Fact),
        draft,
        None,
        Vec::new(),
    );
    let refused = pg
        .ingest_authorized_fact_atomic(&authorized, &[])
        .await
        .expect_err("text holding U+0000 cannot be stored");
    assert!(
        matches!(&refused, StorageError::ConstraintViolation(message)
            if message.contains("SQLSTATE 22021") && !message.contains("0x00")),
        "{refused:?}"
    );
    let counts = pg
        .count_memories(Some(&owner_scope_for_tests(&[owner])), &[owner])
        .await?;
    assert_eq!(counts.fact, 0, "the refused write left nothing behind");
    Ok(())
}

#[tokio::test]
async fn a_nul_escape_in_jsonb_is_a_constraint_violation() {
    let (pg, _db) = fresh_pg("proxiima_unstorable_jsonb").await;
    let error = sqlx::query("SELECT $1::text::jsonb")
        .bind(r#"{"page":"a\u0000b"}"#)
        .execute(pg.pool_for_tests())
        .await
        .expect_err("jsonb cannot hold U+0000");
    let mapped = proxiima_storage_pg::map_err(error);
    assert!(
        matches!(&mapped, StorageError::ConstraintViolation(message)
            if message.contains("SQLSTATE 22P05")),
        "{mapped:?}"
    );
}
