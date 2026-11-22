//! A bounded, bidirectional read of the declarations around a Goal.

use std::collections::{HashMap, HashSet, VecDeque};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::catalog::cursor as wire_cursor;
use crate::storage_ports::other_stores::UnavailableReason;
use crate::verbs::query::{QueryRequest, SurroundingPin};
use crate::{EntityKind, EntityRef, ToolCtx, ToolError};

use super::ancestry::{self, Unread};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalSurroundingsArgs {
    /// A Goal version or series UUID, resolved to its current head.
    pub goal: String,
    /// Maximum distance from the Goal, in either direction, from 1 through 8.
    #[serde(default = "default_depth")]
    pub depth: u8,
    /// Declared pins per page, including pins to anonymous unreadable leaves.
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// Continue the same Goal and depth. Pages are reads, not a stored snapshot.
    #[serde(default)]
    pub cursor: Option<String>,
}

fn default_depth() -> u8 {
    3
}
fn default_limit() -> u32 {
    super::DEFAULT_PAGE_LIMIT
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SurroundingsNode {
    pub id: Uuid,
    pub kind: String,
    pub store: u16,
    pub owner: String,
    pub distance: u8,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SurroundingsEdge {
    pub source: Uuid,
    pub target: Uuid,
    /// The declaring pin field; no new relation is stored by this read.
    pub field: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct UnansweredSurroundings {
    pub store: u16,
    pub reason: UnavailableReason,
    pub count: u32,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GoalSurroundingsOutput {
    pub goal: Uuid,
    pub nodes: Vec<SurroundingsNode>,
    pub edges: Vec<SurroundingsEdge>,
    /// Distinct unreadable leaves on this page. Their ids, owners and kinds
    /// appear nowhere in the answer, including its continuation cursor.
    pub unreadable: u32,
    pub unanswered: Vec<UnansweredSurroundings>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

/// Read declared pins around the Goal in both directions, with remote leaves
/// admitted as the caller and unreadable identities kept out of the answer.
///
/// # Errors
/// Returns malformed selectors, absent Goals, authorization or storage failures.
pub async fn goal_surroundings(
    ctx: ToolCtx,
    args: GoalSurroundingsArgs,
) -> Result<GoalSurroundingsOutput, ToolError> {
    surroundings(&ctx, args).await
}

const CURSOR: wire_cursor::FingerprintedCursor = wire_cursor::FingerprintedCursor {
    version: 1,
    source: "Goal surroundings page",
    rebind_hint: "repeat the Goal and depth that produced it",
};

fn endpoint_id(endpoint: EntityRef) -> Uuid {
    match endpoint {
        EntityRef::Memory(id) => id.into_inner(),
        EntityRef::Goal(id) => id.into_inner(),
    }
}

fn remote(ctx: &ToolCtx, endpoint: EntityRef) -> bool {
    matches!(endpoint, EntityRef::Memory(id) if crate::memory_store_number(id.into_inner()) != ctx.engine().store_number())
}

type Nodes = HashMap<EntityRef, Option<SurroundingsNode>>;

/// Hydrate selectors through the same authorized query as ordinary reads.
/// A missing local endpoint is an anonymous leaf, never a frontier member.
async fn load_nodes(
    ctx: &ToolCtx,
    nodes: &mut Nodes,
    endpoints: &[EntityRef],
) -> Result<(), ToolError> {
    let missing: HashSet<_> = endpoints
        .iter()
        .copied()
        .filter(|endpoint| !remote(ctx, *endpoint) && !nodes.contains_key(endpoint))
        .collect();
    let mut memories = QueryRequest::readable();
    let mut goals = QueryRequest::readable();
    for endpoint in &missing {
        match endpoint {
            EntityRef::Goal(id) => goals.goal_ids.push(*id),
            EntityRef::Memory(id) => memories.memory_ids.push(*id),
        }
    }
    for mut request in [memories, goals] {
        request.include_payloads = false;
        if request.memory_ids.is_empty() && request.goal_ids.is_empty() {
            continue;
        }
        // Id hydration is exact; include every admitted version, not only heads.
        request.limit = u32::try_from(missing.len()).unwrap_or(u32::MAX);
        request.supersession = crate::query::SupersessionStatus::IncludeSuperseded;
        let answer = ctx.engine().query(ctx.authz(), &request).await?;
        for row in answer.memories {
            nodes.insert(
                EntityRef::Memory(row.id),
                Some(SurroundingsNode {
                    id: row.id.into_inner(),
                    kind: row.kind.as_str().into(),
                    store: ctx.engine().store_number(),
                    owner: row.owner.to_string(),
                    distance: 0,
                }),
            );
        }
        for row in answer.goals {
            nodes.insert(
                EntityRef::Goal(row.id),
                Some(SurroundingsNode {
                    id: row.id.into_inner(),
                    kind: EntityKind::Goal.as_str().into(),
                    store: ctx.engine().store_number(),
                    owner: row.owner.to_string(),
                    distance: 0,
                }),
            );
        }
    }
    for endpoint in missing {
        nodes.entry(endpoint).or_insert(None);
    }
    Ok(())
}

struct WalkPage {
    pins: Vec<SurroundingPin>,
    nodes: Nodes,
    distances: HashMap<EntityRef, u8>,
    has_more: bool,
}

/// Replay deterministic breadth-first declarations through the requested
/// page plus one pin. Only the numeric consumed-pin count is put in the
/// cursor: even a page ending on an unreadable pin cannot reveal its id.
async fn walk(
    ctx: &ToolCtx,
    root: EntityRef,
    depth: u8,
    skip: u32,
    limit: u32,
) -> Result<WalkPage, ToolError> {
    let mut nodes = Nodes::new();
    load_nodes(ctx, &mut nodes, &[root]).await?;
    if !matches!(nodes.get(&root), Some(Some(_))) {
        return Err(ToolError::NotFound("goal not found".into()));
    }
    let mut distances = HashMap::from([(root, 0)]);
    let mut queue = VecDeque::from([(root, 0)]);
    let mut seen = HashSet::new();
    let mut consumed = 0_u32;
    let mut pins = Vec::new();
    let mut has_more = false;
    'walk: while let Some((endpoint, distance)) = queue.pop_front() {
        if distance == depth {
            continue;
        }
        let mut offset = 0;
        loop {
            let batch = ctx
                .engine()
                .surrounding_pins(ctx.authz(), endpoint, offset, limit)
                .await?;
            let count = u32::try_from(batch.len()).unwrap_or(u32::MAX);
            let endpoints: Vec<_> = batch
                .iter()
                .flat_map(|pin| [pin.source, pin.target])
                .collect();
            load_nodes(ctx, &mut nodes, &endpoints).await?;
            for pin in batch {
                if !seen.insert(pin.clone()) {
                    continue;
                }
                for other in [pin.source, pin.target] {
                    if let std::collections::hash_map::Entry::Vacant(entry) = distances.entry(other)
                    {
                        entry.insert(distance + 1);
                        if matches!(nodes.get(&other), Some(Some(_))) {
                            queue.push_back((other, distance + 1));
                        }
                    }
                }
                if consumed >= skip {
                    if pins.len() == limit as usize {
                        has_more = true;
                        break 'walk;
                    }
                    pins.push(pin);
                }
                consumed = consumed.checked_add(1).ok_or_else(|| {
                    ToolError::InvalidInput("surroundings cursor exhausted".into())
                })?;
            }
            if count < limit {
                break;
            }
            offset = offset
                .checked_add(count)
                .ok_or_else(|| ToolError::InvalidInput("surroundings star exhausted".into()))?;
        }
    }
    Ok(WalkPage {
        pins,
        nodes,
        distances,
        has_more,
    })
}

async fn surroundings(
    ctx: &ToolCtx,
    args: GoalSurroundingsArgs,
) -> Result<GoalSurroundingsOutput, ToolError> {
    if !(1..=8).contains(&args.depth) {
        return Err(ToolError::InvalidInput(
            "depth must be between 1 and 8".into(),
        ));
    }
    // One pin can introduce at most one remote leaf: its source is local.
    // Keep the page inside the existing call-wide remote read budget.
    let remote_limit =
        u32::try_from(crate::storage_ports::other_stores::MAX_LINEAGE_REMOTE_ORIGINS)
            .expect("the remote origin budget fits a page limit");
    let limit = super::resolve_page_limit(Some(args.limit))?.min(remote_limit);
    // `Proxiima.Goals.readRevision`.
    let goal = super::goal_reads::get_goal(ctx.clone(), &args.goal).await?;
    let root = EntityRef::Goal(ctx.resolve_goal(&goal.goal)?);
    let root_id = endpoint_id(root);
    let fingerprint = wire_cursor::fingerprint(&format!("{root_id}:{}", args.depth));
    let skip = args
        .cursor
        .as_deref()
        .map(|raw| CURSOR.decode::<u32>(&fingerprint, raw))
        .transpose()?
        .unwrap_or(0);
    let page = walk(ctx, root, args.depth, skip, limit).await?;
    let next_cursor = if page.has_more {
        let next = skip
            .checked_add(limit)
            .ok_or_else(|| ToolError::InvalidInput("surroundings cursor exhausted".into()))?;
        Some(CURSOR.encode(&fingerprint, &next))
    } else {
        None
    };
    project(ctx, root, page, next_cursor).await
}

async fn project(
    ctx: &ToolCtx,
    root: EntityRef,
    mut page: WalkPage,
    next_cursor: Option<String>,
) -> Result<GoalSurroundingsOutput, ToolError> {
    let endpoints: HashSet<_> = page
        .pins
        .iter()
        .flat_map(|pin| [pin.source, pin.target])
        .chain([root])
        .collect();
    let mut remote_ids: Vec<_> = endpoints
        .iter()
        .copied()
        .filter(|endpoint| remote(ctx, *endpoint))
        .map(endpoint_id)
        .collect();
    remote_ids.sort_unstable();
    let mut remote_nodes = ancestry::read_remote_targets(ctx, &remote_ids).await;
    let mut unreadable = 0;
    let mut unanswered: Vec<UnansweredSurroundings> = Vec::new();
    let mut admitted = HashMap::new();
    for endpoint in endpoints {
        let id = endpoint_id(endpoint);
        let distance = page.distances[&endpoint];
        let node = if remote(ctx, endpoint) {
            let read = remote_nodes
                .remove(&id)
                .expect("every requested remote target has a projection");
            match read.unread {
                None => read.space.map(|owner| SurroundingsNode {
                    id,
                    kind: read.kind,
                    owner,
                    store: crate::memory_store_number(id),
                    distance,
                }),
                Some(Unread::Erased | Unread::Unavailable(UnavailableReason::NotReadable)) => None,
                Some(Unread::Unavailable(reason)) => {
                    let store = crate::memory_store_number(id);
                    if let Some(count) = unanswered
                        .iter_mut()
                        .find(|item| item.store == store && item.reason == reason)
                    {
                        count.count += 1;
                    } else {
                        unanswered.push(UnansweredSurroundings {
                            store,
                            reason,
                            count: 1,
                        });
                    }
                    continue;
                }
            }
        } else {
            page.nodes.remove(&endpoint).flatten().map(|mut node| {
                node.distance = distance;
                node
            })
        };
        if let Some(node) = node {
            admitted.insert(endpoint, node);
        } else {
            unreadable += 1;
        }
    }
    let edges = page
        .pins
        .into_iter()
        .filter(|pin| admitted.contains_key(&pin.source) && admitted.contains_key(&pin.target))
        .map(|pin| SurroundingsEdge {
            source: endpoint_id(pin.source),
            target: endpoint_id(pin.target),
            field: pin.field,
        })
        .collect();
    let mut nodes: Vec<_> = admitted.into_values().collect();
    nodes.sort_by_key(|node| (node.distance, node.id));
    unanswered.sort_by_key(|item| item.store);
    Ok(GoalSurroundingsOutput {
        goal: endpoint_id(root),
        nodes,
        edges,
        unreadable,
        unanswered,
        has_more: next_cursor.is_some(),
        next_cursor,
    })
}
