//! The upload lifecycle and publishing a staged upload.

use crate::error::map_err;
use proxiima_core::{OwnerId, StorageError};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

/// The database's closed upload lifecycle, shared by staging and publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(
    type_name = "proxiima_core.blob_upload_status",
    rename_all = "lowercase"
)]
pub enum UploadStatus {
    Pending,
    Completed,
    Aborted,
    Expired,
}

/// The result of publishing a staged upload against its cited object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishTransitionDecision {
    WonPending,
    AlreadyFinished,
}

/// Classify a locked upload publication result.
///
/// An upload has one outcome. Once an abort or an expiry is recorded, the
/// upload is final and no Fact may publish it: the abort or expiry is free
/// to let go of its bytes, so a late finish is refused rather than allowed
/// to cite them.
///
/// # Errors
/// [`StorageError::Conflict`] when the row is already aborted or expired;
/// refuses contradictory object identities or affected-row counts.
pub fn finish_transition_decision(
    observed_status: UploadStatus,
    observed_cited_object_id: Option<Uuid>,
    cited_object_id: Uuid,
    rows_affected: u64,
) -> Result<FinishTransitionDecision, StorageError> {
    match rows_affected {
        1 if observed_status == UploadStatus::Pending => Ok(FinishTransitionDecision::WonPending),
        1 => Err(StorageError::ConstraintViolation(
            "upload finish transitioned a non-pending row".into(),
        )),
        0 => match observed_status {
            UploadStatus::Completed if observed_cited_object_id == Some(cited_object_id) => {
                Ok(FinishTransitionDecision::AlreadyFinished)
            }
            UploadStatus::Completed => Err(StorageError::ConstraintViolation(
                "upload already completed against a different cited object".into(),
            )),
            UploadStatus::Aborted => Err(StorageError::Conflict(
                "upload is aborted; prepare and stage the bytes again".into(),
            )),
            UploadStatus::Expired => Err(StorageError::Conflict(
                "upload is expired; prepare and stage the bytes again".into(),
            )),
            UploadStatus::Pending => Err(StorageError::ConstraintViolation(
                "upload finish did not transition pending row".into(),
            )),
        },
        other => Err(StorageError::ConstraintViolation(format!(
            "upload finish affected {other} rows"
        ))),
    }
}

/// Publish an upload on the caller's transaction; return its cleanup bucket.
///
/// Shared by cited Fact admission and direct blob-port completion. Takes the
/// owner fence, canonical-key fence and row lock in that order, verifies the
/// staged hash against the owner-scoped cited object, and settles the row.
/// Completed replays perform no write. No transaction control or S3 I/O.
///
/// # Errors
/// [`StorageError::Conflict`] for an aborted or expired upload (one outcome
/// per upload). Refuses absent uploads, unstaged or mismatched bytes, and
/// contradictory completed objects. Database faults retain their storage
/// error class.
pub async fn complete_upload_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    owner: &OwnerId,
    upload_id: Uuid,
    cited_object_id: Uuid,
) -> Result<String, StorageError> {
    let key = proxiima_core::citations::uploaded_blob_object_key(upload_id);
    crate::access::fences::lock_owner_fence_shared_tx(tx, owner).await?;
    crate::access::fences::lock_object_keys_tx(tx, std::slice::from_ref(&key)).await?;
    let (bucket, status, blob_id, hash): (String, UploadStatus, Option<Uuid>, Option<Vec<u8>>) =
        sqlx::query_as(
            "SELECT bucket, status, blob_id, content_hash
               FROM proxiima_core.blob_uploads
              WHERE owner_id = $1 AND upload_id = $2
              FOR UPDATE",
        )
        .bind(owner.into_inner())
        .bind(upload_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_err)?
        .ok_or_else(|| StorageError::ConstraintViolation("upload not found for Owner".into()))?;

    if status != UploadStatus::Pending {
        // Completed replays the same object; aborted and expired are final.
        finish_transition_decision(status, blob_id, cited_object_id, 0)?;
        return Ok(bucket);
    }
    let hash = hash
        .as_deref()
        .filter(|hash| hash.len() == 32)
        .ok_or_else(|| {
            StorageError::ConstraintViolation(
                "upload has no exact staged content identity; restage before finish".into(),
            )
        })?;
    sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT content_hash FROM proxiima_core.blob
          WHERE blob_id = $1 AND owner_id = $2
            AND schema_id = $3 AND content_hash = $4
          FOR SHARE",
    )
    .bind(cited_object_id)
    .bind(owner.into_inner())
    .bind(proxiima_core::UPLOADED_BLOB_SCHEMA_ID)
    .bind(hash)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_err)?
    .ok_or_else(|| {
        StorageError::ConstraintViolation(
            "cited object does not match the staged upload content identity".into(),
        )
    })?;

    let changed = sqlx::query(
        "UPDATE proxiima_core.blob_uploads
            SET object_key = $1, status = 'completed', blob_id = $2, completed_at = now()
          WHERE owner_id = $3 AND upload_id = $4 AND status = 'pending'",
    )
    .bind(key)
    .bind(cited_object_id)
    .bind(owner.into_inner())
    .bind(upload_id)
    .execute(&mut **tx)
    .await
    .map_err(map_err)?
    .rows_affected();
    finish_transition_decision(status, blob_id, cited_object_id, changed)?;
    Ok(bucket)
}

/// Refuse a transfer while upload publication can still attach one of this
/// series' cited blobs to its source-owned locator.
pub(super) async fn ensure_series_uploads_published(
    tx: &mut Transaction<'_, Postgres>,
    handle: uuid::Uuid,
    from_id: uuid::Uuid,
) -> Result<(), StorageError> {
    // Staged rows can still await publication by a direct blob-port caller
    // or an older host. Engine completion publishes its upload row in the
    // Fact transaction. Refuse a transfer while another staged row could
    // later attach this blob under the source owner.
    // The ordinary fence is identity- and series-scoped: an abandoned prepare
    // has no content identity, and an unrelated staged upload cannot claim
    // this series. The one exception is a row staged before 0007: it has a
    // canonical locator and SHA-256 witness but no BLAKE3 column to relate it
    // to a cited blob. That ambiguous upgrade state must fence the owner until
    // a completion retry re-hashes it; otherwise transfer can move the Fact's
    // blob before completion attaches the source upload row.
    let unpublished_upload: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
               FROM proxiima_core.blob_uploads u
               JOIN proxiima_core.blob b
                 ON b.owner_id = u.owner_id
                AND b.schema_id = 'core/uploaded-blob-v1'
                AND b.content_hash = u.content_hash
              WHERE u.owner_id = $1
                AND u.content_hash IS NOT NULL
                AND b.blob_id IN (
                    SELECT m.blob_id
                      FROM proxiima_core.memory m /* FORGET-POLICY: includes-forgotten (transfer moves forgotten rows too) */
                     WHERE m.handle = $2
                       AND m.owner_id = $1
                       AND m.blob_id IS NOT NULL
                )
                AND (
                    u.status = 'pending'
                    OR (u.status = 'completed' AND u.blob_id IS NULL)
                    OR (
                        u.status IN ('aborted', 'expired')
                        AND NOT EXISTS (
                            SELECT 1
                              FROM proxiima_core.blob_uploads p
                             WHERE p.owner_id = $1
                               AND p.blob_id = b.blob_id
                               AND p.status = 'completed'
                               AND p.completed_at IS NOT NULL
                               AND p.content_hash = b.content_hash
                               AND octet_length(p.sha256) = 32
                               AND p.expected_byte_len >= 0
                               AND btrim(p.bucket) <> ''
                               AND btrim(p.filename) <> ''
                               AND btrim(p.mime) <> ''
                               AND p.object_key = 'objects/' || COALESCE(
                                   p.mounted_from_upload_id, p.upload_id
                               )::text
                        )
                    )
                )
         ) OR EXISTS (
             SELECT 1
               FROM proxiima_core.blob_uploads u
              WHERE u.owner_id = $1
                AND u.content_hash IS NULL
                AND u.status = 'pending'
                AND u.completed_at IS NULL
                AND octet_length(u.sha256) = 32
                AND u.expected_byte_len >= 0
                AND btrim(u.bucket) <> ''
                AND btrim(u.filename) <> ''
                AND btrim(u.mime) <> ''
                AND u.object_key = 'objects/' || COALESCE(
                    u.mounted_from_upload_id, u.upload_id
                )::text
         )",
    )
    .bind(from_id)
    .bind(handle)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_err)?;
    if unpublished_upload {
        return Err(StorageError::Conflict(
            "owner has an upload whose publication is not complete".into(),
        ));
    }
    Ok(())
}
