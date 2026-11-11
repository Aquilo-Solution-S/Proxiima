//! Typed reads on the caller's owner transaction.

use proxiima_core::{EmbeddingSpace, MemoryId, MemoryKind, OwnerId, SchemaId, StorageError};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::error::map_err;
use crate::pg_enums::PgMemoryKind;
use crate::pgvector::stored_space;

/// One version of a memory series, as the caller's transaction sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryVersion {
    pub memory_id: MemoryId,
    /// The series this version belongs to.
    pub handle: Uuid,
    pub kind: MemoryKind,
    pub schema_id: SchemaId,
    pub origins: Vec<MemoryId>,
    /// True when this version is its series' head.
    pub head: bool,
}

/// A memory's current vector in one embedding space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEmbedding {
    pub memory_id: MemoryId,
    pub space: EmbeddingSpace,
}

type VersionRow = (Uuid, Uuid, PgMemoryKind, String, Vec<Uuid>, bool);

fn decoded(rows: Vec<VersionRow>) -> Vec<MemoryVersion> {
    rows.into_iter().map(MemoryVersion::from_row).collect()
}

impl MemoryVersion {
    fn from_row((t, handle, kind, schema_id, origins, head): VersionRow) -> Self {
        Self {
            memory_id: MemoryId::new(t),
            handle,
            kind: kind.into(),
            schema_id: SchemaId::new(schema_id),
            origins: origins.into_iter().map(MemoryId::new).collect(),
            head,
        }
    }
}

macro_rules! version_select {
    ($filter:literal) => {
        concat!(
            "SELECT m.t, m.handle, m.kind, m.schema_id, m.origins,
                    EXISTS (SELECT 1 FROM proxiima_core.memory_head h
                             WHERE h.handle = m.handle AND h.t = m.t) AS head
               FROM proxiima_core.memory m
              WHERE m.owner_id = $1 AND ",
            $filter
        )
    };
}

const VERSIONS_OF_IDS_SQL: &str = version_select!("m.t = ANY($2) ORDER BY m.t");
const VERSIONS_OF_SERIES_SQL: &str = version_select!("m.handle = $2 ORDER BY m.t");
const LOCK_ONE_SQL: &str = version_select!("m.t = $2 FOR SHARE OF m");
const VERSIONS_CITING_SQL: &str = version_select!("m.blob_id = $2 ORDER BY m.t");

fn raw_ids(memory_ids: &[MemoryId]) -> Vec<Uuid> {
    memory_ids.iter().map(|id| id.into_inner()).collect()
}

/// The versions among `memory_ids` that `owner` owns, ordered by `t`.
///
/// Runs on `tx`, which the caller already holds. `tx` is a transaction
/// [`crate::begin_owner_transaction`] opened: owner row security applies, so
/// a forgotten memory and another owner's memory are absent from the answer.
/// Nothing is written or locked.
///
/// # Errors
///
/// Storage errors from the read.
pub async fn memory_versions(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    memory_ids: &[MemoryId],
) -> Result<Vec<MemoryVersion>, StorageError> {
    let rows: Vec<VersionRow> = sqlx::query_as(VERSIONS_OF_IDS_SQL)
        .bind(owner.into_inner())
        .bind(raw_ids(memory_ids))
        .fetch_all(tx.as_mut())
        .await
        .map_err(map_err)?;
    Ok(decoded(rows))
}

/// Every version of the series `handle` that `owner` owns, ordered by `t`.
///
/// Runs on `tx`, which the caller already holds. Owner row security applies,
/// so a forgotten version and another owner's series are absent. Nothing is
/// written or locked.
///
/// # Errors
///
/// Storage errors from the read.
pub async fn series_versions(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    handle: Uuid,
) -> Result<Vec<MemoryVersion>, StorageError> {
    let rows: Vec<VersionRow> = sqlx::query_as(VERSIONS_OF_SERIES_SQL)
        .bind(owner.into_inner())
        .bind(handle)
        .fetch_all(tx.as_mut())
        .await
        .map_err(map_err)?;
    Ok(decoded(rows))
}

/// Read one memory and share-lock its row until the caller commits or rolls
/// back; an erase or transfer of that memory waits.
///
/// Runs on `tx`, which the caller already holds. Owner row security applies:
/// a forgotten or foreign memory is `Ok(None)` and nothing is locked.
/// `PostgreSQL` checks a row lock against the write policy as well, so a
/// caller that may read the memory but not write its kind also gets
/// `Ok(None)`; [`memory_versions`] answers for such a caller.
///
/// # Errors
///
/// Storage errors from the read or the lock.
pub async fn lock_memory_shared(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    memory_id: MemoryId,
) -> Result<Option<MemoryVersion>, StorageError> {
    let row: Option<VersionRow> = sqlx::query_as(LOCK_ONE_SQL)
        .bind(owner.into_inner())
        .bind(memory_id.into_inner())
        .fetch_optional(tx.as_mut())
        .await
        .map_err(map_err)?;
    Ok(row.map(MemoryVersion::from_row))
}

/// Every memory of any kind of `owner` that cites the object
/// `cited_object_id`, ordered by `t`.
///
/// Runs on `tx`, which the caller already holds. Owner row security applies,
/// so a forgotten or foreign memory is absent. Nothing is written or locked.
///
/// # Errors
///
/// Storage errors from the read.
pub async fn memories_citing(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    cited_object_id: Uuid,
) -> Result<Vec<MemoryVersion>, StorageError> {
    let rows: Vec<VersionRow> = sqlx::query_as(VERSIONS_CITING_SQL)
        .bind(owner.into_inner())
        .bind(cited_object_id)
        .fetch_all(tx.as_mut())
        .await
        .map_err(map_err)?;
    Ok(decoded(rows))
}

/// The ids among `cited_object_ids` the store records for `owner`.
///
/// This is the store's record of the object, not a statement about the bytes
/// in the object store. Runs on `tx`, which the caller already holds; owner
/// row security applies, so another owner's object is absent.
///
/// # Errors
///
/// Storage errors from the read.
pub async fn cited_objects_present(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    cited_object_ids: &[Uuid],
) -> Result<Vec<Uuid>, StorageError> {
    sqlx::query_scalar(
        "SELECT b.blob_id FROM proxiima_core.blob b
          WHERE b.owner_id = $1 AND b.blob_id = ANY($2)
          ORDER BY b.blob_id",
    )
    .bind(owner.into_inner())
    .bind(cited_object_ids)
    .fetch_all(tx.as_mut())
    .await
    .map_err(map_err)
}

/// For each memory among `memory_ids` that `owner` owns, each embedding space
/// in which it has a current vector, ordered by memory and space.
///
/// Runs on `tx`, which the caller already holds. The memory's own row
/// security applies, so a forgotten or foreign memory, or one of a kind the
/// caller may not read, has no entry. Nothing is written.
///
/// # Errors
///
/// [`StorageError::Internal`] when a stored width names no embedding lane.
/// Storage errors from the read.
pub async fn memory_embeddings(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    memory_ids: &[MemoryId],
) -> Result<Vec<MemoryEmbedding>, StorageError> {
    let rows: Vec<(Uuid, String, i16)> = sqlx::query_as(
        "SELECT h.entity_id, h.model_id, h.dim
           FROM proxiima_core.embedding_heads h
           JOIN proxiima_core.memory m
             ON m.t = h.entity_id AND m.owner_id = h.owner_id
          WHERE h.owner_id = $1 AND h.entity_id = ANY($2)
          ORDER BY h.entity_id, h.model_id, h.dim",
    )
    .bind(owner.into_inner())
    .bind(raw_ids(memory_ids))
    .fetch_all(tx.as_mut())
    .await
    .map_err(map_err)?;
    rows.into_iter()
        .map(|(entity_id, model_id, dim)| {
            Ok(MemoryEmbedding {
                memory_id: MemoryId::new(entity_id),
                space: stored_space(model_id, dim)?,
            })
        })
        .collect()
}

/// The text-search configurations registered in this store, by name.
///
/// Runs on `tx`, which the caller already holds. The table is store-wide, so
/// there is no owner parameter; its policy admits any owner-scoped
/// transaction. Nothing is written.
///
/// # Errors
///
/// Storage errors from the read.
pub async fn lexical_languages(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<Vec<String>, StorageError> {
    sqlx::query_scalar("SELECT config::text FROM proxiima_core.lexical_languages ORDER BY 1")
        .fetch_all(tx.as_mut())
        .await
        .map_err(map_err)
}
