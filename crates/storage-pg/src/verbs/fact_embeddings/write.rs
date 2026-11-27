use std::collections::{BTreeMap, BTreeSet, HashMap};

use proxiima_core::storage_ports::{EmbeddingWriteProof, VectorPut};
use proxiima_core::{
    EmbeddableEntityRef, EmbeddingSpace, EmbeddingWriteOutcome, OwnerId, SpaceVector, StorageError,
};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::error::map_err;

/// Lifecycle fences held by an embedding write transaction: shared owner fences
/// (level 1), memory series handle locks (level 2), and lifecycle target locks
/// (level 3), all acquired before any job claim, head row, or host index RPC.
pub(crate) struct LockedEmbeddingFences {
    handles: BTreeSet<Uuid>,
    targets: BTreeSet<Uuid>,
}

/// Acquire the store's global lifecycle fences for `targets` in canonical order
/// (shared owner fences -> memory series handles -> owned lifecycle targets).
///
/// Holding these fences through the embedding transaction linearizes both
/// claimed and claimless publication against supersession, forget, restore,
/// transfer, and every erase scope without taking `memory_head` row locks after
/// `vector_index_debt` or `embedding_heads`. Probing only rows owned by each
/// write's `owner` guarantees `lock_pin_targets` never acquires a new owner
/// fence after handle locks.
pub(crate) async fn lock_embedding_write_fences(
    tx: &mut Transaction<'_, Postgres>,
    targets: &[(OwnerId, EmbeddableEntityRef)],
) -> Result<LockedEmbeddingFences, StorageError> {
    if targets.is_empty() {
        return Ok(LockedEmbeddingFences {
            handles: BTreeSet::new(),
            targets: BTreeSet::new(),
        });
    }
    let owners: Vec<OwnerId> = targets.iter().map(|(owner, _)| *owner).collect();
    crate::access::fences::lock_owner_fences_shared_tx(tx, &owners).await?;

    let entity_ids: Vec<Uuid> = targets
        .iter()
        .map(|(_, entity)| entity.entity_id())
        .collect();
    let owner_ids: Vec<Uuid> = owners.iter().map(|owner| owner.into_inner()).collect();

    let memory_rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT DISTINCT m.t, m.handle
           FROM unnest($1::uuid[], $2::uuid[]) AS w(entity_id, owner_id)
           JOIN proxiima_core.memory_including_forgotten m
             ON m.t = w.entity_id AND m.owner_id = w.owner_id
          ORDER BY m.handle, m.t",
    )
    .bind(&entity_ids)
    .bind(&owner_ids)
    .fetch_all(tx.as_mut())
    .await
    .map_err(map_err)?;

    let mut handles = BTreeSet::new();
    let mut locked_targets = BTreeSet::new();
    for (t, handle) in memory_rows {
        handles.insert(handle);
        locked_targets.insert(t);
    }
    if !handles.is_empty() {
        let handle_vec: Vec<Uuid> = handles.iter().copied().collect();
        crate::verbs::forget::lock_memory_handles_tx(tx, &handle_vec).await?;
    }

    let goal_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT g.t
           FROM unnest($1::uuid[], $2::uuid[]) AS w(entity_id, owner_id)
           JOIN proxiima_core.goal g
             ON g.t = w.entity_id AND g.owner_id = w.owner_id
          ORDER BY g.t",
    )
    .bind(&entity_ids)
    .bind(&owner_ids)
    .fetch_all(tx.as_mut())
    .await
    .map_err(map_err)?;
    locked_targets.extend(goal_ids);

    if !locked_targets.is_empty() {
        let target_vec: Vec<Uuid> = locked_targets.iter().copied().collect();
        crate::verbs::forget::lock_lifecycle_targets_tx(tx, &target_vec).await?;
    }

    Ok(LockedEmbeddingFences {
        handles,
        targets: locked_targets,
    })
}

/// Verify that the queue claim authorizing a remote embedding write is still
/// held before committing write-ahead intent or calling `VectorIndex::put_batch`.
pub(crate) async fn verify_embedding_job_claim(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    entity: EmbeddableEntityRef,
    space: &EmbeddingSpace,
    proof: EmbeddingWriteProof,
) -> Result<(), StorageError> {
    let Some((job_id, claim_token)) = proof.claim_fence() else {
        return Ok(());
    };
    let valid = super::jobs::bind_claim_fence(
        sqlx::query(concat!(
            "SELECT 1 FROM proxiima_core.embedding_jobs WHERE ",
            super::jobs::claim_fence!()
        )),
        super::jobs::ClaimFence {
            job_id,
            claim_token,
            owner,
            entity_id: entity.entity_id(),
            space,
        },
    )
    .fetch_optional(tx.as_mut())
    .await
    .map_err(map_err)?;
    if valid.is_none() {
        return Err(StorageError::Conflict(
            "embedding job claim is stale or does not match the write".into(),
        ));
    }
    Ok(())
}

/// Complete the queue claims that authorized `writes`, in the writes'
/// transaction, in canonical `(entity_id, model_id, dim)` byte order: the
/// order a purge locks job rows in, so a batch and a purge never wait on each
/// other's job rows in a cycle. `spaces[i]` is the space of `writes[i]`.
pub(crate) async fn complete_embedding_job_claims(
    tx: &mut Transaction<'_, Postgres>,
    writes: &[proxiima_core::storage_ports::EmbeddingWrite<'_>],
    spaces: &[&EmbeddingSpace],
) -> Result<(), StorageError> {
    let mut claims: Vec<_> = writes.iter().zip(spaces.iter().copied()).collect();
    claims.sort_by(|(a, a_space), (b, b_space)| {
        (
            a.entity.entity_id(),
            a_space.model_id(),
            crate::pgvector::Lane::of(a_space.dim()).width,
        )
            .cmp(&(
                b.entity.entity_id(),
                b_space.model_id(),
                crate::pgvector::Lane::of(b_space.dim()).width,
            ))
    });
    for (write, space) in claims {
        complete_embedding_job_claim(tx, write.owner, write.entity, space, write.proof).await?;
    }
    Ok(())
}

/// Complete the queue claim that authorized an embedding write, in the
/// write's transaction. A write without a claim completes nothing.
/// `Proxiima.Embeddings.settleClaim`.
///
/// The delete holds the job's row lock through the embedding transaction. A
/// stale worker either loses before writing, or commits before reclamation
/// can mint a successor; in the latter case the successor necessarily writes
/// the later head.
async fn complete_embedding_job_claim(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    entity: EmbeddableEntityRef,
    space: &EmbeddingSpace,
    proof: EmbeddingWriteProof,
) -> Result<(), StorageError> {
    let Some((job_id, claim_token)) = proof.claim_fence() else {
        return Ok(());
    };
    let completed = super::jobs::bind_claim_fence(
        sqlx::query(concat!(
            "DELETE FROM proxiima_core.embedding_jobs WHERE ",
            super::jobs::claim_fence!()
        )),
        super::jobs::ClaimFence {
            job_id,
            claim_token,
            owner,
            entity_id: entity.entity_id(),
            space,
        },
    )
    .execute(tx.as_mut())
    .await
    .map_err(map_err)?;
    if completed.rows_affected() == 0 {
        return Err(StorageError::Conflict(
            "embedding job claim is stale or does not match the write".into(),
        ));
    }
    Ok(())
}

/// Append one embedding version and advance the independent latest head.
/// Chunks keep text order, and the head advances only after every chunk is stored.
///
/// Crate-private: this is a raw-owner write below the proof gate. External
/// writers go through `EmbeddingWritePort`, which requires an
/// `EmbeddingWriteProof` only `proxiima-core` can construct.
///
/// Returns version `0` when the entity is not eligible for embedding: a
/// deleted or textless entity is a best-effort no-op, not an error.
///
/// # Errors
///
/// Maps SQL failures through the shared mapper.
pub(crate) async fn insert_embedding(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    entity: EmbeddableEntityRef,
    vectors: &[SpaceVector],
) -> Result<EmbeddingWriteOutcome, StorageError> {
    let owner_id = owner.into_inner();
    let space = validate_embedding_vectors(vectors)?;
    let dim = crate::pgvector::Lane::of(space.dim()).width;
    let model_id = space.model_id();

    let entity_id = entity.entity_id();
    lock_embedding_key(tx.as_mut(), entity_id, model_id, dim).await?;

    if !embedding_entity_is_eligible(tx, owner, entity).await? {
        return Ok(EmbeddingWriteOutcome {
            embedding_version: 0,
        });
    }

    let embedding_version: i32 = sqlx::query_scalar(
        "SELECT COALESCE(max(embedding_version), 0) + 1
           FROM proxiima_core.embeddings
          WHERE entity_id = $1
            AND model_id = $2
            AND dim = $3",
    )
    .bind(entity_id)
    .bind(model_id)
    .bind(dim)
    .fetch_one(tx.as_mut())
    .await
    .map_err(map_err)?;

    for (ordinal, vector) in vectors.iter().enumerate() {
        let chunk_ordinal = i32::try_from(ordinal).map_err(|_| {
            StorageError::ConstraintViolation("embedding has too many chunks".into())
        })?;
        sqlx::query(
            "INSERT INTO proxiima_core.embeddings
                (entity_id, model_id, dim, embedding_version, chunk_ordinal, vec, owner_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(entity_id)
        .bind(model_id)
        .bind(dim)
        .bind(embedding_version)
        .bind(chunk_ordinal)
        .bind(crate::pgvector::vector(vector.values()))
        .bind(owner_id)
        .execute(tx.as_mut())
        .await
        .map_err(map_err)?;
    }

    sqlx::query(
        "INSERT INTO proxiima_core.embedding_heads
            (entity_id, model_id, dim, embedding_version, owner_id)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (entity_id, model_id, dim)
         DO UPDATE SET embedding_version = EXCLUDED.embedding_version",
    )
    .bind(entity_id)
    .bind(model_id)
    .bind(dim)
    .bind(embedding_version)
    .bind(owner_id)
    .execute(tx.as_mut())
    .await
    .map_err(map_err)?;

    Ok(EmbeddingWriteOutcome { embedding_version })
}

fn embedding_lock_key(entity_id: uuid::Uuid, model_id: &str, dim: i16) -> String {
    format!("proxiima-embedding:{entity_id}:{model_id}:{dim}")
}

/// Hold one entity's head in one space until the transaction ends: every
/// writer or debt settler of a head takes it in canonical key order.
pub(crate) async fn lock_embedding_key(
    tx: &mut sqlx::PgConnection,
    entity_id: uuid::Uuid,
    model_id: &str,
    dim: i16,
) -> Result<(), StorageError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(embedding_lock_key(entity_id, model_id, dim))
        .execute(tx)
        .await
        .map(drop)
        .map_err(map_err)
}

/// Acquire [`lock_embedding_key`] for every key in `keys`, sorted and
/// deduplicated in canonical `(entity_id, model_id, dim)` byte order, in one
/// round-trip.
pub(crate) async fn lock_embedding_keys_batch(
    tx: &mut sqlx::PgConnection,
    keys: &[(Uuid, String, i16)],
) -> Result<(), StorageError> {
    if keys.is_empty() {
        return Ok(());
    }
    let mut sorted: Vec<(Uuid, &str, i16)> = keys
        .iter()
        .map(|(entity_id, model_id, dim)| (*entity_id, model_id.as_str(), *dim))
        .collect();
    sorted.sort_unstable();
    sorted.dedup();
    let lock_names: Vec<String> = sorted
        .into_iter()
        .map(|(entity_id, model_id, dim)| embedding_lock_key(entity_id, model_id, dim))
        .collect();
    // `unnest` yields array order with no sort node above it, and
    // `pg_advisory_xact_lock` is VOLATILE / PARALLEL RESTRICTED, so Postgres
    // acquires each key in the exact sorted order built above.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(lock_name, 0)) FROM unnest($1::text[]) AS lock_name")
        .bind(&lock_names)
        .execute(tx)
        .await
        .map(drop)
        .map_err(map_err)
}

/// [`lock_embedding_key`] without waiting: `false` while a writer holds it.
pub(crate) async fn try_lock_embedding_key(
    tx: &mut sqlx::PgConnection,
    entity_id: uuid::Uuid,
    model_id: &str,
    dim: i16,
) -> Result<bool, StorageError> {
    sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(embedding_lock_key(entity_id, model_id, dim))
        .fetch_one(tx)
        .await
        .map_err(map_err)
}

/// One entity's head write to stage for a host [`VectorIndex`].
#[derive(Clone, Copy)]
pub(crate) struct EmbeddingHeadWrite<'a> {
    pub(crate) owner: &'a OwnerId,
    pub(crate) entity: EmbeddableEntityRef,
    pub(crate) vectors: &'a [SpaceVector],
}

/// Allocate strictly monotonic, never-reused generations for eligible remote
/// `writes`, synchronously validate the batch against provider limits via
/// `index.validate_put_batch(&puts)` BEFORE staging any debt, snapshot each
/// key's prior debt row, and record durable `'delete'` write-ahead intent
/// stamped with `clock_timestamp()` (phase 1 of remote publication).
///
/// The caller holds `fences` (levels 1–3) and canonical embedding key locks
/// (level 5) for `writes`, then commits this short transaction and releases its
/// database connection BEFORE calling `VectorIndex::put_batch`.
pub(crate) async fn prepare_remote_publish_intent<'a>(
    tx: &mut Transaction<'_, Postgres>,
    fences: &LockedEmbeddingFences,
    writes: &[EmbeddingHeadWrite<'a>],
    index: &dyn proxiima_core::storage_ports::VectorIndex,
) -> Result<
    (
        Vec<VectorPut<'a>>,
        Vec<crate::vector_index::StagedPublishIntent>,
    ),
    StorageError,
> {
    if writes.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut staged_by_key: BTreeMap<(Uuid, &str, i16), VectorPut<'a>> = BTreeMap::new();
    for write in writes {
        let space = validate_embedding_vectors(write.vectors)?;
        let model_id = space.model_id();
        let dim = crate::pgvector::Lane::of(space.dim()).width;
        let entity_id = write.entity.entity_id();
        let Some((is_head, handle)) =
            embedding_entity_head_status(tx, write.owner, write.entity).await?
        else {
            continue;
        };
        let covered = match handle {
            Some(handle) => fences.handles.contains(&handle) && fences.targets.contains(&entity_id),
            None => fences.targets.contains(&entity_id),
        };
        if !covered {
            return Err(StorageError::Retryable(
                "embedding write target appeared after lifecycle fences".into(),
            ));
        }
        let generation: i64 =
            sqlx::query_scalar("SELECT nextval('proxiima_core.vector_index_generation_seq')")
                .fetch_one(tx.as_mut())
                .await
                .map_err(map_err)?;
        let version: i32 = sqlx::query_scalar(
            "SELECT COALESCE(
                (SELECT embedding_version
                   FROM proxiima_core.embedding_heads
                  WHERE entity_id = $1 AND model_id = $2 AND dim = $3),
                0
             ) + 1",
        )
        .bind(entity_id)
        .bind(model_id)
        .bind(dim)
        .fetch_one(tx.as_mut())
        .await
        .map_err(map_err)?;
        staged_by_key.insert(
            (entity_id, model_id, dim),
            VectorPut {
                entity: write.entity,
                owner: *write.owner,
                version,
                generation,
                is_head,
                vectors: write.vectors,
            },
        );
    }
    let puts: Vec<VectorPut<'a>> = staged_by_key.into_values().collect();
    if puts.is_empty() {
        return Ok((puts, Vec::new()));
    }
    // Validate provider-level entry invariants and transport limits BEFORE
    // reading prior debt or writing `vector_index_debt`, so a rejected batch
    // rolls back cleanly and validation time never consumes the publish lease.
    index.validate_put_batch(&puts)?;
    let staged = stage_publish_intents_with_prior_debt(tx.as_mut(), &puts).await?;
    Ok((puts, staged))
}

type LockedPriorDebtRow = (Uuid, String, i16, String, i64, i64, String);

async fn lock_and_resolve_prior_debt(
    tx: &mut sqlx::PgConnection,
    entities: &[Uuid],
    models: &[&str],
    dims: &[i16],
) -> Result<HashMap<(Uuid, String, i16), crate::vector_index::PriorDebt>, StorageError> {
    // Lock existing debt rows with `FOR NO KEY UPDATE OF d` in canonical key
    // order so the snapshot atomically reflects the exact row state `INSERT ...
    // ON CONFLICT DO UPDATE` replaces (never capturing a prior publisher's
    // intent right before that publisher settles it as unsent).
    let prior_rows: Vec<LockedPriorDebtRow> = sqlx::query_as(
        "SELECT d.entity_id, d.model_id, d.dim, d.op, d.generation,
                d.moved_from_generation, d.xact::text
           FROM proxiima_core.vector_index_debt d
           JOIN unnest($1::uuid[], $2::text[], $3::smallint[]) AS k(entity_id, model_id, dim)
             ON d.entity_id = k.entity_id
            AND d.model_id = k.model_id
            AND d.dim = k.dim
          ORDER BY d.entity_id, d.model_id, d.dim
            FOR NO KEY UPDATE OF d",
    )
    .bind(entities)
    .bind(models)
    .bind(dims)
    .fetch_all(&mut *tx)
    .await
    .map_err(map_err)?;
    let mut prior_by_key = HashMap::with_capacity(prior_rows.len());
    for (entity_id, model_id, dim, op, generation, moved_from_generation, xact) in prior_rows {
        let resolved = crate::vector_index::resolve_prior_debt_for_takeover(
            &mut *tx,
            entity_id,
            &model_id,
            dim,
            crate::vector_index::PriorDebt {
                op,
                generation,
                moved_from_generation,
                xact,
            },
        )
        .await?;
        if let Some(prior) = resolved {
            prior_by_key.insert((entity_id, model_id, dim), prior);
        }
    }
    Ok(prior_by_key)
}

async fn stage_publish_intents_with_prior_debt(
    tx: &mut sqlx::PgConnection,
    puts: &[VectorPut<'_>],
) -> Result<Vec<crate::vector_index::StagedPublishIntent>, StorageError> {
    let mut entities = Vec::with_capacity(puts.len());
    let mut models = Vec::with_capacity(puts.len());
    let mut dims = Vec::with_capacity(puts.len());
    let mut owners = Vec::with_capacity(puts.len());
    let mut generations = Vec::with_capacity(puts.len());
    for put in puts {
        let space = validate_embedding_vectors(put.vectors)?;
        entities.push(put.entity.entity_id());
        models.push(space.model_id());
        dims.push(crate::pgvector::Lane::of(space.dim()).width);
        owners.push(put.owner.into_inner());
        generations.push(put.generation);
    }
    let mut prior_by_key = lock_and_resolve_prior_debt(&mut *tx, &entities, &models, &dims).await?;
    let tx1_xact: String = sqlx::query_scalar(
        "WITH upserted AS (
             INSERT INTO proxiima_core.vector_index_debt
                 (entity_id, model_id, dim, owner_id, op, generation,
                  moved_from_generation, enqueued_at, xact)
             SELECT entity_id, model_id, dim, owner_id, 'delete', generation,
                    0, clock_timestamp(), pg_current_xact_id()
               FROM unnest($1::uuid[], $2::text[], $3::smallint[], $4::uuid[], $5::bigint[])
                    AS intent(entity_id, model_id, dim, owner_id, generation)
             ON CONFLICT (entity_id, model_id, dim) DO UPDATE
                 SET owner_id = EXCLUDED.owner_id,
                     op = 'delete',
                     generation = EXCLUDED.generation,
                     moved_from_generation = 0,
                     enqueued_at = clock_timestamp(),
                     xact = pg_current_xact_id()
             RETURNING xact::text AS xact
         )
         SELECT xact FROM upserted LIMIT 1",
    )
    .bind(&entities)
    .bind(&models)
    .bind(&dims)
    .bind(&owners)
    .bind(&generations)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_err)?;

    let mut staged = Vec::with_capacity(puts.len());
    for idx in 0..puts.len() {
        let key = (entities[idx], models[idx].to_owned(), dims[idx]);
        let prior_debt = prior_by_key.remove(&key);
        staged.push(crate::vector_index::StagedPublishIntent {
            entity_id: key.0,
            model_id: key.1,
            dim: key.2,
            generation: generations[idx],
            tx1_xact: tx1_xact.clone(),
            prior_debt,
        });
    }
    Ok(staged)
}

/// Extract canonical embedding lock keys for `writes`, validating every chunk
/// slice before any database lock or write.
pub(crate) fn embedding_head_lock_keys(
    writes: &[EmbeddingHeadWrite<'_>],
) -> Result<Vec<(Uuid, String, i16)>, StorageError> {
    let mut keys = Vec::with_capacity(writes.len());
    for write in writes {
        let space = validate_embedding_vectors(write.vectors)?;
        let dim = crate::pgvector::Lane::of(space.dim()).width;
        keys.push((write.entity.entity_id(), space.model_id().to_owned(), dim));
    }
    Ok(keys)
}

/// Commit `embedding_heads` for `puts` after `VectorIndex::put_batch` succeeded
/// (phase 3 of remote publication), verifying each still-eligible key's
/// write-ahead intent row still carries `put.generation`.
///
/// - If an entity became ineligible (forgotten, erased, or transferred) while
///   `put_batch` ran, its head is not written and its publish-intent lease is
///   cleared (`generation = 0`) so `'delete'` debt is immediately payable by
///   erase/drain; its outcome reports `embedding_version: 0`.
/// - If a still-eligible key's intent row was popped or replaced (e.g. by a
///   concurrent `drain_debt` or newer publisher), returns `Retryable` so the
///   whole transaction rolls back rather than committing a head whose vectors
///   may have been deleted.
/// - If a still-eligible memory's `is_head` flipped during `put_batch`,
///   re-enqueues `'head'` debt so `drain_debt` reconciles `is_head`.
pub(crate) async fn commit_remote_publish_heads(
    tx: &mut Transaction<'_, Postgres>,
    fences: &LockedEmbeddingFences,
    writes: &[EmbeddingHeadWrite<'_>],
    puts: &[VectorPut<'_>],
) -> Result<Vec<EmbeddingWriteOutcome>, StorageError> {
    let mut committed_versions: HashMap<(Uuid, &str, i16), i32> =
        HashMap::with_capacity(puts.len());
    for put in puts {
        let space = validate_embedding_vectors(put.vectors)?;
        let model_id = space.model_id();
        let dim = crate::pgvector::Lane::of(space.dim()).width;
        let entity_id = put.entity.entity_id();
        let Some((now_is_head, handle)) =
            embedding_entity_head_status(tx, &put.owner, put.entity).await?
        else {
            release_ineligible_publish_intent(
                tx.as_mut(),
                entity_id,
                model_id,
                dim,
                put.generation,
            )
            .await?;
            continue;
        };
        let covered = match handle {
            Some(handle) => fences.handles.contains(&handle) && fences.targets.contains(&entity_id),
            None => fences.targets.contains(&entity_id),
        };
        if !covered {
            return Err(StorageError::Retryable(
                "embedding write target appeared after lifecycle fences".into(),
            ));
        }
        consume_owned_publish_intent(tx.as_mut(), entity_id, model_id, dim, put.generation).await?;
        let committed_version: Option<i32> = sqlx::query_scalar(
            "INSERT INTO proxiima_core.embedding_heads
                 (entity_id, model_id, dim, embedding_version, owner_id, vector_generation)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (entity_id, model_id, dim) DO UPDATE
                 SET embedding_version = GREATEST(
                         proxiima_core.embedding_heads.embedding_version + 1,
                         EXCLUDED.embedding_version
                     ),
                     vector_generation = EXCLUDED.vector_generation
               WHERE proxiima_core.embedding_heads.vector_generation < EXCLUDED.vector_generation
             RETURNING embedding_version",
        )
        .bind(entity_id)
        .bind(model_id)
        .bind(dim)
        .bind(put.version)
        .bind(put.owner.into_inner())
        .bind(put.generation)
        .fetch_optional(tx.as_mut())
        .await
        .map_err(map_err)?;
        let Some(embedding_version) = committed_version else {
            return Err(StorageError::Retryable(
                "a newer embedding generation already committed for this key".into(),
            ));
        };
        if now_is_head != put.is_head {
            enqueue_head_sync_debt(tx.as_mut(), entity_id, model_id, dim, put.owner).await?;
        }
        committed_versions.insert((entity_id, model_id, dim), embedding_version);
    }
    let mut outcomes = Vec::with_capacity(writes.len());
    for write in writes {
        let space = validate_embedding_vectors(write.vectors)?;
        let dim = crate::pgvector::Lane::of(space.dim()).width;
        let key = (write.entity.entity_id(), space.model_id(), dim);
        outcomes.push(EmbeddingWriteOutcome {
            embedding_version: committed_versions.get(&key).copied().unwrap_or(0),
        });
    }
    Ok(outcomes)
}

async fn consume_owned_publish_intent(
    tx: &mut sqlx::PgConnection,
    entity_id: Uuid,
    model_id: &str,
    dim: i16,
    generation: i64,
) -> Result<(), StorageError> {
    let owned_intent = sqlx::query(
        "DELETE FROM proxiima_core.vector_index_debt
          WHERE entity_id = $1
            AND model_id = $2
            AND dim = $3
            AND generation = $4",
    )
    .bind(entity_id)
    .bind(model_id)
    .bind(dim)
    .bind(generation)
    .execute(&mut *tx)
    .await
    .map_err(map_err)?;
    if owned_intent.rows_affected() == 0 {
        return Err(StorageError::Retryable(
            "embedding publish intent was superseded while writing to the vector index".into(),
        ));
    }
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
    .bind(generation)
    .execute(&mut *tx)
    .await
    .map(drop)
    .map_err(map_err)
}

async fn release_ineligible_publish_intent(
    tx: &mut sqlx::PgConnection,
    entity_id: Uuid,
    model_id: &str,
    dim: i16,
    generation: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE proxiima_core.vector_index_debt
            SET generation = 0,
                moved_from_generation = 0,
                enqueued_at = clock_timestamp()
          WHERE entity_id = $1
            AND model_id = $2
            AND dim = $3
            AND (generation = $4 OR moved_from_generation = $4)",
    )
    .bind(entity_id)
    .bind(model_id)
    .bind(dim)
    .bind(generation)
    .execute(tx)
    .await
    .map(drop)
    .map_err(map_err)
}

async fn enqueue_head_sync_debt(
    tx: &mut sqlx::PgConnection,
    entity_id: Uuid,
    model_id: &str,
    dim: i16,
    owner: OwnerId,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO proxiima_core.vector_index_debt
             (entity_id, model_id, dim, owner_id, op, generation, moved_from_generation, enqueued_at)
         VALUES ($1, $2, $3, $4, 'head', 0, 0, clock_timestamp())
         ON CONFLICT (entity_id, model_id, dim) DO UPDATE
             SET owner_id = EXCLUDED.owner_id,
                 op = CASE WHEN proxiima_core.vector_index_debt.op IN ('delete', 'owner')
                           THEN proxiima_core.vector_index_debt.op
                           ELSE 'head' END,
                 generation = CASE WHEN proxiima_core.vector_index_debt.op = 'delete'
                                        AND proxiima_core.vector_index_debt.generation > 0
                                   THEN proxiima_core.vector_index_debt.generation
                                   ELSE 0 END,
                 moved_from_generation = CASE
                     WHEN proxiima_core.vector_index_debt.op = 'delete'
                      AND proxiima_core.vector_index_debt.moved_from_generation > 0
                     THEN proxiima_core.vector_index_debt.moved_from_generation
                     ELSE 0 END,
                 enqueued_at = CASE
                     WHEN proxiima_core.vector_index_debt.op = 'delete'
                      AND (proxiima_core.vector_index_debt.generation > 0
                           OR proxiima_core.vector_index_debt.moved_from_generation > 0)
                     THEN proxiima_core.vector_index_debt.enqueued_at
                     ELSE clock_timestamp() END,
                 xact = pg_current_xact_id()",
    )
    .bind(entity_id)
    .bind(model_id)
    .bind(dim)
    .bind(owner.into_inner())
    .execute(tx)
    .await
    .map(drop)
    .map_err(map_err)
}

/// Validate the whole batch before acquiring a claim or writing any chunk.
pub(crate) fn validate_embedding_vectors(
    vectors: &[SpaceVector],
) -> Result<&EmbeddingSpace, StorageError> {
    let first = vectors.first().ok_or_else(|| {
        StorageError::ConstraintViolation("embedding must contain at least one chunk".into())
    })?;
    if i32::try_from(vectors.len()).is_err() {
        return Err(StorageError::ConstraintViolation(
            "embedding has too many chunks".into(),
        ));
    }
    if vectors.iter().any(|vector| vector.space() != first.space()) {
        return Err(StorageError::ConstraintViolation(
            "embedding chunks must use one space".into(),
        ));
    }
    if vectors
        .iter()
        .any(|vector| vector.values().iter().any(|value| !value.is_finite()))
    {
        return Err(StorageError::ConstraintViolation(
            "embedding chunks must contain finite values".into(),
        ));
    }
    Ok(first.space())
}

async fn embedding_entity_head_status(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    entity: EmbeddableEntityRef,
) -> Result<Option<(bool, Option<Uuid>)>, StorageError> {
    let owner_id = owner.into_inner();
    let entity_id = entity.entity_id();
    sqlx::query_as(
        "SELECT is_head, handle FROM (
            SELECT EXISTS (
                SELECT 1 FROM proxiima_core.memory_head mh
                 WHERE mh.owner_id = m.owner_id
                   AND mh.t = m.t
                   AND mh.handle = m.handle
            ) AS is_head,
            m.handle AS handle
              FROM proxiima_core.live_memory m
             WHERE m.t = $1 AND m.owner_id = $2
            UNION ALL
            SELECT true AS is_head, NULL::uuid AS handle
              FROM proxiima_core.goal g
             WHERE g.t = $1 AND g.owner_id = $2
        ) eligible
        LIMIT 1",
    )
    .bind(entity_id)
    .bind(owner_id)
    .fetch_optional(tx.as_mut())
    .await
    .map_err(map_err)
}

async fn embedding_entity_is_eligible(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    entity: EmbeddableEntityRef,
) -> Result<bool, StorageError> {
    Ok(embedding_entity_head_status(tx, owner, entity)
        .await?
        .is_some())
}
