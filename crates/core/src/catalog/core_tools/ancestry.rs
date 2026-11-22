//! Shared ancestor traversal for Think and memory lineage.

use std::collections::HashMap;

use uuid::Uuid;

use crate::spiice::Provenance;
use crate::storage_ports::other_stores::{
    MAX_LINEAGE_REMOTE_ORIGINS, NodeStatus, OtherStoresError, RemoteOriginRead, UnavailableReason,
};
use crate::{EdgeKind, GetMemoriesReadRequest, MemoryId};
use crate::{ToolCtx, ToolError};

/// Where an ancestor edge lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AncestorTarget {
    /// A memory of this store; the walk may expand it.
    Local(MemoryId),
    /// Another store's memory; a leaf.
    Remote(Uuid),
}

impl AncestorTarget {
    pub(crate) fn uuid(self) -> Uuid {
        match self {
            Self::Local(id) => id.into_inner(),
            Self::Remote(id) => id,
        }
    }
}

/// One ancestor edge: `source` was made from, or is about, `target`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AncestorEdge {
    pub(crate) source: MemoryId,
    pub(crate) target: AncestorTarget,
    pub(crate) kind: EdgeKind,
}

/// The ancestor edges of every `frontier` node the caller can read, newest
/// `(source, target)` first.
///
/// # Errors
///
/// Storage failures of the pin, payload, or external-edge reads.
pub(crate) async fn ancestor_edges(
    engine: &crate::Engine,
    ctx: &ToolCtx,
    frontier: &[MemoryId],
) -> Result<Vec<AncestorEdge>, ToolError> {
    if frontier.is_empty() {
        return Ok(Vec::new());
    }
    let pins = engine.pin_nodes(ctx.authz(), frontier).await?;
    let mut edges = Vec::new();
    let mut grounded: Vec<(MemoryId, &'static [&'static str])> = Vec::new();
    let mut visible = Vec::with_capacity(pins.len());
    let mut reads = Vec::new();
    // `Proxiima.Edges.localAncestorKinds`.
    for node in pins {
        visible.push(node.id);
        reads.extend(node.reads.iter().map(|read| AncestorEdge {
            source: node.id,
            target: AncestorTarget::Local(*read),
            kind: EdgeKind::Read,
        }));
        match ctx.registry().provenance_of(&node.schema_id, node.kind) {
            None | Some(Provenance::OriginEdges) => {
                edges.extend(node.origins.into_iter().map(|origin| AncestorEdge {
                    source: node.id,
                    target: AncestorTarget::Local(origin),
                    kind: EdgeKind::Origin,
                }));
            }
            Some(Provenance::PayloadOnly { subject_columns }) => {
                grounded.push((node.id, subject_columns));
            }
            Some(Provenance::None) => {}
        }
    }
    edges.extend(payload_subjects(engine, ctx, &grounded).await?);
    edges.extend(
        engine
            .external_edges(ctx.authz(), &visible)
            .await?
            .into_iter()
            .map(|(source, target, kind)| AncestorEdge {
                source,
                target: AncestorTarget::Remote(target),
                kind,
            }),
    );
    // Append reads last so the stable sort gives declared pins precedence.
    edges.extend(reads);
    edges.sort_by_key(|edge| std::cmp::Reverse((edge.source, edge.target.uuid())));
    edges.dedup_by_key(|edge| (edge.source, edge.target));
    Ok(edges)
}

/// The subjects `PayloadOnly` nodes ground in, read through the payload
/// fields their schemas declare, as `Reference` edges.
///
/// `get_memories` is the same owner-scoped read `core_get_memories` uses,
/// so a subject the caller may not see is not returned and the walk simply
/// does not reach it — the ordinary shape of an absent node, not a redaction.
async fn payload_subjects(
    engine: &crate::Engine,
    ctx: &ToolCtx,
    grounded: &[(MemoryId, &'static [&'static str])],
) -> Result<Vec<AncestorEdge>, ToolError> {
    if grounded.is_empty() {
        return Ok(Vec::new());
    }
    let response = engine
        .get_memories(
            ctx.authz(),
            &GetMemoriesReadRequest {
                memory_ids: grounded.iter().map(|(id, _)| *id).collect(),
            },
        )
        .await?;
    let declared: HashMap<MemoryId, &'static [&'static str]> = grounded.iter().copied().collect();
    let mut subjects = Vec::new();
    for snapshot in response.memories {
        let (Some(columns), Some(payload)) =
            (declared.get(&snapshot.memory_id), snapshot.payload.as_ref())
        else {
            continue;
        };
        for reference in payload.references() {
            if columns.contains(&reference.field)
                && let Some(id) = reference.target.memory_id()
            {
                subjects.push(AncestorEdge {
                    source: snapshot.memory_id,
                    target: AncestorTarget::Local(id),
                    kind: EdgeKind::Reference,
                });
            }
        }
    }
    Ok(subjects)
}

/// An other-store memory a walk could not read: only `Unavailable` has a
/// reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unread {
    Erased,
    Unavailable(UnavailableReason),
}

impl Unread {
    pub(crate) fn status(self) -> NodeStatus {
        match self {
            Self::Erased => NodeStatus::Erased,
            Self::Unavailable(_) => NodeStatus::Unavailable,
        }
    }

    pub(crate) fn reason(self) -> Option<UnavailableReason> {
        match self {
            Self::Erased => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }
}

/// How an other-store memory renders on a walk: from the read, or empty
/// with the reason it could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteProjection {
    /// The owner's UUID as the store reported it; `None` when it did not answer with the memory.
    pub(crate) space: Option<String>,
    pub(crate) kind: String,
    pub(crate) schema_id: String,
    pub(crate) snippet: String,
    pub(crate) unread: Option<Unread>,
}

/// Read `handles` (distinct, in walk order) through the port as the caller,
/// at most [`MAX_LINEAGE_REMOTE_ORIGINS`] of them under one call-wide
/// deadline; the rest render as `origin limit exceeded`.
pub(crate) async fn read_remote_targets(
    ctx: &ToolCtx,
    handles: &[Uuid],
) -> HashMap<Uuid, RemoteProjection> {
    let service = super::other_stores::service(ctx);
    let has_service = service.is_some();
    let readable = handles.len().min(MAX_LINEAGE_REMOTE_ORIGINS);
    let reads = match service {
        Some(service) if readable > 0 => Some(
            service
                .read_origins_bounded(super::other_stores::caller(ctx), &handles[..readable])
                .await,
        ),
        _ => None,
    };
    // No port wired: every target is on a store this deployment does not know.
    let reads = reads.unwrap_or_else(|| {
        handles[..readable]
            .iter()
            .map(|handle| {
                Err(OtherStoresError::UnknownStore(crate::memory_store_number(
                    *handle,
                )))
            })
            .collect()
    });
    let mut read_by_handle: HashMap<Uuid, _> =
        handles.iter().take(readable).copied().zip(reads).collect();
    handles
        .iter()
        .map(|handle| {
            let read = read_by_handle.remove(handle);
            let capped = has_service && read.is_none() && handles.len() > readable;
            (*handle, project_remote_read(read.as_ref(), capped))
        })
        .collect()
}

pub(crate) fn project_remote_read(
    read: Option<&Result<RemoteOriginRead, OtherStoresError>>,
    capped: bool,
) -> RemoteProjection {
    const SNIPPET_CHARS: usize = crate::ledger!(Proxiima.Edges.maxRemoteSketchChars);
    let unread = |unread| RemoteProjection {
        space: None,
        kind: String::new(),
        schema_id: String::new(),
        snippet: String::new(),
        unread: Some(unread),
    };
    let unavailable = |reason| unread(Unread::Unavailable(reason));
    match read {
        Some(Ok(RemoteOriginRead::Found(_, memory))) => RemoteProjection {
            space: Some(memory.owner.to_string()),
            kind: memory.kind.as_str().to_string(),
            schema_id: memory.schema_id.clone(),
            snippet: memory.body.chars().take(SNIPPET_CHARS).collect(),
            unread: None,
        },
        Some(Ok(RemoteOriginRead::Erased)) => unread(Unread::Erased),
        Some(Err(OtherStoresError::DeadlineExceeded)) => {
            unavailable(UnavailableReason::DeadlineExceeded)
        }
        _ if capped => unavailable(UnavailableReason::OriginLimitExceeded),
        // The store answered that the caller cannot read it, or there is no
        // such store (also: no other store wired).
        Some(Ok(RemoteOriginRead::NotFound) | Err(OtherStoresError::UnknownStore(_))) | None => {
            unavailable(UnavailableReason::NotReadable)
        }
        // The store failed or answered for another memory: retry later.
        Some(Err(_)) => unavailable(UnavailableReason::StoreUnavailable),
    }
}
