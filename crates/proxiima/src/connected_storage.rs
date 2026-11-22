//! The storage-only facade used by one-shot commands.

use proxiima_blob_s3::{CitedBlobStore, UploadExpirySweepOutcome};
use proxiima_core::{
    EmbeddingAnnObservability, EmbeddingOrphanSweepOutcome, StorageError, authz::SystemAuthority,
};
use proxiima_storage_pg::{
    ColdPurgeRetryOptions, ColdPurgeRetryOutcome, EmbeddingMaintenanceLock,
    EmbeddingReconcileOptions, EmbeddingReconcileOutcome, PgStorage, StorageMaintenanceLock,
};

use crate::HostOwnerTransactions;

/// Validated storage for one-shot maintenance commands and extra-table adapters.
///
/// ```compile_fail,E0599
/// fn pool(storage: &proxiima::ConnectedStorage) {
///     let _ = storage.clone_pool_for_backend();
/// }
/// ```
#[derive(Debug)]
pub struct ConnectedStorage(pub(crate) PgStorage);

impl ConnectedStorage {
    /// Open transactions for the authenticated caller and served owner.
    #[must_use]
    pub fn owner_transactions_for_host(&self) -> HostOwnerTransactions {
        HostOwnerTransactions::new(self.0.owner_transactions())
    }

    /// Take the embedding maintenance lease, or return `None` if held.
    ///
    /// # Errors
    /// Returns storage errors from the lease transaction.
    pub async fn try_embedding_maintenance_lock(
        &self,
    ) -> Result<Option<EmbeddingMaintenanceLock>, StorageError> {
        self.0.try_embedding_maintenance_lock().await
    }

    /// Sweep orphan embedding rows and repair divergent vector-index heads.
    ///
    /// # Errors
    /// Returns storage errors from the sweep.
    pub async fn sweep_orphan_embedding_rows(
        &self,
    ) -> Result<EmbeddingOrphanSweepOutcome, StorageError> {
        self.0.sweep_orphan_embedding_rows().await
    }

    /// Enqueue missing embeddings under the supplied maintenance options.
    ///
    /// # Errors
    /// Returns storage errors from reconciliation.
    pub async fn reconcile_embeddings(
        &self,
        options: EmbeddingReconcileOptions<'_>,
    ) -> Result<EmbeddingReconcileOutcome, StorageError> {
        self.0.reconcile_embeddings(options).await
    }

    /// Read embedding backlog, orphan counts and the recall canary.
    ///
    /// # Errors
    /// Returns storage errors from the observability reads.
    pub async fn embedding_ann_observability(
        &self,
    ) -> Result<EmbeddingAnnObservability, StorageError> {
        self.0.embedding_ann_observability().await
    }

    /// Take the storage maintenance lease, or return `None` if held.
    ///
    /// # Errors
    /// Returns storage errors from the lease transaction.
    pub async fn try_storage_maintenance_lock(
        &self,
    ) -> Result<Option<StorageMaintenanceLock>, StorageError> {
        self.0.try_storage_maintenance_lock().await
    }

    /// Drain a bounded batch of due purge debts for the wired object store.
    ///
    /// # Errors
    /// Returns storage errors from the drain or a refusal for invalid options.
    pub async fn retry_cold_object_purges(
        &self,
        options: ColdPurgeRetryOptions,
    ) -> Result<ColdPurgeRetryOutcome, StorageError> {
        self.0.retry_cold_object_purges(options).await
    }

    /// Take the backend storage for a fixture that needs its pool or engine ports.
    #[cfg(feature = "testkit")]
    #[must_use]
    pub fn into_pg_storage_for_tests(self) -> PgStorage {
        self.0
    }
}

/// An expired-upload sweep bound to the object store of a storage command.
#[derive(Debug)]
pub struct StorageUploadSweeper {
    pub(crate) blobs: CitedBlobStore,
    pub(crate) authority: SystemAuthority,
}

impl StorageUploadSweeper {
    /// Expire up to `limit` uploads. Failed purges remain owed to the drain.
    ///
    /// # Errors
    /// Returns storage errors while finding or expiring uploads.
    pub async fn sweep_expired_uploads(
        &self,
        limit: u32,
    ) -> Result<UploadExpirySweepOutcome, StorageError> {
        self.blobs
            .sweep_expired_uploads(&self.authority, limit)
            .await
    }
}
