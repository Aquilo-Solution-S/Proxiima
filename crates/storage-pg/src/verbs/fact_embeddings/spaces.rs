//! Per-Owner embedding state by space: the coverage report a host reads
//! before flipping a moving route, and the purge that drops the spaces a
//! route no longer names.

use proxiima_core::{
    EmbeddingPurgeOutcome, EmbeddingSpace, EmbeddingSpaceCounts, OwnerId, StorageError,
};
use sqlx::{Postgres, Transaction};

use crate::error::map_err;

use super::ensure_nonnegative_limit;
use crate::sidecars::int_to_u64;

/// The spaces an Owner's route names now, with the registry's per-schema
/// veto: what a hydrate or a transfer queues a memory for.
///
/// The engine resolves the route; storage holds neither the router nor the
/// registry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteSpaces<'a> {
    pub(crate) spaces: &'a [EmbeddingSpace],
    pub(crate) non_embeddable_schemas: &'a [String],
}

const EMBEDDING_COVERAGE_SQL: &str = "
WITH heads AS MATERIALIZED (
     SELECT m.t
       FROM proxiima_core.memory_head mh
       JOIN proxiima_core.live_memory m ON m.handle = mh.handle AND m.t = mh.t
      WHERE m.owner_id = $1
        AND mh.schema_id <> ALL($2::text[])
 ),
 spaces AS (
     SELECT DISTINCT e.model_id, e.dim
       FROM proxiima_core.embeddings e
      WHERE e.owner_id = $1
     UNION
     SELECT DISTINCT h.model_id, h.dim
       FROM proxiima_core.embedding_heads h
      WHERE h.owner_id = $1
     UNION
     SELECT DISTINCT j.model_id, j.dim
       FROM proxiima_core.embedding_jobs j
      WHERE j.owner_id = $1
     UNION
     SELECT r.model_id, r.dim
       FROM unnest($3::text[], $4::smallint[]) AS r(model_id, dim)
 )
 SELECT s.model_id,
        s.dim,
        (SELECT count(*) FROM heads)::bigint AS embeddable,
        (SELECT count(*)
           FROM heads h
           JOIN proxiima_core.embedding_heads eh
             ON eh.entity_id = h.t
            AND eh.model_id = s.model_id
            AND eh.dim = s.dim)::bigint AS embedded,
        (SELECT count(*)
           FROM proxiima_core.embeddings e
          WHERE e.owner_id = $1
            AND e.model_id = s.model_id
            AND e.dim = s.dim)::bigint AS vectors,
        j.pending,
        j.processing,
        j.failed,
        j.failed_permanent
   FROM spaces s
  CROSS JOIN LATERAL (
     SELECT count(*) FILTER (WHERE status = 'pending')::bigint AS pending,
            count(*) FILTER (WHERE status = 'processing')::bigint AS processing,
            count(*) FILTER (WHERE status = 'failed')::bigint AS failed,
            count(*) FILTER (WHERE status = 'failed_permanent')::bigint AS failed_permanent
       FROM proxiima_core.embedding_jobs
      WHERE owner_id = $1
        AND model_id = s.model_id
        AND dim = s.dim
 ) j
  ORDER BY s.model_id, s.dim";

#[derive(sqlx::FromRow)]
struct CoverageRow {
    model_id: String,
    dim: i16,
    embeddable: i64,
    embedded: i64,
    vectors: i64,
    pending: i64,
    processing: i64,
    failed: i64,
    failed_permanent: i64,
}

/// `owner`'s embedding state in every space it has vectors or jobs in, and
/// in each of `spaces`.
///
/// # Errors
///
/// Maps SQL failures through the shared mapper; `Internal` for a stored
/// width with no lane.
pub(crate) async fn embedding_coverage(
    pool: &sqlx::PgPool,
    platform: &crate::PgPlatformScope,
    owner: &OwnerId,
    spaces: &[EmbeddingSpace],
    non_embeddable_schemas: &[String],
) -> Result<Vec<(EmbeddingSpace, EmbeddingSpaceCounts)>, StorageError> {
    let (models, dims) = space_columns(spaces);
    let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
    let result = sqlx::query_as::<_, CoverageRow>(EMBEDDING_COVERAGE_SQL)
        .bind(owner.into_inner())
        .bind(non_embeddable_schemas)
        .bind(&models)
        .bind(&dims)
        .fetch_all(tx.as_mut())
        .await
        .map_err(map_err);
    let rows = crate::owner_scope::finish_transaction(tx, result).await?;
    rows.into_iter()
        .map(|row| {
            let space = crate::pgvector::stored_space(row.model_id, row.dim)?;
            Ok((
                space,
                EmbeddingSpaceCounts {
                    embeddable: int_to_u64(row.embeddable, "embeddable")?,
                    embedded: int_to_u64(row.embedded, "embedded")?,
                    vectors: int_to_u64(row.vectors, "vector")?,
                    pending: int_to_u64(row.pending, "pending job")?,
                    processing: int_to_u64(row.processing, "processing job")?,
                    failed: int_to_u64(row.failed, "failed job")?,
                    failed_permanent: int_to_u64(row.failed_permanent, "failed permanent job")?,
                },
            ))
        })
        .collect()
}

/// One statement per call, so a purge and a concurrent write see one
/// snapshot. Each table is cut to `$5` rows; the engine loops until a
/// call deletes fewer than that from every table. `$4` narrows the purge to one series' versions
/// (a transfer); `NULL` is every memory of the Owner. `$6..$8` pin the exact
/// `embedding_heads` keys already locked with `lock_embedding_keys_batch` so
/// the `AFTER DELETE` trigger on `embedding_heads` cannot deadlock with a
/// concurrent remote publication deleting `vector_index_debt` before upserting
/// `embedding_heads`. Jobs go in [`PURGE_EMBEDDING_JOBS_SQL`].
const PURGE_EMBEDDING_SPACES_SQL: &str = "
WITH keep AS (
     SELECT k.model_id, k.dim
       FROM unnest($2::text[], $3::smallint[]) AS k(model_id, dim)
 ),
 doomed_heads AS (
     SELECT entity_id, model_id, dim
       FROM unnest($6::uuid[], $7::text[], $8::smallint[]) AS d(entity_id, model_id, dim)
 ),
 doomed_vectors AS (
     SELECT e.entity_id, e.model_id, e.dim, e.embedding_version, e.chunk_ordinal
       FROM proxiima_core.embeddings e
      WHERE e.owner_id = $1
        AND ($4::uuid[] IS NULL OR e.entity_id = ANY($4::uuid[]))
        AND NOT EXISTS (
            SELECT 1 FROM keep k WHERE k.model_id = e.model_id AND k.dim = e.dim
        )
        AND (
            EXISTS (
                SELECT 1 FROM doomed_heads dh
                 WHERE dh.entity_id = e.entity_id
                   AND dh.model_id = e.model_id
                   AND dh.dim = e.dim
            )
            OR NOT EXISTS (
                SELECT 1 FROM proxiima_core.embedding_heads h
                 WHERE h.entity_id = e.entity_id
                   AND h.model_id = e.model_id
                   AND h.dim = e.dim
            )
        )
      LIMIT $5
 ),
 vectors AS (
     DELETE FROM proxiima_core.embeddings e
      USING doomed_vectors d
      WHERE e.entity_id = d.entity_id
        AND e.model_id = d.model_id
        AND e.dim = d.dim
        AND e.embedding_version = d.embedding_version
        AND e.chunk_ordinal = d.chunk_ordinal
     RETURNING 1
 ),
 heads AS (
     DELETE FROM proxiima_core.embedding_heads h
      USING doomed_heads d
      WHERE h.owner_id = $1
        AND h.entity_id = d.entity_id
        AND h.model_id = d.model_id
        AND h.dim = d.dim
     RETURNING 1
 )
 SELECT (SELECT count(*) FROM vectors)::bigint AS vectors,
        (SELECT count(*) FROM heads)::bigint AS heads";

/// Up to `$5` of `owner`'s jobs outside the spaces `$2, $3` keep, narrowed to
/// one series by `$4`, deleted under blocking row locks taken in canonical
/// `(entity_id, model_id, dim)` order, the order a publication settles its
/// claims in. A job another transaction holds is waited for, and its status
/// is rechecked once that transaction commits: a requeue from `failed` to
/// `pending` still goes, a claim settling as `processing` stays. A caller holds
/// no embedding-key lock while this waits, since a publisher settling its claim
/// locks the row before it waits on its key lock.
const PURGE_EMBEDDING_JOBS_SQL: &str = "
WITH doomed_jobs AS (
     SELECT j.job_id
       FROM proxiima_core.embedding_jobs j
      WHERE j.owner_id = $1
        AND j.status <> 'processing'
        AND ($4::uuid[] IS NULL OR j.entity_id = ANY($4::uuid[]))
        AND NOT EXISTS (
            SELECT 1
              FROM unnest($2::text[], $3::smallint[]) AS k(model_id, dim)
             WHERE k.model_id = j.model_id AND k.dim = j.dim
        )
      ORDER BY j.entity_id, j.model_id COLLATE \"C\", j.dim, j.job_id
      FOR UPDATE OF j
      LIMIT $5
 ),
 jobs AS (
     DELETE FROM proxiima_core.embedding_jobs j
      USING doomed_jobs d
      WHERE j.job_id = d.job_id
     RETURNING 1
 )
 SELECT count(*)::bigint FROM jobs";

/// Delete up to `limit` rows per table of `owner`'s embedding state in
/// spaces outside `keep`.
///
/// # Errors
///
/// `ConstraintViolation` for a negative limit; otherwise maps SQL failures
/// through the shared mapper.
/// `Proxiima.Embeddings.purgeJob`.
pub(crate) async fn purge_embedding_spaces(
    pool: &sqlx::PgPool,
    platform: &crate::PgPlatformScope,
    owner: &OwnerId,
    keep: &[EmbeddingSpace],
    limit: i64,
) -> Result<EmbeddingPurgeOutcome, StorageError> {
    let limit = ensure_nonnegative_limit(limit)?;
    // Level 1 (shared owner fence) -> Level 5 (canonical embedding keys) ->
    // Level 6 (row deletes + vector_index_debt trigger), matching remote
    // publication and whole-owner erase so purge never deadlocks with them.
    let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
    let result = async {
        crate::access::fences::lock_owner_fence_shared_tx(&mut tx, owner).await?;
        purge_vectors_and_heads_in_tx(&mut tx, owner.into_inner(), keep, None, limit).await
    }
    .await;
    let (vectors, heads) = crate::owner_scope::finish_transaction(tx, result).await?;
    // Jobs in a transaction of their own, holding no embedding-key lock while
    // it waits on a job row.
    let mut tx = crate::platform_scope::begin_platform_transaction(pool, platform).await?;
    let result = async {
        crate::access::fences::lock_owner_fence_shared_tx(&mut tx, owner).await?;
        purge_jobs_in_tx(&mut tx, owner.into_inner(), keep, None, limit).await
    }
    .await;
    let jobs = crate::owner_scope::finish_transaction(tx, result).await?;
    Ok(EmbeddingPurgeOutcome {
        vectors,
        heads,
        jobs,
    })
}

/// A destination with no route keeps none of the series' embedding state,
/// so this statement names no space. Its job delete waits on locked rows: a
/// transfer holds both owners' fences exclusively, and a publisher takes its
/// owner's fence shared before it locks a job row, so no publisher holds one
/// of these rows while waiting on the transfer.
const DROP_SERIES_EMBEDDINGS_SQL: &str = "
WITH vectors AS (
     DELETE FROM proxiima_core.embeddings
      WHERE owner_id = $1 AND entity_id = ANY($2::uuid[])
     RETURNING 1
 ),
 heads AS (
     DELETE FROM proxiima_core.embedding_heads
      WHERE owner_id = $1 AND entity_id = ANY($2::uuid[])
     RETURNING 1
 ),
 jobs AS (
     DELETE FROM proxiima_core.embedding_jobs
      WHERE owner_id = $1 AND entity_id = ANY($2::uuid[]) AND status <> 'processing'
     RETURNING 1
 )
 SELECT (SELECT count(*) FROM vectors)::bigint AS vectors,
        (SELECT count(*) FROM heads)::bigint AS heads,
        (SELECT count(*) FROM jobs)::bigint AS jobs";

/// A transfer's leg: after the series' rows moved to `owner_id`, drop its
/// vectors, heads and jobs in spaces the destination's route does not name.
pub(crate) async fn purge_series_embedding_spaces_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    owner_id: uuid::Uuid,
    keep: &[EmbeddingSpace],
    ts: &[uuid::Uuid],
) -> Result<EmbeddingPurgeOutcome, StorageError> {
    if !keep.is_empty() {
        let (vectors, heads) =
            purge_vectors_and_heads_in_tx(tx, owner_id, keep, Some(ts), i64::MAX).await?;
        let jobs = purge_jobs_in_tx(tx, owner_id, keep, Some(ts), i64::MAX).await?;
        return Ok(EmbeddingPurgeOutcome {
            vectors,
            heads,
            jobs,
        });
    }
    let head_keys: Vec<(uuid::Uuid, String, i16)> = sqlx::query_as(
        "SELECT h.entity_id, h.model_id, h.dim
           FROM proxiima_core.embedding_heads h
          WHERE h.owner_id = $1
            AND h.entity_id = ANY($2::uuid[])
          ORDER BY h.entity_id, h.model_id, h.dim",
    )
    .bind(owner_id)
    .bind(ts)
    .fetch_all(tx.as_mut())
    .await
    .map_err(map_err)?;
    super::lock_embedding_keys_batch(tx.as_mut(), &head_keys).await?;
    let (vectors, heads, jobs): (i64, i64, i64) = sqlx::query_as(DROP_SERIES_EMBEDDINGS_SQL)
        .bind(owner_id)
        .bind(ts)
        .fetch_one(tx.as_mut())
        .await
        .map_err(map_err)?;
    Ok(EmbeddingPurgeOutcome {
        vectors: int_to_u64(vectors, "purged vector")?,
        heads: int_to_u64(heads, "purged head")?,
        jobs: int_to_u64(jobs, "purged job")?,
    })
}

/// Up to `limit` vectors and heads outside `keep`, under the canonical
/// embedding-key locks of the heads it deletes; returns their counts.
async fn purge_vectors_and_heads_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    owner_id: uuid::Uuid,
    keep: &[EmbeddingSpace],
    ts: Option<&[uuid::Uuid]>,
    limit: i64,
) -> Result<(u64, u64), StorageError> {
    let (models, dims) = space_columns(keep);
    let head_keys: Vec<(uuid::Uuid, String, i16)> = sqlx::query_as(
        "SELECT h.entity_id, h.model_id, h.dim
           FROM proxiima_core.embedding_heads h
          WHERE h.owner_id = $1
            AND ($4::uuid[] IS NULL OR h.entity_id = ANY($4::uuid[]))
            AND NOT EXISTS (
                SELECT 1
                  FROM unnest($2::text[], $3::smallint[]) AS k(model_id, dim)
                 WHERE k.model_id = h.model_id AND k.dim = h.dim
            )
          ORDER BY h.entity_id, h.model_id, h.dim
          LIMIT $5",
    )
    .bind(owner_id)
    .bind(&models)
    .bind(&dims)
    .bind(ts)
    .bind(limit)
    .fetch_all(tx.as_mut())
    .await
    .map_err(map_err)?;
    super::lock_embedding_keys_batch(tx.as_mut(), &head_keys).await?;
    let mut head_entities = Vec::with_capacity(head_keys.len());
    let mut head_models = Vec::with_capacity(head_keys.len());
    let mut head_dims = Vec::with_capacity(head_keys.len());
    for (entity_id, model_id, dim) in &head_keys {
        head_entities.push(*entity_id);
        head_models.push(model_id.as_str());
        head_dims.push(*dim);
    }
    let (vectors, heads): (i64, i64) = sqlx::query_as(PURGE_EMBEDDING_SPACES_SQL)
        .bind(owner_id)
        .bind(&models)
        .bind(&dims)
        .bind(ts)
        .bind(limit)
        .bind(&head_entities)
        .bind(&head_models)
        .bind(&head_dims)
        .fetch_one(tx.as_mut())
        .await
        .map_err(map_err)?;
    Ok((
        int_to_u64(vectors, "purged vector")?,
        int_to_u64(heads, "purged head")?,
    ))
}

/// Up to `limit` jobs outside `keep`, as [`PURGE_EMBEDDING_JOBS_SQL`] locks
/// them; returns their count.
async fn purge_jobs_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    owner_id: uuid::Uuid,
    keep: &[EmbeddingSpace],
    ts: Option<&[uuid::Uuid]>,
    limit: i64,
) -> Result<u64, StorageError> {
    let (models, dims) = space_columns(keep);
    let jobs: i64 = sqlx::query_scalar(PURGE_EMBEDDING_JOBS_SQL)
        .bind(owner_id)
        .bind(&models)
        .bind(&dims)
        .bind(ts)
        .bind(limit)
        .fetch_one(tx.as_mut())
        .await
        .map_err(map_err)?;
    int_to_u64(jobs, "purged job")
}

/// Queue a series' hot head for every route space it has no vector in: a
/// transfer's other leg, so the moved memory is embedded under the
/// destination's route. A head under a non-embeddable schema, or one whose
/// row is forgotten, queues nothing.
pub(crate) async fn enqueue_series_head_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    owner_id: uuid::Uuid,
    handle: uuid::Uuid,
    route: RouteSpaces<'_>,
) -> Result<(), StorageError> {
    if route.spaces.is_empty() {
        return Ok(());
    }
    let (models, dims) = space_columns(route.spaces);
    sqlx::query(
        "INSERT INTO proxiima_core.embedding_jobs (entity_id, model_id, dim, owner_id)
         SELECT mh.t, r.model_id, r.dim, $2
           FROM proxiima_core.memory_head mh
           JOIN proxiima_core.live_memory m ON m.handle = mh.handle AND m.t = mh.t
          CROSS JOIN unnest($3::text[], $4::smallint[]) AS r(model_id, dim)
          WHERE mh.handle = $1
            AND mh.schema_id <> ALL($5::text[])
            AND NOT EXISTS (
                SELECT 1
                  FROM proxiima_core.embedding_heads h
                 WHERE h.entity_id = mh.t
                   AND h.model_id = r.model_id
                   AND h.dim = r.dim
            )
         ON CONFLICT (owner_id, entity_id, model_id, dim) DO NOTHING",
    )
    .bind(handle)
    .bind(owner_id)
    .bind(&models)
    .bind(&dims)
    .bind(route.non_embeddable_schemas)
    .execute(tx.as_mut())
    .await
    .map_err(map_err)?;
    Ok(())
}

fn space_columns(spaces: &[EmbeddingSpace]) -> (Vec<String>, Vec<i16>) {
    spaces
        .iter()
        .map(|space| {
            (
                space.model_id().to_owned(),
                crate::pgvector::Lane::of(space.dim()).width,
            )
        })
        .unzip()
}
