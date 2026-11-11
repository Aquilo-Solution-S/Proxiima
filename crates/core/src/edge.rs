//! Pins — `memory.origins` / `memory.refs` / `memory.reads`. There is no
//! edge table.
//!
//! docs/concepts.md is the reference.
//!
//! > Edges are fundamental, non-extensible connection patterns. An edge
//! > carries no information beyond its existence: its endpoints, its
//! > direction, its creation time, and its kind. All content lives in
//! > nodes; meaning arises from the synthesis of the connected nodes.
//!
//! Two consequences shape every type below:
//!
//! - **Kind follows operation.** [`EdgeKind`] is never a parameter a
//!   writer chooses. `Origin` is what a derivation declaration produces;
//!   `Reference` is what a schema-declared reference field produces;
//!   `Read` is what the host's [`ReadSet`] produces. No public API takes
//!   an `EdgeKind` from a caller.
//! - **The pin is its own identity.** There is no edge row and no edge
//!   id: a pin is a `MemoryId` in the source row's `origins`, `refs` or
//!   `reads` column, so replaying a write re-asserts the same column values and
//!   idempotency is structural rather than derived from a content hash.
//!   The [`Edge`] / [`EdgeKind`] types below are the read-side
//!   projection of those columns, not a table.

use crate::entity::EntityRef;
use crate::{EntityKind, GoalId, MemoryId, OwnerId, SchemaId};

/// Closed substrate vocabulary for what an edge *is*. Three variants, not
/// extensible — not by spiices, not by core features. A feature that
/// seems to need a fourth kind fails the node-home test (docs/concepts.md
/// Thesis) and is missing a node, not a kind.
///
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
    /// Every kind, in the order pins are listed: origins, references, reads.
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

/// The memories a session read before it wrote (#677): the source of every
/// [`EdgeKind::Read`] pin.
///
/// Host-supplied call context, never a tool argument. A host binds it with
/// [`crate::AuthzContext::with_read_set`]; every memory write the context
/// authorizes keeps it, and the whole write is refused when the caller
/// cannot read one of the named memories: a local one in its named space,
/// inside the write's transaction; an other-store one through the port.
/// Sorted by memory, one entry per memory; empty by default.
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
    pub const MAX_PINS: usize = 1024;

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

/// One end of an edge: where it points ([`EntityRef`] — a memory row or
/// a Goal) plus the entity kind stored there.
///
/// A `Memory`/`Goal` address pins the row. There is no follow-head address.
///
/// The kind travels with the address because the F/A/P layering rule and
/// every wire projection need it, and re-deriving it per read is what
/// made edge reads a second query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct EdgeEndpoint {
    pub kind: EntityKind,
    pub entity: EntityRef,
}

impl EdgeEndpoint {
    /// A pinned memory row.
    #[must_use]
    pub const fn memory(kind: EntityKind, memory_id: MemoryId) -> Self {
        Self {
            kind,
            entity: EntityRef::Memory(memory_id),
        }
    }

    /// A Goal row. Goal endpoints sit outside the F/A/P layer comparison.
    #[must_use]
    pub const fn goal(goal_id: GoalId) -> Self {
        Self {
            kind: EntityKind::Goal,
            entity: EntityRef::Goal(goal_id),
        }
    }

    /// The stable id stored in a pin column for this endpoint.
    #[must_use]
    pub const fn entity_id(self) -> uuid::Uuid {
        match self.entity {
            EntityRef::Memory(memory_id) => memory_id.into_inner(),
            EntityRef::Goal(goal_id) => goal_id.into_inner(),
        }
    }

    /// Check that the endpoint's kind agrees with the entity address.
    ///
    /// Memory endpoints carry an F/A/P kind; Goal endpoints carry `Goal`.
    /// Keeping this check next to the address constructors prevents a caller
    /// from authorizing one entity while declaring another kind.
    ///
    /// # Errors
    ///
    /// Returns an error when the declared kind and addressed entity disagree.
    pub fn validate_shape(self) -> Result<(), String> {
        match (self.kind, self.entity) {
            (EntityKind::Goal, EntityRef::Goal(_))
            | (
                EntityKind::Fact | EntityKind::Abstraction | EntityKind::Perspective,
                EntityRef::Memory(_),
            ) => Ok(()),
            (EntityKind::Goal, EntityRef::Memory(_)) => {
                Err("a Goal endpoint must address a Goal".to_string())
            }
            (_, EntityRef::Goal(_)) => {
                Err("a memory endpoint must carry an F/A/P kind".to_string())
            }
        }
    }

    /// The memory row this endpoint pins, if it pins one.
    #[must_use]
    pub const fn memory_id(self) -> Option<MemoryId> {
        match self.entity {
            EntityRef::Memory(memory_id) => Some(memory_id),
            EntityRef::Goal(_) => None,
        }
    }

    /// F/A/P layer index, or `None` for Goal endpoints, which are a
    /// separate entity axis (docs/concepts.md) and never enter the comparison.
    #[must_use]
    pub const fn layer(self) -> Option<u8> {
        match self.kind {
            EntityKind::Fact => Some(0),
            EntityKind::Abstraction => Some(1),
            EntityKind::Perspective => Some(2),
            EntityKind::Goal => None,
        }
    }
}

/// Target side of an edge as projected to one reader. A readable edge
/// may still have an endpoint the reader may not see; the row is
/// returned with the endpoint withheld rather than suppressed, so the
/// existence of a connection is not itself leaked or hidden by accident.
/// A target the reader could read but which is forgotten is `Forgotten`;
/// an erased one is `Erased`, since the edge already shows it existed;
/// any other target the reader cannot read stays `Redacted`.
/// [`project_memory_target`] is the one rule that picks the variant.
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

    /// The table: what each pin column is, stored as, and may target.
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

    /// Memory pins: origins first, then references (the fork source after
    /// `refs`), then reads.
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
    /// NON-DISCLOSURE. Storage knows which references are Goals, but a
    /// reader must not learn that about a target it cannot read: an
    /// unreadable Goal and an unreadable Memory have to be indistinguishable.
    /// So an unreadable Goal is pushed back into `refs`, where projection
    /// redacts it through the very same path as an unreadable Memory. The
    /// stored column narrows which spine gets probed and nothing else — it
    /// is never an authorization or projection input.
    pub fn resolve_visible_goal_refs(&mut self, visible: &[GoalId]) {
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
    let visible: std::collections::HashMap<MemoryId, EntityKind> =
        nodes.iter().map(|node| (node.id, node.kind)).collect();
    let mut edges = Vec::new();
    for source in nodes {
        for (target, kind) in source.pins() {
            let Some(target_kind) = visible.get(&target).copied() else {
                continue;
            };
            edges.push(Edge {
                source: EdgeEndpoint::memory(source.kind, source.id),
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
                source: EdgeEndpoint::memory(source.kind, source.id),
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

/// How one reader sees a pin's memory target: visible when `live` holds
/// it, else forgotten or erased as `unread` says, else redacted. The one
/// rule every edge read (pins, neighbours, lineage) projects through.
#[must_use]
pub fn project_memory_target<S: std::hash::BuildHasher>(
    target: MemoryId,
    live: &std::collections::HashMap<MemoryId, EntityKind, S>,
    unread: &crate::verbs::query::UnreadTargets,
) -> EdgeTargetProjection {
    if let Some(kind) = live.get(&target).copied() {
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
    source_kind: EntityKind,
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

/// Enforce the layering rule for a proposed edge: `ℓ(source) ≥
/// ℓ(target)` for memory endpoints, with Goal endpoints outside the
/// comparison (docs/concepts.md).
///
/// This is what keeps a Fact from being an interpretation *source*: a
/// Fact asserts no judgment, and every interpretation is a Perspective
/// referring downward.
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
            source.kind.as_str(),
            target.kind.as_str(),
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
    if source.entity == target.entity {
        return Err("an edge cannot point at its own source".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        EdgeEndpoint, EdgeKind, EdgeTargetProjection, MemoryPinField, PinNode, PinTargetKind,
        project_listed_edge, project_memory_target, project_window_edges, validate_edge_layering,
        validate_not_self_loop,
    };
    use crate::{EntityKind, GoalId, MemoryId};

    fn memory(kind: EntityKind) -> EdgeEndpoint {
        EdgeEndpoint::memory(kind, MemoryId::new(uuid::Uuid::now_v7()))
    }

    /// The vocabulary is closed at three (`read` is #677). A fourth
    /// variant is a design change (docs/concepts.md), not a patch, and
    /// this is the test that says so out loud.
    #[test]
    fn the_vocabulary_is_exactly_three_kinds() {
        let all = [EdgeKind::Origin, EdgeKind::Reference, EdgeKind::Read];
        assert_eq!(all.len(), 3);
        for kind in all {
            match kind {
                EdgeKind::Origin | EdgeKind::Reference | EdgeKind::Read => {}
            }
        }
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
            once.merged(
                super::ReadSet::new([super::ReadPin::OtherStore { memory }]).expect("one pin")
            ),
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
}
