//! The network form of a store's vector index: a client and the router
//! between them behave as the index behind the router, each side refuses
//! what the other must not send, and the router serves only its token's
//! holder.

use std::collections::BTreeMap;
use std::sync::Arc;

use proxiima::vector_index_http::{
    VECTOR_INDEX_TOKEN_MIN_LEN, VectorIndexClient, VectorIndexToken, vector_index_router,
};
use proxiima_core::storage_ports::{
    InMemoryVectorIndex, VectorCandidate, VectorCandidateFilter, VectorEntry, VectorIndex,
    VectorKey,
};
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
        .put(memory(1), me, 2, &[vector([0.0, 1.0]), vector([1.0, 0.0])])
        .await
        .unwrap();
    remote
        .put(memory(2), me, 1, &[vector([1.0, 1.0])])
        .await
        .unwrap();
    remote
        .put(
            EmbeddableEntityRef::Goal(GoalId::new(Uuid::from_u128(3))),
            me,
            1,
            &[vector([1.0, 0.0])],
        )
        .await
        .unwrap();

    let query = vector([1.0, 0.0]);
    let filter = VectorCandidateFilter {
        owners: vec![me],
        memories: vec![],
    };
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
        .put(memory(4), you, 1, &[vector([1.0, 0.0])])
        .await
        .unwrap();
    remote
        .put(memory(5), you, 1, &[vector([0.0, 1.0])])
        .await
        .unwrap();
    let exact = VectorCandidateFilter {
        owners: vec![],
        memories: vec![MemoryId::new(Uuid::from_u128(5))],
    };
    let exact_ranked = remote.nearest(&exact, &query, 1).await.unwrap().candidates;
    assert_eq!(exact_ranked.len(), 1);
    assert_eq!(exact_ranked[0].memory_id, MemoryId::new(Uuid::from_u128(5)));
    remote.delete(&[key(4), key(5)]).await.unwrap();
    assert_eq!(
        remote.entries(None, 10).await.unwrap(),
        mine.entries(None, 10).await.unwrap()
    );
    let page = remote.entries(None, 1).await.unwrap();
    let rest = remote.entries(Some(&page[0].key), 10).await.unwrap();
    assert_eq!(page.len() + rest.len(), 3);

    remote.set_owner(&[key(2)], you).await.unwrap();
    remote.delete(&[key(1), key(99)]).await.unwrap();
    assert!(
        remote
            .nearest(&filter, &query, 10)
            .await
            .unwrap()
            .candidates
            .is_empty()
    );
    let moved = remote
        .nearest(
            &VectorCandidateFilter {
                owners: vec![you],
                memories: vec![],
            },
            &query,
            10,
        )
        .await
        .unwrap()
        .candidates;
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].memory_id, MemoryId::new(Uuid::from_u128(2)));
    let left: Vec<VectorEntry> = mine.entries(None, 10).await.unwrap();
    assert_eq!(left.len(), 2);
    assert_eq!(left[0].version, 1);
    assert!(other.entries(None, 10).await.unwrap().is_empty());
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
        .put(memory(1), private, 1, &[vector([1.0, 0.0])])
        .await
        .unwrap();
    remote
        .put(memory(2), private, 1, &[vector([0.0, 1.0])])
        .await
        .unwrap();

    let candidates = remote
        .nearest(
            &VectorCandidateFilter {
                owners: vec![allowed],
                memories: vec![target],
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
                "{\"keys\":[{\"entity_id\":\"00000000-0000-0000-0000-000000000001\",\"model_id\":\"m\",\"dim\":385}]}"
            )
            .await
            .unwrap()
        ),
        400
    );
    assert_eq!(
        status(
            post(
                "nearest",
                "{\"owners\":[],\"memories\":[],\"model_id\":\"m\",\"dim\":384,\"query\":[1.0],\"window\":1}"
            )
            .await
            .unwrap()
        ),
        400,
        "a query not of its space's width"
    );
    let empty = client(&served, 0).put(memory(1), owner(1), 1, &[]).await;
    assert!(
        matches!(empty, Err(StorageError::ConstraintViolation(_))),
        "{empty:?}"
    );
}

/// An index whose answers break the port's promises.
struct Lying {
    candidates: Vec<VectorCandidate>,
    entries: Vec<VectorEntry>,
}

#[async_trait::async_trait]
impl VectorIndex for Lying {
    async fn put(
        &self,
        _: EmbeddableEntityRef,
        _: OwnerId,
        _: i32,
        _: &[SpaceVector],
    ) -> Result<(), StorageError> {
        Err(StorageError::Internal("disk full".into()))
    }

    async fn delete(&self, _: &[VectorKey]) -> Result<(), StorageError> {
        Ok(())
    }

    async fn set_owner(&self, _: &[VectorKey], _: OwnerId) -> Result<(), StorageError> {
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
    ) -> Result<Vec<VectorEntry>, StorageError> {
        Ok(self.entries.clone())
    }
}

/// The client parses each answer where it enters the store: more candidates
/// than the window, a score outside `0..=1`, and entries out of key order or
/// past the limit are the index failing, not results.
#[tokio::test]
async fn the_client_refuses_answers_that_break_the_port() {
    let candidate = |score| VectorCandidate {
        memory_id: MemoryId::new(Uuid::now_v7()),
        similarity_score: score,
    };
    let entry = |id| VectorEntry {
        key: key(id),
        owner: owner(1),
        version: 1,
    };
    let lying = |candidates, entries| {
        Arc::new(Lying {
            candidates,
            entries,
        }) as Arc<dyn VectorIndex>
    };
    let served = serve(BTreeMap::from([
        (
            1,
            lying(
                vec![candidate(0.5), candidate(0.4)],
                vec![entry(2), entry(1)],
            ),
        ),
        (2, lying(vec![candidate(1.5)], vec![entry(1), entry(2)])),
    ]))
    .await;
    let query = vector([1.0, 0.0]);
    let (first, second) = (client(&served, 1), client(&served, 2));
    let unavailable =
        |result: Result<_, StorageError>| matches!(result, Err(StorageError::Unavailable(_)));
    assert!(
        unavailable(
            first
                .nearest(
                    &VectorCandidateFilter {
                        owners: vec![owner(1)],
                        memories: vec![],
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
                },
                &query,
                2,
            )
            .await
            .is_ok()
    );
    assert!(
        unavailable(
            second
                .nearest(
                    &VectorCandidateFilter {
                        owners: vec![owner(1)],
                        memories: vec![],
                    },
                    &query,
                    5,
                )
                .await
                .map(drop),
        ),
        "score above 1"
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
    assert!(second.entries(None, 5).await.is_ok());
    assert!(
        unavailable(first.put(memory(1), owner(1), 1, &[query]).await),
        "an index failure is the index down"
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
        .json(&serde_json::json!({ "keys": [] }));
    assert_eq!(
        lowercase.send().await.unwrap().status(),
        reqwest::StatusCode::OK,
        "the scheme is case-insensitive"
    );

    let stranger = VectorIndexToken::new(OTHER.into()).unwrap();
    let stranger =
        VectorIndexClient::new(&served.base, 0, &PlaintextHosts::none(), stranger).unwrap();
    assert!(matches!(
        stranger.delete(&[key(1)]).await,
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
