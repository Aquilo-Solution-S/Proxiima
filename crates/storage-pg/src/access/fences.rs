//! Owner, source, scope and object-key advisory-lock fences.

use crate::error::map_err;
use proxiima_core::scope::ScopeKind;
use proxiima_core::{OwnerId, StorageError};
use sqlx::{Postgres, Transaction};

/// Sorted, deduplicated owner UUIDs: the canonical order every multi-owner
/// fence acquisition takes.
fn owner_fence_ids(owners: &[OwnerId]) -> Vec<uuid::Uuid> {
    let mut ids: Vec<uuid::Uuid> = owners.iter().map(|owner| owner.into_inner()).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// The owner admission fence is a distinct advisory-lock namespace from
/// lifecycle keys and Memory series handles. Its key is the owner id alone
/// (`proxiima-owner-fence:<uuid>`): one id is one owner. Memory and Goal admission hold
/// this lock in shared mode. Source-scoped erase also holds it shared while
/// taking its source fence exclusively; whole-owner erase takes the
/// single-owner fence exclusively. Multi-owner transfers use the separate
/// canonical-order exclusive helper below.
///
/// # Errors
///
/// Returns [`StorageError`] when Postgres cannot acquire the transaction
/// advisory lock.
pub async fn lock_owner_fence_shared_tx(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
) -> Result<(), StorageError> {
    lock_owner_fences(tx, std::slice::from_ref(owner), false).await
}

/// Acquire shared owner fences in canonical owner order for a multi-owner
/// admission. Batched Goal writes use this before touching their handles and
/// lifecycle targets; sorting prevents crossed batches from forming an
/// owner-fence cycle. Transfers do not use shared fences: their two endpoints
/// need the exclusive helper below so an owner-scoped erase cannot interleave
/// with the move.
pub(crate) async fn lock_owner_fences_shared_tx(
    tx: &mut Transaction<'_, Postgres>,
    owners: &[OwnerId],
) -> Result<(), StorageError> {
    lock_owner_fences(tx, owners, false).await
}

/// Acquire an exclusive owner fence for the whole-owner erase boundary.
pub(crate) async fn lock_owner_fence_exclusive_tx(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
) -> Result<(), StorageError> {
    lock_owner_fences(tx, std::slice::from_ref(owner), true).await
}

/// Acquire exclusive owner fences in canonical order for an ownership move.
///
/// Transfer can add a sourced series to either endpoint's source scope while
/// it removes the same series from the other. Taking both owner boundaries
/// exclusively makes either source-scoped or whole-owner erase linearize
/// wholly before or after that move, without discovering a new source lock
/// after series locks. The same canonical order prevents crossed transfers
/// from forming an owner-fence cycle.
pub(crate) async fn lock_owner_fences_exclusive_tx(
    tx: &mut Transaction<'_, Postgres>,
    owners: &[OwnerId],
) -> Result<(), StorageError> {
    lock_owner_fences(tx, owners, true).await
}

const LOCK_OWNER_FENCES_SHARED_SQL: &str = "\
SELECT pg_advisory_xact_lock_shared(
           hashtextextended('proxiima-owner-fence:' || i::text, 0)
       )
  FROM unnest($1::uuid[]) AS f(i)";

const LOCK_OWNER_FENCES_EXCLUSIVE_SQL: &str = "\
SELECT pg_advisory_xact_lock(
           hashtextextended('proxiima-owner-fence:' || i::text, 0)
       )
  FROM unnest($1::uuid[]) AS f(i)";

/// The one owner-fence acquisition. The four entry points above differ only in
/// arity and lock mode; the key, the ordering rule and the bind chain are
/// shared, so they live here rather than in four copies. SQL cannot
/// parameterize a function name, so the mode picks between two constants that
/// are otherwise identical — the one differing token reads side by side above.
///
/// Ordering comes from the Rust sort plus `unnest`, not from an `ORDER BY` —
/// the same reasoning written on `forget::lock_advisory_ids_tx`: a bare
/// `Function Scan` yields elements in array order, and both lock functions are
/// `VOLATILE` + `PARALLEL RESTRICTED`, so the projection is evaluated once per
/// element, in order.
async fn lock_owner_fences(
    tx: &mut Transaction<'_, Postgres>,
    owners: &[OwnerId],
    exclusive: bool,
) -> Result<(), StorageError> {
    if owners.is_empty() {
        return Ok(());
    }
    let ids = owner_fence_ids(owners);
    let sql = if exclusive {
        LOCK_OWNER_FENCES_EXCLUSIVE_SQL
    } else {
        LOCK_OWNER_FENCES_SHARED_SQL
    };
    // SQL-POLICY: fixed-fragment
    sqlx::query(sql)
        .bind(&ids)
        .execute(&mut **tx)
        .await
        .map_err(map_err)?;
    Ok(())
}

/// Source admission fence.  The owner portion is deliberately repeated in
/// the key, so a source label shared by two owners never serializes them.
pub(crate) async fn lock_source_fence_shared_tx(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    source: &str,
) -> Result<(), StorageError> {
    lock_source_fence(tx, owner, source, false).await
}

/// Exclusive source-scope erase fence.  The caller must already hold the
/// owner's shared fence, preserving owner -> source ordering.
pub(crate) async fn lock_source_fence_exclusive_tx(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    source: &str,
) -> Result<(), StorageError> {
    lock_source_fence(tx, owner, source, true).await
}

const LOCK_SOURCE_FENCE_SHARED_SQL: &str = "\
SELECT pg_advisory_xact_lock_shared(
           hashtextextended('proxiima-source-fence:' || $1::text || ':' || $2, 0)
       )";

const LOCK_SOURCE_FENCE_EXCLUSIVE_SQL: &str = "\
SELECT pg_advisory_xact_lock(
           hashtextextended('proxiima-source-fence:' || $1::text || ':' || $2, 0)
       )";

/// One key, one bind chain; only the lock function differs by mode.
async fn lock_source_fence(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    source: &str,
    exclusive: bool,
) -> Result<(), StorageError> {
    let sql = if exclusive {
        LOCK_SOURCE_FENCE_EXCLUSIVE_SQL
    } else {
        LOCK_SOURCE_FENCE_SHARED_SQL
    };
    // SQL-POLICY: fixed-fragment
    sqlx::query(sql)
        .bind(owner.into_inner())
        .bind(source)
        .execute(&mut **tx)
        .await
        .map_err(map_err)?;
    Ok(())
}

/// The spiice-owned lifecycle-scope fence — one substrate namespace for
/// every declared scope, whatever the spiice calls it.
///
/// A scope is a lifecycle the substrate could not previously see. Its rows
/// carry a bare `<scope>_id uuid` with no foreign key into the spiice's
/// registry, so the row lock an erase takes on the registry row constrains
/// nothing else, and two writes into two different scopes of one owner share
/// the owner fence and the one source fence with each other — neither
/// separates a scope's erase from a write into that same scope. This key
/// does.
///
/// Key: `proxiima-scope-fence:<scope_kind>:<owner_id>:<scope_id>`.
/// The scope KIND leads because two spiices' scopes must not collide on a
/// shared uuid, and the owner portion is repeated for the reason the source
/// fence repeats it: a scope id that somehow appeared under two owners must
/// never serialize them. Scope ids are NEVER hashed into the Memory
/// `t`/handle namespace — a scope is not an admission, and the alias would
/// make a scope id collide with a series handle.
///
/// Position in the global order: AFTER the owner and source fences, BEFORE
/// the Memory handle and lifecycle `t` locks and every row the write
/// touches. Writers take it shared, an erase takes it exclusive: the thing
/// a scope fence excludes is the erase, not the other writers into the same
/// scope.
const LOCK_SCOPE_FENCE_SHARED_SQL: &str = "\
SELECT pg_advisory_xact_lock_shared(
           hashtextextended(
               'proxiima-scope-fence:' || $1 || ':' || $2::text || ':' || $3::text,
               0
           )
       )";

const LOCK_SCOPE_FENCE_EXCLUSIVE_SQL: &str = "\
SELECT pg_advisory_xact_lock(
           hashtextextended(
               'proxiima-scope-fence:' || $1 || ':' || $2::text || ':' || $3::text,
               0
           )
       )";

/// Take one declared scope's fence SHARED, in an already-open transaction.
///
/// The admission side. Every transaction that persists a payload declaring
/// this scope takes it, before its handle/`t` locks, and re-asks under it
/// whether the scope's registry row is still there. Several writers into one
/// scope hold it together; the scope's eraser does not join them.
///
/// # Errors
///
/// [`StorageError`] when Postgres cannot acquire the advisory lock.
pub async fn lock_scope_fence_shared_tx(
    tx: &mut Transaction<'_, Postgres>,
    kind: ScopeKind,
    owner: &OwnerId,
    scope_id: uuid::Uuid,
) -> Result<(), StorageError> {
    lock_scope_fence(tx, kind, owner, scope_id, false).await
}

/// Take one declared scope's fence EXCLUSIVELY, in an already-open
/// transaction.
///
/// The erase side, and the reason the fence exists. A spiice's scope erase
/// takes it BEFORE it reads anything it intends to delete — fence before
/// select — so the footprint it computes is exact by construction rather
/// than by re-checking afterwards: a later writer waits here, and an earlier
/// one has already committed and is in the footprint.
///
/// # Errors
///
/// [`StorageError`] when Postgres cannot acquire the advisory lock.
pub async fn lock_scope_fence_exclusive_tx(
    tx: &mut Transaction<'_, Postgres>,
    kind: ScopeKind,
    owner: &OwnerId,
    scope_id: uuid::Uuid,
) -> Result<(), StorageError> {
    lock_scope_fence(tx, kind, owner, scope_id, true).await
}

/// One key, one bind chain; only the lock function differs by mode — the
/// same shape the owner and source fences above use, and for the same
/// reason: SQL cannot parameterize a function name, so the two constants
/// read side by side and a drift between them is visible.
async fn lock_scope_fence(
    tx: &mut Transaction<'_, Postgres>,
    kind: ScopeKind,
    owner: &OwnerId,
    scope_id: uuid::Uuid,
    exclusive: bool,
) -> Result<(), StorageError> {
    let sql = if exclusive {
        LOCK_SCOPE_FENCE_EXCLUSIVE_SQL
    } else {
        LOCK_SCOPE_FENCE_SHARED_SQL
    };
    // SQL-POLICY: fixed-fragment
    sqlx::query(sql)
        .bind(kind.as_str())
        .bind(owner.into_inner())
        .bind(scope_id)
        .execute(&mut **tx)
        .await
        .map_err(map_err)?;
    Ok(())
}

/// Object-key fence: its own advisory-lock namespace, keyed by the object key
/// itself.
///
/// The invariant it buys: **every writer that creates, moves or destroys a
/// `blob_uploads` row's reference to an object key holds this lock, and every
/// reader that decides whether the object may be destroyed holds it too.**
/// That is what makes the retain anti-join ("no surviving `blob_uploads` row
/// names this key") true AT DECISION TIME rather than true of a READ
/// COMMITTED snapshot. Advisory xact locks are held to commit, so the second
/// of two concurrent erasers of a shared mounted object waits, then observes
/// the first's committed deletes — and the object is enqueued exactly once
/// instead of withheld by both.
///
/// Position in the global order: AFTER the owner and source fences, AFTER the
/// Memory handle and lifecycle `t` locks, and AFTER any row lock the caller
/// already holds — and BEFORE the retain/purge decision and the
/// `cold_purge_pending` insert that follows it. Erase is the one caller that
/// takes it before its row locks, because its rows are the ones the anti-join
/// is about; the pair can therefore still deadlock against a caller that
/// locked its row first, which is a `40P01` and exactly what the bounded
/// retry around both transactions is for.
///
/// Only `blob_uploads` keys are many-to-one, so only they are in this
/// namespace.
///
/// Takers: owner and source-scope erase (every key they might owe), upload
/// abort, the stage locator write, blob GC, and the two transfer legs that
/// re-home an upload row over an object — the in-place move and the mount.
///
/// # Errors
///
/// Returns [`StorageError`] when Postgres cannot acquire the transaction
/// advisory lock.
pub async fn lock_object_keys_tx(
    tx: &mut Transaction<'_, Postgres>,
    object_keys: &[String],
) -> Result<(), StorageError> {
    if object_keys.is_empty() {
        return Ok(());
    }
    // Sorted and deduplicated for the same reason the owner fences are:
    // crossed callers holding two keys each must agree on one order, and one
    // round trip must not take the same lock twice.
    let mut keys: Vec<&str> = object_keys.iter().map(String::as_str).collect();
    keys.sort_unstable();
    keys.dedup();
    // SQL-POLICY: fixed-fragment
    sqlx::query(LOCK_OBJECT_KEYS_SQL)
        .bind(&keys)
        .execute(&mut **tx)
        .await
        .map_err(map_err)?;
    Ok(())
}

const LOCK_OBJECT_KEYS_SQL: &str = "\
SELECT pg_advisory_xact_lock(
           hashtextextended('proxiima-object-key:' || k, 0)
       )
  FROM unnest($1::text[]) AS f(k)";
