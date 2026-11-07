//! Metadata-only cited-blob queries.

use proxiima_core::{AuthzContext, OwnerId};

use super::CitedBlobStore;
use super::guards::ensure_owner_access;
use super::rows::find_held_blobs_on_connection;
use crate::error::BlobError;
use proxiima_core::storage_ports::{CitedBlobHeld, MAX_HELD_BLOB_DIGESTS};

impl CitedBlobStore {
    /// Which of `content_hashes` this owner already holds.
    ///
    /// READ AUTHORITY, NOT WRITE. The caller is asking what the corpus
    /// contains, and the answer is metadata about artefacts it may already
    /// read — so this gates exactly like delivery authorization does. Gating it
    /// as a write instead would refuse a group Viewer an answer it is
    /// entitled to, and would make "may I check before uploading" a stricter
    /// question than "may I download the bytes".
    ///
    /// TOUCHES NO OBJECT STORAGE. It is one indexed Postgres query; nothing
    /// here reaches S3, so a caller can ask cheaply and often.
    ///
    /// # Errors
    /// Returns `BlobError::Denied` when `owner` is not readable under `ctx`,
    /// `BlobError::State` when more than [`MAX_HELD_BLOB_DIGESTS`] digests
    /// are asked about, and `BlobError::Db` on a database fault.
    pub async fn find_held_blobs(
        &self,
        ctx: &AuthzContext,
        owner: OwnerId,
        content_hashes: &[[u8; 32]],
    ) -> Result<Vec<CitedBlobHeld>, BlobError> {
        ensure_owner_access(ctx, &owner)?;
        if content_hashes.len() > MAX_HELD_BLOB_DIGESTS {
            return Err(BlobError::State(format!(
                "at most {MAX_HELD_BLOB_DIGESTS} content hashes per call; got {}",
                content_hashes.len()
            )));
        }
        // Asking about nothing is answered, not refused. A caller cutting a
        // long digest list into batches reaches an empty tail naturally, and
        // "I hold none of the zero artefacts you named" is both the true
        // answer and the one that saves every such caller a guard.
        if content_hashes.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.begin_owner_tx(ctx.owner_scope()).await?;
        let result = find_held_blobs_on_connection(tx.as_mut(), &owner, content_hashes).await?;
        tx.commit().await.map_err(BlobError::Db)?;
        Ok(result)
    }
}
