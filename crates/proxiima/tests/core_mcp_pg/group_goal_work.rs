//! A group's goals are the work of every member who reads them: a viewer
//! sets, modifies and transitions them through the MCP gate, while the
//! viewer relation still refuses its Fact and Perspective writes. A
//! non-member, or a context whose membership storage no longer finds, works
//! no goal of the group; an admin keeps working them.

use super::{
    AgentMemoryApp, AuthzContext, CoreMcpTools, GroupId, OwnerId, PgStorage, Proxiima, Relation,
    Role, UserId, Uuid, call_test_model_tool, clone_split_core_db, db_url, seed_group_membership,
    server_issued_group_space_selector, space_authz,
};

fn goal_set(space: &str, title: &str) -> serde_json::Value {
    serde_json::json!({
        "action": "set", "space": space, "schema_id": "core/simple-text-v1",
        "title": title, "text": "group work", "body": {},
    })
}

async fn goal_call(
    tools: &CoreMcpTools,
    authz: &AuthzContext,
    caller: OwnerId,
    args: serde_json::Value,
) -> Result<String, String> {
    call_test_model_tool(tools, authz.clone(), caller, "core_goal", args)
        .await
        .map(|out| out["handle"].as_str().expect("goal handle").to_owned())
        .map_err(|error| error.to_string())
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one scenario keeps the three callers and both memory refusals together"
)]
async fn group_members_work_group_goals_without_writing_group_memory() {
    let split_db = clone_split_core_db("proxiima_group_goal_work")
        .await
        .expect("PG required for tests");
    let db_name = split_db.name().to_owned();
    let (runtime_url, platform_url) = (
        split_db.runtime_url().to_owned(),
        split_db.platform_url().to_owned(),
    );
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let admin_pool = sqlx::PgPool::connect(&db_url(&db_name)).await?;
        let viewer = OwnerId::from(UserId::new(Uuid::now_v7()));
        let admin = OwnerId::from(UserId::new(Uuid::now_v7()));
        let outsider = OwnerId::from(UserId::new(Uuid::now_v7()));
        let group = OwnerId::from(GroupId::new(Uuid::now_v7()));
        let built = Proxiima::<AgentMemoryApp>::app()
            .configure(|b| {
                b.database_url(runtime_url.clone())
                    .store_number(0)
                    .platform_database_url(platform_url.clone())
                    .owner(viewer)
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
        seed_group_membership(&pg, &group, Relation::Viewer, &viewer).await;
        seed_group_membership(&pg, &group, Relation::Admin, &admin).await;
        let viewer_authz = space_authz(viewer, vec![viewer, group], Role::viewer());
        let admin_authz = space_authz(admin, vec![admin, group], Role::admin());
        let outsider_authz = space_authz(outsider, vec![outsider], Role::viewer());
        let space = server_issued_group_space_selector(&tools, viewer_authz.clone(), viewer).await;

        // The viewer sets, modifies and transitions a goal the group owns.
        let set = goal_call(
            &tools,
            &viewer_authz,
            viewer,
            goal_set(&space, "Viewer goal"),
        )
        .await
        .map_err(|e| format!("a viewer member sets a group goal: {e}"))?;
        let goal_owner: Uuid =
            sqlx::query_scalar("SELECT owner_id FROM proxiima_core.goal WHERE t = $1")
                .bind(set.parse::<Uuid>()?)
                .fetch_one(&admin_pool)
                .await?;
        assert_eq!(goal_owner, group.into_inner(), "the group owns the goal");
        let modified = goal_call(
            &tools,
            &viewer_authz,
            viewer,
            serde_json::json!({
                "action": "modify", "space": space, "goal": set,
                "schema_id": "core/simple-text-v1", "title": "Viewer goal, edited",
                "text": "group work, edited", "body": {},
            }),
        )
        .await
        .map_err(|e| format!("a viewer member modifies a group goal: {e}"))?;
        let paused = goal_call(
            &tools,
            &viewer_authz,
            viewer,
            serde_json::json!({
                "action": "transition", "space": space, "goal": modified,
                "transition": "pause", "pending": "keep",
            }),
        )
        .await
        .map_err(|e| format!("a viewer member transitions a group goal: {e}"))?;

        // Achieving keeps the Goal write ceiling.
        let achieved = goal_call(
            &tools,
            &viewer_authz,
            viewer,
            serde_json::json!({"action": "mark_achieved", "space": space, "goal": paused}),
        )
        .await;
        assert!(achieved.is_err(), "a viewer achieves no group goal");

        // The viewer relation still bounds the group's memory.
        let fact = call_test_model_tool(
            &tools,
            admin_authz.clone(),
            admin,
            "core_remember",
            serde_json::json!({"space": space, "title": "group fact", "body": "b", "tags": []}),
        )
        .await?;
        let fact = fact["handle"].as_str().expect("fact handle").to_owned();
        let remembered = call_test_model_tool(
            &tools,
            viewer_authz.clone(),
            viewer,
            "core_remember",
            serde_json::json!({"space": space, "title": "leak", "body": "b", "tags": []}),
        )
        .await;
        assert!(remembered.is_err(), "a viewer writes no group Fact");
        let interpreted = call_test_model_tool(
            &tools,
            viewer_authz.clone(),
            viewer,
            "core_interpret",
            serde_json::json!({
                "space": space, "claim": "a viewer's stance", "confidence": 80,
                "subjects": [fact],
            }),
        )
        .await;
        assert!(interpreted.is_err(), "a viewer writes no group Perspective");

        // A non-member works none of the group's goals.
        assert!(
            goal_call(
                &tools,
                &outsider_authz,
                outsider,
                goal_set(&space, "Outsider goal")
            )
            .await
            .is_err(),
            "a non-member sets no group goal"
        );
        assert!(
            goal_call(
                &tools,
                &outsider_authz,
                outsider,
                serde_json::json!({
                    "action": "transition", "space": space, "goal": paused,
                    "transition": "resume",
                }),
            )
            .await
            .is_err(),
            "a non-member transitions no group goal"
        );

        // Storage resolves the membership again: a context still naming a
        // viewer role the group no longer holds works no goal.
        let former = OwnerId::from(UserId::new(Uuid::now_v7()));
        let former_authz = space_authz(former, vec![former, group], Role::viewer());
        let stale = goal_call(
            &tools,
            &former_authz,
            former,
            goal_set(&space, "Stale goal"),
        )
        .await;
        assert!(
            stale.is_err(),
            "a stale membership sets no group goal: {stale:?}"
        );

        // An admin keeps working the group's goals.
        let resumed = goal_call(
            &tools,
            &admin_authz,
            admin,
            serde_json::json!({
                "action": "transition", "space": space, "goal": paused, "transition": "resume",
            }),
        )
        .await
        .map_err(|e| format!("an admin transitions a group goal: {e}"))?;
        goal_call(&tools, &admin_authz, admin, goal_set(&space, "Admin goal"))
            .await
            .map_err(|e| format!("an admin sets a group goal: {e}"))?;
        assert_ne!(resumed, paused);

        built.shutdown().await;
        Ok(())
    }
    .await;
    result.expect("group goal work test failed");
}
