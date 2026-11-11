//! The typed reads on the caller's owner transaction, against Postgres.
//!
//! Each read is shown for a live memory, a forgotten memory and a memory of
//! another owner: row security on the caller's transaction decides what is
//! visible, whichever owner the `owner` argument names.

use proxiima_core::storage_ports::{MemoryAuthoringPort, OwnerWritePermit};
use proxiima_core::test_fixtures::{authenticated_context, owner_scope_for_tests};
use proxiima_core::{
    AccessKind, AuthPath, AuthzContext, EdgeEndpoint, EmbeddingDim, EmbeddingSpace, EntityKind,
    MemoryId, MemoryKind, OwnerId, OwnerScope, Role, UserId,
};
use proxiima_pg_testkit::DbGuard;
use proxiima_storage_pg::test_fixtures::{TestMemory, TestNote, create_embedding_of, fresh_pg};
use proxiima_storage_pg::{
    MemoryEmbedding, PgStorage, begin_owner_transaction, cited_objects_present, lexical_languages,
    lock_memory_shared, memories_citing, memory_embeddings, memory_versions, series_versions,
};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

struct World {
    pg: PgStorage,
    _db: DbGuard,
    owner: OwnerId,
    other: OwnerId,
}

async fn world(prefix: &str) -> World {
    let (pg, db) = fresh_pg(prefix).await;
    World {
        pg,
        _db: db,
        owner: OwnerId::from(UserId::new(Uuid::now_v7())),
        other: OwnerId::from(UserId::new(Uuid::now_v7())),
    }
}

/// The scope of a caller holding `role` on `owner`.
fn scope_with(owner: OwnerId, role: Role) -> OwnerScope {
    authenticated_context(AuthzContext::for_subject_with_role(
        UserId::new(Uuid::now_v7()),
        [(owner, role)],
        AuthPath::HostBearer,
    ))
    .owner_scope()
    .expect("sealed roles")
    .clone()
}

impl World {
    /// A caller-held transaction scoped to `scope_owner`.
    async fn tx(&self, scope_owner: OwnerId) -> Transaction<'static, Postgres> {
        self.tx_in(&owner_scope_for_tests(&[scope_owner])).await
    }

    async fn tx_in(&self, scope: &OwnerScope) -> Transaction<'static, Postgres> {
        begin_owner_transaction(&self.pg.clone_pool_for_backend(), scope)
            .await
            .expect("owner transaction")
    }

    async fn write(&self, owner: OwnerId, memory: TestMemory) -> (MemoryId, Uuid) {
        let admitted = memory.create(&self.pg, owner).await.expect("write");
        (admitted.memory_id, admitted.handle)
    }

    async fn forget(&self, owner: OwnerId, memory_id: MemoryId) {
        let permit = OwnerWritePermit::new_for_tests(owner, AccessKind::Fact);
        MemoryAuthoringPort::forget_memory(&self.pg, &permit, memory_id)
            .await
            .expect("forget");
    }

    /// A Fact of `owner` citing the object `(core/bytes-v1, [seed; 32])`; the
    /// admission records the object, and its id is returned.
    async fn cite(&self, owner: OwnerId, seed: u8) -> (MemoryId, Uuid) {
        let cited = TestMemory::fact()
            .cites("core/bytes-v1", [seed; 32])
            .create(&self.pg, owner)
            .await
            .expect("citing write");
        (
            cited.memory_id,
            cited.cited_object_id.expect("cited object"),
        )
    }

    /// Give `memory_id` one vector of `width` in the space of `model`, written
    /// by the embedding write path.
    async fn give_vector(&self, owner: OwnerId, memory_id: MemoryId, model: &str, width: usize) {
        let mut values = vec![0.0; width];
        values[0] = 1.0;
        let version = create_embedding_of(&self.pg, owner, memory_id, model, vec![values])
            .await
            .expect("embedding write");
        assert!(version > 0, "the vector was written");
    }
}

fn space(model: &str, dim: EmbeddingDim) -> EmbeddingSpace {
    EmbeddingSpace::new(model, dim)
}

#[tokio::test]
async fn memory_versions_answer_for_a_live_memory_and_mark_its_head() {
    let w = world("owner_reads_versions").await;
    let (first, handle) = w.write(w.owner, TestMemory::fact()).await;
    let (second, same_handle) = w.write(w.owner, TestMemory::fact().handle(handle)).await;
    assert_eq!(handle, same_handle);
    let mut tx = w.tx(w.owner).await;
    let found = memory_versions(&mut tx, &w.owner, &[second, first])
        .await
        .unwrap();
    let ids: Vec<_> = found.iter().map(|v| v.memory_id).collect();
    assert_eq!(ids, vec![first, second], "ordered by t");
    assert!(
        found
            .iter()
            .all(|v| v.handle == handle && v.kind == MemoryKind::Fact)
    );
    assert_eq!(found.iter().filter(|v| v.head).count(), 1);
    assert!(found[1].head, "the newest version is the head");
    assert_eq!(found[0].schema_id.as_str(), "core/test-fact-v1");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn memory_versions_omit_forgotten_and_foreign_memories() {
    let w = world("owner_reads_versions_hidden").await;
    let (live, _) = w.write(w.owner, TestMemory::fact()).await;
    let (gone, _) = w.write(w.owner, TestMemory::fact()).await;
    let (foreign, _) = w.write(w.other, TestMemory::fact()).await;
    w.forget(w.owner, gone).await;
    let mut tx = w.tx(w.owner).await;
    let found = memory_versions(&mut tx, &w.owner, &[live, gone, foreign])
        .await
        .unwrap();
    assert_eq!(
        found.iter().map(|v| v.memory_id).collect::<Vec<_>>(),
        vec![live]
    );
    let asking_for_other = memory_versions(&mut tx, &w.other, &[foreign])
        .await
        .unwrap();
    assert!(
        asking_for_other.is_empty(),
        "the owner argument does not widen the scope"
    );
    tx.rollback().await.unwrap();
    let mut other_tx = w.tx(w.other).await;
    let hidden = memory_versions(&mut other_tx, &w.owner, &[live])
        .await
        .unwrap();
    assert!(hidden.is_empty());
    other_tx.rollback().await.unwrap();
}

#[tokio::test]
async fn series_versions_list_a_series_without_forgotten_or_foreign_versions() {
    let w = world("owner_reads_series").await;
    let (first, handle) = w.write(w.owner, TestMemory::fact()).await;
    let (second, _) = w.write(w.owner, TestMemory::fact().handle(handle)).await;
    let (third, _) = w.write(w.owner, TestMemory::fact().handle(handle)).await;
    let mut tx = w.tx(w.owner).await;
    let census = series_versions(&mut tx, &w.owner, handle).await.unwrap();
    assert_eq!(
        census.iter().map(|v| v.memory_id).collect::<Vec<_>>(),
        vec![first, second, third]
    );
    assert_eq!(census.iter().filter(|v| v.head).count(), 1);
    tx.rollback().await.unwrap();

    w.forget(w.owner, second).await;
    let mut tx = w.tx(w.owner).await;
    let census = series_versions(&mut tx, &w.owner, handle).await.unwrap();
    assert_eq!(
        census.iter().map(|v| v.memory_id).collect::<Vec<_>>(),
        vec![first, third]
    );
    let asking_for_other = series_versions(&mut tx, &w.other, handle).await.unwrap();
    assert!(asking_for_other.is_empty());
    tx.rollback().await.unwrap();
    let mut other_tx = w.tx(w.other).await;
    assert!(
        series_versions(&mut other_tx, &w.owner, handle)
            .await
            .unwrap()
            .is_empty()
    );
    other_tx.rollback().await.unwrap();
}

async fn probe_update_lock(w: &World, memory_id: MemoryId) -> Result<(), sqlx::Error> {
    let mut probe = w.pg.pool_for_tests().begin().await?;
    let result = sqlx::query("SELECT 1 FROM proxiima_core.memory WHERE t = $1 FOR UPDATE NOWAIT")
        .bind(memory_id.into_inner())
        .fetch_all(&mut *probe)
        .await
        .map(|_| ());
    probe.rollback().await?;
    result
}

#[tokio::test]
async fn lock_memory_shared_holds_the_row_until_the_caller_ends_its_transaction() {
    let w = world("owner_reads_lock").await;
    let (memory_id, handle) = w.write(w.owner, TestMemory::fact()).await;
    let mut tx = w.tx(w.owner).await;
    let locked = lock_memory_shared(&mut tx, &w.owner, memory_id)
        .await
        .unwrap()
        .expect("visible memory");
    assert_eq!(
        (locked.memory_id, locked.handle, locked.head),
        (memory_id, handle, true)
    );
    let blocked = probe_update_lock(&w, memory_id)
        .await
        .expect_err("row is locked");
    let code = blocked
        .as_database_error()
        .and_then(|e| e.code().map(|c| c.to_string()));
    assert_eq!(code.as_deref(), Some("55P03"), "{blocked}");
    tx.rollback().await.unwrap();
    probe_update_lock(&w, memory_id)
        .await
        .expect("lock released");
}

#[tokio::test]
async fn lock_memory_shared_finds_neither_forgotten_nor_foreign_memories() {
    let w = world("owner_reads_lock_hidden").await;
    let (live, _) = w.write(w.owner, TestMemory::fact()).await;
    let (gone, _) = w.write(w.owner, TestMemory::fact()).await;
    let (foreign, _) = w.write(w.other, TestMemory::fact()).await;
    w.forget(w.owner, gone).await;
    let mut tx = w.tx(w.owner).await;
    assert!(
        lock_memory_shared(&mut tx, &w.owner, gone)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        lock_memory_shared(&mut tx, &w.other, foreign)
            .await
            .unwrap()
            .is_none()
    );
    tx.rollback().await.unwrap();
    let mut other_tx = w.tx(w.other).await;
    assert!(
        lock_memory_shared(&mut other_tx, &w.owner, live)
            .await
            .unwrap()
            .is_none()
    );
    other_tx.rollback().await.unwrap();
}

/// A row lock is checked against the write policy too, so a caller that may
/// only read is not answered by the lock; the plain read answers it.
#[tokio::test]
async fn lock_memory_shared_needs_write_authority_over_the_memory() {
    let w = world("owner_reads_lock_reader").await;
    let (memory_id, _) = w.write(w.owner, TestMemory::fact()).await;
    let mut tx = w.tx_in(&scope_with(w.owner, Role::viewer())).await;
    assert!(
        lock_memory_shared(&mut tx, &w.owner, memory_id)
            .await
            .unwrap()
            .is_none()
    );
    let read = memory_versions(&mut tx, &w.owner, &[memory_id])
        .await
        .unwrap();
    assert_eq!(read.len(), 1);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn memories_citing_lists_live_citers_of_an_object_only() {
    let w = world("owner_reads_citing").await;
    let (live, object) = w.cite(w.owner, 7).await;
    let (gone, same_object) = w.cite(w.owner, 7).await;
    assert_eq!(object, same_object, "one object per owner, schema and hash");
    w.write(w.owner, TestMemory::fact()).await;
    w.forget(w.owner, gone).await;
    let mut tx = w.tx(w.owner).await;
    let citing = memories_citing(&mut tx, &w.owner, object).await.unwrap();
    assert_eq!(
        citing.iter().map(|v| v.memory_id).collect::<Vec<_>>(),
        vec![live]
    );
    assert!(
        memories_citing(&mut tx, &w.other, object)
            .await
            .unwrap()
            .is_empty()
    );
    tx.rollback().await.unwrap();
    let mut other_tx = w.tx(w.other).await;
    assert!(
        memories_citing(&mut other_tx, &w.owner, object)
            .await
            .unwrap()
            .is_empty()
    );
    other_tx.rollback().await.unwrap();
}

#[tokio::test]
async fn cited_objects_present_names_only_objects_recorded_for_the_owner() {
    let w = world("owner_reads_objects").await;
    let (_, mine) = w.cite(w.owner, 7).await;
    let (_, theirs) = w.cite(w.other, 7).await;
    let unknown = Uuid::now_v7();
    let mut tx = w.tx(w.owner).await;
    let present = cited_objects_present(&mut tx, &w.owner, &[unknown, theirs, mine])
        .await
        .unwrap();
    assert_eq!(present, vec![mine]);
    assert!(
        cited_objects_present(&mut tx, &w.other, &[theirs])
            .await
            .unwrap()
            .is_empty()
    );
    tx.rollback().await.unwrap();
    let mut other_tx = w.tx(w.other).await;
    assert!(
        cited_objects_present(&mut other_tx, &w.owner, &[mine])
            .await
            .unwrap()
            .is_empty()
    );
    other_tx.rollback().await.unwrap();
}

#[tokio::test]
async fn memory_embeddings_list_each_space_of_a_live_memory() {
    let w = world("owner_reads_embeddings").await;
    let (memory_id, _) = w.write(w.owner, TestMemory::fact()).await;
    let (plain, _) = w.write(w.owner, TestMemory::fact()).await;
    w.give_vector(w.owner, memory_id, "small", 384).await;
    w.give_vector(w.owner, memory_id, "large", 1024).await;
    let mut tx = w.tx(w.owner).await;
    let found = memory_embeddings(&mut tx, &w.owner, &[memory_id, plain])
        .await
        .unwrap();
    assert_eq!(
        found,
        vec![
            MemoryEmbedding {
                memory_id,
                space: space("large", EmbeddingDim::D1024)
            },
            MemoryEmbedding {
                memory_id,
                space: space("small", EmbeddingDim::D384)
            },
        ]
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn memory_embeddings_omit_forgotten_and_foreign_memories() {
    let w = world("owner_reads_embeddings_hidden").await;
    let (live, _) = w.write(w.owner, TestMemory::fact()).await;
    let (gone, _) = w.write(w.owner, TestMemory::fact()).await;
    let (foreign, _) = w.write(w.other, TestMemory::fact()).await;
    for (owner, id) in [(w.owner, live), (w.owner, gone), (w.other, foreign)] {
        w.give_vector(owner, id, "small", 384).await;
    }
    w.forget(w.owner, gone).await;
    let mut tx = w.tx(w.owner).await;
    let found = memory_embeddings(&mut tx, &w.owner, &[live, gone, foreign])
        .await
        .unwrap();
    assert_eq!(
        found.iter().map(|e| e.memory_id).collect::<Vec<_>>(),
        vec![live]
    );
    assert!(
        memory_embeddings(&mut tx, &w.other, &[foreign])
            .await
            .unwrap()
            .is_empty()
    );
    tx.rollback().await.unwrap();
    let mut other_tx = w.tx(w.other).await;
    assert!(
        memory_embeddings(&mut other_tx, &w.owner, &[live])
            .await
            .unwrap()
            .is_empty()
    );
    other_tx.rollback().await.unwrap();
}

/// A caller that reads Facts but no Abstractions learns nothing about an
/// Abstraction it cannot read, its vector included.
#[tokio::test]
async fn memory_reads_omit_a_memory_of_a_kind_the_caller_may_not_read() {
    let w = world("owner_reads_kind_hidden").await;
    let (fact, _) = w.write(w.owner, TestMemory::fact()).await;
    let grounded = TestMemory::fact()
        .kind(MemoryKind::Abstraction)
        .references(vec![EdgeEndpoint::memory(EntityKind::Fact, fact)]);
    let (abstraction, _) = w.write(w.owner, grounded).await;
    for id in [fact, abstraction] {
        w.give_vector(w.owner, id, "small", 384).await;
    }
    let mut tx = w.tx_in(&scope_with(w.owner, Role::ingest())).await;
    let versions = memory_versions(&mut tx, &w.owner, &[fact, abstraction])
        .await
        .unwrap();
    assert_eq!(
        versions.iter().map(|v| v.memory_id).collect::<Vec<_>>(),
        vec![fact]
    );
    let embedded = memory_embeddings(&mut tx, &w.owner, &[fact, abstraction])
        .await
        .unwrap();
    assert_eq!(
        embedded.iter().map(|e| e.memory_id).collect::<Vec<_>>(),
        vec![fact]
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn lexical_languages_list_the_registered_configurations() {
    let w = world("owner_reads_languages").await;
    let mut tx = w.tx(w.owner).await;
    let before = lexical_languages(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    assert!(!before.contains(&"german".to_owned()), "{before:?}");
    TestNote::new("Hallo", "Guten Tag")
        .language("german")
        .create(&w.pg, w.owner)
        .await
        .unwrap();
    let mut tx = w.tx(w.other).await;
    let after = lexical_languages(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    let mut expected = before;
    expected.push("german".to_owned());
    expected.sort();
    assert_eq!(after, expected);
}
