//! One `core_derive` Abstraction may be made from Facts and Abstractions
//! together; a Perspective is made from Abstractions only.

use super::goal_evidence_kinds::{Env, handle_of};

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

fn derive_args(kind: &str, title: &str, sources: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "kind": kind, "title": title, "body": title, "tags": [],
        "source_handles": sources, "model_id": "test-model",
    })
}

#[tokio::test]
async fn an_abstraction_derives_from_a_fact_and_an_abstraction_in_one_call() {
    let env = Env::new("proxiima_mixed_origins")
        .await
        .expect("PG required for tests");
    let result: TestResult<()> = async {
        let fact = env.remember("mixed fact").await?;
        let first = env.derive("mixed first", &fact).await?;
        let mixed = handle_of(
            &env.call(
                "core_derive",
                derive_args("Abstraction", "mixed", &[&fact, &first]),
            )
            .await?,
        )?;

        let read = env
            .read_as(
                env.owner,
                &format!("proxiima://memory/{mixed}?expand_neighbors=true"),
            )
            .await?;
        let mut origins: Vec<&str> = read["neighbor_edges"]
            .as_array()
            .ok_or("neighbor_edges")?
            .iter()
            .filter(|edge| edge["source"] == mixed.as_str() && edge["kind"] == "origin")
            .map(|edge| edge["target"].as_str().ok_or("target"))
            .collect::<Result<_, _>>()?;
        origins.sort_unstable();
        let mut expected = [fact.as_str(), first.as_str()];
        expected.sort_unstable();
        assert_eq!(origins, expected, "{read}");
        Ok(())
    }
    .await;
    env.shutdown().await;
    result.expect("mixed Fact and Abstraction origins");
}

#[tokio::test]
async fn a_perspective_is_refused_a_fact_source_alone_or_in_a_mix() {
    let env = Env::new("proxiima_mixed_perspective")
        .await
        .expect("PG required for tests");
    let result: TestResult<()> = async {
        let fact = env.remember("perspective fact").await?;
        let abstraction = env.derive("perspective abstraction", &fact).await?;
        for sources in [
            vec![fact.as_str()],
            vec![fact.as_str(), abstraction.as_str()],
        ] {
            let err = env
                .call(
                    "core_derive",
                    derive_args("Perspective", "stance", &sources),
                )
                .await
                .expect_err("a Perspective derives from Abstractions only");
            assert!(
                err.to_string()
                    .contains("Perspective cannot be derived from Fact sources"),
                "{sources:?}: {err}"
            );
        }
        env.call(
            "core_derive",
            derive_args("Perspective", "stance", &[&abstraction]),
        )
        .await?;
        Ok(())
    }
    .await;
    env.shutdown().await;
    result.expect("Perspective source kinds");
}
