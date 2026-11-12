use proxiima_core::StorageError;
use proxiima_core::storage_ports::FactIngestPort;
use proxiima_core::verbs::fact_ingest::{AuthorizedFactWrite, FactIngestOutcome};

use crate::error::with_bounded_retry;
use crate::{PgStorage, verbs};

#[async_trait::async_trait]
impl FactIngestPort for PgStorage {
    async fn ingest_authorized_fact_atomic(
        &self,
        authorized: &AuthorizedFactWrite,
        embedding_spaces: &[proxiima_core::EmbeddingSpace],
    ) -> Result<FactIngestOutcome, StorageError> {
        if !authorized.sidecar_payloads().is_empty() {
            return Err(StorageError::ConstraintViolation(
                "typed Fact sidecars require the typed persistence path".into(),
            ));
        }
        verbs::fact_ingest::ingest_authorized_fact_atomic(
            &self.pool,
            authorized,
            embedding_spaces,
            self.store_number,
        )
        .await
    }

    async fn ingest_authorized_fact(
        &self,
        authorized: &AuthorizedFactWrite,
        embedding_spaces: &[proxiima_core::EmbeddingSpace],
    ) -> Result<FactIngestOutcome, StorageError> {
        let sidecar_payloads = authorized.sidecar_payloads();
        let natural_key = verbs::fact_ingest::fact_natural_key(
            authorized.draft(),
            authorized.fact_sidecar_table(),
            authorized.fact_natural_key_columns(),
            authorized.fact_natural_key_values(),
            authorized.reobserves_displaced(),
            sidecar_payloads,
            &self.sidecars,
        )?;
        // Retry the whole begin→body→commit on transient deadlock/
        // serialization. The typed sidecars are data (`SidecarPayload`), so
        // each attempt re-clones them and rebuilds the insert closure — unlike
        // an `FnOnce` closure, this is safely re-runnable.
        with_bounded_retry(move || {
            let natural_key = natural_key.clone();
            let fact_sidecars = self.sidecars.writing(authorized.draft());
            let payloads = sidecar_payloads.to_vec();
            async move {
                let mut tx = crate::owner_scope::begin_owner_transaction(
                    &self.pool,
                    crate::owner_scope::require_owner_scope(
                        authorized.owner_write_permit().owner_scope(),
                    )?,
                )
                .await?;
                let tables = self.sidecars.tables_for_payloads(&payloads)?;
                let scopes = self.scopes.targets_for_payloads(&payloads)?;
                let outcome = verbs::fact_ingest::ingest_authorized_fact_in_tx(
                    &mut tx,
                    &self.sidecars,
                    authorized,
                    embedding_spaces,
                    verbs::fact_ingest::FactAdmissionInput {
                        store_number: self.store_number,
                        natural_key: natural_key.as_ref(),
                        sidecar_tables: &tables,
                        scopes: &scopes,
                        publication: authorized.publication(),
                    },
                    move |tx, outcome| {
                        Box::pin(async move {
                            for payload in &payloads {
                                fact_sidecars
                                    .insert_memory_sidecar(tx, outcome.memory_id, payload)
                                    .await?;
                            }

                            Ok(())
                        })
                    },
                )
                .await?;
                crate::write_wake::commit(tx).await?;
                Ok(outcome)
            }
        })
        .await
    }
}
