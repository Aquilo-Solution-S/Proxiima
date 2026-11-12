//! Read access to other stores, addressed by the number carried in each memory UUID.
//!
//! A host wires one [`OtherStores`] port through `RuntimeBuilder::other_stores`;
//! tools find it as [`OtherStoresService`] in the request's service bag.
//! Display names describe stores; routing and durable origins use their numbers.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use schemars::JsonSchema;
use serde::Serialize;
use uuid::Uuid;

use crate::verbs::query::{EntityKind, SearchMode, SearchOrder, SupersessionStatus, TagMatch};
use crate::{AuthzContext, EdgeKind, OwnerId, SchemaVersion, WriterAttribution};

/// How long a search waits for one other store before reporting it as
/// unanswered.
pub const DEFAULT_REMOTE_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum time one tool call may spend reading other-store origins.
pub const DEFAULT_REMOTE_BATCH_DEADLINE: Duration = Duration::from_secs(10);

/// Maximum number of origin reads in flight for one other store per call.
pub const MAX_REMOTE_READS_PER_STORE: usize = 8;

/// Maximum distinct foreign origins read for one lineage page; remaining
/// origins render unavailable with an explicit reason.
pub const MAX_LINEAGE_REMOTE_ORIGINS: usize = 64;

/// Reciprocal-rank-fusion constant used to merge store result lists.
pub const STORE_RRF_K: f32 = 60.0;

/// Who is asking: the store decides what this caller may see.
#[derive(Debug, Clone, Copy)]
pub struct StoreCaller<'a> {
    pub owner: OwnerId,
    pub authz: &'a AuthzContext,
}

/// One store the caller may read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreInfo {
    pub number: u16,
    pub name: String,
    pub description: String,
    /// Schema ids the store holds; a search filtered to a `schema_id`
    /// exactly one store declares goes to that store only.
    pub schema_ids: Vec<String>,
}

/// A checked inventory of remote stores for one local store.
///
/// Remote numbers are unique and exclude this store's number. Routing and
/// schema rendering share this inventory without choosing a local number again.
///
/// ```compile_fail
/// use proxiima_core::storage_ports::other_stores::StoreInventory;
/// StoreInventory { local_number: 0, remotes: Vec::new() };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreInventory {
    local_number: u16,
    remotes: Vec<StoreInfo>,
}

impl StoreInventory {
    /// Check an inventory where it enters from an other-stores port.
    ///
    /// # Errors
    ///
    /// Refuses a local number or a repeated remote number.
    pub fn new(local_number: u16, remotes: Vec<StoreInfo>) -> Result<Self, OtherStoresError> {
        let mut numbers = std::collections::HashSet::with_capacity(remotes.len());
        for info in &remotes {
            if info.number == local_number {
                return Err(OtherStoresError::LocalStoreInInventory(info.number));
            }
            if !numbers.insert(info.number) {
                return Err(OtherStoresError::DuplicateStoreInInventory(info.number));
            }
        }
        Ok(Self {
            local_number,
            remotes,
        })
    }

    pub(crate) const fn empty(local_number: u16) -> Self {
        Self {
            local_number,
            remotes: Vec::new(),
        }
    }

    #[must_use]
    pub const fn local_number(&self) -> u16 {
        self.local_number
    }

    #[must_use]
    pub fn remotes(&self) -> &[StoreInfo] {
        &self.remotes
    }
}

/// The search a remote store is asked to run.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteSearch {
    pub query: String,
    pub limit: u32,
    pub kind: Option<EntityKind>,
    pub schema_id: Option<String>,
    pub tags: Vec<String>,
    pub tag_match: TagMatch,
    /// Requested mode; each store resolves its own embedding availability.
    pub mode: SearchMode,
    pub since: Option<time::OffsetDateTime>,
    pub until: Option<time::OffsetDateTime>,
    pub order: SearchOrder,
    pub supersession: SupersessionStatus,
    pub min_score: Option<f32>,
    pub semantic_weight: Option<f32>,
}

/// One store's answer, including lexical fallback even when it found no hits.
///
/// The fallback fact is required for an empty answer too.
///
/// ```compile_fail,E0063
/// use proxiima_core::storage_ports::other_stores::RemoteSearchResult;
/// let _ = RemoteSearchResult { hits: Vec::new() };
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteSearchResult {
    pub hits: Vec<RemoteHit>,
    pub degraded_to_lexical: bool,
}

/// One remote search hit, in the requested order within its store.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteHit {
    pub handle: Uuid,
    /// The memory's owner in the issuing store, independent of its writer.
    pub owner: OwnerId,
    pub writer: WriterAttribution,
    pub kind: EntityKind,
    pub schema_id: String,
    pub snippet: String,
    pub created_at: time::OffsetDateTime,
    pub tags: Vec<String>,
    /// The object the hit cites, when it is a Fact that cites one.
    pub citation: Option<crate::verbs::query::FactCitationReadback>,
}

/// One remote memory, as read.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteMemory {
    /// The version this is: the requested one for an exact read, the series
    /// head for a head read.
    pub handle: Uuid,
    /// The series the version belongs to; the same on every version of it.
    pub series: Uuid,
    /// The memory's owner in the issuing store, independent of its writer.
    pub owner: OwnerId,
    pub writer: WriterAttribution,
    pub kind: EntityKind,
    pub schema_id: String,
    pub schema_version: SchemaVersion,
    /// The stored payload, independently of its display title and body.
    pub payload: serde_json::Value,
    pub title: Option<String>,
    pub body: String,
    pub created_at: time::OffsetDateTime,
    /// When the observation occurred: `Some` for every Fact, `None` for an
    /// Abstraction or Perspective. This is independent of the write time.
    pub occurred_at: Option<time::OffsetDateTime>,
    pub tags: Vec<String>,
}

impl RemoteMemory {
    /// A readable Fact of its own one-version series, for tests that need a
    /// valid value and name only the fields they care about.
    #[cfg(any(test, feature = "testkit"))]
    #[must_use]
    pub fn fixture(handle: Uuid) -> Self {
        Self {
            handle,
            series: handle,
            owner: OwnerId::new(Uuid::from_u128(2)),
            writer: WriterAttribution {
                by: Uuid::from_u128(3),
                via: None,
            },
            kind: EntityKind::Fact,
            schema_id: "test/fact-v1".into(),
            schema_version: SchemaVersion::new(1),
            payload: serde_json::json!({"observation": {"count": 7, "unit": "sample"}}),
            title: None,
            body: String::new(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            occurred_at: Some(time::OffsetDateTime::UNIX_EPOCH - time::Duration::days(1)),
            tags: Vec::new(),
        }
    }
}

/// A memory of another store that this caller read through the port: what
/// a derived memory may name in another store
/// (`DerivedMemory::external_origins`). Only [`OtherStoresService`] makes
/// one, so every such name was readable by its caller when the write was
/// prepared, and its layer is the one the store reported.
///
/// ```compile_fail
/// use proxiima_core::storage_ports::other_stores::ReadOrigin;
/// fn forge(handle: uuid::Uuid) -> ReadOrigin {
///     ReadOrigin { handle, kind: proxiima_core::verbs::query::EntityKind::Fact }
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOrigin {
    handle: Uuid,
    kind: EntityKind,
}

impl ReadOrigin {
    #[must_use]
    pub const fn handle(&self) -> Uuid {
        self.handle
    }

    /// The layer the store reported for it.
    #[must_use]
    pub const fn kind(&self) -> EntityKind {
        self.kind
    }

    /// A read this crate's unit tests stand in for, without a port.
    #[cfg(test)]
    pub(crate) const fn for_tests(handle: Uuid, kind: EntityKind) -> Self {
        Self { handle, kind }
    }
}

/// The outcome of reading one remote handle.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteRead {
    Found(Box<RemoteMemory>),
    /// Unknown to the store, or not readable by this caller; the two are
    /// deliberately indistinguishable, as for a local handle.
    NotFound,
    /// The memory existed and was erased.
    Erased,
}

/// A bounded read result that carries the authenticated origin witness.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteOriginRead {
    Found(ReadOrigin, Box<RemoteMemory>),
    NotFound,
    Erased,
}

/// The owning store's answer to a graph read about one of its memories,
/// admitted as the caller exactly as [`OtherStores::read`] admits the memory.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteGraphRead<T> {
    Found(T),
    /// Unknown to the store, or not readable by this caller; as
    /// [`RemoteRead::NotFound`].
    NotFound,
    /// The memory existed and was erased.
    Erased,
}

/// The target of an edge as the owning store projects it to the caller:
/// a target the caller may not read crosses only as its sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteEdgeTarget {
    Visible(Uuid),
    Redacted,
    Forgotten,
    Erased,
}

/// One edge touching a remote memory, as `core_get_memory`'s
/// `expand_neighbors` lists it in the owning store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteEdge {
    pub source: Uuid,
    pub target: RemoteEdgeTarget,
    pub kind: EdgeKind,
}

impl RemoteEdge {
    /// One hop from `memory`: either end is the memory itself.
    fn touches(&self, memory: Uuid) -> bool {
        self.source == memory || self.target == RemoteEdgeTarget::Visible(memory)
    }
}

/// One ancestors page the owning store is asked for, with the limits the
/// local walk applies. `cursor` is the owning store's own `next_cursor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAncestorsPage {
    pub depth: u8,
    pub limit: u32,
    pub cursor: Option<String>,
}

/// One ancestors page from a start in the owning store, as its
/// `core_walk_memory_lineage` answers the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteAncestors {
    pub nodes: Vec<RemoteLineageNode>,
    pub edges: Vec<RemoteLineageEdge>,
    /// The owning store's cursor for the next page; `None` on the last page.
    pub next_cursor: Option<String>,
}

/// `status` of a walk node that is not rendered from a read. The walk never
/// fails on such a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// The other store erased the memory.
    Erased,
    /// An other-store memory that could not be read; see
    /// [`UnavailableReason`].
    Unavailable,
    /// A local memory the caller could read but which is forgotten.
    Forgotten,
}

/// Why an `unavailable` other-store memory was not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, JsonSchema)]
pub enum UnavailableReason {
    /// The store answered that the caller cannot read it, or there is no
    /// such store.
    #[serde(rename = "not readable")]
    NotReadable,
    /// The store failed; retry later.
    #[serde(rename = "store unavailable")]
    StoreUnavailable,
    /// The call-wide deadline passed before the store answered.
    #[serde(rename = "deadline exceeded")]
    DeadlineExceeded,
    /// The page cap on other-store reads was reached.
    #[serde(rename = "origin limit exceeded")]
    OriginLimitExceeded,
}

/// One node of a remote ancestors page.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteLineageNode {
    pub handle: Uuid,
    /// The memory's owner; `None` on a node the store could not read.
    pub owner: Option<OwnerId>,
    pub kind: String,
    pub schema_id: String,
    pub snippet: String,
    pub distance: u8,
    pub status: Option<NodeStatus>,
    pub unavailable_reason: Option<UnavailableReason>,
}

/// One edge of a remote ancestors page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteLineageEdge {
    pub source: Uuid,
    pub target: RemoteEdgeTarget,
    pub kind: EdgeKind,
    pub distance: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OtherStoresError {
    #[error("unknown store '{0}'")]
    UnknownStore(u16),
    #[error("other-store inventory includes local store '{0}'")]
    LocalStoreInInventory(u16),
    #[error("other-store inventory repeats store '{0}'")]
    DuplicateStoreInInventory(u16),
    #[error("store returned memory {returned} for requested memory {requested}")]
    MismatchedMemory { requested: Uuid, returned: Uuid },
    #[error("store unavailable: {0}")]
    Unavailable(String),
    #[error("store cannot honor search: {0}")]
    UnsupportedSearch(String),
    #[error("origin batch deadline exceeded")]
    DeadlineExceeded,
}

/// Which version of a memory's series one read returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadAt {
    /// The version as written: the one the id names.
    Version,
    /// The newest version of the series the id names, as far as this caller
    /// may read it.
    SeriesHead,
}

/// Read access to memory stores beside this one.
pub trait OtherStores: Send + Sync {
    /// The stores `caller` may read.
    fn stores<'a>(
        &'a self,
        caller: StoreCaller<'a>,
    ) -> BoxFuture<'a, Result<Vec<StoreInfo>, OtherStoresError>>;

    /// Search one store as `caller`, in the requested order. Every supplied
    /// filter and score option must be honored or refused with
    /// [`OtherStoresError::UnsupportedSearch`]; never retry a weaker request.
    fn search<'a>(
        &'a self,
        caller: StoreCaller<'a>,
        store: u16,
        args: &'a RemoteSearch,
    ) -> BoxFuture<'a, Result<RemoteSearchResult, OtherStoresError>>;

    /// Read one memory of one store as `caller`: the version `id` names, or
    /// the head of its series. [`RemoteMemory::handle`] says which version
    /// came back, and [`RemoteMemory::series`] its series.
    fn read<'a>(
        &'a self,
        caller: StoreCaller<'a>,
        id: Uuid,
        at: ReadAt,
    ) -> BoxFuture<'a, Result<RemoteRead, OtherStoresError>>;

    /// The one-hop edges of the memory `id` names, in both directions, as
    /// its store admits them to `caller`: an edge the caller may not read
    /// never crosses. A port that cannot serve the read refuses it as
    /// [`OtherStoresError::Unavailable`], never with an empty list.
    fn neighbor_edges<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        _id: Uuid,
    ) -> BoxFuture<'a, Result<RemoteGraphRead<Vec<RemoteEdge>>, OtherStoresError>> {
        Box::pin(async {
            Err(OtherStoresError::Unavailable(
                "neighbor edges not served".into(),
            ))
        })
    }

    /// One ancestors page of the lineage walk from the memory `id` names, as
    /// its store admits it to `caller`. A port that cannot serve the read
    /// refuses it as [`OtherStoresError::Unavailable`], never with an empty
    /// page.
    fn ancestors<'a>(
        &'a self,
        _caller: StoreCaller<'a>,
        _id: Uuid,
        _page: &'a RemoteAncestorsPage,
    ) -> BoxFuture<'a, Result<RemoteGraphRead<RemoteAncestors>, OtherStoresError>> {
        Box::pin(async { Err(OtherStoresError::Unavailable("ancestors not served".into())) })
    }
}

/// The wired port plus its per-store time bound, as tools find it in the
/// service bag.
#[derive(Clone)]
pub struct OtherStoresService {
    port: Arc<dyn OtherStores>,
    timeout: Duration,
}

impl fmt::Debug for OtherStoresService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtherStoresService")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl OtherStoresService {
    #[must_use]
    pub fn new(port: Arc<dyn OtherStores>) -> Self {
        Self {
            port,
            timeout: DEFAULT_REMOTE_TIMEOUT,
        }
    }

    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn port(&self) -> &dyn OtherStores {
        self.port.as_ref()
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Read and check the remote-store inventory under the time bound.
    /// `None` means the list did not answer: it failed or timed out, which
    /// says nothing about which stores exist. An answered inventory is
    /// validated before either routing or schema rendering can use it.
    ///
    /// # Errors
    ///
    /// Refuses an answered inventory containing the local number or duplicates.
    pub async fn stores_bounded(
        &self,
        caller: StoreCaller<'_>,
        local_number: u16,
    ) -> Result<Option<StoreInventory>, OtherStoresError> {
        let Ok(Ok(stores)) = tokio::time::timeout(self.timeout, self.port.stores(caller)).await
        else {
            return Ok(None);
        };
        StoreInventory::new(local_number, stores).map(Some)
    }

    /// Read one handle under the time bound; a timeout is
    /// [`OtherStoresError::Unavailable`]. An exact read must answer with the
    /// version asked for; a head read with a version of the asked store.
    ///
    /// # Errors
    ///
    /// Returns the port's error, or `Unavailable` past the bound.
    pub async fn read_bounded(
        &self,
        caller: StoreCaller<'_>,
        id: Uuid,
        at: ReadAt,
    ) -> Result<RemoteRead, OtherStoresError> {
        let read = tokio::time::timeout(self.timeout, self.port.read(caller, id, at))
            .await
            .unwrap_or_else(|_| Err(OtherStoresError::Unavailable("timed out".into())))?;
        if let RemoteRead::Found(memory) = &read {
            let answers_the_question = match at {
                ReadAt::Version => memory.handle == id,
                ReadAt::SeriesHead => {
                    crate::memory_store_number(memory.handle) == crate::memory_store_number(id)
                }
            };
            if !answers_the_question {
                return Err(OtherStoresError::MismatchedMemory {
                    requested: id,
                    returned: memory.handle,
                });
            }
        }
        Ok(read)
    }

    /// The one-hop edges of a remote memory under the time bound. An edge
    /// that does not touch `id` is refused as an answer for another memory.
    ///
    /// # Errors
    ///
    /// Returns the port's error, `Unavailable` past the bound, or
    /// `MismatchedMemory` for an edge of another memory.
    pub async fn neighbor_edges_bounded(
        &self,
        caller: StoreCaller<'_>,
        id: Uuid,
    ) -> Result<RemoteGraphRead<Vec<RemoteEdge>>, OtherStoresError> {
        let read = tokio::time::timeout(self.timeout, self.port.neighbor_edges(caller, id))
            .await
            .unwrap_or_else(|_| Err(OtherStoresError::Unavailable("timed out".into())))?;
        if let RemoteGraphRead::Found(edges) = &read
            && let Some(stray) = edges.iter().find(|edge| !edge.touches(id))
        {
            return Err(OtherStoresError::MismatchedMemory {
                requested: id,
                returned: stray.source,
            });
        }
        Ok(read)
    }

    /// One remote ancestors page under the time bound. The page must hold
    /// its start as the node at distance 0.
    ///
    /// # Errors
    ///
    /// Returns the port's error, `Unavailable` past the bound, or
    /// `MismatchedMemory` for a page of another start.
    pub async fn ancestors_bounded(
        &self,
        caller: StoreCaller<'_>,
        id: Uuid,
        page: &RemoteAncestorsPage,
    ) -> Result<RemoteGraphRead<RemoteAncestors>, OtherStoresError> {
        let read = tokio::time::timeout(self.timeout, self.port.ancestors(caller, id, page))
            .await
            .unwrap_or_else(|_| Err(OtherStoresError::Unavailable("timed out".into())))?;
        if let RemoteGraphRead::Found(ancestors) = &read {
            let start = ancestors.nodes.iter().find(|node| node.distance == 0);
            if start.is_none_or(|start| start.handle != id) {
                return Err(OtherStoresError::MismatchedMemory {
                    requested: id,
                    returned: start.map_or(Uuid::nil(), |start| start.handle),
                });
            }
        }
        Ok(read)
    }

    /// [`Self::read_bounded`], and for a memory the caller may read, the
    /// [`ReadOrigin`] that lets a derived memory name it. `None` for a handle
    /// the store does not know, will not show this caller, or erased.
    ///
    /// # Errors
    ///
    /// As [`Self::read_bounded`].
    pub async fn read_origin(
        &self,
        caller: StoreCaller<'_>,
        id: Uuid,
    ) -> Result<Option<(ReadOrigin, RemoteMemory)>, OtherStoresError> {
        Ok(
            match self.read_bounded(caller, id, ReadAt::Version).await? {
                RemoteRead::Found(memory) => Some((
                    ReadOrigin {
                        handle: id,
                        kind: memory.kind,
                    },
                    *memory,
                )),
                RemoteRead::NotFound | RemoteRead::Erased => None,
            },
        )
    }

    /// Read origins under one call-wide deadline and construct witnesses only
    /// for memories returned and UUID-checked by this service.
    pub async fn read_origins_bounded(
        &self,
        caller: StoreCaller<'_>,
        handles: &[Uuid],
    ) -> Vec<Result<RemoteOriginRead, OtherStoresError>> {
        self.read_many_bounded(caller, handles)
            .await
            .into_iter()
            .zip(handles)
            .map(|(result, handle)| {
                result.map(|read| match read {
                    RemoteRead::Found(memory) => RemoteOriginRead::Found(
                        ReadOrigin {
                            handle: *handle,
                            kind: memory.kind,
                        },
                        memory,
                    ),
                    RemoteRead::NotFound => RemoteOriginRead::NotFound,
                    RemoteRead::Erased => RemoteOriginRead::Erased,
                })
            })
            .collect()
    }

    /// Read a batch of origins under one call-wide deadline. Reads for each
    /// store are limited to [`MAX_REMOTE_READS_PER_STORE`] in flight; each
    /// read retains [`Self::timeout`] as its individual bound.
    ///
    /// # Errors
    ///
    /// Returns [`OtherStoresError::DeadlineExceeded`] for reads that cannot
    /// start or finish before the call-wide deadline. Individual port errors
    /// remain attached to their corresponding result.
    pub async fn read_many_bounded(
        &self,
        caller: StoreCaller<'_>,
        handles: &[Uuid],
    ) -> Vec<Result<RemoteRead, OtherStoresError>> {
        let mut by_store: std::collections::BTreeMap<u16, Vec<(usize, Uuid)>> =
            std::collections::BTreeMap::new();
        for (index, handle) in handles.iter().copied().enumerate() {
            by_store
                .entry(crate::memory_store_number(handle))
                .or_default()
                .push((index, handle));
        }

        let deadline = tokio::time::Instant::now() + DEFAULT_REMOTE_BATCH_DEADLINE;
        let groups = by_store.into_values().map(|entries| async move {
            stream::iter(entries)
                .map(|(index, handle)| async move {
                    let result = if tokio::time::Instant::now() >= deadline {
                        Err(OtherStoresError::DeadlineExceeded)
                    } else {
                        tokio::time::timeout_at(
                            deadline,
                            self.read_bounded(caller, handle, ReadAt::Version),
                        )
                        .await
                        .unwrap_or(Err(OtherStoresError::DeadlineExceeded))
                    };
                    (index, result)
                })
                .buffer_unordered(MAX_REMOTE_READS_PER_STORE)
                .collect::<Vec<_>>()
                .await
        });
        let mut results = stream::iter(groups)
            .buffer_unordered(usize::MAX)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        results.sort_by_key(|(index, _)| *index);
        results.into_iter().map(|(_, result)| result).collect()
    }
}

/// Which stores one search goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRoute {
    pub local: bool,
    pub remotes: Vec<u16>,
}

/// Pick the stores of one search.
///
/// - `requested` numbers stores explicitly; each must
///   be one the caller may read.
/// - Otherwise a `schema_id` declared by exactly one store — this one
///   counting as a store, by `local_declares` — routes there only.
/// - Otherwise this store and every readable store.
///
/// # Errors
///
/// Returns a message naming the first requested store that is not
/// readable, or an empty request.
pub fn route_stores(
    requested: Option<&[u16]>,
    schema_id: Option<&str>,
    local_declares: impl Fn(&str) -> bool,
    inventory: &StoreInventory,
) -> Result<StoreRoute, String> {
    let local_number = inventory.local_number;
    let readable = &inventory.remotes;
    if let Some(requested) = requested {
        if requested.is_empty() {
            return Err("stores must name at least one store when given".into());
        }
        let mut route = StoreRoute {
            local: false,
            remotes: Vec::new(),
        };
        for number in requested {
            if *number == local_number {
                route.local = true;
                continue;
            }
            let info = readable
                .iter()
                .find(|info| info.number == *number)
                .ok_or_else(|| format!("unknown store '{number}'"))?;
            if !route.remotes.contains(&info.number) {
                route.remotes.push(info.number);
            }
        }
        return Ok(route);
    }
    if let Some(schema_id) = schema_id {
        let mut declaring = readable
            .iter()
            .filter(|info| info.schema_ids.iter().any(|id| id == schema_id));
        match (
            local_declares(schema_id),
            declaring.next(),
            declaring.next(),
        ) {
            (true, None, _) => {
                return Ok(StoreRoute {
                    local: true,
                    remotes: Vec::new(),
                });
            }
            (false, Some(only), None) => {
                return Ok(StoreRoute {
                    local: false,
                    remotes: vec![only.number],
                });
            }
            _ => {}
        }
    }
    Ok(StoreRoute {
        local: true,
        remotes: readable.iter().map(|info| info.number).collect(),
    })
}

/// Reciprocal-rank fusion of ranked lists, `k` = [`STORE_RRF_K`]. Items are
/// never deduplicated across lists: two stores never share a memory. Ties
/// keep list order, then rank.
#[must_use]
pub fn rrf_merge<T>(lists: Vec<Vec<T>>) -> Vec<(T, f32)> {
    let mut scored: Vec<(usize, usize, T, f32)> = Vec::new();
    for (list_index, list) in lists.into_iter().enumerate() {
        for (rank, item) in list.into_iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let score = 1.0 / (STORE_RRF_K + rank as f32 + 1.0);
            scored.push((list_index, rank, item, score));
        }
    }
    scored.sort_by(|a, b| b.3.total_cmp(&a.3).then(a.1.cmp(&b.1)).then(a.0.cmp(&b.0)));
    scored
        .into_iter()
        .map(|(_, _, item, score)| (item, score))
        .collect()
}

#[cfg(test)]
#[path = "other_stores_tests.rs"]
mod tests;
