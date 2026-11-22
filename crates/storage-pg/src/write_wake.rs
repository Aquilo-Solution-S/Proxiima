//! Commit a write and wake the in-process drains
//! ([`proxiima_core::write_wake`]).

use proxiima_core::StorageError;
use sqlx::{Postgres, Transaction};

/// Commit `tx`, then wake local drains.
/// Use it for transactions that write rows a background drain claims (the
/// publication outbox, embedding jobs) or may write them; a wake for a write
/// that enqueued nothing costs a drain one empty claim.
pub(crate) async fn commit(tx: Transaction<'_, Postgres>) -> Result<(), StorageError> {
    proxiima_core::write_wake::commit_and_wake(tx.commit())
        .await
        .map_err(crate::error::map_err)
}

/// Finish this transaction, waking on a successful commit that enqueued work.
pub(crate) async fn finish_enqueuing<T>(
    tx: Transaction<'static, Postgres>,
    result: Result<T, StorageError>,
    enqueued: impl FnOnce(&T) -> bool,
) -> Result<T, StorageError> {
    let wake = result.as_ref().is_ok_and(enqueued);
    let finish = crate::owner_scope::finish_transaction(tx, result);
    if wake {
        proxiima_core::write_wake::commit_and_wake(finish).await
    } else {
        finish.await
    }
}
