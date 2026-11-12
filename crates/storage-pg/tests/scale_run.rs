//! Scale measurements: filtered vector search under owner scope, and
//! whole-owner erase. Each test prints one JSON line and measures; it is not a
//! benchmark gate.
//!
//! Both tests run SMALL by default (seconds), so the gates exercise the
//! harness and it cannot rot. The real scale run sets the size through the
//! environment and runs outside the gates (nextest kills a test after 300 s):
//!
//! ```text
//! PROXIIMA_SCALE_EMBEDDINGS=200000 PROXIIMA_SCALE_QUERIES=50 \
//! PROXIIMA_SCALE_ERASE_MEMORIES=11000 \
//!   cargo test -p proxiima-storage-pg --test scale_run --all-features \
//!     -- --nocapture --test-threads=1
//! ```
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `PROXIIMA_SCALE_EMBEDDINGS` | 3000 | embeddings in the search corpus |
//! | `PROXIIMA_SCALE_QUERIES` | 5 | queries per owner and tuning setting |
//! | `PROXIIMA_SCALE_ERASE_MEMORIES` | 500 | Facts of the owner that is erased |
//!
//! The search vectors are SYNTHETIC: a few fixed cluster centers plus seeded
//! noise, normalized. They are not text embeddings, so a recall figure here
//! says how pgvector's filtered HNSW scan behaves on this geometry, not how
//! retrieval quality looks on real text. The JSON line says so.
//!
//! Each query is one production `search_memories` call, so the candidate
//! window is the one the search asks for. The candidate function answers from
//! an exact owner scan when the index scan returns too few rows; the test
//! cannot see that from outside. A cell with recall 1.0 and a latency far
//! above its neighbors is that scan.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use proxiima_core::owner_inverse::{
    EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
};
use proxiima_core::storage_ports::{MemoryReadPort, OwnerInversePort};
use proxiima_core::test_fixtures::owner_scope_for_tests;
use proxiima_core::verbs::query::{
    EntityKind, MemorySearchRequest, MemorySearchScope, SearchMode, SearchOrder,
    SupersessionStatus, TagMatch,
};
use proxiima_core::{EmbeddingDim, EmbeddingSpace, OwnerId, SpaceVector, UserId};
use proxiima_pg_testkit::split_role_urls;
use proxiima_storage_pg::test_fixtures::{create_user, fresh_pg};
use proxiima_storage_pg::{HnswIterativeScan, PgPlatformScope, PgPoolConfig, PgStorage, PgTuning};
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const MODEL: &str = "fixture/scale-synthetic-v1";
const DIM: usize = 384;
const K: usize = 10;
/// Owner shares of the corpus, in percent: small, medium, large. The large
/// owner takes the remainder, so the shares always add up to the corpus size.
const SHARES_PERCENT: [usize; 3] = [1, 10, 89];
const CLUSTERS: u64 = 16;
/// How far a point sits from its cluster center, relative to the center.
const NOISE: f32 = 0.8;
const CORPUS_SEED: u64 = 0x5eed_0001;
const QUERY_SEED: u64 = 0x5eed_0002;
const EF_SEARCH: [u32; 3] = [40, 100, 400];
const ITERATIVE: [HnswIterativeScan; 3] = [
    HnswIterativeScan::Off,
    HnswIterativeScan::StrictOrder,
    HnswIterativeScan::RelaxedOrder,
];
/// Floor for recall@10 at the default tuning, every owner, every size: just
/// under the lowest value measured at 200,000 embeddings (0.864, the largest
/// owner, one run of 50 queries).
const DEFAULT_RECALL_FLOOR: f64 = 0.85;
const EMBED_BATCH: usize = 1_000;

fn env_count(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Err(_) => default,
        Ok(value) => value
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("{name}={value} is not a count")),
    }
}

// ---- synthetic vectors -----------------------------------------------------

/// xorshift64*: small, seeded, deterministic.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in (0, 1), 16 bits fine.
    fn unit(&mut self) -> f32 {
        let bits = u16::try_from(self.next_u64() >> 48).expect("16 bits");
        (f32::from(bits) + 0.5) / 65_536.0
    }

    /// Standard normal, Box-Muller.
    fn gaussian(&mut self) -> f32 {
        let (u, v) = (self.unit(), self.unit());
        (-2.0 * u.ln()).sqrt() * (std::f32::consts::TAU * v).cos()
    }
}

fn normalized(mut values: Vec<f32>) -> Vec<f32> {
    let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    for value in &mut values {
        *value /= norm;
    }
    values
}

fn gaussian_unit(rng: &mut Rng) -> Vec<f32> {
    normalized((0..DIM).map(|_| rng.gaussian()).collect())
}

/// The cluster centers: fixed, shared by the corpus and the queries.
fn centers() -> Vec<Vec<f32>> {
    let mut rng = Rng::new(0x00ce_47e5);
    (0..CLUSTERS).map(|_| gaussian_unit(&mut rng)).collect()
}

/// One unit vector near a randomly chosen center.
fn point(rng: &mut Rng, centers: &[Vec<f32>]) -> Vec<f32> {
    let pick = usize::try_from(rng.next_u64() % CLUSTERS).expect("cluster index");
    let noise = gaussian_unit(rng);
    normalized(
        centers[pick]
            .iter()
            .zip(&noise)
            .map(|(center, noise)| center + NOISE * noise)
            .collect(),
    )
}

fn vector_text(values: &[f32]) -> String {
    let mut text = String::with_capacity(values.len() * 9 + 2);
    text.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        write!(text, "{value:.6}").expect("write to String");
    }
    text.push(']');
    text
}

// ---- seeding ---------------------------------------------------------------

fn new_owner() -> (UserId, OwnerId) {
    let user = UserId::new(Uuid::now_v7());
    (user, OwnerId::from(user))
}

/// Memories per seeding transaction. The memory pin check takes one advisory
/// lock per inserted row until the transaction ends, so a single statement of
/// a hundred thousand rows exhausts the lock table (measured by this file);
/// the seed stays under it by committing in batches.
const FACT_BATCH: usize = 2_000;

/// What a seeded Fact looks like.
#[derive(Clone, Copy)]
enum Shape {
    /// A bare Fact: head and memory, no sidecar. The erase test's rows.
    Plain,
    /// A `core/agent-note-v1` Fact with its sidecar and projection row, the
    /// shape `search_memories` admits.
    Note,
}

/// `count` live Facts of `shape` for `owner`, one handle and one version
/// each; returns their `t`, in ascending order.
///
/// Statements per batch, not one: `memory_align_head` is a BEFORE INSERT
/// trigger that reads `memory_head` back, and every CTE of one statement
/// reads the same snapshot, so heads written in a sibling CTE are invisible
/// to it. A note's sidecar rows follow the memory row they are declared by,
/// in the same transaction, because referential integrity is deferred.
async fn seed_facts(
    pool: &PgPool,
    owner: OwnerId,
    count: usize,
    shape: Shape,
) -> sqlx::Result<Vec<Uuid>> {
    let owner_id = owner.into_inner();
    let (schema, sidecars): (&str, Vec<&str>) = match shape {
        Shape::Plain => ("core/test-fact-v1", Vec::new()),
        Shape::Note => ("core/agent-note-v1", vec!["proxiima_core.agent_note_v1"]),
    };
    let mut remaining = count;
    while remaining > 0 {
        let batch = remaining.min(FACT_BATCH);
        remaining -= batch;
        // Raw INSERT on purpose: bulk volume, a batch of heads in one
        // statement; the write path admits one Fact per call.
        sqlx::query(
            "INSERT INTO proxiima_core.memory_head (handle, kind, schema_id, owner_id, t)
             SELECT uuidv7(), 'fact', $3, $1, uuidv7()
               FROM generate_series(1, $2)",
        )
        .bind(owner_id)
        .bind(i64::try_from(batch).expect("batch fits"))
        .bind(schema)
        .execute(pool)
        .await?;
        // The stamp and the rows it promises land in one transaction.
        let mut stamped = pool.begin().await?;
        // Raw INSERT on purpose: bulk volume, the admissions of the heads
        // above; the write path admits one Fact per call.
        sqlx::query(
            "INSERT INTO proxiima_core.memory
                 (handle, t, kind, owner_id, schema_id, sidecar_tables, occurred_at, \"by\", via)
             SELECT h.handle, h.t, 'fact', $1, $2,
                    $3, uuid_extract_timestamp(h.t), '00000000-0000-0000-0000-000000000509'::uuid, NULL
               FROM proxiima_core.memory_head h
              WHERE h.owner_id = $1
                AND NOT EXISTS (SELECT 1 FROM proxiima_core.memory m WHERE m.t = h.t)",
        )
        .bind(owner_id)
        .bind(schema)
        .bind(&sidecars)
        .execute(&mut *stamped)
        .await?;
        if matches!(shape, Shape::Note) {
            // Raw INSERT on purpose: bulk volume, the sidecar and search
            // projection rows of the notes above; the write path admits one
            // note per call.
            sqlx::query(
                "WITH ids AS MATERIALIZED (
                     SELECT m.t, row_number() OVER (ORDER BY m.t) AS n
                       FROM proxiima_core.memory m
                      WHERE m.owner_id = $1
                        AND NOT EXISTS (
                                SELECT 1 FROM proxiima_core.agent_note_v1 s WHERE s.t = m.t)
                 ), notes AS (
                     INSERT INTO proxiima_core.agent_note_v1 (t, note_id, title, body, tags)
                     SELECT t, uuidv7(), 'synthetic note ' || n, 'synthetic body ' || n, '{}'
                       FROM ids
                     RETURNING t
                 )
                 INSERT INTO proxiima_core.projection (memory_id, schema_id, owner_id, search_tsv)
                 SELECT t, 'core/agent-note-v1', $1,
                        to_tsvector('english', 'synthetic note ' || n)
                   FROM ids",
            )
            .bind(owner_id)
            .execute(&mut *stamped)
            .await?;
        }
        stamped.commit().await?;
    }
    sqlx::query_scalar("SELECT t FROM proxiima_core.memory WHERE owner_id = $1 ORDER BY t")
        .bind(owner_id)
        .fetch_all(pool)
        .await
}

/// One synthetic chunk per memory, then each memory's embedding head.
///
/// Rows are inserted into the live HNSW index one at a time, as production
/// writes them, not built afterwards: the index under test has the shape the
/// write path gives it. The vectors come from Rust, in batches of text casts.
async fn seed_embeddings(
    pool: &PgPool,
    owner: OwnerId,
    memories: &[Uuid],
    rng: &mut Rng,
    centers: &[Vec<f32>],
) -> sqlx::Result<()> {
    for batch in memories.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch
            .iter()
            .map(|_| vector_text(&point(rng, centers)))
            .collect();
        // Raw INSERT on purpose: bulk volume; the embedding write path stores
        // one entity per call.
        sqlx::query(
            "INSERT INTO proxiima_core.embeddings
                 (entity_id, model_id, embedding_version, owner_id, dim, chunk_ordinal, vec)
             SELECT u.t, $1, 1, $2, $3, 0, u.v::public.vector
               FROM unnest($4::uuid[], $5::text[]) AS u(t, v)",
        )
        .bind(MODEL)
        .bind(owner.into_inner())
        .bind(i16::try_from(DIM).expect("dimension fits"))
        .bind(batch)
        .bind(&texts)
        .execute(pool)
        .await?;
    }
    // Raw INSERT on purpose: bulk volume, the head of every chunk above;
    // the write path promotes a version per entity.
    sqlx::query(
        "INSERT INTO proxiima_core.embedding_heads
             (entity_id, model_id, embedding_version, owner_id, dim)
         SELECT entity_id, model_id, embedding_version, owner_id, dim
           FROM proxiima_core.embeddings
          WHERE owner_id = $1 AND model_id = $2",
    )
    .bind(owner.into_inner())
    .bind(MODEL)
    .execute(pool)
    .await?;
    Ok(())
}

async fn setting(pool: &PgPool, name: &str) -> sqlx::Result<String> {
    sqlx::query_scalar("SELECT current_setting($1)")
        .bind(name)
        .fetch_one(pool)
        .await
}

// ---- test A: filtered vector search ----------------------------------------

/// The exact top `K` of `owner` for `query`, by a sequential scan with every
/// index path off, through the recall canary's joins plus the live-memory
/// join the production candidate path applies.
async fn exact_top(pool: &PgPool, owner: OwnerId, query: &[f32]) -> sqlx::Result<BTreeSet<Uuid>> {
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(
        "SET LOCAL enable_indexscan = off;
         SET LOCAL enable_indexonlyscan = off;
         SET LOCAL enable_bitmapscan = off",
    )
    .execute(&mut *tx)
    .await?;
    let rows: Vec<Uuid> = sqlx::query_scalar(
        "SELECT emb.entity_id
           FROM proxiima_core.embeddings emb
           JOIN proxiima_core.embedding_heads head
             ON head.entity_id = emb.entity_id
            AND head.model_id = emb.model_id
            AND head.dim = emb.dim
            AND head.embedding_version = emb.embedding_version
           JOIN proxiima_core.live_memory m ON m.t = emb.entity_id
          WHERE emb.model_id = $1
            AND emb.owner_id = $2
            AND emb.dim = 384
          ORDER BY emb.vec::public.vector(384) OPERATOR(public.<=>) $3::public.vector(384),
                   emb.entity_id
          LIMIT $4",
    )
    .bind(MODEL)
    .bind(owner.into_inner())
    .bind(vector_text(query))
    .bind(i64::try_from(K).expect("K fits"))
    .fetch_all(&mut *tx)
    .await?;
    tx.rollback().await?;
    Ok(rows.into_iter().collect())
}

struct Timed {
    found: BTreeSet<Uuid>,
    millis: f64,
}

/// A serving handle on `database` whose HNSW session settings are `tuning`,
/// built as `connect_for_tests` builds one, on the already-migrated database.
async fn handle_with(
    database: &str,
    tuning: PgTuning,
) -> Result<PgStorage, Box<dyn std::error::Error>> {
    let (runtime_url, platform_url) = split_role_urls(database).await?;
    let platform_pool = PgPool::connect(&platform_url).await?;
    let platform_scope = PgPlatformScope::new(platform_pool, &["proxiima_core"]).await?;
    Ok(PgStorage::connect_with_config(
        &proxiima_pg_testkit::through_test_pooler(&runtime_url),
        PgPoolConfig::default(),
        tuning,
        platform_scope,
        1,
    )
    .await?)
}

fn search_request(owner: OwnerId, query: &[f32]) -> MemorySearchRequest {
    let space = EmbeddingSpace::new(MODEL, EmbeddingDim::D384);
    MemorySearchRequest {
        scope: MemorySearchScope::Owner(owner),
        read_owners: vec![owner],
        query: "unused: semantic mode ranks by the vector".into(),
        mode: SearchMode::Semantic,
        supersession: SupersessionStatus::HeadsOnly,
        limit: u32::try_from(K).expect("K fits"),
        kind: Some(EntityKind::Fact),
        schema_id: None,
        tags: Vec::new(),
        tag_match: TagMatch::Any,
        since: None,
        until: None,
        order: SearchOrder::Relevance,
        min_score: None,
        semantic_weight: None,
        after: None,
        semantic: Some(
            SpaceVector::new(space, query.to_vec()).expect("the query has the declared width"),
        ),
    }
}

/// One production `search_memories` call for `owner`, on a handle carrying
/// the tuning under test.
async fn production_search(
    pg: &PgStorage,
    owner: OwnerId,
    query: &[f32],
) -> Result<Timed, Box<dyn std::error::Error>> {
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let request = search_request(owner, query);
    let started = Instant::now();
    let page = pg
        .search_memories(
            Some(&owner_scope_for_tests(&[owner])),
            &request,
            registry.search_projections(),
        )
        .await?;
    let millis = started.elapsed().as_secs_f64() * 1_000.0;
    Ok(Timed {
        found: page
            .results
            .iter()
            .map(|hit| hit.memory_id.into_inner())
            .collect(),
        millis,
    })
}

fn count(value: usize) -> f64 {
    f64::from(u32::try_from(value).expect("bounded count"))
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / count(values.len())
}

fn p95(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (sorted.len() * 95).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn round3(value: f64) -> f64 {
    (value * 1_000.0).round() / 1_000.0
}

#[tokio::test]
async fn filtered_vector_search_recall_and_latency_by_owner_share() -> TestResult {
    let total = env_count("PROXIIMA_SCALE_EMBEDDINGS", 3_000);
    let queries = env_count("PROXIIMA_SCALE_QUERIES", 5);
    assert!(
        total >= 100 && queries > 0,
        "corpus and queries must be sized"
    );
    let (pg, database) = fresh_pg("scale_run_search").await;
    let admin = pg.pool_for_tests().clone();
    let centers = centers();

    let small = total * SHARES_PERCENT[0] / 100;
    let medium = total * SHARES_PERCENT[1] / 100;
    let owners = [
        (SHARES_PERCENT[0], small),
        (SHARES_PERCENT[1], medium),
        (SHARES_PERCENT[2], total - small - medium),
    ]
    .map(|(share, rows)| (share, rows, new_owner()));
    let seeding = Instant::now();
    let mut rng = Rng::new(CORPUS_SEED);
    for (_, rows, (user, owner)) in &owners {
        create_user(&admin, *user).await?;
        let memories = seed_facts(&admin, *owner, *rows, Shape::Note).await?;
        seed_embeddings(&admin, *owner, &memories, &mut rng, &centers).await?;
    }
    sqlx::query("ANALYZE proxiima_core.embeddings")
        .execute(&admin)
        .await?;
    let seeding_secs = seeding.elapsed().as_secs_f64();

    let mut query_rng = Rng::new(QUERY_SEED);
    let query_vectors: Vec<Vec<f32>> = (0..queries)
        .map(|_| point(&mut query_rng, &centers))
        .collect();

    // One serving handle per tuning setting: the handle carries the tuning.
    let mut handles = Vec::new();
    for ef_search in EF_SEARCH {
        for iterative in ITERATIVE {
            let tuning = PgTuning {
                hnsw_ef_search: ef_search,
                hnsw_iterative_scan: iterative,
                ..PgTuning::default()
            };
            handles.push((tuning, handle_with(database.name(), tuning).await?));
        }
    }

    let mut grid = Vec::new();
    let mut below_floor = Vec::new();
    for (share, rows, (_, owner)) in &owners {
        let mut exact = Vec::with_capacity(queries);
        for query in &query_vectors {
            exact.push(exact_top(&admin, *owner, query).await?);
        }
        for (tuning, handle) in &handles {
            // One untimed query settles plan and cache state.
            production_search(handle, *owner, &query_vectors[0]).await?;
            let (mut recalls, mut millis) = (Vec::new(), Vec::new());
            for (query, exact) in query_vectors.iter().zip(&exact) {
                let timed = production_search(handle, *owner, query).await?;
                recalls.push(count(timed.found.intersection(exact).count()) / count(exact.len()));
                millis.push(timed.millis);
            }
            let is_default = *tuning == PgTuning::default();
            let recall = mean(&recalls);
            if is_default && recall < DEFAULT_RECALL_FLOOR {
                below_floor.push(format!(
                    "owner share {share}% ({rows} rows): recall@{K} {recall:.3}"
                ));
            }
            grid.push(serde_json::json!({
                "owner_share_percent": share,
                "owner_rows": rows,
                "ef_search": tuning.hnsw_ef_search,
                "iterative_scan": format!("{:?}", tuning.hnsw_iterative_scan),
                "default_tuning": is_default,
                "recall_at_k": round3(recall),
                "latency_ms_mean": round3(mean(&millis)),
                "latency_ms_p95": round3(p95(&millis)),
            }));
        }
    }
    let pgvector_version: String =
        sqlx::query_scalar("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
            .fetch_one(&admin)
            .await?;
    println!(
        "SCALE_RUN_SEARCH {}",
        serde_json::json!({
            "corpus_embeddings": total,
            "dimension": DIM,
            "model_id": MODEL,
            "shares_percent": SHARES_PERCENT,
            "clusters": CLUSTERS,
            "k": K,
            "queries": queries,
            "seeding_seconds": round3(seeding_secs),
            "max_connections": setting(&admin, "max_connections").await?,
            "pgvector_version": pgvector_version,
            "hnsw_max_scan_tuples": PgTuning::default().hnsw_max_scan_tuples,
            "grid": grid,
            "synthetic_vectors": true,
            "production_quality_claim": false,
        })
    );
    // After the line, so a failing size still reports what it measured.
    assert!(
        below_floor.is_empty(),
        "default tuning is under the recall floor {DEFAULT_RECALL_FLOOR}: {below_floor:?}"
    );
    Ok(())
}

// ---- test B: whole-owner erase ---------------------------------------------

/// Advisory locks held in this database by every backend but the caller's.
/// The test database has no other session, so this is the erase.
async fn advisory_locks_of_other_backends(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_locks
          WHERE locktype = 'advisory'
            AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
            AND pid <> pg_backend_pid()",
    )
    .fetch_one(pool)
    .await
}

#[tokio::test]
async fn whole_owner_erase_lock_use_and_wall_time() -> TestResult {
    let memories = env_count("PROXIIMA_SCALE_ERASE_MEMORIES", 500);
    let survivors = 50;
    assert!(memories > 0, "the erased owner needs Facts");
    let (pg, _database) = fresh_pg("scale_run_erase").await;
    let admin = pg.pool_for_tests().clone();
    let (user, owner) = new_owner();
    let (neighbour_user, neighbour) = new_owner();
    create_user(&admin, user).await?;
    create_user(&admin, neighbour_user).await?;
    seed_facts(&admin, owner, memories, Shape::Plain).await?;
    seed_facts(&admin, neighbour, survivors, Shape::Plain).await?;

    // The sampler runs on its own connection for the whole erase.
    let done = Arc::new(AtomicBool::new(false));
    let max_advisory = Arc::new(AtomicI64::new(0));
    let failed_samples = Arc::new(AtomicI64::new(0));
    let sampler = tokio::spawn({
        let (admin, done) = (admin.clone(), done.clone());
        let (max_advisory, failed_samples) = (max_advisory.clone(), failed_samples.clone());
        async move {
            while !done.load(Ordering::Relaxed) {
                // A sample can fail for the reason under test: the erase
                // has filled the lock table. Count it instead of stopping.
                match advisory_locks_of_other_backends(&admin).await {
                    Ok(advisory) => {
                        max_advisory.fetch_max(advisory, Ordering::Relaxed);
                    }
                    Err(_) => {
                        failed_samples.fetch_add(1, Ordering::Relaxed);
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    });
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );
    let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
        user_id: user,
        drop_event_id: "scale-run".into(),
    });
    let started = Instant::now();
    let outcome = pg.erase_personal_owner(&auth, user, &surfaces).await;
    let wall_ms = started.elapsed().as_millis();
    done.store(true, Ordering::Relaxed);
    sampler.await?;
    let (advisory, failed_samples) = (
        max_advisory.load(Ordering::Relaxed),
        failed_samples.load(Ordering::Relaxed),
    );
    let outcome = outcome.unwrap_or_else(|error| {
        panic!(
            "owner erase of {memories} memories failed after {wall_ms} ms \
             (max advisory locks observed {advisory}, failed samples {failed_samples}): {error}"
        )
    });
    assert!(
        matches!(outcome, OwnerEraseOutcome::Completed { .. }),
        "the erase of {memories} memories did not complete: {outcome:?}"
    );

    let left: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM proxiima_core.memory_including_forgotten WHERE owner_id = $1",
    )
    .bind(owner.into_inner())
    .fetch_one(&admin)
    .await?;
    assert_eq!(left, 0, "the erased owner's memories are gone");
    let neighbour_rows: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM proxiima_core.memory_including_forgotten WHERE owner_id = $1),
                (SELECT count(*) FROM proxiima_core.memory_head WHERE owner_id = $1)",
    )
    .bind(neighbour.into_inner())
    .fetch_one(&admin)
    .await?;
    let expected = i64::try_from(survivors).expect("count fits");
    assert_eq!(
        neighbour_rows,
        (expected, expected),
        "the other owner is untouched"
    );

    let max_locks_per_transaction = setting(&admin, "max_locks_per_transaction").await?;
    let max_connections = setting(&admin, "max_connections").await?;
    let max_prepared_transactions = setting(&admin, "max_prepared_transactions").await?;
    let capacity: i64 = sqlx::query_scalar(
        "SELECT current_setting('max_locks_per_transaction')::bigint
              * (current_setting('max_connections')::bigint
                 + current_setting('max_prepared_transactions')::bigint)",
    )
    .fetch_one(&admin)
    .await?;
    println!(
        "SCALE_RUN_ERASE {}",
        serde_json::json!({
            "memories": memories,
            "completed": true,
            "max_advisory_locks_observed": advisory,
            "failed_lock_samples": failed_samples,
            "advisory_locks_per_memory": round3(f64::from(u32::try_from(advisory).expect("bounded")) / count(memories)),
            "wall_ms": wall_ms,
            "max_locks_per_transaction": max_locks_per_transaction,
            "max_connections": max_connections,
            "max_prepared_transactions": max_prepared_transactions,
            "nominal_lock_table_capacity": capacity,
            "sample_interval_ms": 5,
        })
    );
    Ok(())
}
