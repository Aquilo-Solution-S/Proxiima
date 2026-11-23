use super::{
    DrainAction, EmbeddingConfigurationFault, EmbeddingInputRefusal, LlmError, drain_action,
};

#[test]
fn drain_actions_match_ledger_vectors() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/ledger_vectors.json"))
            .expect("parse ledger vectors");
    for row in vectors["Proxiima.Embeddings.drainAction"]
        .as_array()
        .expect("drain action vectors")
    {
        let result = match row["in"][0].as_str().expect("provider result") {
            "success" => Ok(vec![vec![1.0]]),
            "inputRefused" => Err(LlmError::InputRefused(
                EmbeddingInputRefusal::PayloadTooLarge,
            )),
            "transient" => Err(LlmError::retryable("provider unavailable")),
            "configuration" => Err(LlmError::Configuration(
                EmbeddingConfigurationFault::BadRequest,
            )),
            unknown => panic!("unknown provider result: {unknown}"),
        };
        let action = match drain_action(result) {
            DrainAction::Publish(_) => "publish",
            DrainAction::Rescue(_) => "rescue",
            DrainAction::Release(_) => "release",
        };
        assert_eq!(row["out"], action, "{row}");
    }
}
