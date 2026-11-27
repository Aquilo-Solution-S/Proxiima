//! What a store owes its host index after a head change, and the sweep that
//! compares the index with the heads (#519).
//!
//! Triggers on `embedding_heads` and `memory_head` write debt in the change's
//! own transaction. The drain pays it each pass; an erase pays its owner's
//! before it commits. The sweep repairs what the debt cannot reach: an entry
//! no head names, an entry whose owner or series-head status drifted, or a
//! head the index lost.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use proxiima_core::count_u64;
use proxiima_core::storage_ports::{
    VectorEntry, VectorIndex, VectorIndexSweepOutcome, VectorKey, VectorMetadataTarget,
};
use proxiima_core::{EmbeddingDim, EmbeddingSpace, OwnerId, StorageError};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::StoreVectors;
use crate::access::fences::try_lock_owner_fence_shared;
use crate::error::map_err;
use crate::verbs::fact_embeddings::{lock_embedding_keys_batch, try_lock_embedding_key};

/// Keys per index call, so a call's body stays small however many heads one
/// transaction changed.
const KEYS_PER_CALL: usize = proxiima_core::ledger!(Proxiima.Embeddings.hostIndexKeysPerCall);

/// One active write-ahead publish intent (`generation > 0`) observed in
/// `vector_index_debt`: `(entity_id, model_id, dim, generation)`.
pub(crate) type InFlightIntent = (Uuid, String, i16, i64);

/// Prior `vector_index_debt` state overwritten by Phase 1 (`tx1`) so a
/// confirmed-unsent batch can restore the exact pre-Phase-1 disposition
/// (including an in-flight predecessor's generation).
#[derive(Debug, Clone)]
pub(crate) struct PriorDebt {
    pub(crate) op: String,
    pub(crate) generation: i64,
    pub(crate) moved_from_generation: i64,
    pub(crate) xact: String,
}

/// Phase-1 staged intent metadata kept in memory across Phase 2 (`put_batch`)
/// so lease renewal refreshes our exact rows and a failed batch can restore
/// prior debt for keys the provider knows it never dispatched while keeping
/// fenced delete cleanup for keys with uncertain delivery.
#[derive(Debug, Clone)]
pub(crate) struct StagedPublishIntent {
    pub(crate) entity_id: Uuid,
    pub(crate) model_id: String,
    pub(crate) dim: i16,
    pub(crate) generation: i64,
    /// `pg_current_xact_id()::text` of Phase 1 (`tx1`) so failure cleanup can
    /// detect whether a concurrent `memory_head` trigger touched the row while
    /// keeping `generation` positive.
    pub(crate) tx1_xact: String,
    /// Debt row overwritten by Phase 1, if any.
    pub(crate) prior_debt: Option<PriorDebt>,
}

/// Failure from hard-erase debt settlement: either active write-ahead publish
/// intents must be durably invalidated before sending any provider delete
/// fence, or a storage fault occurred.
#[derive(Debug)]
pub(crate) enum SettleDebtError {
    InFlightIntents(Vec<InFlightIntent>),
    Storage(StorageError),
}

impl From<StorageError> for SettleDebtError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// One row of index debt, parsed where it leaves the database.
struct Debt {
    key: VectorKey,
    owner: OwnerId,
    op: DebtOp,
    /// Current `(vector_generation, is_head)` when the embedding head still
    /// exists. `None` means the embedding head was deleted before the debt was
    /// resolved.
    live_head: Option<(i64, bool)>,
}

/// What a debt row asks of the index; the column's CHECK names the same
/// three.
enum DebtOp {
    Delete,
    Owner,
    Head,
}

type DebtRow = (Uuid, String, i16, Uuid, String, Option<(i64, bool)>);

fn space_of(model_id: String, dim: i16) -> Result<EmbeddingSpace, StorageError> {
    let dim = usize::try_from(dim)
        .ok()
        .and_then(EmbeddingDim::from_width)
        .ok_or_else(|| {
            StorageError::ConstraintViolation(format!("index debt names width {dim}"))
        })?;
    Ok(EmbeddingSpace::new(model_id, dim))
}

fn parse_debt(
    (entity_id, model_id, dim, owner, op, live_head): DebtRow,
) -> Result<Debt, StorageError> {
    let op = match op.as_str() {
        "delete" => DebtOp::Delete,
        "owner" => DebtOp::Owner,
        "head" => DebtOp::Head,
        other => {
            return Err(StorageError::ConstraintViolation(format!(
                "index debt names operation {other:?}"
            )));
        }
    };
    Ok(Debt {
        key: VectorKey {
            entity_id,
            space: space_of(model_id, dim)?,
        },
        owner: OwnerId::new(owner),
        op,
        live_head,
    })
}

async fn next_vector_generation(tx: &mut PgConnection) -> Result<i64, StorageError> {
    sqlx::query_scalar("SELECT nextval('proxiima_core.vector_index_generation_seq')")
        .fetch_one(tx)
        .await
        .map_err(map_err)
}

async fn delete_keys(
    index: &dyn VectorIndex,
    keys: &[VectorKey],
    fence: i64,
) -> Result<(), StorageError> {
    for keys in keys.chunks(KEYS_PER_CALL) {
        index.delete(keys, fence).await?;
    }
    Ok(())
}

async fn move_keys(
    index: &dyn VectorIndex,
    moves: BTreeMap<OwnerId, Vec<VectorMetadataTarget>>,
    revision: i64,
) -> Result<(), StorageError> {
    for (owner, targets) in moves {
        for targets in targets.chunks(KEYS_PER_CALL) {
            index.set_owner(targets, owner, revision).await?;
        }
    }
    Ok(())
}

async fn set_head_keys(
    index: &dyn VectorIndex,
    targets: &[VectorMetadataTarget],
    is_head: bool,
    revision: i64,
) -> Result<(), StorageError> {
    for targets in targets.chunks(KEYS_PER_CALL) {
        index.set_head(targets, is_head, revision).await?;
    }
    Ok(())
}

/// Apply debt to the host index: deletes (stamped with `fence_and_revision`),
/// then conditional owner moves, then conditional series-head updates.
///
/// An `'owner'` debt also synchronizes `is_head` so a supersession folded into
/// an owner move is never lost. Because providers track owner and head
/// revisions independently, one monotonic token safely fences deletes, owner
/// moves, and head updates in the same pass.
async fn apply(
    index: &dyn VectorIndex,
    rows: Vec<DebtRow>,
    fence_and_revision: i64,
) -> Result<(), StorageError> {
    let mut deletes = Vec::new();
    let mut moves: BTreeMap<OwnerId, Vec<VectorMetadataTarget>> = BTreeMap::new();
    let mut heads_true = Vec::new();
    let mut heads_false = Vec::new();
    for row in rows {
        let debt = parse_debt(row)?;
        match debt.op {
            DebtOp::Delete => deletes.push(debt.key),
            DebtOp::Owner => match debt.live_head {
                Some((generation, is_head)) if generation > 0 => {
                    let target = VectorMetadataTarget {
                        key: debt.key,
                        generation,
                    };
                    moves.entry(debt.owner).or_default().push(target.clone());
                    if is_head {
                        heads_true.push(target);
                    } else {
                        heads_false.push(target);
                    }
                }
                Some(_) => {}
                None => deletes.push(debt.key),
            },
            DebtOp::Head => match debt.live_head {
                Some((generation, true)) if generation > 0 => {
                    heads_true.push(VectorMetadataTarget {
                        key: debt.key,
                        generation,
                    });
                }
                Some((generation, false)) if generation > 0 => {
                    heads_false.push(VectorMetadataTarget {
                        key: debt.key,
                        generation,
                    });
                }
                Some(_) => {}
                None => deletes.push(debt.key),
            },
        }
    }
    if !deletes.is_empty() {
        delete_keys(index, &deletes, fence_and_revision).await?;
    }
    move_keys(index, moves, fence_and_revision).await?;
    set_head_keys(index, &heads_true, true, fence_and_revision).await?;
    set_head_keys(index, &heads_false, false, fence_and_revision).await
}

type PoppedDebtRow = (Uuid, String, i16, Uuid, String, i64, i64);

/// Resolve live `(owner_id, vector_generation, is_head)` from `embedding_heads`
/// and `memory_head` in a fresh READ COMMITTED statement after debt rows have
/// been locked. Splitting the read from the locking statement prevents
/// PostgreSQL `EvalPlanQual` from joining a newly updated debt row against an
/// older statement snapshot.
async fn resolve_live_debt_metadata(
    tx: &mut PgConnection,
    popped: Vec<PoppedDebtRow>,
) -> Result<Vec<DebtRow>, StorageError> {
    if popped.is_empty() {
        return Ok(Vec::new());
    }
    let mut lookup_entities = Vec::new();
    let mut lookup_models = Vec::new();
    let mut lookup_dims = Vec::new();
    for (entity_id, model_id, dim, _, op, _, _) in &popped {
        if op != "delete" {
            lookup_entities.push(*entity_id);
            lookup_models.push(model_id.as_str());
            lookup_dims.push(*dim);
        }
    }
    let mut live_by_key: HashMap<Key, (Uuid, i64, bool)> = HashMap::new();
    if !lookup_entities.is_empty() {
        let live: Vec<(Uuid, String, i16, Uuid, i64, bool)> = sqlx::query_as(
            "SELECT eh.entity_id,
                    eh.model_id,
                    eh.dim,
                    eh.owner_id,
                    eh.vector_generation,
                    (EXISTS (SELECT 1 FROM proxiima_core.goal g WHERE g.t = eh.entity_id)
                     OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                                 WHERE mh.owner_id = eh.owner_id AND mh.t = eh.entity_id)) AS is_head
               FROM unnest($1::uuid[], $2::text[], $3::smallint[]) AS k(entity_id, model_id, dim)
               JOIN proxiima_core.embedding_heads eh
                 ON eh.entity_id = k.entity_id
                AND eh.model_id = k.model_id
                AND eh.dim = k.dim",
        )
        .bind(&lookup_entities)
        .bind(&lookup_models)
        .bind(&lookup_dims)
        .fetch_all(&mut *tx)
        .await
        .map_err(map_err)?;
        for (entity_id, model_id, dim, owner_id, vector_generation, is_head) in live {
            live_by_key.insert(
                (entity_id, model_id, dim),
                (owner_id, vector_generation, is_head),
            );
        }
    }
    Ok(popped
        .into_iter()
        .map(|(entity_id, model_id, dim, owner_id, op, _, _)| {
            let key = (entity_id, model_id, dim);
            let (live_owner, live_head) = if op == "delete" {
                (owner_id, None)
            } else if let Some(&(eh_owner, vector_generation, is_head)) = live_by_key.get(&key) {
                (eh_owner, Some((vector_generation, is_head)))
            } else {
                (owner_id, None)
            };
            (key.0, key.1, key.2, live_owner, op, live_head)
        })
        .collect())
}

/// Find active write-ahead publish intents (`generation > 0` or transferred
/// `moved_from_generation > 0`) in whole-owner erase's settlement scope before
/// any `DELETE` trigger can overwrite those columns to `0`.
pub(crate) async fn find_in_flight_intents_for_owner(
    tx: &mut PgConnection,
    owner: OwnerId,
    foreign_entities: &[Uuid],
) -> Result<Vec<InFlightIntent>, StorageError> {
    sqlx::query_as(
        "SELECT entity_id,
                model_id,
                dim,
                CASE WHEN generation > 0 THEN generation ELSE moved_from_generation END
           FROM proxiima_core.vector_index_debt
          WHERE (generation > 0 OR moved_from_generation > 0)
            AND (owner_id = $1 OR entity_id = ANY($2::uuid[]))
          ORDER BY entity_id, model_id, dim",
    )
    .bind(owner.into_inner())
    .bind(foreign_entities)
    .fetch_all(tx)
    .await
    .map_err(map_err)
}

/// Find active write-ahead publish intents (`generation > 0` or transferred
/// `moved_from_generation > 0`) for exact `entities` before any `DELETE`
/// trigger can overwrite those columns to `0`.
pub(crate) async fn find_in_flight_intents_for_entities(
    tx: &mut PgConnection,
    entities: &[Uuid],
) -> Result<Vec<InFlightIntent>, StorageError> {
    if entities.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as(
        "SELECT entity_id,
                model_id,
                dim,
                CASE WHEN generation > 0 THEN generation ELSE moved_from_generation END
           FROM proxiima_core.vector_index_debt
          WHERE (generation > 0 OR moved_from_generation > 0)
            AND entity_id = ANY($1::uuid[])
          ORDER BY entity_id, model_id, dim",
    )
    .bind(entities)
    .fetch_all(tx)
    .await
    .map_err(map_err)
}

/// Durably clear active write-ahead publish intents (`generation = 0`,
/// `moved_from_generation = 0`) in a committed single-connection transaction
/// before an erase retries and sends any provider delete fence. Because this
/// commits first, an in-flight publisher whose generation was invalidated can
/// never complete its job claim or commit `embedding_heads` even if the erase
/// later fails or rolls back.
pub(crate) async fn invalidate_publish_intents(
    pool: &PgPool,
    platform: &crate::PgPlatformScope,
    intents: &[InFlightIntent],
) -> Result<(), StorageError> {
    if intents.is_empty() {
        return Ok(());
    }
    let lock_keys: Vec<(Uuid, String, i16)> = intents
        .iter()
        .map(|(entity_id, model_id, dim, _)| (*entity_id, model_id.clone(), *dim))
        .collect();
    let mut entities = Vec::with_capacity(intents.len());
    let mut models = Vec::with_capacity(intents.len());
    let mut dims = Vec::with_capacity(intents.len());
    let mut generations = Vec::with_capacity(intents.len());
    for (entity_id, model_id, dim, generation) in intents {
        entities.push(*entity_id);
        models.push(model_id.as_str());
        dims.push(*dim);
        generations.push(*generation);
    }
    let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
    lock_embedding_keys_batch(tx.as_mut(), &lock_keys).await?;
    sqlx::query(
        "UPDATE proxiima_core.vector_index_debt d
            SET generation = 0,
                moved_from_generation = 0
           FROM unnest($1::uuid[], $2::text[], $3::smallint[], $4::bigint[])
                AS k(entity_id, model_id, dim, generation)
          WHERE d.entity_id = k.entity_id
            AND d.model_id = k.model_id
            AND d.dim = k.dim
            AND (d.generation = k.generation OR d.moved_from_generation = k.generation)",
    )
    .bind(&entities)
    .bind(&models)
    .bind(&dims)
    .bind(&generations)
    .execute(tx.as_mut())
    .await
    .map_err(map_err)?;
    tx.commit().await.map_err(map_err)
}

fn in_flight_intents_from_popped(popped: &[PoppedDebtRow]) -> Vec<InFlightIntent> {
    popped
        .iter()
        .filter_map(|(entity_id, model_id, dim, _, _, generation, moved_from)| {
            let active = if *generation > 0 {
                *generation
            } else if *moved_from > 0 {
                *moved_from
            } else {
                return None;
            };
            Some((*entity_id, model_id.clone(), *dim, active))
        })
        .collect()
}

/// Advisory-lock barrier between hard erase (`settle_owner_debt` /
/// `settle_entity_debt`) and non-blocking unsent cleanup
/// (`release_failed_publish_intents`): cleanup holds this lock SHARED while
/// inspecting `vector_index_debt` and recording into
/// `vector_index_unsent_intents`, and hard erase acquires it EXCLUSIVE right
/// before deleting `vector_index_unsent_intents` (after all provider RPCs have
/// succeeded, immediately before `COMMIT`). That guarantees zero erase residue
/// and zero late reinsertion without a foreign key that would make cleanup
/// wait on `FOR UPDATE` or uncommitted `DELETE` row locks on
/// `vector_index_debt`.
const UNSENT_INTENTS_ERASE_BARRIER_KEY: &str = "proxiima-unsent-intents-erase-barrier";

async fn lock_unsent_intents_erase_barrier_shared(
    tx: &mut PgConnection,
) -> Result<(), StorageError> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 0))")
        .bind(UNSENT_INTENTS_ERASE_BARRIER_KEY)
        .execute(tx)
        .await
        .map(drop)
        .map_err(map_err)
}

/// Which selected-entity scope [`settle_owner_debt`] clears from
/// `proxiima_core.vector_index_unsent_intents` under the exclusive erase
/// barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerUnsentScope {
    /// Whole-owner erase transaction where session-local `selected_memories`
    /// and `selected_goals` hold every selected Memory and Goal of the owner
    /// (covering entities that have zero `embedding_heads` and zero
    /// `vector_index_debt` rows with O(1) Rust memory).
    SelectedTempTables,
    /// Unit-test calls to [`settle_owner_debt`] outside `owner_erase` where
    /// those temporary tables do not exist.
    #[cfg(test)]
    EntitiesOnly,
}

async fn clear_unsent_intents_on_erase(
    tx: &mut PgConnection,
    entities: &[Uuid],
    include_selected_temp_tables: bool,
) -> Result<(), StorageError> {
    if !include_selected_temp_tables && entities.is_empty() {
        return Ok(());
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(UNSENT_INTENTS_ERASE_BARRIER_KEY)
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;
    if include_selected_temp_tables {
        sqlx::query(
            "DELETE FROM proxiima_core.vector_index_unsent_intents
              WHERE entity_id = ANY($1::uuid[])
                 OR entity_id IN (SELECT memory_id FROM selected_memories)
                 OR entity_id IN (SELECT goal_id FROM selected_goals)",
        )
        .bind(entities)
        .execute(&mut *tx)
        .await
        .map(drop)
        .map_err(map_err)
    } else {
        sqlx::query(
            "DELETE FROM proxiima_core.vector_index_unsent_intents
              WHERE entity_id = ANY($1::uuid[])",
        )
        .bind(entities)
        .execute(&mut *tx)
        .await
        .map(drop)
        .map_err(map_err)
    }
}

/// Pay, before a whole-owner erase of `owner` commits, the debt its transaction
/// left, every older debt naming `owner`, and any debt naming one of the erased
/// `erased_entities` regardless of its recorded `owner_id` (covering entities
/// transferred in after a failed write or earlier forget while preserving
/// `TransferRule::StaysOnKey`).
///
/// Whole-owner erase holds `owner`'s exclusive fence (excluding `drain_debt`,
/// `sweep_index::repair`, and publishers for `owner` with O(1) advisory locks)
/// and pre-locks rare foreign-owned or transferred-out debt rows, so settlement
/// takes zero additional advisory locks before provider RPCs. Refuses with
/// [`SettleDebtError::InFlightIntents`] before allocating a fence or calling
/// the provider if any popped row still holds `generation > 0` or
/// `moved_from_generation > 0`. Once provider RPCs finish, acquires the
/// short-held erase barrier to clear every matching
/// `vector_index_unsent_intents` row across `erased_entities`, popped debt
/// rows, and (under [`OwnerUnsentScope::SelectedTempTables`]) every selected
/// Memory and Goal before commit.
pub(crate) async fn settle_owner_debt(
    tx: &mut PgConnection,
    vectors: &StoreVectors,
    owner: OwnerId,
    erased_entities: &[Uuid],
    unsent_scope: OwnerUnsentScope,
) -> Result<(), SettleDebtError> {
    let owner_id = owner.into_inner();
    let popped: Vec<PoppedDebtRow> = sqlx::query_as(
        "DELETE FROM proxiima_core.vector_index_debt
          WHERE xact = pg_current_xact_id()
             OR owner_id = $1
             OR entity_id = ANY($2::uuid[])
         RETURNING entity_id, model_id, dim, owner_id, op, generation, moved_from_generation",
    )
    .bind(owner_id)
    .bind(erased_entities)
    .fetch_all(&mut *tx)
    .await
    .map_err(map_err)?;
    let StoreVectors::Remote(index) = vectors else {
        return Ok(());
    };
    let in_flight = in_flight_intents_from_popped(&popped);
    if !in_flight.is_empty() {
        return Err(SettleDebtError::InFlightIntents(in_flight));
    }
    let mut unsent_entities: Vec<Uuid> = erased_entities.to_vec();
    unsent_entities.extend(popped.iter().map(|(entity_id, ..)| *entity_id));
    unsent_entities.sort_unstable();
    unsent_entities.dedup();
    if !popped.is_empty() {
        let rows = resolve_live_debt_metadata(&mut *tx, popped).await?;
        let fence_and_rev = next_vector_generation(&mut *tx).await?;
        apply(index.as_ref(), rows, fence_and_rev).await?;
    }
    clear_unsent_intents_on_erase(
        &mut *tx,
        &unsent_entities,
        matches!(unsent_scope, OwnerUnsentScope::SelectedTempTables),
    )
    .await?;
    Ok(())
}

/// Pay, before a source-scoped or series erase commits, every debt naming one
/// of its strictly erased `entities` (regardless of recorded `owner_id`),
/// leaving any retained series predecessor's debt or in-flight overwrite intact.
/// Because the caller already holds handle/target fences and canonical
/// embedding-key locks for every affected key, settlement acquires zero
/// additional advisory locks before provider RPCs. Refuses with
/// [`SettleDebtError::InFlightIntents`] before allocating a fence or calling
/// the provider if any popped row still holds `generation > 0` or
/// `moved_from_generation > 0`. Once provider RPCs finish, acquires the
/// short-held erase barrier to clear every matching
/// `vector_index_unsent_intents` row before commit.
pub(crate) async fn settle_entity_debt(
    tx: &mut PgConnection,
    vectors: &StoreVectors,
    entities: &[Uuid],
) -> Result<(), SettleDebtError> {
    if entities.is_empty() {
        return Ok(());
    }
    let popped: Vec<PoppedDebtRow> = sqlx::query_as(
        "DELETE FROM proxiima_core.vector_index_debt
          WHERE entity_id = ANY($1::uuid[])
         RETURNING entity_id, model_id, dim, owner_id, op, generation, moved_from_generation",
    )
    .bind(entities)
    .fetch_all(&mut *tx)
    .await
    .map_err(map_err)?;
    let StoreVectors::Remote(index) = vectors else {
        return Ok(());
    };
    let in_flight = in_flight_intents_from_popped(&popped);
    if !in_flight.is_empty() {
        return Err(SettleDebtError::InFlightIntents(in_flight));
    }
    if !popped.is_empty() {
        let rows = resolve_live_debt_metadata(&mut *tx, popped).await?;
        let fence_and_rev = next_vector_generation(&mut *tx).await?;
        apply(index.as_ref(), rows, fence_and_rev).await?;
    }
    clear_unsent_intents_on_erase(&mut *tx, entities, false).await?;
    Ok(())
}

/// Acquire non-blocking shared owner fences for every distinct owner in
/// `owners` (in sorted order). Returns `false` if any owner's exclusive fence
/// is held (whole-owner erase or transfer).
async fn try_lock_owners_shared(
    tx: &mut PgConnection,
    owners: &[OwnerId],
) -> Result<bool, StorageError> {
    let mut sorted = owners.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    for owner in sorted {
        if !try_lock_owner_fence_shared(&mut *tx, owner).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

struct ClaimedDebtBatch {
    claim_token: i64,
    claimed_keys: Vec<Key>,
    rows: Vec<DebtRow>,
}

async fn stamp_claimed_debt_batch(
    tx: &mut PgConnection,
    claim_token: i64,
    rows: &[DebtRow],
) -> Result<(), StorageError> {
    let mut upd_entities = Vec::with_capacity(rows.len());
    let mut upd_models = Vec::with_capacity(rows.len());
    let mut upd_dims = Vec::with_capacity(rows.len());
    let mut upd_ops = Vec::with_capacity(rows.len());
    for (entity_id, model_id, dim, _, op, live_head) in rows {
        upd_entities.push(*entity_id);
        upd_models.push(model_id.as_str());
        upd_dims.push(*dim);
        upd_ops.push(if op == "delete" || live_head.is_none() {
            "delete"
        } else {
            op.as_str()
        });
    }
    sqlx::query(
        "UPDATE proxiima_core.vector_index_debt d
            SET op = k.op,
                generation = $5,
                moved_from_generation = 0,
                enqueued_at = clock_timestamp()
           FROM unnest($1::uuid[], $2::text[], $3::smallint[], $4::text[])
                AS k(entity_id, model_id, dim, op)
          WHERE d.entity_id = k.entity_id
            AND d.model_id = k.model_id
            AND d.dim = k.dim",
    )
    .bind(&upd_entities)
    .bind(&upd_models)
    .bind(&upd_dims)
    .bind(&upd_ops)
    .bind(-claim_token)
    .execute(tx)
    .await
    .map(drop)
    .map_err(map_err)
}

/// Seconds ordinary debt drain honors a live write-ahead publish intent
/// (`generation > 0` or transferred `moved_from_generation > 0`) or an active
/// concurrent drain claim (`generation < 0`) before treating it as
/// crashed-worker residue and reclaiming it. Hard erases never wait for this
/// lease: they explicitly invalidate publish intents and settle matching debt
/// immediately.
pub(crate) const DEBT_LEASE_SECONDS: f64 =
    Duration::from_secs(proxiima_core::ledger!(Proxiima.Embeddings.debtLeaseSeconds)).as_secs_f64();

/// Interval at which live Phase-2 publications and Phase-2 debt drains refresh
/// their own `enqueued_at = clock_timestamp()` lease, so one missed renewal
/// still lands inside [`DEBT_LEASE_SECONDS`]
/// (`Causa.debt_lease_survives_a_missed_renewal`).
pub(crate) const DEBT_LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(proxiima_core::ledger!(
    Proxiima.Embeddings.debtLeaseRenewSeconds
));

/// Minimum keyset page scanned per iteration of `claim_debt_batch`.
pub(crate) const DEBT_SCAN_MIN_PAGE: i64 =
    proxiima_core::ledger!(Proxiima.Embeddings.debtScanMinPage);

/// Refresh `enqueued_at = clock_timestamp()` on still-active write-ahead
/// publish intents (`generation > 0` or transferred `moved_from_generation > 0`)
/// during Phase 2 (`put_batch`) without changing `xact`, `op`, or generation
/// columns, using key-ordered `FOR NO KEY UPDATE SKIP LOCKED` so renewal never
/// blocks concurrent erases.
pub(crate) async fn renew_publish_intents(
    pool: &PgPool,
    platform: &crate::PgPlatformScope,
    intents: &[StagedPublishIntent],
) -> Result<(), StorageError> {
    if intents.is_empty() {
        return Ok(());
    }
    let mut entities = Vec::with_capacity(intents.len());
    let mut models = Vec::with_capacity(intents.len());
    let mut dims = Vec::with_capacity(intents.len());
    let mut generations = Vec::with_capacity(intents.len());
    for intent in intents {
        entities.push(intent.entity_id);
        models.push(intent.model_id.as_str());
        dims.push(intent.dim);
        generations.push(intent.generation);
    }
    let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
    sqlx::query(
        "UPDATE proxiima_core.vector_index_debt debt
            SET enqueued_at = clock_timestamp()
          WHERE (debt.entity_id, debt.model_id, debt.dim) IN (
                SELECT d.entity_id, d.model_id, d.dim
                  FROM proxiima_core.vector_index_debt d
                  JOIN unnest($1::uuid[], $2::text[], $3::smallint[], $4::bigint[])
                       AS k(entity_id, model_id, dim, generation)
                    ON d.entity_id = k.entity_id
                   AND d.model_id = k.model_id
                   AND d.dim = k.dim
                   AND (d.generation = k.generation OR d.moved_from_generation = k.generation)
                 ORDER BY d.entity_id, d.model_id, d.dim
                   FOR NO KEY UPDATE OF d SKIP LOCKED)",
    )
    .bind(&entities)
    .bind(&models)
    .bind(&dims)
    .bind(&generations)
    .execute(tx.as_mut())
    .await
    .map_err(map_err)?;
    tx.commit().await.map_err(map_err)
}

/// Refresh `enqueued_at = clock_timestamp()` on active negative-token drain
/// claims while Phase-2 provider RPCs are running.
async fn renew_debt_claim(
    pool: &PgPool,
    platform: &crate::PgPlatformScope,
    claim_token: i64,
    claimed_keys: &[Key],
) -> Result<(), StorageError> {
    if claimed_keys.is_empty() {
        return Ok(());
    }
    let (entities, models, dims) = split_keys(claimed_keys);
    let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
    sqlx::query(
        "UPDATE proxiima_core.vector_index_debt debt
            SET enqueued_at = clock_timestamp()
          WHERE (debt.entity_id, debt.model_id, debt.dim) IN (
                SELECT d.entity_id, d.model_id, d.dim
                  FROM proxiima_core.vector_index_debt d
                  JOIN unnest($1::uuid[], $2::text[], $3::smallint[]) AS k(entity_id, model_id, dim)
                    ON d.entity_id = k.entity_id
                   AND d.model_id = k.model_id
                   AND d.dim = k.dim
                 WHERE d.generation = $4
                 ORDER BY d.entity_id, d.model_id, d.dim
                   FOR NO KEY UPDATE OF d SKIP LOCKED)",
    )
    .bind(&entities)
    .bind(&models)
    .bind(&dims)
    .bind(-claim_token)
    .execute(tx.as_mut())
    .await
    .map_err(map_err)?;
    tx.commit().await.map_err(map_err)
}

/// Best-effort cleanup when Phase-2 debt RPCs (`apply`) fail: clears
/// `-claim_token` back to `0` so the next drain retries immediately without
/// waiting for the lease timeout.
async fn release_failed_debt_claim(
    pool: &PgPool,
    platform: &crate::PgPlatformScope,
    claim_token: i64,
    claimed_keys: &[Key],
) -> Result<(), StorageError> {
    if claimed_keys.is_empty() {
        return Ok(());
    }
    let (entities, models, dims) = split_keys(claimed_keys);
    let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
    sqlx::query(
        "UPDATE proxiima_core.vector_index_debt debt
            SET generation = 0,
                enqueued_at = clock_timestamp()
          WHERE (debt.entity_id, debt.model_id, debt.dim) IN (
                SELECT d.entity_id, d.model_id, d.dim
                  FROM proxiima_core.vector_index_debt d
                  JOIN unnest($1::uuid[], $2::text[], $3::smallint[]) AS k(entity_id, model_id, dim)
                    ON d.entity_id = k.entity_id
                   AND d.model_id = k.model_id
                   AND d.dim = k.dim
                 WHERE d.generation = $4
                 ORDER BY d.entity_id, d.model_id, d.dim
                   FOR NO KEY UPDATE OF d SKIP LOCKED)",
    )
    .bind(&entities)
    .bind(&models)
    .bind(&dims)
    .bind(-claim_token)
    .execute(tx.as_mut())
    .await
    .map_err(map_err)?;
    tx.commit().await.map_err(map_err)
}

fn split_keys(keys: &[Key]) -> (Vec<Uuid>, Vec<&str>, Vec<i16>) {
    let mut entities = Vec::with_capacity(keys.len());
    let mut models = Vec::with_capacity(keys.len());
    let mut dims = Vec::with_capacity(keys.len());
    for (entity_id, model_id, dim) in keys {
        entities.push(*entity_id);
        models.push(model_id.as_str());
        dims.push(*dim);
    }
    (entities, models, dims)
}

enum ResolvedUnsent {
    Delete,
    Update {
        op: String,
        generation: i64,
        moved_from_generation: i64,
        xact: Option<String>,
    },
}

type UnsentChainRow = (
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

fn parse_prior_debt(
    prior_op: Option<String>,
    prior_gen: Option<i64>,
    prior_moved_from: Option<i64>,
    prior_xact: Option<String>,
) -> Option<PriorDebt> {
    match (prior_op, prior_gen, prior_moved_from, prior_xact) {
        (Some(op), Some(generation), Some(moved_from_generation), Some(xact)) => Some(PriorDebt {
            op,
            generation,
            moved_from_generation,
            xact,
        }),
        _ => None,
    }
}

async fn consume_unsent_predecessor(
    tx: &mut PgConnection,
    entity_id: Uuid,
    model_id: &str,
    dim: i16,
    generation: i64,
) -> Result<Option<(String, Option<PriorDebt>)>, StorageError> {
    let row: Option<UnsentChainRow> = sqlx::query_as(
        "DELETE FROM proxiima_core.vector_index_unsent_intents
          WHERE entity_id = $1
            AND model_id = $2
            AND dim = $3
            AND generation = $4
         RETURNING tx1_xact::text,
                   prior_op,
                   prior_generation,
                   prior_moved_from_generation,
                   prior_xact::text",
    )
    .bind(entity_id)
    .bind(model_id)
    .bind(dim)
    .bind(generation)
    .fetch_optional(tx)
    .await
    .map_err(map_err)?;
    Ok(row.map(|(tx1_xact, p_op, p_gen, p_moved, p_xact)| {
        (tx1_xact, parse_prior_debt(p_op, p_gen, p_moved, p_xact))
    }))
}

/// Walk any chain of confirmed-unsent predecessor generations in
/// `proxiima_core.vector_index_unsent_intents`, returning the exact final debt
/// action (`Delete`, payable `'owner'`/`'head'`/`'delete'`, or restoring an
/// unresolved live predecessor with a fresh bounded renewal grace window), as
/// `Proxiima.Embeddings.unsentRestore` decides; `saw_transfer` is its
/// `ownerMoved` and `saw_head_flip` its `headFlipped`.
async fn walk_unsent_chain(
    tx: &mut PgConnection,
    entity_id: Uuid,
    model_id: &str,
    dim: i16,
    initial_prior: Option<PriorDebt>,
    mut saw_transfer: bool,
    mut saw_head_flip: bool,
) -> Result<ResolvedUnsent, StorageError> {
    let mut cur_prior = initial_prior;
    loop {
        match cur_prior {
            None => {
                if saw_transfer {
                    return Ok(ResolvedUnsent::Update {
                        op: "owner".to_owned(),
                        generation: 0,
                        moved_from_generation: 0,
                        xact: None,
                    });
                }
                if saw_head_flip {
                    return Ok(ResolvedUnsent::Update {
                        op: "head".to_owned(),
                        generation: 0,
                        moved_from_generation: 0,
                        xact: None,
                    });
                }
                return Ok(ResolvedUnsent::Delete);
            }
            Some(prior) if prior.generation > 0 || prior.moved_from_generation > 0 => {
                let pred_gen = if prior.generation > 0 {
                    prior.generation
                } else {
                    prior.moved_from_generation
                };
                if prior.moved_from_generation > 0 {
                    saw_transfer = true;
                }
                if let Some((pred_tx1_xact, pred_prior)) =
                    consume_unsent_predecessor(tx, entity_id, model_id, dim, pred_gen).await?
                {
                    if prior.xact != pred_tx1_xact {
                        saw_head_flip = true;
                    }
                    cur_prior = pred_prior;
                    continue;
                }
                let (generation, moved_from_generation) = if saw_transfer {
                    (0, pred_gen)
                } else {
                    (pred_gen, 0)
                };
                let xact = (!saw_head_flip).then_some(prior.xact);
                return Ok(ResolvedUnsent::Update {
                    op: "delete".to_owned(),
                    generation,
                    moved_from_generation,
                    xact,
                });
            }
            Some(prior) => {
                let restored_op = if prior.op == "delete" {
                    "delete".to_owned()
                } else if saw_transfer || prior.op == "owner" {
                    "owner".to_owned()
                } else if saw_head_flip || prior.op == "head" {
                    "head".to_owned()
                } else {
                    prior.op
                };
                return Ok(ResolvedUnsent::Update {
                    op: restored_op,
                    generation: 0,
                    moved_from_generation: 0,
                    xact: None,
                });
            }
        }
    }
}

/// Resolve a locked `vector_index_debt` row in Phase 1 before overwriting it
/// with a newer publish intent: consumes any matching durable unsent chain in
/// `vector_index_unsent_intents` so retries inherit the true prior debt, or
/// preserves an unresolved live predecessor's exact generation.
pub(crate) async fn resolve_prior_debt_for_takeover(
    tx: &mut PgConnection,
    entity_id: Uuid,
    model_id: &str,
    dim: i16,
    prior: PriorDebt,
) -> Result<Option<PriorDebt>, StorageError> {
    if prior.generation <= 0 && prior.moved_from_generation == 0 {
        return Ok(Some(PriorDebt {
            generation: 0,
            ..prior
        }));
    }
    let fallback_xact = prior.xact.clone();
    match walk_unsent_chain(tx, entity_id, model_id, dim, Some(prior), false, false).await? {
        ResolvedUnsent::Delete => Ok(None),
        ResolvedUnsent::Update {
            op,
            generation,
            moved_from_generation,
            xact,
        } => Ok(Some(PriorDebt {
            op,
            generation,
            moved_from_generation,
            xact: xact.unwrap_or(fallback_xact),
        })),
    }
}

async fn apply_resolved_unsent(
    tx: &mut PgConnection,
    entity_id: Uuid,
    model_id: &str,
    dim: i16,
    up_to_generation: i64,
    resolved: ResolvedUnsent,
) -> Result<bool, StorageError> {
    match resolved {
        ResolvedUnsent::Delete => {
            sqlx::query(
                "DELETE FROM proxiima_core.vector_index_debt
                  WHERE entity_id = $1 AND model_id = $2 AND dim = $3",
            )
            .bind(entity_id)
            .bind(model_id)
            .bind(dim)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
            sqlx::query(
                "DELETE FROM proxiima_core.vector_index_unsent_intents
                  WHERE entity_id = $1
                    AND model_id = $2
                    AND dim = $3
                    AND generation <= $4",
            )
            .bind(entity_id)
            .bind(model_id)
            .bind(dim)
            .bind(up_to_generation)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
            Ok(true)
        }
        ResolvedUnsent::Update {
            op,
            generation,
            moved_from_generation,
            xact,
        } => {
            // Stamp a fresh `clock_timestamp()` lease when restoring an
            // unresolved predecessor whose own heartbeats could not match while
            // the newer attempt owned the row, giving a live predecessor one
            // bounded lease window to resume heartbeating or finish unsent
            // while still bounding recovery if it crashed.
            sqlx::query(
                "UPDATE proxiima_core.vector_index_debt
                    SET op = $4,
                        generation = $5,
                        moved_from_generation = $6,
                        enqueued_at = clock_timestamp(),
                        xact = COALESCE($7::xid8, pg_current_xact_id())
                  WHERE entity_id = $1 AND model_id = $2 AND dim = $3",
            )
            .bind(entity_id)
            .bind(model_id)
            .bind(dim)
            .bind(&op)
            .bind(generation)
            .bind(moved_from_generation)
            .bind(xact.as_deref())
            .execute(tx)
            .await
            .map_err(map_err)?;
            Ok(false)
        }
    }
}

/// Durably record one skipped unsent publish intent in
/// `proxiima_core.vector_index_unsent_intents` inside the caller's transaction
/// (which already holds `UNSENT_INTENTS_ERASE_BARRIER_KEY` shared). Takes zero
/// row locks on `vector_index_debt` (`EXISTS` is a pure MVCC check), so even a
/// concurrent `FOR UPDATE` or uncommitted `DELETE` on the parent debt row never
/// blocks a 1-connection platform pool or rolls back sibling progress, while
/// hard erase's exclusive barrier clears any matching rows before commit.
async fn record_pending_unsent_intent_in_tx(
    tx: &mut PgConnection,
    intent: &StagedPublishIntent,
) -> Result<(), StorageError> {
    let (prior_op, prior_gen, prior_moved, prior_xact) = match &intent.prior_debt {
        Some(prior) => (
            Some(prior.op.as_str()),
            Some(prior.generation),
            Some(prior.moved_from_generation),
            Some(prior.xact.as_str()),
        ),
        None => (None, None, None, None),
    };
    sqlx::query(
        "INSERT INTO proxiima_core.vector_index_unsent_intents
             (entity_id, model_id, dim, generation, tx1_xact,
              prior_op, prior_generation, prior_moved_from_generation, prior_xact)
         SELECT $1, $2, $3, $4, $5::xid8, $6, $7, $8, $9::xid8
          WHERE EXISTS (
                SELECT 1 FROM proxiima_core.vector_index_debt d
                 WHERE d.entity_id = $1
                   AND d.model_id = $2
                   AND d.dim = $3
                   AND (d.generation >= $4 OR d.moved_from_generation >= $4)
          )
         ON CONFLICT (entity_id, model_id, dim, generation) DO NOTHING",
    )
    .bind(intent.entity_id)
    .bind(&intent.model_id)
    .bind(intent.dim)
    .bind(intent.generation)
    .bind(&intent.tx1_xact)
    .bind(prior_op)
    .bind(prior_gen)
    .bind(prior_moved)
    .bind(prior_xact)
    .execute(tx)
    .await
    .map(drop)
    .map_err(map_err)
}

type PendingUnsentRow = (
    Uuid,
    String,
    i16,
    i64,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

struct ReconcileUnsentCounts {
    retired: u64,
    updated: u64,
}

async fn reconcile_one_pending_unsent(
    tx: &mut PgConnection,
    row: PendingUnsentRow,
) -> Result<Option<bool>, StorageError> {
    let (entity_id, model_id, dim, generation, tx1_xact, p_op, p_gen, p_moved, p_xact) = row;
    let locked: Option<(i64, String)> = sqlx::query_as(
        "SELECT d.moved_from_generation, d.xact::text
           FROM proxiima_core.vector_index_debt d
          WHERE d.entity_id = $1
            AND d.model_id = $2
            AND d.dim = $3
            AND (d.generation = $4 OR d.moved_from_generation = $4)
            FOR NO KEY UPDATE OF d SKIP LOCKED",
    )
    .bind(entity_id)
    .bind(&model_id)
    .bind(dim)
    .bind(generation)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_err)?;
    if let Some((current_moved_from, current_xact)) = locked {
        let prior_debt = parse_prior_debt(p_op, p_gen, p_moved, p_xact);
        let resolved = walk_unsent_chain(
            &mut *tx,
            entity_id,
            &model_id,
            dim,
            prior_debt,
            current_moved_from == generation,
            current_xact != tx1_xact,
        )
        .await?;
        let deleted_debt =
            apply_resolved_unsent(&mut *tx, entity_id, &model_id, dim, generation, resolved)
                .await?;
        if !deleted_debt {
            sqlx::query(
                "DELETE FROM proxiima_core.vector_index_unsent_intents
                  WHERE entity_id = $1 AND model_id = $2 AND dim = $3 AND generation = $4",
            )
            .bind(entity_id)
            .bind(&model_id)
            .bind(dim)
            .bind(generation)
            .execute(&mut *tx)
            .await
            .map_err(map_err)?;
        }
        return Ok(Some(deleted_debt));
    }
    // Keep alive if the matching debt row is currently locked or has been taken
    // over by a newer live publish generation that may still consult this row.
    let still_needed: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM proxiima_core.vector_index_debt d
              WHERE d.entity_id = $1
                AND d.model_id = $2
                AND d.dim = $3
                AND (d.generation >= $4 OR d.moved_from_generation >= $4)
         )",
    )
    .bind(entity_id)
    .bind(&model_id)
    .bind(dim)
    .bind(generation)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_err)?;
    if still_needed {
        return Ok(None);
    }
    sqlx::query(
        "DELETE FROM proxiima_core.vector_index_unsent_intents
          WHERE entity_id = $1 AND model_id = $2 AND dim = $3 AND generation = $4",
    )
    .bind(entity_id)
    .bind(&model_id)
    .bind(dim)
    .bind(generation)
    .execute(&mut *tx)
    .await
    .map_err(map_err)?;
    Ok(Some(true))
}

/// Non-blocking keyset reconciliation of durable pending records in
/// `proxiima_core.vector_index_unsent_intents` before `claim_debt_batch` scans
/// `vector_index_debt`: applies unsent restoration to unlocked matching debt
/// rows, skips markers still needed by locked or newer in-flight publish
/// generations without starving later rows, and prunes obsolete markers.
async fn reconcile_pending_unsent_intents(
    pool: &PgPool,
    platform: &crate::PgPlatformScope,
    limit: i64,
) -> Result<ReconcileUnsentCounts, StorageError> {
    let target = u64::try_from(limit.max(DEBT_SCAN_MIN_PAGE)).unwrap_or(u64::MAX);
    let page_limit = limit.max(DEBT_SCAN_MIN_PAGE);
    let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
    let mut counts = ReconcileUnsentCounts {
        retired: 0,
        updated: 0,
    };
    let mut cursor: Option<(Uuid, String, i16, i64)> = None;
    while counts.retired.saturating_add(counts.updated) < target {
        let pending: Vec<PendingUnsentRow> = match &cursor {
            None => sqlx::query_as(
                "SELECT u.entity_id, u.model_id, u.dim, u.generation, u.tx1_xact::text,
                        u.prior_op, u.prior_generation, u.prior_moved_from_generation,
                        u.prior_xact::text
                   FROM proxiima_core.vector_index_unsent_intents u
                  WHERE NOT EXISTS (
                        SELECT 1 FROM proxiima_core.vector_index_debt d
                         WHERE d.entity_id = u.entity_id
                           AND d.model_id = u.model_id
                           AND d.dim = u.dim
                           AND (d.generation > u.generation OR d.moved_from_generation > u.generation)
                  )
                  ORDER BY u.entity_id, u.model_id COLLATE \"C\", u.dim, u.generation
                  LIMIT $1
                    FOR UPDATE OF u SKIP LOCKED",
            )
            .bind(page_limit)
            .fetch_all(tx.as_mut())
            .await
            .map_err(map_err)?,
            Some((after_e, after_m, after_d, after_g)) => sqlx::query_as(
                "SELECT u.entity_id, u.model_id, u.dim, u.generation, u.tx1_xact::text,
                        u.prior_op, u.prior_generation, u.prior_moved_from_generation,
                        u.prior_xact::text
                   FROM proxiima_core.vector_index_unsent_intents u
                  WHERE NOT EXISTS (
                        SELECT 1 FROM proxiima_core.vector_index_debt d
                         WHERE d.entity_id = u.entity_id
                           AND d.model_id = u.model_id
                           AND d.dim = u.dim
                           AND (d.generation > u.generation OR d.moved_from_generation > u.generation)
                  )
                    AND (u.entity_id, u.model_id COLLATE \"C\", u.dim, u.generation)
                        > ($1, $2::text COLLATE \"C\", $3, $4)
                  ORDER BY u.entity_id, u.model_id COLLATE \"C\", u.dim, u.generation
                  LIMIT $5
                    FOR UPDATE OF u SKIP LOCKED",
            )
            .bind(after_e)
            .bind(after_m)
            .bind(after_d)
            .bind(after_g)
            .bind(page_limit)
            .fetch_all(tx.as_mut())
            .await
            .map_err(map_err)?,
        };
        if pending.is_empty() {
            break;
        }
        let exhausted = pending.len() < usize::try_from(page_limit).unwrap_or(usize::MAX);
        for row in pending {
            cursor = Some((row.0, row.1.clone(), row.2, row.3));
            match reconcile_one_pending_unsent(tx.as_mut(), row).await? {
                Some(true) => counts.retired += 1,
                Some(false) => counts.updated += 1,
                None => {}
            }
            if counts.retired.saturating_add(counts.updated) >= target {
                break;
            }
        }
        if exhausted {
            break;
        }
    }
    tx.commit().await.map_err(map_err)?;
    Ok(counts)
}

async fn settle_uncertain_publish_intent(
    tx: &mut PgConnection,
    key: &Key,
    generation: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE proxiima_core.vector_index_debt
            SET op = 'delete',
                generation = 0,
                moved_from_generation = 0,
                enqueued_at = clock_timestamp()
          WHERE entity_id = $1 AND model_id = $2 AND dim = $3",
    )
    .bind(key.0)
    .bind(&key.1)
    .bind(key.2)
    .execute(&mut *tx)
    .await
    .map_err(map_err)?;
    sqlx::query(
        "DELETE FROM proxiima_core.vector_index_unsent_intents
          WHERE entity_id = $1 AND model_id = $2 AND dim = $3 AND generation <= $4",
    )
    .bind(key.0)
    .bind(&key.1)
    .bind(key.2)
    .bind(generation)
    .execute(&mut *tx)
    .await
    .map(drop)
    .map_err(map_err)
}

/// Non-blocking cleanup when Phase 2 (`VectorIndex::put_batch`) returns an
/// error, running in a single single-connection transaction:
/// - Acquires `UNSENT_INTENTS_ERASE_BARRIER_KEY` shared so hard erase's
///   short-held exclusive barrier linearizes `vector_index_unsent_intents`
///   cleanup without any foreign-key wait on `vector_index_debt`.
/// - For unlocked keys with uncertain delivery (not named in `unsent_indices`),
///   clears `generation` and `moved_from_generation` back to `0` and drops any
///   obsolete unsent markers so ordinary drain immediately removes partially
///   written orphan vectors without waiting for the lease timeout.
/// - For unlocked keys the provider knows it never dispatched (`unsent_indices`),
///   walks any predecessor unsent chain and restores the exact pre-Phase-1 debt
///   state (deleting newly staged rows when untouched, downgrading to `'owner'`
///   or `'head'` if transfer or `memory_head` changed during Phase 2, or
///   restoring an unresolved live predecessor's generation with a fresh lease).
/// - For any unsent key skipped by `SKIP LOCKED` (contended by a trigger,
///   `FOR UPDATE`, uncommitted `DELETE`, or taken over by a retry), records its
///   disposition durably in `vector_index_unsent_intents` in the same
///   transaction with zero locks on `vector_index_debt`, so a 1-connection
///   platform pool never blocks and sibling progress commits immediately.
/// - For a key whose row no longer carries this generation or a newer one (a
///   purge reset it to a settled delete, or a drain paid it), touches nothing:
///   the settled debt stays and no unsent record is written
///   (`Proxiima.Embeddings.an_invalidated_intent_keeps_its_settled_debt`).
///
/// The step per entry is `Proxiima.Embeddings.failedPutStep`.
pub(crate) async fn release_failed_publish_intents(
    pool: &PgPool,
    platform: &crate::PgPlatformScope,
    intents: &[StagedPublishIntent],
    unsent_indices: &HashSet<usize>,
) -> Result<(), StorageError> {
    if intents.is_empty() {
        return Ok(());
    }
    let mut entities = Vec::with_capacity(intents.len());
    let mut models = Vec::with_capacity(intents.len());
    let mut dims = Vec::with_capacity(intents.len());
    let mut generations = Vec::with_capacity(intents.len());
    for intent in intents {
        entities.push(intent.entity_id);
        models.push(intent.model_id.as_str());
        dims.push(intent.dim);
        generations.push(intent.generation);
    }
    let by_key: HashMap<Key, (usize, &StagedPublishIntent)> = intents
        .iter()
        .enumerate()
        .map(|(idx, intent)| {
            (
                (intent.entity_id, intent.model_id.clone(), intent.dim),
                (idx, intent),
            )
        })
        .collect();
    let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
    lock_unsent_intents_erase_barrier_shared(tx.as_mut()).await?;
    let locked: Vec<(Uuid, String, i16, i64, String)> = sqlx::query_as(
        "SELECT d.entity_id, d.model_id, d.dim, d.moved_from_generation, d.xact::text
           FROM proxiima_core.vector_index_debt d
           JOIN unnest($1::uuid[], $2::text[], $3::smallint[], $4::bigint[])
                AS k(entity_id, model_id, dim, generation)
             ON d.entity_id = k.entity_id
            AND d.model_id = k.model_id
            AND d.dim = k.dim
            AND (d.generation = k.generation OR d.moved_from_generation = k.generation)
          ORDER BY d.entity_id, d.model_id, d.dim
            FOR NO KEY UPDATE OF d SKIP LOCKED",
    )
    .bind(&entities)
    .bind(&models)
    .bind(&dims)
    .bind(&generations)
    .fetch_all(tx.as_mut())
    .await
    .map_err(map_err)?;
    let mut settled_keys = HashSet::with_capacity(locked.len());
    for (entity_id, model_id, dim, current_moved_from, current_xact) in locked {
        let key = (entity_id, model_id, dim);
        let Some(&(idx, intent)) = by_key.get(&key) else {
            continue;
        };
        settled_keys.insert(key.clone());
        if !unsent_indices.contains(&idx) {
            settle_uncertain_publish_intent(tx.as_mut(), &key, intent.generation).await?;
            continue;
        }
        let resolved = walk_unsent_chain(
            tx.as_mut(),
            key.0,
            &key.1,
            key.2,
            intent.prior_debt.clone(),
            current_moved_from == intent.generation,
            current_xact != intent.tx1_xact,
        )
        .await?;
        let _ = apply_resolved_unsent(
            tx.as_mut(),
            key.0,
            &key.1,
            key.2,
            intent.generation,
            resolved,
        )
        .await?;
    }
    for (idx, intent) in intents.iter().enumerate() {
        if unsent_indices.contains(&idx)
            && !settled_keys.contains(&(intent.entity_id, intent.model_id.clone(), intent.dim))
        {
            record_pending_unsent_intent_in_tx(tx.as_mut(), intent).await?;
        }
    }
    tx.commit().await.map_err(map_err)
}

const DEBT_CANDIDATES_FIRST_SQL: &str = "
SELECT d.entity_id, d.model_id, d.dim, d.owner_id, eh.owner_id
  FROM proxiima_core.vector_index_debt d
  LEFT JOIN proxiima_core.embedding_heads eh
    ON eh.entity_id = d.entity_id
   AND eh.model_id = d.model_id
   AND eh.dim = d.dim
 WHERE NOT ((d.generation <> 0 OR d.moved_from_generation > 0)
            AND d.enqueued_at > clock_timestamp() - make_interval(secs => $1::double precision))
   AND NOT EXISTS (
       SELECT 1 FROM proxiima_core.vector_index_unsent_intents u
        WHERE u.entity_id = d.entity_id
          AND u.model_id = d.model_id
          AND u.dim = d.dim
          AND (u.generation = d.generation OR u.generation = d.moved_from_generation)
   )
 ORDER BY d.entity_id, d.model_id COLLATE \"C\", d.dim
 LIMIT $2";

const DEBT_CANDIDATES_AFTER_SQL: &str = "
SELECT d.entity_id, d.model_id, d.dim, d.owner_id, eh.owner_id
  FROM proxiima_core.vector_index_debt d
  LEFT JOIN proxiima_core.embedding_heads eh
    ON eh.entity_id = d.entity_id
   AND eh.model_id = d.model_id
   AND eh.dim = d.dim
 WHERE NOT ((d.generation <> 0 OR d.moved_from_generation > 0)
            AND d.enqueued_at > clock_timestamp() - make_interval(secs => $1::double precision))
   AND NOT EXISTS (
       SELECT 1 FROM proxiima_core.vector_index_unsent_intents u
        WHERE u.entity_id = d.entity_id
          AND u.model_id = d.model_id
          AND u.dim = d.dim
          AND (u.generation = d.generation OR u.generation = d.moved_from_generation)
   )
   AND (d.entity_id, d.model_id COLLATE \"C\", d.dim) > ($2, $3::text COLLATE \"C\", $4)
 ORDER BY d.entity_id, d.model_id COLLATE \"C\", d.dim
 LIMIT $5";

/// Try non-blocking owner fences (level 1) and embedding key locks (level 5) on
/// up to `needed` candidates from `page`, then lock matching eligible debt rows
/// with `SKIP LOCKED`. Returns the locked rows and the last inspected key.
async fn lock_debt_page(
    tx: &mut PgConnection,
    page: &[(Uuid, String, i16, Uuid, Option<Uuid>)],
    needed: usize,
    lease_seconds: f64,
) -> Result<(Vec<PoppedDebtRow>, Option<Key>), StorageError> {
    let (mut lock_entities, mut lock_models, mut lock_dims) = (Vec::new(), Vec::new(), Vec::new());
    let mut last_seen = None;
    for (entity_id, model_id, dim, debt_owner, live_owner) in page {
        if lock_entities.len() >= needed {
            break;
        }
        last_seen = Some((*entity_id, model_id.clone(), *dim));
        let mut owners = vec![OwnerId::new(*debt_owner)];
        if let Some(live) = live_owner {
            owners.push(OwnerId::new(*live));
        }
        if !try_lock_owners_shared(&mut *tx, &owners).await? {
            continue;
        }
        if try_lock_embedding_key(&mut *tx, *entity_id, model_id, *dim).await? {
            lock_entities.push(*entity_id);
            lock_models.push(model_id.as_str());
            lock_dims.push(*dim);
        }
    }
    if lock_entities.is_empty() {
        return Ok((Vec::new(), last_seen));
    }
    let locked: Vec<PoppedDebtRow> = sqlx::query_as(
        "SELECT d.entity_id, d.model_id, d.dim, d.owner_id, d.op, d.generation,
                d.moved_from_generation
           FROM proxiima_core.vector_index_debt d
           JOIN unnest($1::uuid[], $2::text[], $3::smallint[]) AS k(entity_id, model_id, dim)
             ON d.entity_id = k.entity_id
            AND d.model_id = k.model_id
            AND d.dim = k.dim
          WHERE NOT ((d.generation <> 0 OR d.moved_from_generation > 0)
                     AND d.enqueued_at > clock_timestamp() - make_interval(secs => $4::double precision))
            AND NOT EXISTS (
                SELECT 1 FROM proxiima_core.vector_index_unsent_intents u
                 WHERE u.entity_id = d.entity_id
                   AND u.model_id = d.model_id
                   AND u.dim = d.dim
                   AND (u.generation = d.generation OR u.generation = d.moved_from_generation)
            )
            FOR NO KEY UPDATE OF d SKIP LOCKED",
    )
    .bind(&lock_entities)
    .bind(&lock_models)
    .bind(&lock_dims)
    .bind(lease_seconds)
    .fetch_all(&mut *tx)
    .await
    .map_err(map_err)?;
    Ok((locked, last_seen))
}

/// Phase 1 of `drain_debt`: scan past locked prefixes, live-leased publish
/// intents, deferred unsent markers, and active concurrent drain claims until
/// `limit` payable debt rows are locked (or the queue is exhausted), allocate a
/// monotonic `claim_token`, durably stamp `generation = -claim_token`,
/// `moved_from_generation = 0`, and `enqueued_at = clock_timestamp()`, resolve
/// live metadata, and commit.
async fn claim_debt_batch(
    pool: &PgPool,
    platform: &crate::PgPlatformScope,
    limit: i64,
    lease_seconds: f64,
) -> Result<Option<ClaimedDebtBatch>, StorageError> {
    let target = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
    let mut cursor: Option<Key> = None;
    let mut locked: Vec<PoppedDebtRow> = Vec::new();
    while locked.len() < target {
        let needed = target - locked.len();
        let page_limit = i64::try_from(needed)
            .unwrap_or(i64::MAX)
            .max(DEBT_SCAN_MIN_PAGE);
        let candidates: Vec<(Uuid, String, i16, Uuid, Option<Uuid>)> = match &cursor {
            None => sqlx::query_as(DEBT_CANDIDATES_FIRST_SQL)
                .bind(lease_seconds)
                .bind(page_limit)
                .fetch_all(tx.as_mut())
                .await
                .map_err(map_err)?,
            Some((after_entity, after_model, after_dim)) => {
                sqlx::query_as(DEBT_CANDIDATES_AFTER_SQL)
                    .bind(lease_seconds)
                    .bind(after_entity)
                    .bind(after_model)
                    .bind(after_dim)
                    .bind(page_limit)
                    .fetch_all(tx.as_mut())
                    .await
                    .map_err(map_err)?
            }
        };
        if candidates.is_empty() {
            break;
        }
        let exhausted = candidates.len() < usize::try_from(page_limit).unwrap_or(usize::MAX);
        let (mut page_locked, last_seen) =
            lock_debt_page(tx.as_mut(), &candidates, needed, lease_seconds).await?;
        let inspected_all = last_seen.as_ref().is_some_and(|last| {
            candidates
                .last()
                .is_some_and(|(e, m, d, _, _)| last.0 == *e && last.1 == *m && last.2 == *d)
        });
        locked.append(&mut page_locked);
        cursor = last_seen;
        if exhausted && inspected_all {
            break;
        }
    }
    if locked.is_empty() {
        tx.commit().await.map_err(map_err)?;
        return Ok(None);
    }
    let claim_token = next_vector_generation(tx.as_mut()).await?;
    let rows = resolve_live_debt_metadata(tx.as_mut(), locked).await?;
    let claimed_keys: Vec<Key> = rows
        .iter()
        .map(|(entity_id, model_id, dim, _, _, _)| (*entity_id, model_id.clone(), *dim))
        .collect();
    stamp_claimed_debt_batch(tx.as_mut(), claim_token, &rows).await?;
    tx.commit().await.map_err(map_err)?;
    Ok(Some(ClaimedDebtBatch {
        claim_token,
        claimed_keys,
        rows,
    }))
}

/// Phase 3 of `drain_debt`: delete only the claimed debt rows still carrying
/// `-claim_token`, locking candidate rows in key order with `SKIP LOCKED` so
/// finalization never blocks or deadlocks against a concurrent multi-key erase.
/// Returns the exact count of rows actually finalized (deleted).
async fn finish_debt_batch(
    pool: &PgPool,
    platform: &crate::PgPlatformScope,
    claim_token: i64,
    claimed_keys: &[Key],
) -> Result<u64, StorageError> {
    let (entities, models, dims) = split_keys(claimed_keys);
    let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
    let deleted: Vec<Key> = sqlx::query_as(
        "DELETE FROM proxiima_core.vector_index_debt debt
          WHERE (debt.entity_id, debt.model_id, debt.dim) IN (
                SELECT d.entity_id, d.model_id, d.dim
                  FROM proxiima_core.vector_index_debt d
                  JOIN unnest($1::uuid[], $2::text[], $3::smallint[]) AS k(entity_id, model_id, dim)
                    ON d.entity_id = k.entity_id
                   AND d.model_id = k.model_id
                   AND d.dim = k.dim
                 WHERE d.generation = $4
                 ORDER BY d.entity_id, d.model_id, d.dim
                   FOR UPDATE OF d SKIP LOCKED)
         RETURNING debt.entity_id, debt.model_id, debt.dim",
    )
    .bind(&entities)
    .bind(&models)
    .bind(&dims)
    .bind(-claim_token)
    .fetch_all(tx.as_mut())
    .await
    .map_err(map_err)?;
    if !deleted.is_empty() {
        let (del_entities, del_models, del_dims) = split_keys(&deleted);
        sqlx::query(
            "DELETE FROM proxiima_core.vector_index_unsent_intents u
              USING unnest($1::uuid[], $2::text[], $3::smallint[]) AS k(entity_id, model_id, dim)
              WHERE u.entity_id = k.entity_id
                AND u.model_id = k.model_id
                AND u.dim = k.dim
                AND u.generation < $4",
        )
        .bind(&del_entities)
        .bind(&del_models)
        .bind(&del_dims)
        .bind(claim_token)
        .execute(tx.as_mut())
        .await
        .map_err(map_err)?;
    }
    tx.commit().await.map_err(map_err)?;
    Ok(count_u64(deleted.len()))
}

/// Pay up to `limit` rows of index debt using single-connection phased
/// transactions with default lease settings. Returns how many rows were
/// actually finalized.
#[cfg(test)]
pub(crate) async fn drain_debt(
    pool: &PgPool,
    platform: &crate::PgPlatformScope,
    vectors: &StoreVectors,
    limit: i64,
) -> Result<u64, StorageError> {
    drain_debt_with_lease(
        pool,
        platform,
        vectors,
        limit,
        DEBT_LEASE_SECONDS,
        DEBT_LEASE_RENEW_INTERVAL,
    )
    .await
}

/// Pay up to `limit` rows of index debt using single-connection phased
/// transactions: Tx A claims payable rows under a recoverable lease
/// (`generation = -claim_token`, `enqueued_at = clock_timestamp()`), Phase 2
/// applies provider RPCs with periodic single-connection lease renewal and
/// immediate best-effort lease release on error, and Tx B removes only rows
/// still bearing `-claim_token`. Returns the exact count of rows finalized.
pub(crate) async fn drain_debt_with_lease(
    pool: &PgPool,
    platform: &crate::PgPlatformScope,
    vectors: &StoreVectors,
    limit: i64,
    lease_seconds: f64,
    renew_interval: Duration,
) -> Result<u64, StorageError> {
    if limit <= 0 {
        return Ok(0);
    }
    let StoreVectors::Remote(index) = vectors else {
        let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
        let deleted = sqlx::query(
            "DELETE FROM proxiima_core.vector_index_debt debt
              WHERE (debt.entity_id, debt.model_id, debt.dim) IN (
                    SELECT entity_id, model_id, dim FROM proxiima_core.vector_index_debt
                     LIMIT $1
                     FOR UPDATE SKIP LOCKED)",
        )
        .bind(limit)
        .execute(tx.as_mut())
        .await
        .map_err(map_err)?;
        tx.commit().await.map_err(map_err)?;
        return Ok(deleted.rows_affected());
    };
    let unsent = reconcile_pending_unsent_intents(pool, platform, limit).await?;
    let Some(batch) = claim_debt_batch(pool, platform, limit, lease_seconds).await? else {
        return Ok(unsent.retired.saturating_add(unsent.updated));
    };
    let apply_result = {
        let apply_fut = apply(index.as_ref(), batch.rows, batch.claim_token);
        tokio::pin!(apply_fut);
        let mut ticker = tokio::time::interval(renew_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                res = &mut apply_fut => break res,
                _ = ticker.tick() => {
                    let _ = renew_debt_claim(
                        pool,
                        platform,
                        batch.claim_token,
                        &batch.claimed_keys,
                    )
                    .await;
                }
            }
        }
    };
    if let Err(err) = apply_result {
        let _ =
            release_failed_debt_claim(pool, platform, batch.claim_token, &batch.claimed_keys).await;
        return Err(err);
    }
    let finalized =
        finish_debt_batch(pool, platform, batch.claim_token, &batch.claimed_keys).await?;
    Ok(if finalized > 0 {
        finalized.saturating_add(unsent.retired)
    } else {
        unsent.retired.saturating_add(unsent.updated)
    })
}

/// Index entries, and heads, compared per page by the sweep.
const SWEEP_PAGE: u32 = proxiima_core::ledger!(Proxiima.Embeddings.reconcilePageSize);

/// A key in the order both sides page in: entity, model id bytewise, dim as the column holds it.
type Key = (Uuid, String, i16);

/// What one head holds: its owner, embedding version, monotonic generation, and
/// series-head status.
type Head = (OwnerId, i32, i64, bool);

/// A stored head row read during a sweep page, pairing its comparable [`Head`]
/// metadata with the PostgreSQL tuple `xmin` generation so `repair` never
/// deletes a freshly recreated head that happens to reuse version `1`.
#[derive(Clone, PartialEq, Eq)]
struct StoredHead {
    head: Head,
    row_xmin: String,
}

/// The sweep key of an index entry's key; a width no column holds is a constraint violation.
fn key_of(key: &VectorKey) -> Result<Key, StorageError> {
    let (entity, model, width) = key.order();
    let dim = i16::try_from(width).map_err(|_| {
        StorageError::ConstraintViolation(format!("index entry names width {width}"))
    })?;
    Ok((entity, model.to_owned(), dim))
}

fn vector_key((entity_id, model_id, dim): &Key) -> Result<VectorKey, StorageError> {
    Ok(VectorKey {
        entity_id: *entity_id,
        space: space_of(model_id.clone(), *dim)?,
    })
}

/// First keyset page over `embedding_heads` in `VectorKey::order`, backed by
/// `embedding_heads_sweep_key_idx`. Keeps `is_head` lookup out of the page
/// scan so PostgreSQL never attaches a whole-table hashed subplan to each page.
const HEADS_FIRST_SQL: &str = "
SELECT eh.entity_id,
       eh.model_id,
       eh.dim,
       eh.owner_id,
       eh.embedding_version,
       eh.vector_generation,
       eh.xmin::text
  FROM proxiima_core.embedding_heads eh
 ORDER BY eh.entity_id, eh.model_id COLLATE \"C\", eh.dim
 LIMIT $1";

/// Continuation keyset page over `embedding_heads` in `VectorKey::order`.
/// Kept separate from [`HEADS_FIRST_SQL`] so PostgreSQL generic plans compile
/// the tuple comparison as an `Index Cond` on `embedding_heads_sweep_key_idx`
/// instead of scanning from the start with a `Filter`.
const HEADS_AFTER_SQL: &str = "
SELECT eh.entity_id,
       eh.model_id,
       eh.dim,
       eh.owner_id,
       eh.embedding_version,
       eh.vector_generation,
       eh.xmin::text
  FROM proxiima_core.embedding_heads eh
 WHERE (eh.entity_id, eh.model_id COLLATE \"C\", eh.dim) > ($1, $2::text COLLATE \"C\", $3)
 ORDER BY eh.entity_id, eh.model_id COLLATE \"C\", eh.dim
 LIMIT $4";

/// Scoped `is_head` resolution for one sweep page's `(entity_id, owner_id)`
/// pairs, probing `goal_t_key` and `memory_head_owner_t_idx` by key rather than
/// scanning either table corpus-wide.
const HEADS_IS_HEAD_SQL: &str = "
SELECT g.t
  FROM unnest($1::uuid[]) AS k(entity_id)
  JOIN proxiima_core.goal g ON g.t = k.entity_id
UNION
SELECT mh.t
  FROM unnest($1::uuid[], $2::uuid[]) AS k(entity_id, owner_id)
  JOIN proxiima_core.memory_head mh
    ON mh.owner_id = k.owner_id AND mh.t = k.entity_id";

/// The first-page sweep head query, for EXPLAIN-based plan guards.
#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
#[must_use]
pub fn heads_first_sql_for_tests() -> &'static str {
    HEADS_FIRST_SQL
}

/// The continuation sweep head-page statement, for EXPLAIN-based plan guards.
#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
#[must_use]
pub fn heads_after_sql_for_tests() -> &'static str {
    HEADS_AFTER_SQL
}

/// The scoped `is_head` page lookup statement, for EXPLAIN-based plan guards.
#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
#[must_use]
pub fn heads_is_head_sql_for_tests() -> &'static str {
    HEADS_IS_HEAD_SQL
}

/// Up to [`SWEEP_PAGE`] heads after `after`, in key order, with `is_head`
/// resolved strictly for the returned page.
async fn heads_after(
    tx: &mut PgConnection,
    after: Option<&Key>,
) -> Result<Vec<(Key, StoredHead)>, StorageError> {
    let rows: Vec<(Uuid, String, i16, Uuid, i32, i64, String)> = match after {
        None => sqlx::query_as(HEADS_FIRST_SQL)
            .bind(i64::from(SWEEP_PAGE))
            .fetch_all(&mut *tx)
            .await
            .map_err(map_err)?,
        Some(key) => sqlx::query_as(HEADS_AFTER_SQL)
            .bind(key.0)
            .bind(&key.1)
            .bind(key.2)
            .bind(i64::from(SWEEP_PAGE))
            .fetch_all(&mut *tx)
            .await
            .map_err(map_err)?,
    };
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut entity_ids = Vec::with_capacity(rows.len());
    let mut owner_ids = Vec::with_capacity(rows.len());
    for &(entity_id, _, _, owner_id, _, _, _) in &rows {
        entity_ids.push(entity_id);
        owner_ids.push(owner_id);
    }
    let head_entities: HashSet<Uuid> = sqlx::query_scalar(HEADS_IS_HEAD_SQL)
        .bind(&entity_ids)
        .bind(&owner_ids)
        .fetch_all(&mut *tx)
        .await
        .map_err(map_err)?
        .into_iter()
        .collect();
    Ok(rows
        .into_iter()
        .map(
            |(entity, model, dim, owner, version, generation, row_xmin)| {
                let is_head = head_entities.contains(&entity);
                (
                    (entity, model, dim),
                    StoredHead {
                        head: (OwnerId::new(owner), version, generation, is_head),
                        row_xmin,
                    },
                )
            },
        )
        .collect())
}

/// The keys where the index and the heads disagree, by what each side held.
fn disagreements(
    entries: &[(Key, VectorEntry)],
    heads: &BTreeMap<Key, StoredHead>,
) -> BTreeMap<Key, (Option<Head>, Option<StoredHead>)> {
    let mut seen: BTreeMap<Key, (Option<Head>, Option<StoredHead>)> = BTreeMap::new();
    for (key, entry) in entries {
        seen.entry(key.clone()).or_default().0 =
            Some((entry.owner, entry.version, entry.generation, entry.is_head));
    }
    for (key, stored) in heads {
        seen.entry(key.clone()).or_default().1 = Some(stored.clone());
    }
    seen.retain(|_, (entry, stored)| *entry != stored.as_ref().map(|h| h.head));
    seen
}

/// Keys repaired per transaction, each holding its head lock until commit.
const REPAIRS_PER_TRANSACTION: usize =
    proxiima_core::ledger!(Proxiima.Embeddings.sweepRepairsPerTransaction);

/// What the index and the heads held for one key.
type Suspect = (Key, (Option<Head>, Option<StoredHead>));

/// Read the current stored head and `xmin` generation for `key` if no pending
/// debt row owns it; returns `Ok(None)` when debt is queued.
async fn read_live_head_if_clear(
    tx: &mut PgConnection,
    key: &Key,
) -> Result<Option<Option<StoredHead>>, StorageError> {
    let debt_pending: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM proxiima_core.vector_index_debt
              WHERE entity_id = $1 AND model_id = $2 AND dim = $3
         )",
    )
    .bind(key.0)
    .bind(&key.1)
    .bind(key.2)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_err)?;
    if debt_pending {
        return Ok(None);
    }
    let now: Option<(Uuid, i32, i64, bool, String)> = sqlx::query_as(
        "SELECT eh.owner_id,
                eh.embedding_version,
                eh.vector_generation,
                (EXISTS (SELECT 1 FROM proxiima_core.goal g WHERE g.t = eh.entity_id)
                 OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                             WHERE mh.owner_id = eh.owner_id AND mh.t = eh.entity_id)) AS is_head,
                eh.xmin::text
           FROM proxiima_core.embedding_heads eh
          WHERE eh.entity_id = $1 AND eh.model_id = $2 AND eh.dim = $3",
    )
    .bind(key.0)
    .bind(&key.1)
    .bind(key.2)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_err)?;
    Ok(Some(now.map(
        |(owner, version, generation, is_head, row_xmin)| StoredHead {
            head: (OwnerId::new(owner), version, generation, is_head),
            row_xmin,
        },
    )))
}

/// Non-blocking level-2 series-handle lock matching `forget::lock_memory_handles_tx`.
async fn try_lock_memory_handle(tx: &mut PgConnection, handle: Uuid) -> Result<bool, StorageError> {
    sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended('proxiima-memory-handle:' || $1::text, 0))",
    )
    .bind(handle)
    .fetch_one(tx)
    .await
    .map_err(map_err)
}

/// Non-blocking level-3 lifecycle target lock matching `proxiima_core.lock_pin_targets`.
async fn try_lock_lifecycle_target(
    tx: &mut PgConnection,
    entity_id: Uuid,
) -> Result<bool, StorageError> {
    sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended('proxiima-forget:' || $1::text, 0))",
    )
    .bind(entity_id)
    .fetch_one(tx)
    .await
    .map_err(map_err)
}

/// Acquire every non-blocking lock a sweep repair needs for one suspect in
/// canonical order: level 1 (`owner` shared), level 2 (series `handle`),
/// level 3 (lifecycle `t`), and level 5 (embedding key).
async fn try_lock_repair_suspect(
    tx: &mut PgConnection,
    key: &Key,
    entry: Option<&Head>,
    read: Option<&StoredHead>,
) -> Result<bool, StorageError> {
    let mut owners = Vec::with_capacity(2);
    if let Some((entry_owner, _, _, _)) = entry {
        owners.push(*entry_owner);
    }
    if let Some(stored) = read {
        owners.push(stored.head.0);
    }
    if !try_lock_owners_shared(&mut *tx, &owners).await? {
        return Ok(false);
    }
    if read.is_some() {
        let handle: Option<Uuid> = sqlx::query_scalar(
            "SELECT handle FROM proxiima_core.memory_including_forgotten WHERE t = $1",
        )
        .bind(key.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_err)?;
        if let Some(handle) = handle
            && !try_lock_memory_handle(&mut *tx, handle).await?
        {
            return Ok(false);
        }
        if !try_lock_lifecycle_target(&mut *tx, key.0).await? {
            return Ok(false);
        }
    }
    try_lock_embedding_key(&mut *tx, key.0, &key.1, key.2).await
}

/// Repair `suspects` under each key's non-blocking locks (`owner` -> series
/// `handle` -> lifecycle `t` -> embedding key), acting only where the head row
/// still has the exact metadata and `xmin` generation the page read and no
/// queued debt already owns the key. Acquiring the series handle and lifecycle
/// locks non-blockingly before the embedding key matches `forget`'s lock order
/// so deleting a lost head and requeueing its `embedding_jobs` row never
/// deadlocks with a concurrent `forget` (which deletes `embedding_jobs` before
/// `embedding_heads`). Holding the embedding key lock across the repair RPCs
/// serializes repair against writers, erases, and `drain_debt`.
async fn repair(
    tx: &mut PgConnection,
    index: &dyn VectorIndex,
    suspects: &[Suspect],
    swept: &mut VectorIndexSweepOutcome,
) -> Result<bool, StorageError> {
    let mut stale = Vec::new();
    let mut moves: BTreeMap<OwnerId, Vec<VectorMetadataTarget>> = BTreeMap::new();
    let mut heads_true = Vec::new();
    let mut heads_false = Vec::new();
    let mut metadata_repaired = 0_u64;
    let mut lost: Vec<(Key, i32, String)> = Vec::new();
    for (key, (entry, read)) in suspects.iter().cloned() {
        if !try_lock_repair_suspect(&mut *tx, &key, entry.as_ref(), read.as_ref()).await? {
            continue;
        }
        let Some(now) = read_live_head_if_clear(&mut *tx, &key).await? else {
            continue;
        };
        if now != read {
            continue;
        }
        match (entry, read) {
            (Some(_), None) => stale.push(vector_key(&key)?),
            (
                Some((held_owner, held_version, held_gen, held_is_head)),
                Some(StoredHead {
                    head: (owner, version, generation, is_head),
                    ..
                }),
            ) if held_version == version && held_gen == generation && generation > 0 => {
                let target = VectorMetadataTarget {
                    key: vector_key(&key)?,
                    generation,
                };
                if held_owner != owner {
                    moves.entry(owner).or_default().push(target.clone());
                }
                if held_is_head != is_head {
                    if is_head {
                        heads_true.push(target);
                    } else {
                        heads_false.push(target);
                    }
                }
                metadata_repaired += 1;
            }
            (
                _,
                Some(StoredHead {
                    head: (_, version, _, _),
                    row_xmin,
                }),
            ) => lost.push((key, version, row_xmin)),
            (None, None) => {}
        }
    }
    if !stale.is_empty() || !moves.is_empty() || !heads_true.is_empty() || !heads_false.is_empty() {
        let fence_and_rev = next_vector_generation(&mut *tx).await?;
        if !stale.is_empty() {
            delete_keys(index, &stale, fence_and_rev).await?;
        }
        move_keys(index, moves, fence_and_rev).await?;
        set_head_keys(index, &heads_true, true, fence_and_rev).await?;
        set_head_keys(index, &heads_false, false, fence_and_rev).await?;
    }
    swept.entries_deleted += count_u64(stale.len());
    swept.metadata_repaired += metadata_repaired;
    if !lost.is_empty() {
        let (deleted, requeued) = delete_lost_heads(&mut *tx, lost).await?;
        swept.heads_deleted += deleted;
        return Ok(requeued > 0);
    }
    Ok(false)
}

type LostHeadCandidate = (Uuid, String, i16, Uuid, i32, String, bool);

async fn fetch_lost_head_candidates(
    tx: &mut PgConnection,
    lost: &[(Key, i32, String)],
) -> Result<Vec<LostHeadCandidate>, StorageError> {
    let mut columns = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ((entity, model, dim), version, row_xmin) in lost {
        columns.0.push(*entity);
        columns.1.push(model.as_str());
        columns.2.push(*dim);
        columns.3.push(*version);
        columns.4.push(row_xmin.as_str());
    }
    sqlx::query_as(
        "SELECT h.entity_id,
                h.model_id,
                h.dim,
                h.owner_id,
                lost.version,
                lost.row_xmin,
                (m.t IS NOT NULL) AS is_live
           FROM unnest($1::uuid[], $2::text[], $3::smallint[], $4::int[], $5::text[])
                AS lost(entity_id, model_id, dim, version, row_xmin)
           JOIN proxiima_core.embedding_heads h
             ON h.entity_id = lost.entity_id
            AND h.model_id = lost.model_id
            AND h.dim = lost.dim
            AND h.embedding_version = lost.version
            AND h.xmin::text = lost.row_xmin
           LEFT JOIN proxiima_core.live_memory m
             ON m.t = h.entity_id
            AND m.owner_id = h.owner_id",
    )
    .bind(&columns.0)
    .bind(&columns.1)
    .bind(&columns.2)
    .bind(&columns.3)
    .bind(&columns.4)
    .fetch_all(tx)
    .await
    .map_err(map_err)
}

/// Acquire non-blocking `FOR SHARE` row locks on live `proxiima_core.memory`
/// rows so sweep defers any memory currently locked `FOR UPDATE` by concurrent
/// `reconcile_embeddings` (including `IncludeStale`) before inspecting or
/// inserting `embedding_jobs`.
async fn lock_unlocked_live_memories(
    tx: &mut PgConnection,
    live_entities: &[Uuid],
) -> Result<HashSet<Uuid>, StorageError> {
    if live_entities.is_empty() {
        return Ok(HashSet::new());
    }
    let locked: Vec<Uuid> = sqlx::query_as::<_, (Uuid,)>(
        "SELECT m.t
           FROM proxiima_core.memory m
          WHERE m.t = ANY($1::uuid[])
            AND m.forgotten_at IS NULL
          ORDER BY m.t
            FOR SHARE OF m SKIP LOCKED",
    )
    .bind(live_entities)
    .fetch_all(tx)
    .await
    .map_err(map_err)?
    .into_iter()
    .map(|(t,)| t)
    .collect();
    Ok(locked.into_iter().collect())
}

/// Inspect existing `embedding_jobs` for `live_keys` (after `FOR SHARE OF m`
/// serialized against `reconcile_embeddings`) and lock every visible job row
/// with non-blocking `FOR UPDATE OF j SKIP LOCKED`.
async fn lock_existing_jobs_for_live_keys(
    tx: &mut PgConnection,
    live_keys: &[(Uuid, String, i16, Uuid)],
) -> Result<(HashMap<Key, Uuid>, HashMap<Uuid, String>), StorageError> {
    if live_keys.is_empty() {
        return Ok((HashMap::new(), HashMap::new()));
    }
    let (mut e, mut m, mut d, mut o) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (entity, model, dim, owner) in live_keys {
        e.push(*entity);
        m.push(model.as_str());
        d.push(*dim);
        o.push(*owner);
    }
    let visible: Vec<(Uuid, String, i16, Uuid)> = sqlx::query_as(
        "SELECT j.entity_id, j.model_id, j.dim, j.job_id
           FROM proxiima_core.embedding_jobs j
           JOIN unnest($1::uuid[], $2::text[], $3::smallint[], $4::uuid[])
                AS k(entity_id, model_id, dim, owner_id)
             ON j.owner_id = k.owner_id
            AND j.entity_id = k.entity_id
            AND j.model_id = k.model_id
            AND j.dim = k.dim",
    )
    .bind(&e)
    .bind(&m)
    .bind(&d)
    .bind(&o)
    .fetch_all(&mut *tx)
    .await
    .map_err(map_err)?;
    let mut visible_by_key = HashMap::with_capacity(visible.len());
    let mut job_ids = Vec::with_capacity(visible.len());
    for (entity, model, dim, job_id) in visible {
        visible_by_key.insert((entity, model, dim), job_id);
        job_ids.push(job_id);
    }
    let mut locked_jobs = HashMap::new();
    if !job_ids.is_empty() {
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT j.job_id, j.status::text
               FROM proxiima_core.embedding_jobs j
              WHERE j.job_id = ANY($1::uuid[])
              ORDER BY j.job_id
                FOR UPDATE OF j SKIP LOCKED",
        )
        .bind(&job_ids)
        .fetch_all(&mut *tx)
        .await
        .map_err(map_err)?;
        for (job_id, status) in rows {
            locked_jobs.insert(job_id, status);
        }
    }
    Ok((visible_by_key, locked_jobs))
}

/// Remove heads whose version and tuple generation are still missing from the
/// host index without blocking on concurrent `reconcile_embeddings` inserts or
/// `renew_embedding_jobs` heartbeats:
/// 1. Lock each live memory row via `FOR SHARE OF m SKIP LOCKED` (deferring
///    any memory currently held `FOR UPDATE` by `reconcile_embeddings`,
///    including `IncludeStale`, before deleting its head or inserting a job).
/// 2. After acquiring `FOR SHARE OF m`, inspect existing `embedding_jobs` and
///    lock visible rows via `FOR UPDATE OF j SKIP LOCKED` (deferring any key
///    currently held by an uncommitted heartbeat `UPDATE`).
/// 3. Delete safe lost heads, reopen unlocked `'failed'` jobs, skip inserts
///    for unlocked active jobs, and insert missing jobs for live memories.
async fn delete_lost_heads(
    tx: &mut PgConnection,
    lost: Vec<(Key, i32, String)>,
) -> Result<(u64, u64), StorageError> {
    let candidates = fetch_lost_head_candidates(&mut *tx, &lost).await?;
    if candidates.is_empty() {
        return Ok((0, 0));
    }
    let live_entities: Vec<Uuid> = candidates
        .iter()
        .filter_map(|(entity, _, _, _, _, _, is_live)| (*is_live).then_some(*entity))
        .collect();
    let unlocked_live = lock_unlocked_live_memories(&mut *tx, &live_entities).await?;
    let live_keys: Vec<(Uuid, String, i16, Uuid)> = candidates
        .iter()
        .filter(|(entity, _, _, _, _, _, is_live)| *is_live && unlocked_live.contains(entity))
        .map(|(entity, model, dim, owner, _, _, _)| (*entity, model.clone(), *dim, *owner))
        .collect();
    let (visible_jobs, locked_jobs) =
        lock_existing_jobs_for_live_keys(&mut *tx, &live_keys).await?;

    let mut del_cols = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut live_plan: HashMap<Key, Option<(Uuid, bool)>> = HashMap::new();
    for (entity, model, dim, _owner, version, row_xmin, is_live) in candidates {
        if is_live {
            if !unlocked_live.contains(&entity) {
                continue;
            }
            let key = (entity, model.clone(), dim);
            match visible_jobs.get(&key) {
                Some(&job_id) => {
                    let Some(status) = locked_jobs.get(&job_id) else {
                        continue;
                    };
                    live_plan.insert(key, Some((job_id, status == "failed")));
                }
                None => {
                    live_plan.insert(key, None);
                }
            }
        }
        del_cols.0.push(entity);
        del_cols.1.push(model);
        del_cols.2.push(dim);
        del_cols.3.push(version);
        del_cols.4.push(row_xmin);
    }
    if del_cols.0.is_empty() {
        return Ok((0, 0));
    }

    let deleted_rows: Vec<(Uuid, String, i16, Uuid)> = sqlx::query_as(
        "DELETE FROM proxiima_core.embedding_heads head
          WHERE (head.entity_id, head.model_id, head.dim) IN (
                SELECT h.entity_id, h.model_id, h.dim
                  FROM proxiima_core.embedding_heads h
                  JOIN unnest($1::uuid[], $2::text[], $3::smallint[], $4::int[], $5::text[])
                       AS lost(entity_id, model_id, dim, version, row_xmin)
                    ON h.entity_id = lost.entity_id
                   AND h.model_id = lost.model_id
                   AND h.dim = lost.dim
                   AND h.embedding_version = lost.version
                   AND h.xmin::text = lost.row_xmin
                   FOR UPDATE OF h SKIP LOCKED)
         RETURNING head.entity_id, head.model_id, head.dim, head.owner_id",
    )
    .bind(&del_cols.0)
    .bind(&del_cols.1)
    .bind(&del_cols.2)
    .bind(&del_cols.3)
    .bind(&del_cols.4)
    .fetch_all(&mut *tx)
    .await
    .map_err(map_err)?;

    let deleted = count_u64(deleted_rows.len());
    let requeued = requeue_deleted_lost_heads(&mut *tx, deleted_rows, &live_plan).await?;
    Ok((deleted, requeued))
}

async fn requeue_deleted_lost_heads(
    tx: &mut PgConnection,
    deleted_rows: Vec<(Uuid, String, i16, Uuid)>,
    live_plan: &HashMap<Key, Option<(Uuid, bool)>>,
) -> Result<u64, StorageError> {
    let (mut ins_e, mut ins_m, mut ins_d, mut ins_o) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut reopen_ids = Vec::new();
    for (entity, model, dim, owner) in deleted_rows {
        match live_plan.get(&(entity, model.clone(), dim)) {
            Some(None) => {
                ins_e.push(entity);
                ins_m.push(model);
                ins_d.push(dim);
                ins_o.push(owner);
            }
            Some(Some((job_id, true))) => reopen_ids.push(*job_id),
            Some(Some((_, false))) | None => {}
        }
    }
    let mut requeued = 0_u64;
    if !ins_e.is_empty() {
        let res = sqlx::query(
            "INSERT INTO proxiima_core.embedding_jobs (entity_id, model_id, dim, owner_id)
             SELECT entity_id, model_id, dim, owner_id
               FROM unnest($1::uuid[], $2::text[], $3::smallint[], $4::uuid[])
                    AS rows(entity_id, model_id, dim, owner_id)
             ON CONFLICT (owner_id, entity_id, model_id, dim) DO NOTHING",
        )
        .bind(&ins_e)
        .bind(&ins_m)
        .bind(&ins_d)
        .bind(&ins_o)
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;
        requeued += res.rows_affected();
    }
    if !reopen_ids.is_empty() {
        let res = sqlx::query(
            "UPDATE proxiima_core.embedding_jobs
                SET status = 'pending',
                    claimed_at = NULL,
                    claim_token = NULL,
                    last_error = NULL,
                    retry_position = NULL
              WHERE job_id = ANY($1::uuid[])
                AND status = 'failed'",
        )
        .bind(&reopen_ids)
        .execute(&mut *tx)
        .await
        .map_err(map_err)?;
        requeued += res.rows_affected();
    }
    Ok(requeued)
}

/// `Proxiima.Embeddings.indexRepair`. Read heads before index entries in key order.
/// Delete entries with no head, repair stale owner or series-head metadata, and
/// remove heads whose version has no entry so reconciliation can rebuild them.
pub(crate) async fn sweep_index(
    pool: &sqlx::PgPool,
    platform: &crate::PgPlatformScope,
    index: &dyn VectorIndex,
) -> Result<VectorIndexSweepOutcome, StorageError> {
    sweep_index_bounded(pool, platform, index, None, None).await
}

/// `Proxiima.Embeddings.indexRepair`, bounded to at most `max_pages` pages
/// starting strictly after `after`. Pass `max_pages = None` for a full sweep.
///
/// `max_pages == Some(0)` performs no work and echoes `after` as `next`, so
/// `(None, Some(0))` returns `next: None` without indicating exhaustion; callers
/// advancing a paginated sweep should pass `max_pages >= 1`.
pub(crate) async fn sweep_index_bounded(
    pool: &sqlx::PgPool,
    platform: &crate::PgPlatformScope,
    index: &dyn VectorIndex,
    after: Option<&VectorKey>,
    max_pages: Option<u32>,
) -> Result<VectorIndexSweepOutcome, StorageError> {
    let mut swept = VectorIndexSweepOutcome::default();
    if max_pages == Some(0) {
        swept.next = after.cloned();
        return Ok(swept);
    }
    let mut cursor: Option<Key> = after.map(key_of).transpose()?;
    let page = usize::try_from(SWEEP_PAGE).unwrap_or(usize::MAX);
    let mut pages_done: u32 = 0;
    loop {
        let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
        let heads = heads_after(tx.as_mut(), cursor.as_ref()).await?;
        tx.commit().await.map_err(map_err)?;
        let after_key = cursor.as_ref().map(vector_key).transpose()?;
        let entry_page = index.entries(after_key.as_ref(), SWEEP_PAGE).await?;
        if entry_page.entries.is_empty() && entry_page.has_more {
            return Err(StorageError::Unavailable(
                "vector index entries claimed more pages without returning an entry".into(),
            ));
        }
        let entries_has_more = entry_page.has_more;
        let entries = entry_page
            .entries
            .into_iter()
            .map(|entry| Ok((key_of(&entry.key)?, entry)))
            .collect::<Result<Vec<(Key, VectorEntry)>, StorageError>>()?;
        // Both sides are complete up to the smaller last key of any unexhausted page.
        let cutoff = [
            entries_has_more
                .then(|| entries.last().map(|(key, _)| key.clone()))
                .flatten(),
            (heads.len() >= page)
                .then(|| heads.last().map(|(key, _)| key.clone()))
                .flatten(),
        ]
        .into_iter()
        .flatten()
        .min();
        let within = |key: &Key| cutoff.as_ref().is_none_or(|cutoff| key <= cutoff);
        let entries: Vec<(Key, VectorEntry)> =
            entries.into_iter().filter(|(key, _)| within(key)).collect();
        let heads: BTreeMap<Key, StoredHead> =
            heads.into_iter().filter(|(key, _)| within(key)).collect();
        let suspects: Vec<Suspect> = disagreements(&entries, &heads).into_iter().collect();
        for suspects in suspects.chunks(REPAIRS_PER_TRANSACTION) {
            let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
            let requeued = repair(tx.as_mut(), index, suspects, &mut swept).await;
            crate::write_wake::finish_enqueuing(tx, requeued, |requeued| *requeued).await?;
        }
        pages_done = pages_done.saturating_add(1);
        if let Some(cutoff) = cutoff {
            if max_pages.is_some_and(|limit| pages_done >= limit) {
                swept.next = Some(vector_key(&cutoff)?);
                return Ok(swept);
            }
            cursor = Some(cutoff);
        } else {
            swept.next = None;
            return Ok(swept);
        }
    }
}

#[cfg(test)]
mod tests;
