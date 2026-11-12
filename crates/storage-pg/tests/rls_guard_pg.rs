use async_trait::async_trait;
use proxiima_core::{
    AuthPath, Authenticator, AuthzContext, Credentials, GroupId, OwnerId, OwnerRoles, Role, UserId,
};
use proxiima_pg_testkit::{create_db, db_url, drop_db, provision_goal_reader, unique_db_name};
use proxiima_storage_pg::{
    PgMigrationStorage, PgPlatformScope, PgPoolConfig, PgTuning, assert_runtime_rls,
    begin_owner_transaction,
};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::str::FromStr;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};

/// Composed DDL from program literals and quoted identifiers: `"text"` is
/// appended as written, `id(name)` as the quoted identifier `name`,
/// `lit(CONST)` as the `&'static str` `CONST`,
/// `pw(&value)` as the quote-doubled literal `value` (a role password, which
/// cannot be bound), `text(&sql)` as another composed `SqlText`.
macro_rules! ddl {
    (@go $sql:ident;) => {};
    (@go $sql:ident; $text:literal $(, $($rest:tt)*)?) => {
        $sql.push($text);
        ddl!(@go $sql; $($($rest)*)?);
    };
    (@go $sql:ident; lit($text:expr) $(, $($rest:tt)*)?) => {
        $sql.push($text);
        ddl!(@go $sql; $($($rest)*)?);
    };
    (@go $sql:ident; pw($value:expr) $(, $($rest:tt)*)?) => {
        $sql.push_literal($value).expect("fixture password");
        ddl!(@go $sql; $($($rest)*)?);
    };
    (@go $sql:ident; text($other:expr) $(, $($rest:tt)*)?) => {
        $sql.push_text($other);
        ddl!(@go $sql; $($($rest)*)?);
    };
    (@go $sql:ident; id($name:expr) $(, $($rest:tt)*)?) => {
        $sql.push_ident(&proxiima_core::PgIdent::quoted($name).expect("fixture identifier"));
        ddl!(@go $sql; $($($rest)*)?);
    };
    ($first:literal $(, $($rest:tt)*)?) => {{
        let mut sql = proxiima_core::SqlText::new($first);
        ddl!(@go sql; $($($rest)*)?);
        sql
    }};
}

static DDL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

struct SyntheticVerifier {
    roles: OwnerRoles,
}

#[path = "rls_guard_pg/memory_paging.rs"]
mod memory_paging;

#[path = "rls_guard_pg/paging_work.rs"]
mod paging_work;

#[path = "rls_guard_pg/access_probes.rs"]
mod access_probes;

#[path = "rls_guard_pg/trigger_scope.rs"]
mod trigger_scope;

#[path = "rls_guard_pg/goal_owner_matches_head.rs"]
mod goal_owner_matches_head;

#[path = "rls_guard_pg/privileged_objects.rs"]
mod privileged_objects;

#[path = "rls_guard_pg/boot_connections.rs"]
mod boot_connections;

#[path = "rls_guard_pg/omitted_schema.rs"]
mod omitted_schema;

#[path = "rls_guard_pg/owner_predicates.rs"]
mod owner_predicates;

#[path = "rls_guard_pg/search_candidates.rs"]
mod search_candidates;

#[path = "rls_guard_pg/substring_work.rs"]
mod substring_work;

#[path = "rls_guard_pg/goal_assignment_work.rs"]
mod goal_assignment_work;

#[async_trait]
impl Authenticator for SyntheticVerifier {
    async fn authenticate(
        &self,
        _credentials: &Credentials,
    ) -> Result<AuthzContext, proxiima_core::AuthError> {
        Ok(AuthzContext::server_resolved(
            self.roles.clone(),
            AuthPath::HostBearer,
        ))
    }
}

async fn execute(pool: &PgPool, statement: impl sqlx::SqlSafeStr) {
    let _lock = DDL_LOCK.get_or_init(|| Mutex::new(())).lock().await;
    sqlx::query(statement).execute(pool).await.unwrap();
}

fn assert_rls_refusal(error: &sqlx::Error) {
    let database = error.as_database_error().expect("database refusal");
    assert!(
        matches!(database.code().as_deref(), Some("42501" | "55000")),
        "{error}"
    );
}

async fn assert_unscoped_owner_access_denied(connection: &mut sqlx::PgConnection) {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM proxiima_core.owners")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        sqlx::query("UPDATE proxiima_core.owners SET owner_id = owner_id")
            .execute(&mut *connection)
            .await
            .unwrap()
            .rows_affected(),
        0
    );
    assert_eq!(
        sqlx::query("DELETE FROM proxiima_core.owners")
            .execute(&mut *connection)
            .await
            .unwrap()
            .rows_affected(),
        0
    );
    // Raw INSERT on purpose: an unscoped connection must be refused by RLS
    let error =
        sqlx::query("INSERT INTO proxiima_core.owners(owner_id, kind) VALUES ($1, 'group')")
            .bind(uuid::Uuid::now_v7())
            .execute(&mut *connection)
            .await
            .expect_err("missing scope refuses INSERT");
    assert_rls_refusal(&error);
}

async fn runtime_pool(database: &str, role: &str, password: &str) -> PgPool {
    let options = PgConnectOptions::from_str(&db_url(database))
        .unwrap()
        .username(role)
        .password(password);
    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .unwrap()
}

fn role_url(database: &str, role: &str, password: &str) -> String {
    let url = db_url(database);
    let (scheme, rest) = url.split_once("://").unwrap();
    let (_credentials, host) = rest.rsplit_once('@').unwrap();
    format!("{scheme}://{role}:{password}@{host}")
}

#[expect(
    clippy::too_many_lines,
    reason = "isolated disposable database fixture"
)]
async fn setup() -> (String, PgPool, PgPool, String, String, String, String) {
    let database = unique_db_name("proxiima_rls_guard");
    create_db(&database).await.unwrap();
    let admin = PgPool::connect(&db_url(&database)).await.unwrap();
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let owner = format!("rls_owner_{suffix}");
    let runtime = format!("rls_runtime_{suffix}");
    let password = format!("pw_{suffix}");
    execute(
        &admin,
        ddl!(
            "CREATE ROLE ",
            id(&owner),
            " LOGIN PASSWORD ",
            pw(&password),
            " NOSUPERUSER NOBYPASSRLS"
        ),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "CREATE ROLE ",
            id(&runtime),
            " LOGIN PASSWORD ",
            pw(&password),
            " NOSUPERUSER NOBYPASSRLS"
        ),
    )
    .await;
    let schema = format!("proxiima_rls_{suffix}");
    execute(
        &admin,
        ddl!("CREATE SCHEMA ", id(&schema), " AUTHORIZATION ", id(&owner)),
    )
    .await;
    execute(
        &admin,
        ddl!("GRANT USAGE ON SCHEMA ", id(&schema), " TO ", id(&runtime)),
    )
    .await;
    for table in ["memory", "sidecar"] {
        execute(
            &admin,
            ddl!(
                "CREATE TABLE ",
                id(&schema),
                ".",
                id(table),
                " (id uuid PRIMARY KEY, owner_id uuid NOT NULL)"
            ),
        )
        .await;
        execute(
            &admin,
            ddl!(
                "ALTER TABLE ",
                id(&schema),
                ".",
                id(table),
                " OWNER TO ",
                id(&owner)
            ),
        )
        .await;
        execute(
            &admin,
            ddl!(
                "GRANT SELECT, INSERT, UPDATE, DELETE ON ",
                id(&schema),
                ".",
                id(table),
                " TO ",
                id(&runtime)
            ),
        )
        .await;
        execute(
            &admin,
            ddl!(
                "ALTER TABLE ",
                id(&schema),
                ".",
                id(table),
                " ENABLE ROW LEVEL SECURITY"
            ),
        )
        .await;
        execute(
            &admin,
            ddl!(
                "ALTER TABLE ",
                id(&schema),
                ".",
                id(table),
                " FORCE ROW LEVEL SECURITY"
            ),
        )
        .await;
        execute(&admin, ddl!("CREATE POLICY proxiima_owner_read ON ", id(&schema), ".", id(table), " FOR SELECT TO PUBLIC USING (owner_id = ANY(COALESCE(NULLIF(current_setting('app.owner', true), '')::uuid[], '{}'::uuid[])))")).await;
        execute(&admin, ddl!("CREATE POLICY proxiima_owner_write ON ", id(&schema), ".", id(table), " FOR ALL TO PUBLIC USING (owner_id = ANY(COALESCE(NULLIF(current_setting('app.owner', true), '')::uuid[], '{}'::uuid[]))) WITH CHECK (owner_id = ANY(COALESCE(NULLIF(current_setting('app.owner', true), '')::uuid[], '{}'::uuid[])))")).await;
        execute(&admin, ddl!("CREATE POLICY proxiima_platform ON ", id(&schema), ".", id(table), " FOR ALL TO ", id(&owner), " USING (current_setting('app.proxiima_scope', true) = 'platform') WITH CHECK (current_setting('app.proxiima_scope', true) = 'platform')")).await;
    }
    let runtime_pool = runtime_pool(&database, &runtime, &password).await;
    (
        database,
        admin,
        runtime_pool,
        owner,
        runtime,
        schema,
        password,
    )
}

/// How long `vacuum_all_visible` keeps vacuuming before it names the holder.
const VISIBILITY_ATTEMPTS: u32 = 50;

/// A table a fixture vacuums to settle its visibility map.
#[derive(Clone, Copy, Debug)]
enum Vacuumed {
    MemoryHead,
    Goal,
    GoalHead,
}

impl Vacuumed {
    fn relation(self) -> &'static str {
        match self {
            Self::MemoryHead => "proxiima_core.memory_head",
            Self::Goal => "proxiima_core.goal",
            Self::GoalHead => "proxiima_core.goal_head",
        }
    }
}

/// `VACUUM (ANALYZE)` each of `tables` until every page of it is all-visible.
///
/// A plan that stops at a page's size is an index-only scan only while the
/// visibility map is set; without it the planner may sort the whole owner
/// instead. One vacuum cannot set the map while any other transaction of the
/// database is older than the fixture's commits (a concurrent autovacuum's
/// analyze is one), so the fixture waits for the map and, when a holder
/// never goes away, fails naming it rather than as a plan flake.
async fn vacuum_all_visible(pool: &PgPool, tables: &[Vacuumed]) {
    for table in tables {
        let relation = table.relation();
        let mut pages = (0, 0);
        for _ in 0..VISIBILITY_ATTEMPTS {
            let vacuum = match table {
                Vacuumed::MemoryHead => sqlx::query("VACUUM (ANALYZE) proxiima_core.memory_head"),
                Vacuumed::Goal => sqlx::query("VACUUM (ANALYZE) proxiima_core.goal"),
                Vacuumed::GoalHead => sqlx::query("VACUUM (ANALYZE) proxiima_core.goal_head"),
            };
            vacuum.execute(pool).await.unwrap();
            pages = sqlx::query_as(
                "SELECT relpages, relallvisible FROM pg_class WHERE oid = $1::regclass",
            )
            .bind(relation)
            .fetch_one(pool)
            .await
            .unwrap();
            if pages.1 >= pages.0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let (total, visible): (i32, i32) = pages;
        if visible < total {
            let holders: Vec<(String, Option<String>)> = sqlx::query_as(
                "SELECT state, left(query, 80) FROM pg_stat_activity
                  WHERE datname = current_database() AND (backend_xmin IS NOT NULL OR backend_xid IS NOT NULL)
                    AND pid <> pg_backend_pid()",
            )
            .fetch_all(pool)
            .await
            .unwrap();
            panic!(
                "{relation}: {visible} of {total} pages all-visible after {VISIBILITY_ATTEMPTS} \
                 vacuums; transactions holding the horizon: {holders:?}"
            );
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the complete split-role fixture keeps its setup together"
)]
async fn setup_full_schema() -> (String, PgPool, PgPool, PgPool, String, String, String) {
    let database = unique_db_name("proxiima_rls_full");
    create_db(&database).await.unwrap();
    let admin = PgPool::connect(&db_url(&database)).await.unwrap();
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let platform = format!("rls_platform_{suffix}");
    let runtime = format!("rls_runtime_full_{suffix}");
    let password = format!("pw_{suffix}");
    for role in [&platform, &runtime] {
        execute(
            &admin,
            ddl!(
                "CREATE ROLE ",
                id(role),
                " LOGIN PASSWORD ",
                pw(&password),
                " NOSUPERUSER NOBYPASSRLS"
            ),
        )
        .await;
    }
    {
        provision_goal_reader(&admin, &platform, &runtime)
            .await
            .expect("goal reader fixture role");
    }
    execute(
        &admin,
        ddl!(
            "GRANT proxiima_search, proxiima_goal_reader TO ",
            id(&platform),
            " WITH INHERIT FALSE, SET TRUE"
        ),
    )
    .await;
    // The extensions are cluster capabilities; the platform role owns the
    // schemas and runs the migration path that applies the baseline and
    // definitions under NOSUPERUSER NOBYPASSRLS.
    execute(&admin, "CREATE EXTENSION IF NOT EXISTS vector").await;
    execute(&admin, "CREATE EXTENSION IF NOT EXISTS btree_gin").await;
    execute(
        &admin,
        ddl!(
            "GRANT CREATE ON DATABASE ",
            id(&database),
            " TO ",
            id(&platform)
        ),
    )
    .await;
    execute(
        &admin,
        ddl!("GRANT USAGE, CREATE ON SCHEMA public TO ", id(&platform)),
    )
    .await;
    let platform_url = role_url(&database, &platform, &password);
    let platform_storage = PgMigrationStorage::connect_with_config(
        &platform_url,
        PgPoolConfig::default(),
        PgTuning::default(),
    )
    .await
    .unwrap();
    platform_storage.run_migrations().await.unwrap();
    let platform_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            PgConnectOptions::from_str(&role_url(&database, &platform, &password)).unwrap(),
        )
        .await
        .unwrap();
    sqlx::query("SELECT set_config('app.proxiima_scope', 'platform', false)")
        .execute(&platform_pool)
        .await
        .unwrap();
    let mut spiice_migrations = sqlx::migrate!("../../spiices/example/migrations");
    spiice_migrations.dangerous_set_table_name("public._sqlx_migrations_proxiima_example");
    let mut connection = platform_pool.acquire().await.unwrap();
    let mut transaction = proxiima_storage_pg::begin_migration_transaction(&mut connection)
        .await
        .unwrap();
    proxiima_storage_pg::run_core_migrations(
        &mut transaction,
        proxiima_storage_pg::MissingMigrationPolicy::Reject,
        proxiima_storage_pg::VectorStorage::Local,
    )
    .await
    .unwrap();
    spiice_migrations
        .run_direct(None, transaction.as_mut(), false)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    drop(connection);
    platform_pool.close().await;
    platform_storage.initialize_store_number(1).await.unwrap();
    execute(
        &admin,
        ddl!(
            "GRANT USAGE ON SCHEMA proxiima_core, proxiima_example TO ",
            id(&runtime),
            ", ",
            id(&platform)
        ),
    )
    .await;
    execute(
        &admin,
        ddl!("GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA proxiima_core, proxiima_example TO ", id(&runtime)),
    )
    .await;
    // The blanket grant covers core views too, which the runtime role may only read.
    execute(&admin, "SELECT proxiima_core.restrict_view_grants()").await;
    let scope_grant: bool =
        sqlx::query_scalar("SELECT has_parameter_privilege($1, 'app.proxiima_scope', 'SET')")
            .bind(&platform)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert!(
        !scope_grant,
        "owner RLS must apply without a parameter grant"
    );
    let platform_connection = runtime_pool(&database, &platform, &password).await;
    execute(
        &admin,
        ddl!("GRANT EXECUTE ON FUNCTION proxiima_core.goal_grants_held(), proxiima_core.goal_memory_read_routes(), proxiima_core.goal_visible_memory_ids(), proxiima_core.goal_visible_memory_heads(), proxiima_core.goal_memory_read_admitted(uuid), proxiima_core.goal_read_admitted(uuid) TO ", id(&runtime)),
    )
    .await;
    let runtime_connection = runtime_pool(&database, &runtime, &password).await;
    (
        database,
        admin,
        runtime_connection,
        platform_connection,
        platform,
        runtime,
        password,
    )
}

async fn cleanup(database: &str, admin: PgPool, owner: &str, runtime: &str) {
    admin.close().await;
    drop_db(database).await.unwrap();
    let control = PgPool::connect(&proxiima_pg_testkit::admin_url())
        .await
        .unwrap();
    execute(&control, ddl!("DROP ROLE IF EXISTS ", id(runtime))).await;
    execute(&control, ddl!("DROP ROLE IF EXISTS ", id(owner))).await;
    control.close().await;
}

async fn restore_owner_read_policy(admin: &PgPool, schema: &str, table: &str) {
    execute(
        admin,
        ddl!("CREATE POLICY proxiima_owner_read ON ", id(schema), ".", id(table), " FOR SELECT TO PUBLIC USING (owner_id = ANY(COALESCE(NULLIF(current_setting('app.owner', true), '')::uuid[], '{}'::uuid[])))"),
    )
    .await;
}

/// A pushed literal reads back as written under either string setting.
async fn assert_literal_reads_back_under_both_string_settings(admin: &PgPool) {
    let original = r"a\b'c";
    let mut select = proxiima_core::SqlText::new("SELECT ");
    select.push_literal(original).unwrap();
    let mut connection = admin.acquire().await.unwrap();
    for set in [
        "SET standard_conforming_strings = on",
        "SET standard_conforming_strings = off",
    ] {
        sqlx::query(set).execute(&mut *connection).await.unwrap();
        let read: String = sqlx::query_scalar(select.clone())
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        assert_eq!(read, original, "{set}");
    }
}

#[tokio::test]
async fn runtime_guard_accepts_complete_schema_and_refuses_structural_breaks() {
    let (database, admin, runtime, owner, runtime_role, schema, password) = setup().await;
    assert_runtime_rls(&runtime, &[&schema]).await.unwrap();
    assert_literal_reads_back_under_both_string_settings(&admin).await;

    let owner_pool = runtime_pool(&database, &owner, &password).await;
    assert!(assert_runtime_rls(&owner_pool, &[&schema]).await.is_err());
    owner_pool.close().await;
    execute(
        &admin,
        ddl!("GRANT ", id(&owner), " TO ", id(&runtime_role)),
    )
    .await;
    assert!(assert_runtime_rls(&runtime, &[&schema]).await.is_err());
    execute(
        &admin,
        ddl!("REVOKE ", id(&owner), " FROM ", id(&runtime_role)),
    )
    .await;
    execute(&admin, ddl!("ALTER ROLE ", id(&runtime_role), " BYPASSRLS")).await;
    assert!(assert_runtime_rls(&runtime, &[&schema]).await.is_err());
    execute(
        &admin,
        ddl!("ALTER ROLE ", id(&runtime_role), " NOBYPASSRLS"),
    )
    .await;
    execute(&admin, ddl!("ALTER ROLE ", id(&runtime_role), " SUPERUSER")).await;
    assert!(assert_runtime_rls(&runtime, &[&schema]).await.is_err());
    execute(
        &admin,
        ddl!("ALTER ROLE ", id(&runtime_role), " NOSUPERUSER"),
    )
    .await;

    let count: i64 = sqlx::query_scalar(ddl!("SELECT count(*) FROM ", id(&schema), ".memory"))
        .fetch_one(&runtime)
        .await
        .unwrap();
    assert_eq!(count, 0);
    // Raw INSERT on purpose: a table with no scope column must refuse the write
    let missing_scope = sqlx::query(ddl!("INSERT INTO ", id(&schema), ".memory (id, owner_id) VALUES ('00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-000000000002')"))
        .execute(&runtime).await;
    assert_rls_refusal(&missing_scope.expect_err("missing app.owner must refuse writes"));

    execute(
        &admin,
        ddl!(
            "DROP POLICY proxiima_owner_read ON ",
            id(&schema),
            ".memory"
        ),
    )
    .await;
    assert!(assert_runtime_rls(&runtime, &[&schema]).await.is_err());
    restore_owner_read_policy(&admin, &schema, "memory").await;

    execute(
        &admin,
        ddl!(
            "ALTER TABLE ",
            id(&schema),
            ".sidecar NO FORCE ROW LEVEL SECURITY"
        ),
    )
    .await;
    assert!(assert_runtime_rls(&runtime, &[&schema]).await.is_err());
    execute(
        &admin,
        ddl!(
            "ALTER TABLE ",
            id(&schema),
            ".sidecar FORCE ROW LEVEL SECURITY"
        ),
    )
    .await;
    execute(
        &admin,
        ddl!("CREATE TABLE ", id(&schema), ".unprotected (id int)"),
    )
    .await;
    assert!(assert_runtime_rls(&runtime, &[&schema]).await.is_err());

    let mut transaction = runtime.begin().await.unwrap();
    sqlx::query("SET LOCAL row_security = off")
        .execute(&mut *transaction)
        .await
        .unwrap();
    let bypass = sqlx::query(ddl!("SELECT count(*) FROM ", id(&schema), ".memory"))
        .execute(&mut *transaction)
        .await;
    assert_rls_refusal(&bypass.expect_err("forced RLS refuses runtime row_security bypass"));
    transaction.rollback().await.unwrap();
    runtime.close().await;
    cleanup(&database, admin, &owner, &runtime_role).await;
}

#[tokio::test]
async fn runtime_guard_rejects_empty_and_unknown_schema_inventory() {
    let (database, admin, runtime, owner, runtime_role, schema, _password) = setup().await;
    assert!(assert_runtime_rls(&runtime, &[]).await.is_err());
    assert!(
        assert_runtime_rls(&runtime, &["does_not_exist"])
            .await
            .is_err()
    );
    runtime.close().await;
    cleanup(&database, admin, &owner, &runtime_role).await;
    let _ = schema;
}

#[tokio::test]
async fn platform_scope_rejects_missing_force_and_policy() {
    let (database, admin, runtime, owner, runtime_role, schema, password) = setup().await;
    let platform = runtime_pool(&database, &owner, &password).await;
    PgPlatformScope::new(platform.clone(), &[&schema])
        .await
        .expect("complete owner-backed platform contract must be admitted");

    execute(
        &admin,
        ddl!(
            "ALTER TABLE ",
            id(&schema),
            ".memory NO FORCE ROW LEVEL SECURITY"
        ),
    )
    .await;
    assert!(
        PgPlatformScope::new(platform.clone(), &[&schema])
            .await
            .is_err(),
        "owner-backed platform scope must require FORCE RLS"
    );
    execute(
        &admin,
        ddl!(
            "ALTER TABLE ",
            id(&schema),
            ".memory FORCE ROW LEVEL SECURITY"
        ),
    )
    .await;

    execute(
        &admin,
        ddl!("DROP POLICY proxiima_platform ON ", id(&schema), ".memory"),
    )
    .await;
    assert!(
        PgPlatformScope::new(platform.clone(), &[&schema])
            .await
            .is_err(),
        "platform scope must require the complete policy contract"
    );

    execute(
        &admin,
        ddl!("CREATE POLICY proxiima_platform ON ", id(&schema), ".memory FOR ALL TO ", id(&owner), " USING (current_setting('app.proxiima_scope', true) = 'platform') WITH CHECK (current_setting('app.proxiima_scope', true) = 'platform')"),
    )
    .await;
    PgPlatformScope::new(platform.clone(), &[&schema])
        .await
        .expect("repaired owner-backed platform contract must be admitted");

    platform.close().await;
    runtime.close().await;
    cleanup(&database, admin, &owner, &runtime_role).await;
}

async fn assert_goal_reader_policy_shape(admin: &PgPool, runtime: &PgPool, schemas: &[&str]) {
    execute(
        admin,
        "ALTER POLICY proxiima_goal_reader ON proxiima_core.goal_access_grant USING (false)",
    )
    .await;
    assert!(
        assert_runtime_rls(runtime, schemas).await.is_err(),
        "the guard must reject an invalid GoalReader policy"
    );
    let census_error = sqlx::query(
        "SELECT proxiima_core.owner_rls_census_table('proxiima_core', 'goal_access_grant')",
    )
    .execute(admin)
    .await
    .expect_err("SQL census rejects the same invalid policy");
    assert!(
        census_error
            .to_string()
            .contains("invalid Goal reader policy")
    );
    execute(
        admin,
        "ALTER POLICY proxiima_goal_reader ON proxiima_core.goal_access_grant USING (true)",
    )
    .await;
    assert_runtime_rls(runtime, schemas)
        .await
        .expect("restored GoalReader policy must be admitted");
}

#[tokio::test]
async fn information_schema_census_matches_catalog_guard_contract() {
    let (database, admin, runtime, platform, owner, runtime_role, _password) =
        setup_full_schema().await;
    let schemas = ["proxiima_core", "proxiima_example"];
    // The privileged inventory cannot omit a table because the runtime lacks a grant.
    let tables: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_schema, table_name FROM information_schema.tables
         WHERE table_schema = ANY($1) AND table_type = 'BASE TABLE'",
    )
    .bind(&schemas[..])
    .fetch_all(&admin)
    .await
    .unwrap();
    assert!(
        tables
            .iter()
            .any(|(s, t)| s == "proxiima_core" && t == "memory")
    );
    assert!(
        tables
            .iter()
            .any(|(s, t)| s == "proxiima_example" && t == "note_v1")
    );
    assert_goal_reader_policy_shape(&admin, &runtime, &schemas).await;
    for (schema, table) in &tables {
        sqlx::query("SELECT proxiima_core.owner_rls_census_table($1, $2)")
            .bind(schema)
            .bind(table)
            .execute(&admin)
            .await
            .unwrap();
    }
    let uncovered: Vec<String> = sqlx::query_scalar(
        "SELECT t.table_schema || '.' || t.table_name
           FROM information_schema.tables t
           JOIN pg_namespace n ON n.nspname = t.table_schema
           JOIN pg_class c ON c.relnamespace = n.oid AND c.relname = t.table_name
          WHERE t.table_schema = ANY($1) AND t.table_type = 'BASE TABLE'
            AND (NOT c.relrowsecurity OR NOT c.relforcerowsecurity
              OR (SELECT count(*) FROM pg_policy p WHERE p.polrelid = c.oid) <>
                 3 + (proxiima_core.is_search_candidate_relation(c.oid) AND EXISTS (
                     SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid
                       AND p.polname = 'proxiima_search_candidates'))::integer
                 + (EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid
                       AND p.polname = 'proxiima_goal_reader'))::integer
              OR NOT EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid
                 AND p.polname = 'proxiima_owner_read' AND p.polcmd = 'r')
              OR NOT EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid
                 AND p.polname = 'proxiima_owner_write' AND p.polcmd = '*')
              OR NOT EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid
                 AND p.polname = 'proxiima_platform' AND p.polroles = ARRAY[c.relowner]))",
    )
    .bind(&schemas[..])
    .fetch_all(&admin)
    .await
    .unwrap();
    assert!(uncovered.is_empty(), "unprotected tables: {uncovered:?}");
    // The guard validates the complete candidate signature and authority
    // before their explicit runtime execution grants enter the inventory.
    assert_runtime_rls(&runtime, &schemas).await.unwrap();
    let unsafe_definers: Vec<String> = sqlx::query_scalar(
        "SELECT n.nspname || '.' || p.proname
           FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = ANY($1) AND p.prosecdef
            AND (NOT EXISTS (
                  SELECT 1 FROM unnest(p.proconfig) config
                   WHERE config LIKE 'search_path=%' AND config ~ ', pg_temp$'
                 ) OR EXISTS (
                  SELECT 1 FROM aclexplode(COALESCE(p.proacl, acldefault('f', p.proowner))) acl
                   WHERE acl.grantee = 0 AND acl.privilege_type = 'EXECUTE'
                 ) OR (p.prorettype <> 'pg_catalog.trigger'::regtype
                     AND has_function_privilege($2, p.oid, 'EXECUTE')
                     AND p.proowner NOT IN (to_regrole('proxiima_search')::oid, to_regrole('proxiima_goal_reader')::oid)))",
    )
    .bind(&schemas[..])
    .bind(&runtime_role)
    .fetch_all(&admin)
    .await
    .unwrap();
    assert!(
        unsafe_definers.is_empty(),
        "additive migration left unsafe definers: {unsafe_definers:?}"
    );
    runtime.close().await;
    platform.close().await;
    cleanup(&database, admin, &owner, &runtime_role).await;
}

#[tokio::test]
async fn authenticated_witness_binds_and_clears_transaction_scope() {
    let (database, admin, runtime, owner, runtime_role, schema, password) = setup().await;
    runtime.close().await;
    let runtime = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            PgConnectOptions::from_str(&db_url(&database))
                .unwrap()
                .username(&runtime_role)
                .password(&password),
        )
        .await
        .unwrap();
    let subject = UserId::new(uuid::Uuid::now_v7());
    let verifier = SyntheticVerifier {
        roles: OwnerRoles::for_subject(subject, []).expect("synthetic roles"),
    };
    let authz = proxiima_core::authenticate(
        &verifier,
        &Credentials::Bearer("synthetic-test-credential".into()),
    )
    .await
    .unwrap();
    let scope = authz.owner_scope().expect("authenticated witness");

    let mut transaction = begin_owner_transaction(&runtime, scope).await.unwrap();
    let (scope_name, owner_setting): (String, String) = sqlx::query_as(
        "SELECT current_setting('app.proxiima_scope'), current_setting('app.owner')",
    )
    .fetch_one(&mut *transaction)
    .await
    .unwrap();
    assert_eq!(scope_name, "owner");
    assert!(owner_setting.contains(&subject.into_inner().to_string()));
    transaction.rollback().await.unwrap();

    let mut next = runtime.begin().await.unwrap();
    let scope_after_rollback: Option<String> =
        sqlx::query_scalar("SELECT current_setting('app.proxiima_scope', true)")
            .fetch_one(&mut *next)
            .await
            .unwrap();
    assert!(scope_after_rollback.is_none_or(|scope| scope.is_empty()));
    next.rollback().await.unwrap();
    let committed = begin_owner_transaction(&runtime, scope).await.unwrap();
    committed.commit().await.unwrap();
    let mut after_commit = runtime.begin().await.unwrap();
    let committed_scope: Option<String> =
        sqlx::query_scalar("SELECT current_setting('app.proxiima_scope', true)")
            .fetch_one(&mut *after_commit)
            .await
            .unwrap();
    assert!(committed_scope.is_none_or(|scope| scope.is_empty()));
    after_commit.rollback().await.unwrap();

    let mut errored = begin_owner_transaction(&runtime, scope).await.unwrap();
    assert!(
        sqlx::query("SELECT * FROM definitely_missing_owner_rls_table")
            .fetch_one(&mut *errored)
            .await
            .is_err()
    );
    errored.rollback().await.unwrap();
    let mut after_error = runtime.begin().await.unwrap();
    let error_scope: Option<String> =
        sqlx::query_scalar("SELECT current_setting('app.proxiima_scope', true)")
            .fetch_one(&mut *after_error)
            .await
            .unwrap();
    assert!(error_scope.is_none_or(|scope| scope.is_empty()));
    after_error.rollback().await.unwrap();

    let mut cancelled = begin_owner_transaction(&runtime, scope).await.unwrap();
    let cancellation = timeout(
        Duration::from_millis(1),
        sqlx::query("SELECT pg_sleep(1)").execute(&mut *cancelled),
    )
    .await;
    assert!(
        cancellation.is_err(),
        "the scoped statement must be cancellable"
    );
    drop(cancelled);
    let mut after_cancel = runtime.begin().await.unwrap();
    let cancelled_scope: Option<String> =
        sqlx::query_scalar("SELECT current_setting('app.proxiima_scope', true)")
            .fetch_one(&mut *after_cancel)
            .await
            .unwrap();
    assert!(cancelled_scope.is_none_or(|scope| scope.is_empty()));
    after_cancel.rollback().await.unwrap();
    runtime.close().await;
    cleanup(&database, admin, &owner, &runtime_role).await;
    let _ = schema;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::cast_possible_wrap,
    reason = "single end-to-end RLS acceptance scenario"
)]
async fn full_policy_isolation_uses_authenticated_scope_and_platform_role() {
    let (database, admin, runtime, platform, platform_role, runtime_role, password) =
        setup_full_schema().await;
    assert_runtime_rls(&runtime, &["proxiima_core", "proxiima_example"])
        .await
        .unwrap();

    let subject = UserId::new(uuid::Uuid::now_v7());
    let owner_a = uuid::Uuid::now_v7();
    let owner_b = uuid::Uuid::now_v7();
    let group_a = GroupId::new(owner_a);
    let group_b = GroupId::new(owner_b);
    // Raw INSERT: this test builds its own split-role database by hand; no PgStorage is connected to it
    sqlx::query(
        "INSERT INTO proxiima_core.owners(owner_id, kind) VALUES ($1,'group'),($2,'group')",
    )
    .bind(owner_a)
    .bind(owner_b)
    .execute(&admin)
    .await
    .unwrap();
    let mut owner_a_t = uuid::Uuid::nil();
    let mut owner_b_t = uuid::Uuid::nil();
    for (owner, handle, t, title) in [
        (owner_a, uuid::Uuid::now_v7(), uuid::Uuid::now_v7(), "A"),
        (owner_b, uuid::Uuid::now_v7(), uuid::Uuid::now_v7(), "B"),
    ] {
        if owner == owner_a {
            owner_a_t = t;
        } else {
            owner_b_t = t;
        }
        // Raw INSERT: this test builds its own split-role database by hand; no PgStorage is connected to it
        sqlx::query(
            "INSERT INTO proxiima_core.memory_head(handle,schema_id,kind,owner_id,t) VALUES ($1,'core/test','fact',$2,$3)",
        )
        .bind(handle)
        .bind(owner)
        .bind(t)
        .execute(&admin)
        .await
        .unwrap();
        let mut transaction = admin.begin().await.unwrap();
        // Raw INSERT: this test builds its own split-role database by hand; no PgStorage is connected to it
        sqlx::query("INSERT INTO proxiima_core.memory(handle,t,kind,owner_id,schema_id,sidecar_tables, occurred_at, \"by\", via) VALUES ($1,$2,'fact',$3,'core/test',ARRAY['proxiima_core.agent_note_v1'], uuid_extract_timestamp($2), '00000000-0000-0000-0000-000000000509'::uuid, NULL)")
            .bind(handle).bind(t).bind(owner).execute(&mut *transaction).await.unwrap();
        // Raw INSERT: this test builds its own split-role database by hand; no PgStorage is connected to it
        sqlx::query(
            "INSERT INTO proxiima_core.agent_note_v1(t,note_id,title,body) VALUES ($1,$2,$3,$3)",
        )
        .bind(t)
        .bind(owner_a_t)
        .bind(title)
        .execute(&mut *transaction)
        .await
        .unwrap();
        transaction.commit().await.unwrap();
    }

    let verifier = SyntheticVerifier {
        roles: OwnerRoles::for_subject(
            subject,
            [
                (OwnerId::from(group_a), Role::editor()),
                (OwnerId::from(group_b), Role::viewer()),
            ],
        )
        .unwrap(),
    };
    let authz = proxiima_core::authenticate(&verifier, &Credentials::Bearer("synthetic".into()))
        .await
        .unwrap();
    let narrowed = authz.narrowed_to_owner(OwnerId::from(group_a)).unwrap();
    let scope = narrowed.owner_scope().unwrap();
    let mut transaction = begin_owner_transaction(&runtime, scope).await.unwrap();
    let visible_sidecars: i64 =
        sqlx::query_scalar("SELECT count(*) FROM proxiima_core.agent_note_v1")
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
    assert_eq!(
        visible_sidecars, 1,
        "foreign sidecar must be redacted by parent ownership"
    );
    transaction.rollback().await.unwrap();

    let full_scope_verifier = SyntheticVerifier {
        roles: OwnerRoles::for_subject(
            subject,
            [
                (OwnerId::from(group_a), Role::editor()),
                (OwnerId::from(group_b), Role::editor()),
            ],
        )
        .unwrap(),
    };
    let full_scope_authz = proxiima_core::authenticate(
        &full_scope_verifier,
        &Credentials::Bearer("cross-owner-pin".into()),
    )
    .await
    .unwrap();
    let mut cross_owner_tx =
        begin_owner_transaction(&runtime, full_scope_authz.owner_scope().unwrap())
            .await
            .unwrap();
    let derived_handle = uuid::Uuid::now_v7();
    let derived_t = uuid::Uuid::now_v7();
    let derived_content = uuid::Uuid::now_v7();
    // Raw INSERT on purpose: written as the runtime role to exercise the cross-owner pin trigger bridge
    sqlx::query("INSERT INTO proxiima_core.content(content_id,owner_id,schema_id,content_hash) VALUES ($1,$2,'core/test',decode(repeat('00',32),'hex'))")
        .bind(derived_content).bind(owner_a)
        .execute(&mut *cross_owner_tx).await.unwrap();
    // Raw INSERT on purpose: written as the runtime role to exercise the cross-owner pin trigger bridge
    sqlx::query("INSERT INTO proxiima_core.memory_head(handle,schema_id,kind,owner_id,t) VALUES ($1,'core/test','abstraction',$2,$3)")
        .bind(derived_handle).bind(owner_a).bind(derived_t)
        .execute(&mut *cross_owner_tx).await.unwrap();
    // Raw INSERT on purpose: written as the runtime role to exercise the cross-owner pin trigger bridge
    sqlx::query("INSERT INTO proxiima_core.memory(handle,t,kind,owner_id,schema_id,content_id,origins, \"by\", via) VALUES ($1,$2,'abstraction',$3,'core/test',$4,ARRAY[$5]::uuid[], '00000000-0000-0000-0000-000000000509'::uuid, NULL)")
        .bind(derived_handle).bind(derived_t).bind(owner_a).bind(derived_content).bind(owner_b_t)
        .execute(&mut *cross_owner_tx).await.unwrap();
    let restored_scope: String =
        sqlx::query_scalar("SELECT current_setting('app.proxiima_scope', true)")
            .fetch_one(&mut *cross_owner_tx)
            .await
            .unwrap();
    assert_eq!(
        restored_scope, "owner",
        "platform trigger bridge must restore owner scope"
    );
    cross_owner_tx.commit().await.unwrap();

    let expected_trigger_functions = [
        "assert_erased_pin_target_insert",
        "memory_erase_witness",
        "goal_erase_witness",
        "goal_pin_target_checks",
        "memory_pin_checks",
        "cooled_forget_grounding",
        "pins_have_grounding_support",
    ];
    let installed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'proxiima_core' AND p.proname = ANY($1::text[])",
    )
    .bind(expected_trigger_functions.to_vec())
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(installed, expected_trigger_functions.len() as i64);
    let unsafe_definers: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace JOIN pg_roles r ON r.oid = p.proowner WHERE n.nspname='proxiima_core' AND p.proname = ANY($1::text[]) AND (r.rolsuper OR r.rolbypassrls)",
    )
    .bind(expected_trigger_functions.to_vec())
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(
        unsafe_definers, 0,
        "RLS trigger definers must be non-superuser/non-BYPASSRLS"
    );

    let new_handle = uuid::Uuid::now_v7();
    let new_t = uuid::Uuid::now_v7();
    let mut writer = begin_owner_transaction(&runtime, scope).await.unwrap();
    // Raw INSERT on purpose: written as the runtime role under owner RLS, the policy being tested
    sqlx::query("INSERT INTO proxiima_core.memory_head(handle,schema_id,kind,owner_id,t) VALUES ($1,'core/agent-note-v1','fact',$2,$3)")
        .bind(new_handle).bind(owner_a).bind(new_t)
        .execute(&mut *writer).await.unwrap();
    // Raw INSERT on purpose: written as the runtime role under owner RLS, the policy being tested
    sqlx::query("INSERT INTO proxiima_core.memory(handle,t,kind,owner_id,schema_id,sidecar_tables, occurred_at, \"by\", via) VALUES ($1,$2,'fact',$3,'core/agent-note-v1',ARRAY['proxiima_core.agent_note_v1'], uuid_extract_timestamp($2), '00000000-0000-0000-0000-000000000509'::uuid, NULL)")
        .bind(new_handle).bind(new_t).bind(owner_a)
        .execute(&mut *writer).await.unwrap();
    // Raw INSERT on purpose: written as the runtime role under owner RLS, the policy being tested
    sqlx::query("INSERT INTO proxiima_core.agent_note_v1(t,note_id,title,body) VALUES ($1,$2,'runtime','runtime')")
        .bind(new_t).bind(uuid::Uuid::now_v7())
        .execute(&mut *writer).await.unwrap();
    writer.commit().await.unwrap();
    let mut verify_writer = begin_owner_transaction(&runtime, scope).await.unwrap();
    let own_sidecars: i64 = sqlx::query_scalar("SELECT count(*) FROM proxiima_core.agent_note_v1")
        .fetch_one(&mut *verify_writer)
        .await
        .unwrap();
    assert_eq!(own_sidecars, 2);
    verify_writer.rollback().await.unwrap();

    let viewer = SyntheticVerifier {
        roles: OwnerRoles::for_subject(subject, [(OwnerId::from(group_a), Role::viewer())])
            .unwrap(),
    };
    let viewer_authz = proxiima_core::authenticate(&viewer, &Credentials::Bearer("viewer".into()))
        .await
        .unwrap();
    let mut viewer_tx = begin_owner_transaction(&runtime, viewer_authz.owner_scope().unwrap())
        .await
        .unwrap();
    // Raw INSERT on purpose: a viewer role must be refused by RLS
    let denied = sqlx::query("INSERT INTO proxiima_core.agent_note_v1(t,note_id,title,body) VALUES ($1,$2,'viewer','viewer')")
        .bind(owner_a_t)
        .bind(uuid::Uuid::now_v7())
        .execute(&mut *viewer_tx)
        .await;
    assert_rls_refusal(&denied.expect_err("viewer write ceiling must refuse INSERT"));
    viewer_tx.rollback().await.unwrap();

    let mixed = SyntheticVerifier {
        roles: OwnerRoles::for_subject(
            subject,
            [
                (OwnerId::from(group_a), Role::viewer()),
                (OwnerId::from(group_b), Role::editor()),
            ],
        )
        .unwrap(),
    };
    let mixed_authz = proxiima_core::authenticate(&mixed, &Credentials::Bearer("mixed".into()))
        .await
        .unwrap();
    let mut mixed_tx = begin_owner_transaction(&runtime, mixed_authz.owner_scope().unwrap())
        .await
        .unwrap();
    // Raw INSERT on purpose: a sidecar row under another owner must be refused by RLS
    let cross_owner_write = sqlx::query("INSERT INTO proxiima_core.agent_note_v1(t,note_id,title,body) VALUES ($1,$2,'mixed','mixed')")
        .bind(owner_a_t)
        .bind(uuid::Uuid::now_v7())
        .execute(&mut *mixed_tx)
        .await;
    assert_rls_refusal(&cross_owner_write.expect_err("writable B must not authorize sidecar A"));
    mixed_tx.rollback().await.unwrap();

    let mut unscoped_platform = platform.begin().await.unwrap();
    assert_unscoped_owner_access_denied(&mut unscoped_platform).await;
    unscoped_platform.rollback().await.unwrap();

    let mut forged_platform = runtime.begin().await.unwrap();
    sqlx::query("SET LOCAL app.proxiima_scope = 'platform'")
        .execute(&mut *forged_platform)
        .await
        .unwrap();
    assert_unscoped_owner_access_denied(&mut forged_platform).await;
    forged_platform.rollback().await.unwrap();

    let mut foreign_insert = begin_owner_transaction(&runtime, scope).await.unwrap();
    let foreign_t = uuid::Uuid::now_v7();
    // Raw INSERT on purpose: a head under another owner must be refused by RLS
    let error = sqlx::query("INSERT INTO proxiima_core.memory_head(handle,schema_id,kind,owner_id,t) VALUES ($1,'core/agent-note-v1','fact',$2,$1)")
        .bind(foreign_t).bind(owner_b).execute(&mut *foreign_insert).await
        .expect_err("owner A cannot insert owner B's head");
    assert_rls_refusal(&error);
    foreign_insert.rollback().await.unwrap();

    // Owner scope registers a lexical language on first use and writes no
    // other deployment metadata.
    let mut metadata_tx = begin_owner_transaction(&runtime, scope).await.unwrap();
    // Raw INSERT on purpose: owner scope may register a lexical language and nothing else
    sqlx::query(
        "INSERT INTO proxiima_core.lexical_languages (config)
         VALUES ('simple'::regconfig) ON CONFLICT DO NOTHING",
    )
    .execute(&mut *metadata_tx)
    .await
    .expect("ingest registers a language");
    let touched = [
        sqlx::query("UPDATE proxiima_core.lexical_default SET config = 'simple'::regconfig")
            .execute(&mut *metadata_tx)
            .await,
        sqlx::query("DELETE FROM proxiima_core.lexical_languages")
            .execute(&mut *metadata_tx)
            .await,
        sqlx::query("DELETE FROM proxiima_core.spiice_surface")
            .execute(&mut *metadata_tx)
            .await,
    ]
    .map(|outcome| outcome.expect("owner-scope metadata write").rows_affected());
    assert_eq!(
        touched,
        [0, 0, 0],
        "owner scope rewrote deployment metadata"
    );
    // Raw INSERT on purpose: owner scope must be refused a spiice surface
    let error = sqlx::query(
        "INSERT INTO proxiima_core.spiice_surface (table_name, spiice_id)
         VALUES ('proxiima_core.forged', 'forged')",
    )
    .execute(&mut *metadata_tx)
    .await
    .expect_err("owner scope registers no spiice surface");
    assert_rls_refusal(&error);
    metadata_tx.rollback().await.unwrap();

    let mut platform_tx = platform.begin().await.unwrap();
    sqlx::query("SET LOCAL app.proxiima_scope = 'platform'")
        .execute(&mut *platform_tx)
        .await
        .unwrap();
    let platform_scope: i64 = sqlx::query_scalar("SELECT count(*) FROM proxiima_core.owners")
        .fetch_one(&mut *platform_tx)
        .await
        .unwrap();
    assert_eq!(platform_scope, 2);
    platform_tx.rollback().await.unwrap();

    let mut empty_scope_tx = runtime.begin().await.unwrap();
    sqlx::query("SET LOCAL app.proxiima_scope = 'owner'")
        .execute(&mut *empty_scope_tx)
        .await
        .unwrap();
    sqlx::query("SET LOCAL app.owner = '{}'")
        .execute(&mut *empty_scope_tx)
        .await
        .unwrap();
    let metadata_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM proxiima_core.spiice_surface")
            .fetch_one(&mut *empty_scope_tx)
            .await
            .unwrap();
    assert_eq!(metadata_rows, 0, "empty owner array must hide metadata too");
    empty_scope_tx.rollback().await.unwrap();

    let mut bypass_tx = runtime.begin().await.unwrap();
    sqlx::query("SET LOCAL row_security = off")
        .execute(&mut *bypass_tx)
        .await
        .unwrap();
    assert_rls_refusal(
        &sqlx::query("SELECT count(*) FROM proxiima_core.owners")
            .execute(&mut *bypass_tx)
            .await
            .expect_err("runtime cannot disable row security"),
    );
    bypass_tx.rollback().await.unwrap();

    runtime.close().await;
    platform.close().await;
    cleanup(&database, admin, &platform_role, &runtime_role).await;
    let _ = password;
}
