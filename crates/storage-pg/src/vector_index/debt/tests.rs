use std::sync::Arc;

use proxiima_core::storage_ports::{InMemoryVectorIndex, VectorEntryPage, VectorIndex};
use proxiima_core::{EmbeddableEntityRef, MemoryId, MemoryKind, SpaceVector, UserId};
use uuid::Uuid;

use super::*;
use crate::test_fixtures::{create_user, fresh_pg_host_index};

/// Index debt is read from the database, so its rows are parsed at that
/// spring: an operation or width the store does not know is refused.
#[test]
fn debt_rows_parse_only_known_operations_and_widths() {
    let id = Uuid::from_u128(1);
    let row = |dim: i16, op: &str| {
        (
            id,
            "debt".to_owned(),
            dim,
            id,
            op.to_owned(),
            Some((1, true)),
        )
    };
    assert!(matches!(
        parse_debt(row(384, "delete")).map(|d| d.op),
        Ok(DebtOp::Delete)
    ));
    assert!(matches!(
        parse_debt(row(384, "owner")).map(|d| d.op),
        Ok(DebtOp::Owner)
    ));
    assert!(matches!(
        parse_debt(row(384, "head")).map(|d| d.op),
        Ok(DebtOp::Head)
    ));
    assert!(matches!(
        parse_debt(row(384, "rename")),
        Err(StorageError::ConstraintViolation(_))
    ));
    assert!(matches!(
        parse_debt(row(385, "delete")),
        Err(StorageError::ConstraintViolation(_))
    ));
}

/// An erase pays, before it commits, the debt its own transaction left and
/// the debt an earlier forget of the same owner left for the drain; another
/// owner's debt stays for the drain.
#[tokio::test]
async fn an_erase_settles_its_owners_debt_before_commit() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) = fresh_pg_host_index("vector_index_settle", index.clone()).await;
    let pool = pg.pool_for_tests();
    let (owner, other) = (
        OwnerId::from(UserId::new(Uuid::now_v7())),
        OwnerId::from(UserId::new(Uuid::now_v7())),
    );
    let (erased, forgotten, foreign) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for (entity, owner) in [(erased, owner), (forgotten, owner), (foreign, other)] {
        create_user(pool, owner.as_user()).await.unwrap();
        // Raw INSERT on purpose: a head for an entity no memory backs, which the embedding write never produces.
        sqlx::query(
            "INSERT INTO proxiima_core.embedding_heads (entity_id, model_id, dim, embedding_version, owner_id, vector_generation)
             VALUES ($1, 'debt', 384, 1, $2, 1)",
        )
        .bind(entity)
        .bind(owner.into_inner())
        .execute(pool)
        .await
        .unwrap();
        let vector = SpaceVector::new(
            EmbeddingSpace::new("debt", EmbeddingDim::D384),
            vec![1.0; 384],
        )
        .unwrap();
        let entity = EmbeddableEntityRef::Memory {
            kind: MemoryKind::Fact,
            memory_id: MemoryId::new(entity),
        };
        index
            .put(entity, owner, 1, 1, true, &[vector])
            .await
            .unwrap();
    }
    // Earlier, committed: a forget of the owner's and a forget of another's.
    sqlx::query("DELETE FROM proxiima_core.embedding_heads WHERE entity_id = ANY($1)")
        .bind(vec![forgotten, foreign])
        .execute(pool)
        .await
        .unwrap();
    let vectors = StoreVectors::Remote(index.clone());

    let mut tx = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM proxiima_core.embedding_heads WHERE entity_id = $1")
        .bind(erased)
        .execute(&mut *tx)
        .await
        .unwrap();
    settle_owner_debt(
        &mut tx,
        &vectors,
        owner,
        &[],
        OwnerUnsentScope::EntitiesOnly,
    )
    .await
    .unwrap();
    let held: Vec<Uuid> = index
        .entries(None, 10)
        .await
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| entry.key.entity_id)
        .collect();
    assert_eq!(
        held,
        [foreign],
        "the owner's vectors left the index before commit"
    );
    let owed: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM proxiima_core.vector_index_debt")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    assert_eq!(owed, [foreign]);
    tx.commit().await.unwrap();
}

/// One transaction's debt reaches the index in calls of a bounded size.
#[tokio::test]
async fn debt_reaches_the_index_in_bounded_calls() {
    #[derive(Default)]
    struct Counting {
        calls: std::sync::Mutex<Vec<usize>>,
    }
    #[async_trait::async_trait]
    impl VectorIndex for Counting {
        async fn put_batch(
            &self,
            _: &[proxiima_core::storage_ports::VectorPut<'_>],
        ) -> Result<(), proxiima_core::storage_ports::VectorPutBatchError> {
            Ok(())
        }
        async fn delete(&self, keys: &[VectorKey], _: i64) -> Result<(), StorageError> {
            self.calls.lock().unwrap().push(keys.len());
            Ok(())
        }
        async fn set_owner(
            &self,
            _: &[VectorMetadataTarget],
            _: OwnerId,
            _: i64,
        ) -> Result<(), StorageError> {
            Ok(())
        }
        async fn set_head(
            &self,
            _: &[VectorMetadataTarget],
            _: bool,
            _: i64,
        ) -> Result<(), StorageError> {
            Ok(())
        }
        async fn nearest(
            &self,
            _: &proxiima_core::storage_ports::VectorCandidateFilter,
            _: &SpaceVector,
            _: u32,
        ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
            Ok(proxiima_core::storage_ports::VectorCandidatePage::new(
                Vec::new(),
                proxiima_core::storage_ports::CandidateBound::Complete,
            ))
        }
        async fn entries(
            &self,
            _: Option<&VectorKey>,
            _: u32,
        ) -> Result<VectorEntryPage, StorageError> {
            Ok(VectorEntryPage {
                entries: Vec::new(),
                has_more: false,
            })
        }
    }
    let index = Counting::default();
    let id = Uuid::from_u128(1);
    let rows = (0..1_200)
        .map(|i| {
            (
                Uuid::from_u128(i),
                "debt".to_owned(),
                384,
                id,
                "delete".to_owned(),
                None,
            )
        })
        .collect();
    apply(&index, rows, 1).await.unwrap();
    assert_eq!(
        *index.calls.lock().unwrap(),
        [KEYS_PER_CALL, KEYS_PER_CALL, 200]
    );
}

/// The sweep leaves a key a drain is writing: a head the index does not
/// hold yet is not deleted while its writer holds the head's lock.
#[tokio::test]
async fn the_sweep_skips_a_head_its_drain_is_writing() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) = fresh_pg_host_index("vector_index_sweep_lock", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();
    let (writing, lost) = (Uuid::now_v7(), Uuid::now_v7());
    for entity in [writing, lost] {
        // Raw INSERT on purpose: a head for an entity no memory backs, which the embedding write never produces.
        sqlx::query(
            "INSERT INTO proxiima_core.embedding_heads (entity_id, model_id, dim, embedding_version, owner_id)
             VALUES ($1, 'debt', 384, 1, $2)",
        )
        .bind(entity)
        .bind(owner.into_inner())
        .execute(&pool)
        .await
        .unwrap();
    }
    let mut drain = pool.begin().await.unwrap();
    assert!(
        try_lock_embedding_key(&mut drain, writing, "debt", 384)
            .await
            .unwrap()
    );

    let swept = sweep_index(&pg.pool, &pg.platform_scope, index.as_ref())
        .await
        .unwrap();
    assert_eq!(swept.heads_deleted, 1, "only the head nobody is writing");
    drain.rollback().await.unwrap();
    let heads: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM proxiima_core.embedding_heads")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(heads, [writing]);
}

/// `drain_debt` acquires the embedding key lock non-blockingly so it serializes
/// against an in-flight writer or sweep repair on that key without blocking,
/// and pays the row once the lock is released.
#[tokio::test]
async fn drain_debt_skips_a_key_held_by_another_transaction_and_pays_once_released() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) = fresh_pg_host_index("vector_index_drain_lock", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();
    let entity = Uuid::now_v7();
    let space = EmbeddingSpace::new("debt", EmbeddingDim::D384);
    let vector = SpaceVector::new(space.clone(), vec![1.0; 384]).unwrap();
    index
        .put(
            EmbeddableEntityRef::Memory {
                kind: MemoryKind::Fact,
                memory_id: MemoryId::new(entity),
            },
            owner,
            1,
            1,
            true,
            &[vector],
        )
        .await
        .unwrap();
    // Raw INSERT on purpose: simulate debt ready to drain while sweep repair holds the key lock.
    sqlx::query(
        "INSERT INTO proxiima_core.vector_index_debt (entity_id, model_id, dim, owner_id, op)
         VALUES ($1, 'debt', 384, $2, 'delete')",
    )
    .bind(entity)
    .bind(owner.into_inner())
    .execute(&pool)
    .await
    .unwrap();

    let mut repair_tx = pool.begin().await.unwrap();
    assert!(
        try_lock_embedding_key(&mut repair_tx, entity, "debt", 384)
            .await
            .unwrap()
    );
    let vectors = StoreVectors::Remote(index.clone());
    let platform = pg.platform_scope_for_host();
    let paid_while_locked = drain_debt(&pool, &platform, &vectors, 10).await.unwrap();
    assert_eq!(paid_while_locked, 0);
    assert_eq!(index.entries(None, 10).await.unwrap().entries.len(), 1);

    repair_tx.commit().await.unwrap();
    let paid_after_release = drain_debt(&pool, &platform, &vectors, 10).await.unwrap();
    assert_eq!(paid_after_release, 1);
    assert!(index.entries(None, 10).await.unwrap().entries.is_empty());
}

/// Sweep repair checks the PostgreSQL tuple `xmin` generation alongside
/// `(owner, version, is_head)` so a head deleted and recreated at version 1
/// (e.g. forget -> restore -> re-embed) between page read and repair is never
/// deleted by a stale sweep page.
#[tokio::test]
async fn repair_skips_a_head_rebuilt_at_the_same_version_after_page_read() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) = fresh_pg_host_index("vector_index_sweep_xmin", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();
    let entity = Uuid::now_v7();
    let space = EmbeddingSpace::new("debt", EmbeddingDim::D384);
    let vector = SpaceVector::new(space.clone(), vec![1.0; 384]).unwrap();

    // Initial v1 head missing from the host index, read by a sweep page.
    sqlx::query(
        "INSERT INTO proxiima_core.embedding_heads (entity_id, model_id, dim, embedding_version, owner_id, vector_generation)
         VALUES ($1, 'debt', 384, 1, $2, 1)",
    )
    .bind(entity)
    .bind(owner.into_inner())
    .execute(&pool)
    .await
    .unwrap();
    let mut read_tx = pool.begin().await.unwrap();
    let page = heads_after(read_tx.as_mut(), None).await.unwrap();
    read_tx.commit().await.unwrap();
    assert_eq!(page.len(), 1);
    let (key, old_stored) = page.into_iter().next().unwrap();

    // Forget + restore + re-embed replaces the row at the exact same version 1
    // in a new transaction, writes valid vectors to the index, and clears debt.
    sqlx::query("DELETE FROM proxiima_core.embedding_heads WHERE entity_id = $1")
        .bind(entity)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO proxiima_core.embedding_heads (entity_id, model_id, dim, embedding_version, owner_id, vector_generation)
         VALUES ($1, 'debt', 384, 1, $2, 1)",
    )
    .bind(entity)
    .bind(owner.into_inner())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM proxiima_core.vector_index_debt WHERE entity_id = $1")
        .bind(entity)
        .execute(&pool)
        .await
        .unwrap();
    index
        .put(
            EmbeddableEntityRef::Memory {
                kind: MemoryKind::Fact,
                memory_id: MemoryId::new(entity),
            },
            owner,
            1,
            1,
            false,
            &[vector],
        )
        .await
        .unwrap();

    // Sweep repair with the stale page's suspect must skip because `xmin` changed.
    let mut swept = VectorIndexSweepOutcome::default();
    let mut repair_tx = pool.begin().await.unwrap();
    repair(
        repair_tx.as_mut(),
        index.as_ref(),
        &[(key, (None, Some(old_stored)))],
        &mut swept,
    )
    .await
    .unwrap();
    repair_tx.commit().await.unwrap();
    assert_eq!(swept.heads_deleted, 0);
    let surviving: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM proxiima_core.embedding_heads WHERE entity_id = $1",
    )
    .bind(entity)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(surviving, 1, "rebuilt v1 head must survive stale sweep");
    let owed: i64 = sqlx::query_scalar("SELECT count(*) FROM proxiima_core.vector_index_debt")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(owed, 0, "stale sweep must not enqueue delete debt");
}

/// `drain_debt` acquires each affected owner's fence shared non-blockingly so a
/// whole-owner erase holding the owner's exclusive fence serializes against
/// draining with O(1) locks, without taking a lock per vector key.
#[tokio::test]
async fn drain_debt_skips_an_owner_whose_exclusive_fence_is_held() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) = fresh_pg_host_index("vector_index_drain_owner_fence", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();
    let entity = Uuid::now_v7();
    let space = EmbeddingSpace::new("debt", EmbeddingDim::D384);
    let vector = SpaceVector::new(space, vec![1.0; 384]).unwrap();
    index
        .put(
            EmbeddableEntityRef::Memory {
                kind: MemoryKind::Fact,
                memory_id: MemoryId::new(entity),
            },
            owner,
            1,
            1,
            true,
            &[vector],
        )
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO proxiima_core.vector_index_debt (entity_id, model_id, dim, owner_id, op)
         VALUES ($1, 'debt', 384, $2, 'delete')",
    )
    .bind(entity)
    .bind(owner.into_inner())
    .execute(&pool)
    .await
    .unwrap();

    let mut erase_tx = pool.begin().await.unwrap();
    crate::access::fences::lock_owner_fence_exclusive_tx(&mut erase_tx, &owner)
        .await
        .unwrap();
    let vectors = StoreVectors::Remote(index.clone());
    let platform = pg.platform_scope_for_host();
    let paid_while_fenced = drain_debt(&pool, &platform, &vectors, 10).await.unwrap();
    assert_eq!(paid_while_fenced, 0);
    assert_eq!(index.entries(None, 10).await.unwrap().entries.len(), 1);

    erase_tx.rollback().await.unwrap();
    let paid_after_release = drain_debt(&pool, &platform, &vectors, 10).await.unwrap();
    assert_eq!(paid_after_release, 1);
    assert!(index.entries(None, 10).await.unwrap().entries.is_empty());
}

/// Phase 3 drain finalization locks candidate debt rows with `FOR UPDATE SKIP LOCKED`
/// so it never blocks or deadlocks against a concurrent multi-key erase holding a
/// debt row lock; skipped rows stay eligible for the next pass.
#[tokio::test]
async fn finish_debt_batch_skips_locked_rows_without_blocking() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) = fresh_pg_host_index("vector_index_finish_skip_locked", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();
    let (entity_a, entity_b) = (Uuid::now_v7(), Uuid::now_v7());
    let claim_token: i64 = 42;
    for entity in [entity_a, entity_b] {
        sqlx::query(
            "INSERT INTO proxiima_core.vector_index_debt
                 (entity_id, model_id, dim, owner_id, op, generation)
             VALUES ($1, 'debt', 384, $2, 'delete', $3)",
        )
        .bind(entity)
        .bind(owner.into_inner())
        .bind(-claim_token)
        .execute(&pool)
        .await
        .unwrap();
    }

    let mut holder_tx = pool.begin().await.unwrap();
    let locked: Uuid = sqlx::query_scalar(
        "SELECT entity_id FROM proxiima_core.vector_index_debt
          WHERE entity_id = $1 FOR UPDATE",
    )
    .bind(entity_a)
    .fetch_one(&mut *holder_tx)
    .await
    .unwrap();
    assert_eq!(locked, entity_a);

    let keys = vec![
        (entity_a, "debt".to_owned(), 384_i16),
        (entity_b, "debt".to_owned(), 384_i16),
    ];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        finish_debt_batch(&pool, &platform, claim_token, &keys),
    )
    .await
    .expect("finish_debt_batch must not block on a locked debt row")
    .unwrap();

    let remaining: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM proxiima_core.vector_index_debt")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        remaining,
        [entity_a],
        "unlocked row deleted while locked row skipped"
    );

    holder_tx.rollback().await.unwrap();
    finish_debt_batch(&pool, &platform, claim_token, &keys)
        .await
        .unwrap();
    let after: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM proxiima_core.vector_index_debt")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        after.is_empty(),
        "skipped row remains eligible once lock releases"
    );
}

/// Lost-head sweep repair acquires the series handle and lifecycle target locks
/// non-blockingly before the embedding key, so it skips a memory whose `forget`
/// has already locked `embedding_jobs` rather than deadlocking on the inverse
/// `embedding_heads` -> `embedding_jobs` lock order.
#[tokio::test]
async fn lost_head_sweep_skips_memory_locked_by_concurrent_forget_without_deadlocking() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) =
        fresh_pg_host_index("vector_index_sweep_forget_no_deadlock", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let note = crate::test_fixtures::TestNote::new("forget race", "body")
        .create(&pg, owner)
        .await
        .unwrap();
    let entity = note.memory_id.into_inner();
    let handle = note.handle;

    // Seed a lost head (present in PG, absent from host index) plus an existing
    // `failed` re-embedding job (the exact row forget deletes before embedding_heads).
    sqlx::query(
        "INSERT INTO proxiima_core.embedding_heads
             (entity_id, model_id, dim, embedding_version, owner_id, vector_generation)
         VALUES ($1, 'debt', 384, 1, $2, 1)",
    )
    .bind(entity)
    .bind(owner.into_inner())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO proxiima_core.embedding_jobs
             (entity_id, model_id, dim, owner_id, status)
         VALUES ($1, 'debt', 384, $2, 'failed')",
    )
    .bind(entity)
    .bind(owner.into_inner())
    .execute(&pool)
    .await
    .unwrap();

    // Mimic `forget` paused between deleting `embedding_jobs` and deleting `embedding_heads`.
    let mut forget_tx = pool.begin().await.unwrap();
    crate::access::fences::lock_owner_fence_shared_tx(&mut forget_tx, &owner)
        .await
        .unwrap();
    crate::verbs::forget::lock_memory_handles_tx(&mut forget_tx, &[handle])
        .await
        .unwrap();
    crate::verbs::forget::lock_lifecycle_targets_tx(&mut forget_tx, &[entity])
        .await
        .unwrap();
    sqlx::query("DELETE FROM proxiima_core.embedding_jobs WHERE entity_id = $1")
        .bind(entity)
        .execute(&mut *forget_tx)
        .await
        .unwrap();

    let swept_during_forget = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sweep_index(&pool, &platform, index.as_ref()),
    )
    .await
    .expect("sweep must skip a memory locked by forget without blocking or deadlocking")
    .unwrap();
    assert_eq!(swept_during_forget.heads_deleted, 0);

    // If forget rolls back, the next sweep deletes the lost head and reopens the failed job.
    forget_tx.rollback().await.unwrap();
    let swept_after = sweep_index(&pool, &platform, index.as_ref()).await.unwrap();
    assert_eq!(swept_after.heads_deleted, 1);
    let job_status: String = sqlx::query_scalar(
        "SELECT status::text FROM proxiima_core.embedding_jobs
          WHERE entity_id = $1 AND model_id = 'debt' AND dim = 384",
    )
    .bind(entity)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(job_status, "pending");
}

/// A locked owner prefix at the front of `vector_index_debt` must never starve
/// payable debt rows belonging to later owners, even when `limit` is smaller
/// than the locked prefix.
#[tokio::test]
async fn drain_debt_advances_past_locked_owner_prefix_to_pay_later_owners() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) = fresh_pg_host_index("vector_index_drain_past_locked", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner_a = OwnerId::from(UserId::new(Uuid::now_v7()));
    let owner_b = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner_a.as_user()).await.unwrap();
    create_user(&pool, owner_b.as_user()).await.unwrap();
    let space = EmbeddingSpace::new("debt", EmbeddingDim::D384);
    let vector = SpaceVector::new(space, vec![1.0; 384]).unwrap();

    // Deterministic UUIDs with strictly more than `DEBT_SCAN_MIN_PAGE` (64)
    // locked rows for owner A so `claim_debt_batch` must follow keyset
    // continuation (`DEBT_CANDIDATES_AFTER_SQL`) to reach owner B.
    let prefix_count = u128::try_from(DEBT_SCAN_MIN_PAGE).unwrap() + 6;
    let prefix_entities: Vec<Uuid> = (1_u128..=prefix_count).map(Uuid::from_u128).collect();
    let later_entity = Uuid::from_u128(1_000);
    for (&entity, owner) in prefix_entities
        .iter()
        .map(|id| (id, owner_a))
        .chain(std::iter::once((&later_entity, owner_b)))
    {
        index
            .put(
                EmbeddableEntityRef::Memory {
                    kind: MemoryKind::Fact,
                    memory_id: MemoryId::new(entity),
                },
                owner,
                1,
                1,
                true,
                std::slice::from_ref(&vector),
            )
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO proxiima_core.vector_index_debt (entity_id, model_id, dim, owner_id, op)
             VALUES ($1, 'debt', 384, $2, 'delete')",
        )
        .bind(entity)
        .bind(owner.into_inner())
        .execute(&pool)
        .await
        .unwrap();
    }

    let mut erase_tx = pool.begin().await.unwrap();
    crate::access::fences::lock_owner_fence_exclusive_tx(&mut erase_tx, &owner_a)
        .await
        .unwrap();

    // `limit = 2` is strictly smaller than A's 70-row locked prefix; drain must
    // page past A across `DEBT_CANDIDATES_AFTER_SQL` and pay B immediately.
    let vectors = StoreVectors::Remote(index.clone());
    let paid = drain_debt(&pool, &platform, &vectors, 2).await.unwrap();
    assert_eq!(
        paid, 1,
        "drain must advance past multi-page locked prefix to pay owner B"
    );
    let remaining: Vec<Uuid> = index
        .entries(None, 200)
        .await
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| entry.key.entity_id)
        .collect();
    assert_eq!(remaining, prefix_entities);
    erase_tx.rollback().await.unwrap();
}

/// `delete_lost_heads` checks existing `embedding_jobs` via non-blocking
/// `FOR UPDATE SKIP LOCKED` before deleting lost heads, so a real uncommitted
/// `renew_embedding_jobs` heartbeat `UPDATE` causes per-key deferral without
/// blocking sweep or starving unlocked siblings.
#[tokio::test]
async fn delete_lost_heads_never_blocks_on_processing_job_locked_by_heartbeat() {
    use proxiima_core::storage_ports::EmbeddingJobPort;

    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) =
        fresh_pg_host_index("vector_index_sweep_heartbeat_no_deadlock", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let locked_note = crate::test_fixtures::TestNote::new("heartbeat locked", "body")
        .create(&pg, owner)
        .await
        .unwrap();
    let free_note = crate::test_fixtures::TestNote::new("heartbeat free", "body")
        .create(&pg, owner)
        .await
        .unwrap();
    let locked_entity = locked_note.memory_id.into_inner();
    let free_entity = free_note.memory_id.into_inner();

    for entity in [locked_entity, free_entity] {
        sqlx::query(
            "INSERT INTO proxiima_core.embedding_heads
                 (entity_id, model_id, dim, embedding_version, owner_id, vector_generation)
             VALUES ($1, 'debt', 384, 1, $2, 1)",
        )
        .bind(entity)
        .bind(owner.into_inner())
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO proxiima_core.embedding_jobs (entity_id, model_id, dim, owner_id, status)
         VALUES ($1, 'debt', 384, $2, 'pending')",
    )
    .bind(locked_entity)
    .bind(owner.into_inner())
    .execute(&pool)
    .await
    .unwrap();
    let claims = pg.claim_pending_embedding_jobs(10, &[], &[]).await.unwrap();
    let claim = claims
        .into_iter()
        .find(|c| c.entity_id.into_inner() == locked_entity)
        .expect("claimed job for locked_entity");

    // Hold a real uncommitted `UPDATE` on the `processing` job via `renew_embedding_jobs`.
    let mut heartbeat_tx = crate::platform_scope::begin_platform_transaction(&pool, &platform)
        .await
        .unwrap();
    let renewed = crate::verbs::fact_embeddings::renew_embedding_jobs(
        heartbeat_tx.as_mut(),
        std::slice::from_ref(&claim),
    )
    .await
    .unwrap();
    assert_eq!(renewed, 1);

    let swept = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sweep_index(&pool, &platform, index.as_ref()),
    )
    .await
    .expect("lost-head sweep must not wait on an uncommitted heartbeat UPDATE")
    .unwrap();
    assert_eq!(
        swept.heads_deleted, 1,
        "unlocked sibling must be repaired while contended key defers"
    );
    let remaining_heads: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM proxiima_core.embedding_heads")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(remaining_heads, [locked_entity]);

    // Once the heartbeat commits, the next sweep repairs `locked_entity` without
    // touching or duplicating its active `'processing'` job.
    heartbeat_tx.commit().await.unwrap();
    let swept_after = sweep_index(&pool, &platform, index.as_ref()).await.unwrap();
    assert_eq!(swept_after.heads_deleted, 1);
    let status: String = sqlx::query_scalar(
        "SELECT status::text FROM proxiima_core.embedding_jobs WHERE job_id = $1",
    )
    .bind(claim.job_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "processing");
}

#[derive(Default)]
struct PausableDeleteIndex {
    inner: InMemoryVectorIndex,
    pause_next_delete: std::sync::atomic::AtomicBool,
    delete_entered: tokio::sync::Notify,
    delete_release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl VectorIndex for PausableDeleteIndex {
    async fn put_batch(
        &self,
        entries: &[proxiima_core::storage_ports::VectorPut<'_>],
    ) -> Result<(), proxiima_core::storage_ports::VectorPutBatchError> {
        self.inner.put_batch(entries).await
    }
    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        if self
            .pause_next_delete
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.delete_entered.notify_one();
            self.delete_release.notified().await;
        }
        self.inner.delete(keys, fence).await
    }
    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }
    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_head(targets, is_head, revision).await
    }
    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.inner.nearest(filter, query, window).await
    }
    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<proxiima_core::storage_ports::VectorEntryPage, StorageError> {
        self.inner.entries(after, limit).await
    }
}

/// Concurrent `drain_debt` callers honor active negative-token claim leases so
/// drain B advances to later keys instead of reclaiming drain A's prefix, and
/// each drain returns the exact number of rows it actually finalized (`0` when
/// its expired claim was reclaimed by another drain).
#[tokio::test]
async fn concurrent_drains_honor_active_claim_lease_and_report_finalized_count() {
    let index = Arc::new(PausableDeleteIndex::default());
    let (pg, _guard) =
        fresh_pg_host_index("vector_index_concurrent_drain_leases", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();
    let vectors = StoreVectors::Remote(index.clone());

    let (k1, k2, k3) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
    for entity in [k1, k2] {
        sqlx::query(
            "INSERT INTO proxiima_core.vector_index_debt (entity_id, model_id, dim, owner_id, op)
             VALUES ($1, 'debt', 384, $2, 'delete')",
        )
        .bind(entity)
        .bind(owner.into_inner())
        .execute(&pool)
        .await
        .unwrap();
    }

    // Drain A claims `k1` (`limit = 1`) and pauses mid-RPC.
    index
        .pause_next_delete
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (pool_a, plat_a, vecs_a) = (pool.clone(), platform.clone(), vectors.clone());
    let drain_a = tokio::spawn(async move { drain_debt(&pool_a, &plat_a, &vecs_a, 1).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        index.delete_entered.notified(),
    )
    .await
    .unwrap();

    // Concurrent drain B (`limit = 1`) skips leased `k1` and finalizes `k2`.
    assert_eq!(drain_debt(&pool, &platform, &vectors, 1).await.unwrap(), 1);
    let left_before_resume: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM proxiima_core.vector_index_debt")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(left_before_resume, [k1]);

    // Resume drain A: finalizes `k1` and reports `1`.
    index.delete_release.notify_one();
    assert_eq!(drain_a.await.unwrap().unwrap(), 1);

    // Expired drain claim: pause drain C on `k3`, expire its lease, let drain D
    // reclaim and finalize `k3` (`1`), then resumed stale drain C reports `0`.
    sqlx::query(
        "INSERT INTO proxiima_core.vector_index_debt (entity_id, model_id, dim, owner_id, op)
         VALUES ($1, 'debt', 384, $2, 'delete')",
    )
    .bind(k3)
    .bind(owner.into_inner())
    .execute(&pool)
    .await
    .unwrap();
    index
        .pause_next_delete
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (pool_c, plat_c, vecs_c) = (pool.clone(), platform.clone(), vectors.clone());
    let drain_c = tokio::spawn(async move { drain_debt(&pool_c, &plat_c, &vecs_c, 1).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        index.delete_entered.notified(),
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE proxiima_core.vector_index_debt
            SET enqueued_at = clock_timestamp() - interval '120 seconds'
          WHERE entity_id = $1",
    )
    .bind(k3)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(drain_debt(&pool, &platform, &vectors, 1).await.unwrap(), 1);
    index.delete_release.notify_one();
    assert_eq!(
        drain_c.await.unwrap().unwrap(),
        0,
        "stale drain whose expired claim was reclaimed must report 0 finalized rows"
    );
}

/// `delete_lost_heads` takes non-blocking `FOR SHARE OF m SKIP LOCKED` on
/// `proxiima_core.memory` before inspecting or inserting `embedding_jobs`, so
/// an uncommitted `IncludeStale` `reconcile_embeddings` transaction holding
/// `FOR UPDATE OF m` and inserting a new job defers that key rather than
/// blocking sweep while holding lifecycle locks.
#[tokio::test]
async fn delete_lost_heads_defers_on_concurrent_reconcile_job_insert_without_blocking() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) = fresh_pg_host_index(
        "vector_index_sweep_reconcile_insert_no_block",
        index.clone(),
    )
    .await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let locked_note = crate::test_fixtures::TestNote::new("reconcile locked", "body")
        .create(&pg, owner)
        .await
        .unwrap();
    let free_note = crate::test_fixtures::TestNote::new("reconcile free", "body")
        .create(&pg, owner)
        .await
        .unwrap();
    let locked_entity = locked_note.memory_id.into_inner();
    let free_entity = free_note.memory_id.into_inner();

    for entity in [locked_entity, free_entity] {
        sqlx::query(
            "INSERT INTO proxiima_core.embedding_heads
                 (entity_id, model_id, dim, embedding_version, owner_id, vector_generation)
             VALUES ($1, 'debt', 384, 1, $2, 1)",
        )
        .bind(entity)
        .bind(owner.into_inner())
        .execute(&pool)
        .await
        .unwrap();
    }

    // Mirror `reconcile_embeddings` (`IncludeStale`): lock `proxiima_core.memory`
    // `FOR UPDATE` and insert into `embedding_jobs` without committing yet.
    let mut reconcile_tx = crate::platform_scope::begin_platform_transaction(&pool, &platform)
        .await
        .unwrap();
    let _: Uuid =
        sqlx::query_scalar("SELECT m.t FROM proxiima_core.memory m WHERE m.t = $1 FOR UPDATE OF m")
            .bind(locked_entity)
            .fetch_one(reconcile_tx.as_mut())
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO proxiima_core.embedding_jobs (entity_id, model_id, dim, owner_id)
         VALUES ($1, 'debt', 384, $2)",
    )
    .bind(locked_entity)
    .bind(owner.into_inner())
    .execute(reconcile_tx.as_mut())
    .await
    .unwrap();

    let swept = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sweep_index(&pool, &platform, index.as_ref()),
    )
    .await
    .expect("lost-head sweep must not wait on an uncommitted reconcile job insert")
    .unwrap();
    assert_eq!(
        swept.heads_deleted, 1,
        "unlocked sibling must be repaired while contended reconcile key defers"
    );
    let remaining_heads: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM proxiima_core.embedding_heads")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(remaining_heads, [locked_entity]);

    // After reconcile commits, the next sweep deletes `locked_entity`'s lost
    // head and preserves the newly committed `'pending'` job.
    reconcile_tx.commit().await.unwrap();
    let swept_after = sweep_index(&pool, &platform, index.as_ref()).await.unwrap();
    assert_eq!(swept_after.heads_deleted, 1);
    let status: String = sqlx::query_scalar(
        "SELECT status::text FROM proxiima_core.embedding_jobs WHERE entity_id = $1",
    )
    .bind(locked_entity)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "pending");
}

async fn seed_locked_unsent_pair(
    pool: &PgPool,
    index: &InMemoryVectorIndex,
    owner: OwnerId,
    k1: Uuid,
    k2: Uuid,
) -> Vec<StagedPublishIntent> {
    let space = EmbeddingSpace::new("debt", EmbeddingDim::D384);
    let vec = SpaceVector::new(space, vec![1.0; 384]).unwrap();
    let mut intents = Vec::new();
    for (idx, entity) in [k1, k2].into_iter().enumerate() {
        let generation = i64::try_from(idx + 1).unwrap();
        index
            .put(
                EmbeddableEntityRef::Memory {
                    kind: MemoryKind::Fact,
                    memory_id: MemoryId::new(entity),
                },
                owner,
                1,
                generation,
                true,
                std::slice::from_ref(&vec),
            )
            .await
            .unwrap();
        let xact: String = sqlx::query_scalar(
            "INSERT INTO proxiima_core.vector_index_debt
                 (entity_id, model_id, dim, owner_id, op, generation, enqueued_at, xact)
             VALUES ($1, 'debt', 384, $2, 'delete', $3, clock_timestamp(), pg_current_xact_id())
             RETURNING xact::text",
        )
        .bind(entity)
        .bind(owner.into_inner())
        .bind(generation + 10)
        .fetch_one(pool)
        .await
        .unwrap();
        intents.push(StagedPublishIntent {
            entity_id: entity,
            model_id: "debt".to_owned(),
            dim: 384,
            generation: generation + 10,
            tx1_xact: xact,
            prior_debt: None,
        });
    }
    intents
}

async fn assert_contended_unsent_cleanup_case(
    case_idx: usize,
    label: &str,
    lock_sql: &'static str,
) {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) = fresh_pg_host_index(
        &format!("vector_index_unsent_nonblocking_1conn_{case_idx}"),
        index.clone(),
    )
    .await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let single_conn_platform = platform.with_single_connection_for_tests().await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();

    let (k1, k2) = (Uuid::from_u128(10), Uuid::from_u128(20));
    let intents = seed_locked_unsent_pair(&pool, &index, owner, k1, k2).await;

    let mut lock_k1 = platform.begin().await.unwrap();
    let _: Uuid = sqlx::query_scalar(lock_sql)
        .bind(k1)
        .fetch_one(&mut *lock_k1)
        .await
        .unwrap();

    let unsent = HashSet::from([0_usize, 1_usize]);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        release_failed_publish_intents(&pool, &single_conn_platform, &intents, &unsent),
    )
    .await
    .unwrap_or_else(|_| {
        panic!("{label}: cleanup must not block a 1-connection platform pool on contended rows")
    })
    .unwrap();

    let mut check_tx = platform.begin().await.unwrap();
    let (k2_debt, pending_unsent): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM proxiima_core.vector_index_debt WHERE entity_id = $1),
                (SELECT count(*) FROM proxiima_core.vector_index_unsent_intents WHERE entity_id = $2)",
    )
    .bind(k2)
    .bind(k1)
    .fetch_one(&mut *check_tx)
    .await
    .unwrap();
    check_tx.rollback().await.unwrap();
    assert_eq!(
        k2_debt, 0,
        "{label}: unlocked sibling k2 must be settled immediately"
    );
    assert_eq!(
        pending_unsent, 1,
        "{label}: contended k1 must be durably recorded in vector_index_unsent_intents"
    );

    // Roll back the contending transaction (modeling e.g. an erase retry on
    // `InFlightIntents` or rolled-back Tx 2), age k1 past the publish lease,
    // and verify drain reconciles k1 instead of deleting its committed vector.
    lock_k1.rollback().await.unwrap();
    sqlx::query(
        "UPDATE proxiima_core.vector_index_debt
            SET enqueued_at = clock_timestamp() - interval '120 seconds'
          WHERE entity_id = $1",
    )
    .bind(k1)
    .execute(&pool)
    .await
    .unwrap();

    let vectors = StoreVectors::Remote(index.clone());
    assert_eq!(
        drain_debt(&pool, &single_conn_platform, &vectors, 100)
            .await
            .unwrap(),
        1,
        "{label}: drain must reconcile pending unsent intent"
    );
    assert_eq!(
        drain_debt(&pool, &single_conn_platform, &vectors, 100)
            .await
            .unwrap(),
        0
    );
    let mut final_tx = platform.begin().await.unwrap();
    let (debt_left, unsent_left): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM proxiima_core.vector_index_debt),
                (SELECT count(*) FROM proxiima_core.vector_index_unsent_intents)",
    )
    .fetch_one(&mut *final_tx)
    .await
    .unwrap();
    final_tx.rollback().await.unwrap();
    assert_eq!((debt_left, unsent_left), (0, 0), "{label}");
    assert_eq!(
        index.entries(None, 10).await.unwrap().entries.len(),
        2,
        "{label}: both committed vectors must survive"
    );
}

/// When an overlapping transaction holds a debt row (`FOR NO KEY UPDATE`,
/// `FOR UPDATE`, or an uncommitted `DELETE`) during failed-publish cleanup,
/// `release_failed_publish_intents` must settle every unlocked sibling
/// immediately and durably record the contended unsent key in
/// `vector_index_unsent_intents` without blocking even on a 1-connection
/// platform pool; if the contending lock releases or rolls back, `drain_debt`
/// reconciles the key and preserves the committed vector.
#[tokio::test]
async fn contended_unsent_cleanup_never_blocks_one_connection_platform_pool() {
    for (case_idx, (label, lock_sql)) in [
        (
            "for_no_key_update",
            "SELECT entity_id FROM proxiima_core.vector_index_debt
              WHERE entity_id = $1 FOR NO KEY UPDATE",
        ),
        (
            "for_update",
            "SELECT entity_id FROM proxiima_core.vector_index_debt
              WHERE entity_id = $1 FOR UPDATE",
        ),
        (
            "uncommitted_delete",
            "DELETE FROM proxiima_core.vector_index_debt
              WHERE entity_id = $1 RETURNING entity_id",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        assert_contended_unsent_cleanup_case(case_idx, label, lock_sql).await;
    }
}

async fn seed_unsent_cutover_rows(
    pool: &PgPool,
    index: &Arc<InMemoryVectorIndex>,
    owner: OwnerId,
    total: u128,
) {
    let space = EmbeddingSpace::new("debt", EmbeddingDim::D384);
    let vec_data = [SpaceVector::new(space, vec![0.0; 384]).unwrap()];
    for idx in 1..=total {
        let entity = Uuid::from_u128(1_000 + idx);
        let committed_gen = i64::try_from(idx).unwrap();
        let staged_gen = committed_gen + 10_000;
        index
            .put_batch(&[proxiima_core::storage_ports::VectorPut {
                entity: EmbeddableEntityRef::Memory {
                    kind: MemoryKind::Fact,
                    memory_id: MemoryId::new(entity),
                },
                owner,
                version: 1,
                generation: committed_gen,
                is_head: true,
                vectors: &vec_data,
            }])
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO proxiima_core.embedding_heads
                 (entity_id, model_id, dim, embedding_version, owner_id, vector_generation)
             VALUES ($1, 'debt', 384, 1, $2, $3)",
        )
        .bind(entity)
        .bind(owner.into_inner())
        .bind(committed_gen)
        .execute(pool)
        .await
        .unwrap();
        let tx1_xact: String = sqlx::query_scalar(
            "INSERT INTO proxiima_core.vector_index_debt
                 (entity_id, model_id, dim, owner_id, op, generation, enqueued_at, xact)
             VALUES ($1, 'debt', 384, $2, 'delete', $3, clock_timestamp(), pg_current_xact_id())
             RETURNING xact::text",
        )
        .bind(entity)
        .bind(owner.into_inner())
        .bind(staged_gen)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO proxiima_core.vector_index_unsent_intents
                 (entity_id, model_id, dim, generation, tx1_xact)
             VALUES ($1, 'debt', 384, $2, $3::xid8)",
        )
        .bind(entity)
        .bind(staged_gen)
        .bind(&tx1_xact)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// Cutover drains loop `drain_debt` until it returns `0`; when more than
/// `DEBT_SCAN_MIN_PAGE` (64) unlocked unsent markers sit in
/// `vector_index_unsent_intents`, each pass must report non-zero progress until
/// both queues are truly exhausted so a subsequent sweep sees zero residual
/// debt and preserves every committed vector.
#[tokio::test]
async fn drain_exhausts_more_than_one_page_of_unsent_intents_before_returning_zero() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) =
        fresh_pg_host_index("vector_index_unsent_cutover_pages", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();

    let total: u128 = 80;
    seed_unsent_cutover_rows(&pool, &index, owner, total).await;

    let vectors = StoreVectors::Remote(index.clone());
    let mut passes = 0_u32;
    let mut retired = 0_u64;
    loop {
        let progressed = drain_debt(&pool, &platform, &vectors, 10).await.unwrap();
        if progressed == 0 {
            break;
        }
        passes += 1;
        retired += progressed;
        assert!(passes <= 10, "cutover drain must terminate cleanly");
    }
    assert!(
        passes >= 2,
        "80 unsent rows must span >1 reconciliation page"
    );
    assert_eq!(retired, u64::try_from(total).unwrap());

    let mut check_tx = platform.begin().await.unwrap();
    let (debt_left, unsent_left): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM proxiima_core.vector_index_debt),
                (SELECT count(*) FROM proxiima_core.vector_index_unsent_intents)",
    )
    .fetch_one(&mut *check_tx)
    .await
    .unwrap();
    check_tx.rollback().await.unwrap();
    assert_eq!((debt_left, unsent_left), (0, 0));
    assert_eq!(
        index.entries(None, 200).await.unwrap().entries.len(),
        usize::try_from(total).unwrap()
    );
}

async fn wait_for_advisory_waiter_on_key(pool: &PgPool, lock_name: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1
                   FROM pg_locks l,
                        LATERAL (SELECT hashtextextended($1, 0) AS h) k
                  WHERE l.locktype = 'advisory'
                    AND l.objsubid = 1
                    AND NOT l.granted
                    AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())
                    AND l.classid::bigint = ((k.h >> 32) & x'ffffffff'::bigint)
                    AND l.objid::bigint = (k.h & x'ffffffff'::bigint)
             )",
        )
        .bind(lock_name)
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected advisory lock waiter on {lock_name}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn wait_for_advisory_waiter(pool: &PgPool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_locks
                  WHERE locktype = 'advisory'
                    AND NOT granted
                    AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
             )",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected at least one advisory lock waiter"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// `purge_embedding_spaces` must acquire the shared owner fence and canonical
/// embedding-key locks before deleting `embedding_heads`, so its `AFTER DELETE`
/// debt trigger serializes cleanly against a concurrent remote publication
/// (which deletes `vector_index_debt` before upserting `embedding_heads`)
/// without deadlocking (`40P01`).
#[tokio::test]
async fn purge_embedding_spaces_serializes_with_remote_publish_without_deadlock() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) = fresh_pg_host_index("purge_publish_no_deadlock", index).await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();

    let entity = Uuid::now_v7();
    let (initial_gen, staged_gen): (i64, i64) = sqlx::query_as(
        "SELECT nextval('proxiima_core.vector_index_generation_seq'),
                nextval('proxiima_core.vector_index_generation_seq')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO proxiima_core.embedding_heads
             (entity_id, model_id, dim, embedding_version, owner_id, vector_generation)
         VALUES ($1, 'debt', 384, 1, $2, $3)",
    )
    .bind(entity)
    .bind(owner.into_inner())
    .bind(initial_gen)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO proxiima_core.vector_index_debt
             (entity_id, model_id, dim, owner_id, op, generation, enqueued_at, xact)
         VALUES ($1, 'debt', 384, $2, 'delete', $3, clock_timestamp(), pg_current_xact_id())",
    )
    .bind(entity)
    .bind(owner.into_inner())
    .bind(staged_gen)
    .execute(&pool)
    .await
    .unwrap();

    let mut pub_tx = platform.begin().await.unwrap();
    crate::access::fences::lock_owner_fence_shared_tx(&mut pub_tx, &owner)
        .await
        .unwrap();
    lock_embedding_keys_batch(&mut pub_tx, &[(entity, "debt".to_owned(), 384)])
        .await
        .unwrap();
    let deleted = sqlx::query(
        "DELETE FROM proxiima_core.vector_index_debt
          WHERE entity_id = $1 AND model_id = 'debt' AND dim = 384 AND generation = $2",
    )
    .bind(entity)
    .bind(staged_gen)
    .execute(&mut *pub_tx)
    .await
    .unwrap();
    assert_eq!(deleted.rows_affected(), 1);

    let (purge_pool, purge_platform) = (pool.clone(), platform.clone());
    let purge_task = tokio::spawn(async move {
        crate::verbs::fact_embeddings::purge_embedding_spaces(
            &purge_pool,
            &purge_platform,
            &owner,
            &[],
            100,
        )
        .await
    });
    wait_for_advisory_waiter(&pool).await;

    sqlx::query(
        "UPDATE proxiima_core.embedding_heads
            SET embedding_version = embedding_version + 1, vector_generation = $2
          WHERE entity_id = $1 AND model_id = 'debt' AND dim = 384",
    )
    .bind(entity)
    .bind(staged_gen)
    .execute(&mut *pub_tx)
    .await
    .unwrap();
    pub_tx.commit().await.unwrap();

    let purged = tokio::time::timeout(std::time::Duration::from_secs(5), purge_task)
        .await
        .expect("purge must complete once publication commits")
        .unwrap()
        .expect("purge must serialize without PostgreSQL deadlock");
    assert_eq!(purged.heads, 1);
    let debt_left: i64 = sqlx::query_scalar("SELECT count(*) FROM proxiima_core.vector_index_debt")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(debt_left, 1);
}

/// On a local pgvector store, `purge_embedding_spaces` must never delete
/// vectors of a newly published key whose head was not in the purge batch's
/// locked `doomed_heads` set (which would leave an `embedding_heads` row with
/// zero vectors that `MissingOnly` reconciliation skips).
#[tokio::test]
async fn local_purge_never_deletes_vectors_of_concurrently_published_head() {
    use crate::test_fixtures::{TestMemory, fresh_pg};
    use proxiima_core::storage_ports::{EmbeddingWrite, EmbeddingWritePort, EmbeddingWriteProof};

    let (pg, _guard) = fresh_pg("local_purge_concurrent_publish").await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();

    let m1 = TestMemory::fact()
        .create(&pg, owner)
        .await
        .unwrap()
        .memory_id;
    let m2 = TestMemory::fact()
        .create(&pg, owner)
        .await
        .unwrap()
        .memory_id;
    let space = EmbeddingSpace::new("local-purge", EmbeddingDim::D384);
    let vec_data = [SpaceVector::new(space, vec![1.0; 384]).unwrap()];

    // Publish m1 first so purge selects only m1 into `doomed_heads`.
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &owner,
        entity: EmbeddableEntityRef::Memory {
            kind: MemoryKind::Fact,
            memory_id: m1,
        },
        vectors: &vec_data,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await
    .unwrap();

    // Hold m1's canonical embedding-key lock so `purge_embedding_spaces` blocks
    // right between selecting `doomed_heads = [m1]` and executing its DELETE.
    let mut hold_m1 = platform.begin().await.unwrap();
    lock_embedding_keys_batch(
        &mut hold_m1,
        &[(m1.into_inner(), "local-purge".to_owned(), 384)],
    )
    .await
    .unwrap();

    let (purge_pool, purge_platform) = (pool.clone(), platform.clone());
    let purge_task = tokio::spawn(async move {
        crate::verbs::fact_embeddings::purge_embedding_spaces(
            &purge_pool,
            &purge_platform,
            &owner,
            &[],
            100,
        )
        .await
    });
    wait_for_advisory_waiter(&pool).await;

    // Publish m2 locally while purge is paused between selection and deletion.
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &owner,
        entity: EmbeddableEntityRef::Memory {
            kind: MemoryKind::Fact,
            memory_id: m2,
        },
        vectors: &vec_data,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await
    .unwrap();

    hold_m1.commit().await.unwrap();
    let purged = tokio::time::timeout(std::time::Duration::from_secs(5), purge_task)
        .await
        .expect("purge must complete once m1 lock releases")
        .unwrap()
        .unwrap();
    assert_eq!(purged.heads, 1);
    assert_eq!(purged.vectors, 1);

    let (m2_heads, m2_vecs): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM proxiima_core.embedding_heads WHERE entity_id = $1),
                (SELECT count(*) FROM proxiima_core.embeddings WHERE entity_id = $1)",
    )
    .bind(m2.into_inner())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (m2_heads, m2_vecs),
        (1, 1),
        "concurrently published m2 must keep both its head and its vector"
    );
}

/// Local batch publication (`StoreVectors::Local`) must acquire embedding-key
/// locks in the same canonical `(entity_id, model_id, dim)` order as
/// `purge_embedding_spaces`, so reverse-ordered batches `[B, A]` never deadlock
/// with a concurrent purge over `[A, B]`.
#[tokio::test]
async fn local_publish_batch_and_purge_share_canonical_key_lock_order() {
    use crate::test_fixtures::{TestMemory, fresh_pg};
    use proxiima_core::storage_ports::{EmbeddingWrite, EmbeddingWritePort, EmbeddingWriteProof};

    let (pg, _guard) = fresh_pg("local_publish_purge_lock_order").await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();

    let mut ids = [
        TestMemory::fact()
            .create(&pg, owner)
            .await
            .unwrap()
            .memory_id,
        TestMemory::fact()
            .create(&pg, owner)
            .await
            .unwrap()
            .memory_id,
    ];
    ids.sort_by_key(|id| id.into_inner());
    let [a, b] = ids;
    let space = EmbeddingSpace::new("local-order", EmbeddingDim::D384);
    let vec_data = [SpaceVector::new(space, vec![1.0; 384]).unwrap()];
    let make_write = |memory_id| EmbeddingWrite {
        owner: &owner,
        entity: EmbeddableEntityRef::Memory {
            kind: MemoryKind::Fact,
            memory_id,
        },
        vectors: &vec_data,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    };
    pg.insert_embeddings(&[make_write(a), make_write(b)])
        .await
        .unwrap();

    // Hold B's embedding-key lock so purge `[A, B]` acquires A and waits on B.
    let mut hold_b = platform.begin().await.unwrap();
    lock_embedding_keys_batch(
        &mut hold_b,
        &[(b.into_inner(), "local-order".to_owned(), 384)],
    )
    .await
    .unwrap();
    let (purge_pool, purge_platform) = (pool.clone(), platform.clone());
    let purge_task = tokio::spawn(async move {
        crate::verbs::fact_embeddings::purge_embedding_spaces(
            &purge_pool,
            &purge_platform,
            &owner,
            &[],
            100,
        )
        .await
    });
    wait_for_advisory_waiter_on_key(
        &pool,
        &format!("proxiima-embedding:{}:local-order:384", b.into_inner()),
    )
    .await;

    // Reverse-ordered `[B, A]` local publish batch must also lock `[A, B]`
    // canonically (waiting specifically on A behind purge instead of B).
    let pg_pub = pg.clone();
    let pub_task = tokio::spawn(async move {
        let vec_data = [SpaceVector::new(
            EmbeddingSpace::new("local-order", EmbeddingDim::D384),
            vec![1.0; 384],
        )
        .unwrap()];
        let w = |memory_id| EmbeddingWrite {
            owner: &owner,
            entity: EmbeddableEntityRef::Memory {
                kind: MemoryKind::Fact,
                memory_id,
            },
            vectors: &vec_data,
            proof: EmbeddingWriteProof::for_direct_write_for_tests(),
        };
        pg_pub.insert_embeddings(&[w(b), w(a)]).await
    });
    wait_for_advisory_waiter_on_key(
        &pool,
        &format!("proxiima-embedding:{}:local-order:384", a.into_inner()),
    )
    .await;

    hold_b.commit().await.unwrap();
    let purged = tokio::time::timeout(std::time::Duration::from_secs(5), purge_task)
        .await
        .expect("purge must complete without deadlock")
        .unwrap()
        .unwrap();
    assert_eq!(purged.heads, 2);
    let published = tokio::time::timeout(std::time::Duration::from_secs(5), pub_task)
        .await
        .expect("reverse-ordered local publish batch must complete without deadlock")
        .unwrap()
        .unwrap();
    assert_eq!(published.len(), 2);
}

async fn wait_for_row_lock_waiter_behind(pool: &PgPool, holder: i32) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_stat_activity
                  WHERE $1 = ANY(pg_blocking_pids(pid))
             )",
        )
        .bind(holder)
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected a backend waiting behind pid {holder}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Publish `entity` in the `purge-claim` space, then queue a pending job to
/// embed it there again.
async fn publish_and_queue_again(
    pg: &crate::PgStorage,
    owner: OwnerId,
    entity: EmbeddableEntityRef,
) {
    use proxiima_core::storage_ports::{EmbeddingWrite, EmbeddingWritePort, EmbeddingWriteProof};

    let space = EmbeddingSpace::new("purge-claim", EmbeddingDim::D384);
    let vectors = [SpaceVector::new(space, vec![1.0; 384]).unwrap()];
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &owner,
        entity,
        vectors: &vectors,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO proxiima_core.embedding_jobs (entity_id, model_id, dim, owner_id)
         VALUES ($1, 'purge-claim', 384, $2)",
    )
    .bind(entity.entity_id())
    .bind(owner.into_inner())
    .execute(pg.pool_for_tests())
    .await
    .unwrap();
}

/// A worker claims a pending job while a purge holds that head's
/// embedding-key lock: the worker's local publication locks the job row
/// before it waits on the key lock. The purge deletes jobs only after the
/// transaction holding its key locks committed, so neither waits on the other
/// in a cycle (`40P01`). Both commit, and the claimed job settles through the
/// worker.
#[tokio::test]
async fn purge_and_a_worker_settling_a_claim_behind_its_key_lock_both_commit() {
    use crate::test_fixtures::{TestMemory, fresh_pg};
    use proxiima_core::storage_ports::{
        EmbeddingJobPort, EmbeddingWrite, EmbeddingWritePort, EmbeddingWriteProof,
    };

    let (pg, _guard) = fresh_pg("purge_claimed_job_no_deadlock").await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();
    let memory_id = TestMemory::fact()
        .create(&pg, owner)
        .await
        .unwrap()
        .memory_id;
    let entity = EmbeddableEntityRef::Memory {
        kind: MemoryKind::Fact,
        memory_id,
    };
    publish_and_queue_again(&pg, owner, entity).await;

    // Hold the head's vector row so the purge statement takes its snapshot,
    // with the job still pending, and then waits inside its first delete.
    let mut hold_vector = pool.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *hold_vector)
        .await
        .unwrap();
    sqlx::query("SELECT 1 FROM proxiima_core.embeddings WHERE entity_id = $1 FOR UPDATE")
        .bind(memory_id.into_inner())
        .execute(&mut *hold_vector)
        .await
        .unwrap();
    let (purge_pool, purge_platform) = (pool.clone(), platform.clone());
    let purge_task = tokio::spawn(async move {
        crate::verbs::fact_embeddings::purge_embedding_spaces(
            &purge_pool,
            &purge_platform,
            &owner,
            &[],
            100,
        )
        .await
    });
    wait_for_row_lock_waiter_behind(&pool, holder).await;

    let claims = pg.claim_pending_embedding_jobs(1, &[], &[]).await.unwrap();
    let [claim] = claims.as_slice() else {
        panic!("expected one claim, got {}", claims.len());
    };
    assert_eq!(claim.entity_id, memory_id);
    let (worker_pg, worker_claim) = (pg.clone(), claim.clone());
    let worker_task = tokio::spawn(async move {
        let vectors = [SpaceVector::new(worker_claim.space.clone(), vec![2.0; 384]).unwrap()];
        worker_pg
            .insert_embeddings(&[EmbeddingWrite {
                owner: &worker_claim.owner,
                entity,
                vectors: &vectors,
                proof: EmbeddingWriteProof::for_claim_for_tests(&worker_claim),
            }])
            .await
    });
    wait_for_advisory_waiter_on_key(
        &pool,
        &format!(
            "proxiima-embedding:{}:purge-claim:384",
            memory_id.into_inner()
        ),
    )
    .await;

    hold_vector.rollback().await.unwrap();
    let purged = tokio::time::timeout(std::time::Duration::from_secs(5), purge_task)
        .await
        .expect("purge must complete once the worker settles")
        .unwrap();
    let published = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task)
        .await
        .expect("the worker must settle its claim once purge commits")
        .unwrap();
    let published = published.expect("the worker must not deadlock with the purge");
    let purged = purged.expect("purge must not deadlock with the settling worker");
    assert_eq!(published.len(), 1);
    assert_eq!((purged.heads, purged.jobs), (1, 0));
    let jobs_left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM proxiima_core.embedding_jobs WHERE entity_id = $1",
    )
    .bind(memory_id.into_inner())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(jobs_left, 0, "the worker's write completed its claim");
}

/// A purge waits on a job row another transaction holds and rechecks it once
/// that commits: a job requeued from `failed` to `pending` outside the route
/// still goes, so the engine's loop never stops while it is queued.
#[tokio::test]
async fn purge_deletes_a_job_requeued_while_it_waits_on_the_row() {
    use crate::test_fixtures::{TestMemory, fresh_pg};

    let (pg, _guard) = fresh_pg("purge_requeued_job").await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();
    let memory_id = TestMemory::fact()
        .create(&pg, owner)
        .await
        .unwrap()
        .memory_id;
    let job_id: Uuid = sqlx::query_scalar(
        "INSERT INTO proxiima_core.embedding_jobs
             (entity_id, model_id, dim, owner_id, status, last_error)
         VALUES ($1, 'purge-requeue', 384, $2, 'failed', 'provider down')
         RETURNING job_id",
    )
    .bind(memory_id.into_inner())
    .bind(owner.into_inner())
    .fetch_one(&pool)
    .await
    .unwrap();

    let mut requeue = pool.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *requeue)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE proxiima_core.embedding_jobs
            SET status = 'pending', last_error = NULL
          WHERE job_id = $1",
    )
    .bind(job_id)
    .execute(&mut *requeue)
    .await
    .unwrap();
    let (purge_pool, purge_platform) = (pool.clone(), platform.clone());
    let purge_task = tokio::spawn(async move {
        crate::verbs::fact_embeddings::purge_embedding_spaces(
            &purge_pool,
            &purge_platform,
            &owner,
            &[],
            100,
        )
        .await
    });
    wait_for_row_lock_waiter_behind(&pool, holder).await;
    requeue.commit().await.unwrap();

    let purged = tokio::time::timeout(std::time::Duration::from_secs(5), purge_task)
        .await
        .expect("purge must complete once the requeue commits")
        .unwrap()
        .expect("purge must not fail on the requeued job");
    assert_eq!(purged.jobs, 1);
    let jobs_left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM proxiima_core.embedding_jobs WHERE owner_id = $1")
            .bind(owner.into_inner())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(jobs_left, 0, "no unrouted job outlives the purge");
}

/// How failure cleanup finds one key's intent row.
#[derive(Clone, Copy)]
enum IntentContention {
    /// The row carries the intent's generation and nothing holds it.
    Free,
    /// A concurrent transaction holds the row.
    Held,
    /// A newer publication took the row over at a later generation.
    Superseded,
    /// A purge settled the key as a delete at generation zero.
    Invalidated,
}

/// One key of the failed batch the cleanup ledger replays.
struct FailedPutCase {
    intent: StagedPublishIntent,
    unsent: bool,
    owner_moved: bool,
    contention: IntentContention,
    row: serde_json::Value,
}

/// Stage a publish intent at `generation`, as a transfer left it when
/// `owner_moved`; returns the row's transaction id.
async fn stage_failed_put_intent(
    pool: &PgPool,
    owner: OwnerId,
    entity_id: Uuid,
    generation: i64,
    owner_moved: bool,
) -> String {
    sqlx::query_scalar(
        "INSERT INTO proxiima_core.vector_index_debt
             (entity_id, model_id, dim, owner_id, op, generation, moved_from_generation,
              enqueued_at, xact)
         VALUES ($1, 'debt', 384, $2, 'delete',
                 CASE WHEN $4 THEN 0 ELSE $3 END, CASE WHEN $4 THEN $3 ELSE 0 END,
                 clock_timestamp(), pg_current_xact_id())
         RETURNING xact::text",
    )
    .bind(entity_id)
    .bind(owner.into_inner())
    .bind(generation)
    .bind(owner_moved)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The prior debt a ledger `UnsentPrior` names; a live predecessor is at `live`.
fn ledger_prior(prior: &serde_json::Value, live: i64, xact: &str) -> Option<PriorDebt> {
    let debt = |op: &str, generation| PriorDebt {
        op: op.to_owned(),
        generation,
        moved_from_generation: 0,
        xact: xact.to_owned(),
    };
    match (prior.as_str(), prior["settled"][0].as_str()) {
        (Some("absent"), None) => None,
        (Some("live"), None) => Some(debt("delete", live)),
        (None, Some(op)) => Some(debt(op, 0)),
        _ => panic!("unknown ledger prior {prior}"),
    }
}

/// What the `n`th key of a failed batch owes afterwards is its ledger row's
/// outcome: the exact debt left on the key and whether an unsent record was
/// written.
async fn assert_failed_put_outcome(read: &mut PgConnection, n: i64, case: &FailedPutCase) {
    let (op, generation, moved_from, recorded): (Option<String>, i64, i64, bool) = sqlx::query_as(
        "SELECT d.op, COALESCE(d.generation, 0), COALESCE(d.moved_from_generation, 0),
                EXISTS (SELECT 1 FROM proxiima_core.vector_index_unsent_intents u
                         WHERE u.entity_id = $1)
           FROM (SELECT 1) AS one
           LEFT JOIN proxiima_core.vector_index_debt d ON d.entity_id = $1",
    )
    .bind(case.intent.entity_id)
    .fetch_one(read)
    .await
    .unwrap();
    let left = (op.as_deref(), generation, moved_from, recorded);
    let contended = || match case.contention {
        IntentContention::Held => 1000 + n,
        IntentContention::Superseded => 2000 + n,
        IntentContention::Free | IntentContention::Invalidated => {
            panic!("{}: an uncontended row has no holder", case.row)
        }
    };
    let out = &case.row["out"];
    let expected = match (out.as_str(), out["owe"][0].as_str()) {
        (Some("oweDelete" | "keepSettled"), None) => (Some("delete"), 0, 0, false),
        (Some("restorePrior"), None) => (Some("head"), 0, 0, false),
        (Some("leaveToLease"), None) => (Some("delete"), contended(), 0, false),
        (Some("recordUnsent"), None) => (Some("delete"), contended(), 0, true),
        (Some("drop"), None) => (None, 0, 0, false),
        (None, Some(owed)) => (Some(owed), 0, 0, false),
        (Some("restoreLive"), None) if case.owner_moved => (Some("delete"), 0, 500 + n, false),
        (Some("restoreLive"), None) => (Some("delete"), 500 + n, 0, false),
        _ => panic!("unknown ledger outcome {out}"),
    };
    assert_eq!(left, expected, "{}", case.row);
}

/// Move each superseded case's row to a newer generation, and settle each
/// invalidated one as a purge does.
async fn take_over_contended_rows(pool: &PgPool, cases: &[FailedPutCase]) {
    for (n, case) in (1_i64..).zip(cases) {
        let takeover = match case.contention {
            IntentContention::Free | IntentContention::Held => continue,
            IntentContention::Superseded => 2000 + n,
            IntentContention::Invalidated => 0,
        };
        sqlx::query(
            "UPDATE proxiima_core.vector_index_debt
                SET op = 'delete', generation = $2, moved_from_generation = 0,
                    enqueued_at = clock_timestamp(), xact = pg_current_xact_id()
              WHERE entity_id = $1",
        )
        .bind(case.intent.entity_id)
        .bind(takeover)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// A failed batch settles each key as `Proxiima.Embeddings.failedPutStep`
/// and, for an unsent key it locked, `Proxiima.Embeddings.unsentRestore`
/// decide; each ledger row is one key of the batch, and the owner-move rows
/// replay `Proxiima.Embeddings.an_owner_move_survives_an_unsent_restore`. A
/// `heldOrSuperseded` row replays twice, as a held row and as one a newer
/// generation took over; an `invalidated` row replays a purge's settled delete
/// (`Proxiima.Embeddings.an_invalidated_intent_keeps_its_settled_debt`).
#[tokio::test]
async fn failed_put_cleanup_matches_ledger_vectors() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _guard) = fresh_pg_host_index("failed_put_cleanup_ledger", index).await;
    let pool = pg.pool_for_tests().clone();
    let platform = pg.platform_scope_for_host();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner.as_user()).await.unwrap();
    let ledger: serde_json::Value =
        serde_json::from_str(include_str!("../../../../core/tests/ledger_vectors.json")).unwrap();
    let rows = |decision: &str| ledger[decision].as_array().unwrap().clone();

    let mut cases = Vec::new();
    let mut stage = async |inputs: (bool, bool, &serde_json::Value), unsent, contention, row| {
        let (owner_moved, head_flipped, prior) = inputs;
        let n = i64::try_from(cases.len()).unwrap() + 1;
        let entity_id = Uuid::from_u128(u128::try_from(n).unwrap());
        let generation = 1000 + n;
        let xact = stage_failed_put_intent(&pool, owner, entity_id, generation, owner_moved).await;
        cases.push(FailedPutCase {
            intent: StagedPublishIntent {
                entity_id,
                model_id: "debt".to_owned(),
                dim: 384,
                generation,
                tx1_xact: if head_flipped {
                    "1".to_owned()
                } else {
                    xact.clone()
                },
                prior_debt: ledger_prior(prior, 500 + n, &xact),
            },
            unsent,
            owner_moved,
            contention,
            row,
        });
    };
    let settled_head = serde_json::json!({"settled": ["head"]});
    for row in rows("Proxiima.Embeddings.failedPutStep") {
        let unsent = match row["in"][0].as_str() {
            Some("uncertain") => false,
            Some("unsent") => true,
            _ => panic!("unknown delivery {row}"),
        };
        let contentions: &[IntentContention] = match row["in"][1].as_str() {
            Some("unlocked") => &[IntentContention::Free],
            Some("heldOrSuperseded") => &[IntentContention::Held, IntentContention::Superseded],
            Some("invalidated") => &[IntentContention::Invalidated],
            _ => panic!("unknown intent row {row}"),
        };
        for &contention in contentions {
            stage(
                (false, false, &settled_head),
                unsent,
                contention,
                row.clone(),
            )
            .await;
        }
    }
    for row in rows("Proxiima.Embeddings.unsentRestore") {
        let inputs = (
            row["in"][0].as_bool().unwrap(),
            row["in"][1].as_bool().unwrap(),
            &row["in"][2].clone(),
        );
        stage(inputs, true, IntentContention::Free, row.clone()).await;
    }

    take_over_contended_rows(&pool, &cases).await;
    let mut contender = platform.begin().await.unwrap();
    let held_keys: Vec<Uuid> = cases
        .iter()
        .filter(|case| matches!(case.contention, IntentContention::Held))
        .map(|case| case.intent.entity_id)
        .collect();
    sqlx::query(
        "SELECT 1 FROM proxiima_core.vector_index_debt WHERE entity_id = ANY($1) FOR UPDATE",
    )
    .bind(&held_keys)
    .execute(&mut *contender)
    .await
    .unwrap();
    let intents: Vec<StagedPublishIntent> = cases.iter().map(|case| case.intent.clone()).collect();
    let unsent: HashSet<usize> = cases
        .iter()
        .enumerate()
        .filter_map(|(idx, case)| case.unsent.then_some(idx))
        .collect();
    release_failed_publish_intents(&pool, &platform, &intents, &unsent)
        .await
        .unwrap();
    contender.rollback().await.unwrap();

    let mut read = platform.begin().await.unwrap();
    for (n, case) in (1_i64..).zip(&cases) {
        assert_failed_put_outcome(&mut read, n, case).await;
    }
    read.rollback().await.unwrap();
}
