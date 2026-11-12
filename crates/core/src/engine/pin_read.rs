//! Load pin-carrier nodes from storage; project [`Edge`] in memory.

use std::collections::{HashMap, HashSet};

use crate::edge::{
    EdgeEndpoint, EdgeTargetProjection, PinNode, pin_created_at, project_listed_edge,
};
use crate::error::ProtocolError;
use crate::storage_ports::{InboundPinQuery, MemoryReadHandle, PinTargetKind};
use crate::verbs::query::{
    EdgeExistsRequest, EdgeExistsResponse, EdgeReadCursor, EdgeReadRequest, EdgeReadResponse,
    SupersessionStatus, UnreadTargets,
};
use crate::{Edge, EdgeKind, EntityKind, EntityRef, GoalId, MemoryId, OwnerId, OwnerScope};

use super::errors::internal_storage_error;

/// Floor on incoming `read_edges` source pages so a small hop `limit`
/// still amortizes GIN. Single-edge probes need only two source rows to
/// establish whether the returned edge has a continuation.
const INBOUND_SOURCE_PAGE_MIN: u32 = 256;

type HopKey = (time::OffsetDateTime, uuid::Uuid, uuid::Uuid, &'static str);

struct PinHop {
    source_id: MemoryId,
    source_kind: EntityKind,
    target: PinTarget,
    kind: EdgeKind,
}

#[derive(Clone, Copy)]
enum PinTarget {
    Memory(MemoryId),
    Goal(GoalId),
}

impl PinTarget {
    const fn id(self) -> uuid::Uuid {
        match self {
            Self::Memory(id) => id.into_inner(),
            Self::Goal(id) => id.into_inner(),
        }
    }

    const fn entity(self) -> EntityRef {
        match self {
            Self::Memory(id) => EntityRef::Memory(id),
            Self::Goal(id) => EntityRef::Goal(id),
        }
    }
}

fn empty_read() -> EdgeReadResponse {
    EdgeReadResponse {
        edges: Vec::new(),
        next_cursor: None,
    }
}

fn memory_ref(entity: Option<EntityRef>) -> Option<MemoryId> {
    match entity? {
        EntityRef::Memory(id) => Some(id),
        EntityRef::Goal(_) => None,
    }
}

fn entity_id(entity: Option<EntityRef>) -> Option<uuid::Uuid> {
    Some(match entity? {
        EntityRef::Memory(id) => id.into_inner(),
        EntityRef::Goal(id) => id.into_inner(),
    })
}

async fn resolve_visible_goal_refs(
    memory_read: &MemoryReadHandle,
    owner_scope: Option<&OwnerScope>,
    read_owners: &[OwnerId],
    nodes: &mut [PinNode],
) -> Result<(), ProtocolError> {
    // Only the ids storage typed as Goals are candidates. Before the pin
    // columns were split every reference had to be probed against the Goal
    // spine because the stored array could not say; now a page whose nodes
    // carry no Goal reference issues no query at all.
    let mut candidates = Vec::new();
    for node in nodes.iter() {
        for goal in node.goal_refs.iter().chain(&node.guided_by) {
            if !candidates.contains(goal) {
                candidates.push(*goal);
            }
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let visible = memory_read
        .load_visible_goal_ids(owner_scope, read_owners, &candidates)
        .await
        .map_err(|err| internal_storage_error("load_visible_goal_ids", &err))?;
    for node in nodes {
        node.resolve_visible_goal_refs(&visible);
    }
    Ok(())
}

fn expand_hops<'a>(nodes: impl IntoIterator<Item = &'a PinNode>) -> Vec<PinHop> {
    let mut hops = Vec::new();
    for node in nodes {
        hops.extend(node.pins().map(|(target_id, kind)| PinHop {
            source_id: node.id,
            source_kind: node.kind,
            target: PinTarget::Memory(target_id),
            kind,
        }));
        hops.extend(node.goal_pins().map(|(target_id, kind)| PinHop {
            source_id: node.id,
            source_kind: node.kind,
            target: PinTarget::Goal(target_id),
            kind,
        }));
    }
    hops
}

fn hop_key(hop: &PinHop) -> HopKey {
    (
        pin_created_at(hop.source_id),
        hop.source_id.into_inner(),
        hop.target.id(),
        hop.kind.as_str(),
    )
}

fn cursor_key(cursor: Option<EdgeReadCursor>) -> Option<HopKey> {
    let cursor = cursor?;
    Some((
        cursor.created_at,
        memory_ref(Some(cursor.source))?.into_inner(),
        entity_id(Some(cursor.target))?,
        cursor.kind.as_str(),
    ))
}

/// What the reader may see of a page's memory targets: the live readable
/// ones by kind, and what it may know of the rest.
#[derive(Debug, Default)]
struct Seen {
    live: HashMap<MemoryId, EntityKind>,
    unread: UnreadTargets,
}

fn page_hops(
    mut hops: Vec<PinHop>,
    cursor: Option<EdgeReadCursor>,
    limit: u32,
    visible: &Seen,
) -> EdgeReadResponse {
    hops.sort_unstable_by(|left, right| hop_key(right).cmp(&hop_key(left)));
    if let Some(cursor) = cursor_key(cursor) {
        hops.retain(|hop| hop_key(hop) < cursor);
    }
    let page_len = usize::try_from(limit).unwrap_or(usize::MAX);
    let truncated = hops.len() > page_len;
    hops.truncate(page_len);
    let edges: Vec<Edge> = hops
        .iter()
        .map(|hop| project_hop_edge(hop, visible))
        .collect();
    let next_cursor = truncated.then(|| {
        let last = hops.last().expect("truncated page is non-empty");
        EdgeReadCursor {
            created_at: pin_created_at(last.source_id),
            source: EntityRef::Memory(last.source_id),
            target: last.target.entity(),
            kind: last.kind,
        }
    });
    EdgeReadResponse { edges, next_cursor }
}

fn project_hop_edge(hop: &PinHop, visible: &Seen) -> Edge {
    match hop.target {
        PinTarget::Memory(target) => project_listed_edge(
            hop.source_kind,
            hop.source_id,
            target,
            hop.kind,
            &visible.live,
            &visible.unread,
        ),
        PinTarget::Goal(target) => Edge {
            source: EdgeEndpoint::memory(hop.source_kind, hop.source_id),
            target: EdgeTargetProjection::visible(EdgeEndpoint::goal(target)),
            kind: hop.kind,
            created_at: pin_created_at(hop.source_id),
        },
    }
}

/// The live readable rows among `ids`, then — for the rest only — what
/// the reader may know of them (forgotten or erased).
async fn load_visible(
    memory_read: &MemoryReadHandle,
    owner_scope: Option<&OwnerScope>,
    read_owners: &[OwnerId],
    ids: &[MemoryId],
) -> Result<Seen, ProtocolError> {
    if ids.is_empty() {
        return Ok(Seen::default());
    }
    let nodes = memory_read
        .load_pin_nodes(owner_scope, read_owners, ids)
        .await
        .map_err(|err| internal_storage_error("load_pin_nodes", &err))?;
    let live: HashMap<MemoryId, EntityKind> =
        nodes.into_iter().map(|node| (node.id, node.kind)).collect();
    let rest: Vec<MemoryId> = ids
        .iter()
        .copied()
        .filter(|id| !live.contains_key(id))
        .collect();
    let unread = if rest.is_empty() {
        UnreadTargets::default()
    } else {
        memory_read
            .unread_targets(owner_scope, read_owners, &rest)
            .await
            .map_err(|err| internal_storage_error("unread_targets", &err))?
    };
    Ok(Seen { live, unread })
}

/// Source and/or inbound pin nodes, then project a newest-first page.
pub(in crate::engine) async fn read_edges_from_nodes(
    memory_read: &MemoryReadHandle,
    owner_scope: Option<&OwnerScope>,
    read_owners: &[OwnerId],
    req: &EdgeReadRequest,
) -> Result<EdgeReadResponse, ProtocolError> {
    if matches!(req.filter.source, Some(EntityRef::Goal(_))) {
        return Ok(empty_read());
    }
    let source_id = memory_ref(req.filter.source);
    let target_id = entity_id(req.filter.target);
    if source_id.is_none() && target_id.is_none() {
        return Ok(empty_read());
    }

    if req.limit == 0 {
        return Ok(empty_read());
    }

    let sources = if let Some(source) = source_id {
        let mut sources = memory_read
            .load_pin_nodes(owner_scope, read_owners, &[source])
            .await
            .map_err(|err| internal_storage_error("load_pin_nodes", &err))?;
        resolve_visible_goal_refs(memory_read, owner_scope, read_owners, &mut sources).await?;
        sources
    } else if let Some(target) = target_id {
        load_incoming_sources(
            memory_read,
            owner_scope,
            read_owners,
            MemoryId::new(target),
            req.filter
                .target
                .expect("target id came from target filter"),
            req,
        )
        .await?
    } else {
        Vec::new()
    };

    let hops = matching_hops(&sources, source_id, req.filter.target, req.filter.kind);

    let mut want: Vec<MemoryId> = hops
        .iter()
        .filter_map(|hop| match hop.target {
            PinTarget::Memory(id) => Some(id),
            PinTarget::Goal(_) => None,
        })
        .collect();
    want.sort_unstable();
    want.dedup();
    let visible = load_visible(memory_read, owner_scope, read_owners, &want).await?;
    Ok(page_hops(hops, req.cursor, req.limit, &visible))
}

pub(in crate::engine) async fn edge_exists_from_nodes(
    memory_read: &MemoryReadHandle,
    owner_scope: Option<&OwnerScope>,
    read_owners: &[OwnerId],
    req: &EdgeExistsRequest,
) -> Result<EdgeExistsResponse, ProtocolError> {
    let read = EdgeReadRequest {
        filter: req.filter.clone(),
        limit: 1,
        cursor: None,
    };
    let page = read_edges_from_nodes(memory_read, owner_scope, read_owners, &read).await?;
    Ok(EdgeExistsResponse {
        exists: !page.edges.is_empty(),
    })
}

fn inbound_source_page_limit(req: &EdgeReadRequest) -> u32 {
    if req.limit == 1 && req.cursor.is_none() {
        2
    } else {
        req.limit.saturating_add(1).max(INBOUND_SOURCE_PAGE_MIN)
    }
}

fn hops_after_cursor(hops: &[PinHop], cursor: Option<EdgeReadCursor>) -> usize {
    match cursor_key(cursor) {
        None => hops.len(),
        Some(cut) => hops.iter().filter(|hop| hop_key(hop) < cut).count(),
    }
}

fn matching_hops(
    sources: &[PinNode],
    source_id: Option<MemoryId>,
    target: Option<EntityRef>,
    kind: Option<EdgeKind>,
) -> Vec<PinHop> {
    expand_hops(sources)
        .into_iter()
        .filter(|hop| source_id.is_none_or(|src| hop.source_id == src))
        .filter(|hop| target.is_none_or(|target| hop.target.entity() == target))
        .filter(|hop| kind.is_none_or(|want| hop.kind == want))
        .collect()
}

async fn load_incoming_sources(
    memory_read: &MemoryReadHandle,
    owner_scope: Option<&OwnerScope>,
    read_owners: &[OwnerId],
    target: MemoryId,
    target_filter: EntityRef,
    req: &EdgeReadRequest,
) -> Result<Vec<PinNode>, ProtocolError> {
    let page_limit = inbound_source_page_limit(req);
    let hop_limit = usize::try_from(req.limit).unwrap_or(usize::MAX);
    let mut sources = Vec::new();
    let mut after = None;
    if let Some(cursor) = req.cursor
        && let Some(src) = memory_ref(Some(cursor.source))
    {
        let mut cursor_sources = memory_read
            .load_pin_nodes(owner_scope, read_owners, &[src])
            .await
            .map_err(|err| internal_storage_error("load_pin_nodes", &err))?;
        resolve_visible_goal_refs(memory_read, owner_scope, read_owners, &mut cursor_sources)
            .await?;
        sources.extend(cursor_sources);
        after = Some(src);
    }
    let targets = [target];
    loop {
        let page = memory_read
            .load_inbound_pin_nodes(
                owner_scope,
                read_owners,
                InboundPinQuery {
                    targets: &targets,
                    // The inbound target carries its own kind, and a Goal is
                    // only ever pinned from `goal_refs` or `guided_by`. Naming the columns
                    // keeps the scan on one GIN index instead of a mixed array.
                    target_kind: if matches!(target_filter, EntityRef::Goal(_)) {
                        PinTargetKind::Goal
                    } else {
                        PinTargetKind::Memory
                    },
                    kind: req.filter.kind,
                    supersession: SupersessionStatus::IncludeSuperseded,
                    after,
                    limit: page_limit,
                },
            )
            .await
            .map_err(|err| internal_storage_error("load_inbound_pin_nodes", &err))?;
        let short = page.len() < usize::try_from(page_limit).unwrap_or(usize::MAX);
        let mut page = page;
        resolve_visible_goal_refs(memory_read, owner_scope, read_owners, &mut page).await?;
        if let Some(last) = page.last() {
            after = Some(last.id);
        }
        sources.extend(page);
        let hops = matching_hops(&sources, None, Some(target_filter), req.filter.kind);
        // `page_hops` needs an extra matching hop to emit a continuation.
        // A full page alone cannot distinguish exhaustion from more sources.
        if hops_after_cursor(&hops, req.cursor) > hop_limit || short {
            break;
        }
    }
    Ok(sources)
}

/// Requested rows plus a newest-first **current-head** inbound sample.
/// Not a complete star — `read_edges` incoming is the complete path.
pub(in crate::engine) async fn neighbor_edges_from_nodes(
    memory_read: &MemoryReadHandle,
    owner_scope: Option<&OwnerScope>,
    read_owners: &[OwnerId],
    memory_ids: &[MemoryId],
    limit: usize,
) -> Result<Vec<Edge>, ProtocolError> {
    if memory_ids.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut requested = memory_read
        .load_pin_nodes(owner_scope, read_owners, memory_ids)
        .await
        .map_err(|err| internal_storage_error("load_pin_nodes", &err))?;
    resolve_visible_goal_refs(memory_read, owner_scope, read_owners, &mut requested).await?;
    let inbound_limit = u32::try_from(limit).unwrap_or(u32::MAX);
    let mut inbound = memory_read
        .load_inbound_pin_nodes(
            owner_scope,
            read_owners,
            InboundPinQuery {
                targets: memory_ids,
                target_kind: PinTargetKind::Memory,
                kind: None,
                supersession: SupersessionStatus::HeadsOnly,
                after: None,
                limit: inbound_limit,
            },
        )
        .await
        .map_err(|err| internal_storage_error("load_inbound_pin_nodes", &err))?;
    resolve_visible_goal_refs(memory_read, owner_scope, read_owners, &mut inbound).await?;

    let mut by_id: HashMap<MemoryId, PinNode> = HashMap::new();
    for node in requested.into_iter().chain(inbound) {
        by_id.entry(node.id).or_insert(node);
    }
    let requested_ids: HashSet<MemoryId> = memory_ids.iter().copied().collect();
    let mut hops: Vec<PinHop> = expand_hops(by_id.values())
        .into_iter()
        .filter(|hop| {
            requested_ids.contains(&hop.source_id)
                || matches!(hop.target, PinTarget::Memory(id) if requested_ids.contains(&id))
        })
        .collect();

    let missing: Vec<MemoryId> = {
        let mut ids: Vec<MemoryId> = hops
            .iter()
            .filter_map(|hop| match hop.target {
                PinTarget::Memory(id) => Some(id),
                PinTarget::Goal(_) => None,
            })
            .filter(|id| !by_id.contains_key(id))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let mut visible = load_visible(memory_read, owner_scope, read_owners, &missing).await?;
    visible
        .live
        .extend(by_id.iter().map(|(id, node)| (*id, node.kind)));

    hops.sort_unstable_by(|left, right| hop_key(right).cmp(&hop_key(left)));
    hops.truncate(limit);
    Ok(hops
        .iter()
        .map(|hop| project_hop_edge(hop, &visible))
        .collect())
}

#[cfg(test)]
#[path = "pin_read_tests.rs"]
mod tests;
