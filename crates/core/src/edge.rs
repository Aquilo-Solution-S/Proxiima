//! Read projections of the pins carried by exact version rows.

use crate::entity::EntityRef;
use crate::{EntityKind, GoalId, MemoryId, MemoryKind, OwnerId, SchemaId};

/// The projection kind assigned by the declaring field or operation.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum EdgeKind {
    /// A memory declares what it was made from. Written only by a node
    /// write carrying a derivation declaration (`derived_from`), in that
    /// write's own transaction.
    Origin,
    /// A payload field points at another node. Derived at ingest from the
    /// schema-declared reference fields of the node's own payload.
    Reference,
    /// The writing session read the target before this write. Written only
    /// from the host's [`ReadSet`], never from tool arguments. Context, not
    /// grounding: it never counts as an origin or a reference, may sit on a
    /// Fact, and may point at any layer.
    Read,
}

impl EdgeKind {
    /// `Proxiima.Edges.pinOrder`.
    pub const ALL: [Self; 3] = [Self::Origin, Self::Reference, Self::Read];

    /// The kind a wire label names; the inverse of [`Self::as_str`].
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == label)
    }

    /// Wire label. Stable contract — also the label
    /// `proxiima_core.memory_external_origin.edge_kind` stores for an
    /// other-store pin.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        // `Proxiima.Edges.pinLabel`.
        match self {
            Self::Origin => "origin",
            Self::Reference => "reference",
            Self::Read => "read",
        }
    }
}

/// One memory the host certifies the writing session read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReadPin {
    /// A memory of this store, read in `space`. Storage checks, inside the
    /// write's transaction, that it is live and readable there.
    Local { memory: MemoryId, space: OwnerId },
    /// A memory of another store; its id carries the store number. Its
    /// space is that store's to judge: an MCP write reads it through the
    /// other-stores port as the caller, as a cross-store derive reads its
    /// sources, and storage refuses a write naming one nobody read.
    OtherStore { memory: MemoryId },
}

impl ReadPin {
    /// The memory this pin names.
    #[must_use]
    pub const fn memory(&self) -> MemoryId {
        match *self {
            Self::Local { memory, .. } | Self::OtherStore { memory } => memory,
        }
    }
}

/// The host-certified exact session observations for an authorized write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadSet(Vec<ReadPin>);

/// Why a [`ReadSet`] could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReadSetError {
    /// The same memory was named in two different spaces, or both as a
    /// local and as an other-store memory.
    #[error("read memory {} named in two spaces", memory.into_inner())]
    ConflictingSpace { memory: MemoryId },
    /// More distinct memories than [`ReadSet::MAX_PINS`].
    #[error(
        "read set names {count} memories; at most {} are kept",
        ReadSet::MAX_PINS
    )]
    TooMany { count: usize },
}

impl ReadSet {
    /// The most memories one read set names. Every pin is probed inside the
    /// write's transaction and stored on the written row, so the set is
    /// bounded where it is built.
    pub const MAX_PINS: usize = crate::ledger!(Proxiima.Edges.maxReadPins);

    /// The empty read set: what a write no host session made carries.
    #[must_use]
    pub const fn new_empty() -> Self {
        Self(Vec::new())
    }

    /// The read set naming `pins`. A memory named twice in the same space is
    /// kept once.
    ///
    /// # Errors
    ///
    /// [`ReadSetError::ConflictingSpace`] when one memory is named in two
    /// spaces; [`ReadSetError::TooMany`] above [`Self::MAX_PINS`] distinct
    /// memories.
    pub fn new(pins: impl IntoIterator<Item = ReadPin>) -> Result<Self, ReadSetError> {
        // Normalize by exact Memory ID before checking for conflicting observations.
        let mut pins: Vec<ReadPin> = pins.into_iter().collect();
        pins.sort_unstable_by_key(|pin| (pin.memory(), *pin));
        pins.dedup();
        if let Some(pair) = pins
            .windows(2)
            .find(|pair| pair[0].memory() == pair[1].memory())
        {
            return Err(ReadSetError::ConflictingSpace {
                memory: pair[0].memory(),
            });
        }
        if pins.len() > Self::MAX_PINS {
            return Err(ReadSetError::TooMany { count: pins.len() });
        }
        Ok(Self(pins))
    }

    /// `self` and `other` together.
    ///
    /// # Errors
    ///
    /// [`ReadSetError::ConflictingSpace`] when the two name one memory in
    /// different spaces; [`ReadSetError::TooMany`] when together they pass
    /// [`Self::MAX_PINS`].
    pub fn merged(self, other: Self) -> Result<Self, ReadSetError> {
        Self::new(self.0.into_iter().chain(other.0))
    }

    #[must_use]
    pub fn pins(&self) -> &[ReadPin] {
        &self.0
    }

    /// The named memories of this store with their spaces, in order.
    pub fn local(&self) -> impl Iterator<Item = (MemoryId, OwnerId)> + '_ {
        self.0.iter().filter_map(|pin| match *pin {
            ReadPin::Local { memory, space } => Some((memory, space)),
            ReadPin::OtherStore { .. } => None,
        })
    }

    /// The named memories of other stores, in order.
    pub fn other_store(&self) -> impl Iterator<Item = MemoryId> + '_ {
        self.0.iter().filter_map(|pin| match *pin {
            ReadPin::Local { .. } => None,
            ReadPin::OtherStore { memory } => Some(memory),
        })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A typed address of one exact version for a graph pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "WireEndpoint", into = "WireEndpoint")]
pub enum EdgeEndpoint {
    Memory {
        kind: MemoryKind,
        memory_id: MemoryId,
    },
    Goal(GoalId),
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename = "EdgeEndpoint")]
struct WireEndpoint {
    kind: EntityKind,
    entity: EntityRef,
}

impl TryFrom<WireEndpoint> for EdgeEndpoint {
    type Error = &'static str;

    fn try_from(wire: WireEndpoint) -> Result<Self, Self::Error> {
        // `Proxiima.Identity.EmbeddableEntity`.
        match wire.entity {
            EntityRef::Memory(id) => MemoryKind::of(wire.kind)
                .map(|kind| Self::memory(kind, id))
                .ok_or("a Goal endpoint must address a Goal"),
            EntityRef::Goal(id) if wire.kind == EntityKind::Goal => Ok(Self::goal(id)),
            EntityRef::Goal(_) => Err("a memory endpoint must carry an F/A/P kind"),
        }
    }
}

impl From<EdgeEndpoint> for WireEndpoint {
    fn from(endpoint: EdgeEndpoint) -> Self {
        Self {
            kind: endpoint.kind(),
            entity: endpoint.entity(),
        }
    }
}

impl EdgeEndpoint {
    /// A pinned memory row.
    #[must_use]
    pub const fn memory(kind: MemoryKind, memory_id: MemoryId) -> Self {
        Self::Memory { kind, memory_id }
    }

    /// A pinned Goal row.
    #[must_use]
    pub const fn goal(goal_id: GoalId) -> Self {
        Self::Goal(goal_id)
    }

    /// The kind stored at this address.
    #[must_use]
    pub const fn kind(self) -> EntityKind {
        match self {
            Self::Memory { kind, .. } => kind.entity_kind(),
            Self::Goal(_) => EntityKind::Goal,
        }
    }

    /// The exact row address.
    #[must_use]
    pub const fn entity(self) -> EntityRef {
        match self {
            Self::Memory { memory_id, .. } => EntityRef::Memory(memory_id),
            Self::Goal(id) => EntityRef::Goal(id),
        }
    }

    /// The stable id stored in a pin column for this endpoint.
    #[must_use]
    pub const fn entity_id(self) -> uuid::Uuid {
        match self.entity() {
            EntityRef::Memory(memory_id) => memory_id.into_inner(),
            EntityRef::Goal(goal_id) => goal_id.into_inner(),
        }
    }

    /// The memory row this endpoint pins, if it pins one.
    #[must_use]
    pub const fn memory_id(self) -> Option<MemoryId> {
        match self {
            Self::Memory { memory_id, .. } => Some(memory_id),
            Self::Goal(_) => None,
        }
    }

    /// F/A/P layer index, or `None` for Goal endpoints, which are a
    /// separate entity axis (docs/concepts.md) and never enter the comparison.
    #[must_use]
    pub const fn layer(self) -> Option<u8> {
        match self {
            Self::Memory {
                kind: MemoryKind::Fact,
                ..
            } => Some(0),
            Self::Memory {
                kind: MemoryKind::Abstraction,
                ..
            } => Some(1),
            Self::Memory {
                kind: MemoryKind::Perspective,
                ..
            } => Some(2),
            Self::Goal(_) => None,
        }
    }
}

/// The read projection of a pin target, with an address only when visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EdgeTargetProjection {
    Visible { target: EdgeEndpoint },
    Redacted,
    Forgotten,
    Erased,
}

impl EdgeTargetProjection {
    #[must_use]
    pub const fn visible(target: EdgeEndpoint) -> Self {
        Self::Visible { target }
    }

    #[must_use]
    pub const fn endpoint(self) -> Option<EdgeEndpoint> {
        match self {
            Self::Visible { target } => Some(target),
            Self::Redacted | Self::Forgotten | Self::Erased => None,
        }
    }
}

/// A whole edge, as read. Four fields is the entire model — there is no
/// id, no relation, no namespace, no authorship column, no payload, no
/// citation and no status. A connection that needs to say more than
/// "these two, this way, since then" is a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub source: EdgeEndpoint,
    pub target: EdgeTargetProjection,
    pub kind: EdgeKind,
    pub created_at: time::OffsetDateTime,
}

/// What the targets of an inbound pin page are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinTargetKind {
    Memory,
    Goal,
}

/// A pin column of a memory row. The field fixes the pin's kind and the kinds
/// it may target; [`Self::spec`] is the one place that says so, and the
/// storage SQL is rendered from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(test, derive(serde::Deserialize), serde(rename_all = "camelCase"))]
pub enum MemoryPinField {
    Origins,
    Refs,
    GoalRefs,
    GuidedBy,
    Reads,
    ForkedFrom,
}

/// How a pin column stores its targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinShape {
    Array,
    Scalar,
}

/// One row of the pin-column table.
#[derive(Debug, Clone, Copy)]
pub struct PinFieldSpec {
    pub column: &'static str,
    pub kind: EdgeKind,
    pub shape: PinShape,
    /// Target kinds admitted from a Fact, an Abstraction and a Perspective, in
    /// that order.
    pub admits: [&'static [EntityKind]; 3],
}

impl MemoryPinField {
    /// Every field, in column order.
    pub const ALL: [Self; 6] = [
        Self::Origins,
        Self::Refs,
        Self::GoalRefs,
        Self::GuidedBy,
        Self::Reads,
        Self::ForkedFrom,
    ];

    /// `Proxiima.Edges.pinField`.
    #[must_use]
    pub const fn spec(self) -> PinFieldSpec {
        const NONE: &[EntityKind] = &[];
        const MEMORIES: &[EntityKind] = &[
            EntityKind::Fact,
            EntityKind::Abstraction,
            EntityKind::Perspective,
        ];
        const GOALS: &[EntityKind] = &[EntityKind::Goal];
        match self {
            Self::Origins => PinFieldSpec {
                column: "origins",
                kind: EdgeKind::Origin,
                shape: PinShape::Array,
                admits: [
                    NONE,
                    &[EntityKind::Fact, EntityKind::Abstraction],
                    &[EntityKind::Abstraction],
                ],
            },
            Self::Refs => PinFieldSpec {
                column: "refs",
                kind: EdgeKind::Reference,
                shape: PinShape::Array,
                admits: [MEMORIES; 3],
            },
            Self::GoalRefs => PinFieldSpec {
                column: "goal_refs",
                kind: EdgeKind::Reference,
                shape: PinShape::Array,
                admits: [GOALS; 3],
            },
            Self::GuidedBy => PinFieldSpec {
                column: "guided_by",
                kind: EdgeKind::Reference,
                shape: PinShape::Array,
                admits: [NONE, NONE, GOALS],
            },
            Self::Reads => PinFieldSpec {
                column: "reads",
                kind: EdgeKind::Read,
                shape: PinShape::Array,
                admits: [MEMORIES; 3],
            },
            Self::ForkedFrom => PinFieldSpec {
                column: "forked_from",
                kind: EdgeKind::Reference,
                shape: PinShape::Scalar,
                admits: [MEMORIES; 3],
            },
        }
    }

    /// The kinds a `source` row may pin through this field. A Goal is no
    /// memory row and admits nothing.
    #[must_use]
    pub const fn admitted(self, source: EntityKind) -> &'static [EntityKind] {
        let admits = self.spec().admits;
        match source {
            EntityKind::Fact => admits[0],
            EntityKind::Abstraction => admits[1],
            EntityKind::Perspective => admits[2],
            EntityKind::Goal => &[],
        }
    }

    /// Whether the field points at Goals or at Memories: a field admits one
    /// or the other, never both.
    #[must_use]
    pub const fn target(self) -> PinTargetKind {
        let admits = self.spec().admits;
        let mut source = 0;
        while source < admits.len() {
            let mut at = 0;
            while at < admits[source].len() {
                if matches!(admits[source][at], EntityKind::Goal) {
                    return PinTargetKind::Goal;
                }
                at += 1;
            }
            source += 1;
        }
        PinTargetKind::Memory
    }
}

/// The ids one field holds on a [`PinNode`], typed by what they target.
enum PinSlot<'a> {
    Memories(&'a [MemoryId]),
    Goals(&'a [GoalId]),
}

/// A memory row as a pin carrier. Storage returns these; [`Edge`] is a
/// view of `origins` / `refs` / `reads` built in the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinNode {
    pub id: MemoryId,
    pub kind: EntityKind,
    /// Which schema wrote this row, and therefore which `Provenance` the
    /// lineage walk must consult before treating `origins` as lineage.
    ///
    /// Carried on the node rather than looked up by the walk because the
    /// walk has no owner-scoped read of its own: the node is the only thing
    /// that crossed the authorization boundary.
    pub schema_id: SchemaId,
    pub origins: Vec<MemoryId>,
    /// Memory reference ids, plus any Goal reference the reader may not see.
    /// An unreadable target stays here so edge projection can redact it
    /// without disclosing whether it was a Memory or Goal.
    pub refs: Vec<MemoryId>,
    /// Goal references. Storage now stores these in their own column, so
    /// they arrive already typed; [`Self::resolve_visible_goal_refs`] then
    /// keeps only the ones this reader may see.
    pub goal_refs: Vec<GoalId>,
    /// Perspective guidance targets. These are projected as ordinary
    /// reference edges, but remain separate from payload-declared Goal refs.
    pub guided_by: Vec<GoalId>,
    /// The memories the host certified the writing session read (#677).
    pub reads: Vec<MemoryId>,
    /// The exact version this memory was forked from.
    pub forked_from: Option<MemoryId>,
}

impl PinNode {
    /// What this node holds in `field`. A fork source already listed in
    /// `refs` is not listed twice.
    fn slot(&self, field: MemoryPinField) -> PinSlot<'_> {
        match field {
            MemoryPinField::Origins => PinSlot::Memories(&self.origins),
            MemoryPinField::Refs => PinSlot::Memories(&self.refs),
            MemoryPinField::GoalRefs => PinSlot::Goals(&self.goal_refs),
            MemoryPinField::GuidedBy => PinSlot::Goals(&self.guided_by),
            MemoryPinField::Reads => PinSlot::Memories(&self.reads),
            MemoryPinField::ForkedFrom => match &self.forked_from {
                Some(source) if !self.refs.contains(source) => {
                    PinSlot::Memories(std::slice::from_ref(source))
                }
                _ => PinSlot::Memories(&[]),
            },
        }
    }

    /// The fields that target `target`, grouped by kind in [`EdgeKind::ALL`]
    /// order and by column order within a kind.
    fn fields_targeting(target: PinTargetKind) -> impl Iterator<Item = (MemoryPinField, EdgeKind)> {
        EdgeKind::ALL.into_iter().flat_map(move |kind| {
            MemoryPinField::ALL
                .into_iter()
                .filter(move |field| field.spec().kind == kind && field.target() == target)
                .map(move |field| (field, kind))
        })
    }

    /// Memory pins following `Proxiima.Edges.pinOrder`, with the fork anchor after ordinary references.
    pub fn pins(&self) -> impl Iterator<Item = (MemoryId, EdgeKind)> + '_ {
        Self::fields_targeting(PinTargetKind::Memory).flat_map(move |(field, kind)| {
            let ids = match self.slot(field) {
                PinSlot::Memories(ids) => ids,
                PinSlot::Goals(_) => &[],
            };
            ids.iter().map(move |id| (*id, kind))
        })
    }

    /// Goal reference pins resolved by an owner-scoped batch read.
    pub fn goal_pins(&self) -> impl Iterator<Item = (GoalId, EdgeKind)> + '_ {
        Self::fields_targeting(PinTargetKind::Goal).flat_map(move |(field, kind)| {
            let ids = match self.slot(field) {
                PinSlot::Goals(ids) => ids,
                PinSlot::Memories(_) => &[],
            };
            ids.iter().map(move |id| (*id, kind))
        })
    }

    /// Drop the Goal references this reader may not see into the raw
    /// carrier, keeping only the readable ones typed as Goals.
    ///
    pub fn resolve_visible_goal_refs(&mut self, visible: &[GoalId]) {
        // `Causa.unreadable_goal_reference_resolves_like_a_memory_reference`.
        if self.goal_refs.is_empty() && self.guided_by.is_empty() {
            return;
        }
        let visible: std::collections::HashSet<_> = visible.iter().copied().collect();
        let mut readable = Vec::with_capacity(self.goal_refs.len());
        for goal in self.goal_refs.drain(..) {
            if visible.contains(&goal) {
                readable.push(goal);
            } else {
                self.refs.push(MemoryId::new(goal.into_inner()));
            }
        }
        self.goal_refs = readable;
        let mut readable = Vec::with_capacity(self.guided_by.len());
        for goal in self.guided_by.drain(..) {
            if visible.contains(&goal) {
                readable.push(goal);
            } else {
                self.refs.push(MemoryId::new(goal.into_inner()));
            }
        }
        self.guided_by = readable;
    }
}

/// `created_at` of a pin is the source row's `UUIDv8` timestamp
/// ([`MemoryId::created_at`]).
#[must_use]
pub fn pin_created_at(source: MemoryId) -> time::OffsetDateTime {
    source.created_at()
}

/// Query-window edges: both endpoints must be in `nodes`. Other pins
/// are omitted, not redacted.
#[must_use]
pub fn project_window_edges(nodes: &[PinNode], cap: usize) -> Vec<Edge> {
    let visible: std::collections::HashMap<MemoryId, MemoryKind> = nodes
        .iter()
        .filter_map(|node| MemoryKind::of(node.kind).map(|kind| (node.id, kind)))
        .collect();
    let mut edges = Vec::new();
    for source in nodes {
        let Some(source_kind) = visible.get(&source.id).copied() else {
            continue;
        };
        for (target, kind) in source.pins() {
            let Some(target_kind) = visible.get(&target).copied() else {
                continue;
            };
            edges.push(Edge {
                source: EdgeEndpoint::memory(source_kind, source.id),
                target: EdgeTargetProjection::visible(EdgeEndpoint::memory(target_kind, target)),
                kind,
                created_at: pin_created_at(source.id),
            });
            if edges.len() >= cap {
                return edges;
            }
        }
        for (target, kind) in source.goal_pins() {
            edges.push(Edge {
                source: EdgeEndpoint::memory(source_kind, source.id),
                target: EdgeTargetProjection::visible(EdgeEndpoint::goal(target)),
                kind,
                created_at: pin_created_at(source.id),
            });
            if edges.len() >= cap {
                return edges;
            }
        }
    }
    edges
}

/// Project an exact target using the admitted read results.
#[must_use]
pub fn project_memory_target<S: std::hash::BuildHasher>(
    target: MemoryId,
    live: &std::collections::HashMap<MemoryId, EntityKind, S>,
    unread: &crate::verbs::query::UnreadTargets,
) -> EdgeTargetProjection {
    // `Proxiima.Edges.projectTarget`.
    if let Some(kind) = live.get(&target).copied().and_then(MemoryKind::of) {
        EdgeTargetProjection::visible(EdgeEndpoint::memory(kind, target))
    } else if unread.forgotten.iter().any(|node| node.memory_id == target) {
        EdgeTargetProjection::Forgotten
    } else if unread.erased.contains(&target) {
        EdgeTargetProjection::Erased
    } else {
        EdgeTargetProjection::Redacted
    }
}

/// Listing projection of one pin; its target through
/// [`project_memory_target`].
#[must_use]
pub fn project_listed_edge<S: std::hash::BuildHasher>(
    source_kind: MemoryKind,
    source: MemoryId,
    target: MemoryId,
    kind: EdgeKind,
    live: &std::collections::HashMap<MemoryId, EntityKind, S>,
    unread: &crate::verbs::query::UnreadTargets,
) -> Edge {
    Edge {
        source: EdgeEndpoint::memory(source_kind, source),
        target: project_memory_target(target, live, unread),
        kind,
        created_at: pin_created_at(source),
    }
}

/// Check the declared reference under `Proxiima.Edges.admitsReferenceLayer`.
///
/// # Errors
///
/// Returns a message when both endpoints are memories and the source
/// sits below the target in the F/A/P order.
pub fn validate_edge_layering(source: EdgeEndpoint, target: EdgeEndpoint) -> Result<(), String> {
    let (Some(source_layer), Some(target_layer)) = (source.layer(), target.layer()) else {
        return Ok(());
    };
    if source_layer < target_layer {
        return Err(format!(
            "layering violation: a {} cannot point at a {} (source layer must be at least the target layer)",
            source.kind().as_str(),
            target.kind().as_str(),
        ));
    }
    Ok(())
}

/// A self-loop is never a connection between two things — it is a row
/// asserting that a node relates to itself, which no node write can mean.
///
/// # Errors
///
/// Returns a message when both endpoints address the same entity.
pub fn validate_not_self_loop(source: EdgeEndpoint, target: EdgeEndpoint) -> Result<(), String> {
    if source.entity() == target.entity() {
        return Err("an edge cannot point at its own source".to_string());
    }
    Ok(())
}

#[cfg(test)]
#[path = "edge_tests.rs"]
mod tests;
