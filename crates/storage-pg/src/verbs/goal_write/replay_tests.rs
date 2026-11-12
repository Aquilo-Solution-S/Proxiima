use super::*;
use proxiima_core::GoalTopologyWrite;
use proxiima_core::verbs::goal_write::GoalState;
use serde_json::{Value, json};

fn topology() -> GoalTopologyWrite {
    GoalTopologyWrite::new(Vec::new(), Vec::new(), Vec::new()).expect("empty topology")
}

fn payload() -> ReplayPayload {
    ReplayPayload {
        schema_id: "test/schema-v1".to_owned(),
        schema_version: 1,
        title: "title".to_owned(),
        text: "text".to_owned(),
        payload: vec![1, 2, 3],
        sidecar: None,
    }
}

fn serialized<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("replay declaration serializes")
}

fn replacement() -> GoalPayloadWrite {
    GoalPayloadWrite::from_payload(
        "replacement title",
        "replacement text",
        proxiima_core::TaskGoalV1 {
            due_at: Some(time::OffsetDateTime::UNIX_EPOCH),
            priority: Some(proxiima_core::TaskPriority::High),
        },
    )
    .expect("replacement payload")
}

fn rewrite_identity(raw: Value) -> Value {
    rewrite_goal_replay(raw, &replacement(), &|id| Ok(id), &|id| Ok(id)).expect("rewrite")
}

#[test]
fn rewrite_roundtrips_all_goal_replay_verbs_and_replaces_payload_slots() {
    let id = uuid::Uuid::from_u128(1);
    let declarations = [
        serialized(&ReplayDeclaration {
            version: 1,
            command: ReplayCommand::Create(CreateBody {
                draft: ReplayDraft {
                    schema_id: "old/schema".into(),
                    schema_version: 1,
                    title: "old".into(),
                    text: "old".into(),
                    payload: vec![0],
                    sidecar: None,
                    state: ReplayGoalState(GoalState::Active),
                    topology: ReplayTopology {
                        motivated_by: vec![ReplayMotivationRef { perspective_id: id }],
                        dependencies: vec![ReplayDependencyRef { goal_id: id }],
                        evidence: vec![ReplayEvidenceRef { memory_id: id }],
                    },
                    supersedes_goal_id: Some(id),
                    authorship: "user",
                },
            }),
        }),
        serialized(&ReplayDeclaration {
            version: 1,
            command: ReplayCommand::Transition(TransitionBody {
                prior_goal_id: id,
                next_state: ReplayGoalState(GoalState::Paused),
                authorship: "user",
                pending: Some(ReplayPendingWork(proxiima_core::PendingWork::Keep)),
            }),
        }),
        serialized(&ReplayDeclaration {
            version: 1,
            command: ReplayCommand::Achieve(PriorAuthorshipBody {
                prior_goal_id: id,
                authorship: "user",
                evidence: Some(vec![ReplayEvidenceRef { memory_id: id }]),
            }),
        }),
        serialized(&ReplayDeclaration {
            version: 1,
            command: ReplayCommand::Modify(ModifyBody {
                prior_goal_id: id,
                replacement: payload(),
                authorship: "user",
                evidence: Some(vec![ReplayEvidenceRef { memory_id: id }]),
            }),
        }),
        serialized(&ReplayDeclaration {
            version: 1,
            command: ReplayCommand::DecomposeChild(DecomposeChildBody {
                parent_goal_id: id,
                authorship: "user",
                topology: (&topology()).into(),
                child_index: 0,
                payload: payload(),
                evidence: vec![ReplayEvidenceRef { memory_id: id }],
            }),
        }),
    ];
    for raw in declarations {
        let rewritten = rewrite_identity(raw.clone());
        let reparsed: StoredReplayDeclaration =
            serde_json::from_value(rewritten.clone()).expect("rewritten v1 declaration");
        assert_eq!(reparsed.version, 1);
        assert_eq!(rewritten["version"], raw["version"]);
        assert_eq!(rewritten["verb"], raw["verb"]);
        if raw["verb"] == "transition" || raw["verb"] == "achieve" {
            assert_eq!(rewritten, raw);
        }
    }
}

#[test]
fn rewrite_refuses_unknown_version_and_closed_values() {
    let base = json!({
        "version": 1,
        "verb": "transition",
        "prior_goal_id": uuid::Uuid::from_u128(1),
        "next_state": "Active",
        "authorship": "user"
    });
    for (field, value) in [
        ("version", json!(2)),
        ("next_state", json!("Unknown")),
        ("authorship", json!("robot")),
        ("pending", json!("later")),
        ("unexpected", json!(true)),
    ] {
        let mut raw = base.clone();
        raw[field] = value;
        assert!(
            rewrite_goal_replay(raw, &replacement(), &|id| Ok(id), &|id| Ok(id)).is_err(),
            "{field}"
        );
    }
}

#[test]
fn rewrite_refuses_unknown_sidecar_kind() {
    let raw = json!({
        "version": 1,
        "verb": "modify",
        "prior_goal_id": uuid::Uuid::from_u128(1),
        "replacement": {
            "schema_id": "old/schema", "schema_version": 1,
            "title": "old", "text": "old", "payload": [0],
            "sidecar": {
                "kind": "Unknown", "schema_id": "old/schema",
                "schema_version": 1, "payload": {}
            }
        },
        "authorship": "user", "evidence": null
    });
    assert!(rewrite_goal_replay(raw, &replacement(), &|id| Ok(id), &|id| Ok(id)).is_err());
}

#[test]
fn rewrite_propagates_mapping_refusal() {
    let id = uuid::Uuid::from_u128(1);
    let raw = serialized(&ReplayDeclaration {
        version: 1,
        command: ReplayCommand::Transition(TransitionBody {
            prior_goal_id: id,
            next_state: ReplayGoalState(GoalState::Active),
            authorship: "user",
            pending: None,
        }),
    });
    let result = rewrite_goal_replay(
        raw,
        &replacement(),
        &|_| Err(internal("memory map refused")),
        &|_| Err(internal("goal map refused")),
    );
    assert!(result.is_err());
}

#[test]
fn typed_draft_projection_preserves_nested_v1_declarations() {
    use proxiima_core::{GoalDependencyRef, GoalEvidenceRef, GoalMotivationRef, MemoryId};

    let id = uuid::Uuid::from_u128(1);
    let payload = proxiima_core::TaskGoalV1 {
        due_at: Some(time::OffsetDateTime::UNIX_EPOCH),
        priority: Some(proxiima_core::TaskPriority::High),
    };
    let encoded = GoalPayloadWrite::from_payload("title", "text", payload.clone()).unwrap();
    let topology = GoalTopologyWrite::new(
        vec![GoalMotivationRef::perspective(MemoryId::new(id))],
        vec![GoalDependencyRef::new(GoalId::new(id))],
        vec![GoalEvidenceRef::new(MemoryId::new(id))],
    )
    .unwrap();
    let draft = GoalDraft::active_from_payload_write(
        OwnerId::new(id),
        encoded,
        topology,
        GoalAuthorship::User,
        IdempotencyKey::new("replay-projection").unwrap(),
    );
    let legacy = json!({
        "schema_id": draft.schema_id.as_str(),
        "schema_version": draft.schema_version.into_inner(),
        "title": draft.title,
        "text": draft.text,
        "payload": draft.payload,
        "sidecar": {
            "kind": "Goal",
            "schema_id": draft.schema_id.as_str(),
            "schema_version": draft.schema_version.into_inner(),
            "payload": payload,
        },
        "state": draft.state,
        "topology": draft.topology,
        "supersedes_goal_id": null,
        "authorship": "user",
    });
    assert_eq!(serialized(&draft_declaration(&draft).unwrap()), legacy);
}

#[test]
fn create_declaration_preserves_the_versioned_wire_shape() {
    let create = ReplayDeclaration {
        version: 1,
        command: ReplayCommand::Create(CreateBody {
            draft: ReplayDraft {
                schema_id: "test/schema-v1".to_owned(),
                schema_version: 1,
                title: "title".to_owned(),
                text: "text".to_owned(),
                payload: vec![1, 2, 3],
                sidecar: None,
                state: ReplayGoalState(GoalState::Active),
                topology: (&topology()).into(),
                supersedes_goal_id: None,
                authorship: "user",
            },
        }),
    };
    assert_eq!(
        serialized(&create),
        json!({
            "version": 1,
            "verb": "create",
            "draft": {
                "schema_id": "test/schema-v1",
                "schema_version": 1,
                "title": "title",
                "text": "text",
                "payload": [1, 2, 3],
                "sidecar": null,
                "state": "Active",
                "topology": {"motivated_by": [], "dependencies": [], "evidence": []},
                "supersedes_goal_id": null,
                "authorship": "user"
            }
        })
    );
}

#[test]
fn transition_declaration_preserves_the_versioned_wire_shape() {
    let id = uuid::Uuid::from_u128(1);
    let pause = ReplayDeclaration {
        version: 1,
        command: ReplayCommand::Transition(TransitionBody {
            prior_goal_id: id,
            next_state: ReplayGoalState(GoalState::Paused),
            authorship: "system_tool",
            pending: Some(ReplayPendingWork(proxiima_core::PendingWork::Keep)),
        }),
    };
    assert_eq!(
        serialized(&pause),
        json!({
            "version": 1,
            "verb": "transition",
            "prior_goal_id": id,
            "next_state": "Paused",
            "authorship": "system_tool",
            "pending": "keep"
        })
    );

    let resume = ReplayDeclaration {
        version: 1,
        command: ReplayCommand::Transition(TransitionBody {
            prior_goal_id: id,
            next_state: ReplayGoalState(GoalState::Active),
            authorship: "external",
            pending: None,
        }),
    };
    assert_eq!(
        serialized(&resume),
        json!({
            "version": 1,
            "verb": "transition",
            "prior_goal_id": id,
            "next_state": "Active",
            "authorship": "external"
        })
    );
}

#[test]
fn achieve_declaration_preserves_the_versioned_wire_shape() {
    let id = uuid::Uuid::from_u128(1);
    let achieve = ReplayDeclaration {
        version: 1,
        command: ReplayCommand::Achieve(PriorAuthorshipBody {
            prior_goal_id: id,
            authorship: "user",
            evidence: Some(Vec::new()),
        }),
    };
    assert_eq!(
        serialized(&achieve),
        json!({
            "version": 1,
            "verb": "achieve",
            "prior_goal_id": id,
            "authorship": "user",
            "evidence": []
        })
    );
}

#[test]
fn modify_declaration_preserves_the_versioned_wire_shape() {
    let id = uuid::Uuid::from_u128(1);
    let modify = ReplayDeclaration {
        version: 1,
        command: ReplayCommand::Modify(ModifyBody {
            prior_goal_id: id,
            replacement: payload(),
            authorship: "user",
            evidence: None,
        }),
    };
    assert_eq!(
        serialized(&modify),
        json!({
            "version": 1,
            "verb": "modify",
            "prior_goal_id": id,
            "replacement": {
                "schema_id": "test/schema-v1",
                "schema_version": 1,
                "title": "title",
                "text": "text",
                "payload": [1, 2, 3],
                "sidecar": null
            },
            "authorship": "user",
            "evidence": null
        })
    );
}

#[test]
fn decompose_child_declaration_preserves_the_versioned_wire_shape() {
    let id = uuid::Uuid::from_u128(1);
    let child = ReplayDeclaration {
        version: 1,
        command: ReplayCommand::DecomposeChild(DecomposeChildBody {
            parent_goal_id: id,
            authorship: "user",
            topology: (&topology()).into(),
            child_index: 0,
            payload: payload(),
            evidence: Vec::new(),
        }),
    };
    assert_eq!(
        serialized(&child),
        json!({
            "version": 1,
            "verb": "decompose_child",
            "parent_goal_id": id,
            "authorship": "user",
            "topology": {"motivated_by": [], "dependencies": [], "evidence": []},
            "child_index": 0,
            "payload": {
                "schema_id": "test/schema-v1",
                "schema_version": 1,
                "title": "title",
                "text": "text",
                "payload": [1, 2, 3],
                "sidecar": null
            },
            "evidence": []
        })
    );
}
