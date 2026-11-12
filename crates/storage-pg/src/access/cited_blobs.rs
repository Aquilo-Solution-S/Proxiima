//! Copying, moving and mounting a memory's cited blobs.

use super::fences::{lock_object_keys_tx, lock_owner_fence_shared_tx};
use super::transfer::{BLOB_SURFACE, dedupe_leg, dedupe_lookup_sql};
use super::upload_keys::blob_upload_object_keys;
use crate::error::map_err;
use proxiima_core::StorageError;
use proxiima_core::owner_inverse::OwnerSurfaces;
use sqlx::{Postgres, Transaction};

/// Which upload metadata the caller is authorized to preserve.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CitedBlobUploads {
    Owner,
    Publication(Option<uuid::Uuid>),
}

/// Preserve a citation with destination-owned metadata over the existing object.
pub(crate) async fn copy_cited_blob_for_owner(
    tx: &mut Transaction<'_, Postgres>,
    surfaces: &OwnerSurfaces,
    from_id: uuid::Uuid,
    to_id: uuid::Uuid,
    blob_id: uuid::Uuid,
    uploads: CitedBlobUploads,
) -> Result<uuid::Uuid, StorageError> {
    let leg = dedupe_leg(surfaces, BLOB_SURFACE)?;
    let (schema_id, content_hash, selected_upload_id) =
        load_cited_blob_source(tx, blob_id, from_id, uploads).await?;
    if from_id == to_id {
        return Ok(blob_id);
    }
    let existing =
        find_destination_blob(tx, leg.dedupe_key, to_id, &schema_id, &content_hash).await?;
    if let Some(dest_blob_id) = existing {
        return Ok(dest_blob_id);
    }
    mount_blob_for_destination(
        tx,
        to_id,
        BlobMountSource {
            owner_id: from_id,
            blob_id,
            schema_id: &schema_id,
            content_hash: &content_hash,
            selected_upload_id,
        },
    )
    .await
}

pub(super) async fn load_cited_blob_source(
    tx: &mut Transaction<'_, Postgres>,
    blob_id: uuid::Uuid,
    owner_id: uuid::Uuid,
    uploads: CitedBlobUploads,
) -> Result<(String, Vec<u8>, Option<uuid::Uuid>), StorageError> {
    // The source owner's fence first: an erase of that owner locks only the
    // keys another owner also names, relying on this fence for the rest.
    lock_owner_fence_shared_tx(tx, &proxiima_core::OwnerId::from(owner_id)).await?;
    let object_keys = blob_upload_object_keys(tx, &[blob_id]).await?;
    lock_object_keys_tx(tx, &object_keys).await?;
    let Some((schema_id, content_hash)) = sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT schema_id, content_hash FROM proxiima_core.blob
          WHERE blob_id = $1 AND owner_id = $2 FOR UPDATE",
    )
    .bind(blob_id)
    .bind(owner_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_err)?
    else {
        return Err(StorageError::ConstraintViolation(
            "memory cites a blob outside its source owner".into(),
        ));
    };
    let selected_upload_id = match uploads {
        CitedBlobUploads::Publication(Some(upload_id)) => Some(upload_id),
        CitedBlobUploads::Publication(None)
            if schema_id == proxiima_core::UPLOADED_BLOB_SCHEMA_ID =>
        {
            return Err(StorageError::ConstraintViolation(
                "fork publication has no captured upload".into(),
            ));
        }
        CitedBlobUploads::Owner | CitedBlobUploads::Publication(None) => None,
    };
    let valid_uploads = has_only_exact_upload_publications(
        tx,
        blob_id,
        owner_id,
        &content_hash,
        selected_upload_id,
    )
    .await?;
    if schema_id == proxiima_core::UPLOADED_BLOB_SCHEMA_ID && !valid_uploads {
        return Err(StorageError::ConstraintViolation(
            "uploaded blob does not have an exact-only completed source-owner publication set"
                .into(),
        ));
    }
    Ok((schema_id, content_hash, selected_upload_id))
}

pub(super) async fn find_destination_blob(
    tx: &mut Transaction<'_, Postgres>,
    dedupe_key: &'static [&'static str],
    owner_id: uuid::Uuid,
    schema_id: &str,
    content_hash: &[u8],
) -> Result<Option<uuid::Uuid>, StorageError> {
    let existing: Option<uuid::Uuid> =
        sqlx::query_scalar(dedupe_lookup_sql(BLOB_SURFACE, "blob_id", dedupe_key)?)
            .bind(owner_id)
            .bind(schema_id)
            .bind(content_hash)
            .fetch_optional(&mut **tx)
            .await
            .map_err(map_err)?;
    if let Some(dest_blob_id) = existing
        && schema_id == proxiima_core::UPLOADED_BLOB_SCHEMA_ID
        && !has_only_exact_upload_publications(tx, dest_blob_id, owner_id, content_hash, None)
            .await?
    {
        return Err(StorageError::ConstraintViolation(
            "destination uploaded blob does not have an exact-only completed publication set"
                .into(),
        ));
    }
    Ok(existing)
}

/// A blob's completed upload rows are a readable publication set only when at
/// least one exists and every one carries exact metadata and the owner-free
/// key minted by its upload lineage. Transfer must reject a mixed valid and
/// malformed set: the read path's newest-row choice could otherwise select
/// the malformed row after transfer.
async fn has_only_exact_upload_publications(
    tx: &mut Transaction<'_, Postgres>,
    blob_id: uuid::Uuid,
    owner_id: uuid::Uuid,
    content_hash: &[u8],
    selected_upload_id: Option<uuid::Uuid>,
) -> Result<bool, StorageError> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM proxiima_core.blob_uploads u
              WHERE u.blob_id = $1
                AND u.owner_id = $2
                AND u.status = 'completed'
                AND u.completed_at IS NOT NULL
                AND u.content_hash = $3
                AND octet_length(u.sha256) = 32
                AND u.expected_byte_len >= 0
                AND btrim(u.bucket) <> ''
                AND btrim(u.filename) <> ''
                AND btrim(u.mime) <> ''
                AND u.object_key =
                    'objects/' || COALESCE(u.mounted_from_upload_id, u.upload_id)::text
                AND ($4::uuid IS NULL OR u.upload_id = $4)
         ) AND NOT EXISTS (
             SELECT 1
               FROM proxiima_core.blob_uploads u
              WHERE u.blob_id = $1
                AND u.owner_id = $2
                AND u.status = 'completed'
                AND ($4::uuid IS NULL OR u.upload_id = $4)
                AND (
                    u.completed_at IS NULL
                    OR u.content_hash IS DISTINCT FROM $3
                    OR octet_length(u.sha256) IS DISTINCT FROM 32
                    OR u.expected_byte_len < 0
                    OR btrim(u.bucket) = ''
                    OR btrim(u.filename) = ''
                    OR btrim(u.mime) = ''
                    OR u.object_key IS DISTINCT FROM
                        'objects/' || COALESCE(
                            u.mounted_from_upload_id, u.upload_id
                        )::text
                )
         )",
    )
    .bind(blob_id)
    .bind(owner_id)
    .bind(content_hash)
    .bind(selected_upload_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_err)
}

/// Case 2: the rows change hands, the object does not move, nothing is
/// minted.
pub(super) async fn move_blob_rows_in_place(
    tx: &mut Transaction<'_, Postgres>,
    blob_id: uuid::Uuid,
    from_id: uuid::Uuid,
    to_id: uuid::Uuid,
    schema_id: &str,
    content_hash: &[u8],
) -> Result<(), StorageError> {
    let moved_blob = sqlx::query(
        "UPDATE proxiima_core.blob
            SET owner_id = $2
          WHERE blob_id = $1 AND owner_id = $3",
    )
    .bind(blob_id)
    .bind(to_id)
    .bind(from_id)
    .execute(&mut **tx)
    .await
    .map_err(map_err)?
    .rows_affected();
    if moved_blob != 1 {
        return Err(StorageError::Retryable(
            "cited blob owner changed during transfer".into(),
        ));
    }
    // The upload row moves with the blob row it describes: the read path
    // requires both to name the same owner, so leaving it behind makes a
    // transferred citation unreadable at the destination while still
    // counting against the source. The object itself does not move — its key
    // derives from `upload_id`, which is unchanged.
    let moved_uploads = sqlx::query(
        "UPDATE proxiima_core.blob_uploads
            SET owner_id = $2
          WHERE blob_id = $1
            AND owner_id = $3
            AND status = 'completed'
            AND completed_at IS NOT NULL
            AND content_hash = $4
            AND octet_length(sha256) = 32
            AND expected_byte_len >= 0
            AND btrim(bucket) <> ''
            AND btrim(filename) <> ''
            AND btrim(mime) <> ''
            AND object_key =
                'objects/' || COALESCE(mounted_from_upload_id, upload_id)::text",
    )
    .bind(blob_id)
    .bind(to_id)
    .bind(from_id)
    .bind(content_hash)
    .execute(&mut **tx)
    .await
    .map_err(map_err)?
    .rows_affected();
    if schema_id == proxiima_core::UPLOADED_BLOB_SCHEMA_ID && moved_uploads == 0 {
        return Err(StorageError::ConstraintViolation(
            "uploaded blob has no source-owner publication row to move".into(),
        ));
    }
    let foreign_upload_remains: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM proxiima_core.blob_uploads
              WHERE blob_id = $1 AND owner_id <> $2
         )",
    )
    .bind(blob_id)
    .bind(to_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_err)?;
    if foreign_upload_remains {
        return Err(StorageError::ConstraintViolation(
            "cited blob has an upload row outside its source owner".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub(super) struct BlobMountSource<'a> {
    pub(super) owner_id: uuid::Uuid,
    pub(super) blob_id: uuid::Uuid,
    pub(super) schema_id: &'a str,
    pub(super) content_hash: &'a [u8],
    pub(super) selected_upload_id: Option<uuid::Uuid>,
}

/// Case 3: the destination gets its own row over the source's object.
pub(super) async fn mount_blob_for_destination(
    tx: &mut Transaction<'_, Postgres>,
    to_id: uuid::Uuid,
    source: BlobMountSource<'_>,
) -> Result<uuid::Uuid, StorageError> {
    let BlobMountSource {
        owner_id: from_id,
        blob_id,
        schema_id,
        content_hash,
        selected_upload_id,
    } = source;
    let dest_blob_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO proxiima_core.blob (owner_id, schema_id, content_hash)
         VALUES ($1, $2, $3)
         RETURNING blob_id",
    )
    .bind(to_id)
    .bind(schema_id)
    .bind(content_hash)
    .fetch_one(&mut **tx)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => StorageError::Retryable(
            "destination dedupe row appeared during transfer; retry the complete transaction"
                .into(),
        ),
        _ => map_err(err),
    })?;
    // One mounted upload row per completed source upload row, naming the
    // same object. `COALESCE(u.mounted_from_upload_id, u.upload_id)` rather
    // than `u.upload_id`: mounting a mount must still resolve to the row
    // that actually uploaded bytes, because an intermediate row never had
    // an object of its own.
    //
    // `status`, `object_store`, `sha256`, `etag` and the byte length are
    // copied because they describe the OBJECT, which is the thing being shared; the expiry
    // and completion timestamps are copied for the same reason. Only the
    // identity columns differ.
    let mounted_uploads = sqlx::query(
        "INSERT INTO proxiima_core.blob_uploads
             (owner_id, bucket, object_store, object_key, filename, mime, expected_byte_len,
              status, blob_id, sha256, content_hash, etag, expires_at, completed_at,
              mounted_from_upload_id)
         SELECT $2, u.bucket, u.object_store, u.object_key, u.filename, u.mime,
                u.expected_byte_len,
                u.status, $3, u.sha256, u.content_hash, u.etag, u.expires_at, u.completed_at,
                COALESCE(u.mounted_from_upload_id, u.upload_id)
          FROM proxiima_core.blob_uploads u
          WHERE u.blob_id = $1
            AND u.owner_id = $4
            AND u.status = 'completed'
            AND u.completed_at IS NOT NULL
            AND u.content_hash = $5
            AND octet_length(u.sha256) = 32
            AND u.expected_byte_len >= 0
            AND btrim(u.bucket) <> ''
            AND btrim(u.filename) <> ''
            AND btrim(u.mime) <> ''
            AND u.object_key =
                'objects/' || COALESCE(u.mounted_from_upload_id, u.upload_id)::text
            AND ($6::uuid IS NULL OR u.upload_id = $6)",
    )
    .bind(blob_id)
    .bind(to_id)
    .bind(dest_blob_id)
    .bind(from_id)
    .bind(content_hash)
    .bind(selected_upload_id)
    .execute(&mut **tx)
    .await
    .map_err(map_err)?
    .rows_affected();
    if schema_id == proxiima_core::UPLOADED_BLOB_SCHEMA_ID && mounted_uploads == 0 {
        return Err(StorageError::ConstraintViolation(
            "uploaded blob has no source-owner publication row to mount".into(),
        ));
    }
    Ok(dest_blob_id)
}

#[cfg(test)]
#[path = "cited_blobs_pg_tests.rs"]
mod cited_blobs_pg_tests;
