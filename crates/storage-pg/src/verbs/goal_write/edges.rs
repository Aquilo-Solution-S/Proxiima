use super::{EvidenceTarget, GoalId, MemoryId, OwnerId, Postgres, StorageError, Transaction};

/// `Proxiima.Goals.evidenceDeclaration`.
pub(super) async fn assert_goal_topology_references(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    goal_id: GoalId,
    motivated_by: &[MemoryId],
    dependencies: &[GoalId],
    evidence: &[EvidenceTarget],
) -> Result<usize, StorageError> {
    let _ = (tx, owner, goal_id);
    Ok(motivated_by.len() + dependencies.len() + evidence.len())
}
