//! Typed Goal write requests. PostgreSQL implements their persistence.

use crate::ProtocolError;
use crate::publication::PublicationStamp;
use crate::storage_ports::other_stores::ReadOrigin;
use crate::text_bounds::{TrimmedLenViolation, check_trimmed_len};
use crate::{
    GoalChange, GoalId, GoalPayload, MemoryId, OwnerId, PendingWork, SchemaId, SchemaVersion,
    SidecarPayload,
};

/// The in-transaction refusal of goal evidence whose memory row the owner
/// transaction cannot see: absent, forgotten (owner reads hide it), or gone
/// since the entry gate. The engine answers it as `entry not found`, the
/// answer an unknown id gets at the gate.
pub const EVIDENCE_DOES_NOT_EXIST: &str = "evidence does not exist";

/// `Proxiima.Goals.GoalState`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    sqlx::Type,
)]
#[sqlx(type_name = "proxiima_core.goal_state")]
pub enum GoalState {
    Active,
    Paused,
    Achieved,
    Abandoned,
}

impl GoalState {
    /// Title Case label returned by Goal reads.
    #[must_use]
    pub const fn list_label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Paused => "Paused",
            Self::Achieved => "Achieved",
            Self::Abandoned => "Abandoned",
        }
    }

    /// Lowercase state label accepted by the filter parser.
    #[must_use]
    pub const fn filter_label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Achieved => "achieved",
            Self::Abandoned => "abandoned",
        }
    }

    /// Parse one case-insensitive filter member. Unknown labels are refused.
    #[must_use]
    pub fn from_filter_label(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "achieved" => Some(Self::Achieved),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }

    /// `Proxiima.Goals.lifecycle`. This checks only the Goal state. Storage checks child closure after locking.
    #[must_use]
    pub const fn may_achieve(self) -> bool {
        matches!(self, Self::Active)
    }
}

impl std::fmt::Display for GoalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.list_label())
    }
}

/// A filter member that is not Active, Paused, Achieved, or Abandoned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalStateParseError(String);

impl std::fmt::Display for GoalStateParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "state must be one of Active, Paused, Achieved, Abandoned; got '{}'",
            self.0
        )
    }
}

impl std::error::Error for GoalStateParseError {}

impl std::str::FromStr for GoalState {
    type Err = GoalStateParseError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::from_filter_label(raw).ok_or_else(|| GoalStateParseError(raw.to_owned()))
    }
}

/// A plain lifecycle transition. Pause alone carries a queued-work disposition.
///
/// Achievement has a dedicated write that may carry evidence.
///
/// ```compile_fail
/// use proxiima_core::PendingWork;
/// use proxiima_core::verbs::goal_write::GoalTransitionKind;
///
/// // `pending` is a field of `Pause` alone.
/// let _ = GoalTransitionKind::Resume {
///     pending: PendingWork::Keep,
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalTransitionKind {
    Pause { pending: PendingWork },
    Resume,
    Abandon,
}

impl GoalTransitionKind {
    /// The state the goal is in after this transition.
    #[must_use]
    pub const fn next_state(self) -> GoalState {
        match self {
            Self::Pause { .. } => GoalState::Paused,
            Self::Resume => GoalState::Active,
            Self::Abandon => GoalState::Abandoned,
        }
    }

    /// `Proxiima.Goals.lifecycle`: the state after this transition from `prior`, or `None` when `prior` refuses it.
    #[must_use]
    pub const fn from_state(self, prior: GoalState) -> Option<GoalState> {
        match (self, prior) {
            (Self::Pause { .. }, GoalState::Active)
            | (Self::Resume, GoalState::Paused)
            | (Self::Abandon, GoalState::Active | GoalState::Paused) => Some(self.next_state()),
            (Self::Pause { .. }, GoalState::Paused)
            | (Self::Resume, GoalState::Active)
            | (
                Self::Pause { .. } | Self::Resume | Self::Abandon,
                GoalState::Achieved | GoalState::Abandoned,
            ) => None,
        }
    }
}

impl From<GoalTransitionKind> for GoalChange {
    fn from(kind: GoalTransitionKind) -> Self {
        match kind {
            GoalTransitionKind::Pause { pending } => Self::Pause { pending },
            GoalTransitionKind::Resume => Self::Resume,
            GoalTransitionKind::Abandon => Self::Abandon,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SystemOrigin {
    Operator,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GoalAuthorship {
    User,
    System(SystemOrigin),
    External,
}

pub const MAX_GOAL_TITLE_CHARS: usize = crate::ledger!(Proxiima.Goals.titleMax);
pub const MAX_GOAL_TEXT_CHARS: usize = crate::ledger!(Proxiima.Goals.textMax);

/// Why a typed goal payload could not be built.
///
/// Each variant carries the [`TrimmedLenViolation`] that produced it, so
/// blank and over-long get different `#[error]` strings. Still [`Copy`]
/// and still matchable by variant: hosts that match `InvalidTitle` need
/// only add `(_)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GoalWriteBuildError {
    #[error("{}", .0.reason("goal title"))]
    InvalidTitle(TrimmedLenViolation),
    #[error("{}", .0.reason("goal text"))]
    InvalidText(TrimmedLenViolation),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GoalDraft {
    pub owner: OwnerId,
    pub schema_id: SchemaId,
    pub schema_version: SchemaVersion,
    pub title: String,
    pub text: String,
    pub payload: Vec<u8>,
    #[serde(skip)]
    pub sidecar_payload: Option<SidecarPayload>,
    pub state: GoalState,
    pub topology: GoalTopologyWrite,
    pub supersedes_goal_id: Option<GoalId>,
    pub authorship: GoalAuthorship,
    pub request_id: String,
}

impl GoalDraft {
    /// Build an Active Goal draft from an already-encoded typed payload: `Proxiima.Goals.birthState`.
    #[must_use]
    pub fn active_from_payload_write(
        owner: OwnerId,
        payload: GoalPayloadWrite,
        topology: GoalTopologyWrite,
        authorship: GoalAuthorship,
        request_id: IdempotencyKey,
    ) -> Self {
        Self {
            owner,
            schema_id: payload.schema_id,
            schema_version: payload.schema_version,
            title: payload.title,
            text: payload.text,
            payload: payload.payload,
            sidecar_payload: payload.sidecar_payload,
            state: GoalState::Active,
            topology,
            supersedes_goal_id: None,
            authorship,
            request_id: request_id.into_string(),
        }
    }

    /// The storage `Owner` for this draft.
    #[must_use]
    pub fn owner(&self) -> OwnerId {
        self.owner
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GoalWriteOutcome {
    pub goal_id: GoalId,
    pub goal_handle: crate::goal_collaboration::GoalHandle,
    /// Number of reference index rows derived from the declared topology.
    pub edge_count: usize,
    /// True when the same `(owner, request_id)` existed and the
    /// body matched — see docs/protocol.md
    pub idempotent_replay: bool,
}

pub use crate::verbs::idempotency::IdempotencyKey;

/// A memory a Goal rests on: held by this store, or by another registered
/// store the caller was shown (`remote`).
///
/// A remote reference exists only with the [`ReadOrigin`] the other-stores
/// service issued for it, so a Goal cannot name a foreign memory its writer
/// could not read. The witness is not serialized: a deserialized reference is
/// local, and a foreign id on it is refused where the Goal is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GoalEvidenceRef {
    memory_id: MemoryId,
    #[serde(skip)]
    remote: Option<ReadOrigin>,
}

impl GoalEvidenceRef {
    #[must_use]
    pub const fn new(memory_id: MemoryId) -> Self {
        Self {
            memory_id,
            remote: None,
        }
    }

    /// Evidence held by another store, read as the caller.
    #[must_use]
    pub const fn remote(origin: ReadOrigin) -> Self {
        Self {
            memory_id: MemoryId::new(origin.handle()),
            remote: Some(origin),
        }
    }

    #[must_use]
    pub const fn memory_id(self) -> MemoryId {
        self.memory_id
    }

    /// The proof the caller read this memory in its own store, when it is
    /// held by another store.
    #[must_use]
    pub const fn remote_origin(self) -> Option<ReadOrigin> {
        self.remote
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GoalMotivationRef {
    perspective_id: MemoryId,
}

impl GoalMotivationRef {
    #[must_use]
    pub const fn perspective(perspective_id: MemoryId) -> Self {
        Self { perspective_id }
    }

    #[must_use]
    pub const fn perspective_id(self) -> MemoryId {
        self.perspective_id
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct GoalDependencyRef {
    goal_id: GoalId,
}

impl GoalDependencyRef {
    #[must_use]
    pub const fn new(goal_id: GoalId) -> Self {
        Self { goal_id }
    }

    #[must_use]
    pub const fn goal_id(self) -> GoalId {
        self.goal_id
    }
}

/// The exact motivating Perspectives, dependency Goals and evidence a Goal declares.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GoalTopologyWrite {
    motivated_by: Vec<GoalMotivationRef>,
    dependencies: Vec<GoalDependencyRef>,
    evidence: Vec<GoalEvidenceRef>,
}

#[derive(serde::Deserialize)]
struct GoalTopologyWire {
    motivated_by: Vec<GoalMotivationRef>,
    dependencies: Vec<GoalDependencyRef>,
    evidence: Vec<GoalEvidenceRef>,
}

impl<'de> serde::Deserialize<'de> for GoalTopologyWrite {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = GoalTopologyWire::deserialize(deserializer)?;
        Self::new(wire.motivated_by, wire.dependencies, wire.evidence)
            .map_err(serde::de::Error::custom)
    }
}

impl GoalTopologyWrite {
    /// Declare optional motivation, dependencies and evidence. An unmotivated Goal belongs to its Owner alone.
    ///
    /// # Errors
    ///
    /// Returns `InvalidArgument` when dependency or evidence refs contain duplicates.
    pub fn new(
        motivated_by: Vec<GoalMotivationRef>,
        dependencies: Vec<GoalDependencyRef>,
        evidence: Vec<GoalEvidenceRef>,
    ) -> Result<Self, ProtocolError> {
        let mut normalized_motivated_by = Vec::with_capacity(motivated_by.len());
        for motivation in motivated_by {
            if !normalized_motivated_by.contains(&motivation) {
                normalized_motivated_by.push(motivation);
            }
        }
        let mut dependency_set = std::collections::BTreeSet::new();
        for dependency in &dependencies {
            if !dependency_set.insert(dependency.goal_id()) {
                return Err(ProtocolError::invalid_argument(
                    "dependencies",
                    "duplicate goal dependency",
                ));
            }
        }
        let mut evidence_set = std::collections::BTreeSet::new();
        for item in &evidence {
            if !evidence_set.insert(item.memory_id().into_inner()) {
                return Err(ProtocolError::invalid_argument(
                    "evidence",
                    "duplicate goal evidence",
                ));
            }
        }
        Ok(Self {
            motivated_by: normalized_motivated_by,
            dependencies,
            evidence,
        })
    }

    #[must_use]
    pub fn motivated_by(&self) -> &[GoalMotivationRef] {
        &self.motivated_by
    }

    #[must_use]
    pub fn dependencies(&self) -> &[GoalDependencyRef] {
        &self.dependencies
    }

    #[must_use]
    pub fn evidence(&self) -> &[GoalEvidenceRef] {
        &self.evidence
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalPayloadWrite {
    pub schema_id: SchemaId,
    pub schema_version: SchemaVersion,
    pub title: String,
    pub text: String,
    pub payload: Vec<u8>,
    pub sidecar_payload: Option<SidecarPayload>,
}

impl GoalPayloadWrite {
    /// Encode a typed Goal payload with admitted display text.
    ///
    /// # Errors
    ///
    /// Returns the typed title or prompt refusal when display admission fails.
    pub fn from_payload<P>(
        title: impl AsRef<str>,
        text: impl AsRef<str>,
        payload: P,
    ) -> Result<Self, GoalWriteBuildError>
    where
        P: GoalPayload,
    {
        let title = normalize_goal_display_field(
            title.as_ref(),
            MAX_GOAL_TITLE_CHARS,
            GoalWriteBuildError::InvalidTitle,
        )?;
        let text = normalize_goal_display_field(
            text.as_ref(),
            MAX_GOAL_TEXT_CHARS,
            GoalWriteBuildError::InvalidText,
        )?;
        let key = payload.goal_key();
        Ok(Self {
            schema_id: P::schema_id(),
            schema_version: SchemaVersion::new(P::SCHEMA_VERSION),
            title,
            text,
            payload: key,
            sidecar_payload: Some(SidecarPayload::goal(payload)),
        })
    }
}

/// Trim and bound one goal display field, tagging the violation with the
/// field it came from.
///
/// `wrap` is the tuple variant itself — `GoalWriteBuildError::InvalidTitle`
/// used as a function — so the caller names the field once and the rule
/// stays in [`check_trimmed_len`].
fn normalize_goal_display_field(
    value: &str,
    max_chars: usize,
    wrap: fn(TrimmedLenViolation) -> GoalWriteBuildError,
) -> Result<String, GoalWriteBuildError> {
    check_trimmed_len(value, max_chars)
        .map(str::to_string)
        .map_err(wrap)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalCreateRequest<P> {
    pub owner: OwnerId,
    pub topology: GoalTopologyWrite,
    pub title: String,
    pub text: String,
    pub payload: P,
    pub request_id: IdempotencyKey,
    pub authorship: GoalAuthorship,
    pub author_self_perspective_id: Option<MemoryId>,
}

impl<P> GoalCreateRequest<P> {
    /// Product/app Active Goal create request for an authenticated
    /// user flow.
    ///
    /// Defaults to [`GoalAuthorship::User`]: the product UX is acting
    /// for the signed-in owner. Use [`Self::with_authorship`] for
    /// system-originated goals.
    ///
    /// Optional motivations name exact Perspective versions.
    ///
    /// # Panics
    ///
    /// Panics only if an empty dependency/evidence topology is rejected,
    /// which would be a programming error in the constructor invariant.
    #[must_use]
    pub fn product(
        owner: OwnerId,
        motivated_by: Vec<GoalMotivationRef>,
        request_id: IdempotencyKey,
        title: impl Into<String>,
        text: impl Into<String>,
        payload: P,
    ) -> Self {
        Self {
            owner,
            topology: GoalTopologyWrite::new(motivated_by, Vec::new(), Vec::new())
                .expect("empty topology is valid"),
            title: title.into(),
            text: text.into(),
            payload,
            request_id,
            authorship: GoalAuthorship::User,
            author_self_perspective_id: None,
        }
    }

    #[must_use]
    pub fn with_topology(mut self, topology: GoalTopologyWrite) -> Self {
        self.topology = topology;
        self
    }

    #[must_use]
    pub fn with_authorship(mut self, authorship: GoalAuthorship) -> Self {
        self.authorship = authorship;
        self
    }

    #[must_use]
    pub fn with_author_self_perspective_id(
        mut self,
        author_self_perspective_id: Option<MemoryId>,
    ) -> Self {
        self.author_self_perspective_id = author_self_perspective_id;
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GoalAtomicContext<'a> {
    pub registry: &'a crate::SpiiceRegistryFrozen,
    pub author_self_perspective_id: Option<MemoryId>,
    /// The stamp used to capture the change Fact when its schema is listenable.
    pub publication: Option<PublicationStamp<'a>>,
}

#[derive(Debug)]
pub struct CreateGoalAtomicRequest<'a> {
    pub draft: GoalDraft,
    pub context: GoalAtomicContext<'a>,
    /// A fresh episode Goal pins this write-act Fact. A replay keeps the original
    /// pin and the episode refuses a replay that would replace it.
    pub write_act_t: Option<MemoryId>,
}

#[derive(Debug, Clone)]
pub struct TransitionGoalAtomicRequest<'a> {
    pub owner: OwnerId,
    pub prior_goal_id: GoalId,
    pub transition: GoalTransitionKind,
    pub authorship: GoalAuthorship,
    pub request_id: IdempotencyKey,
    pub context: GoalAtomicContext<'a>,
}

#[derive(Debug, Clone)]
pub struct AchieveGoalAtomicRequest<'a> {
    pub owner: OwnerId,
    pub prior_goal_id: GoalId,
    pub authorship: GoalAuthorship,
    pub request_id: IdempotencyKey,
    pub context: GoalAtomicContext<'a>,
    pub evidence: Vec<GoalEvidenceRef>,
}

#[derive(Debug, Clone)]
pub struct ModifyGoalAtomicRequest<'a> {
    pub owner: OwnerId,
    pub prior_goal_id: GoalId,
    pub replacement: GoalPayloadWrite,
    pub authorship: GoalAuthorship,
    pub request_id: IdempotencyKey,
    pub context: GoalAtomicContext<'a>,
    pub evidence: Option<Vec<GoalEvidenceRef>>,
    pub motivated_by: Option<Vec<GoalMotivationRef>>,
}

#[derive(Debug)]
pub struct ChildGoalDraft {
    pub payload: GoalPayloadWrite,
    pub evidence: Vec<GoalEvidenceRef>,
    pub request_id: IdempotencyKey,
}

#[derive(Debug)]
pub struct DecomposeGoalAtomicRequest<'a> {
    pub owner: OwnerId,
    pub parent_goal_id: GoalId,
    pub authorship: GoalAuthorship,
    pub context: GoalAtomicContext<'a>,
    pub topology: GoalTopologyWrite,
    pub children: Vec<ChildGoalDraft>,
}

/// A normalized command for probing replay before re-admitting mutable targets.
#[derive(Debug, Clone, Copy)]
pub enum GoalReplayRequest<'request, 'context> {
    Create(&'request CreateGoalAtomicRequest<'context>),
    Transition(&'request TransitionGoalAtomicRequest<'context>),
    Achieve(&'request AchieveGoalAtomicRequest<'context>),
    Modify(&'request ModifyGoalAtomicRequest<'context>),
    Decompose(&'request DecomposeGoalAtomicRequest<'context>),
}

impl GoalReplayRequest<'_, '_> {
    /// The Owner namespace in which the request id is interpreted.
    #[must_use]
    pub fn owner(self) -> OwnerId {
        match self {
            Self::Create(req) => req.draft.owner(),
            Self::Transition(req) => req.owner,
            Self::Achieve(req) => req.owner,
            Self::Modify(req) => req.owner,
            Self::Decompose(req) => req.owner,
        }
    }
}

/// Stored response recovered by an exact Goal-command replay probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalReplayOutcome {
    Goal(GoalWriteOutcome),
    Decompose(DecomposeGoalOutcome),
}

impl GoalReplayOutcome {
    /// The shape a single-Goal verb asked for. `Err` names the mismatch a
    /// port would have to have produced to get here.
    ///
    /// # Errors
    /// The probe answered a single-Goal command with a decomposition.
    pub fn into_goal(self) -> Result<GoalWriteOutcome, &'static str> {
        match self {
            Self::Goal(outcome) => Ok(outcome),
            Self::Decompose(_) => {
                Err("Goal replay port returned a decomposition for a single Goal command")
            }
        }
    }

    /// The shape a decomposition asked for.
    ///
    /// # Errors
    /// The probe answered a decomposition with one Goal.
    pub fn into_decompose(self) -> Result<DecomposeGoalOutcome, &'static str> {
        match self {
            Self::Decompose(outcome) => Ok(outcome),
            Self::Goal(_) => Err("Goal replay port returned one Goal for a decomposition"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecomposedGoalOutcome {
    pub outcome: GoalWriteOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecomposeGoalOutcome {
    pub children: Vec<DecomposedGoalOutcome>,
    pub idempotent_replay: bool,
}

#[cfg(test)]
mod goal_state_matrix_tests {
    use super::GoalState;

    const ALL: [GoalState; 4] = [
        GoalState::Active,
        GoalState::Paused,
        GoalState::Achieved,
        GoalState::Abandoned,
    ];

    // `Proxiima.Goals.lifecycle`
    #[test]
    fn plain_transitions_match_ledger_vectors() {
        use super::GoalTransitionKind;
        use crate::PendingWork;

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum Event {
            Set,
            Decompose,
            Modify,
            Pause([PendingWork; 1]),
            Resume,
            Achieve,
            Abandon,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum Outcome {
            Ok(GoalState),
            Error(Refusal),
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum Refusal {
            WrongState,
        }
        #[derive(serde::Deserialize)]
        struct Vector {
            #[serde(rename = "in")]
            input: (GoalState, Event),
            out: Outcome,
        }

        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/ledger_vectors.json"))
                .expect("ledger vectors are valid JSON");
        let rows: Vec<Vector> = serde_json::from_value(vectors["Proxiima.Goals.lifecycle"].clone())
            .expect("the ledger emits the lifecycle");
        let mut replayed = 0;
        for Vector {
            input: (prior, event),
            out,
        } in rows
        {
            let kind = match event {
                Event::Pause([pending]) => GoalTransitionKind::Pause { pending },
                Event::Resume => GoalTransitionKind::Resume,
                Event::Abandon => GoalTransitionKind::Abandon,
                Event::Set | Event::Decompose | Event::Modify | Event::Achieve => continue,
            };
            let expected = match out {
                Outcome::Ok(next) => Some(next),
                Outcome::Error(Refusal::WrongState) => None,
            };
            assert_eq!(kind.from_state(prior), expected, "{prior} {kind:?}");
            replayed += 1;
        }
        assert_eq!(
            replayed,
            ALL.len() * 4,
            "pause twice, resume and abandon per state"
        );
    }

    // `Proxiima.Goals.lifecycle`.
    #[test]
    fn only_active_may_achieve() {
        assert!(GoalState::Active.may_achieve());
        assert!(!GoalState::Paused.may_achieve());
        assert!(!GoalState::Achieved.may_achieve());
        assert!(!GoalState::Abandoned.may_achieve());
    }

    // `Proxiima.Goals.stateLabel`.
    #[test]
    fn state_labels_match_ledger_vectors() {
        #[derive(serde::Deserialize)]
        struct Vector {
            #[serde(rename = "in")]
            states: [GoalState; 1],
            out: String,
        }

        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/ledger_vectors.json"))
                .expect("ledger vectors are valid JSON");
        let rows: Vec<Vector> =
            serde_json::from_value(vectors["Proxiima.Goals.stateLabel"].clone())
                .expect("the ledger emits all state labels");
        let states: std::collections::BTreeSet<_> = rows.iter().map(|row| row.states[0]).collect();
        assert_eq!(states, ALL.into_iter().collect());
        for Vector {
            states: [state],
            out,
        } in rows
        {
            assert_eq!(state.list_label(), out);
        }
    }

    #[test]
    fn filter_and_list_labels_stay_paired() {
        for state in ALL {
            assert_eq!(
                state.filter_label(),
                state.list_label().to_ascii_lowercase()
            );
            assert_eq!(state.filter_label().parse::<GoalState>().ok(), Some(state));
            assert_eq!(state.list_label().parse::<GoalState>().ok(), Some(state));
            assert_eq!(state.to_string(), state.list_label());
        }
    }
}

#[cfg(test)]
mod goal_display_tests {
    use super::{
        GoalWriteBuildError, MAX_GOAL_TEXT_CHARS, MAX_GOAL_TITLE_CHARS,
        normalize_goal_display_field,
    };

    // `Proxiima.Goals.displayAdmitted`.
    #[test]
    fn display_admission_matches_ledger_vectors() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum Length {
            Blank,
            WithinBound,
            Oversized,
        }

        #[derive(serde::Deserialize)]
        struct Vector {
            #[serde(rename = "in")]
            lengths: [Length; 1],
            out: bool,
        }

        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/ledger_vectors.json"))
                .expect("ledger vectors are valid JSON");
        let rows: Vec<Vector> =
            serde_json::from_value(vectors["Proxiima.Goals.displayAdmitted"].clone())
                .expect("the ledger emits all trimmed length choices");
        assert_eq!(rows.len(), 3);

        let fields = [
            (
                MAX_GOAL_TITLE_CHARS,
                GoalWriteBuildError::InvalidTitle as fn(_) -> _,
            ),
            (MAX_GOAL_TEXT_CHARS, GoalWriteBuildError::InvalidText),
        ];
        for Vector {
            lengths: [length],
            out,
        } in rows
        {
            for (max, wrap) in fields {
                let count = match length {
                    Length::Blank => 0,
                    Length::WithinBound => max,
                    Length::Oversized => max + 1,
                };
                let text = "é".repeat(count);
                let input = format!("\u{2003}{text}\u{a0}");
                let result = normalize_goal_display_field(&input, max, wrap);
                assert_eq!(result.is_ok(), out, "{length:?}, bound {max}");
                if let Ok(normalized) = result {
                    assert_eq!(normalized, text);
                }
            }
        }
    }
}

#[cfg(test)]
mod evidence_ref_tests {
    use super::GoalEvidenceRef;
    use crate::EntityKind;
    use crate::storage_ports::other_stores::ReadOrigin;

    #[test]
    fn a_remote_reference_names_what_was_read_and_is_not_serialized_as_proof() {
        let handle = crate::new_memory_uuid(1);
        let remote = GoalEvidenceRef::remote(ReadOrigin::for_tests(handle, EntityKind::Fact));
        assert_eq!(remote.memory_id().into_inner(), handle);
        assert_eq!(
            remote.remote_origin().map(|origin| origin.handle()),
            Some(handle)
        );

        // Round-tripped bytes carry no proof: the id comes back local, and
        // a write refuses a foreign id that has no read behind it.
        let wire = serde_json::to_string(&remote).expect("serialize");
        let back: GoalEvidenceRef = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(back, GoalEvidenceRef::new(remote.memory_id()));
        assert!(back.remote_origin().is_none());
    }
}
