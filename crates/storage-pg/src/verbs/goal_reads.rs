//! Owner-scoped Goal read atoms used by Engine write preflight.

use proxiima_core::{GoalId, MemoryId, OwnerId, StorageError};
use sqlx::PgConnection;

use crate::error::map_err;

/// `Proxiima.Goals.readEvidence` and `Proxiima.Goals.evidenceDeclaration`.
/// Read the Goal row directly so target visibility cannot shorten its declaration.
pub(crate) async fn load_goal_evidence_on_connection(
    connection: &mut PgConnection,
    owner: &OwnerId,
    goal_id: GoalId,
) -> Result<Option<Vec<MemoryId>>, StorageError> {
    let evidence: Option<Vec<uuid::Uuid>> = sqlx::query_scalar(
        "SELECT evidence_t FROM proxiima_core.goal WHERE t = $1 AND owner_id = $2",
    )
    .bind(goal_id.into_inner())
    .bind(owner.into_inner())
    .fetch_optional(&mut *connection)
    .await
    .map_err(map_err)?;
    Ok(evidence.map(|ids| ids.into_iter().map(MemoryId::new).collect()))
}
