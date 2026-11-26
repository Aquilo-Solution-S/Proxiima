use super::*;
use proxiima_core::test_fixtures::owner_scope_for_tests;
use proxiima_core::verbs::query::SearchCursor;
use proxiima_core::{EmbeddingDim, EmbeddingSpace, SpaceVector};
use proxiima_storage_pg::begin_owner_transaction;
use proxiima_storage_pg::test_fixtures::fresh_pg;
use proxiima_storage_pg::vector_index::EXACT_SCAN_RATIO_FOR_TESTS;
use proxiima_storage_pg::verbs::query::set_hnsw_search_sql_for_tests;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn chunk_vector(dim: EmbeddingDim, x: f32, y: f32) -> pgvector::Vector {
    let mut values = vec![0.0; dim.width()];
    values[0] = x;
    values[1] = y;
    pgvector::Vector::from(values)
}

fn chunk_query(dim: EmbeddingDim) -> SpaceVector {
    let mut values = vec![0.0; dim.width()];
    values[0] = 1.0;
    SpaceVector::new(EmbeddingSpace::new("test-embed", dim), values).expect("matching query width")
}

async fn seed_chunks(
    pg: &PgStorage,
    owner: OwnerId,
    t: Uuid,
    vectors: &[pgvector::Vector],
) -> Result<(), proxiima_core::StorageError> {
    let chunks = vectors.iter().map(pgvector::Vector::to_vec).collect();
    create_embedding_of(pg, owner, MemoryId::new(t), "test-embed", chunks).await?;
    Ok(())
}

/// The last chunk alone crosses the score floor. Wide lanes use halfvec
/// candidates but must report the original vector's cosine, not its rounding.
#[tokio::test]
async fn last_chunk_matches_semantic_and_hybrid_with_full_precision_score() -> TestResult {
    let (pg, _guard) = fresh_pg("last_embedding_chunk").await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let target = seed_note(&pg, owner, "Long document", "Unrelated vocabulary").await?;
    let other = seed_note(
        &pg,
        owner,
        "Other document",
        "Another unrelated observation",
    )
    .await?;
    let zero = seed_note(&pg, owner, "Zero vector", "No defined cosine").await?;
    let projection = note_projection();
    for dim in [
        EmbeddingDim::D1024,
        EmbeddingDim::D2048,
        EmbeddingDim::D3072,
    ] {
        seed_chunks(
            &pg,
            owner,
            target,
            &[
                chunk_vector(dim, 0.0, 1.0),
                chunk_vector(dim, 0.6, 0.8),
                chunk_vector(dim, 0.8, 0.6),
            ],
        )
        .await?;
        seed_chunks(
            &pg,
            owner,
            other,
            &[chunk_vector(dim, 0.6, 0.8), chunk_vector(dim, 0.0, 0.0)],
        )
        .await?;
        seed_chunks(&pg, owner, zero, &[chunk_vector(dim, 0.0, 0.0)]).await?;
        for mode in [SearchMode::Semantic, SearchMode::Hybrid] {
            let mut req = search_req(owner, "absent-query-phrase");
            req.mode = mode;
            req.semantic = Some(chunk_query(dim));
            req.semantic_weight = Some(0.75);
            req.min_score = Some(0.75);
            let page = pg
                .search_memories(
                    Some(&owner_scope_for_tests(&[owner])),
                    &req,
                    std::slice::from_ref(&projection),
                )
                .await?;
            assert_eq!(page.results.len(), 1, "{dim:?} {mode:?}: {page:?}");
            let hit = &page.results[0];
            assert_eq!(hit.memory_id.into_inner(), target);
            assert!(
                (hit.similarity_score - 0.8).abs() < 0.000_001,
                "{dim:?}: {hit:?}"
            );
            let expected_score = if mode == SearchMode::Semantic {
                0.8
            } else {
                0.75 / 61.0
            };
            assert!((hit.score - expected_score).abs() < 0.000_001, "{hit:?}");
            assert!(!page.has_more);
        }
    }
    Ok(())
}

/// 130 top chunks from one memory exceed the initial 40-row window and
/// its first expansion. Distinct results and a second page must survive.
#[tokio::test]
async fn chunk_clusters_do_not_spend_distinct_memory_limit_or_cursor() -> TestResult {
    let (pg, _guard) = fresh_pg("clustered_embedding_chunks").await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let dim = EmbeddingDim::D1024;
    let first = seed_note(&pg, owner, "Dominant", "Many embedding fragments").await?;
    seed_chunks(&pg, owner, first, &vec![chunk_vector(dim, 1.0, 0.0); 130]).await?;
    let mut expected = vec![first];
    for (title, x, y) in [
        ("Second", 0.8, 0.6),
        ("Third", 0.6, 0.8),
        ("Fourth", 0.0, 1.0),
    ] {
        let t = seed_note(&pg, owner, title, "Independent memory").await?;
        seed_chunks(&pg, owner, t, &[chunk_vector(dim, x, y)]).await?;
        expected.push(t);
    }
    let projection = note_projection();
    for mode in [SearchMode::Semantic, SearchMode::Hybrid] {
        let mut req = search_req(owner, "absent-query-phrase");
        req.mode = mode;
        req.limit = 2;
        req.semantic = Some(chunk_query(dim));
        let first_page = pg
            .search_memories(
                Some(&owner_scope_for_tests(&[owner])),
                &req,
                std::slice::from_ref(&projection),
            )
            .await?;
        assert_eq!(
            first_page
                .results
                .iter()
                .map(|hit| hit.memory_id.into_inner())
                .collect::<Vec<_>>(),
            expected[..2]
        );
        assert!(first_page.has_more);
        let last = first_page.results.last().expect("full first page");
        req.after = Some(SearchCursor::Relevance {
            score_bits: last.score.to_bits(),
            memory_id: last.memory_id,
            seen: 2,
        });
        let second_page = pg
            .search_memories(
                Some(&owner_scope_for_tests(&[owner])),
                &req,
                std::slice::from_ref(&projection),
            )
            .await?;
        assert_eq!(
            second_page
                .results
                .iter()
                .map(|hit| hit.memory_id.into_inner())
                .collect::<Vec<_>>(),
            expected[2..]
        );
        assert!(!second_page.has_more);
    }
    Ok(())
}

/// A thousand chunks of one memory, all nearer than any other memory's, do
/// not hide the next memory: the exact scan ranks memories, not chunks.
#[tokio::test]
async fn a_thousand_near_chunks_of_one_memory_do_not_hide_the_next() -> TestResult {
    let (pg, _guard) = fresh_pg("embedding_chunk_cap").await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let dim = EmbeddingDim::D1024;
    let first = seed_note(&pg, owner, "Dominant", "One thousand matching fragments").await?;
    seed_chunks(&pg, owner, first, &vec![chunk_vector(dim, 1.0, 0.0); 1_000]).await?;
    let beyond = seed_note(&pg, owner, "Beyond cap", "More distant memory").await?;
    seed_chunks(&pg, owner, beyond, &[chunk_vector(dim, 0.8, 0.6)]).await?;
    let mut req = search_req(owner, "absent-query-phrase");
    req.mode = SearchMode::Semantic;
    req.limit = 2;
    req.semantic = Some(chunk_query(dim));
    let page = pg
        .search_memories(
            Some(&owner_scope_for_tests(&[owner])),
            &req,
            &[note_projection()],
        )
        .await?;
    assert_eq!(
        page.results
            .iter()
            .map(|hit| hit.memory_id.into_inner())
            .collect::<Vec<_>>(),
        [first, beyond]
    );
    assert!(!page.has_more);
    assert_eq!(
        page.candidate_bound,
        proxiima_core::storage_ports::CandidateBound::Unknown,
        "a short nonempty page claims no exhaustion"
    );
    Ok(())
}

/// A unit vector `degrees` from the query, so its cosine score is `cos θ`.
fn angle_vector(dim: EmbeddingDim, degrees: f32) -> pgvector::Vector {
    let radians = degrees.to_radians();
    chunk_vector(dim, radians.cos(), radians.sin())
}

/// What one memory should score: its best chunk at its head embedding
/// version, and whether `heads_only` admits it.
struct Expected {
    t: Uuid,
    degrees: f32,
    head: bool,
}

impl Expected {
    fn score(&self) -> f64 {
        let radians = self.degrees.to_radians();
        let (x, y) = (f64::from(radians.cos()), f64::from(radians.sin()));
        x / x.hypot(y)
    }
}

/// The exact ranking: best score first, ties by id descending.
fn exact_ranking(expected: &[Expected], heads_only: bool, target: usize) -> Vec<(Uuid, f64)> {
    let mut ranked: Vec<_> = expected
        .iter()
        .filter(|memory| memory.head || !heads_only)
        .map(|memory| (memory.t, memory.score()))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
    ranked.truncate(target);
    ranked
}

/// Both ways the candidate function finds the owners' nearest memories, the
/// index walk and the exact scan, return the exact ranking over single-chunk,
/// multi-chunk, superseded and re-embedded memories and another owner's. A
/// walk the index starves falls back to the exact scan, and an owner with no
/// more chunks than the target is scored in one exact pass, without the index.
/// Chunks in another width do not count toward the lane's size.
#[tokio::test]
async fn candidate_paths_return_the_exact_ranking() -> TestResult {
    let (pg, _guard) = fresh_pg("vector_candidate_ranking").await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let other = OwnerId::from(UserId::new(Uuid::now_v7()));
    let dim = EmbeddingDim::D1024;
    let mut expected = Vec::new();
    // A walk for `t` memories visits about t·N/n of the lane's N chunks, so
    // the rule walks for one and two memories once the owner holds more than
    // twice the ratio's worth.
    let singles = u16::try_from(2 * EXACT_SCAN_RATIO_FOR_TESTS + 40)?;
    for step in 0..singles {
        let degrees = 2.05 + 0.2 * f32::from(step);
        let t = seed_note(&pg, owner, &format!("Single {step}"), "One chunk").await?;
        seed_chunks(&pg, owner, t, &[angle_vector(dim, degrees)]).await?;
        expected.push(Expected {
            t,
            degrees,
            head: true,
        });
    }
    for title in ["Tie one", "Tie two"] {
        let t = seed_note(&pg, owner, title, "Same direction").await?;
        seed_chunks(&pg, owner, t, &[angle_vector(dim, 31.0)]).await?;
        expected.push(Expected {
            t,
            degrees: 31.0,
            head: true,
        });
    }
    let chunked = seed_note(&pg, owner, "Chunked", "Three chunks").await?;
    let chunks = [80.0, 0.8, 50.0].map(|degrees| angle_vector(dim, degrees));
    seed_chunks(&pg, owner, chunked, &chunks).await?;
    expected.push(Expected {
        t: chunked,
        degrees: 0.8,
        head: true,
    });
    let old = TestNote::new("Revised", "First wording")
        .create(&pg, owner)
        .await?;
    let old_t = old.memory_id.into_inner();
    seed_chunks(&pg, owner, old_t, &[angle_vector(dim, 0.6)]).await?;
    expected.push(Expected {
        t: old_t,
        degrees: 0.6,
        head: false,
    });
    let new = TestNote::new("Revised", "Second wording")
        .handle(old.handle)
        .create(&pg, owner)
        .await?;
    let new_t = new.memory_id.into_inner();
    seed_chunks(&pg, owner, new_t, &[angle_vector(dim, 70.3)]).await?;
    expected.push(Expected {
        t: new_t,
        degrees: 70.3,
        head: true,
    });
    let reembedded = seed_note(&pg, owner, "Re-embedded", "Two versions").await?;
    seed_chunks(&pg, owner, reembedded, &[angle_vector(dim, 0.5)]).await?;
    seed_chunks(&pg, owner, reembedded, &[angle_vector(dim, 74.2)]).await?;
    expected.push(Expected {
        t: reembedded,
        degrees: 74.2,
        head: true,
    });
    let theirs = seed_note(&pg, other, "Theirs", "Nearest of all").await?;
    seed_chunks(&pg, other, theirs, &[angle_vector(dim, 0.0)]).await?;
    let owner_chunks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM proxiima_core.embeddings WHERE owner_id = $1")
            .bind(owner.into_inner())
            .fetch_one(pg.pool_for_tests())
            .await?;
    // Enough chunks in another width that, counted into this lane's size, the
    // owner's chunks would fit the exact budget for one memory; the walks
    // below show they are not counted.
    let narrow = EmbeddingDim::D384;
    let narrow_t = seed_note(&pg, other, "Narrow", "Another width").await?;
    let count =
        usize::try_from(owner_chunks * owner_chunks / i64::from(EXACT_SCAN_RATIO_FOR_TESTS))?;
    seed_chunks(
        &pg,
        other,
        narrow_t,
        &vec![chunk_vector(narrow, 1.0, 0.0); count],
    )
    .await?;
    sqlx::query("ANALYZE proxiima_core.embeddings")
        .execute(pg.pool_for_tests())
        .await?;

    let runtime = pg.clone_pool_for_backend();
    let scope = owner_scope_for_tests(&[owner]);
    // Nearest first: their chunk, a stale embedding version, a superseded
    // version, then the multi-chunk memory's best chunk; a walk for one
    // memory must pass over the first three. An `ef_search` of 1 without
    // iterative scan lets the index return one chunk at most, so two memories
    // come only from the exact scan.
    for (target, starved) in [
        (1_usize, false),
        (2, true),
        (3, false),
        (40, false),
        (1_000, false),
    ] {
        for heads_only in [true, false] {
            let mut tx = begin_owner_transaction(&runtime, &scope).await?;
            sqlx::raw_sql(set_hnsw_search_sql_for_tests(&PgTuning::default()))
                .execute(tx.as_mut())
                .await?;
            // A table this small sorts faster than the index walks it, and
            // the walk is what this test needs to see.
            sqlx::raw_sql("SET LOCAL enable_sort = off")
                .execute(tx.as_mut())
                .await?;
            if starved {
                sqlx::raw_sql("SET LOCAL hnsw.ef_search = 1; SET LOCAL hnsw.iterative_scan = off")
                    .execute(tx.as_mut())
                    .await?;
            }
            let walked = "SELECT pg_catalog.pg_stat_get_xact_numscans(
                                'proxiima_core.embeddings_hnsw_d1024'::regclass)";
            let before: i64 = sqlx::query_scalar(walked).fetch_one(tx.as_mut()).await?;
            let rows: Vec<(Uuid, f32)> = sqlx::query_as(
                "SELECT memory_id, similarity_score
                   FROM proxiima_core.vector_candidates_d1024($1, 'test-embed', $2, $3, $4)",
            )
            .bind(vec![owner.into_inner()])
            .bind(pgvector::Vector::from(chunk_query(dim).values().to_vec()))
            .bind(i64::try_from(target)?)
            .bind(heads_only)
            .fetch_all(tx.as_mut())
            .await?;
            let after: i64 = sqlx::query_scalar(walked).fetch_one(tx.as_mut()).await?;
            tx.rollback().await?;
            if target <= 2 {
                assert!(after > before, "target {target} walks the index");
            }
            if target == 3 || i64::try_from(target)? >= owner_chunks {
                assert_eq!(
                    after, before,
                    "{owner_chunks} chunks, target {target}: one exact pass"
                );
            }
            let want = exact_ranking(&expected, heads_only, target);
            assert_eq!(
                rows.iter().map(|row| row.0).collect::<Vec<_>>(),
                want.iter().map(|row| row.0).collect::<Vec<_>>(),
                "target {target}, heads_only {heads_only}"
            );
            for ((_, got), (_, score)) in rows.iter().zip(&want) {
                assert!((f64::from(*got) - score).abs() < 1e-6, "{got} != {score}");
            }
        }
    }
    Ok(())
}
