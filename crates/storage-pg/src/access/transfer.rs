//! Moving a memory series to another owner.

use super::cited_blobs::{
    BlobMountSource, CitedBlobUploads, find_destination_blob, load_cited_blob_source,
    mount_blob_for_destination, move_blob_rows_in_place,
};
use super::fences::{lock_object_keys_tx, lock_owner_fences_exclusive_tx};
use super::owner_registry::ensure_owner_row;
use super::upload_completion::ensure_series_uploads_published;
use super::upload_keys::blob_upload_object_keys;
use crate::error::{map_err, with_bounded_retry};
use crate::verbs::fact_embeddings::{
    RouteSpaces, enqueue_series_head_in_tx, purge_series_embedding_spaces_in_tx,
};
use proxiima_core::owner_inverse::OwnerSurfaces;
use proxiima_core::spiice::{TransferLeg, TransferRule};
use proxiima_core::{EntityId, EntityKind, OwnerId, StorageError};
use sqlx::{PgPool, Postgres, Transaction};

pub(super) fn entity_kind(stored: &str) -> Result<EntityKind, StorageError> {
    match stored {
        "fact" => Ok(EntityKind::Fact),
        "abstraction" => Ok(EntityKind::Abstraction),
        "perspective" => Ok(EntityKind::Perspective),
        "goal" => Ok(EntityKind::Goal),
        other => Err(StorageError::Internal(format!(
            "unknown entity kind {other}"
        ))),
    }
}

/// Transfer one memory **series** to `to_owner`.
///
/// Same `(handle, t)`: a transfer is an owner UPDATE, not a copy. Head and
/// every version on the handle move together (`MemoryHeadAligned`), including
/// forgotten versions, which keep their row and flag. A request may name a
/// forgotten `t`. The series' existing source-owned head participates in the
/// compare-and-set.
/// Returns `true` iff a row under `from_owner` matched and was updated.
///
/// Sidecars follow the memory unless their surface declares `RetainAtSource`.
/// Such rows retain their own owner and are excluded from generated transfer
/// statements and the forget/restore cycle.
///
/// Cited `blob` rows move when no other live series under a different owner
/// still cites them. Embeddings / jobs follow the transferred `t`s so ANN
/// (`emb.owner_id`) stays Tesla-valve. `ingest_keys` for those `t`s are
/// deleted so the prior owner can mint a new series.
///
/// Goals do not transfer — the engine refuses them before storage; this
/// backstop keeps a direct storage call from transferring one. The same
/// holds for a memory whose schema declares `NotTransferable`, kernel or
/// Spiice, decided from the memory row.
///
/// # Errors
///
/// `Conflict` when a cited blob is still referenced by another live series or
/// the source owner still has an upload whose publication is unresolved.
/// `ConstraintViolation` for a Goal entity, for a memory whose schema
/// declares `NotTransferable`, and for unique / check violations.
pub(crate) async fn transfer_to_owner(
    pool: &PgPool,
    platform: &crate::PgPlatformScope,
    permit: &proxiima_core::storage_ports::OwnerWritePermit,
    surfaces: &OwnerSurfaces,
    entity: EntityId,
    to_owner: OwnerId,
    embeddings: RouteSpaces<'_>,
) -> Result<bool, StorageError> {
    let from_owner = *permit.owner();
    // The backstop, DRIVEN by the declaration rather than merely agreeing
    // with it: the entity picks the surface that speaks for its series, and
    // that surface's resolved leg decides. Re-declare `proxiima_core.goal` as
    // a rule that moves rows and goals start transferring, which is what
    // "enforced by the declaration" has to mean if it is to mean anything.
    let memory_id = match entity {
        EntityId::Memory(memory_id) => memory_id,
        EntityId::Goal(_) => {
            return Err(refuse_untransferable_entity(surfaces, GOAL_SPINE));
        }
    };
    if let TransferLeg::Refused { .. } = surfaces.transfer_leg(MEMORY_SPINE) {
        return Err(refuse_untransferable_entity(surfaces, MEMORY_SPINE));
    }
    if from_owner == to_owner {
        return Err(StorageError::ConstraintViolation(
            "transfer destination is the current owner".into(),
        ));
    }
    let from_id = from_owner.into_inner();
    with_bounded_retry(move || async move {
        let mut tx =
            crate::platform_scope::begin_owner_lifecycle_transaction(pool, platform, permit)
                .await?;
        // Transfers participate in the owner fence before probing, locking,
        // or re-homing any series row.  Both endpoints are acquired in the
        // same sorted order, so an owner erase cannot pass the transfer's
        // boundary half-way through and crossed transfers cannot deadlock.
        lock_owner_fences_exclusive_tx(&mut tx, &[from_owner, to_owner]).await?;
        let transferred = transfer_memory_t(
            &mut tx,
            surfaces,
            memory_id.into_inner(),
            from_id,
            to_owner,
            embeddings,
        )
        .await?;
        if !transferred {
            // A false persist can follow real writes: when the head is gone
            // or changed owner after the series reads, blob/content
            // rows are already re-homed to the destination in this
            // transaction. Roll back so they revert to
            // the prior owner.
            tx.rollback().await.map_err(map_err)?;
            return Ok(false);
        }
        crate::write_wake::commit(tx).await?;
        Ok(transferred)
    })
    .await
}

/// The kernel spine tables the two [`EntityId`] arms name. A transfer asks
/// the contract what it may do to an entity by asking about the surface
/// that IS the entity's series.
const MEMORY_SPINE: &str = "proxiima_core.memory";
const GOAL_SPINE: &str = "proxiima_core.goal";

/// The refusal a `NotTransferable` surface earns, with the reason its own
/// declaration gave.
///
/// A surface the registry does not carry resolves to
/// [`TransferLeg::Unreachable`], which freeze refuses at boot — so reaching
/// the fallback here means the registry was assembled by hand, and the
/// honest answer is still a refusal.
fn refuse_untransferable_entity(surfaces: &OwnerSurfaces, table: &str) -> StorageError {
    let entity = match table {
        GOAL_SPINE => "goals",
        _ => "these entities",
    };
    match surfaces.transfer_leg(table) {
        // The declared `why` is written headline-first — the sentence a
        // caller needs, a colon, then the rationale an operator needs. The
        // wire refusal carries the headline, so the message is GENERATED
        // from `NotTransferable { why }` rather than restated beside it:
        // rewording the declaration moves this error, which is what the
        // transfer differential golden is for.
        TransferLeg::Refused { why } => StorageError::ConstraintViolation(format!(
            "{entity} do not transfer: {}",
            TransferRule::refusal_headline(why)
        )),
        _ => StorageError::Internal(format!(
            "{table} declares no transfer leg this substrate can perform"
        )),
    }
}

async fn transfer_memory_t(
    tx: &mut Transaction<'_, Postgres>,
    surfaces: &OwnerSurfaces,
    t: uuid::Uuid,
    from_id: uuid::Uuid,
    to_owner: OwnerId,
    embeddings: RouteSpaces<'_>,
) -> Result<bool, StorageError> {
    let row: Option<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT handle, schema_id
           FROM proxiima_core.memory /* FORGET-POLICY: includes-forgotten (transfer moves forgotten rows too) */
          WHERE t = $1 AND owner_id = $2",
    )
    .bind(t)
    .bind(from_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_err)?;
    let Some((handle, schema_id)) = row else {
        return Ok(false);
    };
    // The backstop for a schema's `NotTransferable`, kernel or Spiice, decided
    // from the row this transaction read and before the destination owner is
    // minted or any row is re-homed: a caller that bypasses the engine is
    // refused, and the refusal commits with nothing.
    if let Some(why) = surfaces.untransferable_schema(&schema_id) {
        return Err(StorageError::ConstraintViolation(format!(
            "memories of schema {schema_id} do not transfer: {}",
            TransferRule::refusal_headline(why)
        )));
    }
    transfer_memory_handle(tx, surfaces, t, handle, from_id, to_owner, embeddings).await
}

const SERIES_TS_SQL: &str = "SELECT t FROM proxiima_core.memory WHERE handle = $1 AND owner_id = $2 /* FORGET-POLICY: includes-forgotten (transfer moves forgotten rows too) */
     ORDER BY t";

/// Acquire the transfer's complete lifecycle boundary in the global order:
/// owner fences (the caller), every series handle, then every series `t`.
///
/// The handle lock is what closes the only growth edge. Append, forget,
/// restore, and erase all take it before changing membership, so once it is
/// held the sorted `t` set cannot grow behind the transfer. The membership
/// reread after the `t` locks remains mandatory: it turns an unexpected
/// writer that crossed this boundary into `Retryable` before any transfer
/// leg can mutate a row. The public transfer port retries that whole
/// transaction with [`with_bounded_retry`].
///
/// An empty result means the series left `from_id` while this transaction was
/// waiting for its handle; that is the ordinary false result, not a partial
/// transfer.
async fn lock_series_membership(
    tx: &mut Transaction<'_, Postgres>,
    handle: uuid::Uuid,
    from_id: uuid::Uuid,
) -> Result<Vec<uuid::Uuid>, StorageError> {
    // A transfer is one series and therefore one handle, but keep the
    // complete-set helper at the seam: callers that ever resolve a compound
    // transfer must still hand the lock helper a sorted set, never grow it
    // while taking locks.
    crate::verbs::forget::lock_memory_handles_tx(tx, &[handle]).await?;

    let expected: Vec<uuid::Uuid> = sqlx::query_scalar(SERIES_TS_SQL)
        .bind(handle)
        .bind(from_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(map_err)?;
    if expected.is_empty() {
        return Ok(expected);
    }

    // The handle lock prevents append/forget/restore/erase from changing the
    // series while these lifecycle locks are acquired. The helper sorts and
    // deduplicates, so crossed transfers share one order for every `t`.
    crate::verbs::forget::lock_forget_memories_tx(tx, &expected).await?;
    let actual: Vec<uuid::Uuid> = sqlx::query_scalar(SERIES_TS_SQL)
        .bind(handle)
        .bind(from_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(map_err)?;
    if actual != expected {
        return Err(StorageError::Retryable(
            "series membership changed before transfer lifecycle locks were complete".into(),
        ));
    }
    Ok(actual)
}

async fn transfer_memory_handle(
    tx: &mut Transaction<'_, Postgres>,
    surfaces: &OwnerSurfaces,
    requested_t: uuid::Uuid,
    handle: uuid::Uuid,
    from_id: uuid::Uuid,
    to_owner: OwnerId,
    embeddings: RouteSpaces<'_>,
) -> Result<bool, StorageError> {
    // The destination's `owners` row is not seeded anywhere, so mint it (or
    // confirm its kind) BEFORE any statement below binds `to_id` into
    // an `owner_id` FK — `blob`, `content`, `memory_head`,
    // `memory`, `sketch` and embeddings all reference
    // `proxiima_core.owners`.
    let to_id = ensure_owner_row(tx.as_mut(), &to_owner).await?;
    let ts = lock_series_membership(tx, handle, from_id).await?;
    if ts.is_empty() {
        return Ok(false);
    }
    // Causal Goal/Perspective links are same-owner references. A Perspective
    // series cannot move while any version points at a Goal, and a Goal cannot
    // retain a motivation pointing at a Perspective series that would move
    // away from it. Check all versions, including forgotten and superseded
    // rows, while the series handle and version locks are held.
    let has_guidance: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM proxiima_core.memory m /* FORGET-POLICY: includes-forgotten (owner transfer moves forgotten rows too) */
               JOIN proxiima_core.goal g ON g.t = ANY(m.guided_by)
              WHERE m.t = ANY($1::uuid[])
         ) OR EXISTS (
             SELECT 1
               FROM proxiima_core.goal g
               JOIN proxiima_core.memory m /* FORGET-POLICY: includes-forgotten (owner transfer moves forgotten rows too) */ ON m.t = ANY(g.motivated_by)
              WHERE m.t = ANY($1::uuid[])
         )",
    )
    .bind(&ts)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_err)?;
    if has_guidance {
        return Err(StorageError::ConstraintViolation(
            "a memory series with Goal/Perspective causal relations cannot transfer owners".into(),
        ));
    }
    if !ts.contains(&requested_t) {
        // The probe that resolved the handle may have preceded a complete
        // erase. A handle is reusable, so never transfer a replacement
        // series merely because it now occupies the captured handle.
        return Ok(false);
    }
    // A head is mandatory whenever a row exists, forgotten or not (forget
    // keeps the row and its head, #488). Accepting a missing head here would
    // turn a live-head race into a transfer that moves sidecars without
    // moving the spine.
    let head: Option<(uuid::Uuid, uuid::Uuid)> = sqlx::query_as(
        "SELECT t, owner_id FROM proxiima_core.memory_head
          WHERE handle = $1",
    )
    .bind(handle)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_err)?;
    let expected_head_t = if let Some((head_t, head_owner)) = head {
        if head_owner != from_id {
            // Preserve the existing live-head denial: a stale owner must not
            // move the series' other rows once its head has left.
            return Ok(false);
        }
        if !ts.contains(&head_t) {
            return Err(StorageError::Retryable(
                "series head advanced after transfer locked its version set".into(),
            ));
        }
        Some(head_t)
    } else {
        // `ts` is non-empty, so the series has rows and must have a head.
        return Err(StorageError::Retryable(
            "memory head disappeared while the series still had rows".into(),
        ));
    };
    ensure_series_uploads_published(tx, handle, from_id).await?;
    // Order, and all of it load-bearing.
    //
    // The two dedupe surfaces run FIRST because both read the series' rows,
    // live and forgotten, under the SOURCE owner to find what the series
    // cites, and the generated re-home is what stops those reads from
    // matching.
    //
    // The generated legs run SECOND, and BEFORE the head compare-and-set. A
    // transfer that took the head row first would hold that lock across
    // every remaining statement, and `memory_head` is the row a same-series
    // ingest has to read: the transfer would be serializing ordinary writes
    // behind its own row work. The transfer race regressions pin exactly
    // that, and pin it as a liveness property ("without adding a lock to
    // normal ingest") rather than as an outcome.
    //
    // Doing real writes before the decision is already this transaction's
    // shape: a false persist rolls the whole thing back, which is why
    // `transfer_to_owner` rolls back rather than committing a `false`.
    transfer_cited_blobs(tx, surfaces, handle, from_id, to_id).await?;
    transfer_content_for_handle(tx, surfaces, handle, from_id, to_id).await?;
    run_generated_transfer_legs(tx, surfaces, from_id, to_id, &ts).await?;
    if let Some(expected_head_t) = expected_head_t
        && !persist_series_head_transfer(tx, handle, from_id, to_id, expected_head_t).await?
    {
        return Ok(false);
    }
    // The series' publication position follows it, so the destination's
    // next capture continues above every position already handed out.
    sqlx::query(
        "UPDATE proxiima_core.publication_counter
            SET owner_id = $3
          WHERE handle = $1 AND owner_id = $2",
    )
    .bind(handle)
    .bind(from_id)
    .bind(to_id)
    .execute(&mut **tx)
    .await
    .map_err(map_err)?;
    // The vectors moved with the series, but they are the destination's
    // only in spaces its route names: the rest go, and the head is queued
    // for each route space it has no vector in.
    purge_series_embedding_spaces_in_tx(tx, to_id, embeddings.spaces, &ts).await?;
    enqueue_series_head_in_tx(tx, to_id, handle, embeddings).await?;
    Ok(true)
}

/// The head compare-and-set that DECIDES whether the transfer happened.
///
/// This is why `memory_head` is a declared bespoke transfer leg and not a
/// generated one: `rows_affected` carries the question. Zero means either
/// the head advanced under us — a retryable race — or the series left the
/// owner entirely, which is the clean `false`. No generated
/// `UPDATE ... WHERE key = ANY($1)` can carry that question, and the two
/// races that hang off the answer are the ones `owner_transfer.rs` pins.
async fn persist_series_head_transfer(
    tx: &mut Transaction<'_, Postgres>,
    handle: uuid::Uuid,
    from_id: uuid::Uuid,
    to_id: uuid::Uuid,
    expected_head_t: uuid::Uuid,
) -> Result<bool, StorageError> {
    let head = sqlx::query(
        "UPDATE proxiima_core.memory_head
            SET owner_id = $3
          WHERE handle = $1 AND owner_id = $2 AND t = $4",
    )
    .bind(handle)
    .bind(from_id)
    .bind(to_id)
    .bind(expected_head_t)
    .execute(&mut **tx)
    .await
    .map_err(map_err)?;
    if head.rows_affected() == 0 {
        let owner_still_matches: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM proxiima_core.memory_head
                  WHERE handle = $1 AND owner_id = $2
             )",
        )
        .bind(handle)
        .bind(from_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(map_err)?;
        if owner_still_matches {
            return Err(StorageError::Retryable(
                "series head advanced before transfer could persist".into(),
            ));
        }
        return Ok(false);
    }
    Ok(true)
}

/// ONE loop over the declarations, in table order.
///
/// Every statement here is a `TransferLeg` the spiice that declared the
/// surface resolved at registry time:
///
/// - [`TransferLeg::Rehomed`] sets the surface's declared owner column,
///   selecting on the column the key names against the series' `t` set and
///   the source owner. That covers `memory`, `sketch`, the three
///   embedding tables and EVERY spiice's projection with one statement shape.
/// - [`TransferLeg::Dropped`] deletes them — one member, `ingest_keys`,
///   whose receipt proves admission by the SOURCE and does not travel.
///
/// A spiice adding a `Follow` surface cannot have it silently not follow:
/// the surface is in this loop because it is in the contract, and a surface
/// this loop cannot reach is `SpiiceRegistryError::UnmovableSurface` at boot
/// rather than a row the source owner can still read after the memory became
/// someone else's.
///
/// `RetainAtSource` resolves to `TransferLeg::Retained`, which is excluded
/// from `generated_transfer_legs()`.
async fn run_generated_transfer_legs(
    tx: &mut Transaction<'_, Postgres>,
    surfaces: &OwnerSurfaces,
    from_id: uuid::Uuid,
    to_id: uuid::Uuid,
    ts: &[uuid::Uuid],
) -> Result<(), StorageError> {
    for (table, leg) in surfaces.generated_transfer_legs() {
        // SQL-POLICY: generated
        sqlx::query(sqlx::AssertSqlSafe(series_leg_sql(table, leg)?))
            .bind(ts)
            .bind(to_id)
            .bind(from_id)
            .execute(&mut **tx)
            .await
            .map_err(map_err)?;
    }
    Ok(())
}

/// The statement one generated transfer leg runs, over the series' `t` set.
///
/// Two shapes, both `$1` = the `t` array, `$2` = the destination owner, and
/// `$3` = the source owner.
/// `Dropped` binds `$2` and `$3` and reads neither as data, which costs two
/// sentinel predicates and buys one call site instead of a second loop whose
/// only difference is a verb.
///
/// Every identifier substituted here is a `&'static str` from a `const`
/// contract that `try_freeze` has already validated, and every one is
/// additionally checked against `information_schema` by
/// `spiice_contract_acceptance::every_column_a_declaration_names_is_a_column_the_catalog_has`.
/// `PgIdent` is the belt to that suspenders: it is what makes the
/// substitution `%I`-equivalent rather than merely well-intentioned.
fn series_leg_sql(table: &str, leg: TransferLeg) -> Result<String, StorageError> {
    let table = crate::pg_ident::PgIdent::table(table)?;
    match leg {
        TransferLeg::Rehomed {
            key_column,
            owner_column,
        } => {
            let owner = crate::pg_ident::PgIdent::column(owner_column)?;
            let key = crate::pg_ident::PgIdent::column(key_column)?;
            // SQL-POLICY: PgIdent
            Ok(format!(
                "UPDATE {} SET {} = $2 WHERE {} = ANY($1::uuid[]) AND {} = $3",
                table.as_str(),
                owner.as_str(),
                key.as_str(),
                owner.as_str()
            ))
        }
        TransferLeg::Dropped { key_column, .. } => {
            let key = crate::pg_ident::PgIdent::column(key_column)?;
            // SQL-POLICY: PgIdent
            Ok(format!(
                concat!(
                    "DELETE FROM {} WHERE {} = ANY($1::uuid[]) ",
                    "AND $2::uuid IS NOT NULL AND $3::uuid IS NOT NULL"
                ),
                table.as_str(),
                key.as_str()
            ))
        }
        other => Err(StorageError::Internal(format!(
            "{} resolved to {other:?}, which is not a generated transfer leg",
            table.as_str()
        ))),
    }
}

/// Move the handle's cited blobs to the destination, deduping rather than
/// refusing when the bytes are shared.
///
/// `TransferRule::FollowOrDedupe` on `proxiima_core.blob`, three cases:
///
/// 1. **The destination already holds these bytes.** Its own row wins;
///    this handle's references are repointed at it and the source keeps
///    whatever it still uses. Moving the row instead would collide with the
///    destination's UNIQUE `(owner_id, schema_id, content_hash)`.
/// 2. **Nothing else references the bytes.** Move the rows in place: same
///    `blob_id`, same `upload_id`, same object, no mount. The common case
///    does not pay for the uncommon one, and a citation id a client already
///    holds does not move under it.
/// 3. **Another owner's live series references the bytes.** The destination
///    gets a row of its own and an upload row that MOUNTS the source's
///    object — OCI's cross-repo blob mount, where a mount is an optimisation
///    over a copy and never a correctness requirement. Nothing in S3 is
///    read, written or copied; ownership is metadata over an immutable
///    store.
///
/// The source's rows are never deleted here. Deleting a blob row decides
/// the fate of an S3 object, and a transfer has no object-store handle in
/// scope; erase does, and its purge is refcounted precisely so a shared
/// object survives one owner leaving. An unreferenced source row is the
/// source owner's own row, reachable by its own erase and reported by
/// reconcile.
async fn transfer_cited_blobs(
    tx: &mut Transaction<'_, Postgres>,
    surfaces: &OwnerSurfaces,
    handle: uuid::Uuid,
    from_id: uuid::Uuid,
    to_id: uuid::Uuid,
) -> Result<(), StorageError> {
    let leg = dedupe_leg(surfaces, BLOB_SURFACE)?;
    let blob_ids: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT blob_id
           FROM proxiima_core.memory /* FORGET-POLICY: includes-forgotten (transfer moves forgotten rows too) */
          WHERE handle = $1 AND owner_id = $2 AND blob_id IS NOT NULL
          ORDER BY blob_id",
    )
    .bind(handle)
    .bind(from_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_err)?;
    // Both re-homing legs below put a `blob_uploads` row over an object key
    // that another owner's erase may be deciding about right now: the in-place
    // move hands the row to the destination, and the mount inserts a second
    // row naming the SAME object. Take the whole sorted key set once, here,
    // before any of it — a per-leg lock would grow the set item by item, which
    // is precisely the shape the lock-order rule forbids.
    let object_keys = blob_upload_object_keys(tx, &blob_ids).await?;
    lock_object_keys_tx(tx, &object_keys).await?;
    for blob_id in blob_ids {
        transfer_one_cited_blob(tx, leg, handle, from_id, to_id, blob_id).await?;
    }
    Ok(())
}

/// The tables whose transfer is `FollowOrDedupe`, named where the
/// orchestration lives rather than inside it.
pub(super) const BLOB_SURFACE: &str = "proxiima_core.blob";
const CONTENT_SURFACE: &str = "proxiima_core.content";

/// The dedupe arm's payload, carried as itself.
///
/// [`TransferLeg`] spans every arm a surface can resolve to; the statements
/// below serve exactly one of them. Narrowing once, at the resolve, is what
/// lets everything downstream take the two fields rather than a leg it would
/// have to re-test — the refusal happens where the answer is produced, not
/// at each place it is read.
#[derive(Debug, Clone, Copy)]
pub(super) struct DedupeLeg {
    pub(super) dedupe_key: &'static [&'static str],
    remaps: &'static [&'static str],
}

/// The registry-resolved dedupe leg for one surface, or a typed refusal.
///
/// `freeze` has already proved this resolves — a `FollowOrDedupe` surface
/// that is not in the spiice's bespoke list is `UnmovableSurface` at boot —
/// so the error arm is reachable only from a hand-assembled registry, and
/// refusing is the honest answer there.
pub(super) fn dedupe_leg(surfaces: &OwnerSurfaces, table: &str) -> Result<DedupeLeg, StorageError> {
    match surfaces.transfer_leg(table) {
        TransferLeg::Deduped { dedupe_key, remaps } => Ok(DedupeLeg { dedupe_key, remaps }),
        other => Err(StorageError::Internal(format!(
            "{table} resolved to {other:?}; this statement serves the dedupe arm"
        ))),
    }
}

/// The `SELECT` that answers "does the destination already hold these
/// bytes", generated from the surface's declared `dedupe_key`.
///
/// The key is not decoration: it is the UNIQUE constraint the move would
/// collide with, which is exactly why the arm exists. Declaring it and then
/// hardcoding the same three columns in SQL is how the two drift, and
/// `every_dedupe_key_is_a_uniqueness_the_schema_enforces` asks
/// `pg_constraint` whether each declared key really is one.
///
/// The bind order is the declared column order, and the caller binds
/// exactly `dedupe_key.len()` values — a key of a different length is a
/// contract change that fails here rather than silently matching the wrong
/// row.
pub(super) fn dedupe_lookup_sql(
    table: &str,
    key_column: &str,
    dedupe_key: &[&'static str],
) -> Result<String, StorageError> {
    let table = crate::pg_ident::PgIdent::table(table)?;
    let key = crate::pg_ident::PgIdent::column(key_column)?;
    let mut predicates = Vec::with_capacity(dedupe_key.len());
    for (n, column) in dedupe_key.iter().enumerate() {
        predicates.push(format!(
            "{} = ${}",
            crate::pg_ident::PgIdent::column(column)?.as_str(),
            n + 1
        ));
    }
    // SQL-POLICY: PgIdent
    Ok(format!(
        "SELECT {} FROM {} WHERE {}",
        key.as_str(),
        table.as_str(),
        predicates.join(" AND ")
    ))
}

/// One referring column repointed from the old row to the new one,
/// generated from the surface's declared `remaps`.
///
/// Scoped by `handle` and not by owner: the memory rows have not changed hands
/// at this point in the transfer; they change hands in the generated re-home
/// later in the same transaction. Scoping by owner here
/// would remap nothing or everything depending on the order.
fn remap_sql(entry: &str) -> Result<String, StorageError> {
    let (table, column) = entry
        .split_once('.')
        .ok_or_else(|| StorageError::Internal(format!("remap {entry} is not <table>.<column>")))?;
    let qualified = format!("proxiima_core.{table}");
    let table = crate::pg_ident::PgIdent::table(&qualified)?;
    let column = crate::pg_ident::PgIdent::column(column)?;
    // SQL-POLICY: PgIdent
    Ok(format!(
        "UPDATE {} SET {} = $3 WHERE handle = $1 AND {} = $2",
        table.as_str(),
        column.as_str(),
        column.as_str()
    ))
}

/// One cited blob, one of the three cases.
async fn transfer_one_cited_blob(
    tx: &mut Transaction<'_, Postgres>,
    leg: DedupeLeg,
    handle: uuid::Uuid,
    from_id: uuid::Uuid,
    to_id: uuid::Uuid,
    blob_id: uuid::Uuid,
) -> Result<(), StorageError> {
    let DedupeLeg { dedupe_key, remaps } = leg;
    let (schema_id, content_hash, _) =
        load_cited_blob_source(tx, blob_id, from_id, CitedBlobUploads::Owner).await?;

    // Case 1: the destination already owns an identical row. The three
    // columns compared are `blob`'s declared `dedupe_key`, read off the
    // contract rather than restated here.
    // SQL-POLICY: generated
    let existing = find_destination_blob(tx, dedupe_key, to_id, &schema_id, &content_hash).await?;
    if let Some(dest_blob_id) = existing {
        if dest_blob_id != blob_id {
            remap_handle_refs(tx, remaps, handle, blob_id, dest_blob_id).await?;
        }
        return Ok(());
    }

    // Case 2: nobody else's live series names these bytes, so the rows can
    // simply change hands. Read `handle <> $2 AND owner_id <> $3` carefully:
    // `$3` is the DESTINATION, so the source owner's OWN second series
    // satisfies it and does not count as sharing.
    let shared: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM proxiima_core.memory /* FORGET-POLICY: includes-forgotten (transfer moves forgotten rows too) */
              WHERE blob_id = $1
                AND handle <> $2
                AND owner_id <> $3
         )",
    )
    .bind(blob_id)
    .bind(handle)
    .bind(to_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_err)?;
    if !shared {
        return move_blob_rows_in_place(tx, blob_id, from_id, to_id, &schema_id, &content_hash)
            .await;
    }

    let dest_blob_id = mount_blob_for_destination(
        tx,
        to_id,
        BlobMountSource {
            owner_id: from_id,
            blob_id,
            schema_id: &schema_id,
            content_hash: &content_hash,
            selected_upload_id: None,
        },
    )
    .await?;
    remap_handle_refs(tx, remaps, handle, blob_id, dest_blob_id).await
}

/// Repoint this series' referring columns from one row to another.
///
/// `remaps` IS the loop. The declaration lists the referring columns
/// this crate can see; a column that points at the row by convention rather
/// than by constraint cannot be listed there, because the spiice declaring
/// it is not the spiice declaring this surface. Freeze refuses the one way
/// such a column could arise, a cited-object schema declaring a sidecar
/// table (`SpiiceRegistryError::CitationSidecarNotRemappable`).
async fn remap_handle_refs(
    tx: &mut Transaction<'_, Postgres>,
    remaps: &'static [&'static str],
    handle: uuid::Uuid,
    from_id: uuid::Uuid,
    to_id: uuid::Uuid,
) -> Result<(), StorageError> {
    for entry in remaps {
        // SQL-POLICY: generated
        sqlx::query(sqlx::AssertSqlSafe(remap_sql(entry)?))
            .bind(handle)
            .bind(from_id)
            .bind(to_id)
            .execute(&mut **tx)
            .await
            .map_err(map_err)?;
    }
    Ok(())
}

/// Re-home Content under the destination so `Memory.owner = Content.owner`
/// after the transfer. Shared payloads stay on the origin owner; only this
/// series is remapped.
async fn transfer_content_for_handle(
    tx: &mut Transaction<'_, Postgres>,
    surfaces: &OwnerSurfaces,
    handle: uuid::Uuid,
    from_id: uuid::Uuid,
    to_id: uuid::Uuid,
) -> Result<(), StorageError> {
    let DedupeLeg { remaps, .. } = dedupe_leg(surfaces, CONTENT_SURFACE)?;
    let rows: Vec<(uuid::Uuid, String, Vec<u8>)> = sqlx::query_as(
        "SELECT DISTINCT c.content_id, c.schema_id, c.content_hash
           FROM proxiima_core.content c
           JOIN (
                 SELECT DISTINCT content_id
                   FROM proxiima_core.memory /* FORGET-POLICY: includes-forgotten (transfer moves forgotten rows too) */
                  WHERE handle = $1 AND owner_id = $2 AND content_id IS NOT NULL
                ) series_content ON series_content.content_id = c.content_id
          ORDER BY c.content_id",
    )
    .bind(handle)
    .bind(from_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_err)?;
    for (old_id, schema_id, hash) in rows {
        let hash: [u8; 32] = hash
            .try_into()
            .map_err(|_| StorageError::Internal("content hash is not 32 bytes".into()))?;
        let new_id = crate::verbs::content::ensure_content(tx, to_id, &schema_id, &hash).await?;
        if new_id == old_id {
            sqlx::query("UPDATE proxiima_core.content SET owner_id = $2 WHERE content_id = $1")
                .bind(old_id)
                .bind(to_id)
                .execute(&mut **tx)
                .await
                .map_err(map_err)?;
        } else {
            // The same generated remap the blob arm runs, off `content`'s
            // own declared `remaps`. No owner filter is needed: this runs
            // BEFORE the generated re-home, so every row of the handle is
            // still the source's.
            remap_handle_refs(tx, remaps, handle, old_id, new_id).await?;
            crate::verbs::content::gc_unreferenced_content(tx, old_id).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped() -> OwnerSurfaces {
        let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
        OwnerSurfaces::for_registry(&registry)
    }

    /// The substrate half of the transfer partition's completeness, and the
    /// exact counterpart of `owner_erase`'s
    /// `every_declared_surface_has_a_leg_this_crate_can_run`.
    ///
    /// Freeze decides in core that every surface reaches SOME leg. What core
    /// cannot know is whether THIS crate can execute the answer: a `Rehomed`
    /// leg whose SQL this file declines to build would be skipped in exactly
    /// the way freeze prevents spiice-side. So every generated leg is
    /// asked for its statement here, and every non-generated one must be a
    /// table a named site in this file claims.
    #[test]
    fn every_declared_surface_has_a_transfer_leg_this_crate_can_run() {
        let surfaces = shipped();
        assert!(
            surfaces.surfaces().len() > 20,
            "the registry should carry the whole core contract, got {}",
            surfaces.surfaces().len()
        );
        for surface in surfaces.surfaces() {
            match surfaces.transfer_leg(surface.table) {
                TransferLeg::Unreachable => panic!(
                    "{} resolves to Unreachable, which freeze should have refused",
                    surface.table
                ),
                leg @ (TransferLeg::Rehomed { .. } | TransferLeg::Dropped { .. }) => {
                    series_leg_sql(surface.table, leg).unwrap_or_else(|e| {
                        panic!(
                            "{} is a generated leg this crate cannot build: {e}",
                            surface.table
                        )
                    });
                }
                TransferLeg::Deduped { .. } => assert!(
                    [BLOB_SURFACE, CONTENT_SURFACE].contains(&surface.table),
                    "{} is a dedupe leg no site in this file orchestrates",
                    surface.table
                ),
                TransferLeg::Bespoke => assert!(
                    [
                        "proxiima_core.memory_head",
                        "proxiima_core.blob_uploads",
                        "proxiima_core.publication_counter",
                    ]
                    .contains(&surface.table),
                    "{} claims a bespoke transfer leg this file does not write",
                    surface.table
                ),
                TransferLeg::StaysOnKey
                | TransferLeg::Retained { .. }
                | TransferLeg::Refused { .. } => {}
            }
        }
    }

    /// The generated set only ever contains the two legs that are statements.
    /// A `StaysOnKey` surface appearing here would run an `UPDATE` over rows
    /// whose owner is already correct by construction.
    #[test]
    fn the_generated_set_is_exactly_the_legs_that_are_statements() {
        let surfaces = shipped();
        let generated = surfaces.generated_transfer_legs();
        assert!(!generated.is_empty(), "the core spine has rows that move");
        for (table, leg) in &generated {
            assert!(
                matches!(
                    leg,
                    TransferLeg::Rehomed { .. } | TransferLeg::Dropped { .. }
                ),
                "{table} is in the generated set as {leg:?}"
            );
        }
        let mut tables: Vec<&str> = generated.iter().map(|(table, _)| *table).collect();
        let sorted = {
            let mut copy = tables.clone();
            copy.sort_unstable();
            copy
        };
        assert_eq!(tables, sorted, "the generated legs run in table order");
        tables.dedup();
        assert_eq!(tables.len(), generated.len(), "one leg per table");
    }

    /// A rehome sets the surface's ONE declared owner column, named as the
    /// declaration names it rather than as `owner_id` by convention.
    #[test]
    fn a_rehomed_leg_sets_the_owner_column_the_surface_declares() {
        let sql = series_leg_sql(
            "test_spiice.thing",
            TransferLeg::Rehomed {
                key_column: "t",
                owner_column: "custodian_id",
            },
        )
        .expect("a rehome is generated");
        assert_eq!(
            sql,
            "UPDATE test_spiice.thing SET custodian_id = $2 \
             WHERE t = ANY($1::uuid[]) AND custodian_id = $3"
        );
    }

    /// Both generated shapes bind the same three parameters in the same order,
    /// which is what lets `run_generated_transfer_legs` be one loop.
    #[test]
    fn a_dropped_leg_binds_the_same_three_parameters_as_a_rehome() {
        let sql = series_leg_sql(
            "test_spiice.thing",
            TransferLeg::Dropped {
                key_column: "t",
                why: "unused here",
            },
        )
        .expect("a drop is generated");
        assert_eq!(
            sql,
            "DELETE FROM test_spiice.thing WHERE t = ANY($1::uuid[]) \
             AND $2::uuid IS NOT NULL AND $3::uuid IS NOT NULL"
        );
    }

    /// The legs that are not statements are refused rather than approximated.
    #[test]
    fn a_leg_that_is_not_a_statement_is_refused_not_guessed() {
        for leg in [
            TransferLeg::StaysOnKey,
            TransferLeg::Bespoke,
            TransferLeg::Unreachable,
            TransferLeg::Retained { why: "x" },
            TransferLeg::Refused { why: "x" },
        ] {
            assert!(
                series_leg_sql("test_spiice.thing", leg).is_err(),
                "{leg:?} produced a statement"
            );
        }
    }

    /// The lookup's placeholders follow the DECLARED column order, because
    /// that is the order the caller binds in.
    #[test]
    fn the_dedupe_lookup_numbers_placeholders_in_declared_order() {
        let sql = dedupe_lookup_sql(
            "proxiima_core.blob",
            "blob_id",
            &["owner_id", "schema_id", "content_hash"],
        )
        .expect("the declared key generates");
        assert_eq!(
            sql,
            "SELECT blob_id FROM proxiima_core.blob WHERE owner_id = $1 \
             AND schema_id = $2 AND content_hash = $3"
        );
    }

    /// A remap names the REFERRING table, which is not the surface being
    /// deduped: `blob`'s remap is a column on `memory`.
    #[test]
    fn a_remap_repoints_the_referring_table_not_the_surface() {
        assert_eq!(
            remap_sql("memory.blob_id").expect("a remap generates"),
            "UPDATE proxiima_core.memory SET blob_id = $3 \
             WHERE handle = $1 AND blob_id = $2"
        );
        assert!(
            remap_sql("blob_id").is_err(),
            "a remap without a table half is not a remap"
        );
    }

    /// Every `remaps` entry spiice #0 declares generates, and every one names
    /// a table other than its own surface.
    #[test]
    fn every_declared_remap_generates_a_statement() {
        let mut seen = 0;
        for surface in shipped().surfaces() {
            let TransferRule::FollowOrDedupe { remaps, .. } = surface.transfer else {
                continue;
            };
            for entry in remaps {
                remap_sql(entry)
                    .unwrap_or_else(|e| panic!("{} declares remap {entry}: {e}", surface.table));
                let (table, _) = entry.split_once('.').expect("checked by remap_sql");
                assert_ne!(
                    format!("proxiima_core.{table}"),
                    surface.table,
                    "a remap repoints a REFERRING column, not the surface's own"
                );
                seen += 1;
            }
        }
        assert_eq!(
            seen, 2,
            "blob and content each declare one referring column"
        );
    }

    /// Goals refuse by DECLARATION, and the refusal quotes the declared
    /// reason rather than a message written next to the `if`.
    #[test]
    fn the_goal_refusal_carries_the_declared_reason() {
        let surfaces = shipped();
        let TransferLeg::Refused { why } = surfaces.transfer_leg(GOAL_SPINE) else {
            panic!("the goal spine declares NotTransferable");
        };
        let StorageError::ConstraintViolation(message) =
            refuse_untransferable_entity(&surfaces, GOAL_SPINE)
        else {
            panic!("an untransferable entity is a constraint the substrate holds");
        };
        let headline = TransferRule::refusal_headline(why);
        assert!(
            message.ends_with(headline),
            "the refusal ({message}) should carry the declaration's headline ({headline})"
        );
        assert_eq!(
            message, "goals do not transfer: a goal series cannot change owner",
            "and the wire text is pinned, because the differential golden reads it"
        );

        // A table nothing declares untransferable falls to Internal rather
        // than borrowing the goal wording.
        assert!(matches!(
            refuse_untransferable_entity(&surfaces, MEMORY_SPINE),
            StorageError::Internal(_)
        ));
    }
}
