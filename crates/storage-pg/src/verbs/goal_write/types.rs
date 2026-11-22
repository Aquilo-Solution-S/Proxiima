use super::{GoalId, GoalState, MemoryId, SchemaId, SchemaVersion};

#[derive(Debug)]
pub(super) struct InsertedGoal {
    pub(super) goal_id: GoalId,
    pub(super) handle: uuid::Uuid,
    pub(super) idempotent_replay: bool,
}

#[derive(Debug, Clone)]
pub(super) struct StoredGoal {
    pub(super) schema_id: SchemaId,
    pub(super) schema_version: SchemaVersion,
    pub(super) title: String,
    /// `goal.text` as stored: `None` is unknown.
    pub(super) text: Option<String>,
    pub(super) payload: Vec<u8>,
    pub(super) state: GoalState,
    pub(super) motivated_by: Vec<MemoryId>,
    pub(super) dependencies: Vec<GoalId>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct EvidenceTarget {
    pub(super) memory_id: MemoryId,
}

/// The row columns outside the draft. Lifecycle writes pass the prior
/// version’s text as stored, including an unknown prompt.
/// Historical rows can lack a stored prompt.
#[derive(Clone, Copy)]
pub(super) struct RowColumns<'a> {
    /// The configured number encoded into this row's minted identity.
    pub(super) store_number: u16,
    pub(super) writer: proxiima_core::WriterAttribution,
    pub(super) text: Option<&'a str>,
    pub(super) write_act_t: Option<uuid::Uuid>,
}
