use crate::storage::StorageError;
#[cfg(any(test, feature = "testkit"))]
use crate::storage_ports::OwnerWritePermit;
#[cfg(any(test, feature = "testkit"))]
use crate::verbs::fact_ingest::FactWriteCommand;
use crate::verbs::fact_ingest::{AuthorizedFactWrite, FactIngestOutcome};

#[async_trait::async_trait]
pub trait FactIngestPort: Send + Sync {
    async fn ingest_authorized_fact_atomic(
        &self,
        authorized: &AuthorizedFactWrite,
        embedding_spaces: &[crate::EmbeddingSpace],
    ) -> Result<FactIngestOutcome, StorageError>;

    /// Compatibility adapter for storage fixtures. Production callers must
    /// use the Engine-minted authorized carrier above. A fixture model id
    /// names a 1024-wide space.
    #[cfg(any(test, feature = "testkit"))]
    async fn ingest_fact_atomic(
        &self,
        permit: &OwnerWritePermit,
        draft: &FactWriteCommand,
        embedding_model_id: Option<&str>,
    ) -> Result<FactIngestOutcome, StorageError> {
        let permit = OwnerWritePermit::new(
            *permit.owner(),
            permit.authority(),
            permit.owner_scope().cloned(),
            permit.reads().clone(),
        );
        let authorized =
            AuthorizedFactWrite::new_for_tests(permit, draft.clone(), None, Vec::new());
        let spaces: Vec<crate::EmbeddingSpace> = embedding_model_id
            .map(|model_id| crate::EmbeddingSpace::new(model_id, crate::EmbeddingDim::D1024))
            .into_iter()
            .collect();
        self.ingest_authorized_fact_atomic(&authorized, &spaces)
            .await
    }

    /// Persist an authorized Fact together with every typed sidecar row it
    /// carries and the citation its authorization settled, in ONE
    /// transaction.
    ///
    /// **Citation.** [`AuthorizedCitation::None`] resolves the draft's own
    /// citation hint; [`AuthorizedCitation::Inline`] upserts the cited
    /// object and, when the Engine's upload verb named an upload, completes
    /// that upload row in the same transaction;
    /// [`AuthorizedCitation::Reference`] verifies the stored object
    /// (existence, owner) in the transaction that writes the Fact.
    ///
    /// The authorized wrapper owns the payload slice because a Fact may be
    /// extended: the substrate owns the Fact and its own sidecar, and a
    /// spiice may add further rows of its own against the same `memory_id` —
    /// extra columns on an event the substrate defines, without the spiice
    /// having to own the event.
    ///
    /// **Destination is resolved per bound payload, never positionally.** Storage
    /// routes each payload by its own `(kind, schema_id, schema_version)`
    /// through the sidecar registry, and an unregistered schema is a
    /// `ConstraintViolation`. Two payloads therefore cannot be transposed
    /// into each other's tables by an ordering slip, and a spiice cannot
    /// name a destination it has not registered. The slice's ORDER is the
    /// insert order and nothing else.
    ///
    /// **All or nothing.** Every payload lands in the same transaction as
    /// the Fact row; one failure rolls back the Fact too. This is the whole
    /// reason extension is expressed as data handed to storage rather than
    /// as a callback invoked mid-transaction: storage keeps sole authority
    /// over the transaction, so an extension can only ADD rows it has
    /// registered — it cannot reach the Fact row, and there is no handle to
    /// misuse.
    ///
    /// Bound payloads are DATA, not closures, so the bounded retry around
    /// begin→body→commit can rebuild the work on every attempt.
    ///
    /// Note what is NOT widened: `AuthorizedFactWrite::fact_sidecar_table`
    /// and its natural-key columns still come from the Fact's OWN registered
    /// schema, because a stateful Fact's identity belongs to the event, not
    /// to whatever a spiice chose to staple onto it.
    ///
    /// # Errors
    ///
    /// `ConstraintViolation` when any payload's schema is not registered as
    /// a memory sidecar, or when a referenced cited object is missing for
    /// the Fact's owner; otherwise storage faults.
    ///
    /// [`AuthorizedCitation::None`]: crate::verbs::fact_ingest::AuthorizedCitation::None
    /// [`AuthorizedCitation::Inline`]: crate::verbs::fact_ingest::AuthorizedCitation::Inline
    /// [`AuthorizedCitation::Reference`]: crate::verbs::fact_ingest::AuthorizedCitation::Reference
    async fn ingest_authorized_fact(
        &self,
        authorized: &AuthorizedFactWrite,
        embedding_spaces: &[crate::EmbeddingSpace],
    ) -> Result<FactIngestOutcome, StorageError>;
}
