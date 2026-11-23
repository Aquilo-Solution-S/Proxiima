use std::sync::Arc;

use proxiima_core::test_fixtures::{owner_fixture, tool_ctx};
use proxiima_core::{AuthPath, AuthzContext, SpiiceRegistry, StorageError, ToolError};

use super::{Checked, owner_transaction};

#[tokio::test]
async fn a_context_without_the_service_answers_unavailable() {
    let owner = owner_fixture();
    let ctx = tool_ctx(
        owner,
        AuthzContext::single_owner(&owner, AuthPath::System),
        Arc::new(
            SpiiceRegistry::new()
                .try_freeze()
                .expect("an empty registry freezes"),
        ),
    );
    let refused = owner_transaction(&ctx)
        .await
        .expect_err("no service, no transaction");
    assert!(
        matches!(&refused, ToolError::Unavailable(message) if message.contains("not configured")),
        "{refused:?}"
    );
}

#[test]
fn a_statement_that_names_the_core_schema_is_refused_before_it_runs() {
    let refused = Checked::new(
        sqlx::query_as::<_, (uuid::Uuid,)>("SELECT t FROM PROXIIMA_CORE.memory WHERE t = $1")
            .bind(uuid::Uuid::nil()),
    )
    .err()
    .expect("core-schema SQL is refused");
    assert!(
        matches!(&refused, StorageError::Internal(message) if message.contains("proxiima_core")),
        "{refused:?}"
    );
    for sql in [
        r#"SELECT t FROM "proxiima_core".memory"#,
        "SELECT t FROM proxiima_core . memory",
        "SELECT t FROM proxiima_core/**/.memory",
        "SELECT t FROM proxiima_core--x\n.memory",
        r#"SELECT t FROM "proxiima_core" . "memory""#,
    ] {
        assert!(
            matches!(Checked::new(sqlx::query(sql)), Err(StorageError::Internal(message))
                if message.contains("proxiima_core")),
            "{sql:?} must be refused"
        );
    }
    assert!(Checked::new(sqlx::query("SELECT 1 FROM proxiima_example.notebooks")).is_ok());
}

#[test]
fn statements_that_change_settings_are_refused_before_they_run() {
    for sql in [
        "SELECT set_config('app.owner', $1, true)",
        "SELECT pg_catalog.set_config('app.owner', $1, false)",
        r#"SELECT "set_config" ('app.owner', $1, true)"#,
        r#"SELECT "pg_catalog"."set_config"('app.owner', $1, false)"#,
        r#"SELECT U&"set\005fconfig"('app.owner', $1, false)"#,
        "SET app.owner = 'value'",
        "RESET app.owner",
        "UPDATE pg_settings SET setting = $1 WHERE name = $2",
    ] {
        assert!(
            Checked::new(sqlx::query(sql)).is_err(),
            "{sql:?} must be refused"
        );
    }
    assert!(
        Checked::new(sqlx::query(
            "UPDATE proxiima_example.notebooks SET name = $1 WHERE notebook_id = $2",
        ))
        .is_ok()
    );
}

#[test]
fn postgres_xml_export_calls_are_outside_extra_table_sql() {
    for sql in [
        "SELECT query_to_xml($1, true, false, '')",
        r#"SELECT "pg_catalog".query_to_xml($1, true, false, '')"#,
        "SELECT table_to_xml($1::regclass, true, false, '')",
        r#"SELECT pg_catalog./**/"QUERY_TO_XML_AND_XMLSCHEMA"($1, true, false, '')"#,
        "SELECT cursor_to_xmlschema($1, true, false, '')",
        "SELECT schema_to_xml($1, true, false, '')",
        "SELECT database_to_xmlschema(true, false, '')",
    ] {
        let refused = Checked::new(sqlx::query(sql).bind("example"))
            .err()
            .unwrap_or_else(|| panic!("{sql:?} is refused"));
        assert!(
            matches!(&refused, StorageError::Internal(message) if message.contains("extra-table SQL")),
            "{sql:?}: {refused:?}"
        );
    }
}

#[test]
fn a_statement_that_is_not_a_data_statement_is_refused_before_it_runs() {
    for sql in [
        "COMMIT",
        "  rollback",
        "/* note */ COMMIT",
        "SAVEPOINT a",
        "SET LOCAL role = 'postgres'",
        "DO $$ BEGIN END $$",
        "",
    ] {
        let refused = Checked::new(sqlx::query(sql))
            .err()
            .unwrap_or_else(|| panic!("{sql:?} is refused"));
        assert!(
            matches!(&refused, StorageError::Internal(message) if message.contains("owner transaction")),
            "{sql:?}: {refused:?}"
        );
    }
    for sql in [
        "select 1",
        "\n  WITH a AS (SELECT 1) SELECT * FROM a",
        "INSERT INTO t VALUES (1)",
        "Update t SET a = 1",
        "DELETE FROM t",
        "MERGE INTO t USING s ON true WHEN MATCHED THEN DO NOTHING",
    ] {
        assert!(Checked::new(sqlx::query(sql)).is_ok(), "{sql:?}");
    }
}
