//! Direct `core_goal` actions are operator-authored: the author category the
//! store keeps in each write's replay identity is `system_operator` for set,
//! modify and every decompose child.

use proxiima_core::GoalId;
use proxiima_storage_pg::test_fixtures::goal_authorship_category;
use uuid::Uuid;

use super::goal_evidence_kinds::{Env, handle_of};

#[tokio::test]
async fn direct_goal_actions_store_operator_authorship() {
    let env = Env::new("proxiima_goal_authorship")
        .await
        .expect("PG required for tests");
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let pool = env.admin().await?;
        let category = |handle: String| {
            let pool = pool.clone();
            async move {
                let goal = GoalId::new(Uuid::parse_str(&handle)?);
                Ok::<_, Box<dyn std::error::Error>>(goal_authorship_category(&pool, goal).await?)
            }
        };
        let fact = env.remember("authorship fact").await?;
        let target = env.interpret("authorship target", &fact).await?;

        let set = handle_of(&env.set_goal(&[&fact], &target, "author-set").await?)?;
        assert_eq!(category(set.clone()).await?, "system_operator", "set");

        let modified = handle_of(
            &env.call(
                "core_goal",
                serde_json::json!({
                    "action": "modify", "goal": set, "schema_id": "core/simple-text-v1",
                    "title": "modified", "text": "modified", "body": {},
                    "idempotency_key": "author-modify",
                }),
            )
            .await?,
        )?;
        assert_eq!(
            category(modified.clone()).await?,
            "system_operator",
            "modify"
        );

        let out = env
            .decompose_goal(
                &modified,
                &[&[&fact], &[&fact]],
                &target,
                "author-decompose",
            )
            .await?;
        for child in out["children"].as_array().ok_or("children")? {
            assert_eq!(
                category(handle_of(child)?).await?,
                "system_operator",
                "decompose child"
            );
        }
        pool.close().await;
        Ok(())
    }
    .await;
    env.shutdown().await;
    result.expect("operator authorship is stored");
}
