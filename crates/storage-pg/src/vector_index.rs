//! Where a store keeps its chunk vectors: its own pgvector index, or a host
//! index behind [`VectorIndex`].

use std::sync::Arc;

use proxiima_core::storage_ports::{
    CandidateBound, VectorCandidate, VectorCandidatePage, VectorIndex,
};
use proxiima_core::{EmbeddingDim, MemoryId, OwnerId, PgIdent, SpaceVector, SqlText, StorageError};
use sqlx::{Postgres, Transaction};

use crate::error::map_err;
use crate::pgvector::{Lane, vector};
use crate::projection::Artifact;

/// Maximum chunk window used by adaptive ANN scans.
pub(crate) const SEMANTIC_SCAN_CAP: u32 = proxiima_core::ledger!(Proxiima.Search.semanticScanCap);

/// Where this store keeps its chunk vectors (#519): a closed choice every
/// vector path matches case by case.
#[derive(Clone)]
pub(crate) enum StoreVectors {
    /// pgvector in the store's own database, written and deleted in the
    /// transaction that changes their rows.
    Local,
    /// A host index; the store keeps the heads and pays index debt (see
    /// [`proxiima_core::storage_ports::VectorIndex`]).
    Remote(Arc<dyn VectorIndex>),
}

impl std::fmt::Debug for StoreVectors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => f.write_str("Local"),
            Self::Remote(_) => f.write_str("Remote"),
        }
    }
}

/// Up to `window` memories nearest `query` from the local pgvector index, on
/// the caller's transaction.
pub(crate) async fn local_nearest(
    context: &mut Transaction<'static, Postgres>,
    owners: &[OwnerId],
    query: &SpaceVector,
    window: u32,
    supersession: proxiima_core::verbs::query::SupersessionStatus,
) -> Result<VectorCandidatePage, StorageError> {
    if owners.is_empty() || window == 0 {
        let bound = if owners.is_empty() {
            CandidateBound::Complete
        } else {
            CandidateBound::Bounded
        };
        return Ok(VectorCandidatePage::new(Vec::new(), bound));
    }
    let lane = Lane::of(query.space().dim());
    let owner_ids: Vec<_> = owners.iter().copied().map(OwnerId::into_inner).collect();
    let function = PgIdent::name(&format!("vector_candidates_d{}", lane.width))?;
    let mut sql = SqlText::new(
        "SELECT memory_id, similarity_score
           FROM proxiima_core.",
    );
    sql.push_ident(&function).push("($1, $2, $3, $4, $5)");
    let rows: Vec<(uuid::Uuid, f32)> = sqlx::query_as(sql)
        .bind(owner_ids)
        .bind(query.space().model_id())
        .bind(vector(query.values()))
        .bind(i64::from(window))
        .bind(matches!(
            supersession,
            proxiima_core::verbs::query::SupersessionStatus::HeadsOnly
        ))
        .fetch_all(context.as_mut())
        .await
        .map_err(map_err)?;
    // A short nonempty page claims no exhaustion, though only the exact scan
    // of the owners' chunks returns one: `Proxiima.Search.chunkCandidateBound`.
    let bound = if rows.is_empty() {
        CandidateBound::Complete
    } else if rows.len() >= usize::try_from(window.min(SEMANTIC_SCAN_CAP)).unwrap_or(usize::MAX) {
        CandidateBound::Bounded
    } else {
        CandidateBound::Unknown
    };
    Ok(VectorCandidatePage::new(
        rows.into_iter()
            .map(|(memory_id, similarity_score)| VectorCandidate {
                memory_id: MemoryId::new(memory_id),
                similarity_score,
                generation: 0,
            })
            .collect(),
        bound,
    ))
}

/// Candidate functions for every supported width, with their inverses.
///
/// Provision `proxiima_search` before installing these artifacts. Memory RLS remains enabled for that role. Runtime roles need
/// explicit execution grants; no candidate function is public.
/// # Errors
/// Propagates invalid generated SQL identifiers.
pub fn vector_candidate_artifacts() -> Result<Vec<Artifact>, StorageError> {
    EmbeddingDim::ALL
        .into_iter()
        .map(|dim| {
            let lane = Lane::of(dim);
            let name = format!("proxiima_core.vector_candidates_d{}", lane.width);
            let name = PgIdent::table(&name)?;
            let mut signature = SqlText::new("");
            signature
                .push_ident(&name)
                .push("(uuid[], text, public.vector, bigint, boolean)");
            let body = nearest_sql(lane);
            let mut ddl = SqlText::new("DROP FUNCTION IF EXISTS ");
            ddl.push_ident(&name)
                .push(
                    "(uuid[], text, public.vector, bigint);
                 CREATE OR REPLACE FUNCTION ",
                )
                .push_ident(&name)
                .push(
                    "(uuid[], text, public.vector, bigint, boolean)
                 RETURNS TABLE(memory_id uuid, similarity_score real)
                 LANGUAGE plpgsql STABLE SECURITY DEFINER
                 SET search_path = pg_catalog, pg_temp
                 SET plan_cache_mode = force_custom_plan
                 AS $body$",
                )
                .push_text(&body)
                .push("$body$;");
            let mut inverse = SqlText::new("DROP FUNCTION IF EXISTS ");
            inverse.push_text(&signature).push(";");
            let forward = crate::search_candidates::installation_sql(
                "proxiima_core",
                &ddl,
                &signature,
                &[
                    "proxiima_core.embeddings",
                    "proxiima_core.embedding_heads",
                    "proxiima_core.memory",
                ],
                &["proxiima_core.embeddings", "proxiima_core.embedding_heads"],
            )?;
            Ok(Artifact { forward, inverse })
        })
        .collect()
}

/// `R` in the rule of `nearest_sql`, measured rather than derived: on two
/// 1024-d corpora of about 100,000 chunks, one single-chunk and one
/// multi-chunk, the exact scan won up to `n² = 101·target·N` and the walk won
/// from `120·target·N`.
const EXACT_SCAN_RATIO: u32 = 108;

/// The most owner chunks the exact scan scores before the walk takes over,
/// whatever the lane's size: it bounds the count that decides, and past it
/// the walk won.
const EXACT_SCAN_BUDGET_CAP: u32 = 32_000;

/// `exact_budget`, the most owner chunks `nearest_sql` scores exactly, and
/// `owned`, the owners' chunks counted up to one past it.
fn exact_budget_sql(lane: Lane, width: &SqlText) -> SqlText {
    let mut sql = SqlText::new("    SELECT LEAST(floor(sqrt(");
    sql.push_u64(u64::from(EXACT_SCAN_RATIO))
        .push(" * target * GREATEST(c.reltuples, target))), ")
        .push_u64(u64::from(EXACT_SCAN_BUDGET_CAP))
        .push(
            ")::bigint
      INTO exact_budget
      FROM pg_catalog.pg_class c
     WHERE c.oid = 'proxiima_core.embeddings_hnsw_d",
        )
        .push_text(width)
        .push(
            "'::regclass;
    SELECT count(*) INTO owned
      FROM (SELECT 1 FROM proxiima_core.embeddings emb
             WHERE emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND ",
        )
        .push(lane.predicate)
        .push(
            "
             LIMIT exact_budget + 1) owned_chunks;",
        );
    sql
}

/// The owners' memories nearest the query, exact or by index.
///
/// A walk for `target` memories through a lane of `N` chunks, `n` of them the
/// owners', visits about `target·N/n` tuples; the exact scan scores all `n`.
/// So the owners' chunks are scored exactly when `n² ≤ R·target·N` and
/// `n ≤ EXACT_SCAN_BUDGET_CAP`, with `N` the row estimate of the lane's own
/// index, so another width's rows do not count; and when the walk's chunk
/// window cannot fill `target` memories. Every statement is planned for the
/// call's owners, because a plan cached for one owner set serves no other.
pub(crate) fn nearest_sql(lane: Lane) -> SqlText {
    let mut cap = SqlText::new("");
    cap.push_u64(u64::from(SEMANTIC_SCAN_CAP));
    let mut width = SqlText::new("");
    width.push_u64(u64::try_from(lane.width).unwrap_or_default());
    let budget = exact_budget_sql(lane, &width);
    let scan = candidate_scan_sql(lane, "chunk_window");
    let exact = best_chunk_scores_sql(lane, false, "target");
    let rerank = best_chunk_scores_sql(lane, true, "target");
    let mut sql = SqlText::new(
        "
DECLARE
    target bigint := LEAST(GREATEST($4, 0), ",
    );
    sql.push_text(&cap)
        .push(
            ");
    chunk_window bigint := GREATEST(target, 1);
    candidate_ids uuid[] := '{}'::uuid[];
    scanned_ids uuid[];
    fetched bigint;
    exact_budget bigint;
    owned bigint;
BEGIN
    PERFORM proxiima_core.assert_search_scope($1);
    IF public.vector_dims($3) IS DISTINCT FROM ",
        )
        .push_text(&width)
        .push(
            " THEN
        RAISE EXCEPTION 'query vector does not match index width'
            USING ERRCODE = 'PX002';
    END IF;
    IF target = 0 THEN
        RETURN;
    END IF;
",
        )
        .push_text(&budget)
        .push(
            "
    IF owned > exact_budget THEN
        LOOP
            WITH scanned AS MATERIALIZED (
",
        )
        .push_text(&scan)
        .push(
            "
            )
            SELECT COALESCE(array_agg(DISTINCT s.entity_id), '{}'::uuid[]), count(*)
              INTO scanned_ids, fetched FROM scanned s;
            SELECT ARRAY(SELECT DISTINCT id FROM unnest(candidate_ids || scanned_ids) id)
              INTO candidate_ids;
            EXIT WHEN fetched < chunk_window
                   OR cardinality(candidate_ids) >= target
                   OR chunk_window >= ",
        )
        .push_text(&cap)
        .push(
            ";
            chunk_window := LEAST(chunk_window * 2, ",
        )
        .push_text(&cap)
        .push(
            ");
        END LOOP;
        IF cardinality(candidate_ids) >= target THEN
            RETURN QUERY
",
        )
        .push_text(&rerank)
        .push(
            "
            RETURN;
        END IF;
    END IF;
    RETURN QUERY
",
        )
        .push_text(&exact)
        .push(
            "
END;
",
        );
    sql
}

/// What `heads_only` (`$5`) admits of a candidate memory `m`: a head a Goal
/// shows the caller, asked only with a Goal subject, or the head of `m`'s own
/// series. A head has its versions' owner
/// (`Proxiima.Identity.seriesContract`), so it is looked up among
/// `m.owner_id`'s heads alone.
const HEADS_ONLY_FILTER: &str = "
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))";

/// Each memory's best chunk at its head embedding version, scored exactly,
/// best first.
///
/// Every memory is scored and sorted before any is checked; the checks then
/// run one memory at a time in that order until `target` memories pass. The
/// lateral `LIMIT 1` is what keeps them there: a subquery with a `LIMIT` is
/// never flattened into a join the planner could run over every memory first.
fn best_chunk_scores_sql(lane: Lane, candidates_only: bool, target: &'static str) -> SqlText {
    let candidate_filter = if candidates_only {
        "
                   AND emb.entity_id = ANY(candidate_ids)"
    } else {
        ""
    };
    let mut sql = SqlText::new(
        "        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND ",
    );
    sql.push(lane.predicate)
        .push(candidate_filter)
        .push(
            "
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)",
        )
        .push(HEADS_ONLY_FILTER)
        .push(
            "
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT ",
        )
        .push(target)
        .push(";");
    sql
}

fn candidate_scan_sql(lane: Lane, limit: &'static str) -> SqlText {
    let public = SqlText::new("::public.");
    let vec = SqlText::fill(lane.vec, &[("::", &public)]);
    let cast = SqlText::fill(lane.cast, &[("::", &public)]);
    let mut sql = SqlText::new(
        "            SELECT emb.entity_id
              FROM proxiima_core.embeddings emb
             WHERE ",
    );
    sql.push(lane.predicate)
        .push(
            "
               AND emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.embedding_heads head
                    WHERE head.entity_id = emb.entity_id
                      AND head.model_id = emb.model_id
                      AND head.dim = emb.dim
                      AND head.embedding_version = emb.embedding_version
               )
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.live_memory m
                    WHERE m.t = emb.entity_id
                      AND m.owner_id = ANY($1)",
        )
        .push(HEADS_ONLY_FILTER)
        .push(
            "
               )
             ORDER BY ",
        )
        .push_text(&vec)
        .push(" OPERATOR(public.<=>) $3")
        .push_text(&cast)
        .push(
            "
             LIMIT ",
        )
        .push(limit);
    sql
}

/// The production candidate scan with its local chunk budget as bind four.
#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
#[must_use]
pub fn candidate_scan_sql_for_tests(dim: EmbeddingDim) -> SqlText {
    candidate_scan_sql(Lane::of(dim), "$4")
}

/// `EXACT_SCAN_RATIO`, for a test that seeds an owner large enough to walk.
#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
pub const EXACT_SCAN_RATIO_FOR_TESTS: u32 = EXACT_SCAN_RATIO;

/// The production exact owner scan, with its target as bind four.
#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
#[must_use]
pub fn best_chunk_scores_sql_for_tests(dim: EmbeddingDim) -> SqlText {
    best_chunk_scores_sql(Lane::of(dim), false, "$4")
}

mod debt;
pub(crate) use debt::{
    DEBT_LEASE_RENEW_INTERVAL, DEBT_LEASE_SECONDS, InFlightIntent, OwnerUnsentScope, PriorDebt,
    SettleDebtError, StagedPublishIntent, drain_debt_with_lease,
    find_in_flight_intents_for_entities, find_in_flight_intents_for_owner,
    invalidate_publish_intents, release_failed_publish_intents, renew_publish_intents,
    resolve_prior_debt_for_takeover, settle_entity_debt, settle_owner_debt, sweep_index,
    sweep_index_bounded,
};
#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
pub use debt::{heads_after_sql_for_tests, heads_first_sql_for_tests, heads_is_head_sql_for_tests};
