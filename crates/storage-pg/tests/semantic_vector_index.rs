//! Semantic retrieval with an independent in-memory index and real owner RLS.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use proxiima_core::storage_ports::{
    CandidateBound, EmbeddingJobPort, EmbeddingWrite, EmbeddingWritePort, EmbeddingWriteProof,
    InMemoryVectorIndex, MemoryReadPort, VectorEntryPage, VectorIndex, VectorKey,
    VectorMetadataTarget, VectorPut, VectorPutBatchError,
};
use proxiima_core::test_fixtures::owner_scope_for_tests;
use proxiima_core::verbs::query::{
    EntityKind, MemorySearchRequest, SearchMode, SearchOrder, SupersessionStatus, TagMatch,
};
use proxiima_core::{
    AccessCeiling, EmbeddableEntityRef, EmbeddingDim, EmbeddingSpace, GoalAccess, MemoryId,
    OwnerId, Role, SchemaId, SpaceVector, StorageError, UserId,
};
use proxiima_storage_pg::test_fixtures::{
    TestDerived, TestGoal, TestNote, create_embedding_of, create_user, enqueue_embedding_job,
    fresh_pg_host_index, insert_goal_grant,
};
use proxiima_storage_pg::{PgStorage, begin_owner_transaction};
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The reference index, recording each window the store asks for.
#[derive(Default)]
struct RecordingIndex {
    inner: InMemoryVectorIndex,
    windows: Mutex<Vec<u32>>,
    put_batches: AtomicUsize,
    puts: AtomicUsize,
    reported_bound: Option<CandidateBound>,
    reported_score: Mutex<Option<f32>>,
    before_window: Mutex<Option<BeforeWindow>>,
}

/// A change made before the index answers `window`.
struct BeforeWindow {
    window: u32,
    change: Change,
}

enum Change {
    /// The store supersedes a note series, on its own connection.
    Supersede {
        pg: Box<PgStorage>,
        owner: OwnerId,
        handle: Uuid,
    },
    /// The index drops an entry whose memory the store still holds.
    Drop(VectorKey),
    /// The index holds a new committed version of an entity's vector.
    Reembed {
        pg: Box<PgStorage>,
        entity: EmbeddableEntityRef,
        owner: OwnerId,
        vector: SpaceVector,
    },
}

#[async_trait::async_trait]
impl VectorIndex for RecordingIndex {
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        self.put_batches.fetch_add(1, Ordering::Relaxed);
        self.puts.fetch_add(entries.len(), Ordering::Relaxed);
        self.inner.put_batch(entries).await
    }

    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        self.inner.delete(keys, fence).await
    }

    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }

    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_head(targets, is_head, revision).await
    }

    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.windows.lock().unwrap().push(window);
        let before = self
            .before_window
            .lock()
            .unwrap()
            .take_if(|before| before.window == window);
        match before.map(|before| before.change) {
            Some(Change::Supersede { pg, owner, handle }) => {
                TestNote::new("Index result", "A later version")
                    .handle(handle)
                    .create(&pg, owner)
                    .await?;
            }
            Some(Change::Drop(key)) => self.inner.delete(&[key], 100).await?,
            Some(Change::Reembed {
                pg,
                entity,
                owner,
                vector,
            }) => {
                self.inner.put(entity, owner, 2, 2, true, &[vector]).await?;
                sqlx::query(
                    "UPDATE proxiima_core.embedding_heads
                        SET embedding_version = 2, vector_generation = 2
                      WHERE entity_id = $1 AND model_id = 'memory-index' AND dim = 384",
                )
                .bind(entity.entity_id())
                .execute(pg.pool_for_tests())
                .await
                .map_err(|err| StorageError::Internal(err.to_string()))?;
            }
            None => {}
        }
        let mut page = self.inner.nearest(filter, query, window).await?;
        if let Some(bound) = self.reported_bound {
            page.bound = bound;
        }
        if let Some(score) = *self.reported_score.lock().unwrap() {
            for candidate in &mut page.candidates {
                candidate.similarity_score = score;
            }
        }
        Ok(page)
    }

    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        self.inner.entries(after, limit).await
    }
}

fn vector(score: f32) -> SpaceVector {
    let mut values = vec![0.0; EmbeddingDim::D384.width()];
    values[0] = score;
    values[1] = (1.0 - score * score).sqrt();
    SpaceVector::new(
        EmbeddingSpace::new("memory-index", EmbeddingDim::D384),
        values,
    )
    .unwrap()
}

fn request(owner: OwnerId) -> MemorySearchRequest {
    MemorySearchRequest {
        scope: proxiima_core::verbs::query::MemorySearchScope::Owner(owner),
        read_owners: vec![owner],
        query: Some("unrelated lexical text".into()),
        mode: SearchMode::Semantic,
        supersession: SupersessionStatus::HeadsOnly,
        limit: 1,
        kind: Some(EntityKind::Fact),
        schema_id: None,
        exclude_schema_ids: Vec::new(),
        tags: vec!["wanted".into()],
        tag_match: TagMatch::All,
        since: None,
        until: None,
        order: SearchOrder::Relevance,
        min_score: None,
        semantic_weight: None,
        after: None,
        semantic: Some(vector(1.0)),
    }
}

fn readable_request(source_owner: OwnerId) -> MemorySearchRequest {
    MemorySearchRequest {
        scope: proxiima_core::verbs::query::MemorySearchScope::Readable,
        read_owners: vec![source_owner],
        query: Some("unrelated lexical text".into()),
        mode: SearchMode::Semantic,
        supersession: SupersessionStatus::HeadsOnly,
        limit: 1,
        kind: Some(EntityKind::Fact),
        schema_id: Some(SchemaId::new("core/agent-note-v1".into())),
        exclude_schema_ids: Vec::new(),
        tags: vec!["wanted".into()],
        tag_match: TagMatch::All,
        since: None,
        until: None,
        order: SearchOrder::Relevance,
        min_score: None,
        semantic_weight: None,
        after: None,
        semantic: Some(vector(1.0)),
    }
}

fn readable_perspective_request(source_owner: OwnerId) -> MemorySearchRequest {
    MemorySearchRequest {
        scope: proxiima_core::verbs::query::MemorySearchScope::Readable,
        read_owners: vec![source_owner],
        query: Some("unrelated lexical text".into()),
        mode: SearchMode::Semantic,
        supersession: SupersessionStatus::HeadsOnly,
        limit: 1,
        kind: Some(EntityKind::Perspective),
        schema_id: Some(SchemaId::new("core/agent-derivation-v1".into())),
        exclude_schema_ids: Vec::new(),
        tags: vec!["wanted".into()],
        tag_match: TagMatch::All,
        since: None,
        until: None,
        order: SearchOrder::Relevance,
        min_score: None,
        semantic_weight: None,
        after: None,
        semantic: Some(vector(1.0)),
    }
}

/// A note tagged `tag`, whose projection the search hydrates.
async fn seed_note(pg: &PgStorage, owner: OwnerId, tag: &str) -> Result<Uuid, StorageError> {
    let note = TestNote::new("Index result", "Hydrated from memory").tags(&[tag]);
    Ok(note.create(pg, owner).await?.memory_id.into_inner())
}

/// A Fact tagged `tag` under a schema no spiice registers. `owner` already
/// has its owner row.
// Raw INSERT on purpose: the write path writes a projection row only for a
// registered schema. This candidate matches the tag, so its schema is the one
// reason the search does not admit it.
async fn seed_unregistered_schema_fact(
    pool: &sqlx::PgPool,
    owner: OwnerId,
    tag: &str,
) -> Result<Uuid, sqlx::Error> {
    let schema = "semantic/other-v1";
    let t = Uuid::now_v7();
    let handle = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO proxiima_core.memory_head(handle, t, kind, owner_id, schema_id)
         VALUES ($1, $2, 'fact', $3, $4)",
    )
    .bind(handle)
    .bind(t)
    .bind(owner.into_inner())
    .bind(schema)
    .execute(pool)
    .await?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO proxiima_core.memory
             (handle, t, kind, owner_id, schema_id, sidecar_tables, occurred_at, \"by\", via)
         VALUES ($1, $2, 'fact', $3, $4, '{}', uuid_extract_timestamp($2),
                 '00000000-0000-0000-0000-000000000509'::uuid, NULL)",
    )
    .bind(handle)
    .bind(t)
    .bind(owner.into_inner())
    .bind(schema)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO proxiima_core.projection(memory_id, schema_id, owner_id, search_tsv, tag)
         VALUES ($1, $2, $3, ''::tsvector, ARRAY[$4])",
    )
    .bind(t)
    .bind(schema)
    .bind(owner.into_inner())
    .bind(tag)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(t)
}

/// Write `entity` `versions` times into the `memory-index` space: its head
/// and the host index hold the last version.
async fn embed(
    pg: &PgStorage,
    owner: OwnerId,
    entity: Uuid,
    versions: usize,
) -> Result<(), StorageError> {
    for _ in 0..versions {
        let chunks = vec![vector(1.0).values().to_vec()];
        create_embedding_of(pg, owner, MemoryId::new(entity), "memory-index", chunks).await?;
    }
    Ok(())
}

// `Proxiima.Embeddings.publishChunks`.
#[tokio::test]
async fn a_stale_second_claim_prevents_every_host_index_put() -> TestResult {
    let index = Arc::new(RecordingIndex::default());
    let (pg, _database) = fresh_pg_host_index("fenced_host_index", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let space = EmbeddingSpace::new("fenced-index", EmbeddingDim::D1024);
    for tag in ["first", "second"] {
        let memory = seed_note(&pg, owner, tag).await?;
        enqueue_embedding_job(&pg, owner, memory, space.model_id()).await?;
    }

    let mut claims = pg.claim_pending_embedding_jobs(2, &[], &[]).await?;
    assert_eq!(claims.len(), 2);
    let valid_token = std::mem::replace(&mut claims[1].claim_token, Uuid::now_v7());
    let vectors = [SpaceVector::new(
        space,
        vec![1.0; EmbeddingDim::D1024.width()],
    )?];
    let writes = |claims: &[_; 2]| {
        claims
            .each_ref()
            .map(|claim: &proxiima_core::EmbeddingJobClaim| EmbeddingWrite {
                owner: &owner,
                entity: EmbeddableEntityRef::Memory {
                    kind: claim.entity_kind,
                    memory_id: claim.entity_id,
                },
                vectors: &vectors,
                proof: EmbeddingWriteProof::for_claim_for_tests(claim),
            })
    };
    let mut claims: [_; 2] = claims.try_into().expect("two claims");
    let refused = pg.insert_embeddings(&writes(&claims)).await;
    assert!(
        matches!(refused, Err(StorageError::Conflict(_))),
        "{refused:?}"
    );
    assert_eq!(index.puts.load(Ordering::Relaxed), 0);
    assert_eq!(index.put_batches.load(Ordering::Relaxed), 0);

    claims[1].claim_token = valid_token;
    assert_eq!(pg.insert_embeddings(&writes(&claims)).await?.len(), 2);
    assert_eq!(index.puts.load(Ordering::Relaxed), 2);
    assert_eq!(
        index.put_batches.load(Ordering::Relaxed),
        1,
        "a multi-claim insert must reach the host index in one put_batch call"
    );
    Ok(())
}

/// Seed a committed `embedding_heads` row and matching host-index entry at
/// `generation` for direct-index search fixture tests.
async fn put_committed_vector(
    pg: &PgStorage,
    index: &dyn VectorIndex,
    entity: EmbeddableEntityRef,
    owner: OwnerId,
    generation: i64,
    vectors: &[SpaceVector],
) -> TestResult {
    let space = vectors[0].space();
    sqlx::query(
        "INSERT INTO proxiima_core.embedding_heads
             (entity_id, model_id, dim, embedding_version, owner_id, vector_generation)
         VALUES ($1, $2, $3, 1, $4, $5)
         ON CONFLICT (entity_id, model_id, dim) DO UPDATE
             SET owner_id = EXCLUDED.owner_id,
                 vector_generation = EXCLUDED.vector_generation",
    )
    .bind(entity.entity_id())
    .bind(space.model_id())
    .bind(i16::try_from(space.dim().width())?)
    .bind(owner.into_inner())
    .bind(generation)
    .execute(pg.pool_for_tests())
    .await?;
    index
        .put(entity, owner, 1, generation, true, vectors)
        .await?;
    Ok(())
}

#[tokio::test]
async fn semantic_port_refuses_invalid_host_index_scores() -> TestResult {
    let index = Arc::new(RecordingIndex::default());
    let (pg, _database) = fresh_pg_host_index("semantic_invalid_index_score", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let visible = seed_note(&pg, owner, "wanted").await?;
    put_committed_vector(
        &pg,
        index.as_ref(),
        EmbeddableEntityRef::Memory {
            kind: proxiima_core::MemoryKind::Fact,
            memory_id: MemoryId::new(visible),
        },
        owner,
        1,
        &[vector(1.0)],
    )
    .await?;
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope = owner_scope_for_tests(&[owner]);
    for invalid in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
        *index.reported_score.lock().unwrap() = Some(invalid);
        let result = pg
            .search_memories(Some(&scope), &request(owner), registry.search_projections())
            .await;
        assert!(
            matches!(
                result,
                Err(StorageError::Unavailable(message)) if message.contains("score outside 0..=1")
            ),
            "score {invalid}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn semantic_port_expands_past_stale_foreign_and_filtered_candidates() -> TestResult {
    let index = Arc::new(RecordingIndex::default());
    let (pg, _database) = fresh_pg_host_index("semantic_memory_index", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let other = OwnerId::from(UserId::new(Uuid::now_v7()));
    let visible = seed_note(&pg, owner, "wanted").await?;
    let wrong_tag = seed_note(&pg, owner, "other").await?;
    let wrong_schema = seed_unregistered_schema_fact(pg.pool_for_tests(), owner, "wanted").await?;
    let foreign = seed_note(&pg, other, "wanted").await?;
    let entity = |id| EmbeddableEntityRef::Memory {
        kind: proxiima_core::MemoryKind::Fact,
        memory_id: MemoryId::new(id),
    };
    // The derived index is stale: it attributes a foreign memory to us and
    // retains erased ids. None may survive the store's RLS admission.
    for id in (0..25)
        .map(|_| Uuid::now_v7())
        .chain([foreign, wrong_tag, wrong_schema])
    {
        index
            .put(entity(id), owner, 1, 1, true, &[vector(1.0)])
            .await?;
    }
    put_committed_vector(
        &pg,
        index.as_ref(),
        entity(visible),
        owner,
        1,
        &[vector(0.1), vector(0.8)],
    )
    .await?;
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope = owner_scope_for_tests(&[owner]);
    let page = pg
        .search_memories(Some(&scope), &request(owner), registry.search_projections())
        .await?;
    assert_eq!(page.results.len(), 1);
    assert_eq!(page.results[0].memory_id.into_inner(), visible);
    assert!((page.results[0].score - 0.8).abs() < 0.000_001);
    assert!(page.results[0].snippet.contains("Hydrated from memory"));
    assert!(!page.has_more);
    assert_eq!(*index.windows.lock().unwrap(), vec![20, 40]);
    Ok(())
}

/// A semantic page is the last widening round's admission alone. A memory
/// the first round admits and a commit supersedes before the second round
/// leaves the page; the round that saw it as a head does not keep it.
#[tokio::test]
async fn a_version_superseded_between_widening_rounds_leaves_the_page() -> TestResult {
    let index = Arc::new(RecordingIndex::default());
    let (pg, _database) = fresh_pg_host_index("semantic_widening_snapshot", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let entity = |id| EmbeddableEntityRef::Memory {
        kind: proxiima_core::MemoryKind::Fact,
        memory_id: MemoryId::new(id),
    };
    let superseded = TestNote::new("Index result", "Hydrated from memory")
        .tags(&["wanted"])
        .create(&pg, owner)
        .await?;
    let kept = seed_note(&pg, owner, "wanted").await?;
    put_committed_vector(
        &pg,
        index.as_ref(),
        entity(superseded.memory_id.into_inner()),
        owner,
        1,
        &[vector(1.0)],
    )
    .await?;
    put_committed_vector(&pg, index.as_ref(), entity(kept), owner, 1, &[vector(0.9)]).await?;
    // Erased ids fill the first window, so it admits two of twenty and widens.
    for _ in 0..18 {
        index
            .put(entity(Uuid::now_v7()), owner, 1, 1, true, &[vector(0.5)])
            .await?;
    }
    *index.before_window.lock().unwrap() = Some(BeforeWindow {
        window: 40,
        change: Change::Supersede {
            pg: Box::new(pg.clone()),
            owner,
            handle: superseded.handle,
        },
    });
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let page = pg
        .search_memories(
            Some(&owner_scope_for_tests(&[owner])),
            &request(owner),
            registry.search_projections(),
        )
        .await?;
    assert_eq!(*index.windows.lock().unwrap(), vec![20, 40]);
    assert!(
        index.before_window.lock().unwrap().is_none(),
        "the series was superseded between the rounds"
    );
    assert_eq!(
        page.results
            .iter()
            .map(|result| result.memory_id.into_inner())
            .collect::<Vec<_>>(),
        [kept]
    );
    assert!(!page.has_more);
    Ok(())
}

/// A candidate an earlier widening round found stays a candidate when a later
/// round's index answer leaves it out: the last round admits every candidate
/// the rounds found, and the memory is still a head the reader may read.
#[tokio::test]
async fn an_eligible_candidate_a_later_round_leaves_out_stays_on_the_page() -> TestResult {
    let index = Arc::new(RecordingIndex::default());
    let (pg, _database) = fresh_pg_host_index("semantic_widening_union", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let entity = |id| EmbeddableEntityRef::Memory {
        kind: proxiima_core::MemoryKind::Fact,
        memory_id: MemoryId::new(id),
    };
    let left_out = seed_note(&pg, owner, "wanted").await?;
    let later = seed_note(&pg, owner, "wanted").await?;
    put_committed_vector(
        &pg,
        index.as_ref(),
        entity(left_out),
        owner,
        1,
        &[vector(1.0)],
    )
    .await?;
    // Erased ids fill the first window after `left_out`, so it admits one of
    // forty and widens; `later` ranks below them and enters at the second.
    for _ in 0..39 {
        index
            .put(entity(Uuid::now_v7()), owner, 1, 1, true, &[vector(0.8)])
            .await?;
    }
    put_committed_vector(&pg, index.as_ref(), entity(later), owner, 1, &[vector(0.5)]).await?;
    *index.before_window.lock().unwrap() = Some(BeforeWindow {
        window: 80,
        change: Change::Drop(key(left_out)),
    });
    let mut req = request(owner);
    req.limit = 2;
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let page = pg
        .search_memories(
            Some(&owner_scope_for_tests(&[owner])),
            &req,
            registry.search_projections(),
        )
        .await?;
    assert_eq!(*index.windows.lock().unwrap(), vec![40, 80]);
    assert!(
        index.before_window.lock().unwrap().is_none(),
        "the second round's answer left the first round's candidate out"
    );
    assert_eq!(
        page.results
            .iter()
            .map(|result| result.memory_id.into_inner())
            .collect::<Vec<_>>(),
        [left_out, later]
    );
    assert!(!page.has_more);
    Ok(())
}

/// A candidate two widening rounds find carries the later round's score: a
/// vector the index replaced between the rounds is the one the page ranks by.
#[tokio::test]
async fn a_candidate_both_rounds_find_carries_the_later_score() -> TestResult {
    let index = Arc::new(RecordingIndex::default());
    let (pg, _database) = fresh_pg_host_index("semantic_widening_rescore", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let entity = |id| EmbeddableEntityRef::Memory {
        kind: proxiima_core::MemoryKind::Fact,
        memory_id: MemoryId::new(id),
    };
    let rescored = seed_note(&pg, owner, "wanted").await?;
    put_committed_vector(
        &pg,
        index.as_ref(),
        entity(rescored),
        owner,
        1,
        &[vector(1.0)],
    )
    .await?;
    // Erased ids fill the first window, so it admits one of twenty and widens.
    for _ in 0..19 {
        index
            .put(entity(Uuid::now_v7()), owner, 1, 1, true, &[vector(0.5)])
            .await?;
    }
    *index.before_window.lock().unwrap() = Some(BeforeWindow {
        window: 40,
        change: Change::Reembed {
            pg: Box::new(pg.clone()),
            entity: entity(rescored),
            owner,
            vector: vector(0.4),
        },
    });
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let page = pg
        .search_memories(
            Some(&owner_scope_for_tests(&[owner])),
            &request(owner),
            registry.search_projections(),
        )
        .await?;
    assert_eq!(*index.windows.lock().unwrap(), vec![20, 40]);
    assert!(
        index.before_window.lock().unwrap().is_none(),
        "the index replaced the vector between the rounds"
    );
    assert_eq!(page.results.len(), 1);
    assert_eq!(page.results[0].memory_id.into_inner(), rescored);
    assert!(
        (page.results[0].score - 0.4).abs() < 0.000_001,
        "the later round's score, not the first round's 1.0; got {}",
        page.results[0].score
    );
    Ok(())
}

async fn publish_semantic_fixture(
    pg: &proxiima_storage_pg::PgStorage,
    source_owner: OwnerId,
    guest: UserId,
    published: Uuid,
    read_ceiling: AccessCeiling,
) -> TestResult {
    create_user(pg.pool_for_tests(), guest).await?;
    let goal_handle = TestGoal::new(&Uuid::now_v7().to_string())
        .title("semantic publication")
        .create(pg, source_owner)
        .await?
        .handle;
    let mut platform = pg.platform_scope_for_host().begin().await?;
    insert_goal_grant(
        &mut *platform,
        goal_handle,
        guest.into_inner(),
        source_owner.into_inner(),
        Role::new(read_ceiling, AccessCeiling::None)?.with_goals(GoalAccess::Reads),
    )
    .await?;
    // Raw INSERT: no write path for goal_memory_publication reachable from test fixtures
    sqlx::query(
        "INSERT INTO proxiima_core.goal_memory_publication
             (goal_handle, memory_t, source_owner, source_generation,
              controller_generation, permitted_use, published_by)
         SELECT $1, $2, mh.owner_id, mh.authority_generation,
                gh.authority_generation, 'read', $3
           FROM proxiima_core.memory_head mh
           JOIN proxiima_core.goal_head gh ON gh.handle = $1
          WHERE mh.t = $2 AND mh.owner_id = $4",
    )
    .bind(goal_handle)
    .bind(published)
    .bind(source_owner.into_inner())
    .bind(source_owner.into_inner())
    .execute(&mut *platform)
    .await?;
    platform.commit().await?;

    Ok(())
}

/// A Goal audience must not lose its exact published Fact when nearer private
/// source-owner vectors consume the remote index's scan cap. The source owner
/// is a selector in `read_owners`, not an authenticated owner Role.
#[tokio::test]
async fn readable_goal_semantic_search_reaches_a_far_published_fact() -> TestResult {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) = fresh_pg_host_index("semantic_goal_publication", index.clone()).await;
    let source_owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let guest = UserId::new(Uuid::now_v7());
    let published = seed_note(&pg, source_owner, "wanted").await?;
    let mut private = Vec::with_capacity(1_200);
    for _ in 0..1_200 {
        private.push(seed_note(&pg, source_owner, "wanted").await?);
    }

    publish_semantic_fixture(&pg, source_owner, guest, published, AccessCeiling::Fact).await?;

    for memory in &private {
        index
            .put(fact(*memory), source_owner, 1, 1, true, &[vector(0.999)])
            .await?;
    }
    put_committed_vector(
        &pg,
        index.as_ref(),
        fact(published),
        source_owner,
        1,
        &[vector(0.1)],
    )
    .await?;

    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope = proxiima_core::test_fixtures::authenticated_context(
        proxiima_core::AuthzContext::for_subject(guest, proxiima_core::AuthPath::HostBearer),
    )
    .owner_scope()
    .expect("direct guest scope")
    .clone();
    let page = pg
        .search_memories(
            Some(&scope),
            &readable_request(source_owner),
            registry.search_projections(),
        )
        .await?;
    assert_eq!(
        page.results.len(),
        1,
        "published result must survive admission"
    );
    assert_eq!(page.results[0].memory_id.into_inner(), published);
    assert!(
        !private.contains(&page.results[0].memory_id.into_inner()),
        "private source-owner memories must remain omitted"
    );
    Ok(())
}

/// A Fact-only source-owner role must not let nearer private Perspectives
/// consume the remote candidate cap before a Goal-published Perspective.
#[tokio::test]
async fn fact_role_source_owner_does_not_starve_goal_published_perspective() -> TestResult {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("semantic_goal_perspective_role", index.clone()).await;
    let source_owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let guest = UserId::new(Uuid::now_v7());
    let origin = seed_note(&pg, source_owner, "grounding").await?;
    let abstraction = TestDerived::abstraction(MemoryId::new(origin))
        .tags(&["grounding"])
        .create(&pg, source_owner)
        .await?
        .memory_id;
    let perspective = || TestDerived::perspective(abstraction).tags(&["wanted"]);
    let published = perspective()
        .create(&pg, source_owner)
        .await?
        .memory_id
        .into_inner();
    let mut private = Vec::with_capacity(1_200);
    for _ in 0..1_200 {
        private.push(
            perspective()
                .create(&pg, source_owner)
                .await?
                .memory_id
                .into_inner(),
        );
    }

    publish_semantic_fixture(
        &pg,
        source_owner,
        guest,
        published,
        AccessCeiling::Perspective,
    )
    .await?;

    for memory in &private {
        index
            .put(
                EmbeddableEntityRef::Memory {
                    kind: proxiima_core::MemoryKind::Perspective,
                    memory_id: MemoryId::new(*memory),
                },
                source_owner,
                1,
                1,
                true,
                &[vector(0.999)],
            )
            .await?;
    }
    put_committed_vector(
        &pg,
        index.as_ref(),
        EmbeddableEntityRef::Memory {
            kind: proxiima_core::MemoryKind::Perspective,
            memory_id: MemoryId::new(published),
        },
        source_owner,
        1,
        &[vector(0.1)],
    )
    .await?;

    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope = proxiima_core::test_fixtures::authenticated_context(
        proxiima_core::AuthzContext::for_subject_with_role(
            guest,
            [(source_owner, Role::ingest())],
            proxiima_core::AuthPath::HostBearer,
        ),
    )
    .owner_scope()
    .expect("verified Fact-only source-owner scope")
    .clone();
    let page = pg
        .search_memories(
            Some(&scope),
            &readable_perspective_request(source_owner),
            registry.search_projections(),
        )
        .await?;
    assert_eq!(
        page.results.len(),
        1,
        "published Perspective must survive admission"
    );
    assert_eq!(page.results[0].memory_id.into_inner(), published);
    assert!(!private.contains(&page.results[0].memory_id.into_inner()));
    Ok(())
}

/// A Goal audience reads the embedding job of the memory published to it and
/// no other job of the source owner: the queue's read policy carries the
/// route `goal_memory_read_admitted` answers.
#[tokio::test]
async fn a_goal_audience_reads_the_published_memorys_embedding_job_only() -> TestResult {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) = fresh_pg_host_index("queue_goal_publication", index).await;
    let source_owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let guest = UserId::new(Uuid::now_v7());
    let published = seed_note(&pg, source_owner, "wanted").await?;
    let private = seed_note(&pg, source_owner, "wanted").await?;
    for memory in [published, private] {
        enqueue_embedding_job(&pg, source_owner, memory, "queue-goal").await?;
    }
    publish_semantic_fixture(&pg, source_owner, guest, published, AccessCeiling::Fact).await?;

    let scope = proxiima_core::test_fixtures::authenticated_context(
        proxiima_core::AuthzContext::for_subject(guest, proxiima_core::AuthPath::HostBearer),
    )
    .owner_scope()
    .expect("direct guest scope")
    .clone();
    let mut tx = begin_owner_transaction(&pg.clone_pool_for_backend(), &scope).await?;
    let readable: Vec<Uuid> = sqlx::query_scalar(
        "SELECT entity_id FROM proxiima_core.embedding_jobs WHERE model_id = 'queue-goal'",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.rollback().await?;
    assert_eq!(readable, [published]);
    Ok(())
}

fn key(entity: Uuid) -> VectorKey {
    VectorKey {
        entity_id: entity,
        space: EmbeddingSpace::new("memory-index", EmbeddingDim::D384),
    }
}

fn fact(id: Uuid) -> EmbeddableEntityRef {
    EmbeddableEntityRef::Memory {
        kind: proxiima_core::MemoryKind::Fact,
        memory_id: MemoryId::new(id),
    }
}

async fn debt_rows(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM proxiima_core.vector_index_debt")
        .fetch_one(pool)
        .await
}

/// A head deleted or moved leaves durable debt, and the next drain pass pays
/// it to the index: the deleted key's chunks go, the moved key's owner
/// follows.
#[tokio::test]
async fn the_drain_pays_head_deletes_and_owner_moves_to_the_index() -> TestResult {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) = fresh_pg_host_index("vector_index_debt_drain", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let other = OwnerId::from(UserId::new(Uuid::now_v7()));
    let gone = seed_note(&pg, owner, "wanted").await?;
    let moved = seed_note(&pg, owner, "wanted").await?;
    seed_note(&pg, other, "wanted").await?;
    for entity in [gone, moved] {
        embed(&pg, owner, entity, 1).await?;
    }
    sqlx::query("DELETE FROM proxiima_core.embedding_heads WHERE entity_id = $1")
        .bind(gone)
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE proxiima_core.embedding_heads SET owner_id = $2 WHERE entity_id = $1")
        .bind(moved)
        .bind(other.into_inner())
        .execute(&pool)
        .await?;
    assert_eq!(debt_rows(&pool).await?, 2, "both changes owe the index");
    assert_eq!(
        index.entries(None, 10).await?.entries.len(),
        2,
        "nothing is paid before the drain"
    );

    assert_eq!(pg.drain_vector_index_debt(100).await?, 2);
    let entries = index.entries(None, 10).await?.entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key, key(moved));
    assert_eq!(entries[0].owner, other);
    assert_eq!(debt_rows(&pool).await?, 0);
    assert_eq!(pg.drain_vector_index_debt(100).await?, 0);
    Ok(())
}

/// The sweep compares the whole index with the heads across pages: an entry
/// with no head goes, an entry under another owner moves, and a head the
/// index lost is deleted so reconciliation embeds it again.
#[tokio::test]
async fn the_sweep_repairs_the_index_against_the_heads() -> TestResult {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) = fresh_pg_host_index("vector_index_sweep", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let other = OwnerId::from(UserId::new(Uuid::now_v7()));
    seed_note(&pg, other, "wanted").await?;
    let kept = seed_note(&pg, owner, "wanted").await?;
    let misowned = seed_note(&pg, owner, "wanted").await?;
    let lost = seed_note(&pg, owner, "wanted").await?;
    let stale_version = seed_note(&pg, owner, "wanted").await?;
    for entity in [kept, misowned, lost, stale_version] {
        embed(&pg, owner, entity, 2).await?;
    }
    // The index then drifts from the heads: it forgets `lost`, files
    // `misowned` under another owner, flips `kept`'s series-head flag, and
    // holds an old version of `stale_version`.
    let current_entries = index.entries(None, 20).await?.entries;
    let gen_of = |id: Uuid| {
        current_entries
            .iter()
            .find(|entry| entry.key.entity_id == id)
            .expect("entry present")
            .generation
    };
    index.delete(&[key(lost)], 100).await?;
    index
        .set_owner(
            &[VectorMetadataTarget {
                key: key(misowned),
                generation: gen_of(misowned),
            }],
            other,
            1,
        )
        .await?;
    index
        .set_head(
            &[VectorMetadataTarget {
                key: key(kept),
                generation: gen_of(kept),
            }],
            false,
            1,
        )
        .await?;
    index
        .put(fact(stale_version), owner, 1, 999, true, &[vector(1.0)])
        .await?;
    // More strays than one sweep page holds (2,000 entries per page).
    for _ in 0..2_200 {
        index
            .put(fact(Uuid::now_v7()), owner, 1, 1, true, &[vector(1.0)])
            .await?;
    }

    let outcome = pg.sweep_orphan_embedding_rows().await?;
    assert_eq!(outcome.embeddings_deleted, 2_200);
    assert_eq!(
        outcome.heads_deleted, 2,
        "the lost head and the stale version"
    );
    let entries = index.entries(None, 10).await?.entries;
    let held: Vec<(VectorKey, OwnerId, bool)> = entries
        .into_iter()
        .map(|e| (e.key, e.owner, e.is_head))
        .collect();
    let mut expected = vec![
        (key(kept), owner, true),
        (key(misowned), owner, true),
        (key(stale_version), owner, true),
    ];
    expected.sort_by(|a, b| a.0.order().cmp(&b.0.order()));
    assert_eq!(held, expected);
    let heads: Vec<Uuid> = sqlx::query_scalar(
        "SELECT entity_id FROM proxiima_core.embedding_heads ORDER BY entity_id",
    )
    .fetch_all(&pool)
    .await?;
    let mut expected = vec![kept, misowned];
    expected.sort();
    assert_eq!(heads, expected);
    Ok(())
}

/// An owner erase deletes the owner's rows and their index entries before it
/// commits, in a database that has no pgvector to hold them.
#[tokio::test]
async fn an_owner_erase_takes_its_entries_out_of_the_index() -> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::OwnerInversePort;

    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) = fresh_pg_host_index("vector_index_erase", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let user = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(user);
    let other = OwnerId::from(UserId::new(Uuid::now_v7()));
    let erased = seed_note(&pg, owner, "wanted").await?;
    let kept = seed_note(&pg, other, "wanted").await?;
    for (entity, entity_owner) in [(erased, owner), (kept, other)] {
        embed(&pg, entity_owner, entity, 1).await?;
    }

    let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
        user_id: user,
        drop_event_id: "test-drop".into(),
    });
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );
    let outcome = pg.erase_personal_owner(&auth, user, &surfaces).await?;
    assert!(
        matches!(outcome, OwnerEraseOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let held: Vec<VectorKey> = index
        .entries(None, 10)
        .await?
        .entries
        .into_iter()
        .map(|entry| entry.key)
        .collect();
    assert_eq!(held, [key(kept)]);
    let heads: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM proxiima_core.embedding_heads")
            .fetch_all(&pool)
            .await?;
    assert_eq!(heads, [kept]);
    assert_eq!(debt_rows(&pool).await?, 0);
    Ok(())
}

#[tokio::test]
async fn short_remote_bound_survives_local_tag_admission() -> TestResult {
    for reported in [CandidateBound::Bounded, CandidateBound::Unknown] {
        let index = Arc::new(RecordingIndex {
            reported_bound: Some(reported),
            ..RecordingIndex::default()
        });
        let (pg, _database) = fresh_pg_host_index("semantic_bound_admission", index.clone()).await;
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let wrong_tag = seed_note(&pg, owner, "other").await?;
        index
            .put(
                EmbeddableEntityRef::Memory {
                    kind: proxiima_core::MemoryKind::Fact,
                    memory_id: MemoryId::new(wrong_tag),
                },
                owner,
                1,
                1,
                true,
                &[vector(1.0)],
            )
            .await?;
        let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
        let page = pg
            .search_memories(
                Some(&owner_scope_for_tests(&[owner])),
                &request(owner),
                registry.search_projections(),
            )
            .await?;
        assert!(page.results.is_empty());
        assert!(!page.has_more);
        assert_eq!(
            page.candidate_bound, reported,
            "admission must not invent source exhaustion"
        );
        assert_eq!(*index.windows.lock().unwrap(), vec![20]);
    }
    Ok(())
}

/// Superseding a series leaves `'head'` debt so the external index filters out
/// superseded versions under `HeadsOnly` without burning candidate windows or
/// widening, while `IncludeSuperseded` still finds them. Forgetting the newer
/// head reverts `memory_head` and restores `is_head = true` on the prior version.
#[tokio::test]
async fn heads_only_skips_superseded_versions_without_widening_and_forget_restores_head()
-> TestResult {
    let index = Arc::new(RecordingIndex::default());
    let (pg, _database) = fresh_pg_host_index("semantic_heads_only_debt", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope = owner_scope_for_tests(&[owner]);

    // Seed 25 superseded versions with higher similarity than the live head.
    let mut series = TestNote::new("Index result", "v0")
        .tags(&["wanted"])
        .create(&pg, owner)
        .await?;
    create_embedding_of(
        &pg,
        owner,
        series.memory_id,
        "memory-index",
        vec![vector(0.99).values().to_vec()],
    )
    .await?;
    let mut prior_id = series.memory_id;
    for rev in 1..=25 {
        let next = TestNote::new("Index result", &format!("v{rev}"))
            .handle(series.handle)
            .tags(&["wanted"])
            .create(&pg, owner)
            .await?;
        let sim = if rev == 25 { 0.4 } else { 0.99 };
        create_embedding_of(
            &pg,
            owner,
            next.memory_id,
            "memory-index",
            vec![vector(sim).values().to_vec()],
        )
        .await?;
        prior_id = series.memory_id;
        series = next;
    }
    let head_id = series.memory_id;

    // Drain the `'head'` debt left by every supersession.
    assert_eq!(pg.drain_vector_index_debt(100).await?, 25);

    // `HeadsOnly` hits the lone head in a single window (`[20]`) with zero
    // widening even though 25 nearer superseded versions exist in the index.
    let page = pg
        .search_memories(Some(&scope), &request(owner), registry.search_projections())
        .await?;
    assert_eq!(page.results.len(), 1);
    assert_eq!(page.results[0].memory_id, head_id);
    assert_eq!(
        *index.windows.lock().unwrap(),
        vec![20],
        "superseded versions must not consume the HeadsOnly window"
    );

    // `IncludeSuperseded` still finds the nearer superseded versions.
    let mut all_req = request(owner);
    all_req.supersession = SupersessionStatus::IncludeSuperseded;
    let all_page = pg
        .search_memories(Some(&scope), &all_req, registry.search_projections())
        .await?;
    assert_eq!(all_page.results.len(), 1);
    assert_ne!(all_page.results[0].memory_id, head_id);

    // Forgetting the current head deletes its embedding head and reverts
    // `memory_head` to `prior_id`, enqueueing `'head'` debt that restores
    // `prior_id`'s `is_head = true` in the external index.
    let permit = proxiima_core::storage_ports::OwnerWritePermit::new_for_tests(
        owner,
        proxiima_core::MemoryKind::Fact,
    );
    proxiima_core::storage_ports::MemoryAuthoringPort::forget_memory(&pg, &permit, head_id).await?;
    assert!(pg.drain_vector_index_debt(100).await? >= 2);
    let reverted = pg
        .search_memories(Some(&scope), &request(owner), registry.search_projections())
        .await?;
    assert_eq!(reverted.results.len(), 1);
    assert_eq!(reverted.results[0].memory_id, prior_id);
    Ok(())
}

/// A Goal audience published an older version of a series must still see that
/// version under `HeadsOnly` even after the source owner supersedes the series
/// privately and drains `'head'` debt.
#[tokio::test]
async fn goal_published_superseded_version_survives_heads_only_on_host_index() -> TestResult {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("semantic_goal_published_superseded", index.clone()).await;
    let source_owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let guest = UserId::new(Uuid::now_v7());

    let published = TestNote::new("Index result", "Published v1")
        .tags(&["wanted"])
        .create(&pg, source_owner)
        .await?;
    embed(&pg, source_owner, published.memory_id.into_inner(), 1).await?;
    publish_semantic_fixture(
        &pg,
        source_owner,
        guest,
        published.memory_id.into_inner(),
        AccessCeiling::Fact,
    )
    .await?;

    // Source owner privately supersedes the series and drains `'head'` debt so
    // the external index now stores `is_head = false` on `published`.
    let successor = TestNote::new("Index result", "Private v2")
        .handle(published.handle)
        .tags(&["wanted"])
        .create(&pg, source_owner)
        .await?;
    embed(&pg, source_owner, successor.memory_id.into_inner(), 1).await?;
    assert_eq!(pg.drain_vector_index_debt(100).await?, 1);
    let entries = index.entries(None, 10).await?.entries;
    let published_entry = entries
        .iter()
        .find(|entry| entry.key.entity_id == published.memory_id.into_inner())
        .expect("published entry present");
    assert!(!published_entry.is_head);

    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope = proxiima_core::test_fixtures::authenticated_context(
        proxiima_core::AuthzContext::for_subject(guest, proxiima_core::AuthPath::HostBearer),
    )
    .owner_scope()
    .expect("direct guest scope")
    .clone();
    let page = pg
        .search_memories(
            Some(&scope),
            &readable_request(source_owner),
            registry.search_projections(),
        )
        .await?;
    assert_eq!(page.results.len(), 1);
    assert_eq!(page.results[0].memory_id, published.memory_id);
    Ok(())
}

/// Bounded host-index sweeps resume cleanly from their returned cursor until
/// `next` is `None`, repairing orphans and metadata drift across slices.
#[tokio::test]
async fn sweep_vector_index_slice_paginates_across_multiple_ticks() -> TestResult {
    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) = fresh_pg_host_index("vector_index_sweep_slice", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let kept = seed_note(&pg, owner, "wanted").await?;
    embed(&pg, owner, kept, 1).await?;
    let kept_gen = index.entries(None, 10).await?.entries[0].generation;
    index
        .set_head(
            &[VectorMetadataTarget {
                key: key(kept),
                generation: kept_gen,
            }],
            false,
            1,
        )
        .await?;
    for _ in 0..2_200 {
        index
            .put(fact(Uuid::now_v7()), owner, 1, 1, true, &[vector(1.0)])
            .await?;
    }

    let first = pg.sweep_vector_index_slice(None, 1).await?;
    assert!(
        first.next.is_some(),
        "first bounded slice must stop after one page with a continuation cursor"
    );
    let mut total_deleted = first.entries_deleted;
    let mut total_repaired = first.metadata_repaired;
    let mut cursor = first.next;
    let mut slices = 1_u32;
    while let Some(after) = cursor {
        let slice = pg.sweep_vector_index_slice(Some(&after), 1).await?;
        total_deleted += slice.entries_deleted;
        total_repaired += slice.metadata_repaired;
        cursor = slice.next;
        slices += 1;
    }
    assert!(slices >= 2);
    assert_eq!(total_deleted, 2_200);
    assert_eq!(total_repaired, 1);
    let remaining = index.entries(None, 10).await?.entries;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].key, key(kept));
    assert!(remaining[0].is_head);
    Ok(())
}

/// An in-memory index whose next `put_batch` pauses until released by the test,
/// allowing deterministic concurrency proofs against lifecycle verbs.
#[derive(Default)]
struct PausableIndex {
    inner: InMemoryVectorIndex,
    arm_next_put: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl PausableIndex {
    fn arm(&self) {
        self.arm_next_put.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl VectorIndex for PausableIndex {
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        if self.arm_next_put.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.put_batch(entries).await
    }

    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        self.inner.delete(keys, fence).await
    }

    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }

    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_head(targets, is_head, revision).await
    }

    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.inner.nearest(filter, query, window).await
    }

    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        self.inner.entries(after, limit).await
    }
}

const CONCURRENCY_PATIENCE: std::time::Duration = std::time::Duration::from_secs(15);

async fn insert_claimless_vector(
    pg: &PgStorage,
    owner: OwnerId,
    memory_id: MemoryId,
) -> Result<(), StorageError> {
    let vectors = [vector(1.0)];
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &owner,
        entity: EmbeddableEntityRef::Memory {
            kind: proxiima_core::MemoryKind::Fact,
            memory_id,
        },
        vectors: &vectors,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await?;
    Ok(())
}

/// Superseding a series while a re-embed is paused inside `put_batch` with
/// pre-existing debt on the same key must linearize cleanly without deadlocking
/// and leave `is_head = false` once debt drains.
#[tokio::test]
async fn supersession_during_paused_put_batch_with_existing_debt_does_not_deadlock() -> TestResult {
    let index = Arc::new(PausableIndex::default());
    let (pg, _database) = fresh_pg_host_index("paused_supersede_debt", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let v1 = TestNote::new("Index result", "v1")
        .tags(&["wanted"])
        .create(&pg, owner)
        .await?;
    insert_claimless_vector(&pg, owner, v1.memory_id).await?;
    // Supersede and forget v2 so v1 is head again AND already has `'head'` debt queued.
    let v2 = TestNote::new("Index result", "v2")
        .handle(v1.handle)
        .tags(&["wanted"])
        .create(&pg, owner)
        .await?;
    let permit = proxiima_core::storage_ports::OwnerWritePermit::new_for_tests(
        owner,
        proxiima_core::MemoryKind::Fact,
    );
    proxiima_core::storage_ports::MemoryAuthoringPort::forget_memory(&pg, &permit, v2.memory_id)
        .await?;
    assert!(debt_rows(pg.pool_for_tests()).await? >= 1);

    index.arm();
    let writer_pg = pg.clone();
    let writer =
        tokio::spawn(async move { insert_claimless_vector(&writer_pg, owner, v1.memory_id).await });
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.entered.notified()).await?;

    let supersede_pg = pg.clone();
    let handle = v1.handle;
    let superseder = tokio::spawn(async move {
        TestNote::new("Index result", "v3")
            .handle(handle)
            .tags(&["wanted"])
            .create(&supersede_pg, owner)
            .await
    });
    tokio::task::yield_now().await;
    index.release.notify_one();

    tokio::time::timeout(CONCURRENCY_PATIENCE, writer).await???;
    tokio::time::timeout(CONCURRENCY_PATIENCE, superseder).await???;
    assert!(pg.drain_vector_index_debt(100).await? >= 1);
    let entries = index.entries(None, 10).await?.entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key, key(v1.memory_id.into_inner()));
    assert!(
        !entries[0].is_head,
        "supersession must flip is_head to false"
    );
    Ok(())
}

/// A claimless first embedding paused during `put_batch` linearizes against a
/// concurrent forget of the series' only live version: neither `embedding_heads`
/// nor the host index retains the forgotten memory once debt drains.
#[tokio::test]
async fn claimless_paused_put_batch_linearizes_against_forget() -> TestResult {
    let index = Arc::new(PausableIndex::default());
    let (pg, _database) = fresh_pg_host_index("paused_claimless_forget", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let note = TestNote::new("Index result", "only version")
        .tags(&["wanted"])
        .create(&pg, owner)
        .await?;

    index.arm();
    let writer_pg = pg.clone();
    let writer =
        tokio::spawn(
            async move { insert_claimless_vector(&writer_pg, owner, note.memory_id).await },
        );
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.entered.notified()).await?;

    let forget_pg = pg.clone();
    let forgetter = tokio::spawn(async move {
        let permit = proxiima_core::storage_ports::OwnerWritePermit::new_for_tests(
            owner,
            proxiima_core::MemoryKind::Fact,
        );
        proxiima_core::storage_ports::MemoryAuthoringPort::forget_memory(
            &forget_pg,
            &permit,
            note.memory_id,
        )
        .await
    });
    tokio::task::yield_now().await;
    index.release.notify_one();

    tokio::time::timeout(CONCURRENCY_PATIENCE, writer).await???;
    tokio::time::timeout(CONCURRENCY_PATIENCE, forgetter).await???;
    pg.drain_vector_index_debt(100).await?;
    assert!(index.entries(None, 10).await?.entries.is_empty());
    let heads: i64 = sqlx::query_scalar("SELECT count(*) FROM proxiima_core.embedding_heads")
        .fetch_one(pg.pool_for_tests())
        .await?;
    assert_eq!(heads, 0);
    Ok(())
}

/// A claimless first embedding paused during `put_batch` linearizes against a
/// concurrent whole-owner erase: the erase waits for the owner fence, deletes
/// the committed head, and settles the index before committing.
#[tokio::test]
async fn claimless_paused_put_batch_linearizes_against_owner_erase() -> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::OwnerInversePort;

    let index = Arc::new(PausableIndex::default());
    let (pg, _database) = fresh_pg_host_index("paused_claimless_erase", index.clone()).await;
    let user = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(user);
    let note = TestNote::new("Index result", "erased")
        .tags(&["wanted"])
        .create(&pg, owner)
        .await?;

    index.arm();
    let writer_pg = pg.clone();
    let writer =
        tokio::spawn(
            async move { insert_claimless_vector(&writer_pg, owner, note.memory_id).await },
        );
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.entered.notified()).await?;

    let erase_pg = pg.clone();
    let eraser = tokio::spawn(async move {
        let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
            user_id: user,
            drop_event_id: "paused-erase".into(),
        });
        let surfaces = OwnerSurfaces::for_registry(
            &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
        );
        erase_pg.erase_personal_owner(&auth, user, &surfaces).await
    });
    tokio::task::yield_now().await;
    index.release.notify_one();

    tokio::time::timeout(CONCURRENCY_PATIENCE, writer).await???;
    let outcome = tokio::time::timeout(CONCURRENCY_PATIENCE, eraser).await???;
    assert!(matches!(outcome, OwnerEraseOutcome::Completed { .. }));
    assert!(
        index.entries(None, 10).await?.entries.is_empty(),
        "owner erase must settle the index before commit without an extra drain"
    );
    assert_eq!(debt_rows(pg.pool_for_tests()).await?, 0);
    Ok(())
}

/// A claimless first embedding paused during `put_batch` linearizes against a
/// real governed transfer of a two-version series, updating both `owner` and
/// `is_head` for both versions once debt drains.
#[tokio::test]
async fn claimless_paused_put_batch_linearizes_against_governed_transfer() -> TestResult {
    use proxiima_core::owner_inverse::OwnerSurfaces;
    use proxiima_core::storage_ports::{OwnerTransferPort, OwnerWritePermit};

    let index = Arc::new(PausableIndex::default());
    let (pg, _database) = fresh_pg_host_index("paused_claimless_transfer", index.clone()).await;
    let owner_a = OwnerId::from(UserId::new(Uuid::now_v7()));
    let owner_b = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(pg.pool_for_tests(), owner_b.as_user()).await?;

    let v1 = TestNote::new("Index result", "v1")
        .tags(&["wanted"])
        .create(&pg, owner_a)
        .await?;
    insert_claimless_vector(&pg, owner_a, v1.memory_id).await?;
    let v2 = TestNote::new("Index result", "v2")
        .handle(v1.handle)
        .tags(&["wanted"])
        .create(&pg, owner_a)
        .await?;

    index.arm();
    let writer_pg = pg.clone();
    let writer =
        tokio::spawn(
            async move { insert_claimless_vector(&writer_pg, owner_a, v2.memory_id).await },
        );
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.entered.notified()).await?;

    let transfer_pg = pg.clone();
    let space = EmbeddingSpace::new("memory-index", EmbeddingDim::D384);
    let transfer = tokio::spawn(async move {
        let permit =
            OwnerWritePermit::transfer_for_tests(owner_a, proxiima_core::MemoryKind::Fact, owner_b);
        let surfaces = OwnerSurfaces::for_registry(
            &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
        );
        transfer_pg
            .transfer_to_owner(
                &permit,
                proxiima_core::EntityId::Memory(v2.memory_id),
                owner_b,
                &surfaces,
                &[space],
            )
            .await
    });
    tokio::task::yield_now().await;
    index.release.notify_one();

    tokio::time::timeout(CONCURRENCY_PATIENCE, writer).await???;
    let moved = tokio::time::timeout(CONCURRENCY_PATIENCE, transfer).await???;
    assert!(moved, "governed transfer must succeed");
    assert!(pg.drain_vector_index_debt(100).await? >= 1);
    let entries = index.entries(None, 10).await?.entries;
    let by_id: std::collections::HashMap<Uuid, (OwnerId, bool)> = entries
        .into_iter()
        .map(|entry| (entry.key.entity_id, (entry.owner, entry.is_head)))
        .collect();
    assert_eq!(
        by_id.get(&v1.memory_id.into_inner()),
        Some(&(owner_b, false)),
        "superseded version must move to owner_b with is_head = false"
    );
    // Depending on whether the transfer committed before or after phase 3, v2
    // is either cleanly moved to owner_b (with is_head = true) or deleted as
    // owner_a's stale attempt with no stale owner_a entry left behind.
    if let Some(&(v2_owner, v2_head)) = by_id.get(&v2.memory_id.into_inner()) {
        assert_eq!(v2_owner, owner_b);
        assert!(v2_head);
    }
    Ok(())
}

/// An index that returns at most `max_page` entries per `entries` call while
/// setting `has_more` accurately, plus an optional broken-empty mode.
struct ShortPageIndex {
    inner: InMemoryVectorIndex,
    max_page: u32,
    broken_empty_has_more: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl VectorIndex for ShortPageIndex {
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        self.inner.put_batch(entries).await
    }

    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        self.inner.delete(keys, fence).await
    }

    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }

    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_head(targets, is_head, revision).await
    }

    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.inner.nearest(filter, query, window).await
    }

    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        if self.broken_empty_has_more.load(Ordering::SeqCst) {
            return Ok(VectorEntryPage {
                entries: Vec::new(),
                has_more: true,
            });
        }
        self.inner.entries(after, limit.min(self.max_page)).await
    }
}

/// A short `entries` page with `has_more: true` must advance by its cutoff
/// without deleting live heads beyond that page; an empty page claiming
/// `has_more: true` must fail with `Unavailable`.
#[tokio::test]
async fn short_entries_page_preserves_live_heads_and_rejects_empty_has_more() -> TestResult {
    let index = Arc::new(ShortPageIndex {
        inner: InMemoryVectorIndex::default(),
        max_page: 2,
        broken_empty_has_more: std::sync::atomic::AtomicBool::new(false),
    });
    let (pg, _database) = fresh_pg_host_index("sweep_short_page", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let mut live = Vec::new();
    for _ in 0..5 {
        let id = seed_note(&pg, owner, "wanted").await?;
        embed(&pg, owner, id, 1).await?;
        live.push(id);
    }
    index
        .put(fact(Uuid::now_v7()), owner, 1, 1, true, &[vector(1.0)])
        .await?;

    let outcome = pg.sweep_orphan_embedding_rows().await?;
    assert_eq!(outcome.embeddings_deleted, 1);
    assert_eq!(
        outcome.heads_deleted, 0,
        "a short page with has_more: true must never delete live heads beyond its cutoff"
    );
    assert_eq!(index.inner.entries(None, 10).await?.entries.len(), 5);

    index.broken_empty_has_more.store(true, Ordering::SeqCst);
    let broken = pg.sweep_vector_index_slice(None, 1).await;
    assert!(
        matches!(broken, Err(StorageError::Unavailable(_))),
        "empty page with has_more: true must fail: {broken:?}"
    );
    Ok(())
}

fn sourced_fact_cmd(
    source: &str,
    key: &str,
    handle: Option<Uuid>,
) -> proxiima_core::verbs::fact_ingest::FactWriteCommand {
    proxiima_core::verbs::fact_ingest::FactWriteCommand {
        schema_id: SchemaId::new("core/test-fact-v1".to_owned()),
        schema_version: proxiima_core::SchemaVersion::new(1),
        handle,
        source_id: Some(source.to_owned()),
        ingest_key: Some(key.to_owned()),
        payload: Vec::new(),
        rendered_text: None,
        lexical_language: None,
        receipt: None,
        occurred_at: None,
        citation: None,
        additional_references: Vec::new(),
        refs: Vec::new(),
        blob_id: None,
        kind: proxiima_core::MemoryKind::Fact,
        request: None,
    }
}

/// Source-scoped erase that deletes a series head and promotes an embedded
/// predecessor from another source acquires the predecessor's embedding key
/// lock BEFORE writing `vector_index_debt`, preventing a deadlock with a
/// concurrent sweep repair deleting that same lost head.
#[tokio::test]
async fn source_scoped_erase_promoting_predecessor_does_not_deadlock_with_sweep_repair()
-> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::{FactIngestPort, OwnerInversePort, OwnerWritePermit};
    use proxiima_core::{MemoryKind, SourceId};

    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("source_erase_sweep_no_deadlock", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let user = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(user);
    let permit = OwnerWritePermit::new_for_tests(owner, MemoryKind::Fact);

    let v1 = pg
        .ingest_fact_atomic(&permit, &sourced_fact_cmd("src-keep", "k-keep", None), None)
        .await?;
    let v2 = pg
        .ingest_fact_atomic(
            &permit,
            &sourced_fact_cmd("src-drop", "k-drop", Some(v1.handle)),
            None,
        )
        .await?;
    for id in [v1.memory_id, v2.memory_id] {
        insert_claimless_vector(&pg, owner, id).await?;
    }
    // Simulate sweep seeing v1 missing from the host index while holding its key lock.
    index.delete(&[key(v1.memory_id.into_inner())], 100).await?;
    let lock_name = format!(
        "proxiima-embedding:{}:memory-index:384",
        v1.memory_id.into_inner()
    );
    let mut sweep_tx = pool.begin().await?;
    let locked: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&lock_name)
            .fetch_one(&mut *sweep_tx)
            .await?;
    assert!(locked);

    let erase_pg = pg.clone();
    let eraser = tokio::spawn(async move {
        let source = SourceId::new("src-drop");
        let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalSourceScope {
            user_id: user,
            source_id: source.clone(),
            drop_event_id: "src-drop-sweep-interleaving".into(),
        });
        let surfaces = OwnerSurfaces::for_registry(
            &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
        );
        erase_pg
            .erase_personal_source_scope(&auth, user, &source, &surfaces)
            .await
    });

    // Wait until the source-scoped erase is queued on v1's embedding key lock
    // (before its triggers have written any `vector_index_debt` row).
    tokio::time::timeout(CONCURRENCY_PATIENCE, async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                     SELECT 1 FROM pg_locks
                      WHERE locktype = 'advisory' AND NOT granted
                        AND ((classid::bigint << 32) | objid::bigint) = hashtextextended($1, 0)
                 )",
            )
            .bind(&lock_name)
            .fetch_one(&pool)
            .await?;
            if waiting {
                return Ok::<(), sqlx::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;

    // Sweep deletes v1's lost head (trigger writes `vector_index_debt(v1)`)
    // without waiting on the erase, then commits and releases the key lock.
    sqlx::query("DELETE FROM proxiima_core.embedding_heads WHERE entity_id = $1")
        .bind(v1.memory_id.into_inner())
        .execute(&mut *sweep_tx)
        .await?;
    sweep_tx.commit().await?;

    let outcome = tokio::time::timeout(CONCURRENCY_PATIENCE, eraser).await???;
    assert!(matches!(outcome, OwnerEraseOutcome::Completed { .. }));
    assert!(index.entries(None, 10).await?.entries.is_empty());
    // `v2`'s debt was settled by `src-drop`'s erase; only retained predecessor
    // `v1`'s debt from sweep remains for the async drain.
    let remaining: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM proxiima_core.vector_index_debt")
            .fetch_all(&pool)
            .await?;
    assert_eq!(remaining, [v1.memory_id.into_inner()]);
    assert_eq!(pg.drain_vector_index_debt(100).await?, 1);
    assert_eq!(debt_rows(&pool).await?, 0);
    Ok(())
}

/// An index that writes the first item of a batch and then fails the call,
/// modeling a split HTTP `put_batch` where chunk 1 landed and chunk 2 failed.
#[derive(Default)]
struct PartialFailBatchIndex {
    inner: InMemoryVectorIndex,
    fail_next: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl VectorIndex for PartialFailBatchIndex {
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            if let Some(first) = entries.first() {
                self.inner.put_batch(std::slice::from_ref(first)).await?;
            }
            return Err(StorageError::Unavailable("later batch chunk failed".into()).into());
        }
        self.inner.put_batch(entries).await
    }

    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        self.inner.delete(keys, fence).await
    }

    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }

    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_head(targets, is_head, revision).await
    }

    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.inner.nearest(filter, query, window).await
    }

    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        self.inner.entries(after, limit).await
    }
}

/// A partially accepted first-time `put_batch` that fails leaves durable
/// write-ahead `'delete'` intent surviving rollback: an immediate hard erase
/// deletes the accepted first-time vectors before committing.
#[tokio::test]
async fn partial_put_batch_failure_leaves_debt_that_hard_erase_and_drain_settle() -> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::OwnerInversePort;

    let index = Arc::new(PartialFailBatchIndex::default());
    let (pg, _database) = fresh_pg_host_index("partial_batch_rollback_debt", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let user = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(user);

    let first = seed_note(&pg, owner, "wanted").await?;
    let second = seed_note(&pg, owner, "wanted").await?;
    let vectors = [vector(1.0)];
    index.fail_next.store(true, Ordering::SeqCst);
    let err = pg
        .insert_embeddings(&[
            EmbeddingWrite {
                owner: &owner,
                entity: fact(first),
                vectors: &vectors,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            },
            EmbeddingWrite {
                owner: &owner,
                entity: fact(second),
                vectors: &vectors,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            },
        ])
        .await;
    assert!(matches!(err, Err(StorageError::Unavailable(_))));
    assert_eq!(
        index.inner.entries(None, 10).await?.entries.len(),
        1,
        "first chunk landed on the index before the later chunk failed"
    );
    let heads: i64 = sqlx::query_scalar("SELECT count(*) FROM proxiima_core.embedding_heads")
        .fetch_one(&pool)
        .await?;
    assert_eq!(heads, 0, "PostgreSQL rolled back the staged heads");
    assert_eq!(
        debt_rows(&pool).await?,
        2,
        "attempted keys left durable debt surviving rollback"
    );

    // Immediate hard erase (no sweep, no drain) must settle those attempted keys.
    let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
        user_id: user,
        drop_event_id: "partial-batch-erase".into(),
    });
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );
    let outcome = pg.erase_personal_owner(&auth, user, &surfaces).await?;
    assert!(matches!(outcome, OwnerEraseOutcome::Completed { .. }));
    assert!(
        index.inner.entries(None, 10).await?.entries.is_empty(),
        "hard erase must delete partially accepted first-time vectors before commit"
    );
    assert_eq!(debt_rows(&pool).await?, 0);
    Ok(())
}

/// Partial overwrite failure: if the host accepts v2 over v1 and a later item
/// fails, PostgreSQL rolls back to v1 and draining the write-ahead `'delete'`
/// intent removes uncommitted v2 from the host index so v2 never supplies a
/// search score; sweep then clears the lost v1 head so reconciliation rebuilds it.
#[tokio::test]
async fn partial_overwrite_failure_deletes_uncommitted_v2_from_index_on_drain() -> TestResult {
    let index = Arc::new(PartialFailBatchIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("partial_overwrite_rollback_drain", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let kept_owner = OwnerId::from(UserId::new(Uuid::now_v7()));

    let existing = seed_note(&pg, kept_owner, "wanted").await?;
    let fresh = seed_note(&pg, kept_owner, "wanted").await?;
    let v1 = [vector(0.2)];
    let v2 = [vector(0.99)];
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &kept_owner,
        entity: fact(existing),
        vectors: &v1,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await?;
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope = owner_scope_for_tests(&[kept_owner]);
    let before = pg
        .search_memories(
            Some(&scope),
            &request(kept_owner),
            registry.search_projections(),
        )
        .await?;
    assert_eq!(before.results.len(), 1);
    assert!(
        before.results[0].score < 0.5,
        "v1 must carry the low score before the failed overwrite: {}",
        before.results[0].score
    );

    index.fail_next.store(true, Ordering::SeqCst);
    let err = pg
        .insert_embeddings(&[
            EmbeddingWrite {
                owner: &kept_owner,
                entity: fact(existing),
                vectors: &v2,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            },
            EmbeddingWrite {
                owner: &kept_owner,
                entity: fact(fresh),
                vectors: &v2,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            },
        ])
        .await;
    assert!(matches!(err, Err(StorageError::Unavailable(_))));
    let pg_version: i32 = sqlx::query_scalar(
        "SELECT embedding_version FROM proxiima_core.embedding_heads WHERE entity_id = $1",
    )
    .bind(existing)
    .fetch_one(&pool)
    .await?;
    assert_eq!(pg_version, 1, "PostgreSQL rolled back the overwrite to v1");
    assert_eq!(
        index.inner.entries(None, 10).await?.entries[0].version,
        2,
        "host index accepted v2 before the later chunk failed"
    );
    // Even BEFORE draining debt, search admission checks `embedding_heads.vector_generation`
    // and rejects uncommitted v2's score (0.99).
    let before_drain = pg
        .search_memories(
            Some(&scope),
            &request(kept_owner),
            registry.search_projections(),
        )
        .await?;
    assert!(
        before_drain.results.is_empty(),
        "uncommitted v2 must never supply a search score after rollback even before draining"
    );

    assert_eq!(pg.drain_vector_index_debt(100).await?, 2);
    assert!(
        index.inner.entries(None, 10).await?.entries.is_empty(),
        "draining write-ahead delete intent must remove uncommitted v2 from the host index"
    );
    let after_drain = pg
        .search_memories(
            Some(&scope),
            &request(kept_owner),
            registry.search_projections(),
        )
        .await?;
    assert!(
        after_drain.results.is_empty(),
        "uncommitted v2 must never supply a search score after draining"
    );
    let swept = pg.sweep_orphan_embedding_rows().await?;
    assert_eq!(
        swept.heads_deleted, 1,
        "sweep removes the lost v1 head so reconciliation can rebuild it cleanly"
    );
    Ok(())
}

/// A partial first-time put under owner A leaves durable `'delete'` debt owned
/// by A while rolling back `embedding_heads`. Transferring the memory to owner
/// B keeps debt on its key (`TransferRule::StaysOnKey`), and hard-erasing B
/// still settles that foreign-owned debt before committing so no vector stays
/// stranded in the host index.
#[tokio::test]
async fn partial_put_then_transfer_then_hard_erase_settles_foreign_debt() -> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::{OwnerInversePort, OwnerTransferPort, OwnerWritePermit};

    let index = Arc::new(PartialFailBatchIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("partial_put_transfer_hard_erase", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let user_a = UserId::new(Uuid::now_v7());
    let user_b = UserId::new(Uuid::now_v7());
    let owner_a = OwnerId::from(user_a);
    let owner_b = OwnerId::from(user_b);
    create_user(&pool, user_b).await?;

    let first = seed_note(&pg, owner_a, "wanted").await?;
    let second = seed_note(&pg, owner_a, "wanted").await?;
    let vectors = [vector(1.0)];
    index.fail_next.store(true, Ordering::SeqCst);
    let err = pg
        .insert_embeddings(&[
            EmbeddingWrite {
                owner: &owner_a,
                entity: fact(first),
                vectors: &vectors,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            },
            EmbeddingWrite {
                owner: &owner_a,
                entity: fact(second),
                vectors: &vectors,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            },
        ])
        .await;
    assert!(matches!(err, Err(StorageError::Unavailable(_))));
    assert_eq!(index.inner.entries(None, 10).await?.entries.len(), 1);

    let space = EmbeddingSpace::new("memory-index", EmbeddingDim::D384);
    let permit =
        OwnerWritePermit::transfer_for_tests(owner_a, proxiima_core::MemoryKind::Fact, owner_b);
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );
    let moved = pg
        .transfer_to_owner(
            &permit,
            proxiima_core::EntityId::Memory(MemoryId::new(first)),
            owner_b,
            &surfaces,
            &[space],
        )
        .await?;
    assert!(moved);
    let debt_owner: Uuid = sqlx::query_scalar(
        "SELECT owner_id FROM proxiima_core.vector_index_debt WHERE entity_id = $1",
    )
    .bind(first)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        debt_owner,
        owner_a.into_inner(),
        "transfer preserves TransferRule::StaysOnKey on vector_index_debt"
    );

    let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
        user_id: user_b,
        drop_event_id: "transfer-foreign-debt-erase".into(),
    });
    let outcome = pg.erase_personal_owner(&auth, user_b, &surfaces).await?;
    assert!(matches!(outcome, OwnerEraseOutcome::Completed { .. }));
    assert!(
        index.inner.entries(None, 10).await?.entries.is_empty(),
        "B's hard erase must settle transferred-in foreign-owned debt before commit"
    );
    let remaining_debt: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM proxiima_core.vector_index_debt")
            .fetch_all(&pool)
            .await?;
    assert_eq!(
        remaining_debt,
        [second],
        "only owner A's un-transferred entity debt remains"
    );
    Ok(())
}

/// An index that writes the batch, pauses inside `put_batch`, and then fails
/// the call once released so tests can inspect committed write-ahead intent
/// before `put_batch` returns and race a waiting erase against rollback.
#[derive(Default)]
struct PausableFailIndex {
    inner: InMemoryVectorIndex,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl VectorIndex for PausableFailIndex {
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        self.inner.put_batch(entries).await?;
        self.entered.notify_one();
        self.release.notified().await;
        Err(StorageError::Unavailable("simulated failure after remote write".into()).into())
    }

    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        self.inner.delete(keys, fence).await
    }

    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }

    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_head(targets, is_head, revision).await
    }

    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.inner.nearest(filter, query, window).await
    }

    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        self.inner.entries(after, limit).await
    }
}

/// Write-ahead `'delete'` intent is committed on its own transaction BEFORE
/// `put_batch` runs: visible on another connection while `put_batch` is still
/// in flight, so a waiting erase settles the partially published vector even
/// when the publication transaction rolls back without any fallback write.
#[tokio::test]
async fn write_ahead_publish_intent_is_committed_before_put_batch_and_survives_rollback()
-> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::OwnerInversePort;

    let index = Arc::new(PausableFailIndex::default());
    let (pg, _database) = fresh_pg_host_index("write_ahead_intent_rollback", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let user = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(user);
    let note = seed_note(&pg, owner, "wanted").await?;

    let writer_pg = pg.clone();
    let writer = tokio::spawn(async move {
        let vectors = [vector(1.0)];
        writer_pg
            .insert_embeddings(&[EmbeddingWrite {
                owner: &owner,
                entity: fact(note),
                vectors: &vectors,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            }])
            .await
    });
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.entered.notified()).await?;

    // Before `put_batch` returns, the `'delete'` intent row is already durable
    // and visible to independent connections.
    let intent_op: String = sqlx::query_scalar(
        "SELECT op FROM proxiima_core.vector_index_debt
          WHERE entity_id = $1 AND model_id = 'memory-index' AND dim = 384",
    )
    .bind(note)
    .fetch_one(&pool)
    .await?;
    assert_eq!(intent_op, "delete");

    // Start an erase while the publisher still holds the owner fence, then
    // release `put_batch` to fail and roll back the publisher transaction.
    let erase_pg = pg.clone();
    let eraser = tokio::spawn(async move {
        let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
            user_id: user,
            drop_event_id: "write-ahead-erase".into(),
        });
        let surfaces = OwnerSurfaces::for_registry(
            &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
        );
        erase_pg.erase_personal_owner(&auth, user, &surfaces).await
    });
    tokio::task::yield_now().await;
    index.release.notify_one();

    let write_res = tokio::time::timeout(CONCURRENCY_PATIENCE, writer).await??;
    assert!(matches!(write_res, Err(StorageError::Unavailable(_))));
    let outcome = tokio::time::timeout(CONCURRENCY_PATIENCE, eraser).await???;
    assert!(matches!(outcome, OwnerEraseOutcome::Completed { .. }));
    assert!(
        index.inner.entries(None, 10).await?.entries.is_empty(),
        "waiting erase must delete the rolled-back write-ahead vector before commit"
    );
    assert_eq!(debt_rows(&pool).await?, 0);
    Ok(())
}

/// Source-scoped erase settles only its own fixed pre-locked entity set and
/// never locks or deletes pending debt belonging to an unrelated source of the
/// same owner.
#[tokio::test]
async fn source_scoped_erase_leaves_unrelated_source_debt_untouched() -> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::{FactIngestPort, OwnerInversePort, OwnerWritePermit};
    use proxiima_core::verbs::fact_ingest::FactWriteCommand;
    use proxiima_core::{MemoryKind, SchemaVersion, SourceId};

    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) = fresh_pg_host_index("source_erase_unrelated_debt", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let user = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(user);
    let permit = OwnerWritePermit::new_for_tests(owner, MemoryKind::Fact);

    let make_cmd = |source: &str, key: &str| FactWriteCommand {
        schema_id: SchemaId::new("core/test-fact-v1".to_owned()),
        schema_version: SchemaVersion::new(1),
        handle: None,
        source_id: Some(source.to_owned()),
        ingest_key: Some(key.to_owned()),
        payload: Vec::new(),
        rendered_text: None,
        lexical_language: None,
        receipt: None,
        occurred_at: None,
        citation: None,
        additional_references: Vec::new(),
        refs: Vec::new(),
        blob_id: None,
        kind: MemoryKind::Fact,
        request: None,
    };
    let drop_fact = pg
        .ingest_fact_atomic(&permit, &make_cmd("src-drop", "k-drop"), None)
        .await?;
    let keep_fact = pg
        .ingest_fact_atomic(&permit, &make_cmd("src-keep", "k-keep"), None)
        .await?;
    insert_claimless_vector(&pg, owner, drop_fact.memory_id).await?;
    insert_claimless_vector(&pg, owner, keep_fact.memory_id).await?;
    // Simulate pending debt on `src-keep` (e.g. a concurrent forget or head flip).
    sqlx::query(
        "INSERT INTO proxiima_core.vector_index_debt (entity_id, model_id, dim, owner_id, op)
         VALUES ($1, 'memory-index', 384, $2, 'head')",
    )
    .bind(keep_fact.memory_id.into_inner())
    .bind(owner.into_inner())
    .execute(&pool)
    .await?;

    let source = SourceId::new("src-drop");
    let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalSourceScope {
        user_id: user,
        source_id: source.clone(),
        drop_event_id: "src-drop-unrelated-debt".into(),
    });
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );
    let outcome = pg
        .erase_personal_source_scope(&auth, user, &source, &surfaces)
        .await?;
    assert!(matches!(outcome, OwnerEraseOutcome::Completed { .. }));

    let entries = index.entries(None, 10).await?.entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key, key(keep_fact.memory_id.into_inner()));
    let remaining_debt: Vec<Uuid> =
        sqlx::query_scalar("SELECT entity_id FROM proxiima_core.vector_index_debt")
            .fetch_all(&pool)
            .await?;
    assert_eq!(
        remaining_debt,
        [keep_fact.memory_id.into_inner()],
        "source-scoped erase must leave unrelated source debt untouched"
    );
    Ok(())
}

/// An index that inspects the maximum number of granted advisory locks held by
/// any active backend inside `delete` (which runs inside the erase transaction
/// before commit), verifying that whole-owner erase stays O(1) in advisory locks.
struct LockInspectingIndex {
    inner: InMemoryVectorIndex,
    pool: Mutex<Option<PgPool>>,
    max_advisory_locks: Mutex<Vec<i64>>,
}

impl LockInspectingIndex {
    fn new() -> Self {
        Self {
            inner: InMemoryVectorIndex::default(),
            pool: Mutex::new(None),
            max_advisory_locks: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl VectorIndex for LockInspectingIndex {
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        self.inner.put_batch(entries).await
    }

    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        let pool = self.pool.lock().unwrap().clone();
        if let Some(pool) = pool {
            let max_locks: i64 = sqlx::query_scalar(
                "SELECT COALESCE(max(cnt), 0)::bigint
                   FROM (
                       SELECT count(*) AS cnt
                         FROM pg_locks l
                         JOIN pg_stat_activity a ON a.pid = l.pid
                        WHERE a.datname = current_database()
                          AND l.locktype = 'advisory'
                          AND l.granted
                        GROUP BY l.pid
                   ) per_backend",
            )
            .fetch_one(&pool)
            .await
            .map_err(|err| StorageError::Internal(err.to_string()))?;
            self.max_advisory_locks.lock().unwrap().push(max_locks);
        }
        self.inner.delete(keys, fence).await
    }

    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }

    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_head(targets, is_head, revision).await
    }

    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.inner.nearest(filter, query, window).await
    }

    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        self.inner.entries(after, limit).await
    }
}

/// Whole-owner erase on a remote vector index holds a constant O(1) number of
/// advisory locks regardless of how many embedded memories the owner has.
#[tokio::test]
async fn whole_owner_erase_remote_index_holds_constant_advisory_locks_across_scale() -> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::OwnerInversePort;

    let index = Arc::new(LockInspectingIndex::new());
    let (pg, _database) = fresh_pg_host_index("whole_owner_erase_o1_locks", index.clone()).await;
    *index.pool.lock().unwrap() = Some(pg.pool_for_tests().clone());
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );

    let mut observed = Vec::new();
    for count in [5_usize, 50_usize] {
        let user = UserId::new(Uuid::now_v7());
        let owner = OwnerId::from(user);
        let mut ids = Vec::with_capacity(count);
        for _ in 0..count {
            ids.push(seed_note(&pg, owner, "wanted").await?);
        }
        let vectors = [vector(1.0)];
        let writes: Vec<EmbeddingWrite<'_>> = ids
            .iter()
            .map(|id| EmbeddingWrite {
                owner: &owner,
                entity: fact(*id),
                vectors: &vectors,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            })
            .collect();
        pg.insert_embeddings(&writes).await?;
        assert_eq!(index.inner.entries(None, 100).await?.entries.len(), count);

        index.max_advisory_locks.lock().unwrap().clear();
        let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
            user_id: user,
            drop_event_id: format!("o1-locks-{count}"),
        });
        let outcome = pg.erase_personal_owner(&auth, user, &surfaces).await?;
        assert!(matches!(outcome, OwnerEraseOutcome::Completed { .. }));
        assert!(index.inner.entries(None, 100).await?.entries.is_empty());
        let peak = index
            .max_advisory_locks
            .lock()
            .unwrap()
            .iter()
            .copied()
            .max()
            .expect("erase must call index.delete before commit");
        observed.push(peak);
    }

    assert_eq!(
        observed[0], observed[1],
        "advisory lock count during whole-owner erase must stay O(1) from 5 to 50 embedded memories: {observed:?}"
    );
    assert!(
        observed[1] <= 10,
        "advisory lock count must remain small and bounded: {observed:?}"
    );
    Ok(())
}

/// An index that writes the batch into its inner store first, then pauses
/// before returning `Ok(())` so tests can run concurrent searches while the
/// remote index holds an uncommitted generation.
#[derive(Default)]
struct PauseAfterWriteIndex {
    inner: InMemoryVectorIndex,
    arm_next_put: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl VectorIndex for PauseAfterWriteIndex {
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        self.inner.put_batch(entries).await?;
        if self.arm_next_put.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }

    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        self.inner.delete(keys, fence).await
    }

    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }

    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_head(targets, is_head, revision).await
    }

    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.inner.nearest(filter, query, window).await
    }

    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        self.inner.entries(after, limit).await
    }
}

/// Remote search verifies each candidate's `generation` against the committed
/// `embedding_heads.vector_generation`: while an overwrite (`v2`, score 0.99)
/// has already landed in the host index and is paused before phase 3 commits,
/// an authorized search never returns uncommitted v2's score; once phase 3
/// commits, the search admits v2 cleanly.
#[tokio::test]
async fn search_rejects_uncommitted_overwrite_score_while_put_batch_is_paused() -> TestResult {
    let index = Arc::new(PauseAfterWriteIndex::default());
    let (pg, _database) = fresh_pg_host_index("search_paused_uncommitted", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let existing = seed_note(&pg, owner, "wanted").await?;
    let v1 = [vector(0.2)];
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &owner,
        entity: fact(existing),
        vectors: &v1,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await?;

    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope = owner_scope_for_tests(&[owner]);
    let initial = pg
        .search_memories(Some(&scope), &request(owner), registry.search_projections())
        .await?;
    assert_eq!(initial.results.len(), 1);
    assert!(initial.results[0].score < 0.5);

    index.arm_next_put.store(true, Ordering::SeqCst);
    let writer_pg = pg.clone();
    let writer = tokio::spawn(async move {
        let v2 = [vector(0.99)];
        writer_pg
            .insert_embeddings(&[EmbeddingWrite {
                owner: &owner,
                entity: fact(existing),
                vectors: &v2,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            }])
            .await
    });
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.entered.notified()).await?;

    // The host index already holds v2, but PostgreSQL still holds v1's generation.
    let entries = index.inner.entries(None, 10).await?.entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].version, 2);
    let while_paused = pg
        .search_memories(Some(&scope), &request(owner), registry.search_projections())
        .await?;
    assert!(
        while_paused.results.is_empty(),
        "search must never return uncommitted v2's score while put_batch is open"
    );

    index.release.notify_one();
    tokio::time::timeout(CONCURRENCY_PATIENCE, writer).await???;

    let after_commit = pg
        .search_memories(Some(&scope), &request(owner), registry.search_projections())
        .await?;
    assert_eq!(after_commit.results.len(), 1);
    assert!(
        after_commit.results[0].score > 0.9,
        "committed v2 must become searchable with its 0.99 score: {}",
        after_commit.results[0].score
    );
    Ok(())
}

/// Monotonic generations plus provider-side `delete(keys, fence)` fencing stop
/// a delayed in-flight `put_batch` from resurrecting an erased vector after
/// hard erase completes.
#[tokio::test]
async fn delayed_in_flight_put_is_fenced_after_hard_erase() -> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::OwnerInversePort;

    let index = Arc::new(PausableIndex::default());
    let (pg, _database) = fresh_pg_host_index("delayed_put_fenced_erase", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );

    let user_a = UserId::new(Uuid::now_v7());
    let owner_a = OwnerId::from(user_a);
    let note_a = seed_note(&pg, owner_a, "wanted").await?;

    index.arm();
    let writer_pg = pg.clone();
    let writer = tokio::spawn(async move {
        let vectors = [vector(1.0)];
        writer_pg
            .insert_embeddings(&[EmbeddingWrite {
                owner: &owner_a,
                entity: fact(note_a),
                vectors: &vectors,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            }])
            .await
    });
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.entered.notified()).await?;

    let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
        user_id: user_a,
        drop_event_id: "delayed-put-erase".into(),
    });
    let outcome = pg.erase_personal_owner(&auth, user_a, &surfaces).await?;
    assert!(matches!(outcome, OwnerEraseOutcome::Completed { .. }));
    assert!(index.inner.entries(None, 10).await?.entries.is_empty());

    // Release the delayed in-flight put AFTER hard erase already completed.
    index.release.notify_one();
    let _ = tokio::time::timeout(CONCURRENCY_PATIENCE, writer).await??;
    assert!(
        index.inner.entries(None, 10).await?.entries.is_empty(),
        "provider delete fence must reject a delayed put arriving after hard erase completion"
    );
    let heads_a: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM proxiima_core.embedding_heads WHERE entity_id = $1",
    )
    .bind(note_a)
    .fetch_one(&pool)
    .await?;
    assert_eq!(heads_a, 0);
    Ok(())
}

/// Monotonic generations stop a delayed in-flight `put_batch` from overwriting
/// a newer successful retry on the host index or downgrading `embedding_heads`.
#[tokio::test]
async fn delayed_in_flight_put_is_fenced_after_retry() -> TestResult {
    let index = Arc::new(PausableIndex::default());
    let (pg, _database) = fresh_pg_host_index("delayed_put_fenced_retry", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner_b = OwnerId::from(UserId::new(Uuid::now_v7()));
    let note_b = seed_note(&pg, owner_b, "wanted").await?;
    index.arm();
    let stale_pg = pg.clone();
    let stale_writer = tokio::spawn(async move {
        let stale = [vector(0.2)];
        stale_pg
            .insert_embeddings(&[EmbeddingWrite {
                owner: &owner_b,
                entity: fact(note_b),
                vectors: &stale,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            }])
            .await
    });
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.entered.notified()).await?;

    // Retry commits a newer generation with score 0.99 while the stale put is stalled.
    let retry_vec = [vector(0.99)];
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &owner_b,
        entity: fact(note_b),
        vectors: &retry_vec,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await?;
    let committed_gen: i64 = sqlx::query_scalar(
        "SELECT vector_generation FROM proxiima_core.embedding_heads WHERE entity_id = $1",
    )
    .bind(note_b)
    .fetch_one(&pool)
    .await?;

    // Release the stalled lower-generation put: provider ignores it and phase 3 rejects it.
    index.release.notify_one();
    let stale_res = tokio::time::timeout(CONCURRENCY_PATIENCE, stale_writer).await??;
    assert!(
        matches!(stale_res, Err(StorageError::Retryable(_))),
        "superseded stale writer must fail retryably without downgrading the head: {stale_res:?}"
    );
    let entries = index.inner.entries(None, 10).await?.entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].generation, committed_gen);
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope_b = owner_scope_for_tests(&[owner_b]);
    let page = pg
        .search_memories(
            Some(&scope_b),
            &request(owner_b),
            registry.search_projections(),
        )
        .await?;
    assert_eq!(page.results.len(), 1);
    assert!(
        page.results[0].score > 0.9,
        "late put must not overwrite the successful retry's 0.99 vector: {}",
        page.results[0].score
    );
    Ok(())
}

/// A failed overwrite leaves `'delete'` debt over an existing v1 head; a
/// subsequent transfer to owner B updates `embedding_heads.owner_id` and must
/// preserve `'delete'` instead of downgrading it to `'owner'`, so draining
/// removes uncommitted v2 rather than moving v2 to owner B.
#[tokio::test]
async fn failed_overwrite_then_transfer_preserves_delete_intent_on_drain() -> TestResult {
    use proxiima_core::owner_inverse::OwnerSurfaces;
    use proxiima_core::storage_ports::{OwnerTransferPort, OwnerWritePermit};

    let index = Arc::new(PartialFailBatchIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("failed_overwrite_transfer_drain", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner_a = OwnerId::from(UserId::new(Uuid::now_v7()));
    let owner_b = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner_b.as_user()).await?;

    let existing = seed_note(&pg, owner_a, "wanted").await?;
    let fresh = seed_note(&pg, owner_a, "wanted").await?;
    let v1 = [vector(0.2)];
    let v2 = [vector(0.99)];
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &owner_a,
        entity: fact(existing),
        vectors: &v1,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await?;

    index.fail_next.store(true, Ordering::SeqCst);
    let err = pg
        .insert_embeddings(&[
            EmbeddingWrite {
                owner: &owner_a,
                entity: fact(existing),
                vectors: &v2,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            },
            EmbeddingWrite {
                owner: &owner_a,
                entity: fact(fresh),
                vectors: &v2,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            },
        ])
        .await;
    assert!(matches!(err, Err(StorageError::Unavailable(_))));
    assert_eq!(index.inner.entries(None, 10).await?.entries[0].version, 2);

    let space = EmbeddingSpace::new("memory-index", EmbeddingDim::D384);
    let permit =
        OwnerWritePermit::transfer_for_tests(owner_a, proxiima_core::MemoryKind::Fact, owner_b);
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );
    let moved = pg
        .transfer_to_owner(
            &permit,
            proxiima_core::EntityId::Memory(MemoryId::new(existing)),
            owner_b,
            &surfaces,
            &[space],
        )
        .await?;
    assert!(moved);

    let op: String = sqlx::query_scalar(
        "SELECT op FROM proxiima_core.vector_index_debt
          WHERE entity_id = $1 AND model_id = 'memory-index' AND dim = 384",
    )
    .bind(existing)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        op, "delete",
        "owner-move trigger must preserve delete intent over a failed overwrite"
    );

    assert_eq!(pg.drain_vector_index_debt(100).await?, 2);
    assert!(
        index.inner.entries(None, 10).await?.entries.is_empty(),
        "draining after transfer must delete uncommitted v2 rather than moving it to owner B"
    );
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope_b = owner_scope_for_tests(&[owner_b]);
    let page = pg
        .search_memories(
            Some(&scope_b),
            &request(owner_b),
            registry.search_projections(),
        )
        .await?;
    assert!(page.results.is_empty());
    Ok(())
}

/// Remote publication holds at most one platform connection at a time (and zero
/// across `put_batch`), so it makes progress both on a 1-connection platform
/// pool (`minPoolConnections = 1`) and when two separately constructed stores
/// share a 2-connection platform pool while a competing transaction holds one
/// of the two connections.
#[tokio::test]
async fn remote_publish_succeeds_with_single_connection_pool_and_shared_two_conn_pool() -> TestResult
{
    use proxiima_storage_pg::{PgPlatformScope, PgPoolConfig, PgTuning};

    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) = fresh_pg_host_index("remote_publish_pool_bounds", index.clone()).await;
    let db_name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pg.pool_for_tests())
        .await?;
    let (runtime_url, platform_url) =
        proxiima_pg_testkit::split_role_urls_without_pgvector(&db_name).await?;

    // 1. Single-connection platform pool (`max_connections = 1`).
    let single_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(&platform_url)
        .await?;
    let single_scope = PgPlatformScope::new(single_pool, &["proxiima_core"]).await?;
    let single_store = PgStorage::connect_with_vector_index(
        &runtime_url,
        PgPoolConfig::default(),
        PgTuning::default(),
        single_scope,
        1,
        index.clone(),
    )
    .await?;
    let owner1 = OwnerId::from(UserId::new(Uuid::now_v7()));
    let note1 = seed_note(&pg, owner1, "wanted").await?;
    let vectors = [vector(1.0)];
    single_store
        .insert_embeddings(&[EmbeddingWrite {
            owner: &owner1,
            entity: fact(note1),
            vectors: &vectors,
            proof: EmbeddingWriteProof::for_direct_write_for_tests(),
        }])
        .await?;

    // 2. Two separate `PgStorage` instances sharing ONE 2-connection platform
    //    pool while a competing transaction holds 1 of the 2 connections.
    let shared_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(&platform_url)
        .await?;
    let shared_scope = PgPlatformScope::new(shared_pool.clone(), &["proxiima_core"]).await?;
    let store_a = PgStorage::connect_with_vector_index(
        &runtime_url,
        PgPoolConfig::default(),
        PgTuning::default(),
        shared_scope.clone(),
        1,
        index.clone(),
    )
    .await?;
    let store_b = PgStorage::connect_with_vector_index(
        &runtime_url,
        PgPoolConfig::default(),
        PgTuning::default(),
        shared_scope.clone(),
        1,
        index.clone(),
    )
    .await?;
    let owner2 = OwnerId::from(UserId::new(Uuid::now_v7()));
    let owner3 = OwnerId::from(UserId::new(Uuid::now_v7()));
    let note2 = seed_note(&pg, owner2, "wanted").await?;
    let note3 = seed_note(&pg, owner3, "wanted").await?;

    // Hold 1 of the 2 platform connections in an unrelated competing transaction.
    let competing_tx = shared_scope.begin().await?;
    let write_a = [EmbeddingWrite {
        owner: &owner2,
        entity: fact(note2),
        vectors: &vectors,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }];
    let write_b = [EmbeddingWrite {
        owner: &owner3,
        entity: fact(note3),
        vectors: &vectors,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }];
    let (res_a, res_b) = tokio::join!(
        store_a.insert_embeddings(&write_a),
        store_b.insert_embeddings(&write_b),
    );
    competing_tx.commit().await?;
    res_a?;
    res_b?;
    assert_eq!(index.entries(None, 10).await?.entries.len(), 3);
    Ok(())
}

/// An index that pauses `put_batch` on demand and can fail `set_head` after
/// `delete` has already applied a provider fence, proving durable intent
/// invalidation prevents ghost head commits after a partially failed drain.
#[derive(Default)]
struct DrainFailureRaceIndex {
    inner: InMemoryVectorIndex,
    pause_next_put: std::sync::atomic::AtomicBool,
    fail_next_set_head: std::sync::atomic::AtomicBool,
    put_entered: tokio::sync::Notify,
    put_release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl VectorIndex for DrainFailureRaceIndex {
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        if self.pause_next_put.swap(false, Ordering::SeqCst) {
            self.put_entered.notify_one();
            self.put_release.notified().await;
        }
        self.inner.put_batch(entries).await
    }

    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        self.inner.delete(keys, fence).await
    }

    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }

    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        if self.fail_next_set_head.swap(false, Ordering::SeqCst) {
            return Err(StorageError::Unavailable(
                "injected set_head failure after delete applied fence".into(),
            ));
        }
        self.inner.set_head(targets, is_head, revision).await
    }

    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.inner.nearest(filter, query, window).await
    }

    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        self.inner.entries(after, limit).await
    }
}

/// If `drain_vector_index_debt` applies a provider delete fence while a
/// publisher's `put_batch(g)` is in flight and then fails on a later RPC in
/// the same pass, the durable pre-RPC intent invalidation ensures Tx 2 fails
/// with `Retryable` rather than committing a head and completing the job claim
/// with no vector in the index.
#[tokio::test]
async fn failed_drain_after_provider_delete_invalidates_in_flight_publish_intent() -> TestResult {
    let index = Arc::new(DrainFailureRaceIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("failed_drain_invalidates_intent", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let bystander = seed_note(&pg, owner, "wanted").await?;
    embed(&pg, owner, bystander, 1).await?;
    let target = seed_note(&pg, owner, "wanted").await?;
    sqlx::query(
        "INSERT INTO proxiima_core.embedding_jobs (entity_id, model_id, dim, owner_id, status)
         VALUES ($1, 'memory-index', 384, $2, 'pending')",
    )
    .bind(target)
    .bind(owner.into_inner())
    .execute(&pool)
    .await?;
    let claims = pg.claim_pending_embedding_jobs(10, &[], &[]).await?;
    let claim = claims
        .into_iter()
        .find(|c| c.entity_id.into_inner() == target)
        .expect("claimed job for target");
    // Enqueue a `'head'` debt row on `bystander` so the expired-lease drain
    // runs both `delete` (for `target`) and `set_head` (which fails).
    index.pause_next_put.store(true, Ordering::SeqCst);
    let pub_pg = pg.clone();
    let publisher = tokio::spawn(async move {
        let vectors = [vector(1.0)];
        pub_pg
            .insert_embeddings(&[EmbeddingWrite {
                owner: &owner,
                entity: fact(target),
                vectors: &vectors,
                proof: EmbeddingWriteProof::for_claim_for_tests(&claim),
            }])
            .await
    });
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.put_entered.notified()).await?;

    // Fresh lease: ordinary drain skips `target` and leaves its intent intact.
    assert_eq!(pg.drain_vector_index_debt(100).await?, 0);

    // Expired lease: ordinary drain reclaims `target` and fails on `bystander`.
    sqlx::query(
        "INSERT INTO proxiima_core.vector_index_debt (entity_id, model_id, dim, owner_id, op)
         VALUES ($1, 'memory-index', 384, $2, 'head')",
    )
    .bind(bystander)
    .bind(owner.into_inner())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE proxiima_core.vector_index_debt
            SET enqueued_at = now() - interval '120 seconds'
          WHERE entity_id = $1",
    )
    .bind(target)
    .execute(&pool)
    .await?;

    index.fail_next_set_head.store(true, Ordering::SeqCst);
    let drain_err = pg.drain_vector_index_debt(100).await;
    assert!(
        matches!(drain_err, Err(StorageError::Unavailable(_))),
        "injected set_head failure must surface: {drain_err:?}"
    );
    index.put_release.notify_one();

    let pub_res = tokio::time::timeout(CONCURRENCY_PATIENCE, publisher).await??;
    assert!(
        matches!(pub_res, Err(StorageError::Retryable(_))),
        "publisher must fail Retryable after drain durably invalidated its intent: {pub_res:?}"
    );
    let (heads, jobs): (i64, i64) = sqlx::query_as(
        "SELECT
             (SELECT count(*) FROM proxiima_core.embedding_heads WHERE entity_id = $1),
             (SELECT count(*) FROM proxiima_core.embedding_jobs WHERE entity_id = $1)",
    )
    .bind(target)
    .fetch_one(&pool)
    .await?;
    assert_eq!(heads, 0, "no ghost head may commit for a fenced put");
    assert_eq!(jobs, 1, "the job claim must remain uncompleted for retry");
    Ok(())
}

/// When a memory failed to publish under owner A and was transferred to owner B
/// (leaving A-owned debt under `TransferRule::StaysOnKey`), a concurrent
/// whole-owner erase of A locks that transferred-out key before allocating its
/// delete fence so B's concurrent publication can never commit a head whose
/// generation was suppressed by A's delete fence.
#[tokio::test]
async fn whole_owner_erase_of_old_owner_serializes_with_destination_publish_of_transferred_key()
-> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::{OwnerInversePort, OwnerTransferPort, OwnerWritePermit};

    let index = Arc::new(PartialFailBatchIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("erase_old_owner_vs_dest_publish", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let user_a = UserId::new(Uuid::now_v7());
    let user_b = UserId::new(Uuid::now_v7());
    let owner_a = OwnerId::from(user_a);
    let owner_b = OwnerId::from(user_b);
    create_user(&pool, user_b).await?;

    let transferred = seed_note(&pg, owner_a, "wanted").await?;
    let filler = seed_note(&pg, owner_a, "wanted").await?;
    let vectors = [vector(1.0)];
    index.fail_next.store(true, Ordering::SeqCst);
    let err = pg
        .insert_embeddings(&[
            EmbeddingWrite {
                owner: &owner_a,
                entity: fact(transferred),
                vectors: &vectors,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            },
            EmbeddingWrite {
                owner: &owner_a,
                entity: fact(filler),
                vectors: &vectors,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            },
        ])
        .await;
    assert!(matches!(err, Err(StorageError::Unavailable(_))));

    let space = EmbeddingSpace::new("memory-index", EmbeddingDim::D384);
    let permit =
        OwnerWritePermit::transfer_for_tests(owner_a, proxiima_core::MemoryKind::Fact, owner_b);
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );
    assert!(
        pg.transfer_to_owner(
            &permit,
            proxiima_core::EntityId::Memory(MemoryId::new(transferred)),
            owner_b,
            &surfaces,
            &[space],
        )
        .await?
    );

    // Race whole-owner erase of A against B publishing `transferred`.
    let erase_pg = pg.clone();
    let erase_surfaces = surfaces.clone();
    let eraser = tokio::spawn(async move {
        let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
            user_id: user_a,
            drop_event_id: "old-owner-erase-race".into(),
        });
        erase_pg
            .erase_personal_owner(&auth, user_a, &erase_surfaces)
            .await
    });
    let pub_pg = pg.clone();
    let publisher = tokio::spawn(async move {
        let vecs = [vector(0.95)];
        loop {
            match pub_pg
                .insert_embeddings(&[EmbeddingWrite {
                    owner: &owner_b,
                    entity: fact(transferred),
                    vectors: &vecs,
                    proof: EmbeddingWriteProof::for_direct_write_for_tests(),
                }])
                .await
            {
                Ok(outcomes) => return Ok(outcomes),
                Err(StorageError::Retryable(_)) => {}
                Err(other) => return Err(other),
            }
        }
    });
    let (erase_res, pub_res) = tokio::join!(eraser, publisher);
    assert!(matches!(erase_res??, OwnerEraseOutcome::Completed { .. }));
    assert_eq!(pub_res??[0].embedding_version, 1);

    let head_gen: i64 = sqlx::query_scalar(
        "SELECT vector_generation FROM proxiima_core.embedding_heads WHERE entity_id = $1",
    )
    .bind(transferred)
    .fetch_one(&pool)
    .await?;
    let entries = index.inner.entries(None, 10).await?.entries;
    assert_eq!(
        entries.len(),
        1,
        "B's committed head must have a matching live vector in the index"
    );
    assert_eq!(entries[0].key.entity_id, transferred);
    assert_eq!(entries[0].owner, owner_b);
    assert_eq!(entries[0].generation, head_gen);
    Ok(())
}

/// Source-scoped erase locks retained series predecessors for head promotion
/// without deadlock, but restricts destructive debt settlement to the erased
/// entities so a retained predecessor's in-flight overwrite is never deleted or
/// fenced.
#[tokio::test]
async fn source_erase_preserves_retained_predecessors_in_flight_overwrite() -> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::{FactIngestPort, OwnerInversePort, OwnerWritePermit};
    use proxiima_core::{MemoryKind, SourceId};

    let index = Arc::new(DrainFailureRaceIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("source_erase_keeps_predecessor_overwrite", index.clone()).await;
    let user = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(user);
    let permit = OwnerWritePermit::new_for_tests(owner, MemoryKind::Fact);

    let v1 = pg
        .ingest_fact_atomic(&permit, &sourced_fact_cmd("src-keep", "k1", None), None)
        .await?;
    let v2 = pg
        .ingest_fact_atomic(
            &permit,
            &sourced_fact_cmd("src-drop", "k2", Some(v1.handle)),
            None,
        )
        .await?;
    for id in [v1.memory_id, v2.memory_id] {
        insert_claimless_vector(&pg, owner, id).await?;
    }

    // Pause an in-flight overwrite of retained predecessor `v1` mid-`put_batch`.
    index.pause_next_put.store(true, Ordering::SeqCst);
    let pub_pg = pg.clone();
    let v1_id = v1.memory_id.into_inner();
    let publisher = tokio::spawn(async move {
        let vecs = [vector(0.95)];
        pub_pg
            .insert_embeddings(&[EmbeddingWrite {
                owner: &owner,
                entity: fact(v1_id),
                vectors: &vecs,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            }])
            .await
    });
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.put_entered.notified()).await?;

    // Erase `src-drop` (`v2`), promoting `v1` back to series head while `v1`'s overwrite is open.
    let source = SourceId::new("src-drop");
    let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalSourceScope {
        user_id: user,
        source_id: source.clone(),
        drop_event_id: "src-drop-keep-v1-overwrite".into(),
    });
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );
    let outcome = pg
        .erase_personal_source_scope(&auth, user, &source, &surfaces)
        .await?;
    assert!(matches!(outcome, OwnerEraseOutcome::Completed { .. }));

    // Release `v1`'s overwrite: it must succeed without being fenced or deleted by `src-drop`'s erase.
    index.put_release.notify_one();
    let outcomes = tokio::time::timeout(CONCURRENCY_PATIENCE, publisher).await???;
    assert_eq!(outcomes[0].embedding_version, 2);
    // Drain the `'head'` debt enqueued by Tx 2 because `v1` became head while `put_batch` ran.
    pg.drain_vector_index_debt(100).await?;

    let entries = index.inner.entries(None, 10).await?.entries;
    assert_eq!(entries.len(), 1, "only v1 survives after v2 is erased");
    assert_eq!(entries[0].key.entity_id, v1_id);
    assert_eq!(entries[0].version, 2);
    assert!(
        entries[0].is_head,
        "retained predecessor must be marked series head after drain"
    );
    Ok(())
}

/// An index wrapper that captures the first `set_head(false)` call without
/// forwarding it so a test can deliver it late after a newer `set_head(true)`
/// repair has already applied.
#[derive(Default)]
struct DelayFirstHeadFalseIndex {
    inner: InMemoryVectorIndex,
    delayed: Mutex<Option<(Vec<VectorMetadataTarget>, bool, i64)>>,
}

#[async_trait::async_trait]
impl VectorIndex for DelayFirstHeadFalseIndex {
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        self.inner.put_batch(entries).await
    }

    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        self.inner.delete(keys, fence).await
    }

    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }

    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        if !is_head {
            let mut slot = self.delayed.lock().unwrap();
            if slot.is_none() {
                *slot = Some((targets.to_vec(), is_head, revision));
                return Ok(());
            }
        }
        self.inner.set_head(targets, is_head, revision).await
    }

    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.inner.nearest(filter, query, window).await
    }

    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        self.inner.entries(after, limit).await
    }
}

/// A delayed `set_head(false, rev1)` from an earlier supersession cannot hide
/// a memory whose series head was later restored and drained at `rev2 > rev1`
/// within the same vector generation.
#[tokio::test]
async fn delayed_set_head_false_cannot_clobber_restored_head() -> TestResult {
    use proxiima_core::MemoryKind;
    use proxiima_core::storage_ports::{OwnerWritePermit, WriteSessionFactory};

    let index = Arc::new(DelayFirstHeadFalseIndex::default());
    let (pg, _database) = fresh_pg_host_index("delayed_set_head_false_fenced", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let v1 = seed_note(&pg, owner, "wanted").await?;
    embed(&pg, owner, v1, 1).await?;

    // Supersede v1 with v2 on the same series handle, then drain (capturing `set_head(false, rev1)`).
    let handle: Uuid =
        sqlx::query_scalar("SELECT handle FROM proxiima_core.live_memory WHERE t = $1")
            .bind(v1)
            .fetch_one(&pool)
            .await?;
    let v2 = TestNote::new("v2", "body")
        .tags(&["wanted"])
        .handle(handle)
        .create(&pg, owner)
        .await?;
    assert_eq!(pg.drain_vector_index_debt(100).await?, 1);
    let (delayed_targets, delayed_flag, delayed_rev) = index
        .delayed
        .lock()
        .unwrap()
        .take()
        .expect("captured delayed set_head(false)");
    assert!(!delayed_flag);

    // Forget v2 so v1 is promoted back to head, and drain (`set_head(true, rev2)`).
    let scope = owner_scope_for_tests(&[owner]);
    let mut session = pg.begin(Some(&scope)).await?;
    let permit = OwnerWritePermit::new_for_tests(owner, MemoryKind::Fact);
    session.forget_memory(&permit, v2.memory_id).await?;
    session.commit().await?;
    assert_eq!(pg.drain_vector_index_debt(100).await?, 1);

    // Now deliver the delayed `set_head(false, rev1)` onto the provider.
    index
        .inner
        .set_head(&delayed_targets, delayed_flag, delayed_rev)
        .await?;

    let entries = index.inner.entries(None, 10).await?.entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key.entity_id, v1);
    assert!(
        entries[0].is_head,
        "delayed set_head(false) must not overwrite newer set_head(true)"
    );
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let page = pg
        .search_memories(Some(&scope), &request(owner), registry.search_projections())
        .await?;
    assert_eq!(page.results.len(), 1);
    assert_eq!(page.results[0].memory_id.into_inner(), v1);
    Ok(())
}

/// Drain every currently pending embedding job through the real
/// `claim_pending_embedding_jobs -> load_embedding_texts -> insert_embeddings`
/// claim-proof path (no direct-write bypass), returning the published entity IDs.
async fn drain_claimed_jobs_real(
    pg: &proxiima_storage_pg::PgStorage,
    owner: OwnerId,
) -> Result<Vec<Uuid>, Box<dyn std::error::Error>> {
    use proxiima_core::storage_ports::{EmbeddingJobPort, EmbeddingTextPort};
    use proxiima_core::verbs::query::EntityKind;

    let claims = pg.claim_pending_embedding_jobs(50, &[], &[]).await?;
    let items: Vec<_> = claims
        .iter()
        .map(|claim| {
            (
                claim.owner,
                EntityKind::from(claim.entity_kind),
                claim.entity_id,
            )
        })
        .collect();
    let scope = owner_scope_for_tests(&[owner]);
    let texts = pg.load_embedding_texts(Some(&scope), &items, &[]).await?;
    let vectors = [vector(0.95)];
    let mut published = Vec::with_capacity(claims.len());
    for (claim, text) in claims.iter().zip(texts.iter()) {
        assert!(
            text.as_deref().is_some_and(|t| !t.is_empty()),
            "claimed entity {:?} must resolve embeddable text",
            claim.entity_id
        );
        let write = EmbeddingWrite {
            owner: &claim.owner,
            entity: fact(claim.entity_id.into_inner()),
            vectors: &vectors,
            proof: EmbeddingWriteProof::for_claim_for_tests(claim),
        };
        pg.insert_embeddings(&[write]).await?;
        published.push(claim.entity_id.into_inner());
    }
    Ok(published)
}

/// Hybrid search rejects an uncommitted remote overwrite score while the write
/// is paused mid-`put_batch`, while preserving the memory's independent lexical
/// match; once the overwrite commits, hybrid search returns its new score.
#[tokio::test]
async fn hybrid_search_rejects_uncommitted_remote_score_while_preserving_lexical_hit() -> TestResult
{
    let index = Arc::new(PauseAfterWriteIndex::default());
    let (pg, _database) = fresh_pg_host_index("hybrid_paused_uncommitted", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let existing = seed_note(&pg, owner, "wanted").await?;
    let v1 = [vector(0.2)];
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &owner,
        entity: fact(existing),
        vectors: &v1,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await?;

    index.arm_next_put.store(true, Ordering::SeqCst);
    let writer_pg = pg.clone();
    let writer = tokio::spawn(async move {
        let v2 = [vector(0.99)];
        writer_pg
            .insert_embeddings(&[EmbeddingWrite {
                owner: &owner,
                entity: fact(existing),
                vectors: &v2,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            }])
            .await
    });
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.entered.notified()).await?;

    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope = owner_scope_for_tests(&[owner]);
    let mut hybrid_req = request(owner);
    hybrid_req.mode = SearchMode::Hybrid;
    hybrid_req.query = Some("Index result".into());

    let while_paused = pg
        .search_memories(Some(&scope), &hybrid_req, registry.search_projections())
        .await?;
    assert_eq!(while_paused.results.len(), 1);
    assert!(
        while_paused.results[0].lexical_score > 0.0,
        "lexical match must survive while remote semantic write is uncommitted"
    );
    assert_eq!(
        while_paused.results[0].similarity_score.to_bits(),
        0.0_f32.to_bits(),
        "uncommitted v2 score must not leak into hybrid search"
    );

    index.release.notify_one();
    tokio::time::timeout(CONCURRENCY_PATIENCE, writer).await???;
    let after_commit = pg
        .search_memories(Some(&scope), &hybrid_req, registry.search_projections())
        .await?;
    assert_eq!(after_commit.results.len(), 1);
    assert!(after_commit.results[0].similarity_score > 0.9);
    Ok(())
}

/// Fresh-namespace cutover preserves semantic coverage for the entire
/// previously-indexed live version set — current heads, superseded versions,
/// and Goal-published older versions — while `reconcile_embeddings` also
/// queues live heads that had no prior `embedding_heads` row, all drained
/// through the real claim-proof pipeline.
#[tokio::test]
async fn fresh_namespace_cutover_rebuilds_heads_superseded_and_goal_versions_via_reconcile()
-> TestResult {
    use proxiima_core::{EmbeddingReconcileOptions, EmbeddingReconcileScope};

    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) = fresh_pg_host_index("fresh_namespace_cutover", index.clone()).await;
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let pg = pg.with_spiices(&registry);
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let guest = UserId::new(Uuid::now_v7());

    // Seed a series where v1 is published to a Goal and privately superseded by v2,
    // plus an unembedded live head with no prior `embedding_heads` row.
    let v1 = TestNote::new("Index result", "Published v1")
        .tags(&["wanted"])
        .create(&pg, owner)
        .await?;
    let v1_id = v1.memory_id.into_inner();
    publish_semantic_fixture(&pg, owner, guest, v1_id, AccessCeiling::Fact).await?;
    let v2 = TestNote::new("Index result", "Private v2")
        .handle(v1.handle)
        .tags(&["wanted"])
        .create(&pg, owner)
        .await?;
    let v2_id = v2.memory_id.into_inner();
    let unembedded = seed_note(&pg, owner, "wanted").await?;

    for id in [v1_id, v2_id] {
        sqlx::query(
            "INSERT INTO proxiima_core.embedding_heads
                 (entity_id, model_id, dim, embedding_version, owner_id, vector_generation)
             VALUES ($1, 'memory-index', 384, 1, $2, 0)",
        )
        .bind(id)
        .bind(owner.into_inner())
        .execute(&pool)
        .await?;
    }
    // Pre-existing `'head'` debt on the superseded Goal-published version `v1`:
    // settling debt before the full sweep prevents sweep from skipping `v1`.
    sqlx::query(
        "INSERT INTO proxiima_core.vector_index_debt
             (entity_id, model_id, dim, owner_id, op, generation)
         VALUES ($1, 'memory-index', 384, $2, 'head', 0)",
    )
    .bind(v1_id)
    .bind(owner.into_inner())
    .execute(&pool)
    .await?;

    assert_eq!(pg.drain_vector_index_debt(100).await?, 1);
    assert_eq!(pg.sweep_orphan_embedding_rows().await?.heads_deleted, 2);
    let index_space = EmbeddingSpace::new("memory-index", EmbeddingDim::D384);
    let reconciled = pg
        .reconcile_embeddings(EmbeddingReconcileOptions {
            space: &index_space,
            scope: EmbeddingReconcileScope::MissingOnly,
            owners: Some(&[owner]),
            non_embeddable_schemas: &[],
            limit: Some(100),
        })
        .await?;
    assert_eq!(reconciled.enqueued, 1);
    assert_eq!(drain_claimed_jobs_real(&pg, owner).await?.len(), 3);

    let owner_scope = owner_scope_for_tests(&[owner]);
    let mut owner_heads = request(owner);
    owner_heads.limit = 10;
    let heads_page = pg
        .search_memories(
            Some(&owner_scope),
            &owner_heads,
            registry.search_projections(),
        )
        .await?;
    let head_ids: Vec<Uuid> = heads_page
        .results
        .iter()
        .map(|r| r.memory_id.into_inner())
        .collect();
    assert!(
        head_ids.contains(&v2_id) && head_ids.contains(&unembedded) && !head_ids.contains(&v1_id)
    );

    let mut owner_all = owner_heads;
    owner_all.supersession = SupersessionStatus::IncludeSuperseded;
    let all_page = pg
        .search_memories(
            Some(&owner_scope),
            &owner_all,
            registry.search_projections(),
        )
        .await?;
    assert!(all_page.results.iter().any(|r| r.memory_id == v1.memory_id));

    let guest_scope = proxiima_core::test_fixtures::authenticated_context(
        proxiima_core::AuthzContext::for_subject(guest, proxiima_core::AuthPath::HostBearer),
    )
    .owner_scope()
    .expect("guest scope")
    .clone();
    let goal_page = pg
        .search_memories(
            Some(&guest_scope),
            &readable_request(owner),
            registry.search_projections(),
        )
        .await?;
    assert_eq!(goal_page.results.len(), 1);
    assert_eq!(goal_page.results[0].memory_id, v1.memory_id);
    Ok(())
}

/// Coordinated in-place backfill and PostgreSQL restore behind a retained
/// provider whose counters include generations, delete fences, and higher
/// metadata revisions (`head_revision = 100` on a restored `generation = 5`
/// entry). Advancing `vector_index_generation_seq` past the combined high-water
/// mark (100) lets sweep repair drifted metadata and reconcile rebuild lost heads.
#[tokio::test]
async fn coordinated_backfill_and_retained_provider_restore_cutover() -> TestResult {
    use proxiima_core::{EmbeddingReconcileOptions, EmbeddingReconcileScope};

    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) = fresh_pg_host_index("retained_restore_cutover", index.clone()).await;
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let pg = pg.with_spiices(&registry);
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let scope = owner_scope_for_tests(&[owner]);

    let backfilled = seed_note(&pg, owner, "wanted").await?;
    let restored_head = seed_note(&pg, owner, "wanted").await?;
    for (id, generation) in [(backfilled, 25_i64), (restored_head, 5_i64)] {
        sqlx::query(
            "INSERT INTO proxiima_core.embedding_heads
                 (entity_id, model_id, dim, embedding_version, owner_id, vector_generation)
             VALUES ($1, 'memory-index', 384, 1, $2, $3)",
        )
        .bind(id)
        .bind(owner.into_inner())
        .bind(generation)
        .execute(&pool)
        .await?;
    }
    index
        .put(fact(backfilled), owner, 1, 25, true, &[vector(0.95)])
        .await?;
    sqlx::query("SELECT setval('proxiima_core.vector_index_generation_seq', 25, true)")
        .execute(&pool)
        .await?;
    let mut all_req = request(owner);
    all_req.limit = 10;
    let after_backfill = pg
        .search_memories(Some(&scope), &all_req, registry.search_projections())
        .await?;
    assert!(
        after_backfill
            .results
            .iter()
            .any(|hit| hit.memory_id.into_inner() == backfilled)
    );

    // 2. Restore behind retained provider:
    //    - `restored_head` at generation 5 has drifted `is_head = false` with post-backup `head_revision = 100`
    //    - `post_restore` stray entry at generation 50
    //    - `retained` fenced on provider at delete fence 50
    index
        .put(fact(restored_head), owner, 1, 5, true, &[vector(0.9)])
        .await?;
    index
        .set_head(
            &[VectorMetadataTarget {
                key: key(restored_head),
                generation: 5,
            }],
            false,
            100,
        )
        .await?;

    let retained = seed_note(&pg, owner, "wanted").await?;
    let post_restore = Uuid::now_v7();
    index
        .put(fact(post_restore), owner, 1, 50, true, &[vector(1.0)])
        .await?;
    index.delete(&[key(retained)], 50).await?;

    // Advance sequence strictly past the max across generation (50), fence (50), and head_revision (100).
    sqlx::query("SELECT setval('proxiima_core.vector_index_generation_seq', 100, true)")
        .execute(&pool)
        .await?;
    let swept = pg.sweep_vector_index_slice(None, 10).await?;
    assert!(swept.next.is_none());
    assert_eq!(swept.entries_deleted, 1, "post-restore stray removed");
    assert_eq!(swept.metadata_repaired, 1, "drifted head_revision repaired");

    // Reconcile + real claim drain rebuilds `retained` past fence 50.
    let index_space = EmbeddingSpace::new("memory-index", EmbeddingDim::D384);
    let reconciled = pg
        .reconcile_embeddings(EmbeddingReconcileOptions {
            space: &index_space,
            scope: EmbeddingReconcileScope::MissingOnly,
            owners: Some(&[owner]),
            non_embeddable_schemas: &[],
            limit: Some(100),
        })
        .await?;
    assert_eq!(reconciled.enqueued, 1);
    let published = drain_claimed_jobs_real(&pg, owner).await?;
    assert_eq!(published, [retained]);

    let after_restore = pg
        .search_memories(Some(&scope), &all_req, registry.search_projections())
        .await?;
    let found_ids: Vec<Uuid> = after_restore
        .results
        .iter()
        .map(|hit| hit.memory_id.into_inner())
        .collect();
    assert!(
        found_ids.contains(&restored_head),
        "HeadsOnly search must see restored_head after repair above head_revision 100"
    );
    assert!(
        found_ids.contains(&retained),
        "republished entry must bypass retained delete fence 50"
    );
    Ok(())
}

/// Ordinary debt drain honors a live publication's write-ahead lease instead of
/// cancelling an in-flight overwrite or deleting the previously committed v1
/// vector; once `put_batch` finishes, Phase 3 commits v2 cleanly.
#[tokio::test]
async fn ordinary_drain_honors_live_publish_lease_and_preserves_committed_v1() -> TestResult {
    let index = Arc::new(DrainFailureRaceIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("ordinary_drain_honors_publish_lease", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let note = seed_note(&pg, owner, "wanted").await?;
    let v1 = [vector(0.3)];
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &owner,
        entity: fact(note),
        vectors: &v1,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await?;
    let v1_gen = index.inner.entries(None, 10).await?.entries[0].generation;

    // Start a claimless overwrite (v2) paused mid-`put_batch`.
    index.pause_next_put.store(true, Ordering::SeqCst);
    let pub_pg = pg.clone();
    let publisher = tokio::spawn(async move {
        let v2 = [vector(0.99)];
        pub_pg
            .insert_embeddings(&[EmbeddingWrite {
                owner: &owner,
                entity: fact(note),
                vectors: &v2,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            }])
            .await
    });
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.put_entered.notified()).await?;

    // Ordinary drain must skip the live publish intent and leave committed v1 intact.
    assert_eq!(pg.drain_vector_index_debt(100).await?, 0);
    let while_paused = index.inner.entries(None, 10).await?.entries;
    assert_eq!(while_paused.len(), 1);
    assert_eq!(while_paused[0].generation, v1_gen);

    // Resume `put_batch`: Phase 3 commits v2 cleanly without being cancelled.
    index.put_release.notify_one();
    let outcomes = tokio::time::timeout(CONCURRENCY_PATIENCE, publisher).await???;
    assert_eq!(outcomes[0].embedding_version, 2);
    let after = index.inner.entries(None, 10).await?.entries;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].version, 2);
    assert!(after[0].generation > v1_gen);
    Ok(())
}

/// Phase 1 stamps `enqueued_at = clock_timestamp()` after lock waits and
/// Phase 2 periodically renews the lease while `put_batch` runs, so neither a
/// Phase-1 lock wait exceeding the lease nor a long healthy `put_batch` lets an
/// ordinary drain cancel the publication or delete its committed v1 vector.
#[tokio::test]
async fn publish_lease_starts_at_dispatch_and_renews_across_long_put_batch() -> TestResult {
    let index = Arc::new(DrainFailureRaceIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("publish_lease_clock_and_renewal", index.clone()).await;
    let pg = pg.with_vector_debt_lease_for_tests(
        std::time::Duration::from_millis(250),
        std::time::Duration::from_millis(60),
    );
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let note = seed_note(&pg, owner, "wanted").await?;
    let v1 = [vector(0.3)];
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &owner,
        entity: fact(note),
        vectors: &v1,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await?;
    let v1_gen = index.inner.entries(None, 10).await?.entries[0].generation;

    // Hold an exclusive owner lock so Phase 1 starts its transaction and blocks
    // for > 250 ms (strictly longer than the lease) before staging intent.
    let mut lock_tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('proxiima-owner:' || $1::text, 0))")
        .bind(owner.into_inner())
        .execute(&mut *lock_tx)
        .await?;

    index.pause_next_put.store(true, Ordering::SeqCst);
    let pub_pg = pg.clone();
    let publisher = tokio::spawn(async move {
        let v2 = [vector(0.98)];
        pub_pg
            .insert_embeddings(&[EmbeddingWrite {
                owner: &owner,
                entity: fact(note),
                vectors: &v2,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            }])
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    lock_tx.rollback().await?;

    tokio::time::timeout(CONCURRENCY_PATIENCE, index.put_entered.notified()).await?;
    // Hold `put_batch` past the 250 ms lease while 60 ms renewals keep it alive.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(
        pg.drain_vector_index_debt(100).await?,
        0,
        "renewed publish lease must prevent ordinary drain from cancelling a live put"
    );
    let while_paused = index.inner.entries(None, 10).await?.entries;
    assert_eq!(while_paused.len(), 1);
    assert_eq!(while_paused[0].generation, v1_gen);

    index.put_release.notify_one();
    let outcomes = tokio::time::timeout(CONCURRENCY_PATIENCE, publisher).await???;
    assert_eq!(outcomes[0].embedding_version, 2);
    Ok(())
}

#[derive(Default)]
struct FailFirstReportUnsentIndex {
    inner: InMemoryVectorIndex,
    fail_next: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl VectorIndex for FailFirstReportUnsentIndex {
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            if let Some(first) = entries.first() {
                self.inner.put_batch(std::slice::from_ref(first)).await?;
            }
            self.entered.notify_one();
            self.release.notified().await;
            return Err(VectorPutBatchError::with_unsent(
                StorageError::Unavailable("chunk 1 failed; trailing entries never sent".into()),
                (1..entries.len()).collect(),
            ));
        }
        self.inner.put_batch(entries).await
    }
    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        self.inner.delete(keys, fence).await
    }
    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }
    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_head(targets, is_head, revision).await
    }
    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.inner.nearest(filter, query, window).await
    }
    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        self.inner.entries(after, limit).await
    }
}

/// When a multi-chunk `put_batch` fails on chunk 1 and names trailing unsent
/// indices in `VectorPutBatchError`, cleanup keeps fenced delete for the
/// uncertain key, removes newly staged debt for an untouched unsent key,
/// restores prior `'head'` debt for an unsent key that had it, and downgrades
/// an unsent key superseded mid-`put_batch` to `'head'` — preserving committed
/// v1 vectors for every unsent key.
#[tokio::test]
async fn failed_batch_preserves_unsent_committed_vectors_prior_debt_and_concurrent_supersession()
-> TestResult {
    let index = Arc::new(FailFirstReportUnsentIndex::default());
    let (pg, _database) = fresh_pg_host_index("failed_batch_unsent_keys", index.clone()).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));

    let mut notes = Vec::new();
    for label in ["n1", "n2", "n3", "n4"] {
        let created = TestNote::new("wanted", label).create(&pg, owner).await?;
        let id = created.memory_id.into_inner();
        embed(&pg, owner, id, 1).await?;
        notes.push((id, created.handle));
    }
    notes.sort_by_key(|(id, _)| *id);
    let [(k1, _), (k2, _), (k3, h3), (k4, h4)] = [notes[0], notes[1], notes[2], notes[3]];

    // Give `k3` pre-existing `'head'` debt before the failing overwrite starts.
    TestNote::new("wanted", "k3-v2")
        .handle(h3)
        .create(&pg, owner)
        .await?;

    index.fail_next.store(true, Ordering::SeqCst);
    let pub_pg = pg.clone();
    let publisher = tokio::spawn(async move {
        let v2 = [vector(0.95)];
        pub_pg
            .insert_embeddings(&[
                EmbeddingWrite {
                    owner: &owner,
                    entity: fact(k1),
                    vectors: &v2,
                    proof: EmbeddingWriteProof::for_direct_write_for_tests(),
                },
                EmbeddingWrite {
                    owner: &owner,
                    entity: fact(k2),
                    vectors: &v2,
                    proof: EmbeddingWriteProof::for_direct_write_for_tests(),
                },
                EmbeddingWrite {
                    owner: &owner,
                    entity: fact(k3),
                    vectors: &v2,
                    proof: EmbeddingWriteProof::for_direct_write_for_tests(),
                },
                EmbeddingWrite {
                    owner: &owner,
                    entity: fact(k4),
                    vectors: &v2,
                    proof: EmbeddingWriteProof::for_direct_write_for_tests(),
                },
            ])
            .await
    });
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.entered.notified()).await?;

    // Supersede `k4` while `put_batch` is in flight so `memory_head` trigger
    // advances `k4`'s debt `xact` while keeping its positive generation.
    TestNote::new("wanted", "k4-v2")
        .handle(h4)
        .create(&pg, owner)
        .await?;
    index.release.notify_one();

    let res = tokio::time::timeout(CONCURRENCY_PATIENCE, publisher).await??;
    assert!(matches!(res, Err(StorageError::Unavailable(_))));

    // Drain pays: `k1` delete (uncertain), `k3` restored `'head'`, `k4` `'head'`.
    // `k2` had no debt left at all!
    assert_eq!(pg.drain_vector_index_debt(100).await?, 3);
    let entries = index.inner.entries(None, 20).await?.entries;
    let by_id: std::collections::HashMap<Uuid, (i32, bool)> = entries
        .into_iter()
        .map(|e| (e.key.entity_id, (e.version, e.is_head)))
        .collect();
    assert!(!by_id.contains_key(&k1), "uncertain k1 must be deleted");
    assert_eq!(
        by_id.get(&k2),
        Some(&(1, true)),
        "untouched k2 v1 stays head"
    );
    assert_eq!(
        by_id.get(&k3),
        Some(&(1, false)),
        "k3 v1 kept with restored head sync"
    );
    assert_eq!(
        by_id.get(&k4),
        Some(&(1, false)),
        "k4 v1 kept with concurrent head sync"
    );
    Ok(())
}

/// If an overlapping head-change transaction holds the row lock on an unsent
/// key's `vector_index_debt` row while `put_batch` fails and reports that key
/// as unsent, `release_failed_publish_intents` settles the contended unsent
/// key in a deadlock-free single-row transaction once the trigger commits,
/// downgrading it to `'head'` instead of leaving a `'delete'` intent behind.
#[tokio::test]
async fn failed_batch_unsent_key_survives_overlapping_supersession_row_lock() -> TestResult {
    let index = Arc::new(FailFirstReportUnsentIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("failed_batch_overlapping_head_lock", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));

    let mut notes = Vec::new();
    for label in ["n1", "n2"] {
        let base = TestNote::new("wanted", &format!("{label}-v0"))
            .create(&pg, owner)
            .await?;
        let created = TestNote::new("wanted", label)
            .handle(base.handle)
            .create(&pg, owner)
            .await?;
        let id = created.memory_id.into_inner();
        embed(&pg, owner, id, 1).await?;
        notes.push((id, created.handle, base.memory_id.into_inner()));
    }
    notes.sort_by_key(|(id, _, _)| *id);
    let [(k1, _, _), (k2, h2, k2_alt)] = [notes[0], notes[1]];

    index.fail_next.store(true, Ordering::SeqCst);
    let pub_pg = pg.clone();
    let publisher = tokio::spawn(async move {
        let v2 = [vector(0.95)];
        pub_pg
            .insert_embeddings(&[
                EmbeddingWrite {
                    owner: &owner,
                    entity: fact(k1),
                    vectors: &v2,
                    proof: EmbeddingWriteProof::for_direct_write_for_tests(),
                },
                EmbeddingWrite {
                    owner: &owner,
                    entity: fact(k2),
                    vectors: &v2,
                    proof: EmbeddingWriteProof::for_direct_write_for_tests(),
                },
            ])
            .await
    });
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.entered.notified()).await?;

    // Open an overlapping transaction that fires `memory_head_changed_index_debt`
    // and holds the row lock on `k2`'s `vector_index_debt` row while cleanup starts.
    let mut head_tx = pool.begin().await?;
    sqlx::query("UPDATE proxiima_core.memory_head SET t = $2 WHERE handle = $1")
        .bind(h2)
        .bind(k2_alt)
        .execute(&mut *head_tx)
        .await?;

    index.release.notify_one();
    // Let cleanup complete its fast `SKIP LOCKED` pass (settling `k1`) and wait
    // on `k2`'s single-row lock.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    head_tx.commit().await?;

    let res = tokio::time::timeout(CONCURRENCY_PATIENCE, publisher).await??;
    assert!(matches!(res, Err(StorageError::Unavailable(_))));

    assert_eq!(pg.drain_vector_index_debt(100).await?, 2);
    let entries = index.inner.entries(None, 20).await?.entries;
    assert_eq!(
        entries.len(),
        1,
        "uncertain k1 deleted; unsent k2 preserved"
    );
    assert_eq!(entries[0].key.entity_id, k2);
    assert_eq!(entries[0].version, 1);
    assert!(
        !entries[0].is_head,
        "overlapping head change synced on drain"
    );
    Ok(())
}

/// A long-open head-change transaction whose `now()` predates the publish lease
/// must preserve `vector_index_debt.enqueued_at` when keeping an active
/// positive publish generation, so ordinary drain cannot immediately expire
/// and cancel the healthy in-flight publication.
#[tokio::test]
async fn stale_supersession_transaction_preserves_live_publish_intent_lease() -> TestResult {
    let index = Arc::new(DrainFailureRaceIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("stale_supersession_preserves_lease", index.clone()).await;
    let pg = pg.with_vector_debt_lease_for_tests(
        std::time::Duration::from_millis(250),
        std::time::Duration::from_secs(60),
    );
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let base = TestNote::new("wanted", "v0").create(&pg, owner).await?;
    let created = TestNote::new("wanted", "v1")
        .handle(base.handle)
        .create(&pg, owner)
        .await?;
    let note = created.memory_id.into_inner();
    embed(&pg, owner, note, 1).await?;
    let v1_gen = index.inner.entries(None, 10).await?.entries[0].generation;

    // Pin `stale_tx`'s transaction-start `now()` strictly older than the 250 ms lease.
    let mut stale_tx = pool.begin().await?;
    let _: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&mut *stale_tx)
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(320)).await;

    index.pause_next_put.store(true, Ordering::SeqCst);
    let pub_pg = pg.clone();
    let publisher = tokio::spawn(async move {
        let v2 = [vector(0.97)];
        pub_pg
            .insert_embeddings(&[EmbeddingWrite {
                owner: &owner,
                entity: fact(note),
                vectors: &v2,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            }])
            .await
    });
    tokio::time::timeout(CONCURRENCY_PATIENCE, index.put_entered.notified()).await?;

    // Commit the old head-change transaction while `put_batch` is live.
    sqlx::query("UPDATE proxiima_core.memory_head SET t = $2 WHERE handle = $1")
        .bind(created.handle)
        .bind(base.memory_id.into_inner())
        .execute(&mut *stale_tx)
        .await?;
    stale_tx.commit().await?;

    assert_eq!(
        pg.drain_vector_index_debt(100).await?,
        0,
        "head trigger preserving positive generation must preserve enqueued_at"
    );
    let while_paused = index.inner.entries(None, 10).await?.entries;
    assert_eq!(while_paused.len(), 1);
    assert_eq!(while_paused[0].generation, v1_gen);

    index.put_release.notify_one();
    let outcomes = tokio::time::timeout(CONCURRENCY_PATIENCE, publisher).await???;
    assert_eq!(outcomes[0].embedding_version, 2);
    assert_eq!(pg.drain_vector_index_debt(100).await?, 1);
    let after = index.inner.entries(None, 10).await?.entries;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].version, 2);
    assert!(!after[0].is_head);
    Ok(())
}

struct AllUnsentFailIndex {
    inner: InMemoryVectorIndex,
    fail_all_unsent: std::sync::atomic::AtomicBool,
}

impl Default for AllUnsentFailIndex {
    fn default() -> Self {
        Self {
            inner: InMemoryVectorIndex::default(),
            fail_all_unsent: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl VectorIndex for AllUnsentFailIndex {
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        if self.fail_all_unsent.load(Ordering::SeqCst) {
            return Err(VectorPutBatchError::with_unsent(
                StorageError::Unavailable("pre-dispatch refusal".into()),
                (0..entries.len()).collect(),
            ));
        }
        self.inner.put_batch(entries).await
    }
    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        self.inner.delete(keys, fence).await
    }
    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }
    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_head(targets, is_head, revision).await
    }
    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.inner.nearest(filter, query, window).await
    }
    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        self.inner.entries(after, limit).await
    }
}

/// Locking `vector_index_debt` (`FOR NO KEY UPDATE OF d`) and consuming
/// `vector_index_unsent_intents` in Phase 1 ensures a retry B cannot capture a
/// stale delete snapshot of an earlier unsent attempt A (whether A just
/// deleted its intent or left a durable unsent record) and turn it into
/// payable delete debt when B also fails unsent.
#[tokio::test]
async fn retry_after_unsent_publish_does_not_recreate_delete_debt() -> TestResult {
    let index = Arc::new(AllUnsentFailIndex::default());
    let (pg, _database) = fresh_pg_host_index("retry_unsent_no_delete_debt", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let note = seed_note(&pg, owner, "wanted").await?;
    embed(&pg, owner, note, 1).await?;

    // Seed an unsettled unsent publish intent from attempt A (simulating a
    // timed-out cleanup row still protected in `vector_index_unsent_intents`).
    let tx1_xact: String = sqlx::query_scalar(
        "INSERT INTO proxiima_core.vector_index_debt
             (entity_id, model_id, dim, owner_id, op, generation, enqueued_at, xact)
         VALUES ($1, 'memory-index', 384, $2, 'delete', 99, clock_timestamp(), pg_current_xact_id())
         RETURNING xact::text",
    )
    .bind(note)
    .bind(owner.into_inner())
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO proxiima_core.vector_index_unsent_intents
             (entity_id, model_id, dim, generation, tx1_xact)
         VALUES ($1, 'memory-index', 384, 99, $2::xid8)",
    )
    .bind(note)
    .bind(&tx1_xact)
    .execute(&pool)
    .await?;

    // Attempt B retries and also fails before dispatch (`all unsent`):
    // Phase 1 must resolve A's durable unsent record as `prior_debt = None`,
    // so B's cleanup deletes the debt row rather than turning A into payable delete debt.
    index.fail_all_unsent.store(true, Ordering::SeqCst);
    let v2 = [vector(0.95)];
    let err = pg
        .insert_embeddings(&[EmbeddingWrite {
            owner: &owner,
            entity: fact(note),
            vectors: &v2,
            proof: EmbeddingWriteProof::for_direct_write_for_tests(),
        }])
        .await;
    assert!(matches!(err, Err(StorageError::Unavailable(_))));

    assert_eq!(debt_rows(&pool).await?, 0);
    let unsent_left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM proxiima_core.vector_index_unsent_intents")
            .fetch_one(&pool)
            .await?;
    assert_eq!(unsent_left, 0);
    assert_eq!(pg.drain_vector_index_debt(100).await?, 0);
    let entries = index.inner.entries(None, 10).await?.entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key.entity_id, note);
    assert_eq!(entries[0].version, 1);
    Ok(())
}

/// Test double that lets callers queue deterministic paused `put_batch` calls
/// that return `all_unsent` once released, exposing real concurrent takeover,
/// transfer, and hard-erase interleavings without synthetic DB seeding.
#[derive(Debug, Default)]
struct GatedUnsentIndex {
    inner: InMemoryVectorIndex,
    gates: tokio::sync::Mutex<
        std::collections::VecDeque<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
}

impl GatedUnsentIndex {
    async fn queue_paused_all_unsent(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        self.gates.lock().await.push_back((entered_tx, release_rx));
        (entered_rx, release_tx)
    }
}

#[async_trait::async_trait]
impl VectorIndex for GatedUnsentIndex {
    async fn put_batch(
        &self,
        items: &[proxiima_core::storage_ports::VectorPut<'_>],
    ) -> Result<(), proxiima_core::storage_ports::VectorPutBatchError> {
        let gate = self.gates.lock().await.pop_front();
        if let Some((entered_tx, release_rx)) = gate {
            let _ = entered_tx.send(());
            let _ = release_rx.await;
            return Err(
                proxiima_core::storage_ports::VectorPutBatchError::with_unsent(
                    StorageError::Unavailable("paused all unsent".into()),
                    (0..items.len()).collect(),
                ),
            );
        }
        self.inner.put_batch(items).await
    }
    async fn delete(&self, keys: &[VectorKey], fence_generation: i64) -> Result<(), StorageError> {
        self.inner.delete(keys, fence_generation).await
    }
    async fn set_owner(
        &self,
        targets: &[proxiima_core::storage_ports::VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_owner(targets, owner, revision).await
    }
    async fn set_head(
        &self,
        targets: &[proxiima_core::storage_ports::VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.inner.set_head(targets, is_head, revision).await
    }
    async fn nearest(
        &self,
        filter: &proxiima_core::storage_ports::VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.inner.nearest(filter, query, window).await
    }
    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        self.inner.entries(after, limit).await
    }
}

fn spawn_overwrite_attempt(
    pg: PgStorage,
    owner: OwnerId,
    note: Uuid,
) -> tokio::task::JoinHandle<
    Result<Vec<proxiima_core::storage_ports::EmbeddingWriteOutcome>, StorageError>,
> {
    tokio::spawn(async move {
        let v2 = [vector(0.95)];
        pg.insert_embeddings(&[EmbeddingWrite {
            owner: &owner,
            entity: fact(note),
            vectors: &v2,
            proof: EmbeddingWriteProof::for_direct_write_for_tests(),
        }])
        .await
    })
}

/// Real concurrent takeover without synthetic DB seeding: attempt B stages over
/// attempt A's live publish intent while A is still paused in `put_batch`, and
/// both fail `all_unsent` in either completion order (A-then-B or B-then-A).
/// Even if A's pre-takeover timestamp is older than one lease when B restores
/// A, A receives a fresh renewal window so an intervening `drain_vector_index_debt`
/// before A finishes cannot prematurely delete the committed vector.
#[tokio::test]
async fn overlapping_unsent_retries_preserve_committed_vector_in_both_orders() -> TestResult {
    let index = Arc::new(GatedUnsentIndex::default());
    let (pg, _database) =
        fresh_pg_host_index("overlapping_unsent_both_orders", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let note = seed_note(&pg, owner, "wanted").await?;
    embed(&pg, owner, note, 1).await?;

    for release_a_first in [true, false] {
        let (a_entered, a_release) = index.queue_paused_all_unsent().await;
        let (b_entered, b_release) = index.queue_paused_all_unsent().await;
        let task_a = spawn_overwrite_attempt(pg.clone(), owner, note);
        tokio::time::timeout(CONCURRENCY_PATIENCE, a_entered).await??;

        // Age A's pre-takeover timestamp past the 60s lease before B stages
        // over it, modeling B holding the debt row longer than one lease while
        // A's heartbeats could not match B's generation.
        sqlx::query(
            "UPDATE proxiima_core.vector_index_debt
                SET enqueued_at = clock_timestamp() - interval '120 seconds'
              WHERE entity_id = $1",
        )
        .bind(note)
        .execute(&pool)
        .await?;

        let task_b = spawn_overwrite_attempt(pg.clone(), owner, note);
        tokio::time::timeout(CONCURRENCY_PATIENCE, b_entered).await??;

        if release_a_first {
            let _ = a_release.send(());
            assert!(matches!(
                tokio::time::timeout(CONCURRENCY_PATIENCE, task_a).await??,
                Err(StorageError::Unavailable(_))
            ));
            let _ = b_release.send(());
            assert!(matches!(
                tokio::time::timeout(CONCURRENCY_PATIENCE, task_b).await??,
                Err(StorageError::Unavailable(_))
            ));
        } else {
            let _ = b_release.send(());
            assert!(matches!(
                tokio::time::timeout(CONCURRENCY_PATIENCE, task_b).await??,
                Err(StorageError::Unavailable(_))
            ));
            // Drain right after B restores A and BEFORE A finishes: A's fresh
            // renewal window must prevent drain from claiming and deleting v1.
            assert_eq!(
                pg.drain_vector_index_debt(100).await?,
                0,
                "restored live predecessor A must not inherit an expired pre-takeover lease"
            );
            assert_eq!(
                index.inner.entries(None, 10).await?.entries.len(),
                1,
                "committed v1 must remain intact while restored A is still in flight"
            );
            let _ = a_release.send(());
            assert!(matches!(
                tokio::time::timeout(CONCURRENCY_PATIENCE, task_a).await??,
                Err(StorageError::Unavailable(_))
            ));
        }

        // Drain any deferred reconciliation; committed v1 must remain intact.
        while pg.drain_vector_index_debt(100).await? > 0 {}
        assert_eq!(debt_rows(&pool).await?, 0);
        let unsent_left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM proxiima_core.vector_index_unsent_intents")
                .fetch_one(&pool)
                .await?;
        assert_eq!(unsent_left, 0);
        let entries = index.inner.entries(None, 10).await?.entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key.entity_id, note);
        assert_eq!(entries[0].version, 1);
    }
    Ok(())
}

/// If ownership transfers while an overwrite `put_batch` is in flight and the
/// provider returns `all_unsent`, cleanup must downgrade the debt to `'owner'`
/// metadata sync under the destination owner so draining updates the vector's
/// owner instead of deleting the destination's valid committed vector.
#[tokio::test]
async fn transfer_during_unsent_put_batch_downgrades_to_owner_sync_and_preserves_vector()
-> TestResult {
    use proxiima_core::owner_inverse::OwnerSurfaces;
    use proxiima_core::storage_ports::{OwnerTransferPort, OwnerWritePermit};

    let index = Arc::new(GatedUnsentIndex::default());
    let (pg, _database) = fresh_pg_host_index("transfer_during_unsent_put", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let owner_a = OwnerId::from(UserId::new(Uuid::now_v7()));
    let owner_b = OwnerId::from(UserId::new(Uuid::now_v7()));
    create_user(&pool, owner_b.as_user()).await?;

    let note = seed_note(&pg, owner_a, "wanted").await?;
    embed(&pg, owner_a, note, 1).await?;

    let (entered, release) = index.queue_paused_all_unsent().await;
    let writer = spawn_overwrite_attempt(pg.clone(), owner_a, note);
    tokio::time::timeout(CONCURRENCY_PATIENCE, entered).await??;

    let space = EmbeddingSpace::new("memory-index", EmbeddingDim::D384);
    let permit =
        OwnerWritePermit::transfer_for_tests(owner_a, proxiima_core::MemoryKind::Fact, owner_b);
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );
    assert!(
        pg.transfer_to_owner(
            &permit,
            proxiima_core::EntityId::Memory(MemoryId::new(note)),
            owner_b,
            &surfaces,
            &[space],
        )
        .await?
    );

    // Ordinary drain during the live transferred publish lease must not delete v1.
    assert_eq!(pg.drain_vector_index_debt(100).await?, 0);

    let _ = release.send(());
    assert!(matches!(
        tokio::time::timeout(CONCURRENCY_PATIENCE, writer).await??,
        Err(StorageError::Unavailable(_))
    ));

    let op: String =
        sqlx::query_scalar("SELECT op FROM proxiima_core.vector_index_debt WHERE entity_id = $1")
            .bind(note)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        op, "owner",
        "confirmed-unsent attempt across transfer must become owner sync"
    );

    assert_eq!(pg.drain_vector_index_debt(100).await?, 1);
    let entries = index.inner.entries(None, 10).await?.entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key.entity_id, note);
    assert_eq!(entries[0].owner, owner_b);
    assert_eq!(entries[0].version, 1);

    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope_b = owner_scope_for_tests(&[owner_b]);
    let page = pg
        .search_memories(
            Some(&scope_b),
            &request(owner_b),
            registry.search_projections(),
        )
        .await?;
    assert_eq!(page.results.len(), 1);
    assert_eq!(page.results[0].memory_id, MemoryId::new(note));
    Ok(())
}

/// Hard owner erase must clear any pending `vector_index_unsent_intents`
/// markers in its scope before committing, and a late `all_unsent` cleanup
/// finishing after erase commits must not reinsert erased entity identifiers.
#[tokio::test]
async fn hard_erase_clears_unsent_intents_and_blocks_late_reinsertion() -> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::OwnerInversePort;

    let index = Arc::new(GatedUnsentIndex::default());
    let (pg, _database) = fresh_pg_host_index("hard_erase_clears_unsent", index.clone()).await;
    let pool = pg.pool_for_tests().clone();
    let user = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(user);
    let note = seed_note(&pg, owner, "wanted").await?;
    embed(&pg, owner, note, 1).await?;

    let (entered, release) = index.queue_paused_all_unsent().await;
    let writer = spawn_overwrite_attempt(pg.clone(), owner, note);
    tokio::time::timeout(CONCURRENCY_PATIENCE, entered).await??;

    // Seed a durable unsent marker alongside the live publish intent to simulate
    // a previously contended unsent record still present when hard erase runs.
    let (staged_gen, tx1_xact): (i64, String) = sqlx::query_as(
        "SELECT generation, xact::text FROM proxiima_core.vector_index_debt WHERE entity_id = $1",
    )
    .bind(note)
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO proxiima_core.vector_index_unsent_intents
             (entity_id, model_id, dim, generation, tx1_xact)
         VALUES ($1, 'memory-index', 384, $2, $3::xid8)",
    )
    .bind(note)
    .bind(staged_gen)
    .bind(&tx1_xact)
    .execute(&pool)
    .await?;

    let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
        user_id: user,
        drop_event_id: "hard-erase-unsent".into(),
    });
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );
    let outcome = pg.erase_personal_owner(&auth, user, &surfaces).await?;
    assert!(matches!(outcome, OwnerEraseOutcome::Completed { .. }));

    let unsent_after_erase: i64 =
        sqlx::query_scalar("SELECT count(*) FROM proxiima_core.vector_index_unsent_intents")
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        unsent_after_erase, 0,
        "hard erase must leave zero unsent markers for erased entities"
    );

    // Late all_unsent cleanup after hard erase must not reinsert erased entity IDs.
    let _ = release.send(());
    assert!(matches!(
        tokio::time::timeout(CONCURRENCY_PATIENCE, writer).await??,
        Err(StorageError::Unavailable(_))
    ));
    let (debt_after_late, unsent_after_late): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM proxiima_core.vector_index_debt),
                (SELECT count(*) FROM proxiima_core.vector_index_unsent_intents)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!((debt_after_late, unsent_after_late), (0, 0));
    assert!(index.inner.entries(None, 10).await?.entries.is_empty());
    Ok(())
}

/// Whole-owner erase must clear `vector_index_unsent_intents` rows for every
/// selected Memory and Goal under the exclusive erase barrier, even when that
/// entity currently has zero `embedding_heads` rows and zero
/// `vector_index_debt` rows (e.g., after an older cleanup recorded a marker
/// across a newer publisher's delete and a subsequent space purge + drain).
#[tokio::test]
async fn whole_owner_erase_clears_unsent_markers_for_entities_without_debt() -> TestResult {
    use proxiima_core::owner_inverse::{
        EraseAuthorization, OwnerEraseOutcome, OwnerEraseTarget, OwnerSurfaces,
    };
    use proxiima_core::storage_ports::OwnerInversePort;

    let index = Arc::new(InMemoryVectorIndex::default());
    let (pg, _database) = fresh_pg_host_index("whole_owner_erase_orphan_unsent", index).await;
    let pool = pg.pool_for_tests().clone();
    let user = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(user);
    let note = seed_note(&pg, owner, "no heads and no debt").await?;

    // Confirm zero heads and zero debt rows, then seed a leftover unsent marker
    // naming the owner's selected memory.
    let (heads_before, debt_before): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM proxiima_core.embedding_heads),
                (SELECT count(*) FROM proxiima_core.vector_index_debt)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!((heads_before, debt_before), (0, 0));
    sqlx::query(
        "INSERT INTO proxiima_core.vector_index_unsent_intents
             (entity_id, model_id, dim, generation, tx1_xact)
         VALUES ($1, 'memory-index', 384, 7, pg_current_xact_id())",
    )
    .bind(note)
    .execute(&pool)
    .await?;

    let auth = EraseAuthorization::new_for_tests(OwnerEraseTarget::PersonalOwner {
        user_id: user,
        drop_event_id: "hard-erase-orphan-unsent".into(),
    });
    let surfaces = OwnerSurfaces::for_registry(
        &proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests(),
    );
    let outcome = pg.erase_personal_owner(&auth, user, &surfaces).await?;
    assert!(matches!(outcome, OwnerEraseOutcome::Completed { .. }));

    let unsent_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM proxiima_core.vector_index_unsent_intents")
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        unsent_after, 0,
        "whole-owner erase must clear unsent markers for selected entities even without debt"
    );
    Ok(())
}
