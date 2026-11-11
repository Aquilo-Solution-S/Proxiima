use proxiima_core::{
    EntityKind, InputContractId, MemoryId, MemoryOutputInvocation, OperatorId,
    OperatorInvocationManifest, OperatorPhase, OperatorProofError, OutputEdgeManifest, SchemaId,
    SchemaVersion,
};
use uuid::Uuid;

fn memory_id() -> MemoryId {
    MemoryId::new(Uuid::now_v7())
}

fn contract_id(seed: &str) -> InputContractId {
    InputContractId::new(Uuid::new_v5(&Uuid::NAMESPACE_URL, seed.as_bytes()))
}

#[test]
fn toa_manifest_rejects_missing_declared_input_edge() {
    let input = memory_id();
    let output = memory_id();
    let manifest = OperatorInvocationManifest::memory_output(MemoryOutputInvocation {
        phase: OperatorPhase::ToA,
        operator_id: OperatorId::new(Uuid::now_v7()),
        input_contract_id: contract_id("test/toa"),
        inputs: vec![(input, EntityKind::Fact)],
        output_memory_id: output,
        output_kind: EntityKind::Abstraction,
        schema_id: SchemaId::new("proxiima-core/agent-derivation".to_string()),
        schema_version: SchemaVersion::new(1),
        output_edges: Vec::new(),
    });

    let err = manifest
        .validate()
        .expect_err("declared input requires output→input ledger edge");
    assert_eq!(
        err,
        OperatorProofError::MissingProvenanceEdge {
            output_memory_id: output,
            input_memory_id: input,
        },
        "{err:?}"
    );
}

#[test]
fn atog_manifest_rejects_goal_output_for_memory_phase() {
    let input = memory_id();
    let goal = proxiima_core::GoalId::new(Uuid::now_v7());
    let manifest = OperatorInvocationManifest::goal_output(
        OperatorPhase::AtoP,
        OperatorId::new(Uuid::now_v7()),
        contract_id("test/bad"),
        vec![(input, EntityKind::Abstraction)],
        goal,
        vec![OutputEdgeManifest::goal_to_memory(goal, input)],
    );

    let err = manifest
        .validate()
        .expect_err("only A→Goal may output Goals");
    assert_eq!(
        err,
        OperatorProofError::PhaseCannotOutputGoal {
            phase: OperatorPhase::AtoP
        },
        "{err:?}"
    );
}

fn manifest_over(
    phase: OperatorPhase,
    output_kind: EntityKind,
    inputs: Vec<(MemoryId, EntityKind)>,
) -> OperatorInvocationManifest {
    let output = memory_id();
    let output_edges = inputs
        .iter()
        .map(|(input, _)| OutputEdgeManifest::memory_to_memory(output, *input))
        .collect();
    OperatorInvocationManifest::memory_output(MemoryOutputInvocation {
        phase,
        operator_id: OperatorId::new(Uuid::now_v7()),
        input_contract_id: contract_id("test/inputs"),
        inputs,
        output_memory_id: output,
        output_kind,
        schema_id: SchemaId::new("proxiima-core/agent-derivation".to_string()),
        schema_version: SchemaVersion::new(1),
        output_edges,
    })
}

#[test]
fn toa_manifest_admits_fact_and_abstraction_inputs_together() {
    manifest_over(
        OperatorPhase::ToA,
        EntityKind::Abstraction,
        vec![
            (memory_id(), EntityKind::Fact),
            (memory_id(), EntityKind::Abstraction),
        ],
    )
    .validate()
    .expect("a mixed Fact and Abstraction input set is one to-Abstraction derivation");
}

#[test]
fn atop_manifest_refuses_a_fact_input() {
    let fact = memory_id();
    let err = manifest_over(
        OperatorPhase::AtoP,
        EntityKind::Perspective,
        vec![(fact, EntityKind::Fact)],
    )
    .validate()
    .expect_err("a Perspective derives from Abstractions only");
    assert_eq!(
        err,
        OperatorProofError::InvalidInputKind {
            memory_id: fact,
            phase: OperatorPhase::AtoP,
            actual: EntityKind::Fact,
        }
    );
}
