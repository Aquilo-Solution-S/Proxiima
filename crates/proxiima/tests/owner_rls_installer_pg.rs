//! `proxiima_core.install_owner_rls` on a spiice schema, the example
//! spiice's: every classification that does not name one owner per table is
//! refused.

#[path = "fixtures/example_engine.rs"]
#[allow(dead_code, reason = "each suite uses part of the shared fixture")]
mod example_engine;

use example_engine::ExampleEngine;
use proxiima::{FactPayload, FactWrite};
use proxiima_core::{
    AuthPath, AuthzContext, OwnerId, SqlText, StorageError, StorageRule, UserId,
    test_fixtures::{authenticated_context, owner_scope_for_tests},
};
use proxiima_example::StampV1;
use sqlx::{Connection, PgConnection, SqlSafeStr};
use uuid::Uuid;

/// The example schema's classification as its migrations list it (baseline,
/// `stamp_follows`, then `stamp_follows_memory_rls`). The host's Follows marks
/// supersede the listed class for `stamp_v1`.
const OWNER_ID_TABLES: &[&str] = &["notebooks"];
const FK_PARENT_TABLES: &[&str] = &["note_v1", "digest_v1"];
const OWNERLESS_TABLES: &[&str] = &["settings"];
/// The `owner: follows` stamp, listed by the example as `memory_owner`. That
/// listing is superseded: the host installs the follows rule (own `owner_id` AND
/// EXISTS over its memory) whatever class the spiice lists.
const MEMORY_OWNER_TABLES: &[&str] = &["stamp_v1"];

const POLICIES: &str = "SELECT c.relname::text, p.polname::text, p.polcmd::text,
        p.polpermissive, p.polroles::text,
        COALESCE(pg_get_expr(p.polqual, p.polrelid), ''),
        COALESCE(pg_get_expr(p.polwithcheck, p.polrelid), ''),
        c.relrowsecurity, c.relforcerowsecurity
   FROM pg_policy AS p
   JOIN pg_class AS c ON c.oid = p.polrelid
   JOIN pg_namespace AS n ON n.oid = c.relnamespace
  WHERE n.nspname = 'proxiima_example'
  ORDER BY 1, 2";

type PolicyRow = (
    String,
    String,
    String,
    bool,
    String,
    String,
    String,
    bool,
    bool,
);

fn named_policy<'a>(policies: &'a [PolicyRow], table: &str, name: &str) -> &'a PolicyRow {
    policies
        .iter()
        .find(|policy| policy.0 == table && policy.1 == name)
        .unwrap_or_else(|| panic!("missing {name} policy on {table}"))
}

fn owned(tables: &[&str]) -> Vec<String> {
    tables.iter().map(|table| (*table).to_owned()).collect()
}

async fn install(
    conn: &mut PgConnection,
    owner_id: &[&str],
    fk_parent: &[&str],
    ownerless: &[&str],
    memory_owner: &[&str],
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT proxiima_core.install_owner_rls('proxiima_example', $1, $2, $3, $4)")
        .bind(owned(owner_id))
        .bind(owned(fk_parent))
        .bind(owned(ownerless))
        .bind(owned(memory_owner))
        .execute(&mut *conn)
        .await
        .map(|_| ())
}

async fn refusal(
    conn: &mut PgConnection,
    owner_id: &[&str],
    fk_parent: &[&str],
    ownerless: &[&str],
    memory_owner: &[&str],
) -> String {
    let mut tx = conn.begin().await.expect("refusal transaction");
    let error = install(&mut tx, owner_id, fk_parent, ownerless, memory_owner)
        .await
        .expect_err("the installer must refuse this classification");
    tx.rollback().await.expect("rollback");
    error.to_string()
}

/// Every classification the installer must refuse, each in a rolled-back
/// transaction.
async fn assert_refusals(conn: &mut PgConnection) {
    let message = refusal(
        conn,
        &[],
        FK_PARENT_TABLES,
        OWNERLESS_TABLES,
        MEMORY_OWNER_TABLES,
    )
    .await;
    assert!(
        message.contains("owner RLS classification missing for proxiima_example.notebooks"),
        "an unclassified table refuses: {message}"
    );

    let mut with_stale = FK_PARENT_TABLES.to_vec();
    with_stale.push("dropped_long_ago_v1");
    let message = refusal(
        conn,
        OWNER_ID_TABLES,
        &with_stale,
        OWNERLESS_TABLES,
        MEMORY_OWNER_TABLES,
    )
    .await;
    assert!(
        message.contains("classified table proxiima_example.dropped_long_ago_v1 does not exist"),
        "a stale name refuses: {message}"
    );

    let mut twice = FK_PARENT_TABLES.to_vec();
    twice.push("notebooks");
    let message = refusal(
        conn,
        OWNER_ID_TABLES,
        &twice,
        OWNERLESS_TABLES,
        MEMORY_OWNER_TABLES,
    )
    .await;
    assert!(
        message.contains("proxiima_example.notebooks is classified 2 times"),
        "a double classification refuses: {message}"
    );

    let mut owner_as_fk = FK_PARENT_TABLES.to_vec();
    owner_as_fk.push("notebooks");
    let message = refusal(
        conn,
        &[],
        &owner_as_fk,
        OWNERLESS_TABLES,
        MEMORY_OWNER_TABLES,
    )
    .await;
    assert!(
        message.contains(
            "proxiima_example.notebooks has an owner_id column but is not in owner_id_tables"
        ),
        "an owner_id table cannot take a parent's policy: {message}"
    );

    let message = refusal(
        conn,
        OWNER_ID_TABLES,
        FK_PARENT_TABLES,
        &[],
        &["settings", "stamp_v1"],
    )
    .await;
    assert!(
        message.contains("settings is in memory_owner_tables but has no uuid t column"),
        "a memory-owner table must be keyed by t: {message}"
    );

    let error =
        sqlx::query("SELECT proxiima_core.install_owner_rls('proxiima_core', '{}', '{}', '{}')")
            .execute(&mut *conn)
            .await
            .expect_err("core's own schema is not a spiice schema");
    assert!(
        error
            .to_string()
            .contains("proxiima_core is not a spiice schema")
    );
}

/// Classifications that do not name one owner per table, on scratch tables
/// in a throwaway schema.
/// (tables, `fk_parent_tables`, refusal or `""`, accepted `(table, read-policy fragment)`).
type ParentCase = (
    &'static str,
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

async fn assert_parent_refusals(conn: &mut PgConnection) {
    let cases: [ParentCase; 5] = [
        (
            "CREATE TABLE probe.self_ref (t uuid PRIMARY KEY, parent_t uuid REFERENCES probe.self_ref (t))",
            "ARRAY['self_ref']",
            "no single-column FK",
            &[],
        ),
        (
            "CREATE TABLE probe.a (t uuid PRIMARY KEY);
             CREATE TABLE probe.b (t uuid PRIMARY KEY REFERENCES probe.a (t));
             ALTER TABLE probe.a ADD FOREIGN KEY (t) REFERENCES probe.b (t)",
            "ARRAY['a', 'b']",
            "reaches itself through its parent FKs",
            &[],
        ),
        (
            "CREATE TABLE probe.amb (id bigint PRIMARY KEY,
                 cited_t uuid REFERENCES proxiima_core.memory (t),
                 about_t uuid REFERENCES proxiima_core.memory (t))",
            "ARRAY['amb']",
            "has 2 candidate parent FKs and none on its leading primary-key column",
            &[],
        ),
        (
            "CREATE TABLE probe.keyed (t uuid PRIMARY KEY REFERENCES proxiima_core.memory (t),
                 cited_t uuid REFERENCES proxiima_core.memory (t));
             CREATE TABLE probe.only_one (id bigint PRIMARY KEY, x uuid REFERENCES proxiima_core.memory (t))",
            "ARRAY['keyed', 'only_one']",
            "",
            &[("probe.keyed", "parent.t = keyed.t"), ("probe.only_one", "parent.t = only_one.x")],
        ),
        // An FK to a partitioned table carries one clone per partition; a
        // partitioned child and its partition each still have one parent.
        (
            "CREATE TABLE probe.u (t uuid PRIMARY KEY REFERENCES proxiima_core.memory (t)) PARTITION BY HASH (t);
             CREATE TABLE probe.u0 PARTITION OF probe.u FOR VALUES WITH (MODULUS 2, REMAINDER 0);
             CREATE TABLE probe.u1 PARTITION OF probe.u FOR VALUES WITH (MODULUS 2, REMAINDER 1);
             CREATE TABLE probe.c (id bigint PRIMARY KEY, ut uuid REFERENCES probe.u (t)) PARTITION BY HASH (id);
             CREATE TABLE probe.c0 PARTITION OF probe.c FOR VALUES WITH (MODULUS 1, REMAINDER 0)",
            "ARRAY['u', 'u0', 'u1', 'c', 'c0']",
            "",
            &[("probe.c", "parent.t = c.ut"), ("probe.c0", "parent.t = c0.ut")],
        ),
    ];
    for (tables, fk_parent, refusal, accepted) in cases {
        let mut tx = conn.begin().await.expect("probe transaction");
        let mut create = SqlText::new("CREATE SCHEMA probe; ");
        create.push(tables);
        sqlx::raw_sql(create)
            .execute(&mut *tx)
            .await
            .expect("probe tables");
        let mut install = SqlText::new("SELECT proxiima_core.install_owner_rls('probe', '{}', ");
        install.push(fk_parent).push(", '{}')");
        let result = sqlx::raw_sql(install).execute(&mut *tx).await;
        if refusal.is_empty() {
            result.expect("an FK on the key, or the only FK, names the owner");
            for (table, fragment) in accepted {
                let key: String = sqlx::query_scalar(
                    "SELECT pg_get_expr(polqual, polrelid) FROM pg_policy
                      WHERE polrelid = $1::regclass AND polname = 'proxiima_owner_read'",
                )
                .bind(table)
                .fetch_one(&mut *tx)
                .await
                .expect("read policy");
                assert!(key.contains(fragment), "{table} keyed as {fragment}: {key}");
            }
        } else {
            let message = result.expect_err(refusal).to_string();
            assert!(message.contains(refusal), "{refusal}: {message}");
        }
        tx.rollback().await.expect("rollback");
    }
    let error = sqlx::query("SELECT proxiima_core.install_owner_rls('public', '{}', '{}', '{}')")
        .execute(&mut *conn)
        .await
        .expect_err("public is not a spiice schema");
    assert!(error.to_string().contains("public is not a spiice schema"));
}

/// Memory-owner tables name their memory by the FK on the leading
/// primary-key column, else their only memory FK with no competing `t`, else
/// `t`; any other shape is ambiguous and refused.
/// (tables, `memory_owner_tables`, refusal or `""`, accepted `(table, fragment)`).
async fn assert_memory_owner_refusals(conn: &mut PgConnection) {
    let cases: [ParentCase; 3] = [
        (
            "CREATE TABLE probe.two_refs (id bigint PRIMARY KEY,
                 cited_t uuid REFERENCES proxiima_core.memory (t),
                 about_t uuid REFERENCES proxiima_core.memory (t))",
            "ARRAY['two_refs']",
            "its memory is ambiguous",
            &[],
        ),
        (
            "CREATE TABLE probe.t_and_ref (t uuid NOT NULL,
                 subject_t uuid REFERENCES proxiima_core.memory (t))",
            "ARRAY['t_and_ref']",
            "its memory is ambiguous",
            &[],
        ),
        (
            "CREATE TABLE probe.keyed (memory_id uuid PRIMARY KEY REFERENCES proxiima_core.memory (t),
                 cited_t uuid REFERENCES proxiima_core.memory (t));
             CREATE TABLE probe.only_ref (id bigint PRIMARY KEY, x uuid REFERENCES proxiima_core.memory (t));
             CREATE TABLE probe.plain (t uuid NOT NULL)",
            "ARRAY['keyed', 'only_ref', 'plain']",
            "",
            &[
                ("probe.keyed", "parent.t = keyed.memory_id"),
                ("probe.only_ref", "parent.t = only_ref.x"),
                ("probe.plain", "parent.t = plain.t"),
            ],
        ),
    ];
    for (tables, memory_owner, refusal, accepted) in cases {
        let mut tx = conn.begin().await.expect("probe transaction");
        let mut create = SqlText::new("CREATE SCHEMA probe; ");
        create.push(tables);
        sqlx::raw_sql(create)
            .execute(&mut *tx)
            .await
            .expect("probe tables");
        let mut install =
            SqlText::new("SELECT proxiima_core.install_owner_rls('probe', '{}', '{}', '{}', ");
        install.push(memory_owner).push(")");
        let result = sqlx::raw_sql(install).execute(&mut *tx).await;
        if refusal.is_empty() {
            result.expect("one memory names each row");
            for (table, fragment) in accepted {
                let key: String = sqlx::query_scalar(
                    "SELECT pg_get_expr(polqual, polrelid) FROM pg_policy
                      WHERE polrelid = $1::regclass AND polname = 'proxiima_owner_read'",
                )
                .bind(table)
                .fetch_one(&mut *tx)
                .await
                .expect("read policy");
                assert!(key.contains(fragment), "{table} keyed as {fragment}: {key}");
            }
        } else {
            let message = result.expect_err(refusal).to_string();
            assert!(message.contains(refusal), "{refusal}: {message}");
        }
        tx.rollback().await.expect("rollback");
    }
}

#[tokio::test]
async fn installer_refuses_invalid_classifications_and_preserves_live_policies() {
    let db = example_engine::migrated_db("proxiima_owner_rls_installer").await;
    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");

    let mut tx = conn.begin().await.expect("install transaction");
    install(
        &mut tx,
        OWNER_ID_TABLES,
        FK_PARENT_TABLES,
        OWNERLESS_TABLES,
        MEMORY_OWNER_TABLES,
    )
    .await
    .expect("the example classification installs");
    let expected: Vec<PolicyRow> = sqlx::query_as(POLICIES)
        .fetch_all(&mut *tx)
        .await
        .expect("installed policies");
    tx.rollback().await.expect("rollback");

    let live: Vec<PolicyRow> = sqlx::query_as(POLICIES)
        .fetch_all(&mut conn)
        .await
        .expect("live policies");
    assert_eq!(
        live.len(),
        3 * (OWNER_ID_TABLES.len()
            + FK_PARENT_TABLES.len()
            + OWNERLESS_TABLES.len()
            + MEMORY_OWNER_TABLES.len()),
        "every example table carries the three owner-RLS policies"
    );
    assert_follows_rule(&live);
    assert_eq!(live, expected, "live policies are the installer result");

    for table in FK_PARENT_TABLES {
        let read = named_policy(&live, table, "proxiima_owner_read");
        let write = named_policy(&live, table, "proxiima_owner_write");
        for policy in [read, write] {
            assert!(
                policy.5.contains(&format!("parent.t = {table}.t")),
                "{}: {}",
                policy.1,
                policy.5
            );
            assert!(!policy.5.contains("notebook_id"), "{}", policy.5);
            assert!(!policy.6.contains("notebook_id"), "{}", policy.6);
        }
    }

    for table in MEMORY_OWNER_TABLES {
        let read = named_policy(&live, table, "proxiima_owner_read");
        let write = named_policy(&live, table, "proxiima_owner_write");
        for policy in [read, write] {
            assert!(
                policy.5.contains("FROM proxiima_core.memory parent")
                    && policy.5.contains(&format!("parent.t = {table}.t")),
                "{} reads through its memory: {}",
                policy.1,
                policy.5
            );
        }
    }

    assert_refusals(&mut conn).await;
    assert_parent_refusals(&mut conn).await;
    assert_memory_owner_refusals(&mut conn).await;
    conn.close().await.expect("close");
}

/// The host's Follows table of the example spiice.
const FOLLOWS_TABLE: &str = "proxiima_example.stamp_v1";

const SYNC_SQL: &str = "SELECT proxiima_core.sync_owner_follows($1, $2)";

/// Leading-`owner_id` btree indexes on `stamp_v1`.
const OWNER_INDEXES: &str = "SELECT count(*) FROM pg_index AS i
     JOIN pg_class AS ic ON ic.oid = i.indexrelid
     JOIN pg_am AS am ON am.oid = ic.relam
     JOIN pg_attribute AS a ON a.attrelid = i.indrelid AND a.attnum = i.indkey[0]
    WHERE i.indrelid = 'proxiima_example.stamp_v1'::regclass
      AND am.amname = 'btree' AND i.indisvalid AND a.attname = 'owner_id'";

async fn platform_scope(conn: &mut PgConnection) {
    sqlx::query("SELECT set_config('app.proxiima_scope', 'platform', true)")
        .execute(&mut *conn)
        .await
        .expect("platform scope");
}

async fn sync(
    conn: &mut PgConnection,
    schemas: &[&str],
    tables: &[&str],
) -> Result<(), sqlx::Error> {
    sqlx::query(SYNC_SQL)
        .bind(owned(schemas))
        .bind(owned(tables))
        .execute(&mut *conn)
        .await
        .map(|_| ())
}

async fn policies(conn: &mut PgConnection) -> Vec<PolicyRow> {
    sqlx::query_as(POLICIES)
        .fetch_all(&mut *conn)
        .await
        .expect("example policies")
}

/// Both owner policies of `stamp_v1` are the host follows rule: its own
/// `owner_id` check AND an EXISTS over its memory. The memory half carries
/// `parent.owner_id = ANY` too, so the own-column half is looked for with the
/// parent's occurrences removed: a plain `memory_owner` policy fails here.
#[track_caller]
fn assert_follows_rule(policies: &[PolicyRow]) {
    let read = named_policy(policies, "stamp_v1", "proxiima_owner_read");
    let write = named_policy(policies, "stamp_v1", "proxiima_owner_write");
    for expr in [&read.5, &write.5, &write.6] {
        assert!(
            expr.replace("parent.owner_id", "")
                .contains("owner_id = ANY")
                && expr.contains("FROM proxiima_core.memory parent")
                && expr.contains("parent.t = stamp_v1.t"),
            "stamp_v1 carries the follows rule: {expr}"
        );
    }
}

#[tokio::test]
async fn sync_gives_a_follows_table_the_combined_rule_and_an_owner_index() {
    let db = example_engine::migrated_db("proxiima_owner_follows_sync").await;
    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    let mut tx = conn.begin().await.expect("sync transaction");
    platform_scope(&mut tx).await;
    // The shared template now installs Follows state automatically. Recreate
    // the pre-sync state here so this SQL test still proves installation.
    sqlx::query("DELETE FROM proxiima_core.owner_follows WHERE table_name = $1")
        .bind(FOLLOWS_TABLE)
        .execute(&mut *tx)
        .await
        .expect("remove the template's mark");
    sqlx::query("DROP INDEX proxiima_example.stamp_v1_owner_follows_idx")
        .execute(&mut *tx)
        .await
        .expect("remove the template's generated owner index");
    install(
        &mut tx,
        OWNER_ID_TABLES,
        FK_PARENT_TABLES,
        OWNERLESS_TABLES,
        MEMORY_OWNER_TABLES,
    )
    .await
    .expect("install the spiice's pre-sync memory-owner policy");
    // Raw INSERT on purpose: another binary's mark, which this host's sync must leave alone.
    sqlx::query(
        "INSERT INTO proxiima_core.owner_follows (table_name) VALUES ('proxiima_other.thing_v1')",
    )
    .execute(&mut *tx)
    .await
    .expect("another binary's mark");
    let before: i64 = sqlx::query_scalar(OWNER_INDEXES)
        .fetch_one(&mut *tx)
        .await
        .expect("indexes");
    assert_eq!(
        before, 0,
        "the example declares no owner_id index on stamp_v1"
    );

    sync(&mut tx, &["proxiima_example"], &[FOLLOWS_TABLE])
        .await
        .expect("the host syncs its Follows table");
    let first = policies(&mut tx).await;
    assert_follows_rule(&first);
    let indexes: i64 = sqlx::query_scalar(OWNER_INDEXES)
        .fetch_one(&mut *tx)
        .await
        .expect("indexes");
    assert_eq!(indexes, 1, "the sync adds one owner_id index");

    sync(&mut tx, &["proxiima_example"], &[FOLLOWS_TABLE])
        .await
        .expect("the sync is idempotent");
    assert_eq!(
        policies(&mut tx).await,
        first,
        "a second sync changes no policy"
    );
    let indexes: i64 = sqlx::query_scalar(OWNER_INDEXES)
        .fetch_one(&mut *tx)
        .await
        .expect("indexes");
    assert_eq!(indexes, 1, "a second sync adds no index");

    let marks: Vec<String> =
        sqlx::query_scalar("SELECT table_name FROM proxiima_core.owner_follows ORDER BY 1")
            .fetch_all(&mut *tx)
            .await
            .expect("marks");
    assert_eq!(
        marks,
        ["proxiima_example.stamp_v1", "proxiima_other.thing_v1"],
        "the sync leaves marks of schemas it does not compose"
    );

    sync(&mut tx, &["proxiima_example"], &[])
        .await
        .expect("a registry without Follows tables unmarks the schema");
    let marks: Vec<String> =
        sqlx::query_scalar("SELECT table_name FROM proxiima_core.owner_follows ORDER BY 1")
            .fetch_all(&mut *tx)
            .await
            .expect("marks");
    assert_eq!(marks, ["proxiima_other.thing_v1"]);
    tx.rollback().await.expect("rollback");
    conn.close().await.expect("close");
}

#[tokio::test]
async fn installer_keeps_the_follows_rule_whatever_the_spiice_lists() {
    let db = example_engine::migrated_db("proxiima_owner_follows_installer").await;
    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    let mut tx = conn.begin().await.expect("install transaction");
    platform_scope(&mut tx).await;
    sync(&mut tx, &["proxiima_example"], &[FOLLOWS_TABLE])
        .await
        .expect("the host syncs its Follows table");
    let synced = policies(&mut tx).await;
    assert_follows_rule(&synced);

    let mut as_owner_id = OWNER_ID_TABLES.to_vec();
    as_owner_id.push("stamp_v1");
    for (label, owner_id, memory_owner) in [
        ("in owner_id_tables", as_owner_id.as_slice(), &[][..]),
        (
            "in memory_owner_tables",
            OWNER_ID_TABLES,
            MEMORY_OWNER_TABLES,
        ),
        ("left out", OWNER_ID_TABLES, &[][..]),
    ] {
        install(
            &mut tx,
            owner_id,
            FK_PARENT_TABLES,
            OWNERLESS_TABLES,
            memory_owner,
        )
        .await
        .unwrap_or_else(|error| panic!("stamp_v1 {label} installs: {error}"));
        assert_eq!(
            policies(&mut tx).await,
            synced,
            "stamp_v1 {label} keeps the follows rule"
        );
    }
    sqlx::query(
        "SELECT proxiima_core.install_owner_rls_table('proxiima_example', 'stamp_v1', 'owner_id')",
    )
    .execute(&mut *tx)
    .await
    .expect("the per-table installer accepts a Follows table");
    assert_eq!(
        policies(&mut tx).await,
        synced,
        "install_owner_rls_table keeps the follows rule"
    );

    for (label, fk_parent, ownerless) in [
        (
            "fk_parent_tables",
            ["note_v1", "digest_v1", "stamp_v1"].as_slice(),
            OWNERLESS_TABLES,
        ),
        (
            "ownerless_tables",
            FK_PARENT_TABLES,
            ["settings", "stamp_v1"].as_slice(),
        ),
    ] {
        let mut savepoint = tx.begin().await.expect("savepoint");
        let message = install(&mut savepoint, OWNER_ID_TABLES, fk_parent, ownerless, &[])
            .await
            .expect_err("a table with owner_id takes no parent's or ownerless policy")
            .to_string();
        savepoint.rollback().await.expect("rollback to savepoint");
        assert!(
            message.contains(
                "proxiima_example.stamp_v1 has an owner_id column but is not in owner_id_tables"
            ),
            "stamp_v1 in {label}: {message}"
        );
    }
    tx.rollback().await.expect("rollback");
    conn.close().await.expect("close");
}

/// How many stamp rows for memory `t` the `owner` scope sees, on the runtime
/// role under owner RLS.
async fn stamps_seen_by(runtime: &sqlx::PgPool, owner: OwnerId, t: Uuid) -> i64 {
    let mut tx =
        proxiima_storage_pg::begin_owner_transaction(runtime, &owner_scope_for_tests(&[owner]))
            .await
            .expect("owner transaction");
    let seen: i64 =
        sqlx::query_scalar("SELECT count(*) FROM proxiima_example.stamp_v1 WHERE t = $1")
            .bind(t)
            .fetch_one(&mut *tx)
            .await
            .expect("stamp count under owner RLS");
    tx.rollback().await.expect("rollback");
    seen
}

/// The r3 leak (F1): a spiice migration that classifies its Follows table as
/// `owner_id` after the host marked it must not let a forgotten memory's row
/// through an owner read.
#[tokio::test]
async fn forgotten_follows_row_hidden_after_spiice_installs_owner_id_class() {
    let harness = ExampleEngine::new("proxiima_owner_follows_forgotten", |_| {}).await;
    let runtime = sqlx::PgPool::connect(harness.db.runtime_url())
        .await
        .expect("runtime connection");
    let mut conn = PgConnection::connect(harness.db.platform_url())
        .await
        .expect("platform connection");
    let mut tx = conn.begin().await.expect("install transaction");
    platform_scope(&mut tx).await;
    sync(&mut tx, &["proxiima_example"], &[FOLLOWS_TABLE])
        .await
        .expect("the host syncs its Follows table");
    let mut as_owner_id = OWNER_ID_TABLES.to_vec();
    as_owner_id.push("stamp_v1");
    install(
        &mut tx,
        &as_owner_id,
        FK_PARENT_TABLES,
        OWNERLESS_TABLES,
        &[],
    )
    .await
    .expect("the spiice classifies stamp_v1 as owner_id");
    tx.commit().await.expect("commit");
    conn.close().await.expect("close");

    let user = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(user);
    let authz = authenticated_context(AuthzContext::for_subject(user, AuthPath::HostBearer));
    let stamp = StampV1 {
        mark: "follows-forgotten".to_owned(),
    };
    let memory_id = harness
        .engine
        .ingest_fact(&authz, FactWrite::new(owner, StampV1::SCHEMA_ID, &stamp))
        .await
        .expect("stamp is remembered")
        .memory_id;
    let t = memory_id.into_inner();
    assert_eq!(
        stamps_seen_by(&runtime, owner, t).await,
        1,
        "the owner sees its live stamp"
    );
    harness
        .engine
        .forget_memory(&authz, owner, memory_id)
        .await
        .expect("owner forgets the memory");
    assert_eq!(
        stamps_seen_by(&runtime, owner, t).await,
        0,
        "the forgotten memory's Follows row is hidden from its owner"
    );
}

#[tokio::test]
async fn refuses_sync_outside_composed_schemas() {
    let db = example_engine::migrated_db("proxiima_owner_follows_refusals").await;
    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    for (schemas, tables, refusal) in [
        (
            &["proxiima_example"][..],
            &["proxiima_core.memory"][..],
            "outside the composed schemas",
        ),
        (
            &["proxiima_core"][..],
            &["proxiima_core.memory"][..],
            "proxiima_core is not a spiice schema",
        ),
        (
            &["proxiima_example"][..],
            &["proxiima_other.thing_v1"][..],
            "outside the composed schemas",
        ),
        (
            &["proxiima_example"][..],
            &["stamp_v1"][..],
            "is not a lowercase schema.table name",
        ),
    ] {
        let mut tx = conn.begin().await.expect("refusal transaction");
        platform_scope(&mut tx).await;
        let message = sync(&mut tx, schemas, tables)
            .await
            .expect_err(refusal)
            .to_string();
        tx.rollback().await.expect("rollback");
        assert!(message.contains(refusal), "{refusal}: {message}");
    }
    let mut tx = conn.begin().await.expect("unscoped transaction");
    let refused = example_engine::frozen_sidecars()
        .sync_owner_follows(&mut tx)
        .await;
    assert!(
        matches!(
            refused,
            Err(StorageError::Refused {
                rule: StorageRule::PlatformScopeRequired,
                ..
            })
        ),
        "{refused:?}"
    );
    tx.rollback().await.expect("rollback");
    conn.close().await.expect("close");
}

#[path = "fixtures/split_core_db.rs"]
mod split_core_db;

#[tokio::test]
async fn automatic_follows_installation_precedes_spiice_lanes_and_serves_skip_boot() {
    use std::borrow::Cow;

    use proxiima::spiice::SpiiceBundle;
    use proxiima::{NamedMigrator, PgMigrationStorage, PgPoolConfig, PgTuning, Proxiima};
    use proxiima_example::ExampleSpiice;
    use sqlx::migrate::{Migration, MigrationType, Migrator};

    let db = split_core_db::clone_split_core_db("proxiima_follows_order")
        .await
        .expect("owned test database");
    let pg = PgMigrationStorage::connect_with_config(
        db.platform_url(),
        PgPoolConfig::default(),
        PgTuning::default(),
    )
    .await
    .expect("migration storage");
    let sidecars = example_engine::frozen_sidecars();
    let probe = Migration::new(
        474,
        Cow::Borrowed("Follows registration precedes table creation"),
        MigrationType::Simple,
        "DO $$ BEGIN
             IF NOT EXISTS (SELECT 1 FROM proxiima_core.owner_follows
                            WHERE table_name = 'proxiima_example.stamp_v1') THEN
                 RAISE EXCEPTION 'Follows registration must precede spiice migrations';
             END IF;
         END $$"
            .into_sql_str(),
        false,
    );
    let mut sources = vec![NamedMigrator::spiice(
        "follows-order",
        Migrator {
            migrations: Cow::Owned(vec![probe]),
            ..Migrator::DEFAULT
        },
    )];
    sources.extend(ExampleSpiice::migrators());
    proxiima::run_core_and_spiice_migrations(&pg, sources, &sidecars)
        .await
        .expect("runner registers Follows before spiice lanes and installs it afterwards");
    let pool = pg.clone_pool_for_backend();
    let mut connection = pool.acquire().await.expect("connection");
    let mut transaction = proxiima::begin_migration_transaction(&mut connection)
        .await
        .expect("platform transaction");
    sidecars
        .ensure_owner_rls(&mut transaction)
        .await
        .expect("post-lane owner index and marks exist");
    transaction.rollback().await.expect("rollback");
    drop(connection);
    pg.clone_pool_for_backend().close().await;

    let owner = proxiima::OwnerId::from(proxiima::UserId::new(uuid::Uuid::now_v7()));
    let built = Proxiima::<ExampleSpiice>::app()
        .configure(|b| {
            b.database_url(db.runtime_url())
                .platform_database_url(db.platform_url())
                .store_number(0)
                .owner(owner)
                .allow_insecure_single_owner()
                .served_surface(proxiima::ServedSurface::All)
        })
        .build()
        .await
        .expect("skip boot validates generated Follows state without installing it");
    built.shutdown().await;
}

#[tokio::test]
async fn preflight_refuses_missing_follows_state_and_empty_sql_run_repairs_it() {
    use proxiima::spiice::SpiiceBundle;
    use proxiima::{MigrationError, PgMigrationStorage, PgPoolConfig, PgTuning};
    use proxiima_example::ExampleSpiice;

    let db = example_engine::migrated_db("proxiima_follows_preflight").await;
    let pg = PgMigrationStorage::connect_with_config(
        db.platform_url(),
        PgPoolConfig::default(),
        PgTuning::default(),
    )
    .await
    .expect("migration storage");
    let sidecars = example_engine::frozen_sidecars();
    for mutation in [
        sqlx::query(
            "DELETE FROM proxiima_core.owner_follows WHERE table_name = 'proxiima_example.stamp_v1'",
        ),
        // Raw INSERT on purpose: drifts the deployed Follows state, which the boot must refuse.
        sqlx::query("INSERT INTO proxiima_core.owner_follows VALUES ('proxiima_example.extra_v1')"),
        sqlx::query("DROP INDEX proxiima_example.stamp_v1_owner_follows_idx"),
    ] {
        let pool = pg.clone_pool_for_backend();
        let mut connection = pool.acquire().await.expect("connection");
        let mut transaction = proxiima::begin_migration_transaction(&mut connection)
            .await
            .expect("platform transaction");
        mutation
            .execute(&mut *transaction)
            .await
            .expect("change deployed Follows state");
        transaction.commit().await.expect("commit");
        drop(connection);
        let refused =
            proxiima::preflight_without_migrations(&pg, ExampleSpiice::migrators(), &sidecars)
                .await;
        assert!(
            matches!(
                refused,
                Err(MigrationError::SidecarOwnerRls(
                    proxiima_core::StorageError::MigrationRequired { .. }
                ))
            ),
            "preflight refuses mismatched deployment: {refused:?}"
        );
        let serving = proxiima::Proxiima::<ExampleSpiice>::app()
            .configure(|b| {
                b.database_url(db.runtime_url())
                    .store_number(0)
                    .platform_database_url(db.platform_url())
                    .served_surface(proxiima::ServedSurface::All)
            })
            .connect_storage()
            .await;
        assert!(
            matches!(
                &serving,
                Err(proxiima::ProxiimaError::MigrationRequired { details })
                    if details.contains("owner_follows")
            ),
            "serving validation names the migrate step: {:?}",
            serving.as_ref().err()
        );
        let pending = proxiima::pending_migrations(
            &pg.clone_pool_for_backend(),
            &ExampleSpiice::migrators(),
            &proxiima::spiice::PgSidecarRegistryFrozen::default(),
            proxiima::VectorStorage::Local,
        )
        .await
        .expect("SQL migration plan");
        assert!(
            pending.is_empty(),
            "registry state is separate from the SQL ledger"
        );
        proxiima::run_core_and_spiice_migrations(&pg, ExampleSpiice::migrators(), &sidecars)
            .await
            .expect("an empty SQL run restores generated state");
        proxiima::preflight_without_migrations(&pg, ExampleSpiice::migrators(), &sidecars)
            .await
            .expect("read-only preflight sees platform-owned Follows marks");
    }
    pg.clone_pool_for_backend().close().await;
}

/// A registered Follows table the database does not have is not something
/// the migrate step re-creates: the preflight refuses it untyped, and serving
/// does not send the operator to migrate.
#[tokio::test]
async fn preflight_refuses_an_absent_follows_table_without_asking_for_migrate() {
    use proxiima::spiice::SpiiceBundle;
    use proxiima::{MigrationError, PgMigrationStorage, PgPoolConfig, PgTuning};
    use proxiima_example::ExampleSpiice;

    let db = example_engine::migrated_db("proxiima_follows_absent").await;
    let pg = PgMigrationStorage::connect_with_config(
        db.platform_url(),
        PgPoolConfig::default(),
        PgTuning::default(),
    )
    .await
    .expect("migration storage");
    let pool = pg.clone_pool_for_backend();
    let mut connection = pool.acquire().await.expect("connection");
    let mut transaction = proxiima::begin_migration_transaction(&mut connection)
        .await
        .expect("platform transaction");
    sqlx::query("DROP TABLE proxiima_example.stamp_v1 CASCADE")
        .execute(&mut *transaction)
        .await
        .expect("drop a registered Follows table");
    transaction.commit().await.expect("commit");
    drop(connection);

    let refused = proxiima::preflight_without_migrations(
        &pg,
        ExampleSpiice::migrators(),
        &example_engine::frozen_sidecars(),
    )
    .await;
    assert!(
        matches!(
            &refused,
            Err(MigrationError::SidecarOwnerRls(
                proxiima_core::StorageError::Internal(message)
            )) if message.contains("do not exist")
        ),
        "{refused:?}"
    );
    let serving = proxiima::Proxiima::<ExampleSpiice>::app()
        .configure(|b| {
            b.database_url(db.runtime_url())
                .store_number(0)
                .platform_database_url(db.platform_url())
                .served_surface(proxiima::ServedSurface::All)
        })
        .connect_storage()
        .await;
    assert!(
        matches!(&serving, Err(proxiima::ProxiimaError::Storage(_))),
        "{:?}",
        serving.as_ref().err()
    );
    pool.close().await;
}

/// Upgrade (the r3 leak): a database migrated before the host marked
/// Follows tables carries `stamp_v1` under the owner-only `owner_id` policy and
/// no mark. Preflight refuses it, and the next run with no pending SQL
/// installs the follows rule.
#[tokio::test]
async fn empty_sql_run_replaces_an_owner_only_follows_policy() {
    use proxiima::spiice::SpiiceBundle;
    use proxiima::{MigrationError, PgMigrationStorage, PgPoolConfig, PgTuning};
    use proxiima_example::ExampleSpiice;

    let db = example_engine::migrated_db("proxiima_follows_owner_only_upgrade").await;
    let pg = PgMigrationStorage::connect_with_config(
        db.platform_url(),
        PgPoolConfig::default(),
        PgTuning::default(),
    )
    .await
    .expect("migration storage");
    let sidecars = example_engine::frozen_sidecars();

    // Recreate the pre-200fe438 state: no mark, spiice-installed owner_id class.
    let pool = pg.clone_pool_for_backend();
    let mut connection = pool.acquire().await.expect("connection");
    let mut transaction = proxiima::begin_migration_transaction(&mut connection)
        .await
        .expect("platform transaction");
    sqlx::query(
        "DELETE FROM proxiima_core.owner_follows WHERE table_name = 'proxiima_example.stamp_v1'",
    )
    .execute(&mut *transaction)
    .await
    .expect("unmark stamp_v1");
    sqlx::query(
        "SELECT proxiima_core.install_owner_rls_table('proxiima_example', 'stamp_v1', 'owner_id')",
    )
    .execute(&mut *transaction)
    .await
    .expect("the spiice installs the owner-only class");
    // The current installer also admits exact Goal publications. Reproduce
    // the historical owner-only read policy this upgrade is intended to repair.
    sqlx::query(
        "ALTER POLICY proxiima_owner_read ON proxiima_example.stamp_v1
         USING (owner_id = ANY(COALESCE(
             NULLIF(current_setting('app.owner', true), '')::uuid[], '{}'::uuid[])))",
    )
    .execute(&mut *transaction)
    .await
    .expect("restore the historical owner-only read predicate");
    transaction.commit().await.expect("commit");
    drop(connection);

    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    let before = policies(&mut conn).await;
    let read = named_policy(&before, "stamp_v1", "proxiima_owner_read");
    assert!(
        read.5.contains("owner_id = ANY") && !read.5.contains("proxiima_core.memory parent"),
        "precondition: stamp_v1 is owner-only: {}",
        read.5
    );

    let refused =
        proxiima::preflight_without_migrations(&pg, ExampleSpiice::migrators(), &sidecars).await;
    assert!(
        matches!(refused, Err(MigrationError::SidecarOwnerRls(_))),
        "preflight refuses the unmarked Follows table: {refused:?}"
    );
    assert!(
        proxiima::pending_migrations(
            &pg.clone_pool_for_backend(),
            &ExampleSpiice::migrators(),
            &proxiima::spiice::PgSidecarRegistryFrozen::default(),
            proxiima::VectorStorage::Local
        )
        .await
        .expect("plan")
        .is_empty(),
        "no SQL is pending"
    );
    proxiima::run_core_and_spiice_migrations(&pg, ExampleSpiice::migrators(), &sidecars)
        .await
        .expect("an empty SQL run repairs the Follows policy");

    assert_follows_rule(&policies(&mut conn).await);
    proxiima::preflight_without_migrations(&pg, ExampleSpiice::migrators(), &sidecars)
        .await
        .expect("preflight accepts the repaired deployment");
    conn.close().await.expect("close");
    pg.clone_pool_for_backend().close().await;
}

#[tokio::test]
async fn a_registered_follows_table_missing_after_migrations_refuses_the_run() {
    let db = split_core_db::clone_split_core_db("proxiima_follows_missing")
        .await
        .expect("owned test database");
    let pg = proxiima::PgMigrationStorage::connect_with_config(
        db.platform_url(),
        proxiima::PgPoolConfig::default(),
        proxiima::PgTuning::default(),
    )
    .await
    .expect("migration storage");
    let refused = proxiima::run_core_and_spiice_migrations(
        &pg,
        Vec::new(),
        &example_engine::frozen_sidecars(),
    )
    .await;
    assert!(
        matches!(refused, Err(proxiima::MigrationError::SidecarOwnerRls(_))),
        "registered sidecars must exist when all migration lanes finish: {refused:?}"
    );
    pg.clone_pool_for_backend().close().await;
}

const RECORDED: &str = "SELECT schema_name || '.' || table_name, class::text, owner_column
   FROM proxiima_core.owner_rls_table ORDER BY 1";

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .map(std::borrow::Cow::into_owned)
}

async fn migration_storage(db: &proxiima::testkit::SplitRoleDb) -> proxiima::PgMigrationStorage {
    proxiima::PgMigrationStorage::connect_with_config(
        db.platform_url(),
        proxiima::PgPoolConfig::default(),
        proxiima::PgTuning::default(),
    )
    .await
    .expect("migration storage")
}

/// A fresh install records every table the example installs, passes the
/// boot check, and leaves the Follows table to its mark.
#[tokio::test]
async fn a_fresh_example_install_records_its_tables_and_boots() {
    use proxiima::spiice::SpiiceBundle;
    use proxiima_example::ExampleSpiice;

    let db = example_engine::migrated_db("proxiima_owner_rls_fresh").await;
    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    let mut tx = conn.begin().await.expect("transaction");
    platform_scope(&mut tx).await;
    let recorded: Vec<(String, String, String)> = sqlx::query_as(RECORDED)
        .fetch_all(&mut *tx)
        .await
        .expect("records");
    tx.rollback().await.expect("rollback");
    let mut tables: Vec<&str> = recorded.iter().map(|row| row.0.as_str()).collect();
    tables.sort_unstable();
    assert_eq!(
        tables,
        [
            "proxiima_example.digest_v1",
            "proxiima_example.note_v1",
            "proxiima_example.notebooks",
            "proxiima_example.settings",
        ]
    );
    let pg = migration_storage(&db).await;
    proxiima::preflight_without_migrations(
        &pg,
        ExampleSpiice::migrators(),
        &example_engine::frozen_sidecars(),
    )
    .await
    .expect("a fresh install boots");
    pg.clone_pool_for_backend().close().await;
}

/// The installer records the class and the owner column of each call, and a
/// second call replaces the record.
#[tokio::test]
async fn the_installer_records_the_class_and_replaces_it_on_a_second_install() {
    let db = example_engine::migrated_db("proxiima_owner_rls_record").await;
    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    let mut tx = conn.begin().await.expect("transaction");
    sqlx::query(
        "CREATE TABLE proxiima_example.probe (
             t uuid PRIMARY KEY REFERENCES proxiima_core.memory (t), owner uuid)",
    )
    .execute(&mut *tx)
    .await
    .expect("probe table");
    platform_scope(&mut tx).await;
    for (class, column) in [("owner_id", "owner"), ("memory_owner", "owner_id")] {
        sqlx::query("SELECT proxiima_core.install_owner_rls_table('proxiima_example', 'probe', $1::proxiima_core.owner_rls_class, $2)")
            .bind(class)
            .bind(column)
            .execute(&mut *tx)
            .await
            .expect("install");
        let row: (String, String) = sqlx::query_as(
            "SELECT class::text, owner_column FROM proxiima_core.owner_rls_table
              WHERE schema_name = 'proxiima_example' AND table_name = 'probe'",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("record");
        assert_eq!(row, (class.to_owned(), column.to_owned()));
    }
    sqlx::query(r#"CREATE TABLE proxiima_example."Mixed_Notes" (owner_id uuid NOT NULL)"#)
        .execute(&mut *tx)
        .await
        .expect("mixed-case table");
    sqlx::query(r"SELECT proxiima_core.install_owner_rls_table('proxiima_example', 'Mixed_Notes', 'owner_id')")
        .execute(&mut *tx)
        .await
        .expect("a mixed-case table installs");
    sqlx::query("SELECT proxiima_core.reinstall_owner_rls()")
        .execute(&mut *tx)
        .await
        .expect("a mixed-case record reinstalls");
    let recorded: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM proxiima_core.owner_rls_table
          WHERE schema_name = 'proxiima_example' AND table_name = 'Mixed_Notes'",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("record");
    assert_eq!(
        recorded, 1,
        "recorded with its exact name, kept by reinstall"
    );
    tx.rollback().await.expect("rollback");
    conn.close().await.expect("close");
}

/// Only the platform scope reads or writes the record, and only it may
/// reinstall.
#[tokio::test]
async fn the_record_is_platform_scope_only() {
    let db = example_engine::migrated_db("proxiima_owner_rls_scope").await;
    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    let mut tx = conn.begin().await.expect("transaction");
    sqlx::query("SELECT set_config('app.proxiima_scope', 'owner', true)")
        .execute(&mut *tx)
        .await
        .expect("owner scope");
    sqlx::query("SELECT set_config('app.owner', $1, true)")
        .bind(format!("{{{}}}", Uuid::now_v7()))
        .execute(&mut *tx)
        .await
        .expect("owner");
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM proxiima_core.owner_rls_table")
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    assert_eq!(visible, 0, "an owner scope reads no record");
    let mut savepoint = tx.begin().await.expect("savepoint");
    let refused = sqlx::query(
        "INSERT INTO proxiima_core.owner_rls_table VALUES
         ('proxiima_example', 'settings', 'owner_id', 'owner_id')
         ON CONFLICT (schema_name, table_name) DO UPDATE SET class = 'owner_id'",
    )
    .execute(&mut *savepoint)
    .await
    .expect_err("an owner scope writes no record");
    assert_eq!(sqlstate(&refused), Some("42501".to_owned()));
    savepoint.rollback().await.expect("rollback to savepoint");
    sqlx::query("SELECT set_config('app.proxiima_scope', '', true)")
        .execute(&mut *tx)
        .await
        .expect("no scope");
    let reinstall = sqlx::query("SELECT proxiima_core.reinstall_owner_rls()")
        .execute(&mut *tx)
        .await
        .expect_err("reinstall outside platform scope");
    assert_eq!(sqlstate(&reinstall), Some("PX001".to_owned()));
    tx.rollback().await.expect("rollback");
    conn.close().await.expect("close");
}

/// A recorded table whose owner policies were replaced by an older generation
/// has the generated policies again after a plain migration run.
#[tokio::test]
async fn a_migration_run_reinstalls_the_policies_of_recorded_tables() {
    let db = example_engine::migrated_db("proxiima_owner_rls_reinstall").await;
    let pg = migration_storage(&db).await;
    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    let generated = policies(&mut conn).await;
    sqlx::raw_sql(
        "ALTER POLICY proxiima_owner_read ON proxiima_example.notebooks
         USING (owner_id = ANY(COALESCE(
             NULLIF(current_setting('app.owner', true), '')::uuid[], '{}'::uuid[])));
         ALTER POLICY proxiima_owner_write ON proxiima_example.notebooks
         USING (owner_id = ANY(COALESCE(
             NULLIF(current_setting('app.owner', true), '')::uuid[], '{}'::uuid[])));",
    )
    .execute(&mut conn)
    .await
    .expect("older generation predicates");
    assert_ne!(policies(&mut conn).await, generated, "precondition: stale");
    migrate_example(&pg).await;
    assert_eq!(policies(&mut conn).await, generated);
    conn.close().await.expect("close");
    pg.clone_pool_for_backend().close().await;
}

/// A recorded table dropped and recreated under its name with hand-written
/// policies has the generated policies again after a migration run.
#[tokio::test]
async fn a_recreated_recorded_table_gets_the_generated_policies_back() {
    let db = example_engine::migrated_db("proxiima_owner_rls_recreated").await;
    let pg = migration_storage(&db).await;
    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    let generated = policies(&mut conn).await;
    sqlx::raw_sql(
        "ALTER TABLE proxiima_example.settings RENAME TO settings_old;
         CREATE TABLE proxiima_example.settings (LIKE proxiima_example.settings_old);
         DROP TABLE proxiima_example.settings_old;
         ALTER TABLE proxiima_example.settings ENABLE ROW LEVEL SECURITY;
         ALTER TABLE proxiima_example.settings FORCE ROW LEVEL SECURITY;
         CREATE POLICY proxiima_owner_read ON proxiima_example.settings FOR SELECT USING (true);
         CREATE POLICY proxiima_owner_write ON proxiima_example.settings FOR ALL USING (true);
         CREATE POLICY proxiima_platform ON proxiima_example.settings FOR ALL TO CURRENT_USER
             USING (current_setting('app.proxiima_scope', true) = 'platform')
             WITH CHECK (current_setting('app.proxiima_scope', true) = 'platform');",
    )
    .execute(&mut conn)
    .await
    .expect("recreated table with hand-written policies");
    assert_ne!(
        policies(&mut conn).await,
        generated,
        "precondition: hand-written"
    );
    migrate_example(&pg).await;
    assert_eq!(policies(&mut conn).await, generated);
    conn.close().await.expect("close");
    pg.clone_pool_for_backend().close().await;
}

async fn migrate_example(pg: &proxiima::PgMigrationStorage) {
    use proxiima::spiice::SpiiceBundle;
    use proxiima_example::ExampleSpiice;

    proxiima::run_core_and_spiice_migrations(
        pg,
        ExampleSpiice::migrators(),
        &example_engine::frozen_sidecars(),
    )
    .await
    .expect("the migration run");
}

/// The refusal of owner policies other than the ones core installed, as
/// serving boot reports it: the tables it names.
async fn altered_policy_refusal(pg: &proxiima::PgMigrationStorage) -> Option<String> {
    use proxiima::MigrationError;
    use proxiima::spiice::SpiiceBundle;
    use proxiima_example::ExampleSpiice;

    match proxiima::preflight_without_migrations(
        pg,
        ExampleSpiice::migrators(),
        &example_engine::frozen_sidecars(),
    )
    .await
    {
        Ok(_) => None,
        Err(MigrationError::SidecarOwnerRls(StorageError::MigrationRequired { details })) => {
            Some(details)
        }
        Err(other) => panic!("expected the altered-policy refusal: {other:?}"),
    }
}

/// A recorded or Follows table whose owner policy was edited after core
/// created it is refused by name, and a migration run repairs it.
#[tokio::test]
async fn serving_refuses_an_owner_policy_core_did_not_install_until_a_run_repairs_it() {
    let db = example_engine::migrated_db("proxiima_owner_rls_edited").await;
    let pg = migration_storage(&db).await;
    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    assert_eq!(
        altered_policy_refusal(&pg).await,
        None,
        "a finished run serves"
    );
    let generated = policies(&mut conn).await;
    for (table, other) in [
        ("notebooks", "proxiima_example.settings"),
        ("stamp_v1", "proxiima_example.notebooks"),
    ] {
        sqlx::raw_sql(match table {
            "notebooks" => {
                "ALTER POLICY proxiima_owner_read ON proxiima_example.notebooks
                 USING (owner_id IS NOT NULL)"
            }
            _ => {
                "ALTER POLICY proxiima_owner_read ON proxiima_example.stamp_v1
                 USING (owner_id IS NOT NULL)"
            }
        })
        .execute(&mut conn)
        .await
        .expect("hand-written predicate");
        let details = altered_policy_refusal(&pg)
            .await
            .unwrap_or_else(|| panic!("{table} is refused"));
        assert!(
            details.contains(&format!("proxiima_example.{table}")),
            "{details}"
        );
        assert!(!details.contains(other), "{details}");
        migrate_example(&pg).await;
        assert_eq!(
            altered_policy_refusal(&pg).await,
            None,
            "{table} serves after a run"
        );
        assert_eq!(policies(&mut conn).await, generated);
    }
    conn.close().await.expect("close");
    pg.clone_pool_for_backend().close().await;
}

/// A run whose lanes committed and whose final transaction failed leaves
/// predicates core did not install; a composition that does not include the
/// failed source refuses them too, and a run that finishes serves again.
#[tokio::test]
async fn serving_refuses_the_policies_a_failed_run_left_in_any_composition() {
    use proxiima::spiice::SpiiceBundle;
    use proxiima::{MigrationError, NamedMigrator};
    use proxiima_example::ExampleSpiice;
    use std::borrow::Cow;

    let db = example_engine::migrated_db("proxiima_owner_rls_unfinished").await;
    let pg = migration_storage(&db).await;
    let sidecars = example_engine::frozen_sidecars();
    // A consumer migration that rewrites the predicates of a recorded table
    // and leaves it unable to take its class again: the lane commits, the
    // reinstall of the final transaction raises.
    let lineup = || {
        let mut sources = ExampleSpiice::migrators();
        sources.push(NamedMigrator::spiice(
            "owner-rls-drift",
            sqlx::migrate::Migrator {
                migrations: Cow::Owned(vec![sqlx::migrate::Migration::new(
                    20_261_201_000_000,
                    Cow::Borrowed("rename the owner column"),
                    sqlx::migrate::MigrationType::Simple,
                    "ALTER TABLE proxiima_example.notebooks RENAME COLUMN owner_id TO owner_x"
                        .into_sql_str(),
                    false,
                )]),
                ..sqlx::migrate::Migrator::DEFAULT
            },
        ));
        sources
    };
    let failed = proxiima::run_core_and_spiice_migrations(&pg, lineup(), &sidecars).await;
    assert!(
        matches!(&failed, Err(MigrationError::SidecarOwnerRls(_))),
        "the final transaction fails: {failed:?}"
    );
    let details = altered_policy_refusal(&pg)
        .await
        .expect("the composition without the failed source refuses");
    assert!(details.contains("proxiima_example.notebooks"), "{details}");

    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    sqlx::query("ALTER TABLE proxiima_example.notebooks RENAME COLUMN owner_x TO owner_id")
        .execute(&mut conn)
        .await
        .expect("repair the cause");
    proxiima::run_core_and_spiice_migrations(&pg, lineup(), &sidecars)
        .await
        .expect("a run that finishes");
    assert_eq!(altered_policy_refusal(&pg).await, None);
    conn.close().await.expect("close");
    pg.clone_pool_for_backend().close().await;
}

/// Owner policies no installer recorded are refused by table name, and one
/// installer call repairs them.
#[tokio::test]
async fn hand_written_owner_policies_are_refused_until_an_installer_records_them() {
    use proxiima::spiice::SpiiceBundle;
    use proxiima_example::ExampleSpiice;

    let db = example_engine::migrated_db("proxiima_owner_rls_unrecorded").await;
    let pg = migration_storage(&db).await;
    let mut conn = PgConnection::connect(db.platform_url())
        .await
        .expect("platform connection");
    sqlx::raw_sql(
        "CREATE TABLE proxiima_example.handmade (owner_id uuid NOT NULL);
         ALTER TABLE proxiima_example.handmade ENABLE ROW LEVEL SECURITY;
         CREATE POLICY proxiima_owner_read ON proxiima_example.handmade
             FOR SELECT USING (owner_id IS NOT NULL);",
    )
    .execute(&mut conn)
    .await
    .expect("hand-written policy");
    let sidecars = example_engine::frozen_sidecars();
    let refused =
        proxiima::preflight_without_migrations(&pg, ExampleSpiice::migrators(), &sidecars).await;
    let Err(proxiima::MigrationError::SidecarOwnerRls(StorageError::MigrationRequired { details })) =
        refused
    else {
        panic!("expected a refusal naming the table: {refused:?}");
    };
    assert!(details.contains("proxiima_example.handmade"), "{details}");
    assert!(!details.contains("proxiima_example.notebooks"), "{details}");
    sqlx::raw_sql(
        "BEGIN;
         SELECT set_config('app.proxiima_scope', 'platform', true);
         SELECT proxiima_core.install_owner_rls_table('proxiima_example', 'handmade', 'owner_id');
         COMMIT;",
    )
    .execute(&mut conn)
    .await
    .expect("the repair is one installer call");
    proxiima::preflight_without_migrations(&pg, ExampleSpiice::migrators(), &sidecars)
        .await
        .expect("the recorded table boots");
    conn.close().await.expect("close");
    pg.clone_pool_for_backend().close().await;
}
