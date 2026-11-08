//! #671: a pin or lineage edge to a memory erased in this store reads
//! `erased target`, from the witness the erase left. Only a reader who can
//! read a memory that pins the erased id learns that it was erased.

use proxiima::Proxiima;
use proxiima_core::{AuthPath, AuthzContext, GroupId, OwnerId, Role, UserId};
use uuid::Uuid;

use super::split_core_db::clone_split_core_db;
use super::{
    AgentMemoryApp, call_test_model_tool, read_test_model_resource,
    server_issued_group_space_selector, space_authz,
};

/// A personal Abstraction is derived from a Fact in an abandoned group;
/// erasing the group leaves the Abstraction's pin and an erase witness.
/// Its neighbours and its lineage then name the Fact `erased target`. The
/// witness itself is readable only by a reader who can read the pinning
/// Abstraction.
#[tokio::test]
async fn a_pin_to_an_erased_memory_reads_erased_target() {
    let split_db = clone_split_core_db("proxiima_erased_targets")
        .await
        .expect("PG required for tests");
    let (runtime_url, platform_url) = (
        split_db.runtime_url().to_owned(),
        split_db.platform_url().to_owned(),
    );
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let personal = OwnerId::from(UserId::new(Uuid::now_v7()));
        let group_id = GroupId::new(Uuid::now_v7());
        let group = OwnerId::from(group_id);
        let built = Proxiima::<AgentMemoryApp>::app()
            .database_url(runtime_url.clone())
            .store_number(0)
            .platform_database_url(platform_url.clone())
            .owner(personal)
            .served_surface(proxiima_core::ServedSurface::All)
            .build()
            .await?;
        register_abandoned_group(&platform_url, group_id).await?;
        let tools = built.host().core_mcp_tools();
        let authz = space_authz(personal, vec![personal, group], Role::admin());
        let shared = server_issued_group_space_selector(&tools, authz.clone(), personal).await;
        let call = |tool: &'static str, args: serde_json::Value| {
            call_test_model_tool(&tools, authz.clone(), personal, tool, args)
        };
        let handle = |made: serde_json::Value| made["handle"].as_str().expect("handle").to_owned();

        let fact = handle(
            call(
                "core_remember",
                serde_json::json!({
                    "space": shared,
                    "title": "group fact",
                    "body": "group fact",
                    "tags": []
                }),
            )
            .await?,
        );
        let derived = handle(
            call(
                "core_derive",
                serde_json::json!({
                    "kind": "Abstraction",
                    "title": "personal conclusion",
                    "body": "personal conclusion",
                    "source_handles": [fact],
                    "model_id": "test-model"
                }),
            )
            .await?,
        );

        erase_group(built.host().engine(), group_id).await?;

        let read = read_test_model_resource(
            &tools,
            authz.clone(),
            personal,
            &format!("proxiima://memory/{derived}?expand_neighbors=true"),
        )
        .await?;
        let targets = edge_targets(&read["neighbor_edges"]);
        assert_eq!(targets, ["erased target"], "{read}");

        let lineage = read_test_model_resource(
            &tools,
            authz.clone(),
            personal,
            &format!("proxiima://memory/{derived}/lineage?direction=ancestors&depth=2"),
        )
        .await?;
        assert_eq!(
            edge_targets(&lineage["edges"]),
            ["erased target"],
            "{lineage}"
        );

        // The witness's owner read policy, fed a reader who reads no memory
        // that pins the erased id: the row is not there for it.
        let pool = built.host().pool_for_tests();
        let fact = Uuid::parse_str(&fact)?;
        assert!(
            witness_visible(pool, personal, fact).await?,
            "the pinning owner sees it"
        );
        let stranger = OwnerId::from(UserId::new(Uuid::now_v7()));
        assert!(
            !witness_visible(pool, stranger, fact).await?,
            "a reader with no pinning memory learns nothing"
        );
        built.shutdown().await;
        Ok(())
    }
    .await;
    result.expect("erased targets");
}

/// An abandoned group: registered as a group, with no members, so its owner
/// may be erased.
async fn register_abandoned_group(
    platform_url: &str,
    group: GroupId,
) -> Result<(), Box<dyn std::error::Error>> {
    let platform = sqlx::PgPool::connect(platform_url).await?;
    let mut register = platform.begin().await?;
    sqlx::query("SELECT set_config('app.proxiima_scope', 'platform', true)")
        .execute(&mut *register)
        .await?;
    sqlx::query("INSERT INTO proxiima_core.owners (owner_id, kind) VALUES ($1, 'group')")
        .bind(group.into_inner())
        .execute(&mut *register)
        .await?;
    register.commit().await?;
    platform.close().await;
    Ok(())
}

/// Whether `reader`'s owner-scope transaction on the runtime pool sees the
/// erase witness of `erased`.
async fn witness_visible(
    pool: &sqlx::PgPool,
    reader: OwnerId,
    erased: Uuid,
) -> Result<bool, Box<dyn std::error::Error>> {
    let scope = proxiima_core::test_fixtures::owner_scope_for_tests(&[reader]);
    let mut tx = proxiima_storage_pg::begin_owner_transaction(pool, &scope).await?;
    let seen: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM proxiima_core.erased_pin_target WHERE t = $1)",
    )
    .bind(erased)
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;
    Ok(seen)
}

/// Erase `group` with system authority; the erase must complete.
async fn erase_group(
    engine: &proxiima_core::Engine,
    group: GroupId,
) -> Result<(), Box<dyn std::error::Error>> {
    let owner = OwnerId::from(group);
    let system =
        proxiima_core::test_fixtures::authenticated_context(AuthzContext::for_subject_with_role(
            UserId::new(Uuid::now_v7()),
            [(owner, Role::admin())],
            AuthPath::System,
        ))
        .narrowed_to_owner(owner)
        .ok_or("the erase context covers the group")?;
    let outcome = engine.erase_group_owner(&system, group).await?;
    assert!(
        matches!(outcome, proxiima_core::OwnerEraseOutcome::Completed { .. }),
        "{outcome:?}"
    );
    Ok(())
}

/// The `target` of every edge in a wire edge array.
fn edge_targets(edges: &serde_json::Value) -> Vec<&str> {
    edges
        .as_array()
        .expect("edges")
        .iter()
        .filter_map(|edge| edge["target"].as_str())
        .collect()
}
