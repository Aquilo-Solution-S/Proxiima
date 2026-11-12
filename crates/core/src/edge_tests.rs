use super::{
    EdgeEndpoint, EdgeKind, EdgeTargetProjection, MemoryPinField, PinNode, PinTargetKind,
    project_listed_edge, project_memory_target, project_window_edges, validate_edge_layering,
    validate_not_self_loop,
};
use crate::{EntityKind, GoalId, MemoryId};

fn memory(kind: EntityKind) -> EdgeEndpoint {
    EdgeEndpoint::memory(kind, MemoryId::new(uuid::Uuid::now_v7()))
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
fn layering_admits_downward_and_level_memory_edges() {
    for (source, target) in [
        (EntityKind::Perspective, EntityKind::Abstraction),
        (EntityKind::Perspective, EntityKind::Fact),
        (EntityKind::Abstraction, EntityKind::Fact),
        (EntityKind::Abstraction, EntityKind::Abstraction),
        (EntityKind::Fact, EntityKind::Fact),
        (EntityKind::Perspective, EntityKind::Perspective),
    ] {
        validate_edge_layering(memory(source), memory(target))
            .expect("source layer at least target layer is admitted");
    }
}

#[test]
fn layering_rejects_upward_memory_edges() {
    for (source, target) in [
        (EntityKind::Fact, EntityKind::Abstraction),
        (EntityKind::Fact, EntityKind::Perspective),
        (EntityKind::Abstraction, EntityKind::Perspective),
    ] {
        validate_edge_layering(memory(source), memory(target))
            .expect_err("an upward memory edge is a layering violation");
    }
}

#[test]
fn a_self_loop_is_rejected() {
    let endpoint = memory(EntityKind::Perspective);
    validate_not_self_loop(endpoint, endpoint).expect_err("self-loop");
    validate_not_self_loop(endpoint, memory(EntityKind::Fact)).expect("distinct endpoints");
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

/// Live wins, then forgotten, then erased; a target nothing names is
/// redacted.
#[test]
fn a_target_projects_live_then_forgotten_then_erased_else_redacted() {
    let [live, forgotten, erased, unknown] =
        std::array::from_fn(|_| MemoryId::new(uuid::Uuid::now_v7()));
    let readable = std::collections::HashMap::from([(live, EntityKind::Fact)]);
    let unread = crate::verbs::query::UnreadTargets {
        forgotten: vec![crate::verbs::query::MemoryLineageNode {
            memory_id: forgotten,
            kind: EntityKind::Fact,
            schema_id: crate::SchemaId::new("test/pin-v1".into()),
            snippet: String::new(),
            distance: 0,
        }],
        erased: vec![erased],
    };
    let project = |target| project_memory_target(target, &readable, &unread);
    assert_eq!(
        project(live),
        EdgeTargetProjection::visible(EdgeEndpoint::memory(EntityKind::Fact, live))
    );
    assert_eq!(project(forgotten), EdgeTargetProjection::Forgotten);
    assert_eq!(project(erased), EdgeTargetProjection::Erased);
    assert_eq!(project(unknown), EdgeTargetProjection::Redacted);
}

#[test]
fn window_projection_keeps_only_pins_inside_the_node_set() {
    let leaf = MemoryId::new(uuid::Uuid::now_v7());
    let hub = MemoryId::new(uuid::Uuid::now_v7());
    let outside = MemoryId::new(uuid::Uuid::now_v7());
    let nodes = [
        PinNode {
            id: leaf,
            kind: EntityKind::Fact,
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
            kind: EntityKind::Abstraction,
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
        kind: EntityKind::Fact,
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
        node.kind,
        node.id,
        node.refs[0],
        EdgeKind::Reference,
        &std::collections::HashMap::new(),
        &crate::verbs::query::UnreadTargets::default(),
    );
    assert!(matches!(edges.target, EdgeTargetProjection::Redacted));
}

fn pin_node(kind: EntityKind) -> PinNode {
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
    let mut node = pin_node(EntityKind::Abstraction);
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
    let mut node = pin_node(EntityKind::Fact);
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
    let mut node = pin_node(EntityKind::Perspective);
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
        kind: EntityKind::Perspective,
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
