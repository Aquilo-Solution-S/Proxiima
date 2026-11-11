//! Offline retrieval evaluation over authored text and explicit judgments.
//! Fixed feature hashes are fixture encoders, not production embedding models.
//! A floor in `FLOORS` catches a regression of this pipeline on this fixture;
//! it is not a statement about production retrieval quality.

#[path = "retrieval_quality/corpus.rs"]
mod corpus;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use proxiima_core::storage_ports::MemoryReadPort;
use proxiima_core::test_fixtures::owner_scope_for_tests;
use proxiima_core::verbs::query::{
    EntityKind, MemorySearchRequest, MemorySearchScope, SearchMode, SearchOrder,
    SupersessionStatus, TagMatch,
};
use proxiima_core::{
    EmbeddingDim, EmbeddingSpace, MemoryId, OwnerId, SpaceVector, StorageError, UserId,
};
use proxiima_storage_pg::PgStorage;
use proxiima_storage_pg::test_fixtures::{TestNote, create_embedding_of, fresh_pg};
use uuid::Uuid;

use corpus::{NOTES, QUERIES};

type TestResult = Result<(), Box<dyn std::error::Error>>;
const K: usize = 5;

fn owner() -> OwnerId {
    OwnerId::from(UserId::new(Uuid::now_v7()))
}
fn count(value: usize) -> f64 {
    f64::from(u32::try_from(value).expect("bounded fixture count"))
}

#[derive(Clone, Copy, serde::Serialize)]
struct Metrics {
    hits: usize,
    recall: f64,
    reciprocal_rank: f64,
    ndcg: f64,
    covered: bool,
}

fn metrics(relevant: &[Uuid], ranked: &[Uuid], k: usize) -> Metrics {
    let relevant: BTreeSet<_> = relevant.iter().copied().collect();
    let ranks: Vec<_> = ranked
        .iter()
        .take(k)
        .enumerate()
        .filter_map(|(rank, id)| relevant.contains(id).then_some(rank + 1))
        .collect();
    let dcg: f64 = ranks
        .iter()
        .map(|rank| 1.0 / (count(*rank) + 1.0).log2())
        .sum();
    let ideal: f64 = (1..=relevant.len().min(k))
        .map(|rank| 1.0 / (count(rank) + 1.0).log2())
        .sum();
    Metrics {
        hits: ranks.len(),
        recall: if relevant.is_empty() {
            0.0
        } else {
            count(ranks.len()) / count(relevant.len())
        },
        reciprocal_rank: ranks.first().map_or(0.0, |rank| 1.0 / count(*rank)),
        ndcg: if ideal == 0.0 { 0.0 } else { dcg / ideal },
        covered: !ranks.is_empty(),
    }
}

#[test]
fn relevance_and_rank_determine_distinct_metrics() {
    let ids = Uuid::from_u128;
    let actual = metrics(&[ids(1), ids(3)], &[ids(9), ids(1), ids(2), ids(3)], 3);
    assert_eq!(actual.hits, 1);
    assert!((actual.recall - 0.5).abs() < 1e-12);
    assert!((actual.reciprocal_rank - 0.5).abs() < 1e-12);
    let expected = (1.0 / 3.0_f64.log2()) / (1.0 + 1.0 / 3.0_f64.log2());
    assert!((actual.ndcg - expected).abs() < 1e-12);
    assert!(actual.covered);
    let miss = metrics(&[ids(1)], &[ids(9)], 5);
    assert_eq!(
        (miss.hits, miss.recall, miss.reciprocal_rank, miss.ndcg),
        (0, 0.0, 0.0, 0.0)
    );
    assert!(!miss.covered);
}

#[derive(Clone, Copy)]
enum Encoder {
    Words,
    Trigrams,
}
impl Encoder {
    fn space(self) -> EmbeddingSpace {
        match self {
            Self::Words => EmbeddingSpace::new("fixture/fnv-word-v1", EmbeddingDim::D384),
            Self::Trigrams => EmbeddingSpace::new("fixture/fnv-trigram-v1", EmbeddingDim::D768),
        }
    }
    fn encode(self, text: &str) -> SpaceVector {
        let space = self.space();
        let mut values = vec![0.0; space.dim().width()];
        let lowered = text.to_ascii_lowercase();
        let mut add = |unit: &[u8]| {
            let hash = unit
                .iter()
                .fold(14_695_981_039_346_656_037_u64, |hash, byte| {
                    (hash ^ u64::from(*byte)).wrapping_mul(1_099_511_628_211)
                });
            let width = u64::try_from(values.len()).expect("supported width");
            let index = usize::try_from(hash % width).expect("index is below vector width");
            values[index] += 1.0;
        };
        match self {
            Self::Words => {
                for word in lowered
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .filter(|s| !s.is_empty())
                {
                    add(word.as_bytes());
                }
            }
            Self::Trigrams => {
                for unit in lowered.as_bytes().windows(3) {
                    add(unit);
                }
            }
        }
        SpaceVector::new(space, values).expect("fixture encoder supplies declared width")
    }
}

async fn seed_note(
    pg: &PgStorage,
    owner: OwnerId,
    note: &corpus::Note,
) -> Result<Uuid, StorageError> {
    let written = TestNote::new(note.key, note.text)
        .tags(&[note.tag])
        .create(pg, owner)
        .await?;
    Ok(written.memory_id.into_inner())
}

async fn seed_embedding(
    pg: &PgStorage,
    owner: OwnerId,
    entity: Uuid,
    vector: &SpaceVector,
) -> Result<(), StorageError> {
    create_embedding_of(
        pg,
        owner,
        MemoryId::new(entity),
        vector.space().model_id(),
        vec![vector.values().to_vec()],
    )
    .await
    .map(drop)
}

fn request(
    owner: OwnerId,
    query: &str,
    mode: SearchMode,
    semantic: SpaceVector,
    tags: Vec<String>,
) -> MemorySearchRequest {
    MemorySearchRequest {
        scope: MemorySearchScope::Owner(owner),
        read_owners: vec![owner],
        query: query.into(),
        mode,
        supersession: SupersessionStatus::HeadsOnly,
        limit: 5,
        kind: Some(EntityKind::Fact),
        schema_id: None,
        tags,
        tag_match: TagMatch::All,
        since: None,
        until: None,
        order: SearchOrder::Relevance,
        min_score: None,
        semantic_weight: (mode == SearchMode::Hybrid).then_some(0.5),
        after: None,
        semantic: Some(semantic),
    }
}

#[tokio::test]
async fn pg_queries_keep_owner_model_and_tag_admission_separate() -> TestResult {
    let (pg, _guard) = fresh_pg("retrieval_scope").await;
    let a = owner();
    let b = owner();
    let a_words = seed_note(&pg, a, &NOTES[0][0]).await?;
    let a_trigrams = seed_note(&pg, a, &NOTES[0][3]).await?;
    let foreign = seed_note(&pg, b, &NOTES[1][0]).await?;
    let query = "supplier invoice";
    seed_embedding(&pg, a, a_words, &Encoder::Words.encode(query)).await?;
    seed_embedding(&pg, a, a_trigrams, &Encoder::Trigrams.encode(query)).await?;
    seed_embedding(&pg, b, foreign, &Encoder::Words.encode(query)).await?;
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    for (encoder, expected, tag) in [
        (Encoder::Words, a_words, "finance"),
        (Encoder::Trigrams, a_trigrams, "ops"),
    ] {
        let mut req = request(
            a,
            query,
            SearchMode::Semantic,
            encoder.encode(query),
            vec![tag.into()],
        );
        let page = pg
            .search_memories(
                Some(&owner_scope_for_tests(&[a])),
                &req,
                registry.search_projections(),
            )
            .await?;
        assert_eq!(
            page.results
                .iter()
                .map(|hit| hit.memory_id.into_inner())
                .collect::<Vec<_>>(),
            vec![expected]
        );
        req.tags = vec!["no-such-tag".into()];
        assert!(
            pg.search_memories(
                Some(&owner_scope_for_tests(&[a])),
                &req,
                registry.search_projections()
            )
            .await?
            .results
            .is_empty()
        );
    }
    Ok(())
}

#[derive(Default, serde::Serialize)]
struct Aggregate {
    queries: usize,
    judged_relevant: usize,
    retrieved: usize,
    hits: usize,
    recall_at_5: f64,
    mrr: f64,
    ndcg_at_5: f64,
    coverage: f64,
    latency_ms: f64,
}
impl Aggregate {
    fn add(&mut self, measured: Metrics, relevant: usize, retrieved: usize, latency_ms: f64) {
        self.queries += 1;
        self.judged_relevant += relevant;
        self.retrieved += retrieved;
        self.hits += measured.hits;
        self.recall_at_5 += measured.recall;
        self.mrr += measured.reciprocal_rank;
        self.ndcg_at_5 += measured.ndcg;
        self.coverage += f64::from(u8::from(measured.covered));
        self.latency_ms += latency_ms;
    }
    fn finish(&mut self) {
        let divisor = count(self.queries);
        self.recall_at_5 /= divisor;
        self.mrr /= divisor;
        self.ndcg_at_5 /= divisor;
        self.coverage /= divisor;
        self.latency_ms /= divisor;
    }
}

type Corpus = BTreeMap<Uuid, (usize, &'static corpus::Note)>;

async fn seed_corpus(pg: &PgStorage, owners: &[OwnerId; 2]) -> Result<Corpus, StorageError> {
    let mut documents = BTreeMap::new();
    for (owner_index, notes) in NOTES.iter().enumerate() {
        for note in notes {
            let id = seed_note(pg, owners[owner_index], note).await?;
            for encoder in [Encoder::Words, Encoder::Trigrams] {
                seed_embedding(pg, owners[owner_index], id, &encoder.encode(note.text)).await?;
            }
            documents.insert(id, (owner_index, note));
        }
    }
    Ok(documents)
}

/// Per-stratum floors: recall@5, MRR, nDCG@5. Each is the measured value
/// rounded down to two decimals, so only float noise is tolerated. Latency is
/// wall-clock and coverage is bounded by recall, so neither has a floor.
#[rustfmt::skip]
const FLOORS: [(&str, [f64; 3]); 16] = [
    ("owner-0/fixture/fnv-trigram-v1/768/hybrid/tag-selective",  [1.00, 1.00, 1.00]),
    ("owner-0/fixture/fnv-trigram-v1/768/hybrid/unfiltered",     [1.00, 1.00, 1.00]),
    ("owner-0/fixture/fnv-trigram-v1/768/semantic/tag-selective",[1.00, 1.00, 1.00]),
    ("owner-0/fixture/fnv-trigram-v1/768/semantic/unfiltered",   [1.00, 1.00, 0.98]),
    ("owner-0/fixture/fnv-word-v1/384/hybrid/tag-selective",     [1.00, 1.00, 0.98]),
    ("owner-0/fixture/fnv-word-v1/384/hybrid/unfiltered",        [0.91, 1.00, 0.93]),
    ("owner-0/fixture/fnv-word-v1/384/semantic/tag-selective",   [1.00, 1.00, 0.98]),
    ("owner-0/fixture/fnv-word-v1/384/semantic/unfiltered",      [0.91, 1.00, 0.92]),
    ("owner-1/fixture/fnv-trigram-v1/768/hybrid/tag-selective",  [1.00, 1.00, 1.00]),
    ("owner-1/fixture/fnv-trigram-v1/768/hybrid/unfiltered",     [1.00, 1.00, 1.00]),
    ("owner-1/fixture/fnv-trigram-v1/768/semantic/tag-selective",[1.00, 1.00, 1.00]),
    ("owner-1/fixture/fnv-trigram-v1/768/semantic/unfiltered",   [1.00, 1.00, 1.00]),
    ("owner-1/fixture/fnv-word-v1/384/hybrid/tag-selective",     [1.00, 1.00, 1.00]),
    ("owner-1/fixture/fnv-word-v1/384/hybrid/unfiltered",        [1.00, 1.00, 1.00]),
    ("owner-1/fixture/fnv-word-v1/384/semantic/tag-selective",   [1.00, 0.91, 0.93]),
    ("owner-1/fixture/fnv-word-v1/384/semantic/unfiltered",      [1.00, 0.91, 0.93]),
];
const FLOOR_NOISE: f64 = 1e-9;

fn assert_floors(strata: &BTreeMap<String, Aggregate>) {
    let mut violations = Vec::new();
    for (key, aggregate) in strata {
        let Some((_, floors)) = FLOORS.iter().find(|(floor_key, _)| floor_key == key) else {
            violations.push(format!("{key}: measured but has no floor"));
            continue;
        };
        for ((metric, measured), floor) in [
            ("recall@5", aggregate.recall_at_5),
            ("mrr", aggregate.mrr),
            ("ndcg@5", aggregate.ndcg_at_5),
        ]
        .into_iter()
        .zip(floors)
        {
            if measured < floor - FLOOR_NOISE {
                violations.push(format!("{key}: {metric} {measured} below floor {floor}"));
            }
        }
    }
    for (key, _) in FLOORS {
        if !strata.contains_key(key) {
            violations.push(format!("{key}: has a floor but the run did not produce it"));
        }
    }
    assert!(
        violations.is_empty(),
        "retrieval quality floors violated:\n{}",
        violations.join("\n")
    );
}

#[tokio::test]
async fn held_out_authored_text_meets_its_floors_by_stratum() -> TestResult {
    let (pg, _guard) = fresh_pg("retrieval_quality").await;
    let owners = [owner(), owner()];
    let documents = seed_corpus(&pg, &owners).await?;
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let mut strata = BTreeMap::<String, Aggregate>::new();
    let mut observations = Vec::new();
    for (query_index, query) in QUERIES.iter().enumerate() {
        for encoder in [Encoder::Words, Encoder::Trigrams] {
            for (mode, mode_name) in [
                (SearchMode::Semantic, "semantic"),
                (SearchMode::Hybrid, "hybrid"),
            ] {
                for filtered in [false, true] {
                    let tags = if filtered {
                        vec![query.tag.into()]
                    } else {
                        Vec::new()
                    };
                    let req = request(
                        owners[query.owner],
                        query.text,
                        mode,
                        encoder.encode(query.text),
                        tags,
                    );
                    let relevant: Vec<_> = documents
                        .iter()
                        .filter_map(|(id, (owner_index, note))| {
                            (*owner_index == query.owner
                                && query.relevant.contains(&note.key)
                                && (!filtered || note.tag == query.tag))
                                .then_some(*id)
                        })
                        .collect();
                    let started = Instant::now();
                    let page = pg
                        .search_memories(
                            Some(&owner_scope_for_tests(&[owners[query.owner]])),
                            &req,
                            registry.search_projections(),
                        )
                        .await?;
                    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
                    let ranked: Vec<_> = page
                        .results
                        .iter()
                        .map(|result| result.memory_id.into_inner())
                        .collect();
                    for id in &ranked {
                        let (owner_index, note) =
                            documents.get(id).expect("returned a fixture memory");
                        assert_eq!(
                            *owner_index, query.owner,
                            "owner admission for query {query_index}"
                        );
                        assert!(
                            !filtered || note.tag == query.tag,
                            "tag admission for query {query_index}"
                        );
                    }
                    let measured = metrics(&relevant, &ranked, K);
                    let space = encoder.space();
                    let filter_name = if filtered {
                        "tag-selective"
                    } else {
                        "unfiltered"
                    };
                    let key = format!(
                        "owner-{}/{}/{}/{mode_name}/{filter_name}",
                        query.owner,
                        space.model_id(),
                        space.dim().width()
                    );
                    strata.entry(key).or_default().add(
                        measured,
                        relevant.len(),
                        ranked.len(),
                        latency_ms,
                    );
                    observations.push(serde_json::json!({"query":query_index,"owner":query.owner,"model":space.model_id(),"dim":space.dim().width(),"mode":mode_name,"filter":filter_name,"judged_relevant":relevant.len(),"retrieved":ranked.len(),"metrics":measured,"latency_ms":latency_ms}));
                }
            }
        }
    }
    for aggregate in strata.values_mut() {
        aggregate.finish();
    }
    println!(
        "RETRIEVAL_BASELINE {}",
        serde_json::json!({
            "fixture":"authored-text-v1", "corpus_documents":documents.len(), "held_out_queries":QUERIES.len(),
            "executed_queries":observations.len(), "k":K, "strata":strata, "observations":observations,
            "encoder":"fixed offline word/trigram feature hashes; no training", "production_quality_claim":false,
        })
    );
    assert_floors(&strata);
    Ok(())
}

#[tokio::test]
async fn pg_canary_reports_multiple_spaces_and_partial_sampling() -> TestResult {
    let (pg, _guard) = fresh_pg("retrieval_canary_strata").await;
    assert!(
        pg.embedding_ann_observability()
            .await?
            .recall_canary
            .is_none()
    );
    for number in 1..=34_u128 {
        let selected = OwnerId::from(UserId::new(Uuid::from_u128(number)));
        let id = seed_note(&pg, selected, &NOTES[0][0]).await?;
        seed_embedding(&pg, selected, id, &Encoder::Words.encode(NOTES[0][0].text)).await?;
        if number <= 2 {
            seed_embedding(
                &pg,
                selected,
                id,
                &Encoder::Trigrams.encode(NOTES[0][0].text),
            )
            .await?;
        }
    }
    let canary = pg
        .embedding_ann_observability()
        .await?
        .recall_canary
        .expect("nonempty strata");
    assert_eq!(canary.available_strata, 36);
    assert_eq!(
        canary.sample_count, 32,
        "comparison work stops at its declared cap"
    );
    assert_eq!(canary.strata.len(), 2);
    assert_eq!(
        canary
            .strata
            .iter()
            .map(|stratum| stratum.sample_count)
            .sum::<u64>(),
        32
    );
    assert!(canary.strata.iter().any(|stratum| stratum.model_id
        == Encoder::Words.space().model_id()
        && stratum.dim == 384));
    assert!(canary.strata.iter().any(|stratum| stratum.model_id
        == Encoder::Trigrams.space().model_id()
        && stratum.dim == 768));
    Ok(())
}
