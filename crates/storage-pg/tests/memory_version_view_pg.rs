//! `proxiima_core.memory_version`, the read-only view of memory versions for
//! spiice and host SQL, and core's own `live_memory` and
//! `memory_including_forgotten`, against Postgres.
//!
//! It shows what the views promise: the caller's row security decides what is
//! visible, no write goes through them, and a lookup by id still reaches the
//! index of the table under them. The two core views differ in one thing:
//! whether a forgotten memory is in them.

use proxiima_core::storage_ports::{MemoryAuthoringPort, OwnerWritePermit};
use proxiima_core::test_fixtures::owner_scope_for_tests;
use proxiima_core::{AccessKind, MemoryId, OwnerId, UserId};
use proxiima_pg_testkit::DbGuard;
use proxiima_storage_pg::test_fixtures::{TestMemory, fresh_pg};
use proxiima_storage_pg::{PgStorage, begin_owner_transaction};
use serde_json::Value;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

struct World {
    pg: PgStorage,
    _db: DbGuard,
    owner: OwnerId,
    other: OwnerId,
}

async fn world(prefix: &str) -> World {
    let (pg, db) = fresh_pg(prefix).await;
    World {
        pg,
        _db: db,
        owner: OwnerId::from(UserId::new(Uuid::now_v7())),
        other: OwnerId::from(UserId::new(Uuid::now_v7())),
    }
}

impl World {
    async fn tx(&self, scope_owner: OwnerId) -> Transaction<'static, Postgres> {
        begin_owner_transaction(
            &self.pg.clone_pool_for_backend(),
            &owner_scope_for_tests(&[scope_owner]),
        )
        .await
        .expect("owner transaction")
    }

    async fn write(&self, owner: OwnerId, memory: TestMemory) -> (MemoryId, Uuid) {
        let admitted = memory.create(&self.pg, owner).await.expect("write");
        (admitted.memory_id, admitted.handle)
    }

    async fn forget(&self, owner: OwnerId, memory_id: MemoryId) {
        let permit = OwnerWritePermit::new_for_tests(owner, AccessKind::Fact);
        MemoryAuthoringPort::forget_memory(&self.pg, &permit, memory_id)
            .await
            .expect("forget");
    }

    /// A transaction in platform scope, where row security shows every row and
    /// the read itself decides what it wants.
    async fn platform_tx(&self) -> Transaction<'static, Postgres> {
        let mut tx = self.pg.pool_for_tests().begin().await.expect("begin");
        sqlx::query("SELECT set_config('app.proxiima_scope', 'platform', true)")
            .execute(&mut *tx)
            .await
            .expect("platform scope");
        tx
    }

    /// The ids among `ids` that the transaction sees through the view.
    async fn visible(&self, scope_owner: OwnerId, ids: &[MemoryId]) -> Vec<Uuid> {
        let raw: Vec<Uuid> = ids.iter().map(|id| id.into_inner()).collect();
        let mut tx = self.tx(scope_owner).await;
        let seen = sqlx::query_scalar(
            "SELECT t FROM proxiima_core.memory_version WHERE t = ANY($1) ORDER BY t",
        )
        .bind(&raw)
        .fetch_all(&mut *tx)
        .await
        .expect("view read");
        tx.rollback().await.unwrap();
        seen
    }
}

#[tokio::test]
async fn an_owner_sees_its_live_versions_and_which_one_is_the_head() {
    let w = world("memory_version_view_owner").await;
    let (first, handle) = w.write(w.owner, TestMemory::fact()).await;
    let (second, _) = w.write(w.owner, TestMemory::fact().handle(handle)).await;
    let mut tx = w.tx(w.owner).await;
    let rows: Vec<(Uuid, Uuid, String, Uuid, String, bool)> = sqlx::query_as(
        "SELECT t, handle, kind::text, owner_id, schema_id, head
           FROM proxiima_core.memory_version WHERE handle = $1 ORDER BY t",
    )
    .bind(handle)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.1, handle);
        assert_eq!(row.2, "fact");
        assert_eq!(row.3, w.owner.into_inner());
        assert_eq!(row.4, "core/test-fact-v1");
    }
    assert_eq!(rows[0].0, first.into_inner());
    assert_eq!(rows[1].0, second.into_inner());
    assert_eq!(
        rows.iter().map(|r| r.5).collect::<Vec<_>>(),
        vec![false, true],
        "only the newest version is the head"
    );
}

#[tokio::test]
async fn a_forgotten_or_foreign_memory_is_hidden_through_the_view() {
    let w = world("memory_version_view_hidden").await;
    let (live, _) = w.write(w.owner, TestMemory::fact()).await;
    let (gone, _) = w.write(w.owner, TestMemory::fact()).await;
    let (foreign, _) = w.write(w.other, TestMemory::fact()).await;
    w.forget(w.owner, gone).await;
    let all = [live, gone, foreign];
    assert_eq!(w.visible(w.owner, &all).await, vec![live.into_inner()]);
    assert_eq!(w.visible(w.other, &all).await, vec![foreign.into_inner()]);
}

/// The SQLSTATE of the refusal; panics if the statement succeeded.
fn refusal(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> String {
    let error = result.expect_err("a write through the view must be refused");
    error
        .as_database_error()
        .and_then(|e| e.code().map(|c| c.to_string()))
        .unwrap_or_else(|| panic!("not a database error: {error}"))
}

#[tokio::test]
async fn no_write_goes_through_the_view() {
    let w = world("memory_version_view_writes").await;
    let (memory_id, _) = w.write(w.owner, TestMemory::fact()).await;
    let t = memory_id.into_inner();
    let statements = [
        (
            "INSERT",
            "INSERT INTO proxiima_core.memory_version (t) VALUES ($1)",
        ),
        (
            "UPDATE",
            "UPDATE proxiima_core.memory_version SET schema_id = 'x' WHERE t = $1",
        ),
        (
            "DELETE",
            "DELETE FROM proxiima_core.memory_version WHERE t = $1",
        ),
    ];
    for (verb, sql) in statements {
        let mut tx = w.tx(w.owner).await;
        // SQL-POLICY: fixed-fragment (a literal from the table above)
        let runtime = refusal(sqlx::query(sql).bind(t).execute(&mut *tx).await);
        tx.rollback().await.unwrap();
        let mut admin = w.pg.pool_for_tests().begin().await.unwrap();
        // SQL-POLICY: fixed-fragment (a literal from the table above)
        let privileged = refusal(sqlx::query(sql).bind(t).execute(&mut *admin).await);
        admin.rollback().await.unwrap();
        // 55000: the view is not updatable, whatever the role was granted.
        assert_eq!(
            (runtime.as_str(), privileged.as_str()),
            ("55000", "55000"),
            "{verb}"
        );
    }
    let schema: String =
        sqlx::query_scalar("SELECT schema_id FROM proxiima_core.memory WHERE t = $1")
            .bind(t)
            .fetch_one(w.pg.pool_for_tests())
            .await
            .unwrap();
    assert_eq!(schema, "core/test-fact-v1", "the memory row is untouched");
}

#[tokio::test]
async fn the_serving_role_may_only_select_the_view() {
    let w = world("memory_version_view_grants").await;
    let mut tx = w.tx(w.owner).await;
    let mut held = Vec::new();
    for privilege in ["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE"] {
        let has: bool = sqlx::query_scalar(
            "SELECT has_table_privilege(current_user, 'proxiima_core.memory_version', $1)",
        )
        .bind(privilege)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        held.push((privilege, has));
    }
    tx.rollback().await.unwrap();
    assert_eq!(
        held,
        [
            ("SELECT", true),
            ("INSERT", false),
            ("UPDATE", false),
            ("DELETE", false),
            ("TRUNCATE", false),
        ]
    );
}

/// Every plan node under `plan`, depth first.
fn plan_nodes<'a>(plan: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(plan);
    if let Some(children) = plan.get("Plans").and_then(Value::as_array) {
        for child in children {
            plan_nodes(child, out);
        }
    }
}

/// The plan of `sql` on the owner's transaction with sequential scans
/// discouraged, as a flat list of nodes.
async fn plan_of(w: &World, sql: &str) -> Vec<Value> {
    let mut tx = w.tx(w.owner).await;
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await
        .unwrap();
    // SQL-POLICY: fixed-fragment (EXPLAIN of a statement the caller formatted from a Uuid)
    let explained: Value =
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN (FORMAT JSON) {sql}")))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    tx.rollback().await.unwrap();
    let mut nodes = Vec::new();
    plan_nodes(&explained[0]["Plan"], &mut nodes);
    nodes.into_iter().cloned().collect()
}

fn scans(nodes: &[Value], relation: &str) -> Vec<String> {
    nodes
        .iter()
        .filter(|node| node["Relation Name"] == relation)
        .filter_map(|node| node["Node Type"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn a_lookup_by_id_through_the_view_uses_the_memory_index() {
    let w = world("memory_version_view_plan").await;
    let (memory_id, _) = w.write(w.owner, TestMemory::fact()).await;
    let id = memory_id.into_inner();

    // SQL-POLICY: fixed-fragment (a Uuid formatted into the literal)
    let nodes = plan_of(
        &w,
        &format!("SELECT owner_id FROM proxiima_core.memory_version WHERE t = '{id}'"),
    )
    .await;
    assert_eq!(scans(&nodes, "memory"), ["Index Scan"], "{nodes:#?}");

    // SQL-POLICY: fixed-fragment (a Uuid formatted into the literal)
    let nodes = plan_of(
        &w,
        &format!("SELECT owner_id FROM proxiima_core.memory_version WHERE t = '{id}' AND head"),
    )
    .await;
    assert_eq!(scans(&nodes, "memory"), ["Index Scan"], "{nodes:#?}");
    let head_scans = scans(&nodes, "memory_head");
    assert!(
        !head_scans.is_empty() && head_scans.iter().all(|scan| scan != "Seq Scan"),
        "{nodes:#?}"
    );
}

const LIVE: &str = "proxiima_core.live_memory";
const WITH_FORGOTTEN: &str = "proxiima_core.memory_including_forgotten";

/// The ids among `ids` that `tx` sees through `view`, oldest first.
async fn seen_through(
    tx: &mut Transaction<'static, Postgres>,
    view: &str,
    ids: &[MemoryId],
) -> Vec<Uuid> {
    let raw: Vec<Uuid> = ids.iter().map(|id| id.into_inner()).collect();
    // SQL-POLICY: fixed-fragment (a view name from the constants above)
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT t FROM {view} WHERE t = ANY($1) ORDER BY t"
    )))
    .bind(&raw)
    .fetch_all(&mut **tx)
    .await
    .expect("view read")
}

fn raw(ids: &[MemoryId]) -> Vec<Uuid> {
    ids.iter().map(|id| id.into_inner()).collect()
}

#[tokio::test]
async fn in_platform_scope_only_the_live_view_leaves_a_forgotten_memory_out() {
    let w = world("memory_version_view_platform").await;
    let (live, _) = w.write(w.owner, TestMemory::fact()).await;
    let (gone, _) = w.write(w.owner, TestMemory::fact()).await;
    w.forget(w.owner, gone).await;
    let all = [live, gone];
    let mut tx = w.platform_tx().await;
    assert_eq!(seen_through(&mut tx, LIVE, &all).await, raw(&[live]));
    assert_eq!(seen_through(&mut tx, WITH_FORGOTTEN, &all).await, raw(&all));
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn in_owner_scope_row_security_hides_a_forgotten_memory_from_both_views() {
    let w = world("memory_version_view_owner_scope").await;
    let (live, _) = w.write(w.owner, TestMemory::fact()).await;
    let (gone, _) = w.write(w.owner, TestMemory::fact()).await;
    let (foreign, _) = w.write(w.other, TestMemory::fact()).await;
    w.forget(w.owner, gone).await;
    let all = [live, gone, foreign];

    let mut tx = w.tx(w.owner).await;
    assert_eq!(seen_through(&mut tx, LIVE, &all).await, raw(&[live]));
    assert_eq!(
        seen_through(&mut tx, WITH_FORGOTTEN, &all).await,
        raw(&[live])
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn the_serving_role_cannot_write_or_lock_through_the_core_views() {
    let w = world("memory_version_view_core_writes").await;
    let (memory_id, _) = w.write(w.owner, TestMemory::fact()).await;
    let t = memory_id.into_inner();
    // Each is refused with 42501: the role holds SELECT on the view and
    // nothing else, and a row lock through a view needs UPDATE on it.
    for sql in [
        "INSERT INTO proxiima_core.live_memory (t) VALUES ($1)",
        "UPDATE proxiima_core.live_memory SET schema_id = 'x' WHERE t = $1",
        "DELETE FROM proxiima_core.live_memory WHERE t = $1",
        "SELECT t FROM proxiima_core.live_memory WHERE t = $1 FOR SHARE",
        "INSERT INTO proxiima_core.memory_including_forgotten (t) VALUES ($1)",
        "UPDATE proxiima_core.memory_including_forgotten SET schema_id = 'x' WHERE t = $1",
        "DELETE FROM proxiima_core.memory_including_forgotten WHERE t = $1",
        "SELECT t FROM proxiima_core.memory_including_forgotten WHERE t = $1 FOR SHARE",
    ] {
        let mut tx = w.tx(w.owner).await;
        // SQL-POLICY: fixed-fragment (a literal of the list above)
        let refused = sqlx::query(sql).bind(t).execute(&mut *tx).await;
        tx.rollback().await.unwrap();
        assert_eq!(refusal(refused), "42501", "{sql}");
    }
}

/// The index each scan of `relation` in `nodes` walks.
fn indexes(nodes: &[Value], relation: &str) -> Vec<String> {
    nodes
        .iter()
        .filter(|node| node["Relation Name"] == relation)
        .filter_map(|node| node["Index Name"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn a_lookup_by_id_through_the_live_view_walks_the_index_the_table_read_does() {
    let w = world("memory_version_view_live_plan").await;
    let (memory_id, _) = w.write(w.owner, TestMemory::fact()).await;
    let id = memory_id.into_inner();

    // SQL-POLICY: fixed-fragment (a Uuid formatted into the literal)
    let through_view = plan_of(
        &w,
        &format!("SELECT owner_id FROM proxiima_core.live_memory WHERE t = '{id}'"),
    )
    .await;
    // SQL-POLICY: fixed-fragment (a Uuid formatted into the literal)
    let on_table = plan_of(
        &w,
        &format!(
            "SELECT owner_id FROM proxiima_core.memory WHERE t = '{id}' AND forgotten_at IS NULL"
        ),
    )
    .await;
    assert_eq!(
        indexes(&through_view, "memory"),
        ["memory_t_key"],
        "{through_view:#?}"
    );
    assert_eq!(
        indexes(&through_view, "memory"),
        indexes(&on_table, "memory"),
        "{through_view:#?}"
    );
    assert_eq!(
        scans(&through_view, "memory"),
        scans(&on_table, "memory"),
        "{through_view:#?}"
    );
}
