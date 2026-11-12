use super::{INBOUND_SOURCE_PAGE_MIN, PinHop, PinTarget, page_hops, read_edges_from_nodes};
use crate::edge::PinNode;
use crate::storage_ports::{InboundPinQuery, MemoryReadPort};
use crate::verbs::query::{EdgeFilter, EdgeReadRequest};
use crate::{
    EdgeKind, EdgeTargetProjection, EntityKind, EntityRef, MemoryId, OwnerId, StorageError,
};
use std::collections::HashMap;
use std::sync::Arc;

fn hop(source: MemoryId, target: MemoryId, kind: EdgeKind) -> PinHop {
    PinHop {
        source_id: source,
        source_kind: EntityKind::Abstraction,
        target: PinTarget::Memory(target),
        kind,
    }
}

#[test]
fn page_hops_redacts_targets_absent_from_visible() {
    let source = MemoryId::new(uuid::Uuid::now_v7());
    let missing = MemoryId::new(uuid::Uuid::now_v7());
    let page = page_hops(
        vec![hop(source, missing, EdgeKind::Reference)],
        None,
        10,
        &super::Seen::default(),
    );
    assert!(matches!(
        page.edges[0].target,
        EdgeTargetProjection::Redacted
    ));
}

/// The page projects each target through what the probe said of it.
#[test]
fn page_hops_lists_forgotten_and_erased_targets_as_the_probe_says() {
    let source = MemoryId::new(uuid::Uuid::now_v7());
    let forgotten = MemoryId::new(uuid::Uuid::now_v7());
    let erased = MemoryId::new(uuid::Uuid::now_v7());
    let seen = super::Seen {
        live: HashMap::new(),
        unread: crate::verbs::query::UnreadTargets {
            forgotten: vec![crate::verbs::query::MemoryLineageNode {
                memory_id: forgotten,
                kind: EntityKind::Fact,
                schema_id: crate::SchemaId::new("test/fact".to_owned()),
                snippet: String::new(),
                distance: 0,
            }],
            erased: vec![erased],
        },
    };
    let page = page_hops(
        vec![
            hop(source, forgotten, EdgeKind::Origin),
            hop(source, erased, EdgeKind::Origin),
        ],
        None,
        10,
        &seen,
    );
    let targets: Vec<_> = page.edges.iter().map(|edge| edge.target).collect();
    assert!(
        targets.contains(&EdgeTargetProjection::Forgotten),
        "{targets:?}"
    );
    assert!(
        targets.contains(&EdgeTargetProjection::Erased),
        "{targets:?}"
    );
}

struct FakePins {
    nodes: Vec<PinNode>,
    visible_goals: Vec<crate::GoalId>,
    /// One entry per `load_visible_goal_ids` round trip. The point of
    /// the split is that a page with no Goal reference makes none.
    goal_probes: Arc<std::sync::Mutex<Vec<Vec<crate::GoalId>>>>,
    /// Ids the reader could read but which are forgotten.
    forgotten: Vec<MemoryId>,
}

impl FakePins {
    fn new(nodes: Vec<PinNode>, visible_goals: Vec<crate::GoalId>) -> Self {
        Self {
            nodes,
            visible_goals,
            goal_probes: Arc::new(std::sync::Mutex::new(Vec::new())),
            forgotten: Vec::new(),
        }
    }

    fn inbound(&self, query: InboundPinQuery<'_>) -> Vec<PinNode> {
        let mut rows: Vec<PinNode> = self
            .nodes
            .iter()
            .filter(|node| {
                // Mirrors the backend: reference hops onto a Goal target
                // come from `goal_refs`, onto a Memory target from `refs`.
                // Origin and read pins are Memory-only in both.
                let references = |node: &PinNode, t: &MemoryId| {
                    if query.target_kind == crate::PinTargetKind::Goal {
                        node.goal_refs
                            .iter()
                            .chain(node.guided_by.iter())
                            .any(|goal| goal.into_inner() == t.into_inner())
                    } else {
                        node.refs.contains(t) || node.forked_from == Some(*t)
                    }
                };
                query.targets.iter().any(|t| match query.kind {
                    Some(EdgeKind::Origin) => node.origins.contains(t),
                    Some(EdgeKind::Reference) => references(node, t),
                    Some(EdgeKind::Read) => node.reads.contains(t),
                    None => {
                        node.origins.contains(t) || references(node, t) || node.reads.contains(t)
                    }
                })
            })
            .cloned()
            .collect();
        rows.sort_by_key(|node| std::cmp::Reverse(node.id.into_inner()));
        if let Some(after) = query.after {
            rows.retain(|n| n.id.into_inner() < after.into_inner());
        }
        rows.truncate(usize::try_from(query.limit).unwrap_or(usize::MAX));
        rows
    }
}

#[async_trait::async_trait]
impl MemoryReadPort for FakePins {
    fn store_number(&self) -> u16 {
        0
    }

    async fn load_fact_text(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _owner: &crate::OwnerId,
        _memory_id: MemoryId,
    ) -> Result<Option<String>, StorageError> {
        Ok(None)
    }

    async fn load_memory_graph_payloads(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _identities: &[crate::storage::MemoryGraphIdentity],
        _schemas: &[crate::read_models::MemorySchemaSpec],
        _include_body: bool,
    ) -> Result<Vec<crate::storage::MemoryGraphPayloadRow>, StorageError> {
        Ok(Vec::new())
    }

    async fn load_sketches(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _read_owners: &[OwnerId],
        _memory_ids: &[MemoryId],
    ) -> Result<Vec<crate::read_models::MemorySketch>, StorageError> {
        Ok(Vec::new())
    }

    async fn load_pin_nodes(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _read_owners: &[OwnerId],
        memory_ids: &[MemoryId],
    ) -> Result<Vec<PinNode>, StorageError> {
        Ok(self
            .nodes
            .iter()
            .filter(|n| memory_ids.contains(&n.id))
            .cloned()
            .collect())
    }

    async fn load_visible_goal_ids(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _read_owners: &[OwnerId],
        goal_ids: &[crate::GoalId],
    ) -> Result<Vec<crate::GoalId>, StorageError> {
        self.goal_probes
            .lock()
            .expect("goal probe log")
            .push(goal_ids.to_vec());
        Ok(goal_ids
            .iter()
            .copied()
            .filter(|id| self.visible_goals.contains(id))
            .collect())
    }

    async fn load_inbound_pin_nodes(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _read_owners: &[OwnerId],
        query: InboundPinQuery<'_>,
    ) -> Result<Vec<PinNode>, StorageError> {
        Ok(self.inbound(query))
    }

    async fn query_memories(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _read_owners: &[OwnerId],
        _req: &crate::verbs::query::QueryRequest,
        _schemas: &[crate::read_models::MemorySchemaSpec],
    ) -> Result<crate::verbs::query::QueryResponse, StorageError> {
        Err(StorageError::Internal("unused".into()))
    }

    async fn count_memories(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _read_owners: &[OwnerId],
    ) -> Result<crate::read_models::MemoryCounts, StorageError> {
        Err(StorageError::Internal("unused".into()))
    }

    async fn search_memories(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _req: &crate::verbs::query::MemorySearchRequest,
        _projections: &[crate::verbs::schema::MemorySearchProjection],
    ) -> Result<crate::verbs::query::MemorySearchPage, StorageError> {
        Err(StorageError::Internal("unused".into()))
    }

    async fn walk_memory_lineage(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _read_owners: &[OwnerId],
        _req: &crate::verbs::query::MemoryLineageRequest,
    ) -> Result<crate::verbs::query::MemoryLineageResponse, StorageError> {
        Err(StorageError::Internal("unused".into()))
    }

    async fn external_edges(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _read_owners: &[OwnerId],
        _memory_ids: &[crate::MemoryId],
    ) -> Result<Vec<(crate::MemoryId, uuid::Uuid, crate::EdgeKind)>, StorageError> {
        Err(StorageError::Internal("unused".into()))
    }

    async fn lineage_nodes(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _read_owners: &[OwnerId],
        _memory_ids: &[crate::MemoryId],
    ) -> Result<Vec<crate::verbs::query::MemoryLineageNode>, StorageError> {
        Err(StorageError::Internal("unused".into()))
    }

    async fn unread_targets(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _read_owners: &[OwnerId],
        memory_ids: &[crate::MemoryId],
    ) -> Result<crate::verbs::query::UnreadTargets, StorageError> {
        Ok(crate::verbs::query::UnreadTargets {
            forgotten: self
                .forgotten
                .iter()
                .filter(|id| memory_ids.contains(id))
                .map(|id| crate::verbs::query::MemoryLineageNode {
                    memory_id: *id,
                    kind: EntityKind::Fact,
                    schema_id: crate::SchemaId::new("test/fact".to_owned()),
                    snippet: String::new(),
                    distance: 0,
                })
                .collect(),
            erased: Vec::new(),
        })
    }

    async fn owned_series_handle(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _owner: crate::OwnerId,
        _schema_id: &crate::SchemaId,
        _sidecar_table: &str,
        _columns: &[(&str, crate::verbs::query::SidecarAtom)],
    ) -> Result<Option<uuid::Uuid>, StorageError> {
        Ok(None)
    }
}

fn hub_fixture(n: usize) -> (OwnerId, MemoryId, Arc<FakePins>) {
    let owner = OwnerId::from(crate::UserId::new(uuid::Uuid::now_v7()));
    let hub = MemoryId::new(uuid::Uuid::now_v7());
    let mut nodes = vec![PinNode {
        id: hub,
        kind: EntityKind::Fact,
        schema_id: crate::SchemaId::new("test/pin-v1".into()),
        origins: Vec::new(),
        refs: Vec::new(),
        goal_refs: Vec::new(),
        guided_by: Vec::new(),
        reads: Vec::new(),
        forked_from: None,
    }];
    for _ in 0..n {
        nodes.push(PinNode {
            id: MemoryId::new(uuid::Uuid::now_v7()),
            kind: EntityKind::Abstraction,
            schema_id: crate::SchemaId::new("test/pin-v1".into()),
            origins: vec![hub],
            refs: Vec::new(),
            goal_refs: Vec::new(),
            guided_by: Vec::new(),
            reads: Vec::new(),
            forked_from: None,
        });
    }
    (owner, hub, Arc::new(FakePins::new(nodes, Vec::new())))
}

#[tokio::test]
async fn incoming_read_edges_retains_cursor_at_source_page_boundaries() {
    for limit in [1, INBOUND_SOURCE_PAGE_MIN, INBOUND_SOURCE_PAGE_MIN + 1] {
        let source_count = usize::try_from(limit).expect("test limit") + 1;
        let (owner, hub, fake) = hub_fixture(source_count);
        let handle: crate::storage_ports::MemoryReadHandle = fake;
        let mut req = EdgeReadRequest {
            filter: EdgeFilter {
                kind: Some(EdgeKind::Origin),
                source: None,
                target: Some(EntityRef::Memory(hub)),
            },
            limit,
            cursor: None,
        };
        let first = read_edges_from_nodes(&handle, None, &[owner], &req)
            .await
            .expect("first page");
        assert_eq!(first.edges.len(), source_count - 1);
        req.cursor = Some(first.next_cursor.unwrap_or_else(|| {
            panic!("limit={limit}: one further incoming edge requires a cursor")
        }));
        let second = read_edges_from_nodes(&handle, None, &[owner], &req)
            .await
            .expect("second page");
        assert_eq!(second.edges.len(), 1);
        assert!(second.next_cursor.is_none());
        assert!(
            first
                .edges
                .iter()
                .all(|edge| edge.source != second.edges[0].source)
        );
    }
}

#[tokio::test]
async fn incoming_read_edges_resumes_between_pin_kinds_and_redacts_targets() {
    for target_visible in [true, false] {
        let (owner, hub, mut fake) = hub_fixture(2);
        let nodes = &mut Arc::get_mut(&mut fake).expect("unique fixture").nodes;
        for node in nodes.iter_mut().filter(|node| node.id != hub) {
            node.refs.push(hub);
        }
        if !target_visible {
            nodes.retain(|node| node.id != hub);
        }
        let handle: crate::storage_ports::MemoryReadHandle = fake;
        let mut req = EdgeReadRequest {
            filter: EdgeFilter {
                kind: None,
                source: None,
                target: Some(EntityRef::Memory(hub)),
            },
            limit: 1,
            cursor: None,
        };
        let mut seen = std::collections::HashSet::new();
        for index in 0..4 {
            let page = read_edges_from_nodes(&handle, None, &[owner], &req)
                .await
                .expect("mixed pin page");
            assert_eq!(page.edges.len(), 1);
            let edge = &page.edges[0];
            assert!(seen.insert((edge.source, edge.kind)));
            assert_eq!(
                matches!(edge.target, EdgeTargetProjection::Redacted),
                !target_visible
            );
            assert_eq!(page.next_cursor.is_some(), index < 3);
            req.cursor = page.next_cursor;
        }
        assert_eq!(seen.len(), 4);
    }
}
