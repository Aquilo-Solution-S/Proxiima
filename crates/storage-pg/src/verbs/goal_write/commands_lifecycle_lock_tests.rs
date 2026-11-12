use super::super::GoalChange;
use super::super::insert::prepare_goal_insert;
use super::super::types::RowColumns;
use super::super::{
    GoalWritePreparation, lock_prepared_goal_write, lock_prepared_goal_writes,
    persist_prepared_goal_write, prepare_goal_write,
};
use crate::PgStorage;
use crate::test_fixtures::create_core_db;
use crate::verbs::goal_timeseries::{GoalWriteCommand, write_goal};
use proxiima_core::GoalPayload;
use proxiima_core::storage_ports::FactIngestPort;
use proxiima_core::storage_ports::OwnerWritePermit;
use proxiima_core::verbs::fact_ingest::FactWriteCommand;
use proxiima_core::verbs::goal_write::{
    GoalAtomicContext, GoalAuthorship, GoalDraft, GoalState, GoalTopologyWrite,
};
use proxiima_core::{
    EdgeEndpoint, EntityKind, MemoryKind, OwnerId, SchemaId, SchemaVersion, SimpleTextGoalV1,
    SpiiceRegistry, StorageError, StorageRule, UserId,
};
use proxiima_pg_testkit::{db_url, drop_db};
use uuid::Uuid;

fn fact() -> FactWriteCommand {
    FactWriteCommand {
        schema_id: SchemaId::new("core/test-fact-v1".into()),
        schema_version: SchemaVersion::new(1),
        handle: None,
        source_id: None,
        ingest_key: None,
        payload: Vec::new(),
        rendered_text: None,
        lexical_language: None,
        receipt: None,
        occurred_at: None,
        citation: None,
        additional_references: Vec::new(),
        refs: Vec::new(),
        blob_id: None,
        kind: proxiima_core::MemoryKind::Fact,
        request: None,
    }
}

async fn grounded_perspective(
    pg: &PgStorage,
    permit: &OwnerWritePermit,
) -> Result<proxiima_core::verbs::fact_ingest::FactIngestOutcome, StorageError> {
    let fact_out = pg.ingest_fact_atomic(permit, &fact(), None).await?;
    let mut abstraction = fact();
    abstraction.kind = proxiima_core::MemoryKind::Abstraction;
    abstraction.additional_references =
        vec![EdgeEndpoint::memory(EntityKind::Fact, fact_out.memory_id)];
    let abstraction = pg.ingest_fact_atomic(permit, &abstraction, None).await?;
    let mut perspective = fact();
    perspective.kind = proxiima_core::MemoryKind::Perspective;
    perspective.additional_references = vec![EdgeEndpoint::memory(
        EntityKind::Abstraction,
        abstraction.memory_id,
    )];
    pg.ingest_fact_atomic(permit, &perspective, None).await
}

#[tokio::test]
async fn goal_motivations_require_same_owner_perspectives() {
    let db_name = format!("proxiima_test_{}", Uuid::now_v7().simple());
    if let Err(e) = create_core_db(&db_name).await {
        panic!("PG required for tests but admin connect failed: {e}");
    }
    let url = db_url(&db_name);
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let pg = PgStorage::connect_for_tests(url.clone()).await?;
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let other = OwnerId::from(UserId::new(Uuid::now_v7()));
        let owner_permit = OwnerWritePermit::new_for_tests(owner, MemoryKind::Fact);
        let other_permit = OwnerWritePermit::new_for_tests(other, MemoryKind::Fact);
        let fact_target = pg.ingest_fact_atomic(&owner_permit, &fact(), None).await?;
        let own_perspective = grounded_perspective(&pg, &owner_permit).await?;
        let other_perspective = grounded_perspective(&pg, &other_permit).await?;
        let pool = pg.pool_for_tests();
        let command = |request_id: &str, target: Uuid| GoalWriteCommand {
            handle: None,
            schema_id: "core/task-v1".into(),
            title: request_id.into(),
            text: None,
            state: GoalState::Active,
            request_id: request_id.into(),
            close_fact_t: None,

            motivated_by: vec![target],
            dependency_t: vec![],
            evidence_t: vec![],
            mint_write_act: false,
            write_act_t: None,
            parent_handle: None,
        };
        let mut tx = pool.begin().await?;
        let inserted = write_goal(
            &mut tx,
            &owner,
            crate::test_fixtures::writer(),
            &command(
                "same-owner-motivation",
                own_perspective.memory_id.into_inner(),
            ),
            1,
        )
        .await?;
        tx.commit().await?;
        let stored: Vec<Uuid> =
            sqlx::query_scalar("SELECT unnest(motivated_by) FROM proxiima_core.goal WHERE t = $1")
                .bind(inserted.t)
                .fetch_all(pool)
                .await?;
        assert_eq!(stored, vec![own_perspective.memory_id.into_inner()]);

        let mut tx = pool.begin().await?;
        let refused = write_goal(
            &mut tx,
            &owner,
            crate::test_fixtures::writer(),
            &command("wrong-kind-motivation", fact_target.memory_id.into_inner()),
            1,
        )
        .await;
        assert!(matches!(refused, Err(StorageError::ConstraintViolation(_))));
        tx.rollback().await?;

        let mut tx = pool.begin().await?;
        let refused = write_goal(
            &mut tx,
            &owner,
            crate::test_fixtures::writer(),
            &command(
                "cross-owner-motivation",
                other_perspective.memory_id.into_inner(),
            ),
            1,
        )
        .await;
        assert!(matches!(
            refused,
            Err(StorageError::Refused {
                rule: StorageRule::GoalMotivationOwnerDiffers,
                ..
            })
        ));
        tx.rollback().await?;
        Ok(())
    }
    .await;
    let _ = drop_db(&db_name).await;
    result.expect("motivation owner validation failed");
}

async fn lock_target(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    t: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT proxiima_core.lock_pin_targets(ARRAY[$1])")
        .bind(t)
        .execute(&mut **tx)
        .await
        .map(|_| ())
}

async fn assert_stale_successor_not_persisted(
    pool: &sqlx::PgPool,
    request_id: &str,
    write_act_t: Uuid,
    handle: Uuid,
    expected_head_t: Uuid,
) -> Result<(), sqlx::Error> {
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*)::bigint FROM proxiima_core.goal
                 WHERE request_id = $1",
        )
        .bind(request_id)
        .fetch_one(pool)
        .await?,
        0,
        "stale successor must not persist a Goal"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*)::bigint FROM proxiima_core.memory WHERE t = $1",
        )
        .bind(write_act_t)
        .fetch_one(pool)
        .await?,
        0,
        "stale successor must not persist its reserved lifecycle Fact"
    );
    let head_t: Uuid =
        sqlx::query_scalar("SELECT t FROM proxiima_core.goal_head WHERE handle = $1")
            .bind(handle)
            .fetch_one(pool)
            .await?;
    assert_eq!(head_t, expected_head_t);
    Ok(())
}

async fn run_concurrent_successor_race(
    pool: &sqlx::PgPool,
    owner: &OwnerId,
    handle: Uuid,
    expected_prior_t: Option<Uuid>,
    first_request_id: &str,
    second_request_id: &str,
) -> Result<(StorageError, Uuid), Box<dyn std::error::Error>> {
    let successor = |request_id: &str| GoalWriteCommand {
        handle: Some(handle),
        schema_id: "core/task-v1".into(),
        title: request_id.into(),
        text: None,
        state: GoalState::Active,
        request_id: request_id.into(),
        close_fact_t: None,

        motivated_by: vec![],
        dependency_t: vec![],
        evidence_t: vec![],
        mint_write_act: true,
        write_act_t: None,
        parent_handle: None,
    };

    // Both preparations observe the same predecessor before either
    // transaction acquires its lifecycle union.
    let mut first_tx = pool.begin().await?;
    let first = prepare_goal_write(
        &mut first_tx,
        owner,
        crate::test_fixtures::writer(),
        &successor(first_request_id),
        expected_prior_t,
        1,
    )
    .await?;
    let mut second_tx = pool.begin().await?;
    let second = prepare_goal_write(
        &mut second_tx,
        owner,
        crate::test_fixtures::writer(),
        &successor(second_request_id),
        expected_prior_t,
        1,
    )
    .await?;
    let GoalWritePreparation::New(first) = first else {
        unreachable!();
    };
    let GoalWritePreparation::New(second) = second else {
        unreachable!();
    };
    let second_write_act_t = second
        .reserved_write_act_t()
        .expect("successor reserves a write-act identity");

    lock_prepared_goal_write(&mut first_tx, &first).await?;
    let mut second_lock = Box::pin(lock_prepared_goal_write(&mut second_tx, &second));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut second_lock)
            .await
            .is_err(),
        "second successor must wait on the predecessor lifecycle lock"
    );

    let first_outcome = persist_prepared_goal_write(&mut first_tx, &first).await?;
    first_tx.commit().await?;
    let second_err = second_lock
        .await
        .expect_err("stale second successor must be rejected");
    second_tx.rollback().await?;

    assert_stale_successor_not_persisted(
        pool,
        second_request_id,
        second_write_act_t,
        handle,
        first_outcome.t,
    )
    .await?;
    Ok((second_err, first_outcome.t))
}

#[tokio::test]
async fn decomposition_locks_the_union_before_first_goal_persist() {
    let db_name = format!("proxiima_test_{}", Uuid::now_v7().simple());
    if let Err(e) = create_core_db(&db_name).await {
        panic!("PG required for tests but admin connect failed: {e}");
    }
    let url = db_url(&db_name);
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let pg = PgStorage::connect_for_tests(url.clone()).await?;
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let permit = OwnerWritePermit::new_for_tests(owner, MemoryKind::Fact);
        let first = pg.ingest_fact_atomic(&permit, &fact(), None).await?;
        let second = pg.ingest_fact_atomic(&permit, &fact(), None).await?;
        let pool = pg.pool_for_tests();
        let mut tx = pool.begin().await?;
        let command = |request_id: &str, evidence_t| GoalWriteCommand {
            handle: None,
            schema_id: "core/task-v1".into(),
            title: request_id.into(),
            text: None,
            state: GoalState::Active,
            request_id: request_id.into(),
            close_fact_t: None,

            motivated_by: vec![],
            dependency_t: vec![],
            evidence_t: vec![evidence_t],
            mint_write_act: false,
            write_act_t: None,
            parent_handle: None,
        };
        let prepared_first = prepare_goal_write(
            &mut tx,
            &owner,
            crate::test_fixtures::writer(),
            &command("union-first", second.memory_id.into_inner()),
            None,
            1,
        )
        .await?;
        let prepared_second = prepare_goal_write(
            &mut tx,
            &owner,
            crate::test_fixtures::writer(),
            &command("union-second", first.memory_id.into_inner()),
            None,
            1,
        )
        .await?;
        assert!(matches!(&prepared_first, GoalWritePreparation::New(_)));
        assert!(matches!(&prepared_second, GoalWritePreparation::New(_)));
        let prepared = vec![prepared_first, prepared_second];
        let lock_items: Vec<&GoalWritePreparation> = prepared.iter().collect();

        // The later child (`union-second`) uses `first.memory_id`; block that
        // target to prove the full child union is acquired before any child
        // can persist.
        let mut blocker = pool.begin().await?;
        lock_target(&mut blocker, first.memory_id.into_inner()).await?;
        let mut lock = Box::pin(lock_prepared_goal_writes(&mut tx, &lock_items));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut lock)
                .await
                .is_err(),
            "union acquisition must wait on a target held by another lifecycle"
        );
        let before: i64 = sqlx::query_scalar(
            "SELECT count(*)::bigint FROM proxiima_core.goal
                 WHERE request_id IN ('union-first', 'union-second')",
        )
        .fetch_one(pool)
        .await?;
        assert_eq!(before, 0, "preparation and blocked union persist no Goal");
        blocker.commit().await?;
        lock.await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*)::bigint FROM proxiima_core.goal
                     WHERE request_id IN ('union-first', 'union-second')",
            )
            .fetch_one(pool)
            .await?,
            0,
            "the union lock itself persists no Goal"
        );
        for item in &prepared {
            let GoalWritePreparation::New(item) = item else {
                unreachable!()
            };
            persist_prepared_goal_write(&mut tx, item).await?;
        }
        tx.commit().await?;
        let after: i64 = sqlx::query_scalar(
            "SELECT count(*)::bigint FROM proxiima_core.goal
                 WHERE request_id IN ('union-first', 'union-second')",
        )
        .fetch_one(pool)
        .await?;
        assert_eq!(after, 2);
        Ok(())
    }
    .await;
    let _ = drop_db(&db_name).await;
    result.expect("decomposition union lock test failed");
}

#[tokio::test]
async fn concurrent_successors_classify_stale_head_before_persisting() {
    let db_name = format!("proxiima_test_{}", Uuid::now_v7().simple());
    if let Err(e) = create_core_db(&db_name).await {
        panic!("PG required for tests but admin connect failed: {e}");
    }
    let url = db_url(&db_name);
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let pg = PgStorage::connect_for_tests(url.clone()).await?;
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let pool = pg.pool_for_tests();

        let mut create_tx = pool.begin().await?;
        let created = write_goal(
            &mut create_tx,
            &owner,
            crate::test_fixtures::writer(),
            &GoalWriteCommand {
                handle: None,
                schema_id: "core/task-v1".into(),
                title: "prior".into(),
                text: None,
                state: GoalState::Active,
                request_id: "successor-prior".into(),
                close_fact_t: None,

                motivated_by: vec![],
                dependency_t: vec![],
                evidence_t: vec![],
                mint_write_act: false,
                write_act_t: None,
                parent_handle: None,
            },
            1,
        )
        .await?;
        create_tx.commit().await?;

        let (second_err, _) = run_concurrent_successor_race(
            pool,
            &owner,
            created.handle,
            Some(created.t),
            "successor-first",
            "successor-second",
        )
        .await?;
        assert!(matches!(second_err, StorageError::Conflict(_)));

        // A low-level write without a named predecessor still uses its
        // prepared head snapshot, so the same race remains retryable.
        let (second_err, _) = run_concurrent_successor_race(
            pool,
            &owner,
            created.handle,
            None,
            "snapshot-first",
            "snapshot-second",
        )
        .await?;
        assert!(matches!(second_err, StorageError::Retryable(_)));
        Ok(())
    }
    .await;
    let _ = drop_db(&db_name).await;
    result.expect("concurrent Goal successors must serialize on the prior head");
}

#[tokio::test]
async fn low_level_write_act_is_reserved_until_union_lock_release() {
    let db_name = format!("proxiima_test_{}", Uuid::now_v7().simple());
    if let Err(e) = create_core_db(&db_name).await {
        panic!("PG required for tests but admin connect failed: {e}");
    }
    let url = db_url(&db_name);
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let pg = PgStorage::connect_for_tests(url.clone()).await?;
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let permit = OwnerWritePermit::admin_for_tests(owner);
        let target = pg.ingest_fact_atomic(&permit, &fact(), None).await?;
        let pool = pg.pool_for_tests();
        let mut tx = pool.begin().await?;
        let prepared = prepare_goal_write(
            &mut tx,
            &owner,
            crate::test_fixtures::writer(),
            &GoalWriteCommand {
                handle: None,
                schema_id: "core/task-v1".into(),
                title: "reserved write-act".into(),
                text: None,
                state: GoalState::Active,
                request_id: "reserved-write-act".into(),
                close_fact_t: None,

                motivated_by: vec![],
                dependency_t: vec![],
                evidence_t: vec![target.memory_id.into_inner()],
                mint_write_act: true,
                write_act_t: None,
                parent_handle: None,
            },
            None,
            1,
        )
        .await?;
        let GoalWritePreparation::New(prepared) = prepared else {
            unreachable!()
        };
        let write_act_t = prepared
            .reserved_write_act_t()
            .expect("reserved write-act t");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM proxiima_core.memory WHERE t = $1",)
                .bind(write_act_t)
                .fetch_one(&mut *tx)
                .await?,
            0,
            "reservation has no Memory row before the union lock is attempted"
        );
        let mut blocker = pool.begin().await?;
        lock_target(&mut blocker, target.memory_id.into_inner()).await?;
        let mut lock = Box::pin(lock_prepared_goal_write(&mut tx, &prepared));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut lock)
                .await
                .is_err(),
            "a blocked Goal union must not persist the reserved write-act"
        );
        blocker.commit().await?;
        lock.await?;
        persist_prepared_goal_write(&mut tx, &prepared).await?;
        tx.commit().await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM proxiima_core.memory WHERE t = $1",)
                .bind(write_act_t)
                .fetch_one(pool)
                .await?,
            1,
            "the exact reserved t is inserted after the union lock"
        );
        Ok(())
    }
    .await;
    let _ = drop_db(&db_name).await;
    result.expect("low-level write-act reservation test failed");
}

#[tokio::test]
async fn terminal_close_fact_is_reserved_until_union_lock_release() {
    let db_name = format!("proxiima_test_{}", Uuid::now_v7().simple());
    if let Err(e) = create_core_db(&db_name).await {
        panic!("PG required for tests but admin connect failed: {e}");
    }
    let url = db_url(&db_name);
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let pg = PgStorage::connect_for_tests(url.clone()).await?;
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let _permit = OwnerWritePermit::admin_for_tests(owner);
        let registry = SpiiceRegistry::new().freeze_or_panic_for_tests();
        let draft = GoalDraft {
            owner,
            schema_id: SchemaId::new(SimpleTextGoalV1::SCHEMA_ID.into()),
            schema_version: SchemaVersion::new(SimpleTextGoalV1::SCHEMA_VERSION),
            title: "reserved close".into(),
            text: "reserved close".into(),
            payload: Vec::new(),
            sidecar_payload: None,
            state: GoalState::Abandoned,
            topology: GoalTopologyWrite::new(Vec::new(), Vec::new(), Vec::new())?,
            supersedes_goal_id: None,
            authorship: GoalAuthorship::User,
            request_id: "reserved-close".into(),
        };
        let pool = pg.pool_for_tests();
        let mut tx = pool.begin().await?;
        let prepared = prepare_goal_insert(
            &mut tx,
            &draft,
            None,
            GoalChange::Set,
            GoalAtomicContext {
                registry: &registry,
                author_self_perspective_id: None,
                publication: None,
            },
            RowColumns {
                store_number: 1,
                writer: crate::test_fixtures::writer(),
                text: Some(&draft.text),
                write_act_t: None,
            },
        )
        .await?;
        let GoalWritePreparation::New(prepared_write) = &prepared.preparation else {
            unreachable!()
        };
        let close_fact_t = prepared_write
            .reserved_close_fact_t()
            .expect("terminal Goal reserves a close Fact t");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM proxiima_core.memory WHERE t = $1",)
                .bind(close_fact_t)
                .fetch_one(&mut *tx)
                .await?,
            0,
            "terminal close reservation has no Memory row before the union lock is attempted"
        );
        let mut blocker = pool.begin().await?;
        lock_target(&mut blocker, close_fact_t).await?;
        let mut lock = Box::pin(lock_prepared_goal_write(&mut tx, prepared_write));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut lock)
                .await
                .is_err(),
            "a blocked terminal Goal union must not persist its close Fact"
        );
        blocker.commit().await?;
        lock.await?;
        persist_prepared_goal_write(&mut tx, prepared_write).await?;
        tx.commit().await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM proxiima_core.memory WHERE t = $1",)
                .bind(close_fact_t)
                .fetch_one(pool)
                .await?,
            1,
            "the exact terminal close t is inserted after release"
        );
        Ok(())
    }
    .await;
    let _ = drop_db(&db_name).await;
    result.expect("terminal close reservation test failed");
}
