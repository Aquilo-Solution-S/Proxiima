//! `proxiima_core.install_sidecar`: one call gives a spiice table its memory
//! foreign key, surface row, integrity triggers and owner-RLS policies, so a
//! spiice migration names no core table.

#[path = "fixtures/example_engine.rs"]
#[allow(dead_code, reason = "each suite uses part of the shared fixture")]
mod example_engine;

use proxiima::testkit::SplitRoleDb;
use proxiima_storage_pg::integrity::{
    declaration_trigger, delete_guard_trigger, key_repoint_trigger, presence_trigger,
};
use sqlx::{Connection, PgConnection, Postgres, Transaction};

const SCHEMA: &str = "some_schema";
const SPIICE: &str = "some-spiice";

/// A migrated database, a platform connection and the scope a migration runs
/// in: the transaction every test works in, rolled back when it drops.
async fn migration_scope(conn: &mut PgConnection) -> Transaction<'_, Postgres> {
    let mut tx = conn.begin().await.expect("transaction");
    sqlx::query("SELECT set_config('app.proxiima_scope', 'platform', true)")
        .execute(&mut *tx)
        .await
        .expect("platform scope");
    create(&mut tx, "CREATE SCHEMA some_schema").await;
    tx
}

async fn migrated_platform() -> (SplitRoleDb, PgConnection) {
    let db = example_engine::migrated_db("proxiima_install_sidecar").await;
    let conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    (db, conn)
}

async fn create(tx: &mut PgConnection, ddl: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl.to_owned()))
        .execute(tx)
        .await
        .unwrap_or_else(|error| panic!("{ddl}: {error}"));
}

async fn install(conn: &mut PgConnection, table: &str, spiice: &str) -> Result<(), sqlx::Error> {
    install_in(conn, SCHEMA, table, spiice).await
}

async fn install_in(
    conn: &mut PgConnection,
    schema: &str,
    table: &str,
    spiice: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT proxiima_core.install_sidecar($1, $2, $3)")
        .bind(schema)
        .bind(table)
        .bind(spiice)
        .execute(conn)
        .await
        .map(|_| ())
}

fn refusal(result: Result<(), sqlx::Error>, why: &str) -> String {
    result.expect_err(why).to_string()
}

/// The names and `pg_get_triggerdef` of the sidecar triggers of one table:
/// the ones on the table, and its presence trigger on `proxiima_core.memory`.
async fn trigger_defs(conn: &mut PgConnection, table: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT t.tgname::text, pg_get_triggerdef(t.oid)
           FROM pg_trigger AS t
          WHERE NOT t.tgisinternal
            AND (t.tgrelid = ($1 || '.' || $2)::regclass
                 OR (t.tgrelid = 'proxiima_core.memory'::regclass
                     AND t.tgname = 'memory_declares_' || $1 || '_' || $2))
          ORDER BY 1",
    )
    .bind(SCHEMA)
    .bind(table)
    .fetch_all(conn)
    .await
    .expect("trigger definitions")
}

async fn trigger_names(conn: &mut PgConnection, table: &str) -> Vec<String> {
    trigger_defs(conn, table)
        .await
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

async fn memory_foreign_keys(conn: &mut PgConnection, table: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT conname::text FROM pg_constraint
          WHERE conrelid = ($1 || '.' || $2)::regclass AND contype = 'f'
            AND confrelid = 'proxiima_core.memory'::regclass
          ORDER BY 1",
    )
    .bind(SCHEMA)
    .bind(table)
    .fetch_all(conn)
    .await
    .expect("foreign keys")
}

/// `(policy, command, USING, WITH CHECK)` of a table's policies.
async fn policies(conn: &mut PgConnection, table: &str) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT policyname::text, cmd::text, COALESCE(qual, ''), COALESCE(with_check, '')
           FROM pg_policies WHERE schemaname = $1 AND tablename = $2 ORDER BY 1",
    )
    .bind(SCHEMA)
    .bind(table)
    .fetch_all(conn)
    .await
    .expect("policies")
}

async fn surface_rows(conn: &mut PgConnection, table: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT table_name, spiice_id FROM proxiima_core.spiice_surface
          WHERE table_name = $1 || '.' || $2 ORDER BY 1",
    )
    .bind(SCHEMA)
    .bind(table)
    .fetch_all(conn)
    .await
    .expect("surface rows")
}

async fn constraint_defs(conn: &mut PgConnection, table: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT conname::text, pg_get_constraintdef(oid) FROM pg_constraint
          WHERE conrelid = ($1 || '.' || $2)::regclass ORDER BY 1",
    )
    .bind(SCHEMA)
    .bind(table)
    .fetch_all(conn)
    .await
    .expect("constraints")
}

async fn rls_forced(conn: &mut PgConnection, table: &str) -> (bool, bool) {
    sqlx::query_as(
        "SELECT relrowsecurity, relforcerowsecurity FROM pg_class
          WHERE oid = ($1 || '.' || $2)::regclass",
    )
    .bind(SCHEMA)
    .bind(table)
    .fetch_one(conn)
    .await
    .expect("RLS flags")
}

const THING: &str = "CREATE TABLE some_schema.thing_v1 (t uuid PRIMARY KEY, body text NOT NULL)";

/// Everything the installer sets up is there after one call, and the four
/// triggers are exactly what the integrity generators emit for the table.
/// The comparison runs the generators' own statements on the same table
/// after dropping the installer's triggers, and compares `pg_get_triggerdef`
/// before and after.
#[tokio::test]
async fn one_call_installs_the_key_surface_triggers_and_policies() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    create(&mut tx, THING).await;

    install(&mut tx, "thing_v1", SPIICE)
        .await
        .expect("the installer takes the table");

    assert_eq!(
        memory_foreign_keys(&mut tx, "thing_v1").await,
        ["thing_v1_t_fkey"]
    );
    assert_eq!(
        surface_rows(&mut tx, "thing_v1").await,
        [("some_schema.thing_v1".to_owned(), SPIICE.to_owned())]
    );
    assert_eq!(rls_forced(&mut tx, "thing_v1").await, (true, true));
    assert_eq!(policies(&mut tx, "thing_v1").await.len(), 3);
    sqlx::query("SELECT proxiima_core.assert_owner_rls_census($1)")
        .bind(SCHEMA)
        .execute(&mut *tx)
        .await
        .expect("the census passes");

    let installed = trigger_defs(&mut tx, "thing_v1").await;
    assert_eq!(installed.len(), 4, "{installed:?}");

    let table = "some_schema.thing_v1";
    let generated = [
        declaration_trigger(table, "t").expect("declaration trigger"),
        key_repoint_trigger(table, "t").expect("repoint trigger"),
        presence_trigger(table, "t").expect("presence trigger"),
        delete_guard_trigger(table, "t").expect("delete guard"),
    ];
    for artifact in &generated {
        create(&mut tx, &artifact.inverse).await;
    }
    assert_eq!(trigger_defs(&mut tx, "thing_v1").await, []);
    for artifact in &generated {
        create(&mut tx, &artifact.forward).await;
    }
    assert_eq!(
        trigger_defs(&mut tx, "thing_v1").await,
        installed,
        "the installer's triggers render as the generators' do"
    );
}

#[tokio::test]
async fn a_second_call_changes_nothing() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    create(&mut tx, THING).await;
    install(&mut tx, "thing_v1", SPIICE).await.expect("first");
    let before = (
        constraint_defs(&mut tx, "thing_v1").await,
        trigger_defs(&mut tx, "thing_v1").await,
        policies(&mut tx, "thing_v1").await,
        surface_rows(&mut tx, "thing_v1").await,
    );

    install(&mut tx, "thing_v1", SPIICE).await.expect("second");

    let after = (
        constraint_defs(&mut tx, "thing_v1").await,
        trigger_defs(&mut tx, "thing_v1").await,
        policies(&mut tx, "thing_v1").await,
        surface_rows(&mut tx, "thing_v1").await,
    );
    assert_eq!(before, after);
}

#[tokio::test]
async fn an_adopted_table_keeps_its_own_memory_foreign_key() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    create(
        &mut tx,
        "CREATE TABLE some_schema.adopted_v1 (t uuid PRIMARY KEY,
            CONSTRAINT adopted_own_fk FOREIGN KEY (t) REFERENCES proxiima_core.memory (t))",
    )
    .await;

    install(&mut tx, "adopted_v1", SPIICE).await.expect("adopt");

    assert_eq!(
        memory_foreign_keys(&mut tx, "adopted_v1").await,
        ["adopted_own_fk"]
    );
}

#[tokio::test]
async fn an_owner_pinned_table_gets_only_the_declaration_triggers_and_owner_id_policies() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    create(
        &mut tx,
        "CREATE TABLE some_schema.owned_v1 (t uuid PRIMARY KEY, owner_id uuid NOT NULL)",
    )
    .await;

    install(&mut tx, "owned_v1", SPIICE).await.expect("install");

    assert_eq!(
        trigger_names(&mut tx, "owned_v1").await,
        [
            "owned_v1_declared_by_memory",
            "owned_v1_declared_by_memory_on_update"
        ]
    );
    let policies = policies(&mut tx, "owned_v1").await;
    assert_eq!(policies.len(), 3);
    assert!(
        policies
            .iter()
            .any(|(_, _, using, check)| using.contains("owner_id") || check.contains("owner_id")),
        "class owner_id policies read the owner_id column: {policies:?}"
    );
}

/// The host marks its Follows tables before the spiice lanes run; a table
/// the mark names keeps the delete guard and the presence trigger although
/// it has its own `owner_id`.
#[tokio::test]
async fn a_follows_table_gets_all_four_triggers() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    sqlx::query("SELECT proxiima_core.sync_owner_follows($1, $2)")
        .bind(vec![SCHEMA])
        .bind(vec!["some_schema.followed_v1"])
        .execute(&mut *tx)
        .await
        .expect("the host marks the table before its lane creates it");
    create(
        &mut tx,
        "CREATE TABLE some_schema.followed_v1 (t uuid PRIMARY KEY, owner_id uuid NOT NULL)",
    )
    .await;

    install(&mut tx, "followed_v1", SPIICE)
        .await
        .expect("install");

    assert_eq!(
        trigger_names(&mut tx, "followed_v1").await,
        [
            "followed_v1_declared_by_memory",
            "followed_v1_declared_by_memory_on_delete",
            "followed_v1_declared_by_memory_on_update",
            "memory_declares_some_schema_followed_v1",
        ]
    );
    sqlx::query("SELECT proxiima_core.assert_owner_rls_census($1)")
        .bind(SCHEMA)
        .execute(&mut *tx)
        .await
        .expect("the follows rule passes the census");
}

#[tokio::test]
async fn the_installer_refuses_outside_platform_scope() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = conn.begin().await.expect("transaction");
    create(&mut tx, "CREATE SCHEMA some_schema").await;
    create(&mut tx, THING).await;

    let message = refusal(
        install(&mut tx, "thing_v1", SPIICE).await,
        "no platform scope",
    );

    assert!(message.contains("platform scope only"), "{message}");
}

#[tokio::test]
async fn an_identifier_that_is_not_lower_case_is_refused() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    create(&mut tx, THING).await;

    for (schema, table) in [
        (SCHEMA, "Thing_v1"),
        (SCHEMA, "thing_v1.t"),
        ("Some_schema", "thing_v1"),
    ] {
        let mut attempt = tx.begin().await.expect("savepoint");
        let message = refusal(
            install_in(&mut attempt, schema, table, SPIICE).await,
            "a bad identifier",
        );
        assert!(
            message.contains("lower-case identifier"),
            "{schema}.{table}: {message}"
        );
    }
}

/// The trigger generators spell the key column unquoted, so a key that only
/// works quoted would install and then fail the boot comparison.
#[tokio::test]
async fn a_reserved_word_as_memory_key_is_refused() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    create(
        &mut tx,
        r#"CREATE TABLE some_schema.keyed_v1 ("user" uuid PRIMARY KEY)"#,
    )
    .await;

    let result = sqlx::query(
        "SELECT proxiima_core.install_sidecar($1, 'keyed_v1', $2, memory_key => 'user')",
    )
    .bind(SCHEMA)
    .bind(SPIICE)
    .execute(&mut *tx)
    .await
    .map(|_| ());

    let message = refusal(result, "a reserved word");
    assert!(message.contains("memory key user"), "{message}");
}

#[tokio::test]
async fn a_derived_trigger_name_over_63_bytes_is_refused() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    // Fine as a table name, but `<table>_declared_by_memory_on_update` is 70 bytes.
    let table = "t".repeat(41);
    create(
        &mut tx,
        "CREATE TABLE some_schema.ttttttttttttttttttttttttttttttttttttttttt (t uuid PRIMARY KEY)",
    )
    .await;

    let message = refusal(install(&mut tx, &table, SPIICE).await, "a long name");

    assert!(message.contains("longer than 63 bytes"), "{message}");
}

#[tokio::test]
async fn a_table_that_does_not_exist_is_refused() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;

    let message = refusal(install(&mut tx, "missing_v1", SPIICE).await, "no table");

    assert!(
        message.contains("is not an existing base table"),
        "{message}"
    );
}

#[tokio::test]
async fn a_table_registered_to_another_spiice_is_refused() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    create(&mut tx, THING).await;
    install(&mut tx, "thing_v1", SPIICE)
        .await
        .expect("first owner");

    let message = refusal(
        install(&mut tx, "thing_v1", "another-spiice").await,
        "a second spiice",
    );

    assert!(
        message.contains("already registered to spiice some-spiice"),
        "{message}"
    );
}
