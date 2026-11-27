//! A store's chunk vectors behind a port (#519).
//!
//! The store keeps each entity's head, its current embedding version per
//! space, beside its rows; the index keeps the chunk vectors. The index
//! narrows candidates and the store authorizes their use: a
//! [`VectorCandidate`] is read by id under the store's owner rules before its
//! id or score is shown. A wrong or stale entry costs results, never a row.
//!
//! Without a host index the store keeps its vectors in its own database,
//! inside the transaction that changes their rows. A host index is called by
//! the store with monotonic, never-reused generations: each remote publication
//! commits durable delete intent stamped with a fresh generation before calling
//! [`VectorIndex::put_batch`], then commits `embedding_heads` only if that
//! intent still owns the key. Every [`VectorIndex::delete`] carries a monotonic
//! `fence` so providers reject late in-flight writes after erase or retry, and
//! remote search verifies each candidate's `generation` against the committed
//! `embedding_heads` row before returning its score.

use std::collections::BTreeMap;
use std::sync::Mutex;

use uuid::Uuid;

use crate::verbs::query::SupersessionStatus;
use crate::{EmbeddableEntityRef, EmbeddingSpace, MemoryId, OwnerId, SpaceVector, StorageError};

/// An index hit awaiting the store's owner-RLS read by id.
///
/// Neither the id nor its score may be exposed until that read admits it and,
/// for a host index, confirms `generation` matches the committed head.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VectorCandidate {
    pub memory_id: MemoryId,
    /// Best-chunk cosine, bounded to `0..=1`.
    pub similarity_score: f32,
    /// Monotonic publication generation of the vectors that produced
    /// `similarity_score`. Host indexes report a positive generation verified
    /// against `embedding_heads.vector_generation` at admission; local
    /// `pgvector` scans join `embedding_heads` in-transaction and set `0`.
    pub generation: i64,
}

/// A source's completeness claim for this scan, not proof of index freshness
/// or all-corpus retrieval quality.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CandidateBound {
    /// The source exhausted the supplied candidate predicate.
    Complete,
    /// At least one source reached a known window or hard cap. Other sources
    /// may also be incomplete; this makes no exhaustion claim about them.
    Bounded,
    /// The source cannot state whether its result is complete.
    #[default]
    Unknown,
}

impl CandidateBound {
    /// Combine bounds from independent candidate legs without losing a known cap.
    #[must_use]
    pub fn combine(self, other: Self) -> Self {
        // `Proxiima.Search.combineBounds`.
        match (self, other) {
            (Self::Bounded, Self::Complete | Self::Bounded | Self::Unknown)
            | (Self::Complete | Self::Unknown, Self::Bounded) => Self::Bounded,
            (Self::Unknown, Self::Complete | Self::Unknown) | (Self::Complete, Self::Unknown) => {
                Self::Unknown
            }
            (Self::Complete, Self::Complete) => Self::Complete,
        }
    }
}

#[cfg(test)]
mod candidate_bound_tests {
    use super::CandidateBound;

    #[test]
    fn bounds_combine_without_losing_a_known_cap() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/ledger_vectors.json")).unwrap();
        let rows = vectors["Proxiima.Search.combineBounds"].as_array().unwrap();
        let bound = |value: &serde_json::Value| match value.as_str() {
            Some("complete") => CandidateBound::Complete,
            Some("bounded") => CandidateBound::Bounded,
            Some("unknown") => CandidateBound::Unknown,
            _ => panic!("invalid candidate bound: {value}"),
        };
        for row in rows {
            assert_eq!(
                bound(&row["in"][0]).combine(bound(&row["in"][1])),
                bound(&row["out"]),
                "{row}",
            );
        }
    }
}

/// Candidates and the source's completeness claim.
///
/// Providers must supply that claim explicitly:
/// ```
/// use proxiima_core::storage_ports::{CandidateBound, VectorCandidatePage};
/// let _ = VectorCandidatePage::new(Vec::new(), CandidateBound::Unknown);
/// ```
/// ```compile_fail
/// use proxiima_core::storage_ports::VectorCandidatePage;
/// let _ = VectorCandidatePage::new(Vec::new());
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct VectorCandidatePage {
    pub candidates: Vec<VectorCandidate>,
    pub bound: CandidateBound,
}

impl VectorCandidatePage {
    #[must_use]
    pub fn new(candidates: Vec<VectorCandidate>, bound: CandidateBound) -> Self {
        Self { candidates, bound }
    }
}

/// The candidates an external index may rank before the store authorizes each
/// result through its own row-level policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorCandidateFilter {
    pub owners: Vec<OwnerId>,
    pub memories: Vec<MemoryId>,
    /// Whether to rank only series heads (`is_head = true`) or include
    /// superseded versions.
    pub supersession: SupersessionStatus,
}

impl Default for VectorCandidateFilter {
    fn default() -> Self {
        Self {
            owners: Vec::new(),
            memories: Vec::new(),
            supersession: SupersessionStatus::HeadsOnly,
        }
    }
}

/// One entity's vectors in one space: what the index holds one version of.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VectorKey {
    pub entity_id: Uuid,
    pub space: EmbeddingSpace,
}

impl VectorKey {
    /// The order [`VectorIndex::entries`] pages in: entity id, model id,
    /// width.
    #[must_use]
    // `Proxiima.Search.vectorEntryKeys`.
    pub fn order(&self) -> (Uuid, &str, usize) {
        (
            self.entity_id,
            self.space.model_id(),
            self.space.dim().width(),
        )
    }
}

/// One key paired with the committed publication `generation` targeted by a
/// conditional metadata update ([`VectorIndex::set_owner`] or
/// [`VectorIndex::set_head`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorMetadataTarget {
    pub key: VectorKey,
    /// The exact positive publication generation that the metadata update
    /// applies to. Providers must ignore the update if the stored entry's
    /// generation differs.
    pub generation: i64,
}

/// One entity's chunks to write to a [`VectorIndex`].
#[derive(Debug, Clone, Copy)]
pub struct VectorPut<'a> {
    pub entity: EmbeddableEntityRef,
    pub owner: OwnerId,
    pub version: i32,
    /// Strictly monotonic, never-reused positive publication generation.
    /// Providers must ignore a put whose `generation` is less than or equal to
    /// the key's current stored generation or delete fence.
    pub generation: i64,
    /// Whether `entity` is currently its series' head. Always `true` for Goals
    /// (which are never search candidates).
    pub is_head: bool,
    pub vectors: &'a [SpaceVector],
}

/// What the index holds for one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorEntry {
    pub key: VectorKey,
    pub owner: OwnerId,
    pub version: i32,
    pub generation: i64,
    pub is_head: bool,
}

/// One ordered page of [`VectorEntry`] items from [`VectorIndex::entries`] and
/// whether more entries remain after the page's last key.
///
/// Carrying exhaustion explicitly lets a provider return fewer than `limit`
/// entries without tricking the sweep into treating a short prefix as the end
/// of the index.
///
/// ```
/// use proxiima_core::storage_ports::VectorEntryPage;
/// let _ = VectorEntryPage::new(Vec::new(), false);
/// ```
/// ```compile_fail
/// use proxiima_core::storage_ports::VectorEntryPage;
/// let _ = VectorEntryPage::new(Vec::new());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorEntryPage {
    pub entries: Vec<VectorEntry>,
    pub has_more: bool,
}

impl VectorEntryPage {
    #[must_use]
    pub fn new(entries: Vec<VectorEntry>, has_more: bool) -> Self {
        Self { entries, has_more }
    }
}

/// What a host vector-index sweep slice repaired and where to resume it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VectorIndexSweepOutcome {
    /// Entries the index held for no head.
    pub entries_deleted: u64,
    /// Heads the index held no matching version for: their vectors were lost,
    /// so reconciliation embeds them again.
    pub heads_deleted: u64,
    /// Entries whose version matched the head while their owner or series-head
    /// status drifted, repaired in place without re-embedding.
    pub metadata_repaired: u64,
    /// Last compared key when a bounded slice stopped before the end of the
    /// store; pass as `after` to resume. `None` means the sweep reached the end.
    pub next: Option<VectorKey>,
}

/// Failure from [`VectorIndex::put_batch`], separating entries whose delivery
/// is uncertain from entries the provider knows it never dispatched.
///
/// Converting a plain [`StorageError`] via [`From`] leaves `unsent_indices`
/// empty so every staged entry defaults to fail-safe fenced cleanup.
#[derive(Debug)]
pub struct VectorPutBatchError {
    /// The underlying storage or transport error.
    pub error: StorageError,
    /// Indices into the `put_batch` slice that the provider knows it never
    /// dispatched. The store restores those keys' prior debt state instead of
    /// scheduling fenced deletes against untouched committed vectors; all other
    /// indices retain fenced cleanup.
    pub unsent_indices: Vec<usize>,
}

impl VectorPutBatchError {
    /// Fail with uncertain delivery for every entry in the batch (fail-safe).
    #[must_use]
    pub fn all_uncertain(error: StorageError) -> Self {
        Self {
            error,
            unsent_indices: Vec::new(),
        }
    }

    /// Fail while naming exact `put_batch` slice indices that were never
    /// dispatched to the provider.
    #[must_use]
    pub fn with_unsent(error: StorageError, unsent_indices: Vec<usize>) -> Self {
        Self {
            error,
            unsent_indices,
        }
    }

    /// Extract the underlying [`StorageError`].
    #[must_use]
    pub fn into_storage_error(self) -> StorageError {
        self.error
    }
}

impl std::fmt::Display for VectorPutBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for VectorPutBatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl From<StorageError> for VectorPutBatchError {
    fn from(error: StorageError) -> Self {
        Self::all_uncertain(error)
    }
}

/// The store's vector index when its vectors live outside its database.
///
/// Every method is idempotent, so the store may repeat any call after a
/// failure: a key the index does not hold is no error.
#[async_trait::async_trait]
pub trait VectorIndex: Send + Sync {
    /// Synchronously validate every entry in `entries` before the store commits
    /// write-ahead delete intents or dispatches any network call.
    ///
    /// Providers with transport-level limits (such as the HTTP client's body
    /// size cap) override this to reject invalid or oversized entries with
    /// [`StorageError::ConstraintViolation`] before Phase 1 commits
    /// `vector_index_debt`, so a rejected batch can never cause the debt drain
    /// to delete untouched committed vectors.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::ConstraintViolation`] when any entry violates
    /// the provider's contract or payload limits.
    fn validate_put_batch(&self, _entries: &[VectorPut<'_>]) -> Result<(), StorageError> {
        Ok(())
    }

    /// Store every entry in `entries`, replacing each entity's earlier version
    /// in its space whenever `entry.generation` exceeds both the key's stored
    /// generation and its delete fence (`Proxiima.Search.putLands`). Entries with a stale or fenced
    /// generation are ignored as idempotent no-ops; a non-positive generation
    /// is a [`StorageError::ConstraintViolation`]. Each entry's `vectors` is
    /// nonempty, in text order, and shares one space. An empty slice is a no-op.
    ///
    /// On failure, providers that split `entries` across multiple requests
    /// should return [`VectorPutBatchError::with_unsent`] for indices that were
    /// never dispatched so the store preserves their prior debt state rather
    /// than deleting untouched committed vectors; any index omitted from
    /// `unsent_indices` receives fenced delete cleanup.
    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError>;

    /// One write of [`Self::put_batch`].
    async fn put(
        &self,
        entity: EmbeddableEntityRef,
        owner: OwnerId,
        version: i32,
        generation: i64,
        is_head: bool,
        vectors: &[SpaceVector],
    ) -> Result<(), StorageError> {
        self.put_batch(&[VectorPut {
            entity,
            owner,
            version,
            generation,
            is_head,
            vectors,
        }])
        .await
        .map_err(VectorPutBatchError::into_storage_error)
    }

    /// Remove every chunk of each key whose generation is at most `fence`, and
    /// advance each key's provider fence to at least `fence` so any late
    /// in-flight `put_batch` with `generation <= fence` is ignored
    /// (`Proxiima.Search.deleteRemoves`). A non-positive `fence` is a
    /// [`StorageError::ConstraintViolation`].
    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError>;

    /// Record `owner` as the owner of each target whose stored entry matches
    /// `target.generation` and whose stored owner revision is strictly below
    /// `revision`, advancing the entry's owner revision to `revision`
    /// (`Proxiima.Search.metadataLands`). Stale
    /// or mismatched targets are ignored as idempotent no-ops; a non-positive
    /// `revision` or `target.generation` is a
    /// [`StorageError::ConstraintViolation`].
    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError>;

    /// Record whether each target is currently its series' head whenever its
    /// stored entry matches `target.generation` and its stored head revision is
    /// strictly below `revision`, advancing the entry's head revision to
    /// `revision` (`Proxiima.Search.metadataLands`, independent of the owner
    /// revision). Stale or mismatched targets are ignored as idempotent
    /// no-ops; a non-positive `revision` or `target.generation` is a
    /// [`StorageError::ConstraintViolation`].
    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError>;

    /// Up to `window` distinct memories in `query`'s space, narrowed to
    /// memories owned by `filter.owners` or named by `filter.memories` and to
    /// series heads when `filter.supersession` is [`SupersessionStatus::HeadsOnly`].
    /// Rank by each memory's best chunk, descending by score, then by memory id
    /// descending. Each returned candidate carries the stored `generation`. A
    /// goal is never a candidate. An empty filter or a zero window produce no
    /// candidates.
    async fn nearest(
        &self,
        filter: &VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<VectorCandidatePage, StorageError>;

    /// Up to `limit` entries strictly after `after`, in [`VectorKey::order`],
    /// together with an explicit `has_more` flag: what the orphan sweep
    /// compares with the store's heads. When `has_more` is `true`, `entries`
    /// must be nonempty (unless `limit == 0`).
    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError>;
}

/// A [`VectorIndex`] in process memory: the reference implementation, and
/// what a test reaches through the network form.
#[derive(Debug, Default)]
pub struct InMemoryVectorIndex {
    state: Mutex<InMemoryState>,
}

#[derive(Debug, Default)]
struct InMemoryState {
    entries: BTreeMap<(Uuid, String, usize), Stored>,
    fences: BTreeMap<(Uuid, String, usize), i64>,
}

#[derive(Debug, Clone)]
struct Stored {
    memory: bool,
    space: EmbeddingSpace,
    owner: OwnerId,
    version: i32,
    generation: i64,
    owner_revision: i64,
    head_revision: i64,
    is_head: bool,
    chunks: Vec<Vec<f32>>,
}

impl InMemoryVectorIndex {
    fn state(&self) -> std::sync::MutexGuard<'_, InMemoryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn map_key(key: &VectorKey) -> (Uuid, String, usize) {
    let (entity, model, width) = key.order();
    (entity, model.to_owned(), width)
}

/// Cosine similarity in `0..=1`; a zero vector scores zero, as the
/// database's `NaN` distance does.
fn similarity(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut aa, mut bb) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        aa += f64::from(*x) * f64::from(*x);
        bb += f64::from(*y) * f64::from(*y);
    }
    let cosine = dot / (aa.sqrt() * bb.sqrt());
    if cosine.is_nan() {
        return 0.0;
    }
    #[allow(clippy::cast_possible_truncation)]
    let score = cosine.clamp(0.0, 1.0) as f32;
    score
}

fn validate_put_entry(entry: &VectorPut<'_>) -> Result<EmbeddingSpace, StorageError> {
    if entry.generation <= 0 {
        return Err(StorageError::ConstraintViolation(
            "embedding generation must be positive".into(),
        ));
    }
    let Some(first) = entry.vectors.first() else {
        return Err(StorageError::ConstraintViolation(
            "embedding must contain at least one chunk".into(),
        ));
    };
    let space = first.space().clone();
    if entry.vectors.iter().any(|vector| vector.space() != &space) {
        return Err(StorageError::ConstraintViolation(
            "embedding chunks must use one space".into(),
        ));
    }
    Ok(space)
}

#[async_trait::async_trait]
impl VectorIndex for InMemoryVectorIndex {
    fn validate_put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), StorageError> {
        for entry in entries {
            let _ = validate_put_entry(entry)?;
        }
        Ok(())
    }

    async fn put_batch(&self, entries: &[VectorPut<'_>]) -> Result<(), VectorPutBatchError> {
        let mut prepared = Vec::with_capacity(entries.len());
        for entry in entries {
            let space = validate_put_entry(entry).map_err(|err| {
                VectorPutBatchError::with_unsent(err, (0..entries.len()).collect())
            })?;
            let key = VectorKey {
                entity_id: entry.entity.entity_id(),
                space: space.clone(),
            };
            prepared.push((
                map_key(&key),
                Stored {
                    memory: matches!(entry.entity, EmbeddableEntityRef::Memory { .. }),
                    space,
                    owner: entry.owner,
                    version: entry.version,
                    generation: entry.generation,
                    owner_revision: 0,
                    head_revision: 0,
                    is_head: entry.is_head,
                    chunks: entry.vectors.iter().map(|v| v.values().to_vec()).collect(),
                },
            ));
        }
        let mut guard = self.state();
        for (key, stored) in prepared {
            let fenced = guard.fences.get(&key).copied().unwrap_or(0);
            let existing = guard.entries.get(&key).map_or(0, |s| s.generation);
            if stored.generation <= fenced || stored.generation <= existing {
                continue;
            }
            guard.entries.insert(key, stored);
        }
        Ok(())
    }

    async fn delete(&self, keys: &[VectorKey], fence: i64) -> Result<(), StorageError> {
        if fence <= 0 {
            return Err(StorageError::ConstraintViolation(
                "delete fence must be positive".into(),
            ));
        }
        let mut guard = self.state();
        for key in keys {
            let k = map_key(key);
            let slot = guard.fences.entry(k.clone()).or_insert(0);
            if fence > *slot {
                *slot = fence;
            }
            if guard
                .entries
                .get(&k)
                .is_some_and(|stored| stored.generation <= fence)
            {
                guard.entries.remove(&k);
            }
        }
        Ok(())
    }

    async fn set_owner(
        &self,
        targets: &[VectorMetadataTarget],
        owner: OwnerId,
        revision: i64,
    ) -> Result<(), StorageError> {
        if revision <= 0 {
            return Err(StorageError::ConstraintViolation(
                "metadata revision must be positive".into(),
            ));
        }
        for target in targets {
            if target.generation <= 0 {
                return Err(StorageError::ConstraintViolation(
                    "metadata target generation must be positive".into(),
                ));
            }
        }
        let mut guard = self.state();
        for target in targets {
            if let Some(stored) = guard.entries.get_mut(&map_key(&target.key))
                && stored.generation == target.generation
                && revision > stored.owner_revision
            {
                stored.owner = owner;
                stored.owner_revision = revision;
            }
        }
        Ok(())
    }

    async fn set_head(
        &self,
        targets: &[VectorMetadataTarget],
        is_head: bool,
        revision: i64,
    ) -> Result<(), StorageError> {
        if revision <= 0 {
            return Err(StorageError::ConstraintViolation(
                "metadata revision must be positive".into(),
            ));
        }
        for target in targets {
            if target.generation <= 0 {
                return Err(StorageError::ConstraintViolation(
                    "metadata target generation must be positive".into(),
                ));
            }
        }
        let mut guard = self.state();
        for target in targets {
            if let Some(stored) = guard.entries.get_mut(&map_key(&target.key))
                && stored.generation == target.generation
                && revision > stored.head_revision
            {
                stored.is_head = is_head;
                stored.head_revision = revision;
            }
        }
        Ok(())
    }

    async fn nearest(
        &self,
        filter: &VectorCandidateFilter,
        query: &SpaceVector,
        window: u32,
    ) -> Result<VectorCandidatePage, StorageError> {
        let mut found: Vec<VectorCandidate> = self
            .state()
            .entries
            .iter()
            .filter(|((entity, _, _), stored)| {
                stored.memory
                    && &stored.space == query.space()
                    && match filter.supersession {
                        SupersessionStatus::HeadsOnly => stored.is_head,
                        SupersessionStatus::IncludeSuperseded => true,
                    }
                    && (filter.owners.contains(&stored.owner)
                        || filter.memories.contains(&MemoryId::new(*entity)))
            })
            .map(|((entity, _, _), stored)| VectorCandidate {
                memory_id: MemoryId::new(*entity),
                similarity_score: stored
                    .chunks
                    .iter()
                    .map(|chunk| similarity(chunk, query.values()))
                    .fold(0.0, f32::max),
                generation: stored.generation,
            })
            .collect();
        found.sort_by(|a, b| {
            b.similarity_score
                .total_cmp(&a.similarity_score)
                .then(b.memory_id.cmp(&a.memory_id))
        });
        let bound =
            if !found.is_empty() && found.len() >= usize::try_from(window).unwrap_or(usize::MAX) {
                CandidateBound::Bounded
            } else {
                CandidateBound::Complete
            };
        found.truncate(usize::try_from(window).unwrap_or(usize::MAX));
        Ok(VectorCandidatePage::new(found, bound))
    }

    async fn entries(
        &self,
        after: Option<&VectorKey>,
        limit: u32,
    ) -> Result<VectorEntryPage, StorageError> {
        use std::ops::Bound;

        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let guard = self.state();
        let range = match after.map(map_key) {
            Some(start) => guard
                .entries
                .range((Bound::Excluded(start), Bound::Unbounded)),
            None => guard.entries.range(..),
        };
        let mut iter = range.map(|((entity, _, _), stored)| VectorEntry {
            key: VectorKey {
                entity_id: *entity,
                space: stored.space.clone(),
            },
            owner: stored.owner,
            version: stored.version,
            generation: stored.generation,
            is_head: stored.is_head,
        });
        let entries: Vec<VectorEntry> = iter.by_ref().take(limit).collect();
        let has_more = iter.next().is_some();
        Ok(VectorEntryPage::new(entries, has_more))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EmbeddingDim, GoalId, MemoryKind, UserId};

    fn vector(space: &EmbeddingSpace, head: [f32; 2]) -> SpaceVector {
        let mut values = vec![0.0; space.dim().width()];
        values[..2].copy_from_slice(&head);
        SpaceVector::new(space.clone(), values).expect("space width")
    }

    fn memory(id: u128) -> EmbeddableEntityRef {
        EmbeddableEntityRef::Memory {
            kind: MemoryKind::Fact,
            memory_id: MemoryId::new(Uuid::from_u128(id)),
        }
    }

    /// The reference index ranks by best chunk, narrows to owners, head status
    /// and the query's space, skips goals, replaces a version with a higher
    /// generation, and fences delayed puts after delete or retry.
    #[tokio::test]
    async fn the_in_memory_index_ranks_best_chunks_of_readable_memories() {
        let index = InMemoryVectorIndex::default();
        let space = EmbeddingSpace::new("m", EmbeddingDim::D384);
        let other_space = EmbeddingSpace::new("m", EmbeddingDim::D768);
        let owner = OwnerId::from(UserId::new(Uuid::from_u128(10)));
        let stranger = OwnerId::from(UserId::new(Uuid::from_u128(11)));
        let query = vector(&space, [1.0, 0.0]);
        let put = |entity, owner, generation, vectors: Vec<SpaceVector>| {
            let index = &index;
            async move {
                index
                    .put(entity, owner, 1, generation, true, &vectors)
                    .await
                    .unwrap();
            }
        };
        put(
            memory(1),
            owner,
            1,
            vec![vector(&space, [0.0, 1.0]), vector(&space, [1.0, 0.0])],
        )
        .await;
        put(memory(2), owner, 2, vec![vector(&space, [1.0, 1.0])]).await;
        put(memory(3), stranger, 3, vec![vector(&space, [1.0, 0.0])]).await;
        put(memory(6), stranger, 4, vec![vector(&space, [0.0, 1.0])]).await;
        put(memory(4), owner, 5, vec![vector(&other_space, [1.0, 0.0])]).await;
        put(
            EmbeddableEntityRef::Goal(GoalId::new(Uuid::from_u128(5))),
            owner,
            6,
            vec![vector(&space, [1.0, 0.0])],
        )
        .await;

        let filter = VectorCandidateFilter {
            owners: vec![owner],
            memories: vec![],
            supersession: SupersessionStatus::HeadsOnly,
        };
        let ranked = index.nearest(&filter, &query, 10).await.unwrap();
        assert_eq!(ranked.bound, CandidateBound::Complete);
        let ids: Vec<u128> = ranked
            .candidates
            .iter()
            .map(|c| c.memory_id.into_inner().as_u128())
            .collect();
        assert_eq!(ids, [1, 2]);
        assert!((ranked.candidates[0].similarity_score - 1.0).abs() < 1e-6);
        assert_eq!(ranked.candidates[0].generation, 1);

        assert!(
            index
                .nearest(&VectorCandidateFilter::default(), &query, 10)
                .await
                .unwrap()
                .candidates
                .is_empty()
        );
        let limited = index.nearest(&filter, &query, 1).await.unwrap();
        assert_eq!(limited.candidates.len(), 1);
        assert_eq!(limited.bound, CandidateBound::Bounded);
        let exact = VectorCandidateFilter {
            owners: vec![],
            memories: vec![MemoryId::new(Uuid::from_u128(6))],
            supersession: SupersessionStatus::IncludeSuperseded,
        };
        let exact_ranked = index.nearest(&exact, &query, 1).await.unwrap().candidates;
        assert_eq!(exact_ranked.len(), 1);
        assert_eq!(exact_ranked[0].memory_id, MemoryId::new(Uuid::from_u128(6)));

        put(memory(1), owner, 7, vec![vector(&space, [0.0, 1.0])]).await;
        let ranked = index.nearest(&filter, &query, 10).await.unwrap().candidates;
        assert_eq!(ranked[0].memory_id.into_inner().as_u128(), 2);

        let key = VectorKey {
            entity_id: Uuid::from_u128(2),
            space: space.clone(),
        };
        index
            .set_owner(
                &[VectorMetadataTarget {
                    key: key.clone(),
                    generation: 2,
                }],
                stranger,
                8,
            )
            .await
            .unwrap();
        let key1 = VectorKey {
            entity_id: Uuid::from_u128(1),
            space,
        };
        index.delete(&[key1], 9).await.unwrap();
        assert!(
            index
                .nearest(&filter, &query, 10)
                .await
                .unwrap()
                .candidates
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_in_memory_index_pages_entries_in_key_order() {
        let index = InMemoryVectorIndex::default();
        let space = EmbeddingSpace::new("m", EmbeddingDim::D384);
        let owner = OwnerId::from(UserId::new(Uuid::from_u128(10)));
        let stranger = OwnerId::from(UserId::new(Uuid::from_u128(11)));
        let vec = [vector(&space, [1.0, 0.0])];
        for id in 2..=6 {
            index
                .put(memory(id), owner, 1, i64::try_from(id).unwrap(), true, &vec)
                .await
                .unwrap();
        }
        let key = VectorKey {
            entity_id: Uuid::from_u128(2),
            space,
        };
        index
            .set_owner(
                &[VectorMetadataTarget {
                    key: key.clone(),
                    generation: 2,
                }],
                stranger,
                7,
            )
            .await
            .unwrap();
        let page = index.entries(None, 2).await.unwrap();
        assert_eq!(page.entries.len(), 2);
        assert!(page.has_more);
        let rest = index.entries(Some(&page.entries[1].key), 10).await.unwrap();
        assert!(!rest.has_more);
        assert_eq!(page.entries.len() + rest.entries.len(), 5);
        assert_eq!(page.entries[0].key, key);
        assert_eq!(page.entries[0].owner, stranger);
        assert_eq!(page.entries[0].generation, 2);
        assert!(page.entries[0].is_head);
    }

    #[tokio::test]
    async fn the_in_memory_index_fences_stale_metadata_updates() {
        let index = InMemoryVectorIndex::default();
        let space = EmbeddingSpace::new("m", EmbeddingDim::D384);
        let owner_a = OwnerId::from(UserId::new(Uuid::from_u128(10)));
        let owner_b = OwnerId::from(UserId::new(Uuid::from_u128(11)));
        let key1 = VectorKey {
            entity_id: Uuid::from_u128(1),
            space: space.clone(),
        };
        index
            .put(
                memory(1),
                owner_a,
                1,
                5,
                true,
                &[vector(&space, [1.0, 0.0])],
            )
            .await
            .unwrap();
        let target_g5 = VectorMetadataTarget {
            key: key1.clone(),
            generation: 5,
        };
        // Non-positive revisions or target generations are refused.
        assert!(matches!(
            index
                .set_head(std::slice::from_ref(&target_g5), false, 0)
                .await,
            Err(StorageError::ConstraintViolation(_))
        ));
        assert!(matches!(
            index
                .set_owner(
                    &[VectorMetadataTarget {
                        key: key1.clone(),
                        generation: 0,
                    }],
                    owner_b,
                    6,
                )
                .await,
            Err(StorageError::ConstraintViolation(_))
        ));
        // Head and owner track independent revisions within generation 5.
        index
            .set_owner(std::slice::from_ref(&target_g5), owner_b, 6)
            .await
            .unwrap();
        index
            .set_head(std::slice::from_ref(&target_g5), false, 6)
            .await
            .unwrap();
        index
            .set_head(std::slice::from_ref(&target_g5), true, 8)
            .await
            .unwrap();
        // A delayed `set_head(false)` with older revision 7 must not clobber revision 8.
        index
            .set_head(std::slice::from_ref(&target_g5), false, 7)
            .await
            .unwrap();
        // A delayed `set_owner(owner_a)` with older revision 5 must not clobber revision 6.
        index
            .set_owner(std::slice::from_ref(&target_g5), owner_a, 5)
            .await
            .unwrap();
        let entries = index.entries(None, 10).await.unwrap().entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].owner, owner_b);
        assert!(entries[0].is_head);

        // A newer publication generation rejects metadata updates targeting generation 5.
        index
            .put(
                memory(1),
                owner_b,
                2,
                10,
                true,
                &[vector(&space, [0.0, 1.0])],
            )
            .await
            .unwrap();
        index
            .set_head(std::slice::from_ref(&target_g5), false, 99)
            .await
            .unwrap();
        index
            .set_owner(std::slice::from_ref(&target_g5), owner_a, 99)
            .await
            .unwrap();
        let after_put = index.entries(None, 10).await.unwrap().entries;
        assert_eq!(after_put[0].generation, 10);
        assert_eq!(after_put[0].owner, owner_b);
        assert!(after_put[0].is_head);
    }

    fn ledger_rows(decision: &str) -> Vec<serde_json::Value> {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/ledger_vectors.json")).unwrap();
        let rows = vectors[decision].as_array().unwrap().clone();
        assert!(!rows.is_empty(), "{decision}");
        rows
    }

    /// The generation or revision `compared` with a stored `5`.
    fn against_five(compared: &serde_json::Value) -> i64 {
        match compared.as_str() {
            Some("older") => 4,
            Some("same") => 5,
            Some("newer") => 6,
            _ => panic!("invalid comparison: {compared}"),
        }
    }

    /// The reference index lands puts, deletes and metadata updates as the
    /// ledger decides, against a stored generation and against a fence; the
    /// metadata rows replay `Proxiima.Search.metadata_needs_the_exact_generation`.
    #[tokio::test]
    async fn the_in_memory_index_lands_writes_as_the_ledger_decides() {
        let space = EmbeddingSpace::new("m", EmbeddingDim::D384);
        let key = VectorKey {
            entity_id: Uuid::from_u128(1),
            space: space.clone(),
        };
        let stored_owner = OwnerId::from(UserId::new(Uuid::from_u128(10)));
        let incoming_owner = OwnerId::from(UserId::new(Uuid::from_u128(11)));
        let vectors = [vector(&space, [1.0, 0.0])];
        let entry = |index: &InMemoryVectorIndex| {
            index
                .state()
                .entries
                .get(&map_key(&key))
                .map(|stored| (stored.owner, stored.generation, stored.is_head))
        };

        for row in ledger_rows("Proxiima.Search.putLands") {
            let generation = against_five(&row["in"][0]);
            let lands = row["out"].as_bool().unwrap();
            let over_stored = stored_at_five(stored_owner, &vectors).await;
            over_stored
                .put(memory(1), incoming_owner, 2, generation, true, &vectors)
                .await
                .unwrap();
            assert_eq!(
                entry(&over_stored) == Some((incoming_owner, generation, true)),
                lands,
                "over a stored generation: {row}"
            );
            let over_fence = InMemoryVectorIndex::default();
            over_fence
                .delete(std::slice::from_ref(&key), 5)
                .await
                .unwrap();
            over_fence
                .put(memory(1), incoming_owner, 2, generation, true, &vectors)
                .await
                .unwrap();
            assert_eq!(entry(&over_fence).is_some(), lands, "over a fence: {row}");
        }

        for row in ledger_rows("Proxiima.Search.deleteRemoves") {
            // The input compares the stored `5` with the fence.
            let fence = 10 - against_five(&row["in"][0]);
            let index = stored_at_five(stored_owner, &vectors).await;
            index
                .delete(std::slice::from_ref(&key), fence)
                .await
                .unwrap();
            assert_eq!(
                entry(&index).is_none(),
                row["out"].as_bool().unwrap(),
                "{row}"
            );
        }

        // Both fields start at revision 5; each update is compared with its own field.
        let at_five = [VectorMetadataTarget {
            key: key.clone(),
            generation: 5,
        }];
        for row in ledger_rows("Proxiima.Search.metadataLands") {
            let target = [VectorMetadataTarget {
                key: key.clone(),
                generation: against_five(&row["in"][0]),
            }];
            let revision = against_five(&row["in"][1]);
            let lands = row["out"].as_bool().unwrap();
            let index = stored_at_five(stored_owner, &vectors).await;
            index.set_owner(&at_five, stored_owner, 5).await.unwrap();
            index.set_head(&at_five, true, 5).await.unwrap();
            index
                .set_owner(&target, incoming_owner, revision)
                .await
                .unwrap();
            index.set_head(&target, false, revision).await.unwrap();
            let (owner, _, is_head) = entry(&index).unwrap();
            assert_eq!(owner == incoming_owner, lands, "owner: {row}");
            assert_eq!(!is_head, lands, "head: {row}");
        }
    }

    async fn stored_at_five(owner: OwnerId, vectors: &[SpaceVector]) -> InMemoryVectorIndex {
        let index = InMemoryVectorIndex::default();
        index
            .put(memory(1), owner, 1, 5, true, vectors)
            .await
            .unwrap();
        index
    }

    #[tokio::test]
    async fn the_in_memory_index_fences_stale_and_deleted_generations() {
        let index = InMemoryVectorIndex::default();
        let space = EmbeddingSpace::new("m", EmbeddingDim::D384);
        let owner = OwnerId::from(UserId::new(Uuid::from_u128(10)));
        let query = vector(&space, [1.0, 0.0]);
        let filter = VectorCandidateFilter {
            owners: vec![owner],
            memories: vec![],
            supersession: SupersessionStatus::HeadsOnly,
        };
        let key1 = VectorKey {
            entity_id: Uuid::from_u128(1),
            space: space.clone(),
        };

        index
            .put(memory(1), owner, 1, 5, true, &[vector(&space, [0.0, 1.0])])
            .await
            .unwrap();
        // Delayed generation <= 5 must not replace generation 5.
        index
            .put(memory(1), owner, 1, 4, true, &[vector(&space, [1.0, 0.0])])
            .await
            .unwrap();
        let ranked = index.nearest(&filter, &query, 10).await.unwrap().candidates;
        assert_eq!(ranked[0].generation, 5);
        assert!(ranked[0].similarity_score < 0.1);

        // Delete with fence 6 removes generation 5 and rejects late puts <= 6.
        index.delete(&[key1], 6).await.unwrap();
        index
            .put(memory(1), owner, 1, 6, true, &[vector(&space, [1.0, 0.0])])
            .await
            .unwrap();
        assert!(
            index
                .nearest(&filter, &query, 10)
                .await
                .unwrap()
                .candidates
                .is_empty()
        );
        // Generation 7 > fence 6 is accepted.
        index
            .put(memory(1), owner, 1, 7, true, &[vector(&space, [1.0, 0.0])])
            .await
            .unwrap();
        let rebuilt = index.nearest(&filter, &query, 10).await.unwrap().candidates;
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].generation, 7);
    }

    #[tokio::test]
    async fn the_in_memory_index_filters_superseded_entries_for_heads_only() {
        let index = InMemoryVectorIndex::default();
        let space = EmbeddingSpace::new("m", EmbeddingDim::D384);
        let owner = OwnerId::from(UserId::new(Uuid::from_u128(10)));
        let query = vector(&space, [1.0, 0.0]);
        let v1 = [vector(&space, [1.0, 0.0])];
        let v2 = [vector(&space, [0.5, 0.5])];
        index
            .put_batch(&[
                VectorPut {
                    entity: memory(1),
                    owner,
                    version: 1,
                    generation: 1,
                    is_head: true,
                    vectors: &v1,
                },
                VectorPut {
                    entity: memory(2),
                    owner,
                    version: 1,
                    generation: 2,
                    is_head: true,
                    vectors: &v2,
                },
            ])
            .await
            .unwrap();
        let key1 = VectorKey {
            entity_id: Uuid::from_u128(1),
            space,
        };
        index
            .set_head(
                &[VectorMetadataTarget {
                    key: key1,
                    generation: 1,
                }],
                false,
                3,
            )
            .await
            .unwrap();
        let heads_only = index
            .nearest(
                &VectorCandidateFilter {
                    owners: vec![owner],
                    memories: vec![],
                    supersession: SupersessionStatus::HeadsOnly,
                },
                &query,
                10,
            )
            .await
            .unwrap()
            .candidates;
        assert_eq!(
            heads_only
                .iter()
                .map(|c| c.memory_id.into_inner().as_u128())
                .collect::<Vec<_>>(),
            [2]
        );
        let all_versions = index
            .nearest(
                &VectorCandidateFilter {
                    owners: vec![owner],
                    memories: vec![],
                    supersession: SupersessionStatus::IncludeSuperseded,
                },
                &query,
                10,
            )
            .await
            .unwrap()
            .candidates;
        assert_eq!(
            all_versions
                .iter()
                .map(|c| c.memory_id.into_inner().as_u128())
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }
}
