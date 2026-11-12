//! A Goal item of `core_episode_commit` is Goal work, like `core_goal`: a
//! member whose role works Goals sets it whatever its memory limits, and a
//! member whose role does not is refused with nothing of the episode stored.

use super::{
    AgentMemoryApp, GroupId, OwnerId, PgStorage, Proxiima, Relation, Role, UserId, Uuid,
    call_test_model_tool, clone_split_core_db, db_url, seed_group_membership,
    server_issued_group_space_selector, space_authz,
};
use proxiima::CoreMcpError;
use proxiima_core::{AccessCeiling, GoalAccess};

fn words(title: &str) -> serde_json::Value {
    serde_json::json!({"schema_id": "core/simple-text-v1", "title": title, "text": title})
}

#[tokio::test]
async fn an_episode_goal_follows_goal_work_not_administration() {
    let split_db = clone_split_core_db("proxiima_unit_goal_work")
        .await
        .expect("PG required for tests");
    let db_name = split_db.name().to_owned();
    let (runtime_url, platform_url) = (
        split_db.runtime_url().to_owned(),
        split_db.platform_url().to_owned(),
    );
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let admin_pool = sqlx::PgPool::connect(&db_url(&db_name)).await?;
        let worker = OwnerId::from(UserId::new(Uuid::now_v7()));
        let ingest = OwnerId::from(UserId::new(Uuid::now_v7()));
        let group = OwnerId::from(GroupId::new(Uuid::now_v7()));
        let built = Proxiima::<AgentMemoryApp>::app()
            .configure(|b| {
                b.database_url(runtime_url.clone())
                    .store_number(0)
                    .platform_database_url(platform_url.clone())
                    .owner(worker)
                    .served_surface(proxiima_core::ServedSurface::All)
            })
            .build()
            .await?;
        let tools = built.host().core_mcp_tools();
        let scope = proxiima_storage_pg::PgPlatformScope::new(
            sqlx::PgPool::connect(&platform_url).await?,
            &["proxiima_core"],
        )
        .await?;
        let pg = PgStorage::connect_with_config(
            &runtime_url,
            proxiima_storage_pg::PgPoolConfig::from_env()?,
            proxiima_storage_pg::PgTuning::from_env()?,
            scope,
            0,
        )
        .await?;
        seed_group_membership(&pg, &group, Relation::Viewer, &worker).await;
        seed_group_membership(&pg, &group, Relation::Ingest, &ingest).await;
        let fact_worker =
            Role::new(AccessCeiling::Fact, AccessCeiling::Fact)?.with_goals(GoalAccess::Works);
        let worker_authz = space_authz(worker, vec![worker, group], fact_worker);
        let ingest_authz = space_authz(ingest, vec![ingest, group], Role::ingest());
        let space = server_issued_group_space_selector(&tools, worker_authz.clone(), worker).await;

        // Fact limits and Goal work: `core_goal` and an episode's Goal item.
        let mut set = words("Worked through core_goal");
        set["action"] = serde_json::json!("set");
        set["space"] = serde_json::json!(space);
        let set =
            call_test_model_tool(&tools, worker_authz.clone(), worker, "core_goal", set).await?;
        assert!(set["handle"].is_string());
        let episode = call_test_model_tool(
            &tools,
            worker_authz.clone(),
            worker,
            "core_episode_commit",
            serde_json::json!({"space": space, "goal": [words("Worked through an episode")]}),
        )
        .await?;
        assert_eq!(episode["goals"].as_array().map(Vec::len), Some(1));
        let goals: i64 =
            sqlx::query_scalar("SELECT count(*) FROM proxiima_core.goal WHERE owner_id = $1")
                .bind(group.into_inner())
                .fetch_one(&admin_pool)
                .await?;
        assert_eq!(goals, 2, "both Goals are stored");

        // No Goal work: the Goal item is refused and the Fact beside it is not stored.
        let refused = call_test_model_tool(
            &tools,
            ingest_authz,
            ingest,
            "core_episode_commit",
            serde_json::json!({
                "space": space,
                "remember": [{"title": "beside the goal", "body": "b", "tags": []}],
                "goal": [words("Not for an ingest member")],
            }),
        )
        .await;
        assert!(
            matches!(&refused, Err(CoreMcpError::Tool { message, .. })
                if message.contains("requires Goal work on this owner")),
            "{refused:?}"
        );
        let stored: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM proxiima_core.goal WHERE owner_id = $1),
                    (SELECT count(*) FROM proxiima_core.memory WHERE owner_id = $1 AND kind = 'fact'
                       AND \"by\" = $2)",
        )
        .bind(group.into_inner())
        .bind(ingest.into_inner())
        .fetch_one(&admin_pool)
        .await?;
        assert_eq!(stored, (2, 0), "nothing of the refused episode is stored");
        admin_pool.close().await;
        built.shutdown().await;
        Ok(())
    }
    .await;
    result.expect("episode Goal items follow Goal work");
}
