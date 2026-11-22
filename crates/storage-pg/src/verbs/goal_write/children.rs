//! `Proxiima.Goals.blocksClose`

use super::{GoalId, GoalState, OwnerId, Postgres, StorageError, Transaction, map_err};

/// `Proxiima.Goals.blocksClose`. The shared lock orders child creation against this read.
pub(super) async fn ensure_goal_children_closed(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    parent_handle: uuid::Uuid,
) -> Result<(), StorageError> {
    let rows: Vec<(uuid::Uuid, GoalState)> = sqlx::query_as(
        "SELECT h.t, g.state
           FROM proxiima_core.goal g
           JOIN proxiima_core.goal_head h ON h.handle = g.handle AND h.t = g.t
          WHERE g.owner_id = $1 AND g.parent_handle = $2
          ORDER BY h.handle",
    )
    .bind(owner.into_inner())
    .bind(parent_handle)
    .fetch_all(tx.as_mut())
    .await
    .map_err(map_err)?;
    open_children(&rows)
}

/// No child head is Active or Paused; the open ones are named in child
/// handle order.
pub(super) fn open_children(rows: &[(uuid::Uuid, GoalState)]) -> Result<(), StorageError> {
    let open: Vec<GoalId> = rows
        .iter()
        .filter(|(_, state)| matches!(state, GoalState::Active | GoalState::Paused))
        .map(|(t, _)| GoalId::new(*t))
        .collect();
    if open.is_empty() {
        Ok(())
    } else {
        Err(StorageError::GoalChildrenOpen { open })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: u128) -> uuid::Uuid {
        uuid::Uuid::from_u128(n)
    }

    // `Proxiima.Goals.blocksClose`
    #[test]
    fn open_children_table() {
        use GoalState::{Abandoned, Achieved, Active, Paused};
        assert!(open_children(&[]).is_ok(), "no children");
        assert!(open_children(&[(t(1), Achieved)]).is_ok(), "one achieved");
        assert!(
            open_children(&[(t(1), Achieved), (t(2), Abandoned)]).is_ok(),
            "achieved and abandoned"
        );
        assert!(
            open_children(&[(t(1), Abandoned), (t(2), Abandoned)]).is_ok(),
            "all abandoned"
        );
        match open_children(&[(t(1), Paused), (t(2), Abandoned), (t(3), Active)]) {
            Err(StorageError::GoalChildrenOpen { open }) => {
                assert_eq!(open, vec![GoalId::new(t(1)), GoalId::new(t(3))]);
            }
            other => panic!("open children: {other:?}"),
        }
    }
}
