//! The generic rule a spiice lane's ledger relies on, through the example
//! spiice's migrations.

use proxiima_core::{PgIdent, SqlText};

/// `SQLx` records each spiice migration as SHA-384 of its file minus the
/// `ignored-chars` of the spiice's `sqlx.toml` (`\r`). The released file
/// bytes are pinned by `scripts/check-migrations.py`; this pins the
/// computation, which an `ignored-chars` edit or an `SQLx` upgrade would
/// change for every deployed ledger at once.
#[test]
fn embedded_spiice_checksums_are_sha384_of_the_files_without_carriage_returns() {
    use sha2::{Digest, Sha384};

    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spiices/example/migrations");
    let migrator = proxiima_example::migrator();
    let mut checked = 0;
    for entry in std::fs::read_dir(&directory).expect("spiice migrations directory") {
        let path = entry.expect("migration entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let Some(version) = name
            .strip_suffix(".sql")
            .and_then(|stem| stem.split('_').next())
            .and_then(|version| version.parse::<i64>().ok())
        else {
            continue;
        };
        let bytes: Vec<u8> = std::fs::read(&path)
            .expect("readable migration")
            .into_iter()
            .filter(|byte| *byte != b'\r')
            .collect();
        let embedded = migrator
            .iter()
            .find(|migration| migration.version == version)
            .unwrap_or_else(|| panic!("{name} is not embedded"));
        assert_eq!(
            embedded.checksum.as_ref(),
            &Sha384::digest(&bytes)[..],
            "{name}: SQLx checksum computation changed"
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        migrator.iter().count(),
        "every embedded migration checked"
    );
}

#[path = "fixtures/example_engine.rs"]
#[allow(
    dead_code,
    reason = "migration regressions use only the frozen sidecars"
)]
mod rename_example_engine;

async fn rename_ledger_snapshot(
    connection: &mut sqlx::PgConnection,
    ledger: &str,
) -> Vec<(i64, Vec<u8>)> {
    let mut select = SqlText::new("SELECT version, checksum FROM ");
    select
        .push_ident(&PgIdent::table(ledger).expect("a plain ledger name"))
        .push(" ORDER BY version");
    sqlx::query_as(select)
        .fetch_all(connection)
        .await
        .expect("migration ledger snapshot")
}

async fn rename_schema_oid(connection: &mut sqlx::PgConnection, schema: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT oid::bigint FROM pg_namespace WHERE nspname=$1")
        .bind(schema)
        .fetch_optional(connection)
        .await
        .expect("schema identity")
}

#[tokio::test]
async fn failing_late_historical_lane_rolls_back_catalog_and_ledgers() {
    use sqlx::SqlSafeStr;
    use sqlx::migrate::{Migration, MigrationType, Migrator};
    use std::borrow::Cow;

    let db = proxiima_pg_testkit::SplitRoleDb::create("proxiima_late_history_rollback", &[])
        .await
        .expect("owned test database");
    let pg = proxiima::PgMigrationStorage::connect_with_config(
        db.platform_url(),
        proxiima::PgPoolConfig::default(),
        proxiima::PgTuning::default(),
    )
    .await
    .expect("migration storage");
    pg.run_migrations().await.expect("finalized core");
    let pool = pg.clone_pool_for_backend();
    let mut connection = pool.acquire().await.expect("migration connection");
    let core_before = rename_ledger_snapshot(&mut connection, "public._sqlx_migrations").await;
    let schema_before = rename_schema_oid(&mut connection, "proxiima_core").await;
    drop(connection);

    let original = proxiima_example::migrator();
    let mut migrations = original.iter().cloned().collect::<Vec<_>>();
    let first = &migrations[0];
    let mut failing = SqlText::from(first.sql.clone());
    failing.push("\nSELECT 1 / 0;");
    migrations[0] = Migration::new(
        first.version,
        first.description.clone(),
        MigrationType::Simple,
        failing.into_sql_str(),
        false,
    );
    let migrator = Migrator {
        migrations: Cow::Owned(migrations),
        ..original
    };
    let error = proxiima::run_core_and_spiice_migrations(
        &pg,
        [proxiima::NamedMigrator::spiice(
            "proxiima-example",
            migrator,
        )],
        &rename_example_engine::frozen_sidecars(),
    )
    .await
    .expect_err("injected historical failure");
    assert!(
        matches!(error, proxiima::MigrationError::Spiice {
        err: sqlx::migrate::MigrateError::ExecuteMigration(ref error, _), ..
    } if error.as_database_error().and_then(sqlx::error::DatabaseError::code).as_deref() == Some("22012")),
        "historical SQL must run before the injected failure: {error}"
    );

    let mut connection = pool.acquire().await.expect("migration connection");
    assert_eq!(
        rename_ledger_snapshot(&mut connection, "public._sqlx_migrations").await,
        core_before
    );
    assert_eq!(
        rename_schema_oid(&mut connection, "proxiima_core").await,
        schema_before
    );
    assert!(
        rename_schema_oid(
            &mut connection,
            &proxiima_storage_pg::historical_catalog_name("proxiima_core")
        )
        .await
        .is_none()
    );
    for schema in [
        "proxiima_example".to_owned(),
        proxiima_storage_pg::historical_catalog_name("proxiima_example"),
    ] {
        assert!(rename_schema_oid(&mut connection, &schema).await.is_none());
    }
    for ledger in [
        "public._sqlx_migrations_proxiima_example".to_owned(),
        proxiima_storage_pg::historical_catalog_name("public._sqlx_migrations_proxiima_example"),
    ] {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(ledger)
            .fetch_one(&mut *connection)
            .await
            .expect("consumer ledger absence");
        assert!(!exists, "a failed historical lane must leave no ledger");
    }
}

/// Give an installed historical store the role shapes a released v0.0.x store
/// has, which the replay's role rebinding does not reproduce: its search
/// functions owned by the historical search role itself, and a policy whose
/// TO list names an operator role spelled with the historical product word.
async fn stage_released_role_shapes(
    database: &str,
    old_core: &str,
    old_example: &str,
    platform: &str,
) {
    use sqlx::Connection as _;

    let historical_search = proxiima_storage_pg::historical_catalog_name("proxiima_search");
    let operator_role = proxiima_storage_pg::historical_catalog_name("proxiima_rename_operator");
    let mut admin = sqlx::PgConnection::connect(&proxiima_pg_testkit::db_url(database))
        .await
        .expect("superuser connection");
    let name = |value: &str| SqlText::from(&PgIdent::name(value).expect("a plain catalog name"));
    let shapes = SqlText::fill(
        "DO $roles$ BEGIN
             IF to_regrole('{historical_search}') IS NULL THEN
                 CREATE ROLE {historical_search} NOLOGIN NOINHERIT;
             END IF;
             IF to_regrole('{operator_role}') IS NULL THEN
                 CREATE ROLE {operator_role} NOLOGIN;
             END IF;
         END $roles$;
         GRANT {historical_search} TO {platform} WITH ADMIN FALSE, INHERIT FALSE, SET TRUE;
         GRANT USAGE ON SCHEMA {old_core} TO {historical_search};
         REASSIGN OWNED BY proxiima_search TO {historical_search};
         ALTER DEFAULT PRIVILEGES FOR ROLE {historical_search} GRANT EXECUTE ON FUNCTIONS TO {platform};
         CREATE POLICY rename_operator ON {old_example}.settings TO {operator_role} USING (true);",
        &[
            ("{historical_search}", &name(&historical_search)),
            ("{operator_role}", &name(&operator_role)),
            (
                "{platform}",
                &SqlText::from(&PgIdent::quoted(platform).expect("the platform role name")),
            ),
            ("{old_core}", &name(old_core)),
            ("{old_example}", &name(old_example)),
        ],
    );
    sqlx::raw_sql(shapes)
        .execute(&mut admin)
        .await
        .expect("released role shapes");
    let owned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = $1 AND p.proowner = to_regrole($2)",
    )
    .bind(old_core)
    .bind(&historical_search)
    .fetch_one(&mut admin)
    .await
    .expect("historical search functions");
    assert!(
        owned > 0,
        "the historical search role owns the search functions"
    );
}

/// After the upgrade the historical search role owns nothing in the store, the
/// search functions belong to `proxiima_search`, and the operator policy still
/// names its operator role.
async fn assert_released_role_shapes_upgraded(connection: &mut sqlx::PgConnection) {
    let historical_search = proxiima_storage_pg::historical_catalog_name("proxiima_search");
    let operator_role = proxiima_storage_pg::historical_catalog_name("proxiima_rename_operator");
    let still_owned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_shdepend s JOIN pg_database d ON d.oid = s.dbid
          WHERE d.datname = current_database() AND s.refobjid = to_regrole($1) AND s.deptype = 'o'",
    )
    .bind(&historical_search)
    .fetch_one(&mut *connection)
    .await
    .expect("historical search ownership");
    assert_eq!(still_owned, 0, "the historical search role is handed over");
    let search_functions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'proxiima_core' AND p.proowner = 'proxiima_search'::regrole",
    )
    .fetch_one(&mut *connection)
    .await
    .expect("current search functions");
    assert!(
        search_functions > 0,
        "proxiima_search owns the search functions"
    );
    let roles: Vec<String> = sqlx::query_scalar(
        "SELECT r.rolname::text FROM pg_policy p, unnest(p.polroles) role_oid
           JOIN pg_roles r ON r.oid = role_oid
          WHERE p.polname = 'rename_operator'
            AND p.polrelid = 'proxiima_example.settings'::regclass",
    )
    .fetch_all(&mut *connection)
    .await
    .expect("operator policy roles");
    assert_eq!(
        roles,
        [operator_role],
        "operator roles are not catalog words"
    );
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one installed release proves schema, ledger, checksum, Follows and data preservation"
)]
async fn installed_historical_example_upgrades_in_place_with_its_real_ledger() {
    use proxiima::spiice::SpiiceBundle;
    use proxiima_storage_pg::{MigrationPhase, MissingMigrationPolicy, VectorStorage};
    use sha2::{Digest, Sha384};

    let db = proxiima_pg_testkit::SplitRoleDb::create("proxiima_installed_history", &[])
        .await
        .expect("owned test database");
    let pg = proxiima::PgMigrationStorage::connect_with_config(
        db.platform_url(),
        proxiima::PgPoolConfig::default(),
        proxiima::PgTuning::default(),
    )
    .await
    .expect("migration storage");
    let pool = pg.clone_pool_for_backend();
    let mut connection = pool.acquire().await.expect("migration connection");
    let mut transaction = proxiima_storage_pg::begin_migration_transaction(&mut connection)
        .await
        .expect("historical transaction");
    proxiima_storage_pg::run_core_phase(
        &mut transaction,
        MissingMigrationPolicy::Reject,
        VectorStorage::Local,
        MigrationPhase::Historical,
    )
    .await
    .expect("original core migrations");
    let sidecars = rename_example_engine::frozen_sidecars();
    sidecars
        .sync_historical_owner_follows(&mut transaction)
        .await
        .expect("original Follows registration");
    let source = proxiima_example::migrator();
    let mut historical =
        proxiima_storage_pg::migrations_for_phase(&source, MigrationPhase::Historical);
    let old_ledger =
        proxiima_storage_pg::historical_catalog_name("public._sqlx_migrations_proxiima_example");
    historical.dangerous_set_table_name(old_ledger.clone());
    historical
        .run_direct(None, transaction.as_mut(), false)
        .await
        .expect("original example migrations");
    let old_core = proxiima_storage_pg::historical_catalog_name("proxiima_core");
    let old_example = proxiima_storage_pg::historical_catalog_name("proxiima_example");
    let core_oid = rename_schema_oid(transaction.as_mut(), &old_core).await;
    let example_oid = rename_schema_oid(transaction.as_mut(), &old_example).await;
    let ledger_oid: i64 = sqlx::query_scalar("SELECT to_regclass($1)::oid::bigint")
        .bind(&old_ledger)
        .fetch_one(transaction.as_mut())
        .await
        .expect("original ledger identity");
    let core_rows = rename_ledger_snapshot(transaction.as_mut(), "public._sqlx_migrations").await;
    let example_rows = rename_ledger_snapshot(transaction.as_mut(), &old_ledger).await;
    let core_source = proxiima_storage_pg::core_migrator();
    let finalizer = core_source
        .iter()
        .find(|migration| {
            proxiima_storage_pg::migration_phase(&core_source, migration)
                == MigrationPhase::Finalize
        })
        .expect("embedded finalizer");
    let mut digest = Sha384::new();
    for (index, kind) in ["functions", "triggers", "policies", "vector"]
        .iter()
        .enumerate()
    {
        if index > 0 {
            digest.update([0]);
        }
        let marker = format!("$rename_history_{kind}$");
        let snapshot = finalizer
            .sql
            .as_str()
            .split(&marker)
            .nth(1)
            .expect("original definitions snapshot");
        digest.update(snapshot.as_bytes());
    }
    let checksum: String = digest
        .finalize()
        .iter()
        .flat_map(|byte| {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            [
                char::from(HEX[usize::from(byte >> 4)]),
                char::from(HEX[usize::from(byte & 15)]),
            ]
        })
        .collect();
    // The record names a schema and a checksum known only at run time; the
    // server formats them into the function.
    sqlx::query(
        "SELECT set_config('proxiima_test.record_schema', $1, true),
                set_config('proxiima_test.record_checksum', $2, true)",
    )
    .bind(&old_core)
    .bind(&checksum)
    .execute(transaction.as_mut())
    .await
    .expect("original release record parameters");
    sqlx::raw_sql(
        "DO $record$ BEGIN
             EXECUTE format(
                 'CREATE OR REPLACE FUNCTION %I.definitions_applied() RETURNS TABLE(checksum text, release text)
                  LANGUAGE sql IMMUTABLE AS $f$ SELECT %L::text, ''0.0.66''::text $f$',
                 current_setting('proxiima_test.record_schema'),
                 current_setting('proxiima_test.record_checksum'));
         END $record$",
    )
    .execute(transaction.as_mut())
    .await
    .expect("original release record");
    let mut insert_setting = SqlText::new("INSERT INTO ");
    insert_setting
        .push_ident(&PgIdent::name(&old_example).expect("a plain catalog name"))
        .push(".settings(key,value) VALUES ($1,$2)");
    sqlx::query(insert_setting)
        .bind("rename-probe")
        .bind("preserved")
        .execute(transaction.as_mut())
        .await
        .expect("existing application data");
    transaction
        .commit()
        .await
        .expect("installed historical release");
    drop(connection);
    let platform: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&pool)
        .await
        .expect("platform role");
    stage_released_role_shapes(db.name(), &old_core, &old_example, &platform).await;

    proxiima::preflight_without_migrations(
        &pg,
        proxiima_example::ExampleSpiice::migrators(),
        &sidecars,
    )
    .await
    .expect_err("skip boot refuses pending catalog finalizers");
    proxiima::run_core_and_spiice_migrations(
        &pg,
        proxiima_example::ExampleSpiice::migrators(),
        &sidecars,
    )
    .await
    .expect("upgrade real historical ledgers");
    proxiima::preflight_without_migrations(
        &pg,
        proxiima_example::ExampleSpiice::migrators(),
        &sidecars,
    )
    .await
    .expect("upgraded composition serves skip boot");
    let mut connection = pool.acquire().await.expect("migration connection");
    let mut transaction = proxiima_storage_pg::begin_migration_transaction(&mut connection)
        .await
        .expect("inspection transaction");
    assert_eq!(
        rename_schema_oid(transaction.as_mut(), "proxiima_core").await,
        core_oid
    );
    assert_eq!(
        rename_schema_oid(transaction.as_mut(), "proxiima_example").await,
        example_oid
    );
    assert!(
        rename_schema_oid(transaction.as_mut(), &old_core)
            .await
            .is_none()
    );
    assert!(
        rename_schema_oid(transaction.as_mut(), &old_example)
            .await
            .is_none()
    );
    let current_ledger = "public._sqlx_migrations_proxiima_example";
    let current_oid: i64 = sqlx::query_scalar("SELECT to_regclass($1)::oid::bigint")
        .bind(current_ledger)
        .fetch_one(transaction.as_mut())
        .await
        .expect("current ledger identity");
    assert_eq!(current_oid, ledger_oid);
    let old_exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(old_ledger)
        .fetch_one(transaction.as_mut())
        .await
        .expect("old ledger absence");
    assert!(!old_exists);
    for (ledger, source, before) in [
        ("public._sqlx_migrations", &core_source, core_rows),
        (current_ledger, &source, example_rows),
    ] {
        let after = rename_ledger_snapshot(transaction.as_mut(), ledger).await;
        // The upgrade appends the one catalog finalizer and every migration
        // released after it, and nothing else.
        let appended: Vec<MigrationPhase> = after
            .iter()
            .filter(|row| !before.contains(row))
            .map(|(version, _)| {
                let migration = source
                    .iter()
                    .find(|migration| migration.version == *version)
                    .expect("an appended version is an embedded migration");
                proxiima_storage_pg::migration_phase(source, migration)
            })
            .collect();
        assert_eq!(
            appended.first(),
            Some(&MigrationPhase::Finalize),
            "exactly one catalog finalizer is appended first"
        );
        assert!(
            appended[1..]
                .iter()
                .all(|phase| *phase == MigrationPhase::Current),
            "only current migrations follow the finalizer: {appended:?}"
        );
        for row in before {
            assert!(
                after.contains(&row),
                "historical checksum and version remain unchanged"
            );
        }
    }
    let value: String =
        sqlx::query_scalar("SELECT value FROM proxiima_example.settings WHERE key='rename-probe'")
            .fetch_one(transaction.as_mut())
            .await
            .expect("preserved application row");
    assert_eq!(value, "preserved");
    let marks: Vec<String> = sqlx::query_scalar(
        "SELECT table_name FROM proxiima_core.owner_follows ORDER BY table_name",
    )
    .fetch_all(transaction.as_mut())
    .await
    .expect("current Follows marks");
    assert_eq!(marks, sidecars.owner_follows_memory_sidecar_tables());
    assert_released_role_shapes_upgraded(transaction.as_mut()).await;
    transaction.rollback().await.expect("inspection finished");
}
