//! `proxiima_core.install_reference`: one call gives a column of a spiice or
//! host table its foreign key to a memory, an owner or a lexical language,
//! so a migration names no core table.

#[path = "fixtures/example_engine.rs"]
#[allow(dead_code, reason = "each suite uses part of the shared fixture")]
mod example_engine;

use proxiima::testkit::SplitRoleDb;
use sqlx::{Connection, PgConnection, Postgres, SqlSafeStr, Transaction};
use uuid::Uuid;

const SCHEMA: &str = "some_schema";
/// The code of every refusal core raises, and Postgres's own for a row that
/// names a missing parent.
const REFUSED: &str = "PX002";
const FOREIGN_KEY_VIOLATION: &str = "23503";

const STEP: &str = "CREATE TABLE some_schema.step (
    id bigint PRIMARY KEY, owner_id uuid NOT NULL, source_t uuid, label text,
    language regconfig)";

async fn migrated_platform() -> (SplitRoleDb, PgConnection) {
    let db = example_engine::migrated_db("proxiima_install_reference").await;
    let conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    (db, conn)
}

/// The scope a migration runs in, with the schema under test: the
/// transaction every test works in, rolled back when it drops.
async fn migration_scope(conn: &mut PgConnection) -> Transaction<'_, Postgres> {
    let mut tx = conn.begin().await.expect("transaction");
    sqlx::query("SELECT set_config('app.proxiima_scope', 'platform', true)")
        .execute(&mut *tx)
        .await
        .expect("platform scope");
    run(&mut tx, "CREATE SCHEMA some_schema").await;
    tx
}

async fn run(conn: &mut PgConnection, sql: impl SqlSafeStr) {
    let sql = sql.into_sql_str();
    sqlx::raw_sql(sql.clone())
        .execute(conn)
        .await
        .unwrap_or_else(|error| panic!("{}: {error}", sql.as_str()));
}

/// One call's arguments. `None` is SQL NULL.
#[derive(Clone, Copy)]
struct Call {
    schema: Option<&'static str>,
    table: &'static str,
    column: Option<&'static str>,
    referenced: Option<&'static str>,
    on_delete: Option<&'static str>,
}

impl Call {
    const fn schema(self, schema: Option<&'static str>) -> Self {
        Self { schema, ..self }
    }

    const fn table(self, table: &'static str) -> Self {
        Self { table, ..self }
    }

    const fn column(self, column: Option<&'static str>) -> Self {
        Self { column, ..self }
    }

    const fn referenced(self, referenced: Option<&'static str>) -> Self {
        Self { referenced, ..self }
    }

    const fn on_delete(self, on_delete: Option<&'static str>) -> Self {
        Self { on_delete, ..self }
    }
}

/// A call the installer takes on [`STEP`]; a refusal case changes one part.
const VALID: Call = Call {
    schema: Some(SCHEMA),
    table: "step",
    column: Some("source_t"),
    referenced: Some("memory"),
    on_delete: Some("no_action"),
};

async fn install(
    conn: &mut PgConnection,
    table: &'static str,
    column: &'static str,
    referenced: &'static str,
    on_delete: &'static str,
) -> Result<(), sqlx::Error> {
    let call = VALID
        .table(table)
        .column(Some(column))
        .referenced(Some(referenced))
        .on_delete(Some(on_delete));
    install_call(conn, call).await
}

async fn install_call(conn: &mut PgConnection, call: Call) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT proxiima_core.install_reference($1, $2, $3, $4, $5)")
        .bind(call.schema)
        .bind(call.table)
        .bind(call.column)
        .bind(call.referenced)
        .bind(call.on_delete)
        .execute(conn)
        .await
        .map(|_| ())
}

/// The SQLSTATE and message of a statement the database refused.
fn refusal(result: Result<(), sqlx::Error>, why: &str) -> (String, String) {
    let error = result.expect_err(why);
    let database = error
        .as_database_error()
        .unwrap_or_else(|| panic!("{why}: {error}"));
    (
        database.code().expect("a SQLSTATE").into_owned(),
        database.message().to_owned(),
    )
}

/// `(name, definition)` of the foreign keys of one table.
async fn foreign_keys(conn: &mut PgConnection, table: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT conname::text, pg_get_constraintdef(oid) FROM pg_constraint
          WHERE conrelid = ($1 || '.' || $2)::regclass AND contype = 'f' ORDER BY 1",
    )
    .bind(SCHEMA)
    .bind(table)
    .fetch_all(conn)
    .await
    .expect("foreign keys")
}

fn key(name: &str, definition: &str) -> (String, String) {
    (name.to_owned(), definition.to_owned())
}

#[tokio::test]
async fn a_memory_reference_carries_the_delete_action_the_call_names() {
    let (_db, mut conn) = migrated_platform().await;
    for (on_delete, definition) in [
        (
            "no_action",
            "FOREIGN KEY (source_t) REFERENCES proxiima_core.memory(t)",
        ),
        (
            "cascade",
            "FOREIGN KEY (source_t) REFERENCES proxiima_core.memory(t) ON DELETE CASCADE",
        ),
    ] {
        let mut tx = migration_scope(&mut conn).await;
        run(&mut tx, STEP).await;

        install(&mut tx, "step", "source_t", "memory", on_delete)
            .await
            .expect("the installer takes the column");

        assert_eq!(
            foreign_keys(&mut tx, "step").await,
            [key("step_source_t_fkey", definition)]
        );
        let mut attempt = tx.begin().await.expect("savepoint");
        let unknown =
            sqlx::query("INSERT INTO some_schema.step (id, owner_id, source_t) VALUES (1, $1, $2)")
                .bind(Uuid::now_v7())
                .bind(Uuid::now_v7())
                .execute(&mut *attempt)
                .await
                .map(|_| ());
        let (code, message) = refusal(unknown, "a memory that does not exist");
        assert_eq!(code, FOREIGN_KEY_VIOLATION, "{message}");
    }
}

#[tokio::test]
async fn an_owner_reference_admits_a_registered_owner_only() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    run(&mut tx, STEP).await;
    run(
        &mut tx,
        "SELECT proxiima_core.install_owner_rls_table('some_schema', 'step', 'owner_id')",
    )
    .await;

    install(&mut tx, "step", "owner_id", "owner", "no_action")
        .await
        .expect("the installer takes the owner column");

    assert_eq!(
        foreign_keys(&mut tx, "step").await,
        [key(
            "step_owner_id_fkey",
            "FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id)"
        )]
    );
    sqlx::query("SELECT proxiima_core.assert_owner_rls_census($1)")
        .bind(SCHEMA)
        .execute(&mut *tx)
        .await
        .expect("the table keeps the policies of its class");

    let registered = Uuid::now_v7();
    sqlx::query("INSERT INTO proxiima_core.owners (owner_id, kind) VALUES ($1, 'personal')")
        .bind(registered)
        .execute(&mut *tx)
        .await
        .expect("register the owner");
    sqlx::query("INSERT INTO some_schema.step (id, owner_id) VALUES (1, $1)")
        .bind(registered)
        .execute(&mut *tx)
        .await
        .expect("a registered owner is admitted");
    let unknown = sqlx::query("INSERT INTO some_schema.step (id, owner_id) VALUES (2, $1)")
        .bind(Uuid::now_v7())
        .execute(&mut *tx)
        .await
        .map(|_| ());
    let (code, message) = refusal(unknown, "an owner that does not exist");
    assert_eq!(code, FOREIGN_KEY_VIOLATION, "{message}");
}

#[tokio::test]
async fn a_lexical_language_reference_admits_a_registered_language_only() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    run(&mut tx, STEP).await;

    install(&mut tx, "step", "language", "lexical_language", "no_action")
        .await
        .expect("the installer takes the regconfig column");

    assert_eq!(
        foreign_keys(&mut tx, "step").await,
        [key(
            "step_language_fkey",
            "FOREIGN KEY (language) REFERENCES proxiima_core.lexical_languages(config)"
        )]
    );
    sqlx::query("INSERT INTO some_schema.step (id, owner_id, language) VALUES (1, $1, 'english')")
        .bind(Uuid::now_v7())
        .execute(&mut *tx)
        .await
        .expect("a registered language is admitted");
    let mut attempt = tx.begin().await.expect("savepoint");
    let unknown = sqlx::query(
        "INSERT INTO some_schema.step (id, owner_id, language) VALUES (2, $1, 'german')",
    )
    .bind(Uuid::now_v7())
    .execute(&mut *attempt)
    .await
    .map(|_| ());
    let (code, message) = refusal(unknown, "a language that is not registered");
    assert_eq!(code, FOREIGN_KEY_VIOLATION, "{message}");
}

#[tokio::test]
async fn a_second_call_changes_nothing() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    run(&mut tx, STEP).await;
    install(&mut tx, "step", "source_t", "memory", "cascade")
        .await
        .expect("first");
    let before = foreign_keys(&mut tx, "step").await;

    install(&mut tx, "step", "source_t", "memory", "cascade")
        .await
        .expect("second");

    assert_eq!(foreign_keys(&mut tx, "step").await, before);
}

/// A table that wrote its foreign key by hand adopts the installer in a later
/// migration: the same statement keeps the key, another one is refused and
/// names the constraint to drop.
#[tokio::test]
async fn a_key_the_column_already_has_is_kept_or_refused_by_its_delete_action() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    run(
        &mut tx,
        "CREATE TABLE some_schema.adopted (id bigint PRIMARY KEY, source_t uuid,
            CONSTRAINT own_name FOREIGN KEY (source_t)
                REFERENCES proxiima_core.memory (t) ON DELETE CASCADE)",
    )
    .await;
    let before = foreign_keys(&mut tx, "adopted").await;

    install(&mut tx, "adopted", "source_t", "memory", "cascade")
        .await
        .expect("the same delete action");
    assert_eq!(foreign_keys(&mut tx, "adopted").await, before);

    let mut attempt = tx.begin().await.expect("savepoint");
    let (code, message) = refusal(
        install(&mut attempt, "adopted", "source_t", "memory", "no_action").await,
        "another delete action",
    );
    assert_eq!(code, REFUSED, "{message}");
    assert!(
        message.contains("through own_name, whose delete action is cascade, not no_action"),
        "{message}"
    );
    drop(attempt);

    // A key with the named action does not hide one with another.
    run(
        &mut tx,
        "ALTER TABLE some_schema.adopted ADD CONSTRAINT a_second_key
            FOREIGN KEY (source_t) REFERENCES proxiima_core.memory (t)",
    )
    .await;
    for on_delete in ["no_action", "cascade"] {
        let mut attempt = tx.begin().await.expect("savepoint");
        let (code, message) = refusal(
            install(&mut attempt, "adopted", "source_t", "memory", on_delete).await,
            "two keys with different delete actions",
        );
        assert_eq!(code, REFUSED, "{on_delete}: {message}");
        assert!(message.contains("whose delete action is"), "{message}");
    }
}

#[tokio::test]
async fn a_call_outside_the_contract_is_refused() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    run(&mut tx, STEP).await;
    // Fine as names, but `<table>_<column>_fkey` is 64 bytes.
    let long_table = "tttttttttttttttttttttttttttttttttttttttttttttttttt";
    run(
        &mut tx,
        "CREATE TABLE some_schema.tttttttttttttttttttttttttttttttttttttttttttttttttt (
            id bigint PRIMARY KEY, source_t uuid)",
    )
    .await;

    let cases = [
        (
            VALID.referenced(Some("goal")),
            "not memory, owner or lexical_language",
        ),
        (
            VALID.referenced(None),
            "not memory, owner or lexical_language",
        ),
        (
            VALID.referenced(Some("lexical_languages")),
            "not memory, owner or lexical_language",
        ),
        (
            VALID.on_delete(Some("set_null")),
            "neither no_action nor cascade",
        ),
        (
            VALID.on_delete(Some("CASCADE")),
            "neither no_action nor cascade",
        ),
        (VALID.on_delete(None), "neither no_action nor cascade"),
        (
            VALID
                .column(Some("owner_id"))
                .referenced(Some("owner"))
                .on_delete(Some("cascade")),
            "cannot cascade from its owner",
        ),
        (
            VALID
                .column(Some("language"))
                .referenced(Some("lexical_language"))
                .on_delete(Some("cascade")),
            "cannot cascade from its lexical language",
        ),
        (
            VALID.referenced(Some("lexical_language")),
            "is uuid, not regconfig",
        ),
        (VALID.column(Some("language")), "is regconfig, not uuid"),
        (VALID.table("missing"), "is not an existing base table"),
        (VALID.column(Some("missing")), "has no column missing"),
        (VALID.column(None), "has no column NULL"),
        (VALID.column(Some("label")), "is text, not uuid"),
        (
            VALID
                .schema(Some("proxiima_core"))
                .table("memory")
                .column(Some("forked_from")),
            "is not a spiice schema",
        ),
        (VALID.schema(None), "is not a spiice schema"),
        (VALID.table(long_table), "longer than 63 bytes"),
    ];
    for (call, expected) in cases {
        let mut attempt = tx.begin().await.expect("savepoint");
        let (code, message) = refusal(install_call(&mut attempt, call).await, expected);
        assert_eq!(code, REFUSED, "{expected}: {message}");
        assert!(message.contains(expected), "{expected}: {message}");
    }
    assert_eq!(foreign_keys(&mut tx, "step").await, []);
}

#[tokio::test]
async fn the_installer_refuses_outside_platform_scope() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = conn.begin().await.expect("transaction");
    run(&mut tx, "CREATE SCHEMA some_schema").await;
    run(&mut tx, STEP).await;

    let (code, message) = refusal(
        install(&mut tx, "step", "source_t", "memory", "no_action").await,
        "no platform scope",
    );

    assert_eq!(code, REFUSED, "{message}");
    assert!(message.contains("platform scope only"), "{message}");
}

/// A sidecar row is stamped on its own memory, so nothing else may delete it:
/// a sidecar references another memory with `no_action`.
#[tokio::test]
async fn a_memory_sidecar_references_another_memory_with_no_action_only() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    run(
        &mut tx,
        "CREATE TABLE some_schema.part_v1 (t uuid PRIMARY KEY, whole_t uuid NOT NULL)",
    )
    .await;
    run(
        &mut tx,
        "SELECT proxiima_core.install_sidecar('some_schema', 'part_v1', 'some-spiice')",
    )
    .await;

    let mut attempt = tx.begin().await.expect("savepoint");
    let (code, message) = refusal(
        install(&mut attempt, "part_v1", "whole_t", "memory", "cascade").await,
        "a cascade on a sidecar",
    );
    assert_eq!(code, REFUSED, "{message}");
    assert!(message.contains("is a memory sidecar"), "{message}");
    drop(attempt);

    install(&mut tx, "part_v1", "whole_t", "memory", "no_action")
        .await
        .expect("a sidecar references another memory with no_action");
    assert_eq!(
        foreign_keys(&mut tx, "part_v1").await,
        [
            key(
                "part_v1_t_fkey",
                "FOREIGN KEY (t) REFERENCES proxiima_core.memory(t)"
            ),
            key(
                "part_v1_whole_t_fkey",
                "FOREIGN KEY (whole_t) REFERENCES proxiima_core.memory(t)"
            ),
        ]
    );
}

/// The same rule holds the other way round: a table that already goes with
/// another memory, through the installer or a key written by hand, is not
/// registered as a sidecar, and the refusal names the constraint.
#[tokio::test]
async fn a_table_that_cascades_from_another_memory_is_not_installed_as_a_sidecar() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    run(
        &mut tx,
        "CREATE TABLE some_schema.part_v1 (t uuid PRIMARY KEY, whole_t uuid NOT NULL);
         CREATE TABLE some_schema.by_hand_v1 (t uuid PRIMARY KEY, whole_t uuid NOT NULL,
            CONSTRAINT by_hand_whole FOREIGN KEY (whole_t)
                REFERENCES proxiima_core.memory (t) ON DELETE CASCADE)",
    )
    .await;
    install(&mut tx, "part_v1", "whole_t", "memory", "cascade")
        .await
        .expect("not yet a sidecar");

    for (table, constraint) in [
        ("part_v1", "part_v1_whole_t_fkey"),
        ("by_hand_v1", "by_hand_whole"),
    ] {
        let mut attempt = tx.begin().await.expect("savepoint");
        let (code, message) = refusal(
            install_sidecar(&mut attempt, table).await,
            "a sidecar that cascades from another memory",
        );
        assert_eq!(code, REFUSED, "{message}");
        assert!(
            message.contains(&format!("goes with another memory through {constraint}")),
            "{message}"
        );
        assert!(message.contains("'no_action'"), "{message}");
        drop(attempt);
        assert!(
            surface_rows(&mut tx, table).await.is_empty(),
            "{table} stays unregistered"
        );
    }
}

/// A sidecar that references another memory with `no_action` before it is
/// installed is a sidecar, and installing it again changes nothing.
#[tokio::test]
async fn a_no_action_reference_before_the_sidecar_is_installed_is_kept() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    run(
        &mut tx,
        "CREATE TABLE some_schema.part_v1 (t uuid PRIMARY KEY REFERENCES proxiima_core.memory (t) ON DELETE CASCADE,
            whole_t uuid NOT NULL)",
    )
    .await;
    install(&mut tx, "part_v1", "whole_t", "memory", "no_action")
        .await
        .expect("the reference");

    install_sidecar(&mut tx, "part_v1")
        .await
        .expect("a cascade on the memory key itself and a no_action reference");
    let before = (
        foreign_keys(&mut tx, "part_v1").await,
        surface_rows(&mut tx, "part_v1").await,
    );
    install_sidecar(&mut tx, "part_v1").await.expect("second");

    assert_eq!(
        (
            foreign_keys(&mut tx, "part_v1").await,
            surface_rows(&mut tx, "part_v1").await
        ),
        before
    );
    assert_eq!(
        before.0,
        [
            key(
                "part_v1_t_fkey",
                "FOREIGN KEY (t) REFERENCES proxiima_core.memory(t) ON DELETE CASCADE"
            ),
            key(
                "part_v1_whole_t_fkey",
                "FOREIGN KEY (whole_t) REFERENCES proxiima_core.memory(t)"
            ),
        ]
    );
    assert_eq!(before.1, ["some_schema.part_v1"]);
}

async fn install_sidecar(conn: &mut PgConnection, table: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT proxiima_core.install_sidecar($1, $2, 'some-spiice')")
        .bind(SCHEMA)
        .bind(table)
        .execute(conn)
        .await
        .map(|_| ())
}

async fn surface_rows(conn: &mut PgConnection, table: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT table_name FROM proxiima_core.spiice_surface WHERE table_name = $1 || '.' || $2",
    )
    .bind(SCHEMA)
    .bind(table)
    .fetch_all(conn)
    .await
    .expect("surface rows")
}

/// The owner policies of a table follow its one memory key. A second
/// reference that leaves that key undecided is refused by the call that adds
/// it, and the table keeps what it had.
#[tokio::test]
async fn a_reference_that_makes_the_tables_memory_ambiguous_is_refused() {
    let (_db, mut conn) = migrated_platform().await;
    let mut tx = migration_scope(&mut conn).await;
    run(
        &mut tx,
        "CREATE TABLE some_schema.verdict (id bigint PRIMARY KEY, owner_id uuid NOT NULL,
            proposal_t uuid NOT NULL, outcome_t uuid);
         SELECT proxiima_core.install_owner_rls_table('some_schema', 'verdict', 'owner_id')",
    )
    .await;
    install(&mut tx, "verdict", "proposal_t", "memory", "cascade")
        .await
        .expect("one memory reference");
    let before = foreign_keys(&mut tx, "verdict").await;

    let mut attempt = tx.begin().await.expect("savepoint");
    let (code, message) = refusal(
        install(&mut attempt, "verdict", "outcome_t", "memory", "cascade").await,
        "a second memory reference",
    );
    assert_eq!(code, REFUSED, "{message}");
    assert!(message.contains("its memory is ambiguous"), "{message}");
    drop(attempt);

    assert_eq!(foreign_keys(&mut tx, "verdict").await, before);
}
