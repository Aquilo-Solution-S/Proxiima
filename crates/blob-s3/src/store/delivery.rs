//! Shared cited-blob read authorization and host delivery target resolution.

use proxiima_core::storage_ports::{
    CitedBlobDeliveryRequest, CitedBlobDeliveryTarget, CitedBlobReadError,
};
use proxiima_core::{AuthzContext, OwnerId};
use proxiima_storage_pg::begin_owner_transaction;

use super::CitedBlobStore;
use super::guards::ensure_owner_access;
use super::rows::{
    BlobReadRecord, load_blob_read_record_on_connection, load_goal_blob_read_record_on_connection,
};

impl CitedBlobStore {
    /// Resolve authority and the exact upload in one owner-scoped transaction.
    /// Both host delivery and verified byte reads enter through this boundary.
    pub(super) async fn authorize_read_record(
        &self,
        authz: &AuthzContext,
        request: CitedBlobDeliveryRequest,
    ) -> Result<(OwnerId, BlobReadRecord), CitedBlobReadError> {
        let scope = authz
            .owner_scope()
            .ok_or(CitedBlobReadError::AccessDenied)?;
        let owner = match &request {
            CitedBlobDeliveryRequest::Owner { owner, .. } => {
                ensure_owner_access(authz, owner).map_err(|_| CitedBlobReadError::AccessDenied)?;
                *owner
            }
            CitedBlobDeliveryRequest::GoalPublication { owner, .. } => {
                if scope.goal_principal().is_none() {
                    return Err(CitedBlobReadError::AccessDenied);
                }
                *owner
            }
        };
        let mut tx = begin_owner_transaction(&self.pool, scope)
            .await
            .map_err(|error| CitedBlobReadError::Unavailable(error.to_string()))?;
        let row = match request {
            CitedBlobDeliveryRequest::Owner {
                cited_object_id, ..
            } => load_blob_read_record_on_connection(tx.as_mut(), &owner, cited_object_id).await,
            CitedBlobDeliveryRequest::GoalPublication {
                goal,
                fact_memory,
                cited_object_id,
                ..
            } => {
                load_goal_blob_read_record_on_connection(
                    tx.as_mut(),
                    &owner,
                    goal,
                    fact_memory,
                    cited_object_id,
                )
                .await
            }
        }
        .map_err(|error| CitedBlobReadError::Unavailable(error.to_string()))?
        .ok_or(CitedBlobReadError::NotFound)?;
        tx.commit()
            .await
            .map_err(|error| CitedBlobReadError::Unavailable(error.to_string()))?;
        // Locator rows are external data. Refuse another store's coordinates
        // before handing them to an adapter or constructing an object client.
        if !self.mints_locator(
            &row.object_store,
            &row.bucket,
            &row.object_key,
            row.upload_id,
            row.mounted_from_upload_id,
        ) {
            return Err(CitedBlobReadError::NotFound);
        }
        Ok((owner, row))
    }

    pub(super) async fn authorize_delivery_record(
        &self,
        authz: &AuthzContext,
        request: CitedBlobDeliveryRequest,
    ) -> Result<CitedBlobDeliveryTarget, CitedBlobReadError> {
        let (owner, row) = self.authorize_read_record(authz, request).await?;
        Ok(CitedBlobDeliveryTarget {
            cited_object_id: row.cited_object_id,
            owner,
            upload_id: row.upload_id,
            mounted_from_upload_id: row.mounted_from_upload_id,
            object_store: self.store_id().clone(),
            bucket: row.bucket,
            object_key: row.object_key,
            content_hash: row.content_hash,
            sha256: row.sha256,
            byte_len: row.byte_len,
            mime: row.mime,
            filename: row.filename,
        })
    }
}
