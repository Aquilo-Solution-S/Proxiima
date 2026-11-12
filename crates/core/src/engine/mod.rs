//! Engine composite — wires `SpiiceRegistryFrozen` behind the typed
//! verb surfaces of docs/protocol.md.

mod access_admin;
mod builder;
mod embeddings;
pub(crate) mod errors;
mod goal_collaboration;
mod goal_write;
mod ingest;
mod memory_authoring;
mod owner_inverse;
mod pin_read;
mod pipeline;
mod query;
mod read_verbs;
mod retention;
mod retired_fact;
mod sidecar_heads;
mod source_cursors;
#[cfg(test)]
mod storage_port_tests;
pub(crate) mod unit_of_work;
mod upload;

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::authz::{EngineAuthority, EngineOperationAuthority, context_for_engine_operation};
use crate::embedding::EmbeddingRouter;
use crate::error::ProtocolError;
use crate::storage_ports::{EngineStoragePorts, OwnerWritePermit};
use crate::verbs::schema::SpiiceRegistryFrozen;

pub use crate::storage_ports::{GoalWorkCommand, HostStateCommand, HostStateOutcome};
pub use access_admin::GroupMemberPage;
pub use embeddings::EmbeddingDrainOutcome;
pub use goal_write::{
    GoalCreatePayloadWriteRequest, GoalDecomposeRequest, GoalMarkAchievedRequest,
    GoalModifyRequest, GoalTransitionRequest,
};
pub use memory_authoring::{
    DerivationIdentity, DerivedMemory, DerivedMemoryOutcome, MemoryTarget, SeriesHandle,
};
pub use pipeline::WritePermit;
pub use read_verbs::{
    FactCitationReadRequest, FactsCitingObjectReadRequest, GetGraphReadRequest,
    GetGraphReadResponse, GetMemoriesReadRequest, GetMemoriesReadResponse, GetMemoryReadRequest,
    GetMemoryReadResponse, SearchReadRequest, SearchReadResponse,
};
pub use retired_fact::RetiredFactErase;
pub use unit_of_work::{
    FactWrite, GoalWorkOutcome, GoalWorkUnitOfWork, HostStateMaintenanceAuthority,
    HostStateUnitOfWork, UnitOfWork,
};
pub use upload::{UploadCompleted, UploadCompletionExpectation};

pub struct Engine {
    registry: SpiiceRegistryFrozen,
    system_authority_binding: crate::authz::SystemAuthorityBinding,
    delegation_runtime_binding: crate::authz::DelegationRuntimeBinding,
    storage: EngineStoragePorts,
    embedding_router: Option<Arc<dyn EmbeddingRouter>>,
    embedding_runtime_policy: crate::embedding::EmbeddingRuntimePolicy,
    /// Shared original-client pause/retry state and observed owner routes.
    embedding_control: embeddings::EmbeddingControl,
    /// Producer identity and capture bounds for listenable Fact schemas
    /// (docs/host-integration.md). Default-constructed means "no source bound", which is
    /// correct for the overwhelming majority of deployments: nothing is
    /// listenable, so nothing needs one.
    publication: crate::publication::PublicationConfig,
    pub(crate) mcp_url: Arc<RwLock<Option<String>>>,
}

impl Engine {
    /// The store number carried by this deployment's memory UUIDs.
    #[must_use]
    pub fn store_number(&self) -> u16 {
        self.storage.query.memory_read.store_number()
    }

    pub(in crate::engine) fn operation_authority<'a, A>(
        &self,
        authority: &'a A,
    ) -> Result<EngineOperationAuthority<'a>, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let operation = context_for_engine_operation(authority)?;
        operation.validate_runtime_binding(Some(&self.delegation_runtime_binding))?;
        Ok(operation)
    }

    pub(in crate::engine) fn validate_write_permit(
        &self,
        permit: &OwnerWritePermit,
    ) -> Result<(), ProtocolError> {
        permit.validate_for_engine(&self.delegation_runtime_binding)
    }

    /// Storage handle, restricted to the engine module so the MCP tool
    /// layer cannot reach storage directly. Owner-scoped operations go
    /// through an engine verb that runs the authz pipeline.
    #[must_use]
    pub(in crate::engine) fn storage(&self) -> &EngineStoragePorts {
        &self.storage
    }

    /// The host's embedding router, if one is installed.
    #[must_use]
    pub fn embedding_router(&self) -> Option<&Arc<dyn EmbeddingRouter>> {
        self.embedding_router.as_ref()
    }

    #[must_use]
    pub const fn embedding_runtime_policy(&self) -> crate::embedding::EmbeddingRuntimePolicy {
        self.embedding_runtime_policy
    }

    /// Bound MCP URL once the host has recorded it with [`Engine::set_mcp_url`].
    /// `None` before that.
    #[must_use]
    pub fn mcp_url(&self) -> Option<String> {
        self.mcp_url.try_read().ok().and_then(|g| g.clone())
    }

    /// Record the bound MCP URL the host serves.
    pub async fn set_mcp_url(&self, url: String) {
        *self.mcp_url.write().await = Some(url);
    }
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("registry", &self.registry)
            .field("storage", &"<storage ports>")
            .finish_non_exhaustive()
    }
}
