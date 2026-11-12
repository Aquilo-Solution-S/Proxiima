//! The documented bound on `core_goal decompose` is exact: a root
//! idempotency key of the longest allowed length with the largest allowed
//! number of children builds every child key and writes every child.

use super::goal_evidence_kinds::{Env, handle_of};

/// The longest root key `core_goal decompose` documents, and the most
/// children it takes.
const ROOT_KEY_MAX: usize = 162;
const MAX_CHILDREN: usize = 50;

#[tokio::test]
async fn decompose_root_key_at_bound_with_max_children_is_accepted() {
    let env = Env::new("proxiima_goal_decompose_bound")
        .await
        .expect("PG required for tests");
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let fact = env.remember("bound fact").await?;
        let target = env.interpret("bound target", &fact).await?;
        let parent = handle_of(&env.set_goal(&[&fact], &target, "bound-parent").await?)?;
        let children: Vec<serde_json::Value> = (0..MAX_CHILDREN)
            .map(|index| {
                serde_json::json!({
                    "schema_id": "core/simple-text-v1", "title": format!("child {index}"),
                    "text": format!("child {index}"), "body": {}, "evidence": [fact],
                })
            })
            .collect();

        let out = env
            .call(
                "core_goal",
                serde_json::json!({
                    "action": "decompose", "parent_goal": parent, "children": children,
                    "motivated_by": [target], "idempotency_key": "k".repeat(ROOT_KEY_MAX),
                }),
            )
            .await?;

        assert_eq!(
            out["children"].as_array().ok_or("children")?.len(),
            MAX_CHILDREN
        );
        Ok(())
    }
    .await;
    env.shutdown().await;
    result.expect("decompose at the key and child bounds");
}
