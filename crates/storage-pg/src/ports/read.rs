use proxiima_core::read_models::MemorySchemaSpec;
use proxiima_core::storage_ports::CitationPort;
use proxiima_core::verbs::query::FactCitations;
use proxiima_core::{MemoryId, OwnerId, StorageError};

use crate::{PgStorage, verbs};

#[async_trait::async_trait]
impl CitationPort for PgStorage {
    async fn facts_citing_object(
        &self,
        owner_scope: Option<&proxiima_core::OwnerScope>,
        read_owners: &[OwnerId],
        cited_object_id: uuid::Uuid,
        schemas: &[MemorySchemaSpec],
        after: Option<proxiima_core::verbs::query::FactCitationCursor>,
        limit: u32,
    ) -> Result<proxiima_core::verbs::query::FactCitationPage, StorageError> {
        let mut tx = crate::owner_scope::begin_owner_transaction(
            &self.pool,
            crate::owner_scope::require_owner_scope(owner_scope)?,
        )
        .await?;
        let result = async {
            verbs::query::facts_citing_object_on_connection(
                &mut tx,
                &self.sidecars,
                read_owners,
                cited_object_id,
                schemas,
                after,
                limit,
            )
            .await
        }
        .await;
        crate::owner_scope::finish_transaction(tx, result).await
    }

    async fn citations_of_facts(
        &self,
        owner_scope: Option<&proxiima_core::OwnerScope>,
        read_owners: &[OwnerId],
        fact_memory_ids: &[MemoryId],
    ) -> Result<FactCitations, StorageError> {
        let mut tx = crate::owner_scope::begin_owner_transaction(
            &self.pool,
            crate::owner_scope::require_owner_scope(owner_scope)?,
        )
        .await?;
        let result = async {
            verbs::query::citations_of_facts_on_connection(&mut tx, read_owners, fact_memory_ids)
                .await
        }
        .await;
        crate::owner_scope::finish_transaction(tx, result).await
    }
}
