//! A real schema evolution and an erase of rows it migrates: a whole-owner
//! erase in both orders, and a series erase that starts while the evolution
//! runs. A gate trigger on `memory` parks the side that goes first inside its
//! transaction, holding every lock it took so far.

use super::*;
use proxiima_core::owner_inverse::{
    EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
};
use proxiima_core::storage_ports::OwnerInversePort;
use std::time::Duration;

const DELETE_GATE: i64 = 2_026_111_200_001;
const UPDATE_GATE: i64 = 2_026_111_200_002;
const PATIENCE: Duration = Duration::from_mins(1);

/// Two owners with two v1 Facts each, in a database migrated for v1.
struct Seeded {
    db: proxiima_pg_testkit::SplitRoleDb,
    pg: PgMigrationStorage,
    victim: OwnerId,
    bystander: OwnerId,
}

async fn seed() -> Seeded {
    let db = split_core_db::clone_split_core_db("proxiima_evolution_erase")
        .await
        .expect("PG required");
    let mut r1 = SpiiceRegistry::new();
    register_v1(&mut r1).unwrap();
    let r1 = r1.try_freeze().unwrap();
    let pg = PgMigrationStorage::connect_with_config(
        db.platform_url(),
        PgPoolConfig::default(),
        PgTuning::default(),
    )
    .await
    .unwrap();
    let f1 = sidecars_v1().freeze_against(&r1).unwrap();
    proxiima::run_core_and_spiice_migrations(&pg, AppV1::migrators(), &f1)
        .await
        .unwrap();
    pg.initialize_store_number(0).await.unwrap();
    let old = storage(db.runtime_url(), db.platform_url(), &r1, sidecars_v1()).await;
    let engine = proxiima_core::Engine::new(r1, Arc::new(old).storage_ports())
        .try_with_publication_config(PublicationConfig::new(
            PublicationSource::new("urn:proxiima:evolution").unwrap(),
        ))
        .unwrap();
    let admin = sqlx::PgPool::connect(&db.admin_url()).await.unwrap();
    let mut owners = Vec::new();
    for _ in 0..2 {
        let owner = OwnerId::new(Uuid::now_v7());
        let authz = authenticated_context(AuthzContext::for_subject_with_role(
            UserId::new(Uuid::now_v7()),
            [(owner, Role::admin())],
            AuthPath::HostBearer,
        ));
        proxiima_storage_pg::test_fixtures::create_user(&admin, UserId::new(owner.into_inner()))
            .await
            .unwrap();
        for n in 0..2 {
            engine
                .ingest_fact(
                    &authz,
                    FactWrite::new(
                        owner,
                        SOURCE_FEED,
                        &FactV1 {
                            task_id: Uuid::now_v7(),
                            ended_at: n,
                            text: format!("fact {n}"),
                        },
                    ),
                )
                .await
                .unwrap();
        }
        owners.push(owner);
    }
    admin.close().await;
    Seeded {
        db,
        pg,
        victim: owners[0],
        bystander: owners[1],
    }
}

/// The gates take their key shared, so after the test lets go the two sides
/// do not wait for each other here. The update gate fires only for a schema
/// change, which only the evolution makes.
async fn install_gates(admin: &sqlx::PgPool) {
    let mut delete = proxiima_core::SqlText::new(
        "CREATE FUNCTION public.race_gate_delete() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM pg_advisory_xact_lock_shared(",
    );
    delete.push_i64(DELETE_GATE);
    delete.push(
        "); RETURN OLD; END $$;
         CREATE TRIGGER a0_race_gate_delete BEFORE DELETE ON proxiima_core.memory
         FOR EACH ROW EXECUTE FUNCTION public.race_gate_delete();",
    );
    let mut update = proxiima_core::SqlText::new(
        "CREATE FUNCTION public.race_gate_update() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM pg_advisory_xact_lock_shared(",
    );
    update.push_i64(UPDATE_GATE);
    update.push(
        "); RETURN NEW; END $$;
         CREATE TRIGGER a0_race_gate_update BEFORE UPDATE ON proxiima_core.memory
         FOR EACH ROW WHEN (OLD.schema_id IS DISTINCT FROM NEW.schema_id)
         EXECUTE FUNCTION public.race_gate_update();",
    );
    for gate in [delete, update] {
        sqlx::raw_sql(gate).execute(admin).await.unwrap();
    }
}

/// Lock waits of the backends connected to this test's database. A wait for
/// a row shows as a transaction-id lock, which names no database, so the
/// backend's own database scopes the count: other tests share the cluster.
async fn ungranted_waiters(admin: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*)::bigint FROM pg_locks l JOIN pg_stat_activity a USING (pid)
          WHERE NOT l.granted AND a.datname = current_database()",
    )
    .fetch_one(admin)
    .await
    .unwrap()
}

async fn wait_for_waiters(admin: &sqlx::PgPool, at_least: i64) {
    tokio::time::timeout(PATIENCE, async {
        while ungranted_waiters(admin).await < at_least {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the expected backends wait");
}

/// The second side either finishes on its own or parks behind the first.
async fn settle<T>(admin: &sqlx::PgPool, second: &tokio::task::JoinHandle<T>, parked: i64) {
    tokio::time::timeout(PATIENCE, async {
        while !second.is_finished() && ungranted_waiters(admin).await < parked + 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the second side finishes or waits");
}

async fn hold(admin: &sqlx::PgPool, key: i64) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
    let mut connection = admin.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(key)
        .execute(&mut *connection)
        .await
        .unwrap();
    connection
}

async fn release(mut connection: sqlx::pool::PoolConnection<sqlx::Postgres>, key: i64) {
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut *connection)
        .await
        .unwrap();
}

/// Replace each source with a new target row: inserts that pin, and deletes
/// of the superseded rows.
fn evolve_replace(
    input: &proxiima::spiice::MigrationInput,
    plan: &mut proxiima::spiice::MigrationPlan,
) -> Result<(), proxiima_core::StorageError> {
    for source in input.memories() {
        if let Some(old) = source.payload::<FactV1>() {
            let target = plan.fact(
                source,
                FactV2 {
                    task_id: old.task_id,
                    ended_at: old.ended_at,
                    text: old.text.clone(),
                },
                MigrationPlacement::NEW,
                MigrationPins::default(),
            )?;
            plan.map_memory(source, target, &[])?;
        }
    }
    Ok(())
}

struct AppV2Replace;
impl SpiiceBundle for AppV2Replace {
    fn register(r: &mut SpiiceRegistry) -> Result<(), SpiiceRegistryError> {
        register_v2(r)
    }
    fn register_pg_sidecars(r: &mut PgSidecarRegistry) {
        r.add_fact::<FactV2>();
    }
    fn migrators() -> Vec<NamedMigrator> {
        vec![NamedMigrator::new(SPIICE, migrator(evolve_replace))]
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one scenario: seed, gate, race, assert"
)]
async fn evolution_and_owner_erase(erase_first: bool, replace: bool) {
    let seeded = seed().await;
    let mut r2 = SpiiceRegistry::new();
    register_v2(&mut r2).unwrap();
    let r2 = r2.try_freeze().unwrap();
    let sidecars = |replace: bool| {
        if replace {
            sidecars_v2_with(evolve_replace)
        } else {
            sidecars_v2()
        }
    };
    let f2 = sidecars(replace).freeze_against(&r2).unwrap();
    let admin = sqlx::PgPool::connect(&seeded.db.admin_url()).await.unwrap();
    install_gates(&admin).await;
    let erasing = Arc::new(
        storage(
            seeded.db.runtime_url(),
            seeded.db.platform_url(),
            &r2,
            sidecars(replace),
        )
        .await,
    );
    let surfaces = OwnerSurfaces::for_registry(&r2);
    let pg = Arc::new(seeded.pg);
    let victim = seeded.victim;

    let erase = {
        let erasing = erasing.clone();
        move || {
            tokio::spawn(async move {
                let user = UserId::new(victim.into_inner());
                let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
                    user_id: user,
                    drop_event_id: "evolution-race".into(),
                });
                erasing.erase_personal_owner(&auth, user, &surfaces).await
            })
        }
    };
    let evolve = {
        let (pg, f2) = (pg.clone(), f2.clone());
        move || {
            tokio::spawn(async move {
                let migrators = if replace {
                    AppV2Replace::migrators()
                } else {
                    AppV2::migrators()
                };
                proxiima::run_core_and_spiice_migrations(pg.as_ref(), migrators, &f2).await
            })
        }
    };

    let (erased, evolved);
    if erase_first {
        let gate = hold(&admin, DELETE_GATE).await;
        let first = erase();
        wait_for_waiters(&admin, 1).await;
        let second = evolve();
        settle(&admin, &second, 1).await;
        release(gate, DELETE_GATE).await;
        erased = first.await.unwrap();
        evolved = second.await.unwrap();
    } else {
        // A retaining evolution rewrites schema ids in place; a replacing one
        // deletes the superseded rows.
        let evolution_gate = if replace { DELETE_GATE } else { UPDATE_GATE };
        let gate = hold(&admin, evolution_gate).await;
        let first = evolve();
        wait_for_waiters(&admin, 1).await;
        let second = erase();
        settle(&admin, &second, 1).await;
        release(gate, evolution_gate).await;
        evolved = first.await.unwrap();
        erased = second.await.unwrap();
    }
    evolved.expect("the evolution finishes");
    assert!(matches!(
        erased.expect("the erase finishes"),
        OwnerEraseOutcome::Completed { .. }
    ));

    let rows = |owner: OwnerId| {
        let admin = admin.clone();
        async move {
            sqlx::query_as::<_, (i64, i64, i64)>(
                "SELECT (SELECT count(*) FROM proxiima_core.memory WHERE owner_id = $1),
                        (SELECT count(*) FROM proxiima_core.memory
                          WHERE owner_id = $1 AND schema_id = $2),
                        (SELECT count(*) FROM evolution_test.fact_v2 f
                           JOIN proxiima_core.memory m ON m.t = f.t WHERE m.owner_id = $1)",
            )
            .bind(owner.into_inner())
            .bind(TARGET_ID)
            .fetch_one(&admin)
            .await
            .unwrap()
        }
    };
    assert_eq!(
        rows(seeded.victim).await,
        (0, 0, 0),
        "nothing of the erased owner is left"
    );
    assert_eq!(
        rows(seeded.bystander).await,
        (2, 2, 2),
        "the other owner's rows are evolved and intact"
    );
    let leftovers: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM evolution_test.fact_v1) + (SELECT count(*) FROM evolution_test.fact_v2 f
                  WHERE NOT EXISTS (SELECT 1 FROM proxiima_core.memory m WHERE m.t = f.t))",
    )
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(
        leftovers, 0,
        "no sidecar row outlives its memory or its schema"
    );
    admin.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_whole_owner_erase_in_flight_and_a_schema_evolution_both_finish() {
    evolution_and_owner_erase(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_schema_evolution_in_flight_and_a_whole_owner_erase_both_finish() {
    evolution_and_owner_erase(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_whole_owner_erase_in_flight_and_a_replacing_schema_evolution_both_finish() {
    evolution_and_owner_erase(true, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replacing_schema_evolution_in_flight_and_a_whole_owner_erase_both_finish() {
    evolution_and_owner_erase(false, true).await;
}

/// A series erase that starts while a schema evolution holds the tables waits
/// for them before it takes its owner's fence or any other advisory lock, so
/// the evolution never waits for the erase.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_series_erase_waits_for_a_schema_evolution_before_it_takes_a_fence() {
    let seeded = seed().await;
    let mut r2 = SpiiceRegistry::new();
    register_v2(&mut r2).unwrap();
    let r2 = r2.try_freeze().unwrap();
    let f2 = sidecars_v2().freeze_against(&r2).unwrap();
    let admin = sqlx::PgPool::connect(&seeded.db.admin_url()).await.unwrap();
    install_gates(&admin).await;
    let victim = seeded.victim;
    let target: Uuid = sqlx::query_scalar(
        "SELECT t FROM proxiima_core.memory WHERE owner_id = $1 ORDER BY t LIMIT 1",
    )
    .bind(victim.into_inner())
    .fetch_one(&admin)
    .await
    .unwrap();
    let active = storage(
        seeded.db.runtime_url(),
        seeded.db.platform_url(),
        &r2,
        sidecars_v2(),
    )
    .await;
    let (engine, authority) =
        proxiima_core::Engine::new(r2, Arc::new(active).storage_ports()).into_system_authority();
    let pg = Arc::new(seeded.pg);

    let gate = hold(&admin, UPDATE_GATE).await;
    let evolution = tokio::spawn(async move {
        proxiima::run_core_and_spiice_migrations(pg.as_ref(), AppV2::migrators(), &f2).await
    });
    wait_for_waiters(&admin, 1).await;
    let erase = tokio::spawn(async move {
        let mut unit = engine.system_unit_of_work(&authority).unwrap();
        unit.erase_own_series(
            SPIICE,
            victim,
            proxiima::spiice::SeriesSelection::Ids(vec![proxiima_core::MemoryId::new(target)]),
            proxiima::spiice::EraseMode::Erase,
        )
        .await
        .expect("the series erase runs");
        unit.commit().await.expect("the series erase commits");
    });
    // Advisory locks the backends of this database hold while they wait for a
    // table: none, once the erase is the one that waits.
    let held_by_table_waiters = tokio::time::timeout(PATIENCE, async {
        loop {
            let (waiting, held): (i64, i64) = sqlx::query_as(
                "WITH here AS (SELECT * FROM pg_locks WHERE database =
                         (SELECT oid FROM pg_database WHERE datname = current_database()))
                 SELECT (SELECT count(*) FROM here WHERE NOT granted AND locktype = 'relation'),
                        (SELECT count(*) FROM here waiting JOIN here held USING (pid)
                          WHERE NOT waiting.granted AND waiting.locktype = 'relation'
                            AND held.granted AND held.locktype = 'advisory')",
            )
            .fetch_one(&admin)
            .await
            .unwrap();
            if waiting > 0 {
                break held;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the erase waits for the evolution's tables");
    assert_eq!(
        held_by_table_waiters, 0,
        "the erase holds no fence while it waits"
    );
    release(gate, UPDATE_GATE).await;
    evolution.await.unwrap().expect("the evolution finishes");
    erase.await.expect("the erase finishes");

    let left: Vec<(Uuid, bool)> = sqlx::query_as(
        "SELECT owner_id, bool_and(schema_id = $1) FROM proxiima_core.memory
          WHERE t <> $2 GROUP BY owner_id ORDER BY count(*)",
    )
    .bind(TARGET_ID)
    .bind(target)
    .fetch_all(&admin)
    .await
    .unwrap();
    let erased: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM proxiima_core.memory WHERE t = $1)
              + (SELECT count(*) FROM evolution_test.fact_v2 WHERE t = $1)",
    )
    .bind(target)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(erased, 0, "the erased Fact and its sidecar row are gone");
    assert_eq!(
        left,
        vec![
            (victim.into_inner(), true),
            (seeded.bystander.into_inner(), true)
        ],
        "every other Fact is evolved: one of the erased series' owner, two of the other"
    );
    admin.close().await;
}
