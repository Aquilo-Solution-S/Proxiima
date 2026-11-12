use super::*;
use proxiima::MigrationError;
use proxiima_core::env_from_pairs as env;
use proxiima_pg_testkit::{create_db, db_url, drop_db, split_role_urls, unique_db_name};
use sqlx::migrate::MigrateError;
use std::error::Error as _;

const RESET_CONFIRM_ENV: &str = super::RESET_CONFIRM_ENV.name();

fn parsed(argv: &[&str]) -> Result<Args, String> {
    parse_args(argv.iter().map(|arg| (*arg).to_string()))
}

#[test]
fn a_blank_variable_is_unset_and_a_padded_one_is_trimmed() {
    let args = Args::default();
    for blank in ["", "  \t "] {
        let error = resolve_database_url(&args, &env(&[("DATABASE_URL", blank)]))
            .expect_err("blank is unset");
        assert!(
            error.to_string().contains("database URL required"),
            "{error}"
        );
        let error =
            store_number(&env(&[("PROXIIMA_STORE_NUMBER", blank)])).expect_err("blank is unset");
        assert!(error.to_string().contains("is required"), "{error}");
    }
    assert_eq!(
        resolve_database_url(&args, &env(&[("DATABASE_URL", " postgres://u@h/d ")]))
            .expect("padded url"),
        "postgres://u@h/d"
    );
    assert_eq!(
        store_number(&env(&[("PROXIIMA_STORE_NUMBER", " 3 ")])).expect("padded number"),
        3
    );
    let flag = Args {
        database_url: Some("postgres://flag@h/d".into()),
        ..Args::default()
    };
    assert_eq!(
        resolve_database_url(&flag, &env(&[("DATABASE_URL", "postgres://env@h/d")]))
            .expect("flag wins"),
        "postgres://flag@h/d"
    );
}

#[test]
fn a_reset_runs_only_on_the_exact_confirmation_token() {
    let token = RESET_CONFIRM_VALUE;
    require_reset_confirmation(&env(&[(RESET_CONFIRM_ENV, token)])).expect("exact token");
    for refused in [None, Some(""), Some("yes"), Some(" reset-my-dev-db")] {
        let pairs = refused.map(|value| [(RESET_CONFIRM_ENV, value)]);
        let lookup = env(pairs.as_ref().map_or(&[], |pairs| pairs.as_slice()));
        let error = require_reset_confirmation(&lookup).expect_err("not the token");
        assert!(error.to_string().contains(RESET_CONFIRM_ENV), "{error}");
    }
}

#[test]
fn the_command_line_is_read_once_and_a_stranger_is_refused() {
    assert_eq!(parsed(&[]).expect("no arguments"), Args::default());
    let all = parsed(&[
        "--reset",
        "--reset-dependent-lanes",
        "--host-vector-index",
        "--database-url",
        "postgres://u@h/d",
    ])
    .expect("every flag");
    assert!(all.reset && all.reset_dependent_lanes && all.host_vector_index);
    assert_eq!(all.database_url.as_deref(), Some("postgres://u@h/d"));
    let joined = parsed(&["--database-url=postgres://u@h/d"]).expect("the = spelling");
    assert_eq!(joined.database_url.as_deref(), Some("postgres://u@h/d"));

    // A misspelled flag must not run as the plain migration.
    let error = parsed(&["--rset"]).expect_err("unknown flag");
    assert!(error.contains("unknown argument --rset"), "{error}");
    assert!(parsed(&["--reset", "extra"]).is_err());
    // The value of --database-url is a value, never the next flag.
    assert!(parsed(&["--database-url"]).is_err());
    assert!(parsed(&["--database-url", "--reset"]).is_err());
}

/// The migration error a refused migrate step carries.
fn migration_cause(refused: &proxiima::ProxiimaError) -> Option<&MigrationError> {
    refused.source()?.downcast_ref()
}

/// The tool's migrate step on `platform_url`, store number 0.
async fn run_compiled_migrations(platform_url: &str) -> Result<(), proxiima::ProxiimaError> {
    compiled_app(platform_url, 0, false)
        .configure(|b| b.missing_migration_policy(proxiima::MissingMigrationPolicy::Reject))
        .migrate()
        .await
}

// A date-shaped ledger row no compiled migrator recognizes — e.g. a
// spiice lane retired before the version-namespace rules existed. The
// reset cannot attribute it, so it must survive and block strict migration
// rather than be deleted by guesswork.
const UNATTRIBUTABLE_TIMESTAMP_MIGRATION_VERSION: i64 = 20_260_622_000_000;

/// A schema left by a spiice no longer compiled in is reported as
/// unowned; the compiled spiices' schemas are not.
#[tokio::test]
async fn reset_reports_a_leftover_retired_spiice_schema_as_unowned() {
    let db_name = unique_db_name("proxiima_dev_migrate_unowned");
    create_db(&db_name).await.expect("PG required for tests");
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let pool = sqlx::PgPool::connect(&db_url(&db_name)).await?;
        sqlx::raw_sql(
            "CREATE SCHEMA proxiima_core; CREATE SCHEMA proxiima_example; \
                 CREATE SCHEMA proxiima_retired",
        )
        .execute(&pool)
        .await?;
        assert_eq!(
            unowned_proxiima_schemas(&pool, &compiled_schema_names()?).await?,
            ["proxiima_retired"]
        );
        pool.close().await;
        Ok(())
    }
    .await;
    let _ = drop_db(&db_name).await;
    result.expect("unowned schema report");
}

/// The example spiice's own ledger, which the reset test seeds next to
/// the shared one: the reset attributes it to the compiled spiice and
/// drops it with its schema.
async fn assert_example_ledger_dropped(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    let exists: bool = sqlx::query_scalar(
        "SELECT to_regclass('public._sqlx_migrations_proxiima_example') IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    assert!(!exists, "reset drops the compiled spiice's ledger");
    Ok(())
}

#[tokio::test]
async fn reset_deletes_core_namespace_rows_and_reports_leftovers() {
    let db_name = unique_db_name("proxiima_dev_migrate_reset");
    create_db(&db_name).await.expect("PG required for tests");
    let url = db_url(&db_name);
    let (_, platform_url) = split_role_urls(&db_name).await.expect("split role URLs");

    let result: Result<(), Box<dyn std::error::Error>> = async {
        let pg = proxiima_storage_pg::PgMigrationStorage::connect_with_config(
            &platform_url,
            PgPoolConfig::from_env()?,
            PgTuning::from_env()?,
        )
        .await?;
        sqlx::query("CREATE SCHEMA proxiima_core")
            .execute(&pg.clone_pool_for_backend())
            .await?;
        sqlx::query("CREATE SCHEMA proxiima_example")
            .execute(&pg.clone_pool_for_backend())
            .await?;
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS public._sqlx_migrations (
                    version bigint PRIMARY KEY,
                    description text NOT NULL,
                    installed_on timestamptz NOT NULL DEFAULT now(),
                    success boolean NOT NULL,
                    checksum bytea NOT NULL,
                    execution_time bigint NOT NULL
                );
                CREATE TABLE public._sqlx_migrations_proxiima_example
                    (LIKE public._sqlx_migrations INCLUDING ALL);",
        )
        .execute(&pg.clone_pool_for_backend())
        .await?;
        for version in [
            1_i64,
            2,
            3,
            4,
            5,
            6,
            7,
            // Orphaned draft rows from a squashed dev-cycle lane fall in
            // the core namespace and must go without being enumerated.
            12,
            13,
            14,
            15,
            UNATTRIBUTABLE_TIMESTAMP_MIGRATION_VERSION,
        ] {
            sqlx::query(
                "INSERT INTO public._sqlx_migrations
                        (version, description, success, checksum, execution_time)
                     VALUES ($1, 'old', true, decode('00', 'hex'), 0)",
            )
            .bind(version)
            .execute(&pg.clone_pool_for_backend())
            .await?;
        }

        reset_local_dev_database_confirmed(&pg, &url, DependentLanes::Refuse).await?;

        assert_example_ledger_dropped(&pg.clone_pool_for_backend()).await?;
        let remaining: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM public._sqlx_migrations ORDER BY version")
                .fetch_all(&pg.clone_pool_for_backend())
                .await?;
        assert_eq!(
            remaining,
            vec![UNATTRIBUTABLE_TIMESTAMP_MIGRATION_VERSION],
            "reset must delete every core-namespace row and keep only the \
                 unattributable leftover"
        );

        let refused = run_compiled_migrations(&platform_url)
            .await
            .expect_err("unattributable versions block migration after reset");
        assert!(
            matches!(
                migration_cause(&refused),
                Some(MigrationError::Core(MigrateError::VersionMissing(
                    UNATTRIBUTABLE_TIMESTAMP_MIGRATION_VERSION
                )))
            ),
            "{refused:?}"
        );
        let core_exists: bool =
            sqlx::query_scalar("SELECT to_regnamespace('proxiima_core') IS NOT NULL")
                .fetch_one(&pg.clone_pool_for_backend())
                .await?;
        assert!(
            !core_exists,
            "strict refusal recreated no schema after reset"
        );
        reset_local_dev_database_confirmed(&pg, &url, DependentLanes::Reset).await?;
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM public._sqlx_migrations")
            .fetch_one(&pg.clone_pool_for_backend())
            .await?;
        assert_eq!(
            remaining, 0,
            "explicit lane reset clears unowned versions even without foreign dependents"
        );
        run_compiled_migrations(&platform_url).await?;
        Ok(())
    }
    .await;

    let _ = drop_db(&db_name).await;
    result.expect("dev reset retired-row regression failed");
}

/// A consumer lane's row in the shared ledger: timestamp-shaped, in the
/// downstream-host suffix lane (lane ordering: crates/proxiima/src/migrations.rs; see docs/host-integration.md).
const CONSUMER_LANE_VERSION: i64 = 20_260_924_000_060;

/// What a consumer lane embedding Proxiima builds on `proxiima_core`: an FK
/// and a view that depend on it, and a trigger on its table whose routine
/// lives in the consumer's schema.
const CONSUMER_LANE: &str = "
        CREATE SCHEMA consumer;
        CREATE TABLE consumer.orders (
            id bigint PRIMARY KEY,
            memory_t uuid NOT NULL REFERENCES proxiima_core.memory (t)
        );
        CREATE VIEW consumer.memory_ids AS SELECT t FROM proxiima_core.memory;
        CREATE FUNCTION consumer.audit() RETURNS trigger
            LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
        CREATE TRIGGER consumer_audit AFTER INSERT ON proxiima_core.memory
            FOR EACH ROW EXECUTE FUNCTION consumer.audit();";

/// `CONSUMER_LANE` with its row in the shared ledger, plus a second lane
/// composed through the facade that keeps its own ledger, written by the
/// same migration role.
async fn build_consumer_lanes(
    pg: &PgMigrationStorage,
    admin: &sqlx::PgPool,
) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(CONSUMER_LANE).execute(admin).await?;
    sqlx::query(
        "INSERT INTO public._sqlx_migrations
                (version, description, success, checksum, execution_time)
             VALUES ($1, 'consumer orders', true, decode('00', 'hex'), 0)",
    )
    .bind(CONSUMER_LANE_VERSION)
    .execute(admin)
    .await?;
    sqlx::raw_sql(
        "CREATE TABLE public._sqlx_migrations_consumer
                 (LIKE public._sqlx_migrations INCLUDING ALL);
             INSERT INTO public._sqlx_migrations_consumer
                 (version, description, success, checksum, execution_time)
             VALUES (20260924000061, 'consumer audit', true, '\\x00', 0);",
    )
    .execute(&pg.clone_pool_for_backend())
    .await?;
    Ok(())
}

#[tokio::test]
async fn reset_refuses_to_cascade_into_a_consumer_lane_unless_asked() {
    let db_name = unique_db_name("proxiima_dev_migrate_reset_consumer");
    create_db(&db_name).await.expect("PG required for tests");
    let url = db_url(&db_name);
    let (_, platform_url) = split_role_urls(&db_name).await.expect("split role URLs");

    let result: Result<(), Box<dyn std::error::Error>> = async {
        let pg = proxiima_storage_pg::PgMigrationStorage::connect_with_config(
            &platform_url,
            PgPoolConfig::from_env()?,
            PgTuning::from_env()?,
        )
        .await?;
        run_compiled_migrations(&platform_url).await?;
        let admin = sqlx::PgPool::connect(&url).await?;
        build_consumer_lanes(&pg, &admin).await?;

        let ledger = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT version FROM public._sqlx_migrations
                      UNION ALL
                     SELECT version FROM public._sqlx_migrations_consumer
                      ORDER BY 1",
            )
            .fetch_all(&admin)
            .await
        };
        let consumer_fks = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pg_constraint
                      WHERE conrelid = 'consumer.orders'::regclass AND contype = 'f'",
            )
            .fetch_one(&admin)
            .await
        };
        let core_exists = || async {
            sqlx::query_scalar::<_, bool>("SELECT to_regnamespace('proxiima_core') IS NOT NULL")
                .fetch_one(&admin)
                .await
        };

        // Plain reset: refuses before dropping or deleting anything, and
        // names every object the CASCADE would have taken.
        let before = ledger().await?;
        let refusal = reset_local_dev_database_confirmed(&pg, &url, DependentLanes::Refuse)
            .await
            .expect_err("a consumer lane depends on proxiima_core")
            .to_string();
        for named in [
            "constraint orders_memory_t_fkey on table consumer.orders",
            "view consumer.memory_ids",
            "trigger consumer_audit on table proxiima_core.memory",
        ] {
            assert!(refusal.contains(named), "{named} missing from: {refusal}");
        }
        assert!(core_exists().await?, "refusal must drop nothing");
        assert_eq!(
            consumer_fks().await?,
            1,
            "refusal must keep the consumer FK"
        );
        assert_eq!(ledger().await?, before, "refusal must delete no ledger row");

        // Opt-in over a lane ledger this role cannot delete from: refused
        // up front, so it never stops halfway.
        sqlx::query("CREATE TABLE public._sqlx_migrations_foreign (version bigint)")
            .execute(&admin)
            .await?;
        let refusal = reset_local_dev_database_confirmed(&pg, &url, DependentLanes::Reset)
            .await
            .expect_err("an undeletable lane ledger must refuse the opt-in")
            .to_string();
        assert!(
            refusal.contains("public._sqlx_migrations_foreign"),
            "{refusal}"
        );
        assert!(core_exists().await?, "the refused opt-in must drop nothing");
        assert_eq!(
            ledger().await?,
            before,
            "the refused opt-in must delete no row"
        );
        sqlx::query("DROP TABLE public._sqlx_migrations_foreign")
            .execute(&admin)
            .await?;

        // Opt-in: the dependents go with Proxiima's schemas, and so does
        // every ledger row no Proxiima lane owns, shared or per-lane —
        // none is left claiming an object that no longer exists.
        reset_local_dev_database_confirmed(&pg, &url, DependentLanes::Reset).await?;
        assert!(!core_exists().await?, "proxiima_core is dropped");
        assert_eq!(consumer_fks().await?, 0, "the consumer FK went with it");
        assert!(
            ledger().await?.is_empty(),
            "no orphaned ledger row may survive the opt-in reset"
        );

        // Proxiima's own lane is not a dependent of itself, nor is a
        // default ACL scoped to one of its schemas: a freshly migrated
        // database resets without the opt-in.
        run_compiled_migrations(&platform_url).await?;
        sqlx::query(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA proxiima_core GRANT SELECT ON TABLES TO PUBLIC",
        )
        .execute(&admin)
        .await?;
        reset_local_dev_database_confirmed(&pg, &url, DependentLanes::Refuse).await?;
        run_compiled_migrations(&platform_url).await?;
        Ok(())
    }
    .await;

    let _ = drop_db(&db_name).await;
    result.expect("dev reset consumer-lane regression failed");
}
#[tokio::test]
async fn explicit_lane_reset_checks_ledger_permissions_without_foreign_dependents() {
    let db = proxiima_pg_testkit::SplitRoleDb::create("proxiima_reset_ledger_permission", &[])
        .await
        .expect("owned PG required");
    let pg = PgMigrationStorage::connect_with_config(
        db.platform_url(),
        PgPoolConfig::default(),
        PgTuning::default(),
    )
    .await
    .expect("migration storage");
    let admin = sqlx::PgPool::connect(&db.admin_url())
        .await
        .expect("admin pool");
    sqlx::query("CREATE SCHEMA proxiima_reset_guard")
        .execute(&pg.clone_pool_for_backend())
        .await
        .expect("reset marker");
    sqlx::query("CREATE TABLE public._sqlx_migrations_guard (version bigint)")
        .execute(&admin)
        .await
        .expect("ledger owned by another role");
    let refused = reset_local_dev_database_confirmed(&pg, &db.admin_url(), DependentLanes::Reset)
        .await
        .expect_err("reset cannot clear the foreign ledger");
    assert!(
        refused
            .to_string()
            .contains("public._sqlx_migrations_guard"),
        "{refused}"
    );
    let marker_exists: bool =
        sqlx::query_scalar("SELECT to_regnamespace('proxiima_reset_guard') IS NOT NULL")
            .fetch_one(&admin)
            .await
            .expect("marker probe");
    assert!(marker_exists, "permission refusal dropped the schema");
    admin.close().await;
}
