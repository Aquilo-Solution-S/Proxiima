//! Runtime coverage for the migration reset boundary.

use std::collections::{BTreeMap, BTreeSet};

use proxiima_core::{SqlText, StorageError};
use proxiima_pg_testkit::{
    DbGuard, admin_url_or_skip, create_db, split_role_urls, split_role_urls_without_pgvector,
};
use proxiima_storage_pg::{
    PgMigrationStorage, PgPoolConfig, PgTuning, VectorStorage, core_migrator_for, definitions,
    ensure_core_schema_current, test_fixtures::stage_previous_head,
};

const BASELINE: &str = include_str!("../migrations/0069_baseline.sql");

/// Both store kinds, each with the database its roles are provisioned for.
const KINDS: [VectorStorage; 2] = [VectorStorage::Local, VectorStorage::HostIndex];

/// The definitions text a store of this kind holds, the rendered functions
/// included.
fn definition_files(vectors: VectorStorage) -> Vec<String> {
    let mut files = vec![
        definitions::relation_roles_sql().as_str().to_owned(),
        definitions::FUNCTIONS.to_owned(),
        definitions::POLICIES.to_owned(),
    ];
    if vectors == VectorStorage::Local {
        files.push(definitions::VECTOR.to_owned());
    }
    files
}

fn definitions_record_sql(checksum: &'static str, release: &'static str) -> SqlText {
    let mut sql = SqlText::new(
        "CREATE OR REPLACE FUNCTION proxiima_core.definitions_applied()
         RETURNS TABLE(checksum text, release text) LANGUAGE sql IMMUTABLE
         AS $$ SELECT '",
    );
    sql.push(checksum).push("', '").push(release).push("' $$");
    sql
}

fn declared_names(source: &str, keyword: &str) -> BTreeMap<String, usize> {
    let mut names = BTreeMap::new();
    for mut line in source.lines().map(str::trim) {
        // Generated installers execute dollar-quoted DDL under its owner.
        if let Some(execution) = line.strip_prefix("EXECUTE $")
            && let Some((_, statement)) = execution.split_once('$')
        {
            line = statement;
        }
        let Some(index) = line.find(keyword) else {
            continue;
        };
        if !line.starts_with("CREATE") {
            continue;
        }
        let rest = line[index + keyword.len()..].trim_start();
        let name = rest
            .split(['(', ' ', '\t'])
            .next()
            .unwrap_or_default()
            .trim_start_matches("proxiima_core.");
        if !name.is_empty() {
            *names.entry(name.to_owned()).or_insert(0) += 1;
        }
    }
    names
}

fn declared_goal_reader_tables(source: &str) -> BTreeSet<String> {
    let mut grant = String::new();
    let mut tables = BTreeSet::new();
    for line in source.lines().map(str::trim) {
        if grant.is_empty() && line.starts_with("GRANT SELECT ON ") {
            grant.push_str(line);
        } else if !grant.is_empty() {
            grant.push(' ');
            grant.push_str(line);
        }
        if !grant.ends_with("TO proxiima_goal_reader;") {
            continue;
        }
        let Some(list) = grant.strip_prefix("GRANT SELECT ON ") else {
            grant.clear();
            continue;
        };
        let list = list.trim_end_matches(" TO proxiima_goal_reader;");
        tables.extend(
            list.split(',')
                .map(|table| table.trim().trim_start_matches("proxiima_core.").to_owned()),
        );
        grant.clear();
    }
    tables
}

async fn catalog_snapshot(pool: &sqlx::PgPool) -> Vec<String> {
    let functions: Vec<String> = sqlx::query_scalar(
        "SELECT format('%s|%s|%s|%s', p.proname,
                       pg_get_function_identity_arguments(p.oid), p.prokind,
                       COALESCE(p.proacl::text, '') ||
                       CASE WHEN p.prokind <> 'a' THEN pg_get_functiondef(p.oid) ELSE '' END)
           FROM pg_proc p
           JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'proxiima_core'
          ORDER BY p.proname, pg_get_function_identity_arguments(p.oid), p.prokind",
    )
    .fetch_all(pool)
    .await
    .expect("functions");
    let aggregates: Vec<String> = sqlx::query_scalar(
        "SELECT format('%s|%s|%s|%s', p.proname,
                       pg_get_function_identity_arguments(p.oid),
                       COALESCE(a.aggtransfn::regprocedure::text, ''),
                       COALESCE(p.proacl::text, ''))
           FROM pg_proc p
           JOIN pg_namespace n ON n.oid = p.pronamespace
           JOIN pg_aggregate a ON a.aggfnoid = p.oid
          WHERE n.nspname = 'proxiima_core'
          ORDER BY p.proname, pg_get_function_identity_arguments(p.oid)",
    )
    .fetch_all(pool)
    .await
    .expect("aggregates");
    let triggers: Vec<String> = sqlx::query_scalar(
        "SELECT format('%s|%s|%s', c.relname, t.tgname, pg_get_triggerdef(t.oid))
           FROM pg_trigger t
           JOIN pg_class c ON c.oid = t.tgrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'proxiima_core' AND NOT t.tgisinternal
          ORDER BY c.relname, t.tgname",
    )
    .fetch_all(pool)
    .await
    .expect("triggers");
    let policies: Vec<String> = sqlx::query_scalar(
        "SELECT format('%s|%s|%s|%s', c.relname, p.polname,
                       pg_get_expr(p.polqual, p.polrelid),
                       pg_get_expr(p.polwithcheck, p.polrelid))
           FROM pg_policy p
           JOIN pg_class c ON c.oid = p.polrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'proxiima_core'
          ORDER BY c.relname, p.polname",
    )
    .fetch_all(pool)
    .await
    .expect("policies");
    [functions, aggregates, triggers, policies].concat()
}

async fn migration_db(
    prefix: &str,
    vectors: VectorStorage,
) -> Option<(String, PgMigrationStorage, DbGuard)> {
    admin_url_or_skip()?;
    let name = format!(
        "proxiima_mig_reset_{prefix}_{}",
        uuid::Uuid::new_v4().simple()
    );
    create_db(&name).await.expect("create test database");
    let (_, platform_url) = match vectors {
        VectorStorage::Local => split_role_urls(&name).await,
        VectorStorage::HostIndex => split_role_urls_without_pgvector(&name).await,
    }
    .expect("split roles");
    let migration = PgMigrationStorage::connect_with_config(
        &platform_url,
        PgPoolConfig::default(),
        PgTuning::default(),
    )
    .await
    .expect("migration connection")
    .with_vector_storage(vectors);
    Some((name.clone(), migration, DbGuard::adopt(name)))
}

/// Each kind installs exactly its own migrations: the host-index kind into a
/// database with no `vector` extension, the local kind with pgvector and the
/// vector column.
#[tokio::test]
async fn fresh_database_runs_its_kinds_migrations_and_definitions() {
    for vectors in KINDS {
        let Some((_name, migration, _guard)) = migration_db("fresh", vectors).await else {
            return;
        };
        migration.run_migrations().await.expect("fresh migration");
        let pool = migration.clone_pool_for_backend();
        let recorded: Vec<i64> = sqlx::query_scalar(
            "SELECT version FROM public._sqlx_migrations WHERE success ORDER BY version",
        )
        .fetch_all(&pool)
        .await
        .expect("ledger");
        assert_eq!(
            recorded,
            core_migrator_for(vectors)
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            "{vectors:?}"
        );
        assert!(
            definitions::current_matches(
                &mut pool.acquire().await.expect("connection").detach(),
                vectors
            )
            .await
            .expect("definitions")
        );
        let (extension, vector_column, vector_functions): (bool, bool, bool) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector'),
                    EXISTS (SELECT 1 FROM information_schema.columns
                             WHERE table_schema = 'proxiima_core'
                               AND table_name = 'embeddings' AND column_name = 'vec'),
                    EXISTS (SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                             WHERE n.nspname = 'proxiima_core'
                               AND p.proname = 'vector_candidates_d384')",
        )
        .fetch_one(&pool)
        .await
        .expect("vector parts");
        let expected = vectors == VectorStorage::Local;
        assert_eq!(
            (extension, vector_column, vector_functions),
            (expected, expected, expected),
            "{vectors:?}"
        );
    }
}

#[tokio::test]
async fn definitions_pass_is_idempotent() {
    for vectors in KINDS {
        let Some((_name, migration, _guard)) = migration_db("idempotent", vectors).await else {
            return;
        };
        migration.run_migrations().await.expect("first migration");
        let pool = migration.clone_pool_for_backend();
        let before = catalog_snapshot(&pool).await;
        sqlx::raw_sql(definitions_record_sql(
            "deadbeef",
            proxiima_core::RELEASE_VERSION,
        ))
        .execute(&pool)
        .await
        .expect("force definitions pass");
        migration.run_migrations().await.expect("second migration");
        let after = catalog_snapshot(&pool).await;
        assert_eq!(before, after, "{vectors:?}");
    }
}

#[tokio::test]
async fn definitions_preserve_spiice_trigger() {
    let Some((_name, migration, _guard)) = migration_db("trigger", VectorStorage::Local).await
    else {
        return;
    };
    migration.run_migrations().await.expect("first migration");
    let pool = migration.clone_pool_for_backend();
    sqlx::raw_sql(
        "CREATE FUNCTION proxiima_core.test_spiice_trigger()
         RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
         CREATE TRIGGER test_spiice_trigger
         BEFORE INSERT ON proxiima_core.installation
         FOR EACH ROW EXECUTE FUNCTION proxiima_core.test_spiice_trigger()",
    )
    .execute(&pool)
    .await
    .expect("spiice trigger");
    sqlx::raw_sql(definitions_record_sql(
        "deadbeef",
        proxiima_core::RELEASE_VERSION,
    ))
    .execute(&pool)
    .await
    .expect("force definitions pass");
    migration.run_migrations().await.expect("definitions rerun");
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname='test_spiice_trigger')",
    )
    .fetch_one(&pool)
    .await
    .expect("trigger catalog");
    assert!(exists);
}

#[tokio::test]
async fn core_catalog_has_no_undeclared_objects() {
    for vectors in KINDS {
        census(vectors).await;
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one core-only catalog compares the declared functions, triggers and policies"
)]
async fn census(vectors: VectorStorage) {
    let Some((_name, migration, _guard)) = migration_db("census", vectors).await else {
        return;
    };
    migration.run_migrations().await.expect("migration");
    let pool = migration.clone_pool_for_backend();
    let files = definition_files(vectors);
    let mut declared = declared_names(BASELINE, "FUNCTION");
    for (name, count) in declared_names(BASELINE, "AGGREGATE") {
        *declared.entry(name).or_default() += count;
    }
    for file in &files {
        for keyword in ["FUNCTION", "AGGREGATE"] {
            for (name, count) in declared_names(file, keyword) {
                *declared.entry(name).or_default() += count;
            }
        }
    }
    declared.insert("definitions_applied".into(), 1);
    let live: BTreeMap<String, usize> = sqlx::query_scalar(
        "SELECT p.proname FROM pg_proc p
           JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'proxiima_core'",
    )
    .fetch_all(&pool)
    .await
    .expect("functions")
    .into_iter()
    .fold(BTreeMap::new(), |mut map, name| {
        *map.entry(name).or_default() += 1;
        map
    });
    assert_eq!(live, declared, "{vectors:?}");
    let mut declared_triggers = BTreeSet::new();
    let mut trigger_name = None;
    for line in definitions::TRIGGERS.lines() {
        let tokens: Vec<_> = line.split_whitespace().collect();
        if line.trim_start().starts_with("CREATE")
            && let Some(trigger) = tokens.iter().position(|token| *token == "TRIGGER")
        {
            let name = tokens.get(trigger + 1).copied().unwrap_or_default();
            trigger_name = Some(if name == "IF" {
                tokens.get(trigger + 4).copied().unwrap_or_default()
            } else {
                name
            });
        }
        if let Some(on) = line.find(" ON proxiima_core.")
            && let Some(name) = trigger_name.take()
        {
            let table = line[on + " ON proxiima_core.".len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_end_matches(';')
                .to_owned();
            declared_triggers.insert((table, name.to_owned()));
        }
    }
    let live_triggers: BTreeSet<(String, String)> = sqlx::query_as(
        "SELECT c.relname, t.tgname FROM pg_trigger t
           JOIN pg_class c ON c.oid = t.tgrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'proxiima_core' AND NOT t.tgisinternal",
    )
    .fetch_all(&pool)
    .await
    .expect("triggers")
    .into_iter()
    .collect();
    assert_eq!(live_triggers, declared_triggers);
    let tables: BTreeSet<String> = definitions::POLICIES
        .lines()
        .filter_map(|line| {
            line.split("'proxiima_core', '")
                .nth(1)?
                .split('\'')
                .next()
                .map(str::to_owned)
        })
        .collect();
    let mut declared_policies: BTreeSet<(String, String)> = tables
        .into_iter()
        .flat_map(|table| {
            [
                "proxiima_owner_read",
                "proxiima_owner_write",
                "proxiima_platform",
            ]
            .map(|policy| (table.clone(), policy.to_owned()))
        })
        .collect();
    declared_policies.extend(
        files
            .iter()
            .flat_map(|file| file.lines())
            .filter_map(|line| {
                let declaration = line.trim().strip_prefix("CREATE POLICY ")?;
                let (policy, rest) = declaration.split_once(" ON proxiima_core.")?;
                let table = rest.split_whitespace().next()?;
                Some((table.to_owned(), policy.to_owned()))
            }),
    );
    declared_policies.extend(
        declared_goal_reader_tables(definitions::FUNCTIONS)
            .into_iter()
            .map(|table| (table, "proxiima_goal_reader".to_owned())),
    );
    let live_policies: BTreeSet<(String, String)> = sqlx::query_as(
        "SELECT c.relname, p.polname FROM pg_policy p
           JOIN pg_class c ON c.oid = p.polrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'proxiima_core'",
    )
    .fetch_all(&pool)
    .await
    .expect("policies")
    .into_iter()
    .collect();
    assert_eq!(live_policies, declared_policies, "{vectors:?}");
}

#[tokio::test]
async fn newer_definitions_release_refuses_run_and_preflight() {
    let Some((_name, migration, _guard)) = migration_db("newer", VectorStorage::Local).await else {
        return;
    };
    migration.run_migrations().await.expect("first migration");
    let pool = migration.clone_pool_for_backend();
    sqlx::query(
        "CREATE OR REPLACE FUNCTION proxiima_core.definitions_applied()
         RETURNS TABLE(checksum text, release text) LANGUAGE sql IMMUTABLE
         AS $$ SELECT 'deadbeef', '99.0.0' $$",
    )
    .execute(&pool)
    .await
    .expect("newer record");
    let run_error = migration
        .run_migrations()
        .await
        .expect_err("newer release refuses run");
    assert!(
        matches!(&run_error, StorageError::Internal(message)
            if message.contains("newer release 99.0.0")),
        "got: {run_error}"
    );
    let preflight_error = ensure_core_schema_current(
        &pool,
        proxiima_storage_pg::MissingMigrationPolicy::Reject,
        VectorStorage::Local,
    )
    .await
    .expect_err("newer release refuses preflight");
    assert!(
        matches!(&preflight_error, StorageError::Internal(message)
            if message.contains("newer release 99.0.0")),
        "got: {preflight_error}"
    );
}

#[tokio::test]
async fn same_release_checksum_drift_reapplies_definitions() {
    let Some((_name, migration, _guard)) = migration_db("checksum", VectorStorage::Local).await
    else {
        return;
    };
    migration.run_migrations().await.expect("first migration");
    let pool = migration.clone_pool_for_backend();
    sqlx::raw_sql(definitions_record_sql(
        "deadbeef",
        proxiima_core::RELEASE_VERSION,
    ))
    .execute(&pool)
    .await
    .expect("drift record");
    migration
        .run_migrations()
        .await
        .expect("reapply definitions");
    let matches = definitions::current_matches(
        &mut pool.acquire().await.expect("connection").detach(),
        VectorStorage::Local,
    )
    .await
    .expect("definitions");
    assert!(matches);
}

/// A store installed before the baseline reset is refused, whichever kind
/// opens it and whatever versions it recorded: nothing is stamped, adopted or
/// created, and the ledger is as it was.
#[tokio::test]
async fn a_pre_reset_ledger_is_refused_and_left_as_it_was() {
    for vectors in KINDS {
        for recorded in [33..=56_i64, 1..=29, 56..=56] {
            let Some((_name, migration, _guard)) = migration_db("refused", vectors).await else {
                return;
            };
            let pool = migration.clone_pool_for_backend();
            for version in recorded.clone() {
                sqlx::query(
                    "INSERT INTO public._sqlx_migrations
                         (version, description, success, checksum, execution_time)
                     VALUES ($1, 'old', true, $2, 0)",
                )
                .bind(version)
                .bind(vec![0_u8; 48])
                .execute(&pool)
                .await
                .expect("ledger row");
            }
            let refusals = [
                migration.run_migrations().await,
                ensure_core_schema_current(
                    &pool,
                    proxiima_storage_pg::MissingMigrationPolicy::Allow,
                    vectors,
                )
                .await,
            ];
            for refusal in refusals {
                assert!(
                    matches!(refusal, Err(StorageError::SchemaResetRequired { .. })),
                    "{vectors:?} {recorded:?}: {refusal:?}"
                );
            }
            let (rows, schema): (i64, bool) = sqlx::query_as(
                "SELECT (SELECT count(*) FROM public._sqlx_migrations),
                        EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'proxiima_core')",
            )
            .fetch_one(&pool)
            .await
            .expect("state");
            assert_eq!(
                (rows, schema),
                (i64::try_from(recorded.count()).expect("count"), false),
                "{vectors:?}: the refusal changed nothing"
            );
        }
    }
}

/// A database that holds a schema of an earlier install and no core ledger
/// row is not blank: the run refuses it and installs nothing beside it.
#[tokio::test]
async fn an_earlier_schema_without_a_ledger_is_refused() {
    for vectors in KINDS {
        let Some((_name, migration, _guard)) = migration_db("earlier", vectors).await else {
            return;
        };
        let pool = migration.clone_pool_for_backend();
        sqlx::query("CREATE SCHEMA proxima_core")
            .execute(&pool)
            .await
            .expect("earlier schema");
        let refusal = migration.run_migrations().await;
        assert!(
            matches!(refusal, Err(StorageError::SchemaResetRequired { .. })),
            "{vectors:?}: {refusal:?}"
        );
        let installed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'proxiima_core')",
        )
        .fetch_one(&pool)
        .await
        .expect("state");
        assert!(!installed, "{vectors:?}: the refusal installed nothing");
    }
}

/// The core ledger rows, `(version, checksum)`, and whether the column the
/// baseline dropped exists.
async fn core_state(pool: &sqlx::PgPool) -> (Vec<(i64, Vec<u8>)>, bool) {
    let ledger = sqlx::query_as(
        "SELECT version, checksum FROM public._sqlx_migrations
          WHERE version <= 9999 ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .expect("ledger");
    let column = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                         WHERE table_schema = 'proxiima_core'
                           AND table_name = 'delegated_authority_grants'
                           AND column_name = 'auth_epoch')",
    )
    .fetch_one(pool)
    .await
    .expect("column");
    (ledger, column)
}

async fn grant_ids(pool: &sqlx::PgPool) -> Vec<uuid::Uuid> {
    let mut transaction = pool.begin().await.expect("transaction");
    sqlx::query("SELECT set_config('app.proxiima_scope', 'platform', true)")
        .execute(&mut *transaction)
        .await
        .expect("platform scope");
    sqlx::query_scalar(
        "SELECT delegation_id FROM proxiima_core.delegated_authority_grants ORDER BY 1",
    )
    .fetch_all(&mut *transaction)
    .await
    .expect("grants")
}

/// The rows `sql` selects, each as one text value, read as the platform.
async fn rows_as_text(pool: &sqlx::PgPool, sql: &'static str) -> Vec<String> {
    let mut transaction = pool.begin().await.expect("transaction");
    sqlx::query("SELECT set_config('app.proxiima_scope', 'platform', true)")
        .execute(&mut *transaction)
        .await
        .expect("platform scope");
    sqlx::query_scalar(sql)
        .fetch_all(&mut *transaction)
        .await
        .expect("rows")
}

/// Every core ledger row, whole.
const LEDGER_ROWS: &str = "SELECT l::text FROM public._sqlx_migrations l
     WHERE version <= 9999 ORDER BY version";
/// Every delegated grant, whole.
const GRANT_ROWS: &str = "SELECT to_jsonb(g)::text
      FROM proxiima_core.delegated_authority_grants g ORDER BY delegation_id";
/// Every delegated grant without the column the baseline drops.
const GRANT_ROWS_OF_THE_BASELINE: &str = "SELECT (to_jsonb(g) - 'auth_epoch')::text
      FROM proxiima_core.delegated_authority_grants g ORDER BY delegation_id";

/// Stage a previous-head store on a fresh install and give it one delegated
/// grant, whose id is returned.
async fn previous_head_with_a_grant(pool: &sqlx::PgPool, vectors: VectorStorage) -> uuid::Uuid {
    stage_previous_head(pool, vectors)
        .await
        .expect("previous head");
    let grant = uuid::Uuid::now_v7();
    let mut transaction = pool.begin().await.expect("transaction");
    sqlx::query("SELECT set_config('app.proxiima_scope', 'platform', true)")
        .execute(&mut *transaction)
        .await
        .expect("platform scope");
    proxiima_storage_pg::test_fixtures::insert_delegated_grant(
        &mut *transaction,
        grant,
        uuid::Uuid::now_v7(),
        uuid::Uuid::now_v7(),
        uuid::Uuid::now_v7(),
        None,
    )
    .await
    .expect("grant");
    transaction.commit().await.expect("commit");
    grant
}

/// A store at the previous head is adopted in place by a migration run, its
/// rows kept; the same store one migration short is refused and left as it
/// was.
#[tokio::test]
async fn a_previous_head_store_is_adopted_and_a_shorter_one_refused() {
    for vectors in KINDS {
        let Some((_name, migration, _guard)) = migration_db("adopt", vectors).await else {
            return;
        };
        migration.run_migrations().await.expect("fresh migration");
        let pool = migration.clone_pool_for_backend();
        let grant = previous_head_with_a_grant(&pool, vectors).await;
        let short = migration_db("short", vectors).await.expect("database");
        short.1.run_migrations().await.expect("fresh migration");
        let short_pool = short.1.clone_pool_for_backend();

        assert!(
            matches!(
                ensure_core_schema_current(
                    &pool,
                    proxiima_storage_pg::MissingMigrationPolicy::Reject,
                    vectors
                )
                .await,
                Err(StorageError::MigrationRequired { .. })
            ),
            "{vectors:?}: an adoptable store is not current before a run"
        );
        let grants = rows_as_text(&pool, GRANT_ROWS_OF_THE_BASELINE).await;
        migration.run_migrations().await.expect("adoption");
        assert_eq!(
            rows_as_text(&pool, GRANT_ROWS_OF_THE_BASELINE).await,
            grants,
            "{vectors:?}: the adoption keeps every other value of the row"
        );
        let (ledger, column) = core_state(&pool).await;
        assert_eq!(
            ledger,
            core_migrator_for(vectors)
                .iter()
                .map(|migration| (migration.version, migration.checksum.to_vec()))
                .collect::<Vec<_>>(),
            "{vectors:?}: the ledger is the baseline's"
        );
        assert!(!column, "{vectors:?}: the adoption drops the column");
        assert_eq!(grant_ids(&pool).await, vec![grant], "{vectors:?}");
        assert!(
            definitions::current_matches(
                &mut pool.acquire().await.expect("connection").detach(),
                vectors
            )
            .await
            .expect("definitions"),
            "{vectors:?}"
        );

        let short_grant = previous_head_with_a_grant(&short_pool, vectors).await;
        sqlx::query(
            "DELETE FROM public._sqlx_migrations
              WHERE version = (SELECT max(version) FROM public._sqlx_migrations)",
        )
        .execute(&short_pool)
        .await
        .expect("drop the newest row");
        let before = (
            core_state(&short_pool).await,
            rows_as_text(&short_pool, LEDGER_ROWS).await,
            rows_as_text(&short_pool, GRANT_ROWS).await,
        );
        let refusal = short.1.run_migrations().await;
        assert!(
            matches!(&refusal, Err(StorageError::SchemaResetRequired { details })
                if details.contains("v0.0.73")),
            "{vectors:?}: {refusal:?}"
        );
        let after = (
            core_state(&short_pool).await,
            rows_as_text(&short_pool, LEDGER_ROWS).await,
            rows_as_text(&short_pool, GRANT_ROWS).await,
        );
        assert_eq!(before, after, "{vectors:?}: the refusal changed nothing");
        assert!(after.0.1, "{vectors:?}: the column stays");
        assert_eq!(
            grant_ids(&short_pool).await,
            vec![short_grant],
            "{vectors:?}: the row stays"
        );
    }
}
