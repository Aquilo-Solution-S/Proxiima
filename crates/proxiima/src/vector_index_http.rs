//! The network form of a store's vector index (#519): [`VectorIndexClient`]
//! is a [`VectorIndex`] a store calls over HTTP, and [`vector_index_router`]
//! serves any [`VectorIndex`] to the stores it names, so one host can back
//! several stores' indexes with the engine it chooses.
//!
//! The wire is JSON over `POST`, under the router's mount point:
//!
//! | Path | Body | Answer |
//! |---|---|---|
//! | `/v1/stores/{store}/put` | `{entries: [{entity, owner, version, generation, is_head, model_id, dim, vectors}]}` | `{}` |
//! | `/v1/stores/{store}/delete` | `{keys, fence}` | `{}` |
//! | `/v1/stores/{store}/owner` | `{targets: [{entity_id, model_id, dim, generation}], owner, revision}` | `{}` |
//! | `/v1/stores/{store}/head` | `{targets: [{entity_id, model_id, dim, generation}], is_head, revision}` | `{}` |
//! | `/v1/stores/{store}/nearest` | `{owners, memories, supersession, model_id, dim, query, window}` | `{candidates: [{memory_id, score, generation}]}` |
//! | `/v1/stores/{store}/entries` | `{after, limit}` | `{entries: [{entity_id, model_id, dim, owner, version, generation, is_head}], has_more}` |
//!
//! `entity` is `{"kind": "Fact" | "Abstraction" | "Perspective" | "Goal", "id": uuid}`;
//! a key is `{entity_id, model_id, dim}`; `vectors` and `query` are arrays of
//! numbers of the space's width. A refusal answers `400` with
//! `{"error": text}` (and optional `all_unsent` / request-relative
//! `unsent_indices` on `/put`), any other failure `503` without the index's
//! own error (carrying validated `unsent_indices` on `/put` when reported),
//! pre-dispatch `401`/`404` refusals carry `all_unsent: true`, and an unknown
//! store answers `404`. A body over [`VECTOR_INDEX_BODY_LIMIT`] is refused.
//!
//! Every call carries `Authorization: Bearer <token>`, the
//! [`VectorIndexToken`] the client and the router are both built with. The
//! router answers any other call `401` before it reads the body.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use proxiima_core::storage_ports::{
    CandidateBound, VectorCandidate, VectorCandidateFilter, VectorCandidatePage, VectorEntry,
    VectorEntryPage, VectorIndex, VectorKey, VectorMetadataTarget, VectorPut, VectorPutBatchError,
};
use proxiima_core::verbs::query::SupersessionStatus;
use proxiima_core::{
    EmbeddableEntityRef, EmbeddingDim, EmbeddingSpace, EndpointUrlPolicy, EntityKind, GoalId,
    MemoryId, MemoryKind, OwnerId, PlaintextHosts, SpaceVector, StorageError,
    validate_endpoint_url_for_hosts,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::ProxiimaError;

/// Each index call's bound: searches wait on `nearest` and remote publications
/// wait on `put_batch`.
pub const VECTOR_INDEX_REQUEST_TIMEOUT: Duration =
    Duration::from_secs(proxiima_core::ledger!(Proxiima.Search.vectorRequestSeconds));

/// The largest body [`vector_index_router`] reads: a `put` of one memory's
/// chunks at the widest space fits many times over, and the store sends at
/// most 500 keys per call.
pub const VECTOR_INDEX_BODY_LIMIT: usize = proxiima_core::ledger!(Proxiima.Search.vectorBodyBytes);

/// The fewest characters a [`VectorIndexToken`] has. 32 random bytes in hex
/// are 64, so `openssl rand -hex 32` makes one.
pub const VECTOR_INDEX_TOKEN_MIN_LEN: usize =
    proxiima_core::ledger!(Proxiima.Search.vectorTokenMinChars);

/// The secret a [`VectorIndexClient`] and the [`vector_index_router`] serving
/// it share: the client sends it as a bearer token on every call, and the
/// router refuses any call without it.
#[derive(Clone)]
pub struct VectorIndexToken(String);

impl VectorIndexToken {
    /// `token`, when it has at least [`VECTOR_INDEX_TOKEN_MIN_LEN`]
    /// characters and each is printable ASCII other than a space, so it
    /// travels in a header as it is.
    ///
    /// # Errors
    ///
    /// [`ProxiimaError::Config`] naming the rule `token` breaks, never the
    /// token itself.
    pub fn new(token: String) -> Result<Self, ProxiimaError> {
        if !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(ProxiimaError::Config(
                "a vector index token is printable ASCII without spaces".into(),
            ));
        }
        if token.len() < VECTOR_INDEX_TOKEN_MIN_LEN {
            return Err(ProxiimaError::Config(format!(
                "a vector index token has at least {VECTOR_INDEX_TOKEN_MIN_LEN} characters"
            )));
        }
        Ok(Self(token))
    }
}

impl std::fmt::Debug for VectorIndexToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VectorIndexToken(***)")
    }
}

#[derive(Serialize, Deserialize)]
struct WireEntity {
    kind: EntityKind,
    id: Uuid,
}

#[derive(Serialize, Deserialize)]
struct WireKey {
    entity_id: Uuid,
    model_id: String,
    dim: usize,
}

#[derive(Serialize, Deserialize)]
struct WirePutItem {
    entity: WireEntity,
    owner: Uuid,
    version: i32,
    generation: i64,
    is_head: bool,
    model_id: String,
    dim: usize,
    vectors: Vec<Vec<f32>>,
}

#[derive(Serialize, Deserialize)]
struct PutBody {
    entries: Vec<WirePutItem>,
}

#[derive(Serialize, Deserialize)]
struct DeleteBody {
    keys: Vec<WireKey>,
    fence: i64,
}

#[derive(Serialize, Deserialize)]
struct WireMetadataTarget {
    #[serde(flatten)]
    key: WireKey,
    generation: i64,
}

#[derive(Serialize, Deserialize)]
struct OwnerBody {
    targets: Vec<WireMetadataTarget>,
    owner: Uuid,
    revision: i64,
}

#[derive(Serialize, Deserialize)]
struct HeadBody {
    targets: Vec<WireMetadataTarget>,
    is_head: bool,
    revision: i64,
}

#[derive(Serialize, Deserialize)]
struct NearestBody {
    owners: Vec<Uuid>,
    memories: Vec<Uuid>,
    supersession: SupersessionStatus,
    model_id: String,
    dim: usize,
    query: Vec<f32>,
    window: u32,
}

#[derive(Serialize, Deserialize)]
struct WireCandidate {
    memory_id: Uuid,
    score: f32,
    generation: i64,
}

#[derive(Serialize, Deserialize)]
struct NearestAnswer {
    candidates: Vec<WireCandidate>,
}

#[derive(Serialize, Deserialize)]
struct EntriesBody {
    after: Option<WireKey>,
    limit: u32,
}

/// An entry is its key's three fields beside its owner, version, generation, and head status.
#[derive(Serialize, Deserialize)]
struct WireEntry {
    #[serde(flatten)]
    key: WireKey,
    owner: Uuid,
    version: i32,
    generation: i64,
    is_head: bool,
}

#[derive(Serialize, Deserialize)]
struct EntriesAnswer {
    entries: Vec<WireEntry>,
    has_more: bool,
}

#[derive(Serialize, Deserialize)]
struct Done {}

#[derive(Serialize, Deserialize)]
struct WireError {
    error: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    all_unsent: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unsent_indices: Vec<usize>,
}

fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::ConstraintViolation(message.into())
}

fn space(model_id: String, dim: usize) -> Result<EmbeddingSpace, StorageError> {
    let dim = EmbeddingDim::from_width(dim)
        .ok_or_else(|| invalid(format!("width {dim} is not an index width")))?;
    Ok(EmbeddingSpace::new(model_id, dim))
}

fn space_vector(space: &EmbeddingSpace, values: Vec<f32>) -> Result<SpaceVector, StorageError> {
    SpaceVector::new(space.clone(), values).map_err(|error| invalid(error.to_string()))
}

fn key_to_wire(key: &VectorKey) -> WireKey {
    WireKey {
        entity_id: key.entity_id,
        model_id: key.space.model_id().to_owned(),
        dim: key.space.dim().width(),
    }
}

fn key_from_wire(key: WireKey) -> Result<VectorKey, StorageError> {
    Ok(VectorKey {
        entity_id: key.entity_id,
        space: space(key.model_id, key.dim)?,
    })
}

fn keys_to_wire(keys: &[VectorKey]) -> Vec<WireKey> {
    keys.iter().map(key_to_wire).collect()
}

fn keys_from_wire(keys: Vec<WireKey>) -> Result<Vec<VectorKey>, StorageError> {
    keys.into_iter().map(key_from_wire).collect()
}

fn metadata_targets_to_wire(
    targets: &[VectorMetadataTarget],
) -> Result<Vec<WireMetadataTarget>, StorageError> {
    targets
        .iter()
        .map(|target| {
            if target.generation <= 0 {
                return Err(invalid("metadata target generation must be positive"));
            }
            Ok(WireMetadataTarget {
                key: key_to_wire(&target.key),
                generation: target.generation,
            })
        })
        .collect()
}

fn metadata_targets_from_wire(
    targets: Vec<WireMetadataTarget>,
) -> Result<Vec<VectorMetadataTarget>, StorageError> {
    targets
        .into_iter()
        .map(|target| {
            if target.generation <= 0 {
                return Err(invalid("metadata target generation must be positive"));
            }
            Ok(VectorMetadataTarget {
                key: key_from_wire(target.key)?,
                generation: target.generation,
            })
        })
        .collect()
}

fn entity_from_wire(entity: &WireEntity) -> EmbeddableEntityRef {
    match MemoryKind::of(entity.kind) {
        Some(kind) => EmbeddableEntityRef::Memory {
            kind,
            memory_id: MemoryId::new(entity.id),
        },
        None => EmbeddableEntityRef::Goal(GoalId::new(entity.id)),
    }
}

/// A store's [`VectorIndex`] at a remote host, through the network form.
pub struct VectorIndexClient {
    client: reqwest::Client,
    base: reqwest::Url,
    token: VectorIndexToken,
}

impl std::fmt::Debug for VectorIndexClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorIndexClient")
            .field("origin", &self.base.origin().ascii_serialization())
            .finish_non_exhaustive()
    }
}

impl VectorIndexClient {
    /// The index of store `store` served at `base`: `https://`, or `http://`
    /// to loopback or a host named in `plaintext_hosts`, since every call
    /// carries memory vectors. Each call sends `token` as its bearer token,
    /// is bounded by [`VECTOR_INDEX_REQUEST_TIMEOUT`] and follows no
    /// redirect.
    ///
    /// # Errors
    ///
    /// [`ProxiimaError::Config`] for a URL that is not one of those.
    pub fn new(
        base: &str,
        store: u16,
        plaintext_hosts: &PlaintextHosts,
        token: VectorIndexToken,
    ) -> Result<Self, ProxiimaError> {
        let mut base = reqwest::Url::parse(base)
            .map_err(|err| ProxiimaError::Config(format!("vector index URL: {err}")))?;
        // `Proxiima.Transport.outboundTransport`.
        match base.scheme() {
            "https" => {}
            "http" => validate_endpoint_url_for_hosts(
                base.as_str(),
                EndpointUrlPolicy::AllowLoopbackHttp,
                plaintext_hosts,
            )
            .map_err(|_| {
                ProxiimaError::Config(
                    "the vector index URL must be https:// unless its host is loopback or a \
                     named plaintext host: every call carries memory vectors"
                        .into(),
                )
            })?,
            other => {
                return Err(ProxiimaError::Config(format!(
                    "the vector index URL must be http(s), got scheme {other:?}"
                )));
            }
        }
        if base.host_str().is_none() {
            return Err(ProxiimaError::Config(
                "the vector index URL has no host".into(),
            ));
        }
        let path = format!("{}/v1/stores/{store}/", base.path().trim_end_matches('/'));
        base.set_path(&path);
        let client = reqwest::Client::builder()
            .timeout(VECTOR_INDEX_REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| ProxiimaError::Config(format!("vector index client: {err}")))?;
        Ok(Self {
            client,
            base,
            token,
        })
    }

    async fn call<B: Serialize, A: for<'de> Deserialize<'de>>(
        &self,
        verb: &str,
        body: &B,
    ) -> Result<A, StorageError> {
        let bytes = serde_json::to_vec(body)
            .map_err(|err| StorageError::Internal(format!("vector index {verb}: {err}")))?;
        self.call_raw(verb, bytes).await
    }

    async fn call_raw<A: for<'de> Deserialize<'de>>(
        &self,
        verb: &str,
        body: Vec<u8>,
    ) -> Result<A, StorageError> {
        let url = self
            .base
            .join(verb)
            .map_err(|err| StorageError::Internal(format!("vector index URL: {err}")))?;
        let request = self
            .client
            .post(url)
            .bearer_auth(&self.token.0)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        let unavailable = |err: reqwest::Error| {
            StorageError::Unavailable(format!("vector index {verb}: {}", err.without_url()))
        };
        let response = request.send().await.map_err(unavailable)?;
        let status = response.status();
        if status.is_success() {
            return response.json().await.map_err(unavailable);
        }
        let reason = response
            .json::<WireError>()
            .await
            .map_or_else(|_| status.to_string(), |body| body.error);
        if status == StatusCode::BAD_REQUEST {
            return Err(invalid(format!("vector index {verb}: {reason}")));
        }
        Err(StorageError::Unavailable(format!(
            "vector index {verb}: {reason}"
        )))
    }

    async fn call_raw_put(
        &self,
        body: Vec<u8>,
        chunk_len: usize,
    ) -> Result<(), (StorageError, Vec<usize>)> {
        let url = self.base.join("put").map_err(|err| {
            (
                StorageError::Internal(format!("vector index URL: {err}")),
                Vec::new(),
            )
        })?;
        let request = self
            .client
            .post(url)
            .bearer_auth(&self.token.0)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        let unavailable = |err: reqwest::Error| {
            (
                StorageError::Unavailable(format!("vector index put: {}", err.without_url())),
                Vec::new(),
            )
        };
        let response = request.send().await.map_err(unavailable)?;
        let status = response.status();
        if status.is_success() {
            return response.json::<Done>().await.map(drop).map_err(unavailable);
        }
        let (reason, server_unsent) = response.json::<WireError>().await.map_or_else(
            |_| (status.to_string(), Vec::new()),
            |body| {
                // `Proxiima.Search.putUnsentReading`. Fail-safe: `all_unsent: true`
                // and explicit `unsent_indices` are mutually exclusive wire
                // encodings. Reject contradictory payloads as wholly uncertain so a
                // malformed response never clears write-ahead debt for an
                // uncertain write.
                if body.all_unsent {
                    return (
                        body.error,
                        if body.unsent_indices.is_empty() {
                            (0..chunk_len).collect()
                        } else {
                            Vec::new()
                        },
                    );
                }
                // Fail-safe: accept server-reported request-relative indices
                // only if every index is strictly within this request's slice.
                let valid = body.unsent_indices.iter().all(|&idx| idx < chunk_len);
                (
                    body.error,
                    if valid {
                        body.unsent_indices
                    } else {
                        Vec::new()
                    },
                )
            },
        );
        let err = if status == StatusCode::BAD_REQUEST {
            invalid(format!("vector index put: {reason}"))
        } else {
            StorageError::Unavailable(format!("vector index put: {reason}"))
        };
        Err((err, server_unsent))
    }
}

/// Fixed JSON wrapper framing around `PutBody`: `{"entries":[` + `]}`.
const PUT_BODY_PREFIX: &[u8] = b"{\"entries\":[";
const PUT_BODY_SUFFIX: &[u8] = b"]}";
const PUT_BODY_OVERHEAD: usize = PUT_BODY_PREFIX.len() + PUT_BODY_SUFFIX.len();

fn encode_put_item(entry: &VectorPut<'_>) -> Result<Vec<u8>, StorageError> {
    if entry.generation <= 0 {
        return Err(invalid("embedding generation must be positive"));
    }
    let first = entry
        .vectors
        .first()
        .ok_or_else(|| invalid("embedding must contain at least one chunk"))?;
    let space = first.space();
    if entry.vectors.iter().any(|vector| vector.space() != space) {
        return Err(invalid("embedding chunks must use one space"));
    }
    if entry
        .vectors
        .iter()
        .any(|vector| vector.values().iter().any(|value| !value.is_finite()))
    {
        return Err(invalid("embedding chunks must contain finite values"));
    }
    let item = WirePutItem {
        entity: WireEntity {
            kind: entry.entity.entity_kind(),
            id: entry.entity.entity_id(),
        },
        owner: entry.owner.into_inner(),
        version: entry.version,
        generation: entry.generation,
        is_head: entry.is_head,
        model_id: space.model_id().to_owned(),
        dim: space.dim().width(),
        vectors: entry.vectors.iter().map(|v| v.values().to_vec()).collect(),
    };
    let encoded = serde_json::to_vec(&item)
        .map_err(|err| StorageError::Internal(format!("vector index put: {err}")))?;
    if PUT_BODY_OVERHEAD.saturating_add(encoded.len()) > VECTOR_INDEX_BODY_LIMIT {
        return Err(invalid(format!(
            "embedding entry exceeds vector index HTTP body limit ({VECTOR_INDEX_BODY_LIMIT} bytes)"
        )));
    }
    Ok(encoded)
}

#[async_trait::async_trait]
impl VectorIndex for VectorIndexClient {
    /// Validate every entry's shape and exact encoded JSON size against
    /// [`VECTOR_INDEX_BODY_LIMIT`] before Phase 1 commits write-ahead delete
    /// intents.
    fn validate_put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), StorageError> {
        for entry in entries {
            let _ = encode_put_item(entry)?;
        }
        Ok(())
    }

    /// Send `entries` in one or more HTTP requests whose exact encoded JSON
    /// size stays within [`VECTOR_INDEX_BODY_LIMIT`]. Every entry is validated
    /// and sized before the first request is sent; if a chunk fails, entries in
    /// `0..end` remain fenced by the store's write-ahead delete intent while
    /// trailing unsent entries `end..entries.len()` are reported in
    /// [`VectorPutBatchError::unsent_indices`] so their untouched committed
    /// vectors and prior debt are preserved.
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        if entries.is_empty() {
            return Ok(());
        }
        let encoded_items: Vec<Vec<u8>> = entries
            .iter()
            .map(encode_put_item)
            .collect::<Result<_, _>>()
            .map_err(|err| VectorPutBatchError::with_unsent(err, (0..entries.len()).collect()))?;
        let mut index = 0;
        while index < encoded_items.len() {
            let mut end = index + 1;
            let mut body_len = PUT_BODY_OVERHEAD + encoded_items[index].len();
            while end < encoded_items.len()
                && body_len + 1 + encoded_items[end].len() <= VECTOR_INDEX_BODY_LIMIT
            {
                body_len += 1 + encoded_items[end].len();
                end += 1;
            }
            let mut body = Vec::with_capacity(body_len);
            body.extend_from_slice(PUT_BODY_PREFIX);
            for (offset, item) in encoded_items[index..end].iter().enumerate() {
                if offset > 0 {
                    body.push(b',');
                }
                body.extend_from_slice(item);
            }
            body.extend_from_slice(PUT_BODY_SUFFIX);
            if let Err((err, request_unsent)) = self.call_raw_put(body, end - index).await {
                let mut unsent: Vec<usize> = request_unsent
                    .into_iter()
                    .map(|rel| index + rel)
                    .chain(end..entries.len())
                    .collect();
                unsent.sort_unstable();
                unsent.dedup();
                return Err(VectorPutBatchError::with_unsent(err, unsent));
            }
            index = end;
        }
        Ok(())
    }

    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        if fence <= 0 {
            return Err(invalid("delete fence must be positive"));
        }
        let body = DeleteBody {
            keys: keys_to_wire(keys),
            fence,
        };
        self.call::<_, Done>("delete", &body).await.map(drop)
    }

    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        if revision <= 0 {
            return Err(invalid("metadata revision must be positive"));
        }
        let body = OwnerBody {
            targets: metadata_targets_to_wire(targets)?,
            owner: owner.into_inner(),
            revision,
        };
        self.call::<_, Done>("owner", &body).await.map(drop)
    }

    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        if revision <= 0 {
            return Err(invalid("metadata revision must be positive"));
        }
        let body = HeadBody {
            targets: metadata_targets_to_wire(targets)?,
            is_head,
            revision,
        };
        self.call::<_, Done>("head", &body).await.map(drop)
    }

    async fn nearest(
        &self,
        filter: &VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<VectorCandidatePage, StorageError> {
        let body = NearestBody {
            owners: filter
                .owners
                .iter()
                .copied()
                .map(OwnerId::into_inner)
                .collect(),
            memories: filter
                .memories
                .iter()
                .map(|memory| memory.into_inner())
                .collect(),
            supersession: filter.supersession,
            model_id: query.space().model_id().to_owned(),
            dim: query.space().dim().width(),
            query: query.values().to_vec(),
            window,
        };
        let answer: NearestAnswer = self.call("nearest", &body).await?;
        if answer.candidates.len() > usize::try_from(window).unwrap_or(usize::MAX) {
            return Err(StorageError::Unavailable(
                "vector index nearest answered past its window".into(),
            ));
        }
        let candidates = answer
            .candidates
            .into_iter()
            .map(|candidate| {
                if candidate.generation <= 0 {
                    return Err(StorageError::Unavailable(
                        "vector index nearest answered a non-positive generation".into(),
                    ));
                }
                Ok(VectorCandidate {
                    memory_id: MemoryId::new(candidate.memory_id),
                    similarity_score: candidate.score,
                    generation: candidate.generation,
                })
            })
            .collect::<Result<Vec<_>, StorageError>>()?;
        Ok(VectorCandidatePage::new(
            candidates,
            // `Proxiima.Search.httpCandidateBound`.
            CandidateBound::Unknown,
        ))
    }

    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        let body = EntriesBody {
            after: after.map(key_to_wire),
            limit,
        };
        let answer: EntriesAnswer = self.call("entries", &body).await?;
        let entries = answer
            .entries
            .into_iter()
            .map(|entry| {
                if entry.generation <= 0 {
                    return Err(StorageError::Unavailable(
                        "vector index entries answered a non-positive generation".into(),
                    ));
                }
                Ok(VectorEntry {
                    key: key_from_wire(entry.key)
                        .map_err(|error| StorageError::Unavailable(error.to_string()))?,
                    owner: OwnerId::new(entry.owner),
                    version: entry.version,
                    generation: entry.generation,
                    is_head: entry.is_head,
                })
            })
            .collect::<Result<Vec<_>, StorageError>>()?;
        // The sweep pages by the last key: an unordered answer would skip.
        let ordered = entries
            .windows(2)
            .all(|pair| pair[0].key.order() < pair[1].key.order())
            && after.is_none_or(|after| {
                entries
                    .first()
                    .is_none_or(|first| after.order() < first.key.order())
            });
        if !ordered || entries.len() > usize::try_from(limit).unwrap_or(usize::MAX) {
            return Err(StorageError::Unavailable(
                "vector index entries answered out of key order or past its limit".into(),
            ));
        }
        if limit > 0 && entries.is_empty() && answer.has_more {
            return Err(StorageError::Unavailable(
                "vector index entries claimed more pages without returning an entry".into(),
            ));
        }
        Ok(VectorEntryPage {
            entries,
            has_more: answer.has_more,
        })
    }
}

/// The indexes a [`vector_index_router`] serves, by store number.
type Indexes = Arc<BTreeMap<u16, Arc<dyn VectorIndex>>>;

/// The server half: serve each store's index in `indexes` on the wire the
/// [module documentation](self) describes, to callers that present `token`.
pub fn vector_index_router(
    indexes: BTreeMap<u16, Arc<dyn VectorIndex>>,
    token: &VectorIndexToken,
) -> axum::Router {
    axum::Router::new()
        // `Proxiima.Search.vectorOperations`.
        .route("/v1/stores/{store}/put", axum::routing::post(put))
        .route("/v1/stores/{store}/delete", axum::routing::post(delete))
        .route("/v1/stores/{store}/owner", axum::routing::post(set_owner))
        .route("/v1/stores/{store}/head", axum::routing::post(set_head))
        .route("/v1/stores/{store}/nearest", axum::routing::post(nearest))
        .route("/v1/stores/{store}/entries", axum::routing::post(entries))
        .layer(axum::extract::DefaultBodyLimit::max(
            VECTOR_INDEX_BODY_LIMIT,
        ))
        .layer(axum::middleware::from_fn_with_state(
            Sha256::digest(&token.0),
            authenticate,
        ))
        .with_state(Arc::new(indexes))
}

/// Authenticate the request before dispatch by comparing the token digests in constant time.
async fn authenticate(
    State(expected): State<sha2::digest::Output<Sha256>>,
    request: Request,
    next: Next,
) -> Response {
    if bearer(request.headers())
        .is_some_and(|presented| bool::from(Sha256::digest(presented).ct_eq(&expected)))
    {
        return next.run(request).await;
    }
    let mut response = refusal_all_unsent(
        StatusCode::UNAUTHORIZED,
        "a vector index call needs the index's bearer token".into(),
    );
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

/// The token of an `Authorization: Bearer <token>` header; the scheme is
/// case-insensitive.
fn bearer(headers: &HeaderMap) -> Option<&[u8]> {
    let (scheme, token) = headers
        .get(header::AUTHORIZATION)?
        .as_bytes()
        .split_at_checked(b"Bearer ".len())?;
    scheme.eq_ignore_ascii_case(b"Bearer ").then_some(token)
}

fn json_response<A: Serialize>(status: StatusCode, body: &A) -> Response {
    match serde_json::to_vec(body) {
        Ok(bytes) => (
            status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn refusal(status: StatusCode, error: String) -> Response {
    refusal_with_unsent(status, error, Vec::new())
}

fn refusal_all_unsent(status: StatusCode, error: String) -> Response {
    json_response(
        status,
        &WireError {
            error,
            all_unsent: true,
            unsent_indices: Vec::new(),
        },
    )
}

fn refusal_with_unsent(status: StatusCode, error: String, unsent_indices: Vec<usize>) -> Response {
    json_response(
        status,
        &WireError {
            error,
            all_unsent: false,
            unsent_indices,
        },
    )
}

trait WireResponse {
    const PRE_DISPATCH_ALL_UNSENT: bool = false;

    fn into_response(self, store: u16) -> Response;
}

impl<A: Serialize> WireResponse for Result<A, StorageError> {
    fn into_response(self, store: u16) -> Response {
        match self {
            Ok(answer) => json_response(StatusCode::OK, &answer),
            Err(StorageError::ConstraintViolation(error)) => {
                refusal(StatusCode::BAD_REQUEST, error)
            }
            Err(other) => {
                tracing::warn!(store, error = %other, "vector index call failed");
                refusal(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the index is unavailable".into(),
                )
            }
        }
    }
}

struct PutResponse(Response);

impl WireResponse for PutResponse {
    const PRE_DISPATCH_ALL_UNSENT: bool = true;

    fn into_response(self, _store: u16) -> Response {
        self.0
    }
}

/// One call: the store's index, the body parsed where it enters, the
/// index's answer or failure on the wire.
async fn serve<B, R, F, Fut>(indexes: &Indexes, store: u16, body: &Bytes, call: F) -> Response
where
    B: for<'de> Deserialize<'de>,
    R: WireResponse,
    F: FnOnce(Arc<dyn VectorIndex>, B) -> Fut,
    Fut: std::future::Future<Output = R>,
{
    // `Proxiima.Search.vectorErrorStatus`.
    let refusal = if R::PRE_DISPATCH_ALL_UNSENT {
        refusal_all_unsent
    } else {
        refusal
    };
    let Some(index) = indexes.get(&store).cloned() else {
        return refusal(StatusCode::NOT_FOUND, format!("no index for store {store}"));
    };
    let body = match serde_json::from_slice::<B>(body) {
        Ok(body) => body,
        Err(error) => return refusal(StatusCode::BAD_REQUEST, error.to_string()),
    };
    call(index, body).await.into_response(store)
}

async fn put(State(indexes): State<Indexes>, Path(store): Path<u16>, body: Bytes) -> Response {
    serve(&indexes, store, &body, |index, body: PutBody| async move {
        let parsed = match parse_wire_puts(body.entries) {
            Ok(parsed) => parsed,
            Err(StorageError::ConstraintViolation(error)) => {
                return PutResponse(refusal_all_unsent(StatusCode::BAD_REQUEST, error));
            }
            Err(other) => {
                tracing::warn!(store, error = %other, "vector index put validation failed");
                return PutResponse(refusal_all_unsent(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the index is unavailable".into(),
                ));
            }
        };
        let puts: Vec<VectorPut<'_>> = parsed
            .iter()
            .map(
                |(entity, owner, version, generation, is_head, vectors)| VectorPut {
                    entity: *entity,
                    owner: *owner,
                    version: *version,
                    generation: *generation,
                    is_head: *is_head,
                    vectors,
                },
            )
            .collect();
        let response = match index.put_batch(&puts).await {
            Ok(()) => json_response(StatusCode::OK, &Done {}),
            Err(batch_err) => {
                // `Proxiima.Search.putUnsentReading` without `all_unsent`.
                let mut unsent = if batch_err.unsent_indices.iter().all(|&idx| idx < puts.len()) {
                    batch_err.unsent_indices
                } else {
                    tracing::warn!(
                        store,
                        "vector index returned out-of-range unsent_indices; treating request as uncertain"
                    );
                    Vec::new()
                };
                unsent.sort_unstable();
                unsent.dedup();
                match batch_err.error {
                    StorageError::ConstraintViolation(error) => {
                        refusal_with_unsent(StatusCode::BAD_REQUEST, error, unsent)
                    }
                    other => {
                        tracing::warn!(store, error = %other, "vector index call failed");
                        refusal_with_unsent(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "the index is unavailable".into(),
                            unsent,
                        )
                    }
                }
            }
        };
        PutResponse(response)
    })
    .await
}

type ParsedWirePut = (
    EmbeddableEntityRef,
    OwnerId,
    i32,
    i64,
    bool,
    Vec<SpaceVector>,
);

fn parse_wire_puts(entries: Vec<WirePutItem>) -> Result<Vec<ParsedWirePut>, StorageError> {
    let mut parsed = Vec::with_capacity(entries.len());
    for item in entries {
        if item.generation <= 0 {
            return Err(invalid("embedding generation must be positive"));
        }
        if item.vectors.is_empty() {
            return Err(invalid("embedding must contain at least one chunk"));
        }
        let space = space(item.model_id, item.dim)?;
        let vectors = item
            .vectors
            .into_iter()
            .map(|values| space_vector(&space, values))
            .collect::<Result<Vec<_>, _>>()?;
        parsed.push((
            entity_from_wire(&item.entity),
            OwnerId::new(item.owner),
            item.version,
            item.generation,
            item.is_head,
            vectors,
        ));
    }
    Ok(parsed)
}

async fn delete(State(indexes): State<Indexes>, Path(store): Path<u16>, body: Bytes) -> Response {
    serve(
        &indexes,
        store,
        &body,
        |index, body: DeleteBody| async move {
            if body.fence <= 0 {
                return Err(invalid("delete fence must be positive"));
            }
            let keys = keys_from_wire(body.keys)?;
            index.delete(&keys, body.fence).await.map(|()| Done {})
        },
    )
    .await
}

async fn set_owner(
    State(indexes): State<Indexes>,
    Path(store): Path<u16>,
    body: Bytes,
) -> Response {
    serve(
        &indexes,
        store,
        &body,
        |index, body: OwnerBody| async move {
            if body.revision <= 0 {
                return Err(invalid("metadata revision must be positive"));
            }
            let targets = metadata_targets_from_wire(body.targets)?;
            index
                .set_owner(&targets, OwnerId::new(body.owner), body.revision)
                .await
                .map(|()| Done {})
        },
    )
    .await
}

async fn set_head(State(indexes): State<Indexes>, Path(store): Path<u16>, body: Bytes) -> Response {
    serve(&indexes, store, &body, |index, body: HeadBody| async move {
        if body.revision <= 0 {
            return Err(invalid("metadata revision must be positive"));
        }
        let targets = metadata_targets_from_wire(body.targets)?;
        index
            .set_head(&targets, body.is_head, body.revision)
            .await
            .map(|()| Done {})
    })
    .await
}

async fn nearest(State(indexes): State<Indexes>, Path(store): Path<u16>, body: Bytes) -> Response {
    serve(
        &indexes,
        store,
        &body,
        |index, body: NearestBody| async move {
            let query = space_vector(&space(body.model_id, body.dim)?, body.query)?;
            let filter = VectorCandidateFilter {
                owners: body.owners.into_iter().map(OwnerId::new).collect(),
                memories: body.memories.into_iter().map(MemoryId::new).collect(),
                supersession: body.supersession,
            };
            let candidates = index
                .nearest(&filter, &query, body.window)
                .await?
                .candidates;
            Ok(NearestAnswer {
                candidates: candidates
                    .into_iter()
                    .map(|candidate| WireCandidate {
                        memory_id: candidate.memory_id.into_inner(),
                        score: candidate.similarity_score,
                        generation: candidate.generation,
                    })
                    .collect(),
            })
        },
    )
    .await
}

async fn entries(State(indexes): State<Indexes>, Path(store): Path<u16>, body: Bytes) -> Response {
    serve(
        &indexes,
        store,
        &body,
        |index, body: EntriesBody| async move {
            let after = body.after.map(key_from_wire).transpose()?;
            let page = index.entries(after.as_ref(), body.limit).await?;
            Ok(EntriesAnswer {
                entries: page
                    .entries
                    .into_iter()
                    .map(|entry| WireEntry {
                        key: key_to_wire(&entry.key),
                        owner: entry.owner.into_inner(),
                        version: entry.version,
                        generation: entry.generation,
                        is_head: entry.is_head,
                    })
                    .collect(),
                has_more: page.has_more,
            })
        },
    )
    .await
}
