//! Live-Postgres stateful `Causa` invariant oracle and planted-bug controls.
//!
//! Connects `lean/Causa/Principles.lean`, `lean/Causa/Provenance.lean`, and
//! `lean/Causa/Goals.lean` to the live Rust + PostgreSQL engine:
//! 1. `causa_oracle_planted_bug_controls` exercises single-fault controls
//!    proving every `CausaViolation` variant reports its exact violation.
//! 2. `causa_stateful_trace_preserves_all_invariants` drives deterministic
//!    randomized `Engine` + `PgStorage` traces across two owners against live
//!    PostgreSQL and verifies the stored Memory graph and Goal-forest
//!    invariants (`Causa.RetainedCommittedStoreValid`, `Causa.GoalForestInv`,
//!    and Principles 1, 2, 4, 5, 6, 7, 8, and 9) after every setup and trace
//!    step. Role-resolution access rules (Principle 3) are verified separately
//!    by the owner-authority and RLS suites.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use proxiima_core::lexical_language::LEXICAL_LANGUAGE_DEPLOYMENT_DEFAULT;
use proxiima_core::owner_inverse::OwnerSurfaces;
use proxiima_core::test_fixtures::authenticated_context;
use proxiima_core::verbs::fact_ingest::FactWriteCommand;
use proxiima_core::verbs::goal_write::{
    ChildGoalDraft, GoalAuthorship, GoalDependencyRef, GoalEvidenceRef, GoalMotivationRef,
    GoalPayloadWrite, GoalTopologyWrite, GoalTransitionKind, IdempotencyKey,
};
use proxiima_core::{
    AgentDerivationV1, AgentNoteV1, AuthPath, AuthzContext, DerivationIdentity, DerivedMemory,
    EdgeEndpoint, Engine, ErrorCode, ForkMemoryRequest, GoalCreatePayloadWriteRequest,
    GoalDecomposeRequest, GoalId, GoalMarkAchievedRequest, GoalModifyRequest,
    GoalTransitionRequest, InputContractId, InterpretationConfidence, InterpretationV1,
    MemoryDestination, MemoryId, MemoryKind, MemoryTarget, OperatorId, OwnerId, PendingWork,
    ProtocolError, SchemaId, SchemaVersion, SeriesHandle, SidecarPayload, SpiiceRegistry, UserId,
};
use proxiima_pg_testkit::{DbGuard, db_url};
use proxiima_storage_pg::test_fixtures::fresh_pg;
use proxiima_storage_pg::verbs::forget::{MemoryColdStore, erase_memory};
use proxiima_storage_pg::{PgHostStateEraseContext, PgStorage};
use sqlx::PgPool;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const DEFAULT_SEED: u64 = 0xC405_A001_2026_1124;
const DEFAULT_STEPS: usize = 160;

// ---------------------------------------------------------------------------
// 1. Live Postgres Graph Snapshot & Causa Invariant Checker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotMemoryKind {
    Fact,
    Abstraction,
    Perspective,
}

impl SnapshotMemoryKind {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "fact" => Ok(Self::Fact),
            "abstraction" => Ok(Self::Abstraction),
            "perspective" => Ok(Self::Perspective),
            other => Err(format!("unknown memory_kind {other:?}")),
        }
    }

    const fn as_pin_kind(self) -> SnapshotPinKind {
        match self {
            Self::Fact => SnapshotPinKind::Fact,
            Self::Abstraction => SnapshotPinKind::Abstraction,
            Self::Perspective => SnapshotPinKind::Perspective,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotPinKind {
    Fact,
    Abstraction,
    Perspective,
    Goal,
}

impl SnapshotPinKind {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "fact" => Ok(Self::Fact),
            "abstraction" => Ok(Self::Abstraction),
            "perspective" => Ok(Self::Perspective),
            "goal" => Ok(Self::Goal),
            other => Err(format!("unknown pin_target_kind {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotGoalState {
    Active,
    Paused,
    Achieved,
    Abandoned,
}

impl SnapshotGoalState {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "Active" => Ok(Self::Active),
            "Paused" => Ok(Self::Paused),
            "Achieved" => Ok(Self::Achieved),
            "Abandoned" => Ok(Self::Abandoned),
            other => Err(format!("unknown goal_state {other:?}")),
        }
    }

    const fn is_terminal(self) -> bool {
        matches!(self, Self::Achieved | Self::Abandoned)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemoryRow {
    handle: Uuid,
    t: Uuid,
    kind: SnapshotMemoryKind,
    owner_id: Uuid,
    origins: Vec<Uuid>,
    refs: Vec<Uuid>,
    goal_refs: Vec<Uuid>,
    guided_by: Vec<Uuid>,
    forked_from: Option<Uuid>,
    reads: Vec<Uuid>,
    forgotten: bool,
    tick: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemoryHeadRow {
    handle: Uuid,
    kind: SnapshotMemoryKind,
    owner_id: Uuid,
    t: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoalRow {
    handle: Uuid,
    t: Uuid,
    owner_id: Uuid,
    state: SnapshotGoalState,
    close_fact_t: Option<Uuid>,
    assignment_t: Option<Uuid>,
    dependency_t: Vec<Uuid>,
    evidence_t: Vec<Uuid>,
    motivated_by: Vec<Uuid>,
    parent_handle: Option<Uuid>,
    born_tick: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoalHeadRow {
    handle: Uuid,
    owner_id: Uuid,
    t: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ErasedTargetRow {
    t: Uuid,
    kind: SnapshotPinKind,
}

#[derive(Debug, Clone, Default)]
struct CausaGraphSnapshot {
    memories: Vec<MemoryRow>,
    memory_heads: Vec<MemoryHeadRow>,
    goals: Vec<GoalRow>,
    goal_heads: Vec<GoalHeadRow>,
    erased_targets: Vec<ErasedTargetRow>,
}

/// Immutable pin fingerprint captured when a Memory admission is first seen.
// Holds `Causa.principle_5_no_transition_retargets_a_pin`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PinnedMemoryRecord {
    handle: Uuid,
    kind: SnapshotMemoryKind,
    owner_id: Uuid,
    origins: Vec<Uuid>,
    refs: Vec<Uuid>,
    goal_refs: Vec<Uuid>,
    guided_by: Vec<Uuid>,
    forked_from: Option<Uuid>,
    reads: Vec<Uuid>,
}

impl PinnedMemoryRecord {
    fn from_row(row: &MemoryRow) -> Self {
        Self {
            handle: row.handle,
            kind: row.kind,
            owner_id: row.owner_id,
            origins: row.origins.clone(),
            refs: row.refs.clone(),
            goal_refs: row.goal_refs.clone(),
            guided_by: row.guided_by.clone(),
            forked_from: row.forked_from,
            reads: row.reads.clone(),
        }
    }
}

/// Observed history carried across steps for transition-level invariants.
#[derive(Debug, Clone, Default)]
struct TraceHistory {
    memories: HashMap<Uuid, PinnedMemoryRecord>,
    erased: HashMap<Uuid, SnapshotPinKind>,
}

/// Specific invariant violations mapped to `Causa`'s graph and Goal-forest rules.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CausaViolation {
    // `Causa.principle_1_fact_has_no_origins`
    FactHasOrigins {
        t: Uuid,
    },
    // `Causa.RetainedMemoryGraphValid.abstractionHasOrigins`
    AbstractionMissingOrigins {
        t: Uuid,
    },
    // `Causa.principle_2_derivation_goes_up_and_grounds_in_facts`
    AbstractionOriginBadKind {
        t: Uuid,
        origin: Uuid,
    },
    // `Causa.principle_2_derivation_goes_up_and_grounds_in_facts`
    PerspectiveOriginBadKind {
        t: Uuid,
        origin: Uuid,
    },
    // `Causa.RetainedMemoryGraphValid.derivedProvenance` and `Causa.principle_7_a_read_pin_never_grounds`
    DerivedMissingProvenance {
        t: Uuid,
    },
    // `Causa.RetainedMemoryGraphValid.pinTimeStrict` and `Causa.grounding_wf`
    PinTimeOrderViolation {
        source: Uuid,
        target: Uuid,
    },
    // `Causa.principle_4_the_field_fixes_the_kind` (`memoryGoalIdsDisjoint`)
    MemoryGoalIdCollision {
        id: Uuid,
    },
    // `Causa.ErasedTargetsDisjointMemories` and `Causa.ErasedTargetsDisjointGoals`
    ErasedTargetLiveCollision {
        id: Uuid,
    },
    // `Causa.hard_erase_memory_records_kind`
    MissingErasedTarget {
        t: Uuid,
    },
    // `Causa.hard_erase_memory_records_kind`
    ErasedTargetKindMismatch {
        t: Uuid,
        expected: SnapshotPinKind,
        actual: SnapshotPinKind,
    },
    // `Causa.principle_4_the_field_fixes_the_kind` and `Causa.principle_6_erasure_leaves_no_dangling_pin`
    DanglingMemoryPin {
        source: Uuid,
        field: &'static str,
        target: Uuid,
    },
    // `Causa.principle_4_the_field_fixes_the_kind` and `Causa.principle_6_erasure_leaves_no_dangling_pin`
    DanglingGoalPin {
        source: Uuid,
        field: &'static str,
        target: Uuid,
    },
    // `Causa.Memory.guided_by_only_perspective`
    NonPerspectiveGuidedBy {
        t: Uuid,
    },
    // `Causa.MemoryHeadAligned`
    BadMemoryHeadAlignment {
        handle: Uuid,
    },
    // `Causa.ForkAnchorAligned`
    ForkAnchorMisaligned {
        handle: Uuid,
    },
    // `Causa.principle_5_no_transition_retargets_a_pin`
    PinRetargeted {
        t: Uuid,
        field: &'static str,
    },
    // `Causa.principle_8_a_goal_never_grounds`
    GoalInMemoryOrigins {
        t: Uuid,
        goal_id: Uuid,
    },
    // `Causa.principle_8_a_goal_never_grounds` (`guidance_does_not_ground`)
    GuidanceOverlapsOrigins {
        t: Uuid,
        goal_id: Uuid,
    },
    // `Causa.GoalForestInv` (goal head / series alignment)
    BadGoalHeadAlignment {
        handle: Uuid,
    },
    // `Causa.goal_tree_acyclic`
    GoalTreeCycle {
        handle: Uuid,
    },
    // `Causa.ParentsExist`
    GoalMissingParent {
        handle: Uuid,
        parent_handle: Uuid,
    },
    // `Causa.ParentOlder`
    GoalParentNotOlder {
        child: Uuid,
        parent: Uuid,
    },
    // `Causa.closed_ancestor_closes_descendant`
    OpenGoalUnderClosedAncestor {
        child: Uuid,
        ancestor: Uuid,
    },
    // `Causa.RetainedGoalAssignmentValid`
    BadGoalAssignment {
        goal_t: Uuid,
        target: Uuid,
    },
    // `Causa.RetainedGoalEvidenceValid`
    BadGoalEvidence {
        goal_t: Uuid,
        target: Uuid,
    },
    // `Causa.goal_motivations_retained_valid`
    BadGoalMotivation {
        goal_t: Uuid,
        target: Uuid,
    },
    // `Causa.RetainedCommittedStoreValid` (dependencies)
    BadGoalDependency {
        goal_t: Uuid,
        target: Uuid,
    },
    // `Causa.RetainedGoalTerminalCloseFactValid`
    BadGoalTerminalCloseFact {
        goal_t: Uuid,
    },
    DuplicateMemoryVersion {
        t: Uuid,
    },
    DuplicateGoalVersion {
        t: Uuid,
    },
    DuplicateErasedTarget {
        t: Uuid,
    },
    UnrecordedMemoryDisappearance {
        t: Uuid,
    },
}

struct SnapshotIndex<'a> {
    memories: HashMap<Uuid, &'a MemoryRow>,
    goals: HashMap<Uuid, &'a GoalRow>,
    erased: HashMap<Uuid, SnapshotPinKind>,
}

impl<'a> SnapshotIndex<'a> {
    fn build(snapshot: &'a CausaGraphSnapshot) -> Result<Self, CausaViolation> {
        let mut memories = HashMap::with_capacity(snapshot.memories.len());
        for row in &snapshot.memories {
            if memories.insert(row.t, row).is_some() {
                return Err(CausaViolation::DuplicateMemoryVersion { t: row.t });
            }
        }
        let mut goals = HashMap::with_capacity(snapshot.goals.len());
        for row in &snapshot.goals {
            if memories.contains_key(&row.t) {
                return Err(CausaViolation::MemoryGoalIdCollision { id: row.t });
            }
            if goals.insert(row.t, row).is_some() {
                return Err(CausaViolation::DuplicateGoalVersion { t: row.t });
            }
        }
        let mut erased = HashMap::with_capacity(snapshot.erased_targets.len());
        for row in &snapshot.erased_targets {
            if memories.contains_key(&row.t) || goals.contains_key(&row.t) {
                return Err(CausaViolation::ErasedTargetLiveCollision { id: row.t });
            }
            if erased.insert(row.t, row.kind).is_some() {
                return Err(CausaViolation::DuplicateErasedTarget { t: row.t });
            }
        }
        Ok(Self {
            memories,
            goals,
            erased,
        })
    }

    fn resolve_memory_kind(&self, id: Uuid) -> Option<SnapshotMemoryKind> {
        if let Some(m) = self.memories.get(&id) {
            return Some(m.kind);
        }
        match self.erased.get(&id).copied() {
            Some(SnapshotPinKind::Fact) => Some(SnapshotMemoryKind::Fact),
            Some(SnapshotPinKind::Abstraction) => Some(SnapshotMemoryKind::Abstraction),
            Some(SnapshotPinKind::Perspective) => Some(SnapshotMemoryKind::Perspective),
            Some(SnapshotPinKind::Goal) | None => None,
        }
    }

    fn is_goal_identity(&self, id: Uuid) -> bool {
        self.goals.contains_key(&id) || self.erased.get(&id) == Some(&SnapshotPinKind::Goal)
    }

    fn resolves_to_memory(&self, id: Uuid) -> bool {
        self.resolve_memory_kind(id).is_some()
    }
}

/// Verifies the stored `Causa` Memory graph and Goal-forest invariants over
/// `snapshot`, plus transition pin stability and erase-tombstone fidelity
/// against `history`.
fn verify_causa_snapshot(
    snapshot: &CausaGraphSnapshot,
    history: &TraceHistory,
) -> Result<(), CausaViolation> {
    let idx = SnapshotIndex::build(snapshot)?;
    verify_erased_history(&idx, history)?;
    verify_memory_rows(snapshot, &idx, history)?;
    verify_pin_time_strict(snapshot, &idx)?;
    verify_memory_series_alignment(snapshot, &idx)?;
    verify_goal_rows(snapshot, &idx)?;
    verify_goal_forest(snapshot, &idx)?;
    Ok(())
}

fn verify_erased_history(
    idx: &SnapshotIndex<'_>,
    history: &TraceHistory,
) -> Result<(), CausaViolation> {
    for (&t, &expected) in &history.erased {
        let Some(&actual) = idx.erased.get(&t) else {
            return Err(CausaViolation::MissingErasedTarget { t });
        };
        if actual != expected {
            return Err(CausaViolation::ErasedTargetKindMismatch {
                t,
                expected,
                actual,
            });
        }
    }
    for (&t, prev) in &history.memories {
        if !idx.memories.contains_key(&t) {
            if !history.erased.contains_key(&t) {
                return Err(CausaViolation::UnrecordedMemoryDisappearance { t });
            }
            let Some(&actual) = idx.erased.get(&t) else {
                return Err(CausaViolation::MissingErasedTarget { t });
            };
            let expected = prev.kind.as_pin_kind();
            if actual != expected {
                return Err(CausaViolation::ErasedTargetKindMismatch {
                    t,
                    expected,
                    actual,
                });
            }
        }
    }
    Ok(())
}

fn verify_memory_rows(
    snapshot: &CausaGraphSnapshot,
    idx: &SnapshotIndex<'_>,
    history: &TraceHistory,
) -> Result<(), CausaViolation> {
    for row in &snapshot.memories {
        verify_pin_stability(row, &history.memories)?;
        verify_memory_kind_and_pins(row, idx)?;
    }
    Ok(())
}

fn verify_pin_stability(
    row: &MemoryRow,
    history: &HashMap<Uuid, PinnedMemoryRecord>,
) -> Result<(), CausaViolation> {
    let Some(prev) = history.get(&row.t) else {
        return Ok(());
    };
    if prev.handle != row.handle {
        return Err(CausaViolation::PinRetargeted {
            t: row.t,
            field: "handle",
        });
    }
    if prev.origins != row.origins {
        return Err(CausaViolation::PinRetargeted {
            t: row.t,
            field: "origins",
        });
    }
    if prev.refs != row.refs {
        return Err(CausaViolation::PinRetargeted {
            t: row.t,
            field: "refs",
        });
    }
    if prev.goal_refs != row.goal_refs {
        return Err(CausaViolation::PinRetargeted {
            t: row.t,
            field: "goal_refs",
        });
    }
    if prev.guided_by != row.guided_by {
        return Err(CausaViolation::PinRetargeted {
            t: row.t,
            field: "guided_by",
        });
    }
    if prev.forked_from != row.forked_from {
        return Err(CausaViolation::PinRetargeted {
            t: row.t,
            field: "forked_from",
        });
    }
    if prev.reads != row.reads {
        return Err(CausaViolation::PinRetargeted {
            t: row.t,
            field: "reads",
        });
    }
    Ok(())
}

fn verify_memory_kind_and_pins(
    row: &MemoryRow,
    idx: &SnapshotIndex<'_>,
) -> Result<(), CausaViolation> {
    // `Causa.principle_1_fact_has_no_origins`
    if row.kind == SnapshotMemoryKind::Fact && !row.origins.is_empty() {
        return Err(CausaViolation::FactHasOrigins { t: row.t });
    }
    // `Causa.RetainedMemoryGraphValid.abstractionHasOrigins`
    if row.kind == SnapshotMemoryKind::Abstraction && row.origins.is_empty() {
        return Err(CausaViolation::AbstractionMissingOrigins { t: row.t });
    }
    // `Causa.RetainedMemoryGraphValid.derivedProvenance` and `Causa.principle_7_a_read_pin_never_grounds`:
    // `reads` is deliberately excluded here so read context can never satisfy provenance.
    if row.kind != SnapshotMemoryKind::Fact && row.origins.is_empty() && row.refs.is_empty() {
        return Err(CausaViolation::DerivedMissingProvenance { t: row.t });
    }
    // `Causa.Memory.guided_by_only_perspective`
    if row.kind != SnapshotMemoryKind::Perspective && !row.guided_by.is_empty() {
        return Err(CausaViolation::NonPerspectiveGuidedBy { t: row.t });
    }

    for &origin in &row.origins {
        // `Causa.principle_8_a_goal_never_grounds`
        if row.guided_by.contains(&origin) {
            return Err(CausaViolation::GuidanceOverlapsOrigins {
                t: row.t,
                goal_id: origin,
            });
        }
        if idx.is_goal_identity(origin) {
            return Err(CausaViolation::GoalInMemoryOrigins {
                t: row.t,
                goal_id: origin,
            });
        }
        let Some(target_kind) = idx.resolve_memory_kind(origin) else {
            return Err(CausaViolation::DanglingMemoryPin {
                source: row.t,
                field: "origins",
                target: origin,
            });
        };
        match row.kind {
            SnapshotMemoryKind::Fact => return Err(CausaViolation::FactHasOrigins { t: row.t }),
            SnapshotMemoryKind::Abstraction
                if !matches!(
                    target_kind,
                    SnapshotMemoryKind::Fact | SnapshotMemoryKind::Abstraction
                ) =>
            {
                return Err(CausaViolation::AbstractionOriginBadKind { t: row.t, origin });
            }
            SnapshotMemoryKind::Perspective if target_kind != SnapshotMemoryKind::Abstraction => {
                return Err(CausaViolation::PerspectiveOriginBadKind { t: row.t, origin });
            }
            SnapshotMemoryKind::Abstraction | SnapshotMemoryKind::Perspective => {}
        }
    }

    for &target in &row.refs {
        if !idx.resolves_to_memory(target) {
            return Err(CausaViolation::DanglingMemoryPin {
                source: row.t,
                field: "refs",
                target,
            });
        }
    }
    if let Some(forked) = row.forked_from
        && !idx.resolves_to_memory(forked)
    {
        return Err(CausaViolation::DanglingMemoryPin {
            source: row.t,
            field: "forked_from",
            target: forked,
        });
    }
    for &goal_id in &row.goal_refs {
        if !idx.is_goal_identity(goal_id) {
            return Err(CausaViolation::DanglingGoalPin {
                source: row.t,
                field: "goal_refs",
                target: goal_id,
            });
        }
    }
    for &goal_id in &row.guided_by {
        if let Some(goal) = idx.goals.get(&goal_id) {
            if goal.owner_id != row.owner_id {
                return Err(CausaViolation::DanglingGoalPin {
                    source: row.t,
                    field: "guided_by",
                    target: goal_id,
                });
            }
        } else if idx.erased.get(&goal_id) != Some(&SnapshotPinKind::Goal) {
            return Err(CausaViolation::DanglingGoalPin {
                source: row.t,
                field: "guided_by",
                target: goal_id,
            });
        }
    }
    Ok(())
}

/// Checks `Causa.RetainedMemoryGraphValid.pinTimeStrict`: every stored live
/// pin (`origins ∪ refs ∪ forked_from`) points strictly backward in admission
/// order (`target.tick < source.tick`), which catches both cycles and acyclic
/// forward pins and witnesses well-founded grounding (`Causa.grounding_wf`).
fn verify_pin_time_strict(
    snapshot: &CausaGraphSnapshot,
    idx: &SnapshotIndex<'_>,
) -> Result<(), CausaViolation> {
    for row in &snapshot.memories {
        for &target in row
            .origins
            .iter()
            .chain(row.refs.iter())
            .chain(row.forked_from.iter())
        {
            if let Some(tgt) = idx.memories.get(&target)
                && tgt.tick >= row.tick
            {
                return Err(CausaViolation::PinTimeOrderViolation {
                    source: row.t,
                    target,
                });
            }
        }
    }
    Ok(())
}

/// Checks `Causa.MemoryHeadAligned` across all historical and head versions of
/// every series, plus `Causa.ForkAnchorAligned` across versions of each handle.
fn verify_memory_series_alignment(
    snapshot: &CausaGraphSnapshot,
    idx: &SnapshotIndex<'_>,
) -> Result<(), CausaViolation> {
    let mut heads_by_handle: HashMap<Uuid, &MemoryHeadRow> =
        HashMap::with_capacity(snapshot.memory_heads.len());
    for head in &snapshot.memory_heads {
        if heads_by_handle.insert(head.handle, head).is_some() {
            return Err(CausaViolation::BadMemoryHeadAlignment {
                handle: head.handle,
            });
        }
        let Some(row) = idx.memories.get(&head.t) else {
            return Err(CausaViolation::BadMemoryHeadAlignment {
                handle: head.handle,
            });
        };
        if row.handle != head.handle || row.kind != head.kind || row.owner_id != head.owner_id {
            return Err(CausaViolation::BadMemoryHeadAlignment {
                handle: head.handle,
            });
        }
    }
    let mut fork_anchor_by_handle: HashMap<Uuid, Option<Uuid>> = HashMap::new();
    for row in &snapshot.memories {
        let Some(head) = heads_by_handle.get(&row.handle) else {
            return Err(CausaViolation::BadMemoryHeadAlignment { handle: row.handle });
        };
        if row.kind != head.kind || row.owner_id != head.owner_id {
            return Err(CausaViolation::BadMemoryHeadAlignment { handle: row.handle });
        }
        if let Some(&expected_anchor) = fork_anchor_by_handle.get(&row.handle) {
            if row.forked_from != expected_anchor {
                return Err(CausaViolation::ForkAnchorMisaligned { handle: row.handle });
            }
        } else {
            fork_anchor_by_handle.insert(row.handle, row.forked_from);
        }
    }
    Ok(())
}

fn verify_goal_rows(
    snapshot: &CausaGraphSnapshot,
    idx: &SnapshotIndex<'_>,
) -> Result<(), CausaViolation> {
    for goal in &snapshot.goals {
        // `Causa.RetainedGoalTerminalCloseFactValid`
        if goal.state.is_terminal() != goal.close_fact_t.is_some() {
            return Err(CausaViolation::BadGoalTerminalCloseFact { goal_t: goal.t });
        }
        if let Some(close_fact) = goal.close_fact_t {
            let valid = if let Some(mem) = idx.memories.get(&close_fact) {
                mem.kind == SnapshotMemoryKind::Fact && mem.owner_id == goal.owner_id
            } else {
                idx.erased.get(&close_fact) == Some(&SnapshotPinKind::Fact)
            };
            if !valid {
                return Err(CausaViolation::BadGoalTerminalCloseFact { goal_t: goal.t });
            }
        }
        // `Causa.RetainedGoalAssignmentValid`
        if let Some(assignment) = goal.assignment_t {
            let valid = if let Some(mem) = idx.memories.get(&assignment) {
                mem.kind == SnapshotMemoryKind::Perspective && mem.owner_id == goal.owner_id
            } else {
                idx.erased.get(&assignment) == Some(&SnapshotPinKind::Perspective)
            };
            if !valid {
                return Err(CausaViolation::BadGoalAssignment {
                    goal_t: goal.t,
                    target: assignment,
                });
            }
        }
        // `Causa.RetainedGoalEvidenceValid`
        for &ev in &goal.evidence_t {
            let valid = matches!(
                idx.resolve_memory_kind(ev),
                Some(SnapshotMemoryKind::Fact | SnapshotMemoryKind::Abstraction)
            );
            if !valid {
                return Err(CausaViolation::BadGoalEvidence {
                    goal_t: goal.t,
                    target: ev,
                });
            }
        }
        // `Causa.goal_motivations_retained_valid`
        for &mot in &goal.motivated_by {
            let valid = if let Some(mem) = idx.memories.get(&mot) {
                mem.kind == SnapshotMemoryKind::Perspective && mem.owner_id == goal.owner_id
            } else {
                idx.erased.get(&mot) == Some(&SnapshotPinKind::Perspective)
            };
            if !valid {
                return Err(CausaViolation::BadGoalMotivation {
                    goal_t: goal.t,
                    target: mot,
                });
            }
        }
        // `Causa.RetainedCommittedStoreValid` (dependencies)
        for &dep in &goal.dependency_t {
            if !idx.is_goal_identity(dep) {
                return Err(CausaViolation::BadGoalDependency {
                    goal_t: goal.t,
                    target: dep,
                });
            }
        }
    }
    Ok(())
}

/// Checks Goal-head alignment, `Causa.ParentsExist`, `Causa.goal_tree_acyclic`,
/// `Causa.ParentOlder`, and `Causa.closed_ancestor_closes_descendant`.
fn verify_goal_forest(
    snapshot: &CausaGraphSnapshot,
    idx: &SnapshotIndex<'_>,
) -> Result<(), CausaViolation> {
    let mut head_goal: HashMap<Uuid, &GoalRow> = HashMap::with_capacity(snapshot.goal_heads.len());
    for head in &snapshot.goal_heads {
        let Some(&row) = idx.goals.get(&head.t) else {
            return Err(CausaViolation::BadGoalHeadAlignment {
                handle: head.handle,
            });
        };
        if row.handle != head.handle || row.owner_id != head.owner_id {
            return Err(CausaViolation::BadGoalHeadAlignment {
                handle: head.handle,
            });
        }
        if head_goal.insert(head.handle, row).is_some() {
            return Err(CausaViolation::BadGoalHeadAlignment {
                handle: head.handle,
            });
        }
    }
    for row in &snapshot.goals {
        let Some(&head) = head_goal.get(&row.handle) else {
            return Err(CausaViolation::BadGoalHeadAlignment { handle: row.handle });
        };
        if row.owner_id != head.owner_id || row.parent_handle != head.parent_handle {
            return Err(CausaViolation::BadGoalHeadAlignment { handle: row.handle });
        }
    }

    // Pass 1: parent existence and acyclicity (`Causa.ParentsExist`, `Causa.goal_tree_acyclic`).
    for (&handle, &row) in &head_goal {
        let mut visited = HashSet::new();
        visited.insert(handle);
        let mut cursor = row.parent_handle;
        while let Some(parent) = cursor {
            if !visited.insert(parent) {
                return Err(CausaViolation::GoalTreeCycle { handle });
            }
            let Some(&parent_row) = head_goal.get(&parent) else {
                return Err(CausaViolation::GoalMissingParent {
                    handle,
                    parent_handle: parent,
                });
            };
            if parent_row.owner_id != row.owner_id {
                return Err(CausaViolation::GoalMissingParent {
                    handle,
                    parent_handle: parent,
                });
            }
            cursor = parent_row.parent_handle;
        }
    }

    // Pass 2: parent creation order (`Causa.ParentOlder`).
    for (&handle, &row) in &head_goal {
        if let Some(parent) = row.parent_handle {
            let parent_row = head_goal[&parent];
            if parent_row.born_tick >= row.born_tick {
                return Err(CausaViolation::GoalParentNotOlder {
                    child: handle,
                    parent,
                });
            }
        }
    }

    // Pass 3: closed ancestry (`Causa.closed_ancestor_closes_descendant`).
    for (&handle, &row) in &head_goal {
        if row.state.is_terminal() {
            continue;
        }
        let mut cursor = row.parent_handle;
        while let Some(parent) = cursor {
            let parent_row = head_goal[&parent];
            if parent_row.state.is_terminal() {
                return Err(CausaViolation::OpenGoalUnderClosedAncestor {
                    child: handle,
                    ancestor: parent,
                });
            }
            cursor = parent_row.parent_handle;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 2. Single-Fault Planted-Bug Controls (No Postgres Needed)
// ---------------------------------------------------------------------------

struct ValidFixture {
    snapshot: CausaGraphSnapshot,
    history: TraceHistory,
    owner: Uuid,
    early_fact_t: Uuid,
    fact_t: Uuid,
    abs_h: Uuid,
    abs_t: Uuid,
    persp_t: Uuid,
    erased_fact_t: Uuid,
    unpinned_fact_h: Uuid,
    unpinned_fact_t: Uuid,
    root_goal_h: Uuid,
    root_goal_t: Uuid,
    child_goal_h: Uuid,
    child_goal_t: Uuid,
}

#[expect(
    clippy::too_many_arguments,
    reason = "single fixture builder avoids duplicating MemoryRow fields across controls"
)]
fn fixture_memory(
    handle: Uuid,
    t: Uuid,
    kind: SnapshotMemoryKind,
    owner_id: Uuid,
    origins: Vec<Uuid>,
    goal_refs: Vec<Uuid>,
    guided_by: Vec<Uuid>,
    tick: u64,
) -> MemoryRow {
    MemoryRow {
        handle,
        t,
        kind,
        owner_id,
        origins,
        refs: Vec::new(),
        goal_refs,
        guided_by,
        forked_from: None,
        reads: Vec::new(),
        forgotten: false,
        tick,
    }
}

fn fixture_goals(
    owner: Uuid,
    fact_t: Uuid,
    abs_t: Uuid,
    persp_t: Uuid,
    root: (Uuid, Uuid),
    child: (Uuid, Uuid),
) -> (Vec<GoalRow>, Vec<GoalHeadRow>) {
    let goals = vec![
        GoalRow {
            handle: root.0,
            t: root.1,
            owner_id: owner,
            state: SnapshotGoalState::Active,
            close_fact_t: None,
            assignment_t: Some(persp_t),
            dependency_t: Vec::new(),
            evidence_t: vec![fact_t],
            motivated_by: vec![persp_t],
            parent_handle: None,
            born_tick: 10,
        },
        GoalRow {
            handle: child.0,
            t: child.1,
            owner_id: owner,
            state: SnapshotGoalState::Achieved,
            close_fact_t: Some(fact_t),
            assignment_t: None,
            dependency_t: vec![root.1],
            evidence_t: vec![abs_t],
            motivated_by: vec![persp_t],
            parent_handle: Some(root.0),
            born_tick: 20,
        },
    ];
    let goal_heads = goals
        .iter()
        .map(|g| GoalHeadRow {
            handle: g.handle,
            owner_id: g.owner_id,
            t: g.t,
        })
        .collect();
    (goals, goal_heads)
}

fn fixture_memories(
    owner: Uuid,
    erased_fact_t: Uuid,
    ids: &[(Uuid, Uuid); 7],
    root_goal_t: Uuid,
) -> (Vec<MemoryRow>, Vec<MemoryHeadRow>) {
    let [
        (early_h, early_t),
        (fact_h, fact_t),
        (abs_h, abs_t),
        (persp_h, persp_t),
        (unpinned_h, unpinned_t),
        (refl_fact_h, refl_fact_t),
        (refl_abs_h, refl_abs_t),
    ] = *ids;
    let memories = vec![
        fixture_memory(
            early_h,
            early_t,
            SnapshotMemoryKind::Fact,
            owner,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            1,
        ),
        fixture_memory(
            fact_h,
            fact_t,
            SnapshotMemoryKind::Fact,
            owner,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            2,
        ),
        fixture_memory(
            abs_h,
            abs_t,
            SnapshotMemoryKind::Abstraction,
            owner,
            vec![fact_t, erased_fact_t],
            Vec::new(),
            Vec::new(),
            3,
        ),
        fixture_memory(
            persp_h,
            persp_t,
            SnapshotMemoryKind::Perspective,
            owner,
            vec![abs_t],
            Vec::new(),
            vec![root_goal_t],
            4,
        ),
        fixture_memory(
            unpinned_h,
            unpinned_t,
            SnapshotMemoryKind::Fact,
            owner,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            5,
        ),
        fixture_memory(
            refl_fact_h,
            refl_fact_t,
            SnapshotMemoryKind::Fact,
            owner,
            Vec::new(),
            vec![root_goal_t],
            Vec::new(),
            6,
        ),
        fixture_memory(
            refl_abs_h,
            refl_abs_t,
            SnapshotMemoryKind::Abstraction,
            owner,
            vec![refl_fact_t],
            Vec::new(),
            Vec::new(),
            7,
        ),
    ];
    let memory_heads = memories
        .iter()
        .map(|m| MemoryHeadRow {
            handle: m.handle,
            kind: m.kind,
            owner_id: m.owner_id,
            t: m.t,
        })
        .collect();
    (memories, memory_heads)
}

fn valid_causa_fixture() -> ValidFixture {
    let owner = Uuid::now_v7();
    let erased_fact_t = Uuid::now_v7();
    let ids = [
        (Uuid::now_v7(), Uuid::now_v7()),
        (Uuid::now_v7(), Uuid::now_v7()),
        (Uuid::now_v7(), Uuid::now_v7()),
        (Uuid::now_v7(), Uuid::now_v7()),
        (Uuid::now_v7(), Uuid::now_v7()),
        // Principle 9 chain: reflective Fact observing root_goal_t + Abstraction over it.
        (Uuid::now_v7(), Uuid::now_v7()),
        (Uuid::now_v7(), Uuid::now_v7()),
    ];
    let (root_goal_h, root_goal_t) = (Uuid::now_v7(), Uuid::now_v7());
    let (child_goal_h, child_goal_t) = (Uuid::now_v7(), Uuid::now_v7());
    let (memories, memory_heads) = fixture_memories(owner, erased_fact_t, &ids, root_goal_t);
    let (goals, goal_heads) = fixture_goals(
        owner,
        ids[1].1,
        ids[2].1,
        ids[3].1,
        (root_goal_h, root_goal_t),
        (child_goal_h, child_goal_t),
    );
    let erased_targets = vec![ErasedTargetRow {
        t: erased_fact_t,
        kind: SnapshotPinKind::Fact,
    }];
    let history = TraceHistory {
        memories: memories
            .iter()
            .map(|m| (m.t, PinnedMemoryRecord::from_row(m)))
            .collect(),
        erased: HashMap::from([(erased_fact_t, SnapshotPinKind::Fact)]),
    };
    ValidFixture {
        snapshot: CausaGraphSnapshot {
            memories,
            memory_heads,
            goals,
            goal_heads,
            erased_targets,
        },
        history,
        owner,
        early_fact_t: ids[0].1,
        fact_t: ids[1].1,
        abs_h: ids[2].0,
        abs_t: ids[2].1,
        persp_t: ids[3].1,
        erased_fact_t,
        unpinned_fact_h: ids[4].0,
        unpinned_fact_t: ids[4].1,
        root_goal_h,
        root_goal_t,
        child_goal_h,
        child_goal_t,
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one single-fault control per CausaViolation variant keeps the matrix complete"
)]
fn causa_oracle_planted_bug_controls() {
    let base = valid_causa_fixture();
    let empty = TraceHistory::default();
    assert_eq!(
        verify_causa_snapshot(&base.snapshot, &base.history),
        Ok(()),
        "valid baseline snapshot must pass"
    );

    // 1. FactHasOrigins (strictly earlier Fact as origin -> single fault, no time violation)
    let mut s = base.snapshot.clone();
    s.memories[1].origins.push(base.early_fact_t);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::FactHasOrigins { t: base.fact_t })
    );

    // 2. AbstractionMissingOrigins (refs present so DerivedMissingProvenance does not fire)
    let mut s = base.snapshot.clone();
    s.memories[2].origins.clear();
    s.memories[2].refs.push(base.fact_t);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::AbstractionMissingOrigins { t: base.abs_t })
    );

    // 3. AbstractionOriginBadKind (strictly earlier Perspective as origin -> single fault)
    let mut s = base.snapshot.clone();
    let bad_abs_h = Uuid::now_v7();
    let bad_abs_t = Uuid::now_v7();
    s.memories.push(fixture_memory(
        bad_abs_h,
        bad_abs_t,
        SnapshotMemoryKind::Abstraction,
        base.owner,
        vec![base.persp_t],
        Vec::new(),
        Vec::new(),
        10,
    ));
    s.memory_heads.push(MemoryHeadRow {
        handle: bad_abs_h,
        kind: SnapshotMemoryKind::Abstraction,
        owner_id: base.owner,
        t: bad_abs_t,
    });
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::AbstractionOriginBadKind {
            t: bad_abs_t,
            origin: base.persp_t,
        })
    );

    // 4. PerspectiveOriginBadKind (strictly earlier Fact directly as Perspective origin)
    let mut s = base.snapshot.clone();
    s.memories[3].origins = vec![base.fact_t];
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::PerspectiveOriginBadKind {
            t: base.persp_t,
            origin: base.fact_t,
        })
    );

    // 5. DerivedMissingProvenance + Principle 7 (`reads` populated, `origins` & `refs` empty)
    let mut s = base.snapshot.clone();
    s.memories[3].origins.clear();
    s.memories[3].refs.clear();
    s.memories[3].reads.push(base.fact_t);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::DerivedMissingProvenance { t: base.persp_t })
    );

    // 6. PinTimeOrderViolation (acyclic forward pin: tick 3 referencing tick 5)
    let mut s = base.snapshot.clone();
    s.memories[2].refs.push(base.unpinned_fact_t);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::PinTimeOrderViolation {
            source: base.abs_t,
            target: base.unpinned_fact_t,
        })
    );

    // 7. MemoryGoalIdCollision (standalone extra Goal reusing fact_t -> single fault)
    let mut s = base.snapshot.clone();
    let extra_h = Uuid::now_v7();
    s.goals.push(GoalRow {
        handle: extra_h,
        t: base.fact_t,
        owner_id: base.owner,
        state: SnapshotGoalState::Active,
        close_fact_t: None,
        assignment_t: None,
        dependency_t: Vec::new(),
        evidence_t: Vec::new(),
        motivated_by: Vec::new(),
        parent_handle: None,
        born_tick: 30,
    });
    s.goal_heads.push(GoalHeadRow {
        handle: extra_h,
        owner_id: base.owner,
        t: base.fact_t,
    });
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::MemoryGoalIdCollision { id: base.fact_t })
    );

    // 8. ErasedTargetLiveCollision
    let mut s = base.snapshot.clone();
    s.erased_targets.push(ErasedTargetRow {
        t: base.unpinned_fact_t,
        kind: SnapshotPinKind::Fact,
    });
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::ErasedTargetLiveCollision {
            id: base.unpinned_fact_t,
        })
    );

    // 9. MissingErasedTarget (unpinned erased row missing its tombstone)
    let mut s = base.snapshot.clone();
    let mut h = base.history.clone();
    s.memories.retain(|m| m.t != base.unpinned_fact_t);
    s.memory_heads.retain(|m| m.handle != base.unpinned_fact_h);
    h.erased.insert(base.unpinned_fact_t, SnapshotPinKind::Fact);
    assert_eq!(
        verify_causa_snapshot(&s, &h),
        Err(CausaViolation::MissingErasedTarget {
            t: base.unpinned_fact_t,
        })
    );

    // 10. ErasedTargetKindMismatch (unpinned Fact erased with wrong tombstone kind)
    let mut s = base.snapshot.clone();
    let mut h = base.history.clone();
    s.memories.retain(|m| m.t != base.unpinned_fact_t);
    s.memory_heads.retain(|m| m.handle != base.unpinned_fact_h);
    s.erased_targets.push(ErasedTargetRow {
        t: base.unpinned_fact_t,
        kind: SnapshotPinKind::Perspective,
    });
    h.erased.insert(base.unpinned_fact_t, SnapshotPinKind::Fact);
    assert_eq!(
        verify_causa_snapshot(&s, &h),
        Err(CausaViolation::ErasedTargetKindMismatch {
            t: base.unpinned_fact_t,
            expected: SnapshotPinKind::Fact,
            actual: SnapshotPinKind::Perspective,
        })
    );

    // 11. DanglingMemoryPin (tombstone absent for pinned erased Fact)
    let mut s = base.snapshot.clone();
    s.erased_targets.clear();
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::DanglingMemoryPin {
            source: base.abs_t,
            field: "origins",
            target: base.erased_fact_t,
        })
    );

    // 12. DanglingGoalPin (unknown Goal in Fact.goal_refs)
    let ghost_goal = Uuid::now_v7();
    let mut s = base.snapshot.clone();
    s.memories[1].goal_refs.push(ghost_goal);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::DanglingGoalPin {
            source: base.fact_t,
            field: "goal_refs",
            target: ghost_goal,
        })
    );

    // 13. NonPerspectiveGuidedBy
    let mut s = base.snapshot.clone();
    s.memories[2].guided_by.push(base.root_goal_t);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::NonPerspectiveGuidedBy { t: base.abs_t })
    );

    // 14. BadMemoryHeadAlignment (historical version owner mismatch while head aligns)
    let mut s = base.snapshot.clone();
    let older_abs_t = Uuid::now_v7();
    s.memories.push(fixture_memory(
        base.abs_h,
        older_abs_t,
        SnapshotMemoryKind::Abstraction,
        Uuid::now_v7(),
        vec![base.fact_t],
        Vec::new(),
        Vec::new(),
        3,
    ));
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::BadMemoryHeadAlignment { handle: base.abs_h })
    );

    // 15. ForkAnchorMisaligned (two versions of same handle disagree on forked_from)
    let mut s = base.snapshot.clone();
    let second_abs_t = Uuid::now_v7();
    let mut second_abs = fixture_memory(
        base.abs_h,
        second_abs_t,
        SnapshotMemoryKind::Abstraction,
        base.owner,
        vec![base.fact_t],
        Vec::new(),
        Vec::new(),
        10,
    );
    second_abs.forked_from = Some(base.abs_t);
    s.memories.push(second_abs);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::ForkAnchorMisaligned { handle: base.abs_h })
    );

    // 16. PinRetargeted (Principle 5: surviving admission's `reads` mutated)
    let mut s = base.snapshot.clone();
    s.memories[2].reads.push(base.early_fact_t);
    assert_eq!(
        verify_causa_snapshot(&s, &base.history),
        Err(CausaViolation::PinRetargeted {
            t: base.abs_t,
            field: "reads",
        })
    );

    // 17. GoalInMemoryOrigins (Principle 8: Goal placed in Abstraction origins)
    let mut s = base.snapshot.clone();
    s.memories[2].origins.push(base.root_goal_t);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::GoalInMemoryOrigins {
            t: base.abs_t,
            goal_id: base.root_goal_t,
        })
    );

    // 18. GuidanceOverlapsOrigins (Principle 8: structurally coupled with GoalInMemoryOrigins
    // because guided_by targets live in Goal space while origins targets live in Memory space)
    let mut s = base.snapshot.clone();
    s.memories[3].origins.push(base.root_goal_t);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::GuidanceOverlapsOrigins {
            t: base.persp_t,
            goal_id: base.root_goal_t,
        })
    );

    // 19. BadGoalHeadAlignment (goal_head points to non-existent Goal row)
    let mut s = base.snapshot.clone();
    s.goal_heads[0].t = Uuid::now_v7();
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::BadGoalHeadAlignment {
            handle: base.root_goal_h,
        })
    );

    // 20. GoalTreeCycle (both goals Active so no open-under-closed fault co-occurs;
    // any cycle mathematically implies a born_tick inversion on at least one edge)
    let mut s = base.snapshot.clone();
    s.goals[1].state = SnapshotGoalState::Active;
    s.goals[1].close_fact_t = None;
    s.goals[0].parent_handle = Some(base.child_goal_h);
    assert!(matches!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::GoalTreeCycle { .. })
    ));

    // 21. GoalMissingParent
    let missing_parent = Uuid::now_v7();
    let mut s = base.snapshot.clone();
    s.goals[1].parent_handle = Some(missing_parent);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::GoalMissingParent {
            handle: base.child_goal_h,
            parent_handle: missing_parent,
        })
    );

    // 22. GoalParentNotOlder (valid acyclic parent born after child)
    let mut s = base.snapshot.clone();
    s.goals[0].born_tick = 25;
    s.goals[1].born_tick = 20;
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::GoalParentNotOlder {
            child: base.child_goal_h,
            parent: base.root_goal_h,
        })
    );

    // 23. OpenGoalUnderClosedAncestor
    let mut s = base.snapshot.clone();
    s.goals[0].state = SnapshotGoalState::Achieved;
    s.goals[0].close_fact_t = Some(base.fact_t);
    s.goals[1].state = SnapshotGoalState::Active;
    s.goals[1].close_fact_t = None;
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::OpenGoalUnderClosedAncestor {
            child: base.child_goal_h,
            ancestor: base.root_goal_h,
        })
    );

    // 24. BadGoalAssignment (Fact instead of Perspective)
    let mut s = base.snapshot.clone();
    s.goals[0].assignment_t = Some(base.fact_t);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::BadGoalAssignment {
            goal_t: base.root_goal_t,
            target: base.fact_t,
        })
    );

    // 25. BadGoalEvidence (Perspective instead of Fact/Abstraction)
    let mut s = base.snapshot.clone();
    s.goals[0].evidence_t.push(base.persp_t);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::BadGoalEvidence {
            goal_t: base.root_goal_t,
            target: base.persp_t,
        })
    );

    // 26. BadGoalMotivation (Abstraction instead of Perspective)
    let mut s = base.snapshot.clone();
    s.goals[0].motivated_by = vec![base.abs_t];
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::BadGoalMotivation {
            goal_t: base.root_goal_t,
            target: base.abs_t,
        })
    );

    // 27. BadGoalDependency (non-existent Goal ID)
    let ghost_dep = Uuid::now_v7();
    let mut s = base.snapshot.clone();
    s.goals[1].dependency_t.push(ghost_dep);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::BadGoalDependency {
            goal_t: base.child_goal_t,
            target: ghost_dep,
        })
    );

    // 28. BadGoalTerminalCloseFact (Achieved goal with Abstraction close_fact_t)
    let mut s = base.snapshot.clone();
    s.goals[1].close_fact_t = Some(base.abs_t);
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::BadGoalTerminalCloseFact {
            goal_t: base.child_goal_t,
        })
    );

    // 29. DuplicateMemoryVersion (two Memory rows sharing the same version id t)
    let mut s = base.snapshot.clone();
    s.memories.push(s.memories[1].clone());
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::DuplicateMemoryVersion { t: base.fact_t })
    );

    // 30. DuplicateGoalVersion (two Goal rows sharing the same version id t)
    let mut s = base.snapshot.clone();
    s.goals.push(s.goals[0].clone());
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::DuplicateGoalVersion {
            t: base.root_goal_t,
        })
    );

    // 31. DuplicateErasedTarget (two tombstones sharing the same target id t)
    let mut s = base.snapshot.clone();
    s.erased_targets.push(s.erased_targets[0].clone());
    assert_eq!(
        verify_causa_snapshot(&s, &empty),
        Err(CausaViolation::DuplicateErasedTarget {
            t: base.erased_fact_t,
        })
    );

    // 32. UnrecordedMemoryDisappearance (observed unpinned Memory vanishes without erase history)
    let mut s = base.snapshot.clone();
    s.memories.retain(|m| m.t != base.unpinned_fact_t);
    s.memory_heads.retain(|h| h.handle != base.unpinned_fact_h);
    assert_eq!(
        verify_causa_snapshot(&s, &base.history),
        Err(CausaViolation::UnrecordedMemoryDisappearance {
            t: base.unpinned_fact_t,
        })
    );
}

// ---------------------------------------------------------------------------
// 3. Live Postgres Loader & Stateful Randomized Trace Driver
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct MemoryDbRow {
    handle: Uuid,
    t: Uuid,
    kind: String,
    owner_id: Uuid,
    origins: Vec<Uuid>,
    refs: Vec<Uuid>,
    goal_refs: Vec<Uuid>,
    guided_by: Vec<Uuid>,
    forked_from: Option<Uuid>,
    reads: Vec<Uuid>,
    forgotten: bool,
}

#[derive(sqlx::FromRow)]
struct GoalDbRow {
    handle: Uuid,
    t: Uuid,
    owner_id: Uuid,
    state: String,
    close_fact_t: Option<Uuid>,
    assignment_t: Option<Uuid>,
    dependency_t: Vec<Uuid>,
    evidence_t: Vec<Uuid>,
    motivated_by: Vec<Uuid>,
    parent_handle: Option<Uuid>,
}

struct TickLedger {
    next_tick: u64,
    memory_ticks: HashMap<Uuid, u64>,
    goal_born_ticks: HashMap<Uuid, u64>,
}

impl TickLedger {
    fn new() -> Self {
        Self {
            next_tick: 1,
            memory_ticks: HashMap::new(),
            goal_born_ticks: HashMap::new(),
        }
    }

    fn tick_for_memory(&mut self, t: Uuid) -> u64 {
        *self.memory_ticks.entry(t).or_insert_with(|| {
            let tick = self.next_tick;
            self.next_tick += 1;
            tick
        })
    }

    fn born_for_goal(&mut self, handle: Uuid) -> u64 {
        *self.goal_born_ticks.entry(handle).or_insert_with(|| {
            let tick = self.next_tick;
            self.next_tick += 1;
            tick
        })
    }
}

async fn load_memories(
    admin_pool: &PgPool,
    ticks: &mut TickLedger,
) -> TestResult<(Vec<MemoryRow>, Vec<MemoryHeadRow>)> {
    let rows: Vec<MemoryDbRow> = sqlx::query_as(
        "SELECT handle, t, kind::text AS kind, owner_id, origins, refs, goal_refs, guided_by,
                forked_from, reads, (forgotten_at IS NOT NULL) AS forgotten
           FROM proxiima_core.memory
          ORDER BY t ASC",
    )
    .fetch_all(admin_pool)
    .await?;
    let mut memories = Vec::with_capacity(rows.len());
    for row in rows {
        let tick = ticks.tick_for_memory(row.t);
        memories.push(MemoryRow {
            handle: row.handle,
            t: row.t,
            kind: SnapshotMemoryKind::parse(&row.kind)?,
            owner_id: row.owner_id,
            origins: row.origins,
            refs: row.refs,
            goal_refs: row.goal_refs,
            guided_by: row.guided_by,
            forked_from: row.forked_from,
            reads: row.reads,
            forgotten: row.forgotten,
            tick,
        });
    }
    let head_rows: Vec<(Uuid, String, Uuid, Uuid)> = sqlx::query_as(
        "SELECT handle, kind::text, owner_id, t
           FROM proxiima_core.memory_head",
    )
    .fetch_all(admin_pool)
    .await?;
    let mut heads = Vec::with_capacity(head_rows.len());
    for (handle, kind, owner_id, t) in head_rows {
        heads.push(MemoryHeadRow {
            handle,
            kind: SnapshotMemoryKind::parse(&kind)?,
            owner_id,
            t,
        });
    }
    Ok((memories, heads))
}

async fn load_goals(
    admin_pool: &PgPool,
    ticks: &mut TickLedger,
) -> TestResult<(Vec<GoalRow>, Vec<GoalHeadRow>)> {
    let rows: Vec<GoalDbRow> = sqlx::query_as(
        "SELECT handle, t, owner_id, state::text AS state, close_fact_t, assignment_t,
                dependency_t, evidence_t, motivated_by, parent_handle
           FROM proxiima_core.goal
          ORDER BY t ASC",
    )
    .fetch_all(admin_pool)
    .await?;
    let mut goals = Vec::with_capacity(rows.len());
    for row in rows {
        let born_tick = ticks.born_for_goal(row.handle);
        goals.push(GoalRow {
            handle: row.handle,
            t: row.t,
            owner_id: row.owner_id,
            state: SnapshotGoalState::parse(&row.state)?,
            close_fact_t: row.close_fact_t,
            assignment_t: row.assignment_t,
            dependency_t: row.dependency_t,
            evidence_t: row.evidence_t,
            motivated_by: row.motivated_by,
            parent_handle: row.parent_handle,
            born_tick,
        });
    }
    let head_rows: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT handle, owner_id, t
           FROM proxiima_core.goal_head",
    )
    .fetch_all(admin_pool)
    .await?;
    let heads = head_rows
        .into_iter()
        .map(|(handle, owner_id, t)| GoalHeadRow {
            handle,
            owner_id,
            t,
        })
        .collect();
    Ok((goals, heads))
}

async fn load_causa_snapshot(
    admin_pool: &PgPool,
    ticks: &mut TickLedger,
) -> TestResult<CausaGraphSnapshot> {
    let (memories, memory_heads) = load_memories(admin_pool, ticks).await?;
    let (goals, goal_heads) = load_goals(admin_pool, ticks).await?;
    let erased_rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT t, kind::text
           FROM proxiima_core.erased_pin_target",
    )
    .fetch_all(admin_pool)
    .await?;
    let mut erased_targets = Vec::with_capacity(erased_rows.len());
    for (t, kind) in erased_rows {
        erased_targets.push(ErasedTargetRow {
            t,
            kind: SnapshotPinKind::parse(&kind)?,
        });
    }
    Ok(CausaGraphSnapshot {
        memories,
        memory_heads,
        goals,
        goal_heads,
        erased_targets,
    })
}

/// Deterministic xorshift64* PRNG so every failing trace reproduces from its seed.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(if seed == 0 { DEFAULT_SEED } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn range(&mut self, bound: usize) -> usize {
        if bound <= 1 {
            return 0;
        }
        usize::try_from(self.next_u64() % (bound as u64)).expect("modulo fits usize")
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            Some(&items[self.range(items.len())])
        }
    }
}

#[derive(Debug, Default)]
struct TraceCoverage {
    facts: usize,
    reflective_facts: usize,
    reflective_derivations: usize,
    abstractions: usize,
    perspectives: usize,
    interpretations: usize,
    revisions: usize,
    forks: usize,
    forgets: usize,
    restores: usize,
    hard_erases: usize,
    goals_created: usize,
    goals_decomposed: usize,
    goals_paused: usize,
    goals_resumed: usize,
    goals_modified: usize,
    goals_achieved: usize,
    goals_abandoned: usize,
    refused_open_child_close: usize,
    refused_illegal_origin: usize,
    refused_stale_inherited_pin: usize,
    refused_erased_fork_source: usize,
}

#[derive(Debug, Clone)]
struct TrackedGoal {
    handle: Uuid,
    head_t: GoalId,
    state: SnapshotGoalState,
    parent_handle: Option<Uuid>,
    motivated_by: Vec<MemoryId>,
    evidence: Vec<MemoryId>,
}

struct OwnerTrack {
    owner: OwnerId,
    authz: AuthzContext,
    facts: Vec<MemoryId>,
    reflective_facts: HashMap<MemoryId, GoalId>,
    abstractions: Vec<MemoryId>,
    perspectives: Vec<MemoryId>,
    interpretations: Vec<MemoryId>,
    forgotten: HashSet<MemoryId>,
    erased: HashSet<MemoryId>,
    goals: Vec<TrackedGoal>,
}

impl OwnerTrack {
    fn new(owner: OwnerId) -> Self {
        Self {
            owner,
            authz: authenticated_context(AuthzContext::single_owner(&owner, AuthPath::HostBearer)),
            facts: Vec::new(),
            reflective_facts: HashMap::new(),
            abstractions: Vec::new(),
            perspectives: Vec::new(),
            interpretations: Vec::new(),
            forgotten: HashSet::new(),
            erased: HashSet::new(),
            goals: Vec::new(),
        }
    }

    fn live_facts(&self) -> Vec<MemoryId> {
        self.facts
            .iter()
            .copied()
            .filter(|id| !self.forgotten.contains(id))
            .collect()
    }

    fn live_abstractions(&self) -> Vec<MemoryId> {
        self.abstractions
            .iter()
            .copied()
            .filter(|id| !self.forgotten.contains(id))
            .collect()
    }

    fn live_derived_perspectives(&self) -> Vec<MemoryId> {
        self.perspectives
            .iter()
            .copied()
            .filter(|id| !self.forgotten.contains(id))
            .collect()
    }

    fn live_interpretations(&self) -> Vec<MemoryId> {
        self.interpretations
            .iter()
            .copied()
            .filter(|id| !self.forgotten.contains(id))
            .collect()
    }

    fn live_all_perspectives(&self) -> Vec<MemoryId> {
        let mut out = self.live_derived_perspectives();
        out.extend(self.live_interpretations());
        out
    }

    fn open_goals(&self) -> Vec<TrackedGoal> {
        self.goals
            .iter()
            .filter(|g| !g.state.is_terminal())
            .cloned()
            .collect()
    }

    fn has_open_child(&self, handle: Uuid) -> bool {
        self.goals
            .iter()
            .any(|g| g.parent_handle == Some(handle) && !g.state.is_terminal())
    }

    fn has_nonlive(&self, ids: &[MemoryId]) -> bool {
        ids.iter()
            .any(|id| self.forgotten.contains(id) || self.erased.contains(id))
    }

    fn kind_of(&self, t: MemoryId) -> SnapshotPinKind {
        if self.facts.contains(&t) {
            SnapshotPinKind::Fact
        } else if self.abstractions.contains(&t) {
            SnapshotPinKind::Abstraction
        } else {
            SnapshotPinKind::Perspective
        }
    }

    fn remove_erased(&mut self, t: MemoryId) {
        self.facts.retain(|&id| id != t);
        self.reflective_facts.remove(&t);
        self.abstractions.retain(|&id| id != t);
        self.perspectives.retain(|&id| id != t);
        self.interpretations.retain(|&id| id != t);
        self.forgotten.remove(&t);
        self.erased.insert(t);
    }
}

fn assert_stale_motivation_refusal(err: &ProtocolError) {
    assert!(
        err.code == ErrorCode::InvalidArgument
            && err
                .message
                .contains("goal motivation perspective does not exist"),
        "expected stale motivation perspective InvalidArgument, got {err:?}"
    );
}

fn assert_stale_evidence_refusal(err: &ProtocolError) {
    assert!(
        err.code == ErrorCode::Forbidden && err.message == "entry not found",
        "expected anti-oracle stale evidence Forbidden(\"entry not found\"), got {err:?}"
    );
}

struct OracleWorld {
    pg: PgStorage,
    admin_pool: PgPool,
    engine: Engine,
    erase_ctx: PgHostStateEraseContext,
    owners: [OwnerTrack; 2],
    ticks: TickLedger,
    history: TraceHistory,
    coverage: TraceCoverage,
    seq: usize,
    _db: DbGuard,
}

impl OracleWorld {
    async fn new() -> TestResult<Self> {
        let (pg, db) = fresh_pg("causa_oracle").await;
        let pg = pg.with_cold(Arc::new(MemoryColdStore));
        let frozen = SpiiceRegistry::new().freeze_or_panic_for_tests();
        let erase_ctx =
            PgHostStateEraseContext::for_surfaces_for_tests(OwnerSurfaces::for_registry(&frozen))?;
        let engine = Engine::new_or_panic_for_tests(frozen, Arc::new(pg.clone()).storage_ports());
        let admin_pool = PgPool::connect(&db_url(db.name())).await?;
        let owner_a = OwnerId::from(UserId::new(Uuid::now_v7()));
        let owner_b = OwnerId::from(UserId::new(Uuid::now_v7()));
        Ok(Self {
            pg,
            admin_pool,
            engine,
            erase_ctx,
            owners: [OwnerTrack::new(owner_a), OwnerTrack::new(owner_b)],
            ticks: TickLedger::new(),
            history: TraceHistory::default(),
            coverage: TraceCoverage::default(),
            seq: 0,
            _db: db,
        })
    }

    fn next_seq(&mut self) -> usize {
        self.seq += 1;
        self.seq
    }

    fn derivation_identity(&self) -> DerivationIdentity {
        let seed = format!("causa-oracle/op-{}", self.seq);
        DerivationIdentity::new(
            OperatorId::new(Uuid::now_v7()),
            InputContractId::new(Uuid::new_v5(&Uuid::NAMESPACE_URL, seed.as_bytes())),
        )
    }

    async fn persisted_version_sets(&self) -> TestResult<(Vec<Uuid>, Vec<Uuid>)> {
        let memories: Vec<Uuid> =
            sqlx::query_scalar("SELECT t FROM proxiima_core.memory ORDER BY t ASC")
                .fetch_all(&self.admin_pool)
                .await?;
        let goals: Vec<Uuid> =
            sqlx::query_scalar("SELECT t FROM proxiima_core.goal ORDER BY t ASC")
                .fetch_all(&self.admin_pool)
                .await?;
        Ok((memories, goals))
    }

    async fn assert_persisted_versions_unchanged(
        &self,
        expected: &(Vec<Uuid>, Vec<Uuid>),
        context: &str,
    ) -> TestResult {
        let actual = self.persisted_version_sets().await?;
        assert_eq!(
            actual, *expected,
            "{context} must not commit any memory or goal rows"
        );
        Ok(())
    }

    async fn assert_goal_refusal_unchanged(
        &self,
        handle: Uuid,
        expected_head: GoalId,
        expected_versions: &(Vec<Uuid>, Vec<Uuid>),
        context: &str,
    ) -> TestResult {
        let actual: Uuid =
            sqlx::query_scalar("SELECT t FROM proxiima_core.goal_head WHERE handle = $1")
                .bind(handle)
                .fetch_one(&self.admin_pool)
                .await?;
        assert_eq!(
            actual,
            expected_head.into_inner(),
            "{context} must not advance goal_head"
        );
        self.assert_persisted_versions_unchanged(expected_versions, context)
            .await
    }

    async fn check_invariants(
        &mut self,
        step: usize,
        op_name: &str,
    ) -> TestResult<CausaGraphSnapshot> {
        let snapshot = load_causa_snapshot(&self.admin_pool, &mut self.ticks).await?;
        if let Err(err) = verify_causa_snapshot(&snapshot, &self.history) {
            return Err(format!("Causa violation at step {step} after {op_name}: {err:?}").into());
        }
        for row in &snapshot.memories {
            self.history
                .memories
                .entry(row.t)
                .or_insert_with(|| PinnedMemoryRecord::from_row(row));
        }
        Ok(snapshot)
    }

    async fn op_remember_fact(
        &mut self,
        idx: usize,
        rng: &mut Rng,
        force_reflective: bool,
    ) -> TestResult<Option<(MemoryId, Option<GoalId>)>> {
        let seq = self.next_seq();
        let mut targets = Vec::new();
        let owner_track = &self.owners[idx];
        let live_facts = owner_track.live_facts();
        if let Some(&f) = rng.pick(&live_facts)
            && rng.range(2) == 0
        {
            targets.push(EdgeEndpoint::memory(MemoryKind::Fact, f));
        }
        let observed_goal = rng
            .pick(&owner_track.goals)
            .filter(|_| force_reflective || rng.range(2) == 0)
            .map(|g| g.head_t);
        if let Some(goal_id) = observed_goal {
            targets.push(EdgeEndpoint::goal(goal_id));
        }
        let payload = AgentNoteV1 {
            note_id: Uuid::now_v7(),
            title: format!("oracle-fact-{seq}"),
            body: format!("observed fact at step {seq}"),
            tags: vec!["oracle".into()],
            idempotency_key: None,
        };
        let sidecars = [SidecarPayload::fact(payload.clone())];
        let mut cmd = FactWriteCommand::from_payload("oracle/fact", &payload)
            .with_additional_references(targets);
        cmd.lexical_language = Some(LEXICAL_LANGUAGE_DEPLOYMENT_DEFAULT.to_owned());
        let auth = self
            .engine
            .authorize_fact_ingest(&self.owners[idx].authz, cmd, &sidecars)
            .await?;
        let outcome = self.engine.ingest_authorized_fact(&auth).await?;
        self.owners[idx].facts.push(outcome.memory_id);
        self.coverage.facts += 1;
        if let Some(goal_id) = observed_goal {
            // Verify postcondition against live Postgres: `goal_refs` carries the observed Goal.
            let stored_goal_refs: Vec<Uuid> =
                sqlx::query_scalar("SELECT goal_refs FROM proxiima_core.memory WHERE t = $1")
                    .bind(outcome.memory_id.into_inner())
                    .fetch_one(&self.admin_pool)
                    .await?;
            assert!(
                stored_goal_refs.contains(&goal_id.into_inner()),
                "reflective fact {} must persist goal_refs containing {}",
                outcome.memory_id.into_inner(),
                goal_id.into_inner()
            );
            self.owners[idx]
                .reflective_facts
                .insert(outcome.memory_id, goal_id);
            self.coverage.reflective_facts += 1;
        }
        Ok(Some((outcome.memory_id, observed_goal)))
    }

    async fn op_derive_abstraction_from(
        &mut self,
        idx: usize,
        origins: Vec<MemoryId>,
        target: MemoryTarget,
    ) -> TestResult<MemoryId> {
        let seq = self.next_seq();
        let is_revision = matches!(target, MemoryTarget::Revision(_));
        let reflective_origins: Vec<(MemoryId, GoalId)> = origins
            .iter()
            .filter_map(|id| {
                self.owners[idx]
                    .reflective_facts
                    .get(id)
                    .map(|&goal_id| (*id, goal_id))
            })
            .collect();
        let payload = AgentDerivationV1 {
            title: format!("abs-{seq}"),
            body: format!("abstraction derived at step {seq}"),
            tags: vec!["oracle".into()],
            idempotency_key: None,
            source_memory_ids: Vec::new(),
            model_id: "causa-oracle".into(),
            client_name: "causa-oracle".into(),
            client_version: "1".into(),
        };
        let derived = DerivedMemory::abstraction(
            target,
            self.owners[idx].owner,
            format!("abs-{seq}"),
            payload,
            origins,
            self.derivation_identity(),
        )?;
        let outcome = self
            .engine
            .derive_memory(&self.owners[idx].authz, derived)
            .await?;
        self.owners[idx].abstractions.push(outcome.memory_id);
        self.coverage.abstractions += 1;
        if is_revision {
            self.coverage.revisions += 1;
        }
        if !reflective_origins.is_empty() {
            let stored_origins: Vec<Uuid> =
                sqlx::query_scalar("SELECT origins FROM proxiima_core.memory WHERE t = $1")
                    .bind(outcome.memory_id.into_inner())
                    .fetch_one(&self.admin_pool)
                    .await?;
            for (refl_fact, goal_id) in reflective_origins {
                assert!(
                    stored_origins.contains(&refl_fact.into_inner()),
                    "reflective derivation must retain reflective fact origin"
                );
                assert!(
                    !stored_origins.contains(&goal_id.into_inner()),
                    "reflective derivation must keep observed goal outside origins"
                );
            }
            self.coverage.reflective_derivations += 1;
        }
        Ok(outcome.memory_id)
    }

    async fn op_derive_abstraction(
        &mut self,
        idx: usize,
        rng: &mut Rng,
        revise: bool,
    ) -> TestResult {
        let live_facts = self.owners[idx].live_facts();
        let live_abs = self.owners[idx].live_abstractions();
        let mut candidates = live_facts;
        candidates.extend(live_abs.iter().copied());
        let Some(&origin_a) = rng.pick(&candidates) else {
            return Ok(());
        };
        let mut origins = vec![origin_a];
        if let Some(&origin_b) = rng.pick(&candidates)
            && origin_b != origin_a
            && rng.range(2) == 0
        {
            origins.push(origin_b);
        }
        let target = if revise && let Some(&prior) = rng.pick(&live_abs) {
            MemoryTarget::Revision(prior)
        } else {
            MemoryTarget::Series(SeriesHandle::new(Uuid::now_v7()))
        };
        self.op_derive_abstraction_from(idx, origins, target)
            .await?;
        Ok(())
    }

    async fn op_derive_perspective(
        &mut self,
        idx: usize,
        rng: &mut Rng,
        interpretation: bool,
    ) -> TestResult {
        let seq = self.next_seq();
        let owner = self.owners[idx].owner;
        let open_goals = self.owners[idx].open_goals();
        let guided: Vec<GoalId> = rng
            .pick(&open_goals)
            .filter(|_| rng.range(2) == 0)
            .map(|g| g.head_t)
            .into_iter()
            .collect();

        if interpretation {
            let live_facts = self.owners[idx].live_facts();
            let Some(&subject) = rng.pick(&live_facts) else {
                return Ok(());
            };
            let live_interps = self.owners[idx].live_interpretations();
            let target = if rng.range(3) == 0
                && let Some(&prior) = rng.pick(&live_interps)
            {
                MemoryTarget::Revision(prior)
            } else {
                MemoryTarget::Series(SeriesHandle::new(Uuid::now_v7()))
            };
            let is_revision = matches!(target, MemoryTarget::Revision(_));
            let payload = InterpretationV1 {
                claim: format!("interp-{seq}"),
                confidence: InterpretationConfidence::try_from(75_i16).expect("valid confidence"),
                subjects: vec![(subject, MemoryKind::Fact)],
                model_id: "causa-oracle".into(),
                client_name: "causa-oracle".into(),
                client_version: "1".into(),
            };
            let derived = DerivedMemory::guided_interpretation(
                target,
                owner,
                format!("interp-{seq}"),
                payload,
                guided,
            );
            let outcome = self
                .engine
                .derive_memory(&self.owners[idx].authz, derived)
                .await?;
            self.owners[idx].interpretations.push(outcome.memory_id);
            self.coverage.perspectives += 1;
            self.coverage.interpretations += 1;
            if is_revision {
                self.coverage.revisions += 1;
            }
            return Ok(());
        }

        let live_abs = self.owners[idx].live_abstractions();
        let Some(&origin) = rng.pick(&live_abs) else {
            return Ok(());
        };
        let live_persp = self.owners[idx].live_derived_perspectives();
        let target = if rng.range(3) == 0
            && let Some(&prior) = rng.pick(&live_persp)
        {
            MemoryTarget::Revision(prior)
        } else {
            MemoryTarget::Series(SeriesHandle::new(Uuid::now_v7()))
        };
        let is_revision = matches!(target, MemoryTarget::Revision(_));
        let payload = AgentDerivationV1 {
            title: format!("persp-{seq}"),
            body: format!("perspective at step {seq}"),
            tags: vec!["oracle".into()],
            idempotency_key: None,
            source_memory_ids: Vec::new(),
            model_id: "causa-oracle".into(),
            client_name: "causa-oracle".into(),
            client_version: "1".into(),
        };
        let derived = DerivedMemory::guided_perspective(
            target,
            owner,
            format!("persp-{seq}"),
            payload,
            vec![origin],
            self.derivation_identity(),
            guided,
        )?;
        let outcome = self
            .engine
            .derive_memory(&self.owners[idx].authz, derived)
            .await?;
        self.owners[idx].perspectives.push(outcome.memory_id);
        self.coverage.perspectives += 1;
        if is_revision {
            self.coverage.revisions += 1;
        }
        Ok(())
    }

    async fn op_fork_memory(&mut self, idx: usize, rng: &mut Rng) -> TestResult {
        let seq = self.next_seq();
        let mut ap: Vec<(MemoryId, MemoryKind)> = self.owners[idx]
            .live_abstractions()
            .into_iter()
            .map(|id| (id, MemoryKind::Abstraction))
            .collect();
        ap.extend(
            self.owners[idx]
                .live_derived_perspectives()
                .into_iter()
                .map(|id| (id, MemoryKind::Perspective)),
        );
        let Some(&(source, kind)) = rng.pick(&ap) else {
            return Ok(());
        };
        let source_has_erased_pin =
            self.history
                .memories
                .get(&source.into_inner())
                .is_some_and(|rec| {
                    rec.origins
                        .iter()
                        .chain(rec.refs.iter())
                        .any(|t| self.history.erased.contains_key(t))
                });
        let before_versions = if source_has_erased_pin {
            Some(self.persisted_version_sets().await?)
        } else {
            None
        };
        let res = self
            .engine
            .fork_memory(
                &self.owners[idx].authz,
                &ForkMemoryRequest {
                    source,
                    destination: MemoryDestination::Owner(self.owners[idx].owner),
                    request_id: IdempotencyKey::new(format!("fork-{seq}")).expect("key"),
                },
            )
            .await;
        if let Some(before) = before_versions {
            let err = res.expect_err("forking source with hard-erased origin/ref must be refused");
            assert_eq!(
                err.code,
                ErrorCode::NotFound,
                "expected NotFound for erased fork pin, got {err:?}"
            );
            self.assert_persisted_versions_unchanged(&before, "refused erased-pin fork")
                .await?;
            self.coverage.refused_erased_fork_source += 1;
            return Ok(());
        }
        let outcome = res?;
        match kind {
            MemoryKind::Abstraction => self.owners[idx].abstractions.push(outcome.memory_id),
            MemoryKind::Perspective => self.owners[idx].perspectives.push(outcome.memory_id),
            MemoryKind::Fact => unreachable!("facts are never forked"),
        }
        self.coverage.forks += 1;
        Ok(())
    }

    async fn op_forget_or_restore(
        &mut self,
        idx: usize,
        rng: &mut Rng,
        restore: bool,
    ) -> TestResult {
        if restore {
            let mut forgotten: Vec<MemoryId> = self.owners[idx].forgotten.iter().copied().collect();
            forgotten.sort_by_key(|id| id.into_inner());
            let Some(&target) = rng.pick(&forgotten) else {
                return Ok(());
            };
            self.engine
                .restore_memory(&self.owners[idx].authz, self.owners[idx].owner, target)
                .await?;
            let is_forgotten: bool = sqlx::query_scalar(
                "SELECT (forgotten_at IS NOT NULL) FROM proxiima_core.memory WHERE t = $1",
            )
            .bind(target.into_inner())
            .fetch_one(&self.admin_pool)
            .await?;
            assert!(
                !is_forgotten,
                "restore_memory must clear forgotten_at on {}",
                target.into_inner()
            );
            self.owners[idx].forgotten.remove(&target);
            self.coverage.restores += 1;
            return Ok(());
        }
        let mut live = self.owners[idx].live_facts();
        live.extend(self.owners[idx].live_abstractions());
        live.extend(self.owners[idx].live_all_perspectives());
        if live.len() <= 3 {
            return Ok(());
        }
        let Some(&target) = rng.pick(&live) else {
            return Ok(());
        };
        self.engine
            .forget_memory(&self.owners[idx].authz, self.owners[idx].owner, target)
            .await?;
        let is_forgotten: bool = sqlx::query_scalar(
            "SELECT (forgotten_at IS NOT NULL) FROM proxiima_core.memory WHERE t = $1",
        )
        .bind(target.into_inner())
        .fetch_one(&self.admin_pool)
        .await?;
        assert!(
            is_forgotten,
            "forget_memory must mark forgotten_at on {}",
            target.into_inner()
        );
        self.owners[idx].forgotten.insert(target);
        self.coverage.forgets += 1;
        Ok(())
    }

    async fn hard_erase_target(&mut self, idx: usize, target: MemoryId) -> TestResult {
        let expected_kind = self.owners[idx].kind_of(target);
        let owner = self.owners[idx].owner;
        let mut tx = self.admin_pool.begin().await?;
        let _plan = erase_memory(
            &mut tx,
            self.pg.sidecars(),
            &self.erase_ctx,
            &owner,
            target.into_inner(),
        )
        .await?;
        tx.commit().await?;

        // Verify postconditions: row removed from memory and tombstone recorded with exact kind.
        let still_exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM proxiima_core.memory WHERE t = $1)")
                .bind(target.into_inner())
                .fetch_one(&self.admin_pool)
                .await?;
        assert!(
            !still_exists,
            "erase_memory must physically delete {}",
            target.into_inner()
        );
        self.owners[idx].remove_erased(target);
        self.history
            .erased
            .insert(target.into_inner(), expected_kind);
        self.coverage.hard_erases += 1;
        Ok(())
    }

    async fn op_hard_erase_memory(&mut self, idx: usize, rng: &mut Rng) -> TestResult {
        let mut candidates = self.owners[idx].facts.clone();
        candidates.extend(self.owners[idx].abstractions.iter().copied());
        candidates.extend(self.owners[idx].perspectives.iter().copied());
        candidates.extend(self.owners[idx].interpretations.iter().copied());
        if candidates.len() <= 4 {
            return Ok(());
        }
        let Some(&target) = rng.pick(&candidates) else {
            return Ok(());
        };
        self.hard_erase_target(idx, target).await
    }

    async fn op_create_goal(&mut self, idx: usize, rng: &mut Rng) -> TestResult<Option<GoalId>> {
        let seq = self.next_seq();
        let live_persp = self.owners[idx].live_all_perspectives();
        let Some(&persp) = rng.pick(&live_persp) else {
            return Ok(None);
        };
        let live_facts = self.owners[idx].live_facts();
        let evidence_id = rng.pick(&live_facts).copied();
        let evidence = evidence_id
            .map(|f| vec![GoalEvidenceRef::new(f)])
            .unwrap_or_default();
        let deps = rng
            .pick(&self.owners[idx].goals)
            .filter(|_| rng.range(2) == 0)
            .map(|g| vec![GoalDependencyRef::new(g.head_t)])
            .unwrap_or_default();
        let topology =
            GoalTopologyWrite::new(vec![GoalMotivationRef::perspective(persp)], deps, evidence)?;
        let req = GoalCreatePayloadWriteRequest {
            owner: self.owners[idx].owner,
            topology,
            payload: GoalPayloadWrite {
                schema_id: SchemaId::new("core/simple-text-v1".into()),
                schema_version: SchemaVersion::new(1),
                title: format!("goal-{seq}"),
                text: format!("goal text {seq}"),
                payload: Vec::new(),
                sidecar_payload: None,
            },
            request_id: IdempotencyKey::new(format!("goal-create-{seq}")).expect("key"),
            authorship: GoalAuthorship::User,
            author_self_perspective_id: Some(persp),
        };
        let outcome = self
            .engine
            .create_goal_from_payload_write(&self.owners[idx].authz, &req)
            .await?;
        let handle: Uuid = sqlx::query_scalar("SELECT handle FROM proxiima_core.goal WHERE t = $1")
            .bind(outcome.goal_id.into_inner())
            .fetch_one(&self.admin_pool)
            .await?;
        self.owners[idx].goals.push(TrackedGoal {
            handle,
            head_t: outcome.goal_id,
            state: SnapshotGoalState::Active,
            parent_handle: None,
            motivated_by: vec![persp],
            evidence: evidence_id.into_iter().collect(),
        });
        self.coverage.goals_created += 1;
        Ok(Some(outcome.goal_id))
    }

    async fn op_decompose_goal(&mut self, idx: usize, rng: &mut Rng) -> TestResult {
        let seq = self.next_seq();
        let active: Vec<TrackedGoal> = self.owners[idx]
            .goals
            .iter()
            .filter(|g| g.state == SnapshotGoalState::Active)
            .cloned()
            .collect();
        let Some(parent) = rng.pick(&active).cloned() else {
            return Ok(());
        };
        let live_persp = self.owners[idx].live_all_perspectives();
        let Some(&persp) = rng.pick(&live_persp) else {
            return Ok(());
        };
        let live_facts = self.owners[idx].live_facts();
        let evidence_id = rng.pick(&live_facts).copied();
        let evidence = evidence_id
            .map(|f| vec![GoalEvidenceRef::new(f)])
            .unwrap_or_default();
        let req = GoalDecomposeRequest {
            owner: self.owners[idx].owner,
            parent_goal_id: parent.head_t,
            authorship: GoalAuthorship::User,
            topology: GoalTopologyWrite::new(
                vec![GoalMotivationRef::perspective(persp)],
                Vec::new(),
                Vec::new(),
            )?,
            children: vec![ChildGoalDraft {
                payload: GoalPayloadWrite {
                    schema_id: SchemaId::new("core/simple-text-v1".into()),
                    schema_version: SchemaVersion::new(1),
                    title: format!("child-{seq}"),
                    text: format!("child goal {seq}"),
                    payload: Vec::new(),
                    sidecar_payload: None,
                },
                evidence,
                request_id: IdempotencyKey::new(format!("goal-child-{seq}")).expect("key"),
            }],
            author_self_perspective_id: None,
        };
        let outcome = self
            .engine
            .decompose_goal(&self.owners[idx].authz, &req)
            .await?;
        for child in outcome.children {
            let child_handle: Uuid =
                sqlx::query_scalar("SELECT handle FROM proxiima_core.goal WHERE t = $1")
                    .bind(child.outcome.goal_id.into_inner())
                    .fetch_one(&self.admin_pool)
                    .await?;
            self.owners[idx].goals.push(TrackedGoal {
                handle: child_handle,
                head_t: child.outcome.goal_id,
                state: SnapshotGoalState::Active,
                parent_handle: Some(parent.handle),
                motivated_by: vec![persp],
                evidence: evidence_id.into_iter().collect(),
            });
        }
        self.coverage.goals_decomposed += 1;
        Ok(())
    }

    async fn op_modify_goal(&mut self, idx: usize, goal: &TrackedGoal, seq: usize) -> TestResult {
        let replacement_persp = self.owners[idx].live_all_perspectives().first().copied();
        let replacement_motivation =
            replacement_persp.map(|p| vec![GoalMotivationRef::perspective(p)]);
        let has_stale_evidence = self.owners[idx].has_nonlive(&goal.evidence);
        let has_stale_motivation =
            replacement_persp.is_none() && self.owners[idx].has_nonlive(&goal.motivated_by);
        let before_versions = if has_stale_evidence || has_stale_motivation {
            Some(self.persisted_version_sets().await?)
        } else {
            None
        };
        let res = self
            .engine
            .modify_goal(
                &self.owners[idx].authz,
                &GoalModifyRequest {
                    owner: self.owners[idx].owner,
                    prior_goal_id: goal.head_t,
                    replacement: GoalPayloadWrite {
                        schema_id: SchemaId::new("core/simple-text-v1".into()),
                        schema_version: SchemaVersion::new(1),
                        title: format!("mod-{seq}"),
                        text: format!("modified goal {seq}"),
                        payload: Vec::new(),
                        sidecar_payload: None,
                    },
                    authorship: GoalAuthorship::User,
                    request_id: IdempotencyKey::new(format!("goal-mod-{seq}")).expect("key"),
                    evidence: None,
                    motivated_by: replacement_motivation,
                    author_self_perspective_id: None,
                },
            )
            .await;
        if let Some(before) = before_versions {
            let context = if has_stale_evidence {
                let err = res.expect_err("modifying goal with non-live carried evidence must fail");
                assert_stale_evidence_refusal(&err);
                "stale-evidence modify"
            } else {
                let err =
                    res.expect_err("modifying goal with non-live carried motivation must fail");
                assert_stale_motivation_refusal(&err);
                "stale-motivation modify"
            };
            self.assert_goal_refusal_unchanged(goal.handle, goal.head_t, &before, context)
                .await?;
            self.coverage.refused_stale_inherited_pin += 1;
            return Ok(());
        }
        let outcome = res?;
        if let Some(tracked) = self.owners[idx]
            .goals
            .iter_mut()
            .find(|g| g.handle == goal.handle)
        {
            tracked.head_t = outcome.goal_id;
            if let Some(p) = replacement_persp {
                tracked.motivated_by = vec![p];
            }
        }
        self.coverage.goals_modified += 1;
        Ok(())
    }

    async fn op_transition_or_modify_goal(&mut self, idx: usize, rng: &mut Rng) -> TestResult {
        let seq = self.next_seq();
        let open = self.owners[idx].open_goals();
        let Some(goal) = rng.pick(&open).cloned() else {
            return Ok(());
        };
        if rng.range(3) == 0 && goal.state == SnapshotGoalState::Active {
            return self.op_modify_goal(idx, &goal, seq).await;
        }
        let has_stale_motivation = self.owners[idx].has_nonlive(&goal.motivated_by);

        let (transition, next_state) = match goal.state {
            SnapshotGoalState::Active => (
                GoalTransitionKind::Pause {
                    pending: PendingWork::Keep,
                },
                SnapshotGoalState::Paused,
            ),
            SnapshotGoalState::Paused => (GoalTransitionKind::Resume, SnapshotGoalState::Active),
            SnapshotGoalState::Achieved | SnapshotGoalState::Abandoned => return Ok(()),
        };
        let before_versions = if has_stale_motivation {
            Some(self.persisted_version_sets().await?)
        } else {
            None
        };
        let res = self
            .engine
            .transition_goal(
                &self.owners[idx].authz,
                &GoalTransitionRequest {
                    owner: self.owners[idx].owner,
                    prior_goal_id: goal.head_t,
                    transition,
                    authorship: GoalAuthorship::User,
                    request_id: IdempotencyKey::new(format!("goal-tr-{seq}")).expect("key"),
                    author_self_perspective_id: None,
                },
            )
            .await;
        if let Some(before) = before_versions {
            let err = res.expect_err("transitioning goal with non-live motivation must fail");
            assert_stale_motivation_refusal(&err);
            self.assert_goal_refusal_unchanged(
                goal.handle,
                goal.head_t,
                &before,
                "stale-motivation transition",
            )
            .await?;
            self.coverage.refused_stale_inherited_pin += 1;
            return Ok(());
        }
        let outcome = res?;
        if let Some(tracked) = self.owners[idx]
            .goals
            .iter_mut()
            .find(|g| g.handle == goal.handle)
        {
            tracked.head_t = outcome.goal_id;
            tracked.state = next_state;
            // `Proxiima.Goals.mayDeclareEvidence`: transitions record empty evidence.
            tracked.evidence.clear();
        }
        match next_state {
            SnapshotGoalState::Paused => self.coverage.goals_paused += 1,
            SnapshotGoalState::Active => self.coverage.goals_resumed += 1,
            SnapshotGoalState::Achieved | SnapshotGoalState::Abandoned => {}
        }
        Ok(())
    }

    async fn op_close_goal(&mut self, idx: usize, rng: &mut Rng) -> TestResult {
        let seq = self.next_seq();
        let open = self.owners[idx].open_goals();
        let Some(goal) = rng.pick(&open).cloned() else {
            return Ok(());
        };
        let has_open_child = self.owners[idx].has_open_child(goal.handle);
        let has_stale_motivation = self.owners[idx].has_nonlive(&goal.motivated_by);
        let before_versions = if has_open_child || has_stale_motivation {
            Some(self.persisted_version_sets().await?)
        } else {
            None
        };
        let abandon = goal.state == SnapshotGoalState::Paused || rng.range(2) == 0;
        let res = if abandon {
            self.engine
                .transition_goal(
                    &self.owners[idx].authz,
                    &GoalTransitionRequest {
                        owner: self.owners[idx].owner,
                        prior_goal_id: goal.head_t,
                        transition: GoalTransitionKind::Abandon,
                        authorship: GoalAuthorship::User,
                        request_id: IdempotencyKey::new(format!("goal-ab-{seq}")).expect("key"),
                        author_self_perspective_id: None,
                    },
                )
                .await
        } else {
            let live_facts = self.owners[idx].live_facts();
            let evidence = rng
                .pick(&live_facts)
                .map(|&f| vec![GoalEvidenceRef::new(f)])
                .unwrap_or_default();
            self.engine
                .mark_goal_achieved(
                    &self.owners[idx].authz,
                    &GoalMarkAchievedRequest {
                        owner: self.owners[idx].owner,
                        prior_goal_id: goal.head_t,
                        authorship: GoalAuthorship::User,
                        request_id: IdempotencyKey::new(format!("goal-ach-{seq}")).expect("key"),
                        evidence,
                        author_self_perspective_id: None,
                    },
                )
                .await
        };

        if let Some(before) = before_versions {
            let context = if has_open_child {
                let err = res.expect_err("closing parent goal with open child must be refused");
                assert!(
                    err.code == ErrorCode::InvalidArgument && err.message.contains("open children"),
                    "expected open-children InvalidArgument, got {err:?}"
                );
                self.coverage.refused_open_child_close += 1;
                "open-child close"
            } else {
                let err = res.expect_err("closing goal with non-live motivation must be refused");
                assert_stale_motivation_refusal(&err);
                self.coverage.refused_stale_inherited_pin += 1;
                "stale-motivation close"
            };
            self.assert_goal_refusal_unchanged(goal.handle, goal.head_t, &before, context)
                .await?;
            return Ok(());
        }
        let outcome = res?;
        let final_state = if abandon {
            SnapshotGoalState::Abandoned
        } else {
            SnapshotGoalState::Achieved
        };
        if let Some(tracked) = self.owners[idx]
            .goals
            .iter_mut()
            .find(|g| g.handle == goal.handle)
        {
            tracked.head_t = outcome.goal_id;
            tracked.state = final_state;
        }
        if abandon {
            self.coverage.goals_abandoned += 1;
        } else {
            self.coverage.goals_achieved += 1;
        }
        Ok(())
    }

    /// Deliberately attempts a Causa Principle 2 violation (Perspective derived
    /// directly from a Fact) and asserts the Engine returns the exact origins refusal.
    async fn op_attempt_illegal_perspective_from_fact(&mut self, idx: usize) -> TestResult {
        let seq = self.next_seq();
        let Some(&fact) = self.owners[idx].live_facts().first() else {
            return Ok(());
        };
        let payload = AgentDerivationV1 {
            title: format!("bad-persp-{seq}"),
            body: "must be refused: Perspective cannot originate from Fact".into(),
            tags: Vec::new(),
            idempotency_key: None,
            source_memory_ids: Vec::new(),
            model_id: "causa-oracle".into(),
            client_name: "causa-oracle".into(),
            client_version: "1".into(),
        };
        let derived = DerivedMemory::perspective(
            MemoryTarget::Series(SeriesHandle::new(Uuid::now_v7())),
            self.owners[idx].owner,
            format!("bad-persp-{seq}"),
            payload,
            vec![fact],
            self.derivation_identity(),
        )?;
        let before = self.persisted_version_sets().await?;
        let err = self
            .engine
            .derive_memory(&self.owners[idx].authz, derived)
            .await
            .expect_err("Perspective with a Fact origin must be refused");
        assert!(
            err.code == ErrorCode::InvalidArgument && err.message.contains("origins"),
            "expected origins InvalidArgument, got {err:?}"
        );
        self.assert_persisted_versions_unchanged(&before, "refused illegal origin write")
            .await?;
        self.coverage.refused_illegal_origin += 1;
        Ok(())
    }

    /// Deterministically seeds and verifies both the erased-pin fork refusal
    /// and the stale-inherited-pin goal refusal, asserting unchanged persisted
    /// state after each refusal.
    async fn seed_guard_refusals(&mut self, owner_idx: usize, rng: &mut Rng) -> TestResult {
        // 1. Erased-pin fork refusal: Fact -> Abstraction -> hard erase Fact -> fork Abstraction.
        let (tmp_fact, _) = self
            .op_remember_fact(owner_idx, rng, false)
            .await?
            .expect("seed disposable fact");
        self.check_invariants(0, "seed_erased_fork_fact").await?;
        let tmp_abs = self
            .op_derive_abstraction_from(
                owner_idx,
                vec![tmp_fact],
                MemoryTarget::Series(SeriesHandle::new(Uuid::now_v7())),
            )
            .await?;
        self.check_invariants(0, "seed_erased_fork_abs").await?;
        self.hard_erase_target(owner_idx, tmp_fact).await?;
        self.check_invariants(0, "seed_erased_fork_erase").await?;

        let seq = self.next_seq();
        let before = self.persisted_version_sets().await?;
        let err = self
            .engine
            .fork_memory(
                &self.owners[owner_idx].authz,
                &ForkMemoryRequest {
                    source: tmp_abs,
                    destination: MemoryDestination::Owner(self.owners[owner_idx].owner),
                    request_id: IdempotencyKey::new(format!("seed-erased-fork-{seq}"))
                        .expect("key"),
                },
            )
            .await
            .expect_err("forking abstraction with erased origin must be refused");
        assert_eq!(err.code, ErrorCode::NotFound);
        self.assert_persisted_versions_unchanged(&before, "seeded erased-pin fork")
            .await?;
        self.coverage.refused_erased_fork_source += 1;
        self.check_invariants(0, "seed_erased_fork_refusal").await?;

        // 2. Stale inherited motivation refusal: forget Perspective -> pause Goal -> restore.
        let goal = self.owners[owner_idx].goals[0].clone();
        let persp = goal.motivated_by[0];
        self.engine
            .forget_memory(
                &self.owners[owner_idx].authz,
                self.owners[owner_idx].owner,
                persp,
            )
            .await?;
        self.owners[owner_idx].forgotten.insert(persp);
        self.check_invariants(0, "seed_stale_pin_forget").await?;

        let seq = self.next_seq();
        let before = self.persisted_version_sets().await?;
        let err = self
            .engine
            .transition_goal(
                &self.owners[owner_idx].authz,
                &GoalTransitionRequest {
                    owner: self.owners[owner_idx].owner,
                    prior_goal_id: goal.head_t,
                    transition: GoalTransitionKind::Pause {
                        pending: PendingWork::Keep,
                    },
                    authorship: GoalAuthorship::User,
                    request_id: IdempotencyKey::new(format!("seed-stale-tr-{seq}")).expect("key"),
                    author_self_perspective_id: None,
                },
            )
            .await
            .expect_err("transitioning goal with forgotten motivation must be refused");
        assert_stale_motivation_refusal(&err);
        self.assert_goal_refusal_unchanged(
            goal.handle,
            goal.head_t,
            &before,
            "seeded stale-pin transition",
        )
        .await?;
        self.coverage.refused_stale_inherited_pin += 1;
        self.check_invariants(0, "seed_stale_pin_refusal").await?;

        self.engine
            .restore_memory(
                &self.owners[owner_idx].authz,
                self.owners[owner_idx].owner,
                persp,
            )
            .await?;
        self.owners[owner_idx].forgotten.remove(&persp);
        self.check_invariants(0, "seed_stale_pin_restore").await?;
        Ok(())
    }

    async fn seed_owner(&mut self, owner_idx: usize, rng: &mut Rng) -> TestResult {
        self.op_remember_fact(owner_idx, rng, false).await?;
        self.check_invariants(0, "seed_fact").await?;
        self.op_derive_abstraction(owner_idx, rng, false).await?;
        self.check_invariants(0, "seed_abstraction").await?;
        self.op_derive_perspective(owner_idx, rng, false).await?;
        self.check_invariants(0, "seed_perspective").await?;
        let goal_id = self
            .op_create_goal(owner_idx, rng)
            .await?
            .expect("seeded goal");
        self.check_invariants(0, "seed_goal").await?;

        // Explicit Principle 9 chain: Goal -> reflective Fact -> Abstraction grounded on that Fact.
        let (refl_fact_id, observed) = self
            .op_remember_fact(owner_idx, rng, true)
            .await?
            .expect("seeded reflective fact");
        assert_eq!(observed, Some(goal_id));
        self.check_invariants(0, "seed_reflective_fact").await?;

        let _refl_abs_id = self
            .op_derive_abstraction_from(
                owner_idx,
                vec![refl_fact_id],
                MemoryTarget::Series(SeriesHandle::new(Uuid::now_v7())),
            )
            .await?;
        self.check_invariants(0, "seed_reflective_derivation")
            .await?;

        self.op_decompose_goal(owner_idx, rng).await?;
        self.check_invariants(0, "seed_decompose").await?;
        self.op_attempt_illegal_perspective_from_fact(owner_idx)
            .await?;
        self.check_invariants(0, "seed_illegal_origin").await?;
        self.seed_guard_refusals(owner_idx, rng).await?;
        Ok(())
    }

    async fn run_random_step(&mut self, rng: &mut Rng) -> TestResult<&'static str> {
        let owner_idx = rng.range(2);
        let (op_name, res): (&'static str, TestResult) = match rng.range(13) {
            0 => (
                "remember_fact",
                self.op_remember_fact(owner_idx, rng, false)
                    .await
                    .map(|_| ()),
            ),
            1 => (
                "remember_reflective_fact",
                self.op_remember_fact(owner_idx, rng, true)
                    .await
                    .map(|_| ()),
            ),
            2 => (
                "derive_abstraction",
                self.op_derive_abstraction(owner_idx, rng, false).await,
            ),
            3 => (
                "revise_abstraction",
                self.op_derive_abstraction(owner_idx, rng, true).await,
            ),
            4 => (
                "derive_perspective",
                self.op_derive_perspective(owner_idx, rng, false).await,
            ),
            5 => (
                "derive_interpretation",
                self.op_derive_perspective(owner_idx, rng, true).await,
            ),
            6 => ("fork_memory", self.op_fork_memory(owner_idx, rng).await),
            7 => (
                "forget_memory",
                self.op_forget_or_restore(owner_idx, rng, false).await,
            ),
            8 => (
                "restore_memory",
                self.op_forget_or_restore(owner_idx, rng, true).await,
            ),
            9 => (
                "hard_erase_memory",
                self.op_hard_erase_memory(owner_idx, rng).await,
            ),
            10 => {
                if rng.range(2) == 0 {
                    (
                        "create_goal",
                        self.op_create_goal(owner_idx, rng).await.map(|_| ()),
                    )
                } else {
                    (
                        "decompose_goal",
                        self.op_decompose_goal(owner_idx, rng).await,
                    )
                }
            }
            11 => (
                "transition_or_modify_goal",
                self.op_transition_or_modify_goal(owner_idx, rng).await,
            ),
            _ => {
                if rng.range(4) == 0 {
                    (
                        "illegal_perspective_from_fact",
                        self.op_attempt_illegal_perspective_from_fact(owner_idx)
                            .await,
                    )
                } else {
                    ("close_goal", self.op_close_goal(owner_idx, rng).await)
                }
            }
        };
        res.map_err(|err| format!("op={op_name}: {err}"))?;
        Ok(op_name)
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn assert_full_coverage(cov: &TraceCoverage) {
    assert!(cov.facts > 0, "expected facts: {cov:?}");
    assert!(
        cov.reflective_facts > 0,
        "expected reflective facts: {cov:?}"
    );
    assert!(
        cov.reflective_derivations > 0,
        "expected reflective derivations: {cov:?}"
    );
    assert!(cov.abstractions > 0, "expected abstractions: {cov:?}");
    assert!(cov.perspectives > 0, "expected perspectives: {cov:?}");
    assert!(cov.interpretations > 0, "expected interpretations: {cov:?}");
    assert!(cov.revisions > 0, "expected revisions: {cov:?}");
    assert!(cov.forks > 0, "expected forks: {cov:?}");
    assert!(cov.forgets > 0, "expected forgets: {cov:?}");
    assert!(cov.restores > 0, "expected restores: {cov:?}");
    assert!(cov.hard_erases > 0, "expected hard erases: {cov:?}");
    assert!(cov.goals_created > 0, "expected goals created: {cov:?}");
    assert!(
        cov.goals_decomposed > 0,
        "expected goals decomposed: {cov:?}"
    );
    assert!(cov.goals_paused > 0, "expected goals paused: {cov:?}");
    assert!(cov.goals_resumed > 0, "expected goals resumed: {cov:?}");
    assert!(cov.goals_modified > 0, "expected goals modified: {cov:?}");
    assert!(cov.goals_achieved > 0, "expected goals achieved: {cov:?}");
    assert!(cov.goals_abandoned > 0, "expected goals abandoned: {cov:?}");
    assert!(
        cov.refused_open_child_close > 0,
        "expected refused open-child close: {cov:?}"
    );
    assert!(
        cov.refused_illegal_origin > 0,
        "expected refused illegal origin: {cov:?}"
    );
    assert!(
        cov.refused_stale_inherited_pin > 0,
        "expected refused stale inherited pin: {cov:?}"
    );
    assert!(
        cov.refused_erased_fork_source > 0,
        "expected refused erased fork source: {cov:?}"
    );
}

#[tokio::test]
async fn causa_stateful_trace_preserves_all_invariants() -> TestResult {
    let seed = env_u64("PROXIIMA_CAUSA_SEED", DEFAULT_SEED);
    let steps = env_usize("PROXIIMA_CAUSA_STEPS", DEFAULT_STEPS);
    let mut rng = Rng::new(seed);
    let mut world = OracleWorld::new().await?;

    for owner_idx in 0..2 {
        world.seed_owner(owner_idx, &mut rng).await?;
    }

    for step in 1..=steps {
        let op_name = world
            .run_random_step(&mut rng)
            .await
            .map_err(|err| format!("seed={seed} step={step}: {err}"))?;
        world.check_invariants(step, op_name).await?;
    }

    assert_full_coverage(&world.coverage);
    world.admin_pool.close().await;
    Ok(())
}
