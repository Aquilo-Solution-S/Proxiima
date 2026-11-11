//! Object keys an upload implies, and the debts a terminal upload owes.

use crate::error::map_err;
use proxiima_core::{OwnerId, StorageError};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Every object key the upload row `u` implies, as rows of
/// `k(object_key, key_kind, key_id)`; filter on `k.object_key IS NOT NULL`.
///
/// What an upload row owes is derived from its own ids, not read from the
/// key it happens to record: a row records `pending/<u>` until its stage
/// lands and `objects/<m>` after, and a crash between the canonical PUT and
/// that record leaves both objects behind one recorded key.
///
/// - `objects/<m>` with `m = coalesce(mounted_from_upload_id, upload_id)`,
///   the object the row's bytes live in (`key_kind = 'objects'`).
/// - `pending/<u>`, the transfer key, unless the row is a mount, which never
///   had one (`key_kind = 'pending'`).
/// - the recorded `object_key` when it is neither (`key_kind = 'recorded'`).
///
/// Key text matches `proxiima_core::citations::uploaded_blob_object_key` and
/// `upload_transfer_key`: `PostgreSQL`'s `uuid::text` is the same lower-case
/// hyphenated form.
macro_rules! upload_implied_keys_sql {
    () => {
        "CROSS JOIN LATERAL (VALUES
            ('objects/' || coalesce(u.mounted_from_upload_id, u.upload_id)::text,
             'objects', coalesce(u.mounted_from_upload_id, u.upload_id)),
            (CASE WHEN u.mounted_from_upload_id IS NULL
                  THEN 'pending/' || u.upload_id::text END,
             'pending', u.upload_id),
            (CASE WHEN u.object_key <> 'objects/' || coalesce(u.mounted_from_upload_id, u.upload_id)::text
                   AND u.object_key <> 'pending/' || u.upload_id::text
                  THEN u.object_key END,
             'recorded', NULL::uuid)
        ) AS k(object_key, key_kind, key_id)"
    };
}
pub(crate) use upload_implied_keys_sql;

/// True when the upload row `other` implies the key `k` (see
/// [`upload_implied_keys_sql`]) in `u`'s store: the refcount an erase asks
/// before it owes `k`. Each arm is an index condition.
macro_rules! other_upload_implies_key_sql {
    () => {
        "other.object_store = u.object_store
         AND (other.object_key = k.object_key
              OR (k.key_kind = 'objects'
                  AND (other.upload_id = k.key_id OR other.mounted_from_upload_id = k.key_id))
              OR (k.key_kind = 'pending'
                  AND other.upload_id = k.key_id AND other.mounted_from_upload_id IS NULL))"
    };
}
pub(crate) use other_upload_implies_key_sql;

/// The object keys an erase of these upload rows must fence: every
/// `objects/<minting>` they imply and every key they record. Transfer keys
/// need no fence: exactly one row ever implies one.
const UPLOAD_FENCE_KEYS_SQL: &str = "\
SELECT DISTINCT key FROM (
    SELECT object_key AS key FROM proxiima_core.blob_uploads WHERE blob_id = ANY($1::uuid[])
    UNION
    SELECT 'objects/' || coalesce(mounted_from_upload_id, upload_id)::text
      FROM proxiima_core.blob_uploads WHERE blob_id = ANY($1::uuid[])
) keys
ORDER BY key";

/// The erase fence keys statement, for the plan pin.
#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
#[must_use]
pub fn upload_fence_keys_sql_for_tests() -> &'static str {
    UPLOAD_FENCE_KEYS_SQL
}

/// Owe an upload's transfer key and its canonical object in the transaction
/// that ends the upload (abort, expiry, or a stage that lost to either).
///
/// The caller holds the owner fence, the canonical key's object fence and the
/// row lock, and has just made the row terminal: one outcome per upload means
/// no Fact can cite it afterwards, so nothing else can own `objects/<u>`. A
/// pending upload is never transferred or mounted, so no other row implies
/// either key. The debt carries the owner, so the owner-scoped transaction
/// may write it.
///
/// # Errors
///
/// Returns [`StorageError`] when the debt cannot be written.
pub async fn owe_terminal_upload_keys_tx(
    tx: &mut Transaction<'_, Postgres>,
    store: &proxiima_core::ObjectStoreId,
    owner: &OwnerId,
    upload_id: Uuid,
) -> Result<Vec<crate::verbs::forget::ColdPurgeEntry>, StorageError> {
    let keys = [
        proxiima_core::citations::upload_transfer_key(upload_id),
        proxiima_core::citations::uploaded_blob_object_key(upload_id),
    ];
    let rows: Vec<(String, String)> = sqlx::query_as(
        "INSERT INTO proxiima_core.cold_purge_pending (object_key, owner_id, object_store)
         SELECT key, $2, $3 FROM unnest($1::text[]) AS f(key)
         ON CONFLICT (object_store, object_key) DO UPDATE
            SET enqueued_at = now(), next_attempt_at = now()
         RETURNING object_key, object_store",
    )
    .bind(&keys[..])
    .bind(owner.into_inner())
    .bind(store.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(map_err)?;
    Ok(rows
        .into_iter()
        .map(
            |(object_key, object_store)| crate::verbs::forget::ColdPurgeEntry {
                object_key,
                object_store,
            },
        )
        .collect())
}

/// What [`purge_owed_keys_after_commit`] left owed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct OwedKeysPurgeOutcome {
    /// Keys whose delete failed; their debt stays for the drain.
    pub failed: u64,
    /// Some debt is still owed: a failed delete, or a debt row this attempt
    /// could not reconcile.
    pub pending: bool,
}

/// Destroy what a committed transaction owed, clearing each debt as its
/// object goes; a failure leaves the debt for the drain and backs it off.
/// For the object-store adapter's own terminal transitions.
pub async fn purge_owed_keys_after_commit(
    pool: &PgPool,
    platform: &crate::PgPlatformScope,
    cold: &dyn proxiima_core::ColdObjectStore,
    entries: Vec<crate::verbs::forget::ColdPurgeEntry>,
) -> OwedKeysPurgeOutcome {
    let plan = crate::verbs::forget::ColdPurgePlan::from_entries(entries);
    let outcome = crate::verbs::forget::purge_cold_objects_after_commit_with_platform(
        pool, platform, cold, &plan,
    )
    .await;
    OwedKeysPurgeOutcome {
        failed: outcome.failed,
        pending: outcome.pending,
    }
}

/// Every object key the upload rows of `blob_ids` name or imply as their
/// canonical object, sorted for [`lock_object_keys_tx`](super::fences::lock_object_keys_tx). Both transfer legs
/// that re-home an upload row over an object take this set before they touch
/// a row, and blob GC takes it before deciding what it owes.
pub(crate) async fn blob_upload_object_keys(
    tx: &mut Transaction<'_, Postgres>,
    blob_ids: &[uuid::Uuid],
) -> Result<Vec<String>, StorageError> {
    if blob_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar(UPLOAD_FENCE_KEYS_SQL)
        .bind(blob_ids)
        .fetch_all(&mut **tx)
        .await
        .map_err(map_err)
}
