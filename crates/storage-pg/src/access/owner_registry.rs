//! Registering owners and groups in `proxiima_core.owners`.

use super::owner_columns::lock_group_membership_tx;
use crate::error::map_err;
use proxiima_core::{GroupId, OwnerId, StorageError};
use sqlx::PgConnection;

/// Register `owner` in `proxiima_core.owners` if it is not registered yet.
///
/// The only production owners upsert on the data path. Memory / goal /
/// citation / cited-blob / retention writes all go through this. A group is
/// registered when it is created (`register_group`), so an id a write reaches unregistered
/// is a user seen for the first time and is registered as `personal`. A
/// registered id keeps the kind it was registered with: the registry is never
/// rewritten from a write.
///
/// No authorization runs here. The caller supplies its authorized owner-scoped
/// transaction (RLS limits admission to its writable owners), or a
/// platform-scoped transaction for a host operation. Acquire the shared owner
/// fence before admission, as on memory and Goal writes.
/// This function never begins, commits, rolls back, retries or sets a scope;
/// on a retryable error, the caller retries its whole transaction. All effects
/// are database rows in that transaction; no post-commit call is needed.
///
/// # Errors
///
/// Storage errors from the upsert.
pub async fn ensure_owner_row(
    conn: &mut sqlx::PgConnection,
    owner: &OwnerId,
) -> Result<uuid::Uuid, StorageError> {
    let owner_id = owner.into_inner();
    // `owners` is append-only (no UPDATE): a concurrent first insert waits
    // on whichever owner uniqueness constraint it reaches, then does nothing.
    // No conflict target is intentional: v0.0.30 also enforces
    // `(owner_id, kind)`, and either arbiter must settle the same first sighting.
    sqlx::query(
        "INSERT INTO proxiima_core.owners (owner_id, kind)
         VALUES ($1, 'personal')
         ON CONFLICT DO NOTHING",
    )
    .bind(owner_id)
    .execute(&mut *conn)
    .await
    .map_err(map_err)?;
    Ok(owner_id)
}

/// Register `group` as a group, eagerly, when it is created.
///
/// No authorization runs, the authorization hooks included: the host vouches
/// for this write as for its own rows.
/// The caller must supply a platform-scoped transaction (`&mut *tx`). This
/// function never begins, commits, rolls back, retries or sets a scope; on a
/// retryable error, the caller retries its whole transaction. All effects are
/// database rows in that transaction; no post-commit call is needed.
///
/// # Errors
///
/// [`StorageError::Conflict`] when the id is already registered as a user.
/// Other storage errors from the upsert.
pub async fn register_group(conn: &mut PgConnection, group: GroupId) -> Result<(), StorageError> {
    lock_group_membership_tx(conn, group).await?;
    insert_group_row(conn, group).await?;
    if registered_group_on_connection(conn, group).await? {
        Ok(())
    } else {
        Err(StorageError::Conflict(
            "owner id is registered as a user, not a group".into(),
        ))
    }
}

pub(super) async fn insert_group_row(
    conn: &mut PgConnection,
    group: GroupId,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO proxiima_core.owners (owner_id, kind)
         VALUES ($1, 'group')
         ON CONFLICT DO NOTHING",
    )
    .bind(group.into_inner())
    .execute(&mut *conn)
    .await
    .map_err(map_err)?;
    Ok(())
}

/// Whether `owner` is registered, regardless of its owner kind.
///
/// # Errors
///
/// Storage errors from the probe.
pub(crate) async fn registered_owner_on_connection(
    conn: &mut PgConnection,
    owner: OwnerId,
) -> Result<bool, StorageError> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM proxiima_core.owners WHERE owner_id = $1
         )",
    )
    .bind(owner.into_inner())
    .fetch_one(&mut *conn)
    .await
    .map_err(map_err)
}

/// Whether `group` is registered as a group in `proxiima_core.owners`.
///
/// # Errors
///
/// Storage errors from the probe.
pub(crate) async fn registered_group_on_connection(
    conn: &mut sqlx::PgConnection,
    group: GroupId,
) -> Result<bool, StorageError> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM proxiima_core.owners
              WHERE owner_id = $1 AND kind = 'group'
         )",
    )
    .bind(group.into_inner())
    .fetch_one(&mut *conn)
    .await
    .map_err(map_err)
}
