//! The network form of a store's vector index: a client and the router
//! between them behave as the index behind the router, each side refuses
//! what the other must not send, and the router serves only its token's
//! holder.

use std::collections::BTreeMap;
use std::sync::Arc;

use proxiima::vector_index_http::{
    VECTOR_INDEX_BODY_LIMIT, VECTOR_INDEX_TOKEN_MIN_LEN, VectorIndexClient, VectorIndexToken,
    vector_index_router,
};
use proxiima_core::storage_ports::{
    InMemoryVectorIndex, VectorCandidate, VectorCandidateFilter, VectorEntry, VectorEntryPage,
    VectorIndex, VectorKey, VectorMetadataTarget, VectorPut,
};
use proxiima_core::verbs::query::SupersessionStatus;
use proxiima_core::{
    EmbeddableEntityRef, EmbeddingDim, EmbeddingSpace, GoalId, MemoryId, MemoryKind, OwnerId,
    PlaintextHosts, SpaceVector, StorageError, UserId,
};
use uuid::Uuid;

/// The token the routers here are built with.
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// A token of the right shape that is not [`TOKEN`].
const OTHER: &str = "fedcba9876543210fedcba9876543210";

fn token() -> VectorIndexToken {
    VectorIndexToken::new(TOKEN.into()).unwrap()
}

/// A router on a loopback port, stopped when dropped.
struct Served {
    base: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(indexes: BTreeMap<u16, Arc<dyn VectorIndex>>) -> Served {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = vector_index_router(indexes, &token());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Served { base, task }
}

fn client(served: &Served, store: u16) -> VectorIndexClient {
    VectorIndexClient::new(&served.base, store, &PlaintextHosts::none(), token()).unwrap()
}

fn space() -> EmbeddingSpace {
    EmbeddingSpace::new("wire", EmbeddingDim::D384)
}

fn vector(head: [f32; 2]) -> SpaceVector {
    let mut values = vec![0.0; 384];
    values[..2].copy_from_slice(&head);
    SpaceVector::new(space(), values).unwrap()
}

fn memory(id: u128) -> EmbeddableEntityRef {
    EmbeddableEntityRef::Memory {
        kind: MemoryKind::Perspective,
        memory_id: MemoryId::new(Uuid::from_u128(id)),
    }
}

fn key(id: u128) -> VectorKey {
    VectorKey {
        entity_id: Uuid::from_u128(id),
        space: space(),
    }
}

fn owner(id: u128) -> OwnerId {
    OwnerId::from(UserId::new(Uuid::from_u128(id)))
}

fn owner_filter(who: OwnerId) -> VectorCandidateFilter {
    VectorCandidateFilter {
        owners: vec![who],
        memories: vec![],
        supersession: SupersessionStatus::HeadsOnly,
    }
}

fn meta_target(id: u128, generation: i64) -> VectorMetadataTarget {
    VectorMetadataTarget {
        key: key(id),
        generation,
    }
}

/// Every call through the wire lands on the store's own index and answers
/// what that index answers; another store's index stays untouched.
#[tokio::test]
async fn the_wire_carries_every_call_to_its_stores_index() {
    let (mine, other) = (
        Arc::new(InMemoryVectorIndex::default()),
        Arc::new(InMemoryVectorIndex::default()),
    );
    let served = serve(BTreeMap::from([
        (3, mine.clone() as Arc<dyn VectorIndex>),
        (4, other.clone() as Arc<dyn VectorIndex>),
    ]))
    .await;
    let remote = client(&served, 3);
    let (me, you) = (owner(10), owner(11));
    remote
        .put(
            memory(1),
            me,
            2,
            1,
            true,
            &[vector([0.0, 1.0]), vector([1.0, 0.0])],
        )
        .await
        .unwrap();
    remote
        .put(memory(2), me, 1, 2, true, &[vector([1.0, 1.0])])
        .await
        .unwrap();
    remote
        .put(
            EmbeddableEntityRef::Goal(GoalId::new(Uuid::from_u128(3))),
            me,
            1,
            3,
            true,
            &[vector([1.0, 0.0])],
        )
        .await
        .unwrap();

    let query = vector([1.0, 0.0]);
    let filter = owner_filter(me);
    let wire = remote.nearest(&filter, &query, 10).await.unwrap();
    assert_eq!(
        wire.bound,
        proxiima_core::storage_ports::CandidateBound::Unknown
    );
    assert_eq!(
        wire.candidates,
        mine.nearest(&filter, &query, 10).await.unwrap().candidates
    );
    assert_eq!(wire.candidates.len(), 2, "a goal is no candidate");

    remote
        .put(memory(4), you, 1, 4, true, &[vector([1.0, 0.0])])
        .await
        .unwrap();
    remote
        .put(memory(5), you, 1, 5, true, &[vector([0.0, 1.0])])
        .await
        .unwrap();
    let exact = VectorCandidateFilter {
        owners: vec![],
        memories: vec![MemoryId::new(Uuid::from_u128(5))],
        supersession: SupersessionStatus::IncludeSuperseded,
    };
    let exact_ranked = remote.nearest(&exact, &query, 1).await.unwrap().candidates;
    assert_eq!(exact_ranked.len(), 1);
    assert_eq!(exact_ranked[0].memory_id, MemoryId::new(Uuid::from_u128(5)));
    assert_eq!(exact_ranked[0].generation, 5);
    remote.delete(&[key(4), key(5)], 6).await.unwrap();
    assert_eq!(
        remote.entries(None, 10).await.unwrap(),
        mine.entries(None, 10).await.unwrap()
    );

    remote
        .set_owner(&[meta_target(2, 2)], you, 7)
        .await
        .unwrap();
    remote.delete(&[key(1), key(99)], 8).await.unwrap();
    assert!(
        remote
            .nearest(&filter, &query, 10)
            .await
            .unwrap()
            .candidates
            .is_empty()
    );
    let moved = remote
        .nearest(&owner_filter(you), &query, 10)
        .await
        .unwrap()
        .candidates;
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].memory_id, MemoryId::new(Uuid::from_u128(2)));
    assert_eq!(moved[0].generation, 2);
    let left: Vec<VectorEntry> = mine.entries(None, 10).await.unwrap().entries;
    assert_eq!(left.len(), 2);
    assert_eq!(left[0].version, 1);
    assert!(left[0].is_head);
    assert!(other.entries(None, 10).await.unwrap().entries.is_empty());
}

/// Paginated `entries` calls step across the HTTP wire with `has_more`.
#[tokio::test]
async fn entries_pages_across_the_http_wire() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let served = serve(BTreeMap::from([(0, index as Arc<dyn VectorIndex>)])).await;
    let remote = client(&served, 0);
    let me = owner(10);
    for (id, generation) in [(1_u128, 1_i64), (2, 2), (3, 3)] {
        remote
            .put(memory(id), me, 1, generation, true, &[vector([1.0, 0.0])])
            .await
            .unwrap();
    }
    let page = remote.entries(None, 1).await.unwrap();
    assert!(page.has_more);
    assert_eq!(page.entries.len(), 1);
    let rest = remote
        .entries(Some(&page.entries[0].key), 10)
        .await
        .unwrap();
    assert!(!rest.has_more);
    assert_eq!(page.entries.len() + rest.entries.len(), 3);
}

/// A delayed `put` with `generation <= fence` is ignored across the HTTP wire
/// after `delete(keys, fence)` advances the key's provider fence.
#[tokio::test]
async fn the_wire_fences_delayed_puts_after_delete() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let served = serve(BTreeMap::from([(0, index.clone() as Arc<dyn VectorIndex>)])).await;
    let remote = client(&served, 0);
    let me = owner(10);
    remote
        .put(memory(1), me, 1, 5, true, &[vector([1.0, 0.0])])
        .await
        .unwrap();
    remote.delete(&[key(1)], 6).await.unwrap();
    remote
        .put(memory(1), me, 1, 5, true, &[vector([1.0, 0.0])])
        .await
        .unwrap();
    assert!(index.entries(None, 10).await.unwrap().entries.is_empty());
    remote
        .put(memory(1), me, 1, 7, true, &[vector([1.0, 0.0])])
        .await
        .unwrap();
    let entries = index.entries(None, 10).await.unwrap().entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].generation, 7);
}

/// `entries(_, 0)` round-trips cleanly across HTTP: a nonempty index reports
/// `has_more: true` with an empty slice, and an empty index reports `false`.
#[tokio::test]
async fn entries_limit_zero_reports_has_more_without_returning_entries() {
    let (populated, empty) = (
        Arc::new(InMemoryVectorIndex::default()),
        Arc::new(InMemoryVectorIndex::default()),
    );
    let served = serve(BTreeMap::from([
        (0, populated as Arc<dyn VectorIndex>),
        (1, empty as Arc<dyn VectorIndex>),
    ]))
    .await;
    let populated_client = client(&served, 0);
    populated_client
        .put(memory(1), owner(10), 1, 1, true, &[vector([1.0, 0.0])])
        .await
        .unwrap();
    let nonempty_zero = populated_client.entries(None, 0).await.unwrap();
    assert!(nonempty_zero.entries.is_empty());
    assert!(nonempty_zero.has_more);

    let empty_zero = client(&served, 1).entries(None, 0).await.unwrap();
    assert!(empty_zero.entries.is_empty());
    assert!(!empty_zero.has_more);
}

/// `set_head` toggles series-head visibility under `HeadsOnly` across the wire
/// without hiding the entry from `IncludeSuperseded`.
#[tokio::test]
async fn the_wire_carries_set_head_and_supersession_filters() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let served = serve(BTreeMap::from([(0, index as Arc<dyn VectorIndex>)])).await;
    let remote = client(&served, 0);
    let me = owner(10);
    let v1 = [vector([1.0, 0.0])];
    let v2 = [vector([0.5, 0.5])];
    remote
        .put_batch(&[
            VectorPut {
                entity: memory(1),
                owner: me,
                version: 1,
                generation: 1,
                is_head: true,
                vectors: &v1,
            },
            VectorPut {
                entity: memory(2),
                owner: me,
                version: 1,
                generation: 2,
                is_head: true,
                vectors: &v2,
            },
        ])
        .await
        .unwrap();
    remote
        .set_head(
            &[VectorMetadataTarget {
                key: key(1),
                generation: 1,
            }],
            false,
            3,
        )
        .await
        .unwrap();
    let query = vector([1.0, 0.0]);
    let heads_only = remote
        .nearest(
            &VectorCandidateFilter {
                owners: vec![me],
                memories: vec![],
                supersession: SupersessionStatus::HeadsOnly,
            },
            &query,
            10,
        )
        .await
        .unwrap()
        .candidates;
    assert_eq!(heads_only.len(), 1);
    assert_eq!(heads_only[0].memory_id, MemoryId::new(Uuid::from_u128(2)));
    let all_versions = remote
        .nearest(
            &VectorCandidateFilter {
                owners: vec![me],
                memories: vec![],
                supersession: SupersessionStatus::IncludeSuperseded,
            },
            &query,
            10,
        )
        .await
        .unwrap()
        .candidates;
    assert_eq!(all_versions.len(), 2);
}

/// Conditional metadata updates across HTTP ignore delayed lower revisions and
/// mismatched generations, and reject non-positive revisions/generations at
/// both client and router boundaries.
#[tokio::test]
async fn the_wire_fences_delayed_metadata_updates_and_rejects_non_positive_revisions() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let served = serve(BTreeMap::from([(0, index.clone() as Arc<dyn VectorIndex>)])).await;
    let remote = client(&served, 0);
    let (me, you) = (owner(10), owner(11));
    remote
        .put(memory(1), me, 1, 5, true, &[vector([1.0, 0.0])])
        .await
        .unwrap();
    let target = VectorMetadataTarget {
        key: key(1),
        generation: 5,
    };
    remote
        .set_head(std::slice::from_ref(&target), false, 6)
        .await
        .unwrap();
    remote
        .set_head(std::slice::from_ref(&target), true, 8)
        .await
        .unwrap();
    // Delayed `set_head(false)` with revision 7 must not clobber revision 8.
    remote
        .set_head(std::slice::from_ref(&target), false, 7)
        .await
        .unwrap();
    // Owner update with matching generation and positive revision applies; lower revision is ignored.
    remote
        .set_owner(std::slice::from_ref(&target), you, 8)
        .await
        .unwrap();
    remote
        .set_owner(std::slice::from_ref(&target), me, 6)
        .await
        .unwrap();
    let entries = index.entries(None, 10).await.unwrap().entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].owner, you);
    assert!(entries[0].is_head);

    assert!(matches!(
        remote
            .set_head(std::slice::from_ref(&target), false, 0)
            .await,
        Err(StorageError::ConstraintViolation(_))
    ));
    assert!(matches!(
        remote
            .set_owner(
                &[VectorMetadataTarget {
                    key: key(1),
                    generation: 0,
                }],
                me,
                9,
            )
            .await,
        Err(StorageError::ConstraintViolation(_))
    ));
    let http = reqwest::Client::new();
    let bad_head = http
        .post(format!("{}/v1/stores/0/head", served.base))
        .header("content-type", "application/json")
        .bearer_auth(TOKEN)
        .body("{\"targets\":[{\"entity_id\":\"00000000-0000-0000-0000-000000000001\",\"model_id\":\"m\",\"dim\":384,\"generation\":5}],\"is_head\":false,\"revision\":0}")
        .send()
        .await
        .unwrap();
    assert_eq!(bad_head.status().as_u16(), 400);
    let bad_owner = http
        .post(format!("{}/v1/stores/0/owner", served.base))
        .header("content-type", "application/json")
        .bearer_auth(TOKEN)
        .body("{\"targets\":[{\"entity_id\":\"00000000-0000-0000-0000-000000000001\",\"model_id\":\"m\",\"dim\":384,\"generation\":0}],\"owner\":\"00000000-0000-0000-0000-00000000000a\",\"revision\":9}")
        .send()
        .await
        .unwrap();
    assert_eq!(bad_owner.status().as_u16(), 400);
}

/// Filtering happens before ranking and the window: a nearer vector owned by
/// nobody in the store's scope cannot consume the exact candidate's slot.
#[tokio::test]
async fn the_wire_filters_private_nearer_vectors_before_the_window() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let served = serve(BTreeMap::from([(0, index.clone() as Arc<dyn VectorIndex>)])).await;
    let remote = client(&served, 0);
    let private = owner(10);
    let allowed = owner(11);
    let target = MemoryId::new(Uuid::from_u128(2));
    remote
        .put(memory(1), private, 1, 1, true, &[vector([1.0, 0.0])])
        .await
        .unwrap();
    remote
        .put(memory(2), private, 1, 2, true, &[vector([0.0, 1.0])])
        .await
        .unwrap();

    let candidates = remote
        .nearest(
            &VectorCandidateFilter {
                owners: vec![allowed],
                memories: vec![target],
                supersession: SupersessionStatus::HeadsOnly,
            },
            &vector([1.0, 0.0]),
            1,
        )
        .await
        .unwrap()
        .candidates;
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].memory_id, target);
}

/// The router parses each body where it enters: an unknown store, a body
/// that is not the call's, and a width the store does not index are refused,
/// and the client reads a refusal as the index's, anything else as down.
#[tokio::test]
async fn the_router_refuses_unknown_stores_and_malformed_calls() {
    let served = serve(BTreeMap::from([(
        0,
        Arc::new(InMemoryVectorIndex::default()) as Arc<dyn VectorIndex>,
    )]))
    .await;
    let unknown = client(&served, 9)
        .nearest(
            &VectorCandidateFilter {
                owners: vec![owner(1)],
                memories: vec![],
                supersession: SupersessionStatus::HeadsOnly,
            },
            &vector([1.0, 0.0]),
            5,
        )
        .await;
    assert!(
        matches!(unknown, Err(StorageError::Unavailable(_))),
        "{unknown:?}"
    );

    let http = reqwest::Client::new();
    let post = |path: &str, body: &'static str| {
        http.post(format!("{}/v1/stores/0/{path}", served.base))
            .header("content-type", "application/json")
            .bearer_auth(TOKEN)
            .body(body)
            .send()
    };
    let status = |response: reqwest::Response| response.status().as_u16();
    assert_eq!(
        status(post("nearest", "{\"owners\":[]}").await.unwrap()),
        400
    );
    assert_eq!(
        status(
            post(
                "delete",
                "{\"keys\":[{\"entity_id\":\"00000000-0000-0000-0000-000000000001\",\"model_id\":\"m\",\"dim\":385}],\"fence\":1}"
            )
            .await
            .unwrap()
        ),
        400
    );
    assert_eq!(
        status(
            post(
                "delete",
                "{\"keys\":[{\"entity_id\":\"00000000-0000-0000-0000-000000000001\",\"model_id\":\"m\",\"dim\":384}],\"fence\":0}"
            )
            .await
            .unwrap()
        ),
        400,
        "non-positive delete fence refused at the HTTP boundary"
    );
    assert_eq!(
        status(
            post(
                "nearest",
                "{\"owners\":[],\"memories\":[],\"supersession\":\"HeadsOnly\",\"model_id\":\"m\",\"dim\":384,\"query\":[1.0],\"window\":1}"
            )
            .await
            .unwrap()
        ),
        400,
        "a query not of its space's width"
    );
    assert_eq!(
        status(
            post(
                "put",
                "{\"entries\":[{\"entity\":{\"kind\":\"Fact\",\"id\":\"00000000-0000-0000-0000-000000000001\"},\"owner\":\"00000000-0000-0000-0000-000000000002\",\"version\":1,\"generation\":1,\"is_head\":true,\"model_id\":\"m\",\"dim\":384,\"vectors\":[]}]}"
            )
            .await
            .unwrap()
        ),
        400,
        "empty chunk list refused at the HTTP boundary"
    );
    let empty = client(&served, 0)
        .put(memory(1), owner(1), 1, 1, true, &[])
        .await;
    assert!(
        matches!(empty, Err(StorageError::ConstraintViolation(_))),
        "{empty:?}"
    );
    let zero_gen = client(&served, 0)
        .put(memory(1), owner(1), 1, 0, true, &[vector([1.0, 0.0])])
        .await;
    assert!(
        matches!(zero_gen, Err(StorageError::ConstraintViolation(_))),
        "{zero_gen:?}"
    );
}

/// An index whose answers break the port's promises.
struct Lying {
    candidates: Vec<VectorCandidate>,
    entries: Vec<VectorEntry>,
    has_more: bool,
}

#[async_trait::async_trait]
impl VectorIndex for Lying {
    async fn put_batch(&self, _: &[VectorPut<'_>]) -> Result<(), proxiima::VectorPutBatchError> {
        Err(StorageError::Internal("disk full".into()).into())
    }

    async fn delete(&self, _: &[VectorKey], _: i64) -> Result<(), StorageError> {
        Ok(())
    }

    async fn set_owner(
        &self,
        _: &[VectorMetadataTarget],
        _: OwnerId,
        _: i64,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    async fn set_head(
        &self,
        _: &[VectorMetadataTarget],
        _: bool,
        _: i64,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    async fn nearest(
        &self,
        _: &VectorCandidateFilter,
        _: &SpaceVector,
        _: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        Ok(proxiima_core::storage_ports::VectorCandidatePage::new(
            self.candidates.clone(),
            proxiima_core::storage_ports::CandidateBound::Bounded,
        ))
    }

    async fn entries(
        &self,
        _: Option<&VectorKey>,
        _: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        Ok(VectorEntryPage {
            entries: self.entries.clone(),
            has_more: self.has_more,
        })
    }
}

/// The client refuses answers past the requested window and entries outside
/// the requested keyset page.
#[tokio::test]
async fn the_client_refuses_answers_that_break_the_port() {
    let candidate = |score| VectorCandidate {
        memory_id: MemoryId::new(Uuid::now_v7()),
        similarity_score: score,
        generation: 1,
    };
    let entry = |id| VectorEntry {
        key: key(id),
        owner: owner(1),
        version: 1,
        generation: 1,
        is_head: true,
    };
    let lying = |candidates, entries, has_more| {
        Arc::new(Lying {
            candidates,
            entries,
            has_more,
        }) as Arc<dyn VectorIndex>
    };
    let served = serve(BTreeMap::from([
        (
            1,
            lying(
                vec![candidate(0.5), candidate(0.4)],
                vec![entry(2), entry(1)],
                false,
            ),
        ),
        (2, lying(vec![], vec![entry(1), entry(2)], false)),
        (3, lying(vec![], vec![], true)),
    ]))
    .await;
    let query = vector([1.0, 0.0]);
    let (first, second, third) = (client(&served, 1), client(&served, 2), client(&served, 3));
    let unavailable =
        |result: Result<_, StorageError>| matches!(result, Err(StorageError::Unavailable(_)));
    assert!(
        unavailable(
            first
                .nearest(
                    &VectorCandidateFilter {
                        owners: vec![owner(1)],
                        memories: vec![],
                        supersession: SupersessionStatus::HeadsOnly,
                    },
                    &query,
                    1,
                )
                .await
                .map(drop),
        ),
        "past the window"
    );
    assert!(
        first
            .nearest(
                &VectorCandidateFilter {
                    owners: vec![owner(1)],
                    memories: vec![],
                    supersession: SupersessionStatus::HeadsOnly,
                },
                &query,
                2,
            )
            .await
            .is_ok()
    );
    assert!(
        unavailable(first.entries(None, 5).await.map(drop)),
        "out of key order"
    );
    assert!(
        unavailable(second.entries(None, 1).await.map(drop)),
        "past the limit"
    );
    assert!(
        unavailable(second.entries(Some(&key(1)), 5).await.map(drop)),
        "not after the cursor"
    );
    assert!(
        unavailable(third.entries(None, 5).await.map(drop)),
        "empty page claiming has_more"
    );
    assert!(second.entries(None, 5).await.is_ok());
    assert!(
        unavailable(first.put(memory(1), owner(1), 1, 1, true, &[query]).await),
        "an index failure is the index down"
    );
}

/// The client sizes `put_batch` on exact encoded JSON bytes (including
/// `model_id`) and refuses an individually oversized or mixed-space entry as a
/// `ConstraintViolation` before sending any HTTP request.
#[tokio::test]
async fn put_batch_validates_and_sizes_entries_on_exact_bytes_before_sending() {
    let index = Arc::new(InMemoryVectorIndex::default());
    let served = serve(BTreeMap::from([(0, index.clone() as Arc<dyn VectorIndex>)])).await;
    let remote = client(&served, 0);

    // Mixed spaces in one entry refused before any network write.
    let other_space = EmbeddingSpace::new("other", EmbeddingDim::D384);
    let mixed = [
        vector([1.0, 0.0]),
        SpaceVector::new(other_space, vec![0.0; 384]).unwrap(),
    ];
    let mixed_err = remote.put(memory(1), owner(1), 1, 1, true, &mixed).await;
    assert!(
        matches!(mixed_err, Err(StorageError::ConstraintViolation(_))),
        "{mixed_err:?}"
    );
    assert!(index.entries(None, 10).await.unwrap().entries.is_empty());

    // A single entry whose encoded JSON (via a huge `model_id`) exceeds the
    // body limit is refused with ConstraintViolation rather than HTTP 413.
    let huge_space = EmbeddingSpace::new("m".repeat(VECTOR_INDEX_BODY_LIMIT), EmbeddingDim::D384);
    let huge_vec = [SpaceVector::new(huge_space, vec![0.0; 384]).unwrap()];
    let valid_vec = [vector([1.0, 0.0])];
    let oversized_err = remote
        .put_batch(&[
            VectorPut {
                entity: memory(1),
                owner: owner(1),
                version: 1,
                generation: 1,
                is_head: true,
                vectors: &valid_vec,
            },
            VectorPut {
                entity: memory(2),
                owner: owner(1),
                version: 1,
                generation: 2,
                is_head: true,
                vectors: &huge_vec,
            },
        ])
        .await;
    assert!(
        matches!(
            oversized_err,
            Err(proxiima::VectorPutBatchError {
                error: StorageError::ConstraintViolation(_),
                ref unsent_indices,
            }) if unsent_indices == &[0, 1]
        ),
        "{oversized_err:?}"
    );
    assert!(
        index.entries(None, 10).await.unwrap().entries.is_empty(),
        "upfront validation must not send earlier items when a later item is oversized"
    );

    // Multi-chunk HTTP failure: when chunk 1 fails on a 2-chunk batch, the
    // client reports chunk 2's trailing index in `unsent_indices` without
    // sending chunk 2.
    let failing = Arc::new(Lying {
        candidates: vec![],
        entries: vec![],
        has_more: false,
    }) as Arc<dyn VectorIndex>;
    let served_fail = serve(BTreeMap::from([(0, failing)])).await;
    let failing_remote = client(&served_fail, 0);
    // Two ~9 MiB items each fit under the 16 MiB body cap individually, so
    // `put_batch` splits them across two HTTP requests.
    let big_space = EmbeddingSpace::new(
        "m".repeat((VECTOR_INDEX_BODY_LIMIT / 2) + 1024),
        EmbeddingDim::D384,
    );
    let big_vec = [SpaceVector::new(big_space, vec![0.0; 384]).unwrap()];
    let split_err = failing_remote
        .put_batch(&[
            VectorPut {
                entity: memory(3),
                owner: owner(1),
                version: 1,
                generation: 1,
                is_head: true,
                vectors: &big_vec,
            },
            VectorPut {
                entity: memory(4),
                owner: owner(1),
                version: 1,
                generation: 2,
                is_head: true,
                vectors: &big_vec,
            },
        ])
        .await;
    assert!(
        matches!(
            split_err,
            Err(proxiima::VectorPutBatchError {
                error: StorageError::Unavailable(_),
                ref unsent_indices,
            }) if unsent_indices == &[1]
        ),
        "chunk 1 failure must report chunk 2 as unsent: {split_err:?}"
    );
}

fn semantic_request_for(owner: OwnerId) -> proxiima_core::verbs::query::MemorySearchRequest {
    use proxiima_core::verbs::query::{
        MemorySearchRequest, MemorySearchScope, SearchMode, SearchOrder, SupersessionStatus,
        TagMatch,
    };
    MemorySearchRequest {
        scope: MemorySearchScope::Owner(owner),
        read_owners: vec![owner],
        query: Some("index result".into()),
        mode: SearchMode::Semantic,
        supersession: SupersessionStatus::HeadsOnly,
        limit: 5,
        kind: None,
        schema_id: None,
        exclude_schema_ids: Vec::new(),
        tags: vec![],
        tag_match: TagMatch::All,
        since: None,
        until: None,
        order: SearchOrder::Relevance,
        min_score: None,
        semantic_weight: None,
        after: None,
        semantic: Some(vector([1.0, 0.0])),
    }
}

/// An oversized HTTP entry in a mixed batch is rejected via `validate_put_batch`
/// BEFORE Phase 1 commits any write-ahead delete intent, so the subsequent debt
/// drain never deletes untouched committed vectors belonging to earlier keys in
/// that batch.
#[tokio::test]
async fn oversized_http_batch_rejects_before_committing_intent_leaving_committed_vectors_intact() {
    use proxiima_core::storage_ports::{
        EmbeddingWrite, EmbeddingWritePort, EmbeddingWriteProof, MemoryReadPort,
    };
    use proxiima_core::test_fixtures::owner_scope_for_tests;
    use proxiima_storage_pg::test_fixtures::{TestNote, fresh_pg_host_index};

    let backing = Arc::new(InMemoryVectorIndex::default());
    let served = serve(BTreeMap::from([(
        0,
        backing.clone() as Arc<dyn VectorIndex>,
    )]))
    .await;
    let remote = Arc::new(client(&served, 0));
    let (pg, _db) = fresh_pg_host_index("http_oversized_batch_keeps_committed", remote).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let existing = TestNote::new("Index result", "committed v1")
        .create(&pg, owner)
        .await
        .unwrap()
        .memory_id;
    let oversized_target = TestNote::new("Oversized", "body")
        .create(&pg, owner)
        .await
        .unwrap()
        .memory_id;

    let v1 = [vector([1.0, 0.0])];
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &owner,
        entity: EmbeddableEntityRef::Memory {
            kind: MemoryKind::Fact,
            memory_id: existing,
        },
        vectors: &v1,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await
    .unwrap();
    let committed_gen = backing.entries(None, 10).await.unwrap().entries[0].generation;

    // Submit a mixed batch: valid overwrite of `existing` + multi-chunk entry exceeding the HTTP cap.
    let v2 = [vector([0.0, 1.0])];
    let huge_space = EmbeddingSpace::new("http-index", EmbeddingDim::D3072);
    let huge_chunk = SpaceVector::new(huge_space, vec![0.123_456_7_f32; 3072]).unwrap();
    // Each float serializes as `0.1234567,` (10 bytes), so `limit / (3072 * 9) + 1` always exceeds the cap.
    let chunk_count = VECTOR_INDEX_BODY_LIMIT / (3072 * 9) + 1;
    let huge_vec = vec![huge_chunk; chunk_count];
    let err = pg
        .insert_embeddings(&[
            EmbeddingWrite {
                owner: &owner,
                entity: EmbeddableEntityRef::Memory {
                    kind: MemoryKind::Fact,
                    memory_id: existing,
                },
                vectors: &v2,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            },
            EmbeddingWrite {
                owner: &owner,
                entity: EmbeddableEntityRef::Memory {
                    kind: MemoryKind::Fact,
                    memory_id: oversized_target,
                },
                vectors: &huge_vec,
                proof: EmbeddingWriteProof::for_direct_write_for_tests(),
            },
        ])
        .await;
    assert!(
        matches!(err, Err(StorageError::ConstraintViolation(_))),
        "{err:?}"
    );

    let debt_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM proxiima_core.vector_index_debt")
            .fetch_one(pg.pool_for_tests())
            .await
            .unwrap();
    assert_eq!(debt_count, 0);
    assert_eq!(pg.drain_vector_index_debt(100).await.unwrap(), 0);

    let entries = backing.entries(None, 10).await.unwrap().entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key.entity_id, existing.into_inner());
    assert_eq!(entries[0].generation, committed_gen);

    let scope = owner_scope_for_tests(&[owner]);
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let page = pg
        .search_memories(
            Some(&scope),
            &semantic_request_for(owner),
            registry.search_projections(),
        )
        .await
        .unwrap();
    assert_eq!(page.results.len(), 1);
    assert_eq!(page.results[0].memory_id, existing);

    // Confirm the host maintenance facade exposes both debt drain and slice sweep.
    let _ = pin_host_maintenance_facade;
}

async fn pin_host_maintenance_facade(
    storage: &proxiima::ConnectedStorage,
    after: Option<&proxiima::VectorKey>,
) -> Result<(u64, proxiima::VectorIndexSweepOutcome), proxiima::StorageError> {
    let drained = storage.drain_vector_index_debt(100).await?;
    let swept = storage.sweep_vector_index_slice(after, 100).await?;
    Ok((drained, swept))
}

#[tokio::test]
async fn search_refuses_an_out_of_range_score_from_the_http_index() {
    use proxiima_core::storage_ports::MemoryReadPort;
    use proxiima_core::test_fixtures::owner_scope_for_tests;
    use proxiima_core::verbs::query::{
        MemorySearchRequest, MemorySearchScope, SearchMode, SearchOrder, SupersessionStatus,
        TagMatch,
    };
    use proxiima_storage_pg::test_fixtures::{TestNote, fresh_pg_host_index};

    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let candidate_id = MemoryId::new(Uuid::now_v7());
    let served = serve(BTreeMap::from([(
        1,
        Arc::new(Lying {
            candidates: vec![VectorCandidate {
                memory_id: candidate_id,
                similarity_score: 1.5,
                generation: 1,
            }],
            entries: vec![],
            has_more: false,
        }) as Arc<dyn VectorIndex>,
    )]))
    .await;
    let index = client(&served, 1);
    let (pg, _db) = fresh_pg_host_index("http_index_invalid_score", Arc::new(index)).await;
    TestNote::new("Index result", "Hydrated from memory")
        .create(&pg, owner)
        .await
        .unwrap();
    let scope = owner_scope_for_tests(&[owner]);
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let request = MemorySearchRequest {
        scope: MemorySearchScope::Owner(owner),
        read_owners: vec![owner],
        query: Some("index result".into()),
        mode: SearchMode::Semantic,
        supersession: SupersessionStatus::HeadsOnly,
        limit: 1,
        kind: None,
        schema_id: None,
        exclude_schema_ids: Vec::new(),
        tags: vec![],
        tag_match: TagMatch::All,
        since: None,
        until: None,
        order: SearchOrder::Relevance,
        min_score: None,
        semantic_weight: None,
        after: None,
        semantic: Some(vector([1.0, 0.0])),
    };
    let result = pg
        .search_memories(Some(&scope), &request, registry.search_projections())
        .await;
    assert!(
        matches!(
            result,
            Err(StorageError::Unavailable(ref message)) if message.contains("score outside 0..=1")
        ),
        "{result:?}",
    );
}

/// Every call carries memory vectors, so the client takes plaintext only to
/// loopback or a named host.
#[test]
fn the_client_takes_plaintext_only_to_loopback_or_a_named_host() {
    let none = PlaintextHosts::none();
    let new = |base: &str, hosts: &PlaintextHosts| VectorIndexClient::new(base, 0, hosts, token());
    assert!(new("https://index.example", &none).is_ok());
    assert!(new("http://127.0.0.1:9", &none).is_ok());
    assert!(new("http://index.example", &none).is_err());
    assert!(new("ftp://index.example", &none).is_err());
    assert!(new("not a url", &none).is_err());
    let named = PlaintextHosts::parse("index.internal").unwrap();
    assert!(new("http://index.internal:8080/base", &named).is_ok());
}

/// The router serves only a caller with its token, and turns any other away
/// before it reads the body: a malformed body without the token is a `401`,
/// not a `400`.
#[tokio::test]
async fn the_router_serves_only_the_holder_of_its_token() {
    let served = serve(BTreeMap::from([(
        0,
        Arc::new(InMemoryVectorIndex::default()) as Arc<dyn VectorIndex>,
    )]))
    .await;
    let url = format!("{}/v1/stores/0/delete", served.base);
    let http = reqwest::Client::new();
    let refused = |response: reqwest::Response| {
        response.status() == reqwest::StatusCode::UNAUTHORIZED
            && response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .is_some_and(|challenge| challenge == "Bearer")
    };
    let no_token = http.post(&url).body("not json").send().await.unwrap();
    assert!(refused(no_token), "no token");
    let other = http.post(&url).bearer_auth(OTHER).body("not json");
    assert!(refused(other.send().await.unwrap()), "another token");

    let lowercase = http
        .post(&url)
        .header(reqwest::header::AUTHORIZATION, format!("bearer {TOKEN}"))
        .json(&serde_json::json!({ "keys": [], "fence": 1 }));
    assert_eq!(
        lowercase.send().await.unwrap().status(),
        reqwest::StatusCode::OK,
        "the scheme is case-insensitive"
    );

    let stranger = VectorIndexToken::new(OTHER.into()).unwrap();
    let stranger =
        VectorIndexClient::new(&served.base, 0, &PlaintextHosts::none(), stranger).unwrap();
    assert!(matches!(
        stranger.delete(&[key(1)], 1).await,
        Err(StorageError::Unavailable(_))
    ));
}

/// A call without the token is turned away before its body is read: the
/// body here fails the test the moment anything polls it.
#[tokio::test]
async fn the_router_reads_no_body_before_the_token() {
    use tower::ServiceExt;

    let unread = futures::stream::poll_fn(
        |_| -> std::task::Poll<Option<Result<axum::body::Bytes, std::io::Error>>> {
            panic!("the router read an unauthenticated body")
        },
    );
    let request = axum::http::Request::post("/v1/stores/0/delete")
        .body(axum::body::Body::from_stream(unread))
        .unwrap();
    let response = vector_index_router(BTreeMap::new(), &token())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
}

/// A token is long, printable ASCII without spaces, and never shown.
#[test]
fn a_token_is_long_printable_ascii() {
    let at_least = "a".repeat(VECTOR_INDEX_TOKEN_MIN_LEN);
    assert!(VectorIndexToken::new(at_least).is_ok());
    let short = "a".repeat(VECTOR_INDEX_TOKEN_MIN_LEN - 1);
    assert!(VectorIndexToken::new(short).is_err(), "too short");
    assert!(
        VectorIndexToken::new(format!("{TOKEN} ")).is_err(),
        "a space"
    );
    assert!(
        VectorIndexToken::new(format!("{TOKEN}\n")).is_err(),
        "a control character"
    );
    assert!(
        VectorIndexToken::new(format!("{TOKEN}é")).is_err(),
        "not ASCII"
    );
    assert!(
        !format!("{:?}", token()).contains(TOKEN),
        "Debug never shows it"
    );
}

struct PartialUnsentBackend {
    succeed_first: std::sync::atomic::AtomicBool,
    unsent: Vec<usize>,
}

#[async_trait::async_trait]
impl VectorIndex for PartialUnsentBackend {
    async fn put_batch(&self, _: &[VectorPut<'_>]) -> Result<(), proxiima::VectorPutBatchError> {
        if self
            .succeed_first
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        Err(proxiima::VectorPutBatchError::with_unsent(
            StorageError::Unavailable("internal sub-batch failed".into()),
            self.unsent.clone(),
        ))
    }
    async fn delete(&self, _: &[VectorKey], _: i64) -> Result<(), StorageError> {
        Ok(())
    }
    async fn set_owner(
        &self,
        _: &[VectorMetadataTarget],
        _: OwnerId,
        _: i64,
    ) -> Result<(), StorageError> {
        Ok(())
    }
    async fn set_head(
        &self,
        _: &[VectorMetadataTarget],
        _: bool,
        _: i64,
    ) -> Result<(), StorageError> {
        Ok(())
    }
    async fn nearest(
        &self,
        _: &VectorCandidateFilter,
        _: &SpaceVector,
        _: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        Ok(proxiima_core::storage_ports::VectorCandidatePage::new(
            Vec::new(),
            proxiima_core::storage_ports::CandidateBound::Unknown,
        ))
    }
    async fn entries(
        &self,
        _: Option<&VectorKey>,
        _: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        Ok(VectorEntryPage::new(Vec::new(), false))
    }
}

/// The HTTP wire preserves provider-reported request-relative `unsent_indices`
/// inside a failed request and translates them to batch offsets alongside
/// trailing unsent HTTP chunks; if a lying server sends an out-of-range index,
/// the client discards the server list and keeps the failed chunk uncertain.
#[tokio::test]
async fn http_put_preserves_provider_unsent_indices_and_rejects_out_of_range_indices() {
    let item = |id, generation, vectors| VectorPut {
        entity: memory(id),
        owner: owner(1),
        version: 1,
        generation,
        is_head: true,
        vectors,
    };
    // 1. Chunk 1 (`0..2`) succeeds; chunk 2 (`2..4`) fails with request-relative
    //    `unsent_indices: [1]` (batch index `3`), while chunk 3 (`4..5`) is never
    //    sent -> client translates and merges to `[3, 4]`.
    let backend = Arc::new(PartialUnsentBackend {
        succeed_first: std::sync::atomic::AtomicBool::new(true),
        unsent: vec![1],
    }) as Arc<dyn VectorIndex>;
    let served = serve(BTreeMap::from([(0, backend)])).await;
    let remote = client(&served, 0);

    // Each item is slightly over 1/3 of `VECTOR_INDEX_BODY_LIMIT`, so 2 items
    // fit per HTTP request (`0..2`, `2..4`) and item 4 spills into request 3.
    let med_space = EmbeddingSpace::new(
        "m".repeat((VECTOR_INDEX_BODY_LIMIT / 3) + 1024),
        EmbeddingDim::D384,
    );
    let med_vec = [SpaceVector::new(med_space, vec![0.0; 384]).unwrap()];
    let err = remote
        .put_batch(&[
            item(1, 1, &med_vec),
            item(2, 2, &med_vec),
            item(3, 3, &med_vec),
            item(4, 4, &med_vec),
            item(5, 5, &med_vec),
        ])
        .await;
    assert!(
        matches!(
            err,
            Err(proxiima::VectorPutBatchError {
                error: StorageError::Unavailable(_),
                ref unsent_indices,
            }) if unsent_indices == &[3, 4]
        ),
        "must translate chunk-2 relative [1] -> batch [3] and include chunk-3 [4]: {err:?}"
    );

    // 2. A raw server replying with out-of-range `unsent_indices: [0, 99]` on
    //    chunk 1 (len 1) must not confuse the client: only trailing chunk [1]
    //    is trusted as unsent.
    let lying_app = axum::Router::new().route(
        "/v1/stores/0/put",
        axum::routing::post(|| async {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"error":"unavailable","unsent_indices":[0,99]}"#,
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, lying_app).await });
    let lying_client = VectorIndexClient::new(
        &format!("http://{addr}"),
        0,
        &PlaintextHosts::none(),
        token(),
    )
    .unwrap();
    let big_space = EmbeddingSpace::new(
        "m".repeat((VECTOR_INDEX_BODY_LIMIT / 2) + 1024),
        EmbeddingDim::D384,
    );
    let big_vec = [SpaceVector::new(big_space, vec![0.0; 384]).unwrap()];
    let lying_err = lying_client
        .put_batch(&[item(4, 1, &big_vec), item(5, 2, &big_vec)])
        .await;
    handle.abort();
    assert!(
        matches!(
            lying_err,
            Err(proxiima::VectorPutBatchError {
                error: StorageError::Unavailable(_),
                ref unsent_indices,
            }) if unsent_indices == &[1]
        ),
        "out-of-range server unsent_indices must fall back to trailing chunk only: {lying_err:?}"
    );

    // 3. A buggy backing adapter returning `[0, 99]` on a 2-item chunk through
    //    `vector_index_router` must cause the router to treat the entire failed
    //    request as uncertain (omitting `0`), leaving only trailing chunk `[2]`.
    let buggy_backend = Arc::new(PartialUnsentBackend {
        succeed_first: std::sync::atomic::AtomicBool::new(false),
        unsent: vec![0, 99],
    }) as Arc<dyn VectorIndex>;
    let buggy_served = serve(BTreeMap::from([(0, buggy_backend)])).await;
    let buggy_client = client(&buggy_served, 0);
    let buggy_err = buggy_client
        .put_batch(&[
            item(6, 1, &med_vec),
            item(7, 2, &med_vec),
            item(8, 3, &med_vec),
        ])
        .await;
    assert!(
        matches!(
            buggy_err,
            Err(proxiima::VectorPutBatchError {
                error: StorageError::Unavailable(_),
                ref unsent_indices,
            }) if unsent_indices == &[2]
        ),
        "router must treat an invalid backing unsent_indices report as wholly uncertain: {buggy_err:?}"
    );
}

/// A raw server replying with contradictory `all_unsent: true` AND non-empty
/// `unsent_indices` must be treated as wholly uncertain for that request
/// (whether indices are out-of-range `[1]` or in-range `[0]`): a 1-item batch
/// reports `[]` and a 2-chunk batch trusts only trailing chunk `[1]`.
#[tokio::test]
async fn http_client_rejects_contradictory_all_unsent_and_indices() {
    let item = |id, generation, vectors| VectorPut {
        entity: memory(id),
        owner: owner(1),
        version: 1,
        generation,
        is_head: true,
        vectors,
    };
    let big_space = EmbeddingSpace::new(
        "m".repeat((VECTOR_INDEX_BODY_LIMIT / 2) + 1024),
        EmbeddingDim::D384,
    );
    let big_vec = [SpaceVector::new(big_space, vec![0.0; 384]).unwrap()];
    for payload in [
        r#"{"error":"unavailable","all_unsent":true,"unsent_indices":[1]}"#,
        r#"{"error":"unavailable","all_unsent":true,"unsent_indices":[0]}"#,
    ] {
        let mixed_app = axum::Router::new().route(
            "/v1/stores/0/put",
            axum::routing::post(move || async move {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    payload,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move { axum::serve(listener, mixed_app).await });
        let mixed_client = VectorIndexClient::new(
            &format!("http://{addr}"),
            0,
            &PlaintextHosts::none(),
            token(),
        )
        .unwrap();
        let single_err = mixed_client.put_batch(&[item(9, 1, &big_vec)]).await;
        let two_chunk_err = mixed_client
            .put_batch(&[item(10, 1, &big_vec), item(11, 2, &big_vec)])
            .await;
        handle.abort();
        assert!(
            matches!(
                single_err,
                Err(proxiima::VectorPutBatchError {
                    error: StorageError::Unavailable(_),
                    ref unsent_indices,
                }) if unsent_indices.is_empty()
            ),
            "contradictory {payload} on 1-item batch must report empty unsent_indices: {single_err:?}"
        );
        assert!(
            matches!(
                two_chunk_err,
                Err(proxiima::VectorPutBatchError {
                    error: StorageError::Unavailable(_),
                    ref unsent_indices,
                }) if unsent_indices == &[1]
            ),
            "contradictory {payload} on 2-chunk batch must trust only trailing chunk [1]: {two_chunk_err:?}"
        );
    }
}

/// The `unsent_indices` the router itself answers a two-entry `/put` with,
/// read off the raw response: a `503` that never claims `all_unsent`.
async fn router_unsent(served: &Served) -> Vec<usize> {
    let mut values = vec![0.0_f32; 384];
    values[0] = 1.0;
    let wire_put = |id: u128| {
        serde_json::json!({
            "entity": {"kind": proxiima_core::EntityKind::Perspective, "id": Uuid::from_u128(id)},
            "owner": owner(1).into_inner(),
            "version": 1,
            "generation": 1,
            "is_head": true,
            "model_id": "wire",
            "dim": 384,
            "vectors": [values.clone()],
        })
    };
    let response = reqwest::Client::new()
        .post(format!("{}/v1/stores/0/put", served.base))
        .header("content-type", "application/json")
        .bearer_auth(TOKEN)
        .body(serde_json::json!({"entries": [wire_put(1), wire_put(2)]}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 503);
    let answer: serde_json::Value = response.json().await.unwrap();
    assert_eq!(answer.get("all_unsent"), None, "{answer}");
    answer
        .get("unsent_indices")
        .map_or_else(Vec::new, |indices| {
            serde_json::from_value(indices.clone()).unwrap()
        })
}

/// `/put` refusals read as `Proxiima.Search.putUnsentReading` decides: a raw
/// server answers each `all_unsent` and `unsent_indices` combination for a
/// two-entry request, and the router forwards what an index reports without
/// `all_unsent`. The router's own answer is read raw as well, so a client that
/// discards what the router should have dropped cannot hide it.
#[tokio::test]
async fn put_refusals_read_as_the_ledger_decides() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../core/tests/ledger_vectors.json")).unwrap();
    let rows = vectors["Proxiima.Search.putUnsentReading"]
        .as_array()
        .unwrap();
    assert!(!rows.is_empty());
    let chunks = [vector([1.0, 0.0])];
    let entries = [1, 2].map(|id| VectorPut {
        entity: memory(id),
        owner: owner(1),
        version: 1,
        generation: 1,
        is_head: true,
        vectors: &chunks,
    });
    let unsent_of = |result: Result<(), proxiima::VectorPutBatchError>| {
        result.expect_err("the put is refused").unsent_indices
    };
    for row in rows {
        let all_unsent = row["in"][0].as_bool().unwrap();
        let indices = match row["in"][1].as_str().unwrap() {
            "absent" => Vec::new(),
            "inRange" => vec![1],
            "outOfRange" => vec![2],
            other => panic!("unknown unsent indices {other}"),
        };
        let read: Vec<usize> = match row["out"].as_str().unwrap() {
            "every" => vec![0, 1],
            "listed" => vec![1],
            "uncertain" => Vec::new(),
            other => panic!("unknown unsent reading {other}"),
        };

        let body = serde_json::json!({
            "error": "refused",
            "all_unsent": all_unsent,
            "unsent_indices": indices,
        })
        .to_string();
        let app = axum::Router::new().route(
            "/v1/stores/0/put",
            axum::routing::post(move || {
                let body = body.clone();
                async move {
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let raw = tokio::spawn(async move { axum::serve(listener, app).await });
        let raw_client = VectorIndexClient::new(
            &format!("http://{addr}"),
            0,
            &PlaintextHosts::none(),
            token(),
        )
        .unwrap();
        let client_read = unsent_of(raw_client.put_batch(&entries).await);
        raw.abort();
        assert_eq!(client_read, read, "client: {row}");

        if !all_unsent {
            let backend = Arc::new(PartialUnsentBackend {
                succeed_first: std::sync::atomic::AtomicBool::new(false),
                unsent: indices,
            }) as Arc<dyn VectorIndex>;
            let served = serve(BTreeMap::from([(0, backend)])).await;
            let routed = unsent_of(client(&served, 0).put_batch(&entries).await);
            assert_eq!(routed, read, "router: {row}");

            assert_eq!(router_unsent(&served).await, read, "router body: {row}");
        }
    }
}

#[derive(Debug)]
struct SwitchableClient {
    good: VectorIndexClient,
    stale: VectorIndexClient,
    use_stale: Arc<std::sync::atomic::AtomicBool>,
}

impl SwitchableClient {
    fn active(&self) -> &VectorIndexClient {
        if self.use_stale.load(std::sync::atomic::Ordering::SeqCst) {
            &self.stale
        } else {
            &self.good
        }
    }
}

#[async_trait::async_trait]
impl VectorIndex for SwitchableClient {
    async fn put_batch(
        &self,
        items: &[VectorPut<'_>],
    ) -> Result<(), proxiima_core::storage_ports::VectorPutBatchError> {
        self.active().put_batch(items).await
    }
    async fn delete(&self, keys: &[VectorKey], fence_generation: i64) -> Result<(), StorageError> {
        self.active().delete(keys, fence_generation).await
    }
    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.active().set_owner(targets, owner, revision).await
    }
    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        self.active().set_head(targets, is_head, revision).await
    }
    async fn nearest(
        &self,
        filter: &VectorCandidateFilter,
        query: &SpaceVector,
        limit: u32,
    ) -> Result<proxiima_core::storage_ports::VectorCandidatePage, StorageError> {
        self.active().nearest(filter, query, limit).await
    }
    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        self.active().entries(after, limit).await
    }
}

/// Pre-dispatch HTTP refusals (token mismatch `401` during rotation and unknown
/// store `404`) carry `all_unsent: true` so the client marks every entry in the
/// rejected request (plus trailing chunks) as unsent and `PgStorage` preserves
/// committed vectors once credentials recover and drain runs.
#[tokio::test]
async fn http_pre_dispatch_refusals_preserve_committed_vectors_after_token_recovery() {
    use proxiima_core::storage_ports::{
        EmbeddingWrite, EmbeddingWritePort, EmbeddingWriteProof, MemoryReadPort,
    };
    use proxiima_core::test_fixtures::owner_scope_for_tests;
    use proxiima_storage_pg::test_fixtures::{TestNote, fresh_pg_host_index};

    let backing = Arc::new(InMemoryVectorIndex::default());
    let served = serve(BTreeMap::from([(0, backing as Arc<dyn VectorIndex>)])).await;

    // 1. Direct client checks for `401` (stale token) and `404` (unknown store).
    let wrong_token = VectorIndexToken::new(OTHER.into()).unwrap();
    let stale_client =
        VectorIndexClient::new(&served.base, 0, &PlaintextHosts::none(), wrong_token).unwrap();
    let unknown_store_client = client(&served, 99);
    let v = [vector([1.0, 0.0])];
    let put = |id, generation| VectorPut {
        entity: memory(id),
        owner: owner(1),
        version: 1,
        generation,
        is_head: true,
        vectors: &v,
    };
    let puts = [put(1, 1), put(2, 2)];
    for (label, remote) in [("401", &stale_client), ("404", &unknown_store_client)] {
        let err = remote.put_batch(&puts).await;
        assert!(
            matches!(
                err,
                Err(proxiima::VectorPutBatchError {
                    error: StorageError::Unavailable(_),
                    ref unsent_indices,
                }) if unsent_indices == &[0, 1]
            ),
            "{label} pre-dispatch refusal must mark all entries unsent: {err:?}"
        );
    }

    // 2. End-to-end with PgStorage: commit v1 using valid credentials, fail an
    //    overwrite through a stale-token client, recover credentials and drain.
    let use_stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let switchable = Arc::new(SwitchableClient {
        good: client(&served, 0),
        stale: stale_client,
        use_stale: Arc::clone(&use_stale),
    });
    let (pg, _database) = fresh_pg_host_index("http_token_rotation_unsent", switchable).await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let note = TestNote::new("index result", "committed v1")
        .create(&pg, owner)
        .await
        .unwrap()
        .memory_id;
    let entity = EmbeddableEntityRef::Memory {
        kind: MemoryKind::Fact,
        memory_id: note,
    };
    let v1 = [vector([1.0, 0.0])];
    pg.insert_embeddings(&[EmbeddingWrite {
        owner: &owner,
        entity,
        vectors: &v1,
        proof: EmbeddingWriteProof::for_direct_write_for_tests(),
    }])
    .await
    .unwrap();

    use_stale.store(true, std::sync::atomic::Ordering::SeqCst);
    let v2 = [vector([0.0, 1.0])];
    let err = pg
        .insert_embeddings(&[EmbeddingWrite {
            owner: &owner,
            entity,
            vectors: &v2,
            proof: EmbeddingWriteProof::for_direct_write_for_tests(),
        }])
        .await;
    assert!(matches!(err, Err(StorageError::Unavailable(_))));
    use_stale.store(false, std::sync::atomic::Ordering::SeqCst);

    let debt: i64 = sqlx::query_scalar("SELECT count(*) FROM proxiima_core.vector_index_debt")
        .fetch_one(pg.pool_for_tests())
        .await
        .unwrap();
    assert_eq!(debt, 0);
    assert_eq!(pg.drain_vector_index_debt(100).await.unwrap(), 0);
    let scope = owner_scope_for_tests(&[owner]);
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let page = pg
        .search_memories(
            Some(&scope),
            &semantic_request_for(owner),
            registry.search_projections(),
        )
        .await
        .unwrap();
    assert_eq!(page.results.len(), 1);
    assert_eq!(page.results[0].memory_id, note);
}
