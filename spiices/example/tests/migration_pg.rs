//! The example spiice's migrations: owner RLS on a split-role database, and
//! its one tool's owner scope.

use proxiima::spiice::owner_transaction;
use proxiima::testkit::{SplitRoleDb, scoped_authz, shared_authz};
use proxiima::{AuthzContext, BuiltProxiima, OwnerId, QueryRequest, company_owner};
use proxiima_example::NoteV1;
use proxiima_example::contract::LIST_NOTEBOOKS_TOOL;
use proxiima_example::testkit::{build_engine, register_notebook, remember_note};
use uuid::Uuid;

/// The example migrator applies on a split-role database, its census passes,
/// and the Engine admits a scoped note into the typed sidecar.
#[tokio::test]
async fn the_baseline_applies_under_split_roles_and_the_census_passes() {
    let db = SplitRoleDb::create("proxiima_example_baseline", &[])
        .await
        .expect("PG required");
    let owner = company_owner(Uuid::now_v7());
    let built = build_engine(db.runtime_url(), db.platform_url(), owner)
        .await
        .expect("the example baseline boots under the runtime RLS guard");
    let authz = scoped_authz(owner);
    let runtime = sqlx::PgPool::connect(db.runtime_url()).await.unwrap();
    let notebook_id = register_notebook(
        &runtime,
        authz.owner_scope().expect("owner scope"),
        owner,
        "inbox",
    )
    .await
    .expect("the owner registers a notebook");
    let engine = built.host().engine();
    let memory_id = remember_note(
        engine,
        &authz,
        owner,
        &NoteV1 {
            notebook_id,
            key: "first".to_owned(),
            body: "a note under owner RLS".to_owned(),
            written_at: time::OffsetDateTime::now_utc(),
        },
    )
    .await
    .expect("a note in a registered notebook is admitted");
    let response = engine
        .query(&authz, &QueryRequest::readable())
        .await
        .expect("owner query");
    assert!(
        response
            .memories
            .iter()
            .any(|memory| memory.id == memory_id)
    );

    let admin = sqlx::PgPool::connect(&db.admin_url()).await.unwrap();
    let versions: Vec<i64> = sqlx::query_scalar(
        "SELECT version FROM public._sqlx_migrations_proxiima_example ORDER BY version",
    )
    .fetch_all(&admin)
    .await
    .expect("the example's own ledger");
    assert_eq!(
        versions,
        proxiima_example::migrator()
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>()
    );
    admin.close().await;
    runtime.close().await;
    built.shutdown().await;
}

/// Static SQL: one notebook row, written on an owner transaction.
const INSERT_NOTEBOOK_SQL: &str =
    "INSERT INTO proxiima_example.notebooks (notebook_id, owner_id, name) VALUES ($1, $2, $3)";

/// `(notebook_id, name)` of every notebook the example's tool lists for
/// `owner` under `authz`, called as a host calls a registry tool.
async fn listed(built: &BuiltProxiima, authz: AuthzContext, owner: OwnerId) -> Vec<(Uuid, String)> {
    let output = built
        .host()
        .core_mcp_tools()
        .call_core_tool(
            authz,
            owner,
            None,
            LIST_NOTEBOOKS_TOOL,
            serde_json::json!({}),
        )
        .await
        .expect("the tool answers");
    output["notebooks"]
        .as_array()
        .expect("a notebook list")
        .iter()
        .map(|item| {
            (
                item["notebook_id"]
                    .as_str()
                    .and_then(|id| id.parse().ok())
                    .expect("a notebook id"),
                item["name"].as_str().expect("a name").to_owned(),
            )
        })
        .collect()
}

/// Through the tool: an owner lists its own notebooks and not another
/// owner's, even on a scope that may read both owners. A row written on an
/// owner transaction that is dropped without a commit is not there
/// afterwards, nor one whose transaction a statement tried to commit; one
/// that commits is.
#[tokio::test]
async fn the_tool_lists_the_callers_notebooks_on_its_owner_transaction() {
    let db = SplitRoleDb::create("proxiima_example_tool", &[])
        .await
        .expect("PG required");
    let owner_a = company_owner(Uuid::now_v7());
    let owner_b = company_owner(Uuid::now_v7());
    let built = build_engine(db.runtime_url(), db.platform_url(), owner_a)
        .await
        .expect("boot");
    let runtime = sqlx::PgPool::connect(db.runtime_url()).await.unwrap();
    let authz_a = scoped_authz(owner_a);
    let scope_a = authz_a.owner_scope().expect("scope a");
    let notebook_a = register_notebook(&runtime, scope_a, owner_a, "a's")
        .await
        .expect("a registers");
    let a_notebook = vec![(notebook_a, "a's".to_owned())];
    assert_eq!(listed(&built, authz_a.clone(), owner_a).await, a_notebook);
    let both = shared_authz(&[owner_a, owner_b]);
    assert_eq!(
        listed(&built, both.clone(), owner_a).await,
        a_notebook,
        "the shared scope reads owner A's notebook as owner A"
    );
    assert_eq!(
        listed(&built, both, owner_b).await,
        Vec::new(),
        "owner B must not see owner A's notebook, even on a scope that reads both"
    );

    let ctx_a = proxiima::ToolCtx::new(
        owner_a,
        authz_a.clone(),
        built.host().services().clone(),
        proxiima::ToolCaller::new("test", "test", "0"),
        built.host().engine().clone(),
    );
    let mut dropped = owner_transaction(&ctx_a)
        .await
        .expect("the runtime publishes owner transactions");
    dropped
        .execute(
            sqlx::query(INSERT_NOTEBOOK_SQL)
                .bind(Uuid::now_v7())
                .bind(owner_a.into_inner())
                .bind("dropped"),
        )
        .await
        .expect("the insert runs");
    drop(dropped);
    assert_eq!(
        listed(&built, authz_a.clone(), owner_a).await,
        a_notebook,
        "a dropped owner transaction rolls back"
    );

    let mut two_commands = owner_transaction(&ctx_a)
        .await
        .expect("another transaction");
    two_commands
        .execute(
            sqlx::query(INSERT_NOTEBOOK_SQL)
                .bind(Uuid::now_v7())
                .bind(owner_a.into_inner())
                .bind("two commands"),
        )
        .await
        .expect("the insert runs");
    two_commands
        .execute(sqlx::query("SELECT 1; COMMIT"))
        .await
        .expect_err("a call runs one command");
    drop(two_commands);
    assert_eq!(
        listed(&built, authz_a.clone(), owner_a).await,
        a_notebook,
        "a statement cannot end the transaction"
    );

    let kept = Uuid::now_v7();
    let mut committed = owner_transaction(&ctx_a)
        .await
        .expect("a second transaction");
    committed
        .execute(
            sqlx::query(INSERT_NOTEBOOK_SQL)
                .bind(kept)
                .bind(owner_a.into_inner())
                .bind("kept"),
        )
        .await
        .expect("the insert runs");
    committed.commit().await.expect("commit");
    assert_eq!(
        listed(&built, authz_a, owner_a).await,
        vec![(notebook_a, "a's".to_owned()), (kept, "kept".to_owned())]
    );
    runtime.close().await;
    built.shutdown().await;
}
