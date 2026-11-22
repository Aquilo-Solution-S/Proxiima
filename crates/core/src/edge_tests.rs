use super::{
    EdgeEndpoint, EdgeKind, EdgeTargetProjection, MemoryPinField, PinNode, PinTargetKind,
    project_listed_edge, project_memory_target, project_window_edges, validate_not_self_loop,
};
use crate::{EntityKind, GoalId, MemoryId, MemoryKind};

fn memory(kind: EntityKind) -> EdgeEndpoint {
    EdgeEndpoint::memory(
        crate::MemoryKind::of(kind).unwrap(),
        MemoryId::new(uuid::Uuid::now_v7()),
    )
}

#[test]
fn a_read_set_refuses_one_memory_in_two_spaces() {
    let memory = MemoryId::new(uuid::Uuid::now_v7());
    let (a, b) = (
        crate::OwnerId::new(uuid::Uuid::now_v7()),
        crate::OwnerId::new(uuid::Uuid::now_v7()),
    );
    let pin = |space| super::ReadPin::Local { memory, space };
    assert_eq!(
        super::ReadSet::new([pin(a), pin(b)]),
        Err(super::ReadSetError::ConflictingSpace { memory })
    );
    let once = super::ReadSet::new([pin(a), pin(a)]).expect("a repeat is kept once");
    assert_eq!(once.pins(), &[pin(a)]);
    assert_eq!(
        once.clone()
            .merged(super::ReadSet::new([pin(b)]).expect("one pin")),
        Err(super::ReadSetError::ConflictingSpace { memory })
    );
    // One memory cannot be both of this store and of another.
    assert_eq!(
        once.merged(super::ReadSet::new([super::ReadPin::OtherStore { memory }]).expect("one pin")),
        Err(super::ReadSetError::ConflictingSpace { memory })
    );
}

#[test]
fn a_read_set_splits_local_and_other_store_reads_in_order() {
    let space = crate::OwnerId::new(uuid::Uuid::now_v7());
    let (first, second, third) = (
        MemoryId::new(uuid::Uuid::from_u128(1)),
        MemoryId::new(uuid::Uuid::from_u128(2)),
        MemoryId::new(uuid::Uuid::from_u128(3)),
    );
    let reads = super::ReadSet::new([
        super::ReadPin::OtherStore { memory: third },
        super::ReadPin::Local {
            memory: second,
            space,
        },
        super::ReadPin::OtherStore { memory: first },
    ])
    .expect("three memories");
    assert_eq!(reads.local().collect::<Vec<_>>(), [(second, space)]);
    assert_eq!(reads.other_store().collect::<Vec<_>>(), [first, third]);
}

#[test]
fn a_read_set_refuses_more_than_its_cap() {
    let space = crate::OwnerId::new(uuid::Uuid::now_v7());
    let pins = |count: usize| {
        (0..count).map(move |_| super::ReadPin::Local {
            memory: MemoryId::new(uuid::Uuid::now_v7()),
            space,
        })
    };
    let full = super::ReadSet::new(pins(super::ReadSet::MAX_PINS)).expect("the cap fits");
    assert_eq!(
        full.merged(super::ReadSet::new(pins(1)).expect("one pin")),
        Err(super::ReadSetError::TooMany {
            count: super::ReadSet::MAX_PINS + 1
        })
    );
}

#[test]
fn a_self_loop_is_rejected() {
    let endpoint = memory(EntityKind::Perspective);
    validate_not_self_loop(endpoint, endpoint).expect_err("self-loop");
    validate_not_self_loop(endpoint, memory(EntityKind::Fact)).expect("distinct endpoints");
}

#[test]
fn endpoint_wire_shape_preserves_valid_addresses_and_refuses_mismatched_kinds() {
    let id = uuid::Uuid::now_v7();
    for kind in crate::MemoryKind::ALL {
        let endpoint = EdgeEndpoint::memory(kind, MemoryId::new(id));
        let wire = serde_json::json!({"kind": kind, "entity": {"Memory": id}});
        assert_eq!(serde_json::to_value(endpoint).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<EdgeEndpoint>(wire).unwrap(),
            endpoint
        );
        assert!(
            serde_json::from_value::<EdgeEndpoint>(
                serde_json::json!({"kind": kind, "entity": {"Goal": id}}),
            )
            .is_err()
        );
    }
    let goal = EdgeEndpoint::goal(GoalId::new(id));
    let wire = serde_json::json!({"kind": "Goal", "entity": {"Goal": id}});
    assert_eq!(serde_json::to_value(goal).unwrap(), wire);
    assert_eq!(serde_json::from_value::<EdgeEndpoint>(wire).unwrap(), goal);
    assert!(
        serde_json::from_value::<EdgeEndpoint>(
            serde_json::json!({"kind": "Goal", "entity": {"Memory": id}}),
        )
        .is_err()
    );
}

#[test]
fn a_redacted_target_yields_no_endpoint() {
    let endpoint = memory(EntityKind::Fact);
    assert_eq!(
        EdgeTargetProjection::visible(endpoint).endpoint(),
        Some(endpoint)
    );
    assert_eq!(EdgeTargetProjection::Redacted.endpoint(), None);
    assert_eq!(EdgeTargetProjection::Forgotten.endpoint(), None);
    assert_eq!(EdgeTargetProjection::Erased.endpoint(), None);
}

#[test]
fn window_projection_keeps_only_pins_inside_the_node_set() {
    let leaf = MemoryId::new(uuid::Uuid::now_v7());
    let hub = MemoryId::new(uuid::Uuid::now_v7());
    let outside = MemoryId::new(uuid::Uuid::now_v7());
    let nodes = [
        PinNode {
            id: leaf,
            kind: MemoryKind::Fact,
            schema_id: crate::SchemaId::new("test/pin-v1".into()),
            origins: Vec::new(),
            refs: Vec::new(),
            goal_refs: Vec::new(),
            guided_by: Vec::new(),
            reads: Vec::new(),
            forked_from: None,
        },
        PinNode {
            id: hub,
            kind: MemoryKind::Abstraction,
            schema_id: crate::SchemaId::new("test/pin-v1".into()),
            origins: vec![leaf],
            refs: vec![outside],
            goal_refs: Vec::new(),
            guided_by: Vec::new(),
            reads: Vec::new(),
            forked_from: None,
        },
    ];
    let edges = project_window_edges(&nodes, 50);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].kind, EdgeKind::Origin);
    assert_eq!(edges[0].source.memory_id(), Some(hub));
    assert_eq!(
        edges[0].target.endpoint().and_then(EdgeEndpoint::memory_id),
        Some(leaf)
    );
}

#[test]
fn unresolved_goal_reference_stays_a_redactable_raw_id() {
    let source = MemoryId::new(uuid::Uuid::now_v7());
    let goal = GoalId::new(uuid::Uuid::now_v7());
    let mut node = PinNode {
        id: source,
        kind: MemoryKind::Fact,
        schema_id: crate::SchemaId::new("test/pin-v1".into()),
        origins: Vec::new(),
        refs: Vec::new(),
        goal_refs: vec![goal],
        guided_by: Vec::new(),
        reads: Vec::new(),
        forked_from: None,
    };
    // Storage typed it as a Goal, but this reader cannot see it, so it
    // falls back into the raw carrier and redacts exactly as an
    // unreadable Memory would. The stored column must not leak.
    node.resolve_visible_goal_refs(&[]);
    assert!(node.goal_refs.is_empty());
    assert_eq!(node.refs, vec![MemoryId::new(goal.into_inner())]);
    let edges = project_listed_edge(
        crate::MemoryKind::Fact,
        node.id,
        node.refs[0],
        EdgeKind::Reference,
        &std::collections::HashMap::new(),
        &crate::verbs::query::UnreadTargets::default(),
    );
    assert!(matches!(edges.target, EdgeTargetProjection::Redacted));
}

fn pin_node(kind: MemoryKind) -> PinNode {
    PinNode {
        id: MemoryId::new(uuid::Uuid::now_v7()),
        kind,
        schema_id: crate::SchemaId::new("test/pin-v1".into()),
        origins: Vec::new(),
        refs: Vec::new(),
        goal_refs: Vec::new(),
        guided_by: Vec::new(),
        reads: Vec::new(),
        forked_from: None,
    }
}

#[test]
fn pins_list_origins_then_references_with_the_fork_source_then_reads() {
    let [origin, reference, read, source] =
        std::array::from_fn(|_| MemoryId::new(uuid::Uuid::now_v7()));
    let mut node = pin_node(MemoryKind::Abstraction);
    node.origins = vec![origin];
    node.refs = vec![reference];
    node.reads = vec![read];
    node.forked_from = Some(source);
    assert_eq!(
        node.pins().collect::<Vec<_>>(),
        vec![
            (origin, EdgeKind::Origin),
            (reference, EdgeKind::Reference),
            (source, EdgeKind::Reference),
            (read, EdgeKind::Read),
        ]
    );
}

#[test]
fn a_fork_source_already_in_refs_is_listed_once() {
    let source = MemoryId::new(uuid::Uuid::now_v7());
    let mut node = pin_node(MemoryKind::Fact);
    node.refs = vec![source];
    node.forked_from = Some(source);
    assert_eq!(
        node.pins().collect::<Vec<_>>(),
        vec![(source, EdgeKind::Reference)]
    );
}

#[test]
fn goal_pins_list_goal_references_then_guidance() {
    let [referenced, guide] = std::array::from_fn(|_| GoalId::new(uuid::Uuid::now_v7()));
    let mut node = pin_node(MemoryKind::Perspective);
    node.goal_refs = vec![referenced];
    node.guided_by = vec![guide];
    assert_eq!(
        node.goal_pins().collect::<Vec<_>>(),
        vec![
            (referenced, EdgeKind::Reference),
            (guide, EdgeKind::Reference)
        ]
    );
}

#[test]
fn a_pin_field_targets_goals_or_memories_never_both() {
    for field in MemoryPinField::ALL {
        let admitted: Vec<EntityKind> = [
            EntityKind::Fact,
            EntityKind::Abstraction,
            EntityKind::Perspective,
        ]
        .into_iter()
        .flat_map(|source| field.admitted(source).iter().copied())
        .collect();
        let goals = admitted.iter().filter(|k| **k == EntityKind::Goal).count();
        assert!(goals == 0 || goals == admitted.len(), "{field:?}");
        assert_eq!(
            field.target() == PinTargetKind::Goal,
            goals > 0,
            "{field:?}"
        );
    }
    assert!(MemoryPinField::Refs.admitted(EntityKind::Goal).is_empty());
}

#[test]
fn edge_labels_parse_back_to_their_kind() {
    for kind in EdgeKind::ALL {
        assert_eq!(EdgeKind::parse(kind.as_str()), Some(kind));
    }
    assert_eq!(EdgeKind::parse("fork"), None);
}

#[test]
fn unresolved_guidance_stays_a_redactable_raw_id() {
    let source = MemoryId::new(uuid::Uuid::now_v7());
    let goal = GoalId::new(uuid::Uuid::now_v7());
    let mut node = PinNode {
        id: source,
        kind: MemoryKind::Perspective,
        schema_id: crate::SchemaId::new("test/pin-v1".into()),
        origins: Vec::new(),
        refs: Vec::new(),
        goal_refs: Vec::new(),
        guided_by: vec![goal],
        reads: Vec::new(),
        forked_from: None,
    };
    node.resolve_visible_goal_refs(&[]);
    assert!(node.guided_by.is_empty());
    assert_eq!(node.refs, vec![MemoryId::new(goal.into_inner())]);
}

#[derive(serde::Deserialize)]
struct LedgerRow<I, O> {
    #[serde(rename = "in")]
    input: I,
    out: O,
}

fn ledger_rows<I, O>(key: &str, count: usize) -> Vec<LedgerRow<I, O>>
where
    I: serde::de::DeserializeOwned,
    O: serde::de::DeserializeOwned,
{
    let root: serde_json::Value =
        serde_json::from_str(include_str!("../tests/ledger_vectors.json")).unwrap();
    let rows: Vec<_> = serde_json::from_value(root[key].clone()).unwrap();
    assert_eq!(rows.len(), count, "{key}");
    rows
}

#[test]
fn pin_labels_match_ledger_vectors() {
    for row in ledger_rows::<[EdgeKind; 1], String>("Proxiima.Edges.pinLabel", EdgeKind::ALL.len())
    {
        let [kind] = row.input;
        assert_eq!(kind.as_str(), row.out);
        assert_eq!(EdgeKind::parse(&row.out), Some(kind));
    }
}

#[test]
fn pin_field_table_matches_ledger_vectors() {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FieldSpec {
        column: String,
        kind: EdgeKind,
        shape: super::PinShape,
        fact: Vec<EntityKind>,
        abstraction: Vec<EntityKind>,
        perspective: Vec<EntityKind>,
    }
    let fields = MemoryPinField::ALL;
    for row in
        ledger_rows::<[MemoryPinField; 1], FieldSpec>("Proxiima.Edges.pinField", fields.len())
    {
        let [field] = row.input;
        let spec = field.spec();
        assert_eq!(spec.column, row.out.column, "{field:?}");
        assert_eq!(spec.kind, row.out.kind, "{field:?}");
        assert_eq!(spec.shape, row.out.shape, "{field:?}");
        assert_eq!(
            spec.admits,
            [
                row.out.fact.as_slice(),
                row.out.abstraction.as_slice(),
                row.out.perspective.as_slice()
            ],
            "{field:?}",
        );
    }
}

#[test]
fn projected_target_precedence_matches_ledger_vectors() {
    let id = MemoryId::new(uuid::Uuid::now_v7());
    for row in ledger_rows::<[bool; 3], String>("Proxiima.Edges.projectTarget", 8) {
        let [live, forgotten, erased] = row.input;
        let readable = live
            .then_some((id, MemoryKind::Fact))
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        let unread = crate::verbs::query::UnreadTargets {
            forgotten: forgotten
                .then(|| crate::verbs::query::MemoryLineageNode {
                    memory_id: id,
                    kind: MemoryKind::Fact,
                    schema_id: crate::SchemaId::new("test/pin-v1".into()),
                    snippet: String::new(),
                    distance: 0,
                })
                .into_iter()
                .collect(),
            erased: erased.then_some(id).into_iter().collect(),
        };
        let projected = project_memory_target(id, &readable, &unread);
        assert_eq!(
            serde_json::to_value(projected).unwrap()["state"],
            row.out,
            "{:?}",
            row.input
        );
        if live {
            assert_eq!(
                projected.endpoint(),
                Some(EdgeEndpoint::memory(crate::MemoryKind::Fact, id))
            );
        }
    }
}
