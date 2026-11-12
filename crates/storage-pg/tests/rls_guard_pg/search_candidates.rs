//! The narrowly scoped candidate authority is the only runtime definer exception.

use proxiima_core::SqlText;
use proxiima_storage_pg::assert_runtime_rls;
use proxiima_storage_pg::search_candidates::ARGUMENT_TYPES;
use proxiima_storage_pg::test_fixtures::fresh_pg;
use sqlx::PgPool;

use super::{assert_rls_refusal, execute};

async fn assert_refused(runtime: &PgPool, reason: &str) {
    let error = assert_runtime_rls(runtime, &["proxiima_core"])
        .await
        .expect_err("unsafe candidate authority must fail boot")
        .to_string();
    assert!(error.contains(reason), "{error}");
}

#[tokio::test]
async fn runtime_cannot_assume_or_replace_candidate_authority() {
    let (pg, _database) = fresh_pg("candidate_runtime_authority").await;
    let admin = pg.pool_for_tests();
    let runtime = pg.clone_pool_for_backend();
    assert_runtime_rls(&runtime, &["proxiima_core"])
        .await
        .unwrap();
    execute(admin, "CREATE FUNCTION proxiima_core.search_lexical_candidates(integer) RETURNS integer LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS 'SELECT $1'").await;
    execute(
        admin,
        "REVOKE ALL ON FUNCTION proxiima_core.search_lexical_candidates(integer) FROM PUBLIC",
    )
    .await;
    execute(
        admin,
        "ALTER FUNCTION proxiima_core.search_lexical_candidates(integer) OWNER TO proxiima_search",
    )
    .await;
    assert_refused(&runtime, "search_lexical_candidates").await;
    execute(
        admin,
        "DROP FUNCTION proxiima_core.search_lexical_candidates(integer)",
    )
    .await;
    let error = sqlx::query("SET ROLE proxiima_search")
        .execute(&runtime)
        .await
        .expect_err("runtime cannot assume the candidate role");
    assert_rls_refusal(&error);
    let mut replacement =
        SqlText::new("CREATE OR REPLACE FUNCTION proxiima_core.search_lexical_candidates(");
    replacement.push(ARGUMENT_TYPES).push(
        ") \
         RETURNS TABLE(t uuid, schema_id text, lexical_score real) \
         LANGUAGE sql AS 'SELECT NULL::uuid, NULL::text, NULL::real WHERE false'",
    );
    let error = sqlx::query(replacement)
        .execute(&runtime)
        .await
        .expect_err("runtime cannot replace a candidate function");
    assert_rls_refusal(&error);
}

#[tokio::test]
async fn candidate_definers_reject_public_execution_and_hijacked_paths() {
    let (pg, _database) = fresh_pg("candidate_definer_catalog").await;
    let admin = pg.pool_for_tests();
    let runtime = pg.clone_pool_for_backend();
    let signature = ddl!(
        "proxiima_core.search_lexical_candidates(",
        lit(ARGUMENT_TYPES),
        ")"
    );
    execute(
        admin,
        ddl!("GRANT EXECUTE ON FUNCTION ", text(&signature), " TO PUBLIC"),
    )
    .await;
    assert_refused(&runtime, "PUBLIC EXECUTE").await;
    execute(
        admin,
        ddl!(
            "REVOKE EXECUTE ON FUNCTION ",
            text(&signature),
            " FROM PUBLIC"
        ),
    )
    .await;
    execute(
        admin,
        ddl!(
            "ALTER FUNCTION ",
            text(&signature),
            " SET search_path = pg_temp, pg_catalog"
        ),
    )
    .await;
    assert_refused(&runtime, "search_path").await;
    execute(
        admin,
        ddl!(
            "ALTER FUNCTION ",
            text(&signature),
            " SET search_path = pg_catalog, pg_temp"
        ),
    )
    .await;
    assert_runtime_rls(&runtime, &["proxiima_core"])
        .await
        .unwrap();
}

#[tokio::test]
async fn candidate_policy_is_read_only_and_requires_a_projection_shape() {
    let (pg, _database) = fresh_pg("candidate_policy_catalog").await;
    let admin = pg.pool_for_tests();
    let runtime = pg.clone_pool_for_backend();
    let platform: String = sqlx::query_scalar(
        "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE oid = 'proxiima_core.memory'::regclass",
    ).fetch_one(admin).await.unwrap();
    execute(admin, "CREATE TABLE proxiima_core.document_search(memory_id uuid PRIMARY KEY REFERENCES proxiima_core.memory(t), search_tsv tsvector NOT NULL)").await;
    execute(
        admin,
        ddl!(
            "ALTER TABLE proxiima_core.document_search OWNER TO ",
            id(&platform)
        ),
    )
    .await;
    execute(admin, "SELECT proxiima_core.owner_rls_apply_class('test', 'memory_owner', 'proxiima_core', 'document_search', 'memory_owner', NULL)").await;
    execute(admin, "CREATE POLICY proxiima_search_candidates ON proxiima_core.document_search FOR SELECT TO proxiima_search USING (true)").await;
    assert_runtime_rls(&runtime, &["proxiima_core"])
        .await
        .unwrap();
    execute(admin, "CREATE POLICY proxiima_search_candidates ON proxiima_core.memory FOR SELECT TO proxiima_search USING (true)").await;
    assert_refused(
        &runtime,
        "proxiima_core.memory has an invalid proxiima_search_candidates",
    )
    .await;
    execute(
        admin,
        "DROP POLICY proxiima_search_candidates ON proxiima_core.memory",
    )
    .await;
    execute(
        admin,
        "DROP POLICY proxiima_search_candidates ON proxiima_core.document_search",
    )
    .await;
    execute(admin, "CREATE POLICY proxiima_search_candidates ON proxiima_core.document_search FOR ALL TO proxiima_search USING (true) WITH CHECK (true)").await;
    assert_refused(
        &runtime,
        "document_search has an invalid proxiima_search_candidates",
    )
    .await;
    execute(
        admin,
        "DROP POLICY proxiima_search_candidates ON proxiima_core.document_search",
    )
    .await;
    execute(
        admin,
        "GRANT UPDATE ON proxiima_core.memory TO proxiima_search",
    )
    .await;
    assert_refused(&runtime, "read-only role").await;
    execute(
        admin,
        "REVOKE UPDATE ON proxiima_core.memory FROM proxiima_search",
    )
    .await;
    execute(
        admin,
        "GRANT CREATE ON SCHEMA proxiima_core TO proxiima_search",
    )
    .await;
    assert_refused(&runtime, "read-only role").await;
    execute(
        admin,
        "REVOKE CREATE ON SCHEMA proxiima_core FROM proxiima_search",
    )
    .await;
    assert_runtime_rls(&runtime, &["proxiima_core"])
        .await
        .unwrap();
}
