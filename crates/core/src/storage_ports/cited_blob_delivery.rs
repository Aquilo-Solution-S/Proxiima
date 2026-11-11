//! Host-only authorization results for cited-blob delivery.

use uuid::Uuid;

use crate::authz::KeyDeclaration;
use crate::authz::{
    DelegationRuntimeAuthority, EngineAuthority, ScopeKey, context_for_engine_operation,
};
use crate::{GoalHandle, MemoryId, ObjectStoreId, OwnerId, SpiiceRegistryFrozen};

use super::cited_blob_read::{CitedBlobReadError, CitedBlobReadService};

/// Select a delivery read route. Goal requests retain the exact
/// publication route rather than being reduced to owner access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CitedBlobDeliveryRequest {
    Owner {
        owner: OwnerId,
        cited_object_id: Uuid,
    },
    GoalPublication {
        owner: OwnerId,
        goal: GoalHandle,
        fact_memory: MemoryId,
        cited_object_id: Uuid,
    },
}

/// Backend-only proof that the shared read service performed its authority
/// checks for this exact request. Its constructor is private (`pub(super)`).
///
/// ```compile_fail
/// use proxiima_core::storage_ports::{CitedBlobDeliveryPermit, CitedBlobDeliveryRequest};
/// fn forge(authz: &proxiima_core::AuthzContext, request: CitedBlobDeliveryRequest) {
///     let _ = CitedBlobDeliveryPermit::new(authz, request);
/// }
/// ```
#[derive(Debug)]
pub struct CitedBlobDeliveryPermit<'a> {
    authz: &'a crate::AuthzContext,
    request: CitedBlobDeliveryRequest,
}

impl<'a> CitedBlobDeliveryPermit<'a> {
    pub(super) fn new(authz: &'a crate::AuthzContext, request: CitedBlobDeliveryRequest) -> Self {
        Self { authz, request }
    }

    /// Consume the permit into the already-checked authority and request.
    #[must_use]
    pub fn into_parts(self) -> (&'a crate::AuthzContext, CitedBlobDeliveryRequest) {
        (self.authz, self.request)
    }
}

/// Why a host could not bind delivery to its declared read operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CitedBlobDeliveryBindingError {
    #[error("delivery operation {operation} is not a registered read tool or action")]
    NotRegisteredRead { operation: ScopeKey },
    #[error("delivery read service is not bound to the supplied runtime authority")]
    RuntimeMismatch,
}

/// Host-owned delivery authorization bound immutably to one registered read
/// operation. The host supplies the delivery transport after admission.
pub struct CitedBlobDeliveryService {
    read: CitedBlobReadService,
    operation: ScopeKey,
    declaration: KeyDeclaration,
}

impl std::fmt::Debug for CitedBlobDeliveryService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CitedBlobDeliveryService")
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

impl CitedBlobDeliveryService {
    /// Bind delivery to an existing registered read operation.
    ///
    /// The consuming host selects an operation whose contract includes byte
    /// delivery, once when constructing its route. That fixed catalog
    /// declaration governs every subsequent call; request input selects no key.
    ///
    /// # Errors
    ///
    /// Returns `NotRegisteredRead` for an absent, write, resource, or bare
    /// dispatcher key; dispatchers require an exact registered read action.
    /// Returns `RuntimeMismatch` if the uncloneable boot witness does not
    /// belong to this read service, including an unbound service. The witness
    /// is retained by host runtime handles, never shared spiice services.
    pub fn bind(
        read: CitedBlobReadService,
        registry: &SpiiceRegistryFrozen,
        operation: ScopeKey,
        authority: &DelegationRuntimeAuthority,
    ) -> Result<Self, CitedBlobDeliveryBindingError> {
        if !read.is_bound_to(authority) {
            return Err(CitedBlobDeliveryBindingError::RuntimeMismatch);
        }
        let descriptor = match &operation {
            ScopeKey::Tool(tool) | ScopeKey::Action { tool, .. } => {
                registry.mcp_tool(tool.as_str())
            }
            ScopeKey::Resource(_) => None,
        };
        let declaration = descriptor
            .filter(|descriptor| descriptor.palette_keys().contains(&operation))
            .map(|descriptor| match &operation {
                ScopeKey::Tool(_) => descriptor.declaration(None),
                ScopeKey::Action { action, .. } => descriptor.declaration(Some(action.as_str())),
                ScopeKey::Resource(_) => KeyDeclaration::UNDECLARED,
            });
        let Some(declaration) = declaration.filter(|declaration| declaration.is_read()) else {
            return Err(CitedBlobDeliveryBindingError::NotRegisteredRead { operation });
        };
        Ok(Self {
            read,
            operation,
            declaration,
        })
    }

    /// Re-authorize one request under the bound operation before resolving a
    /// host delivery target. Call once for every new request.
    ///
    /// # Errors
    ///
    /// Returns `AccessDenied` before locator lookup for an excluded operation,
    /// expired authority or a mismatched runtime. Otherwise forwards the
    /// backend read refusal without exposing a target.
    pub async fn authorize_delivery<A>(
        &self,
        authority: &A,
        request: CitedBlobDeliveryRequest,
    ) -> Result<AdmittedCitedBlobDelivery, CitedBlobReadError>
    where
        A: EngineAuthority + ?Sized,
    {
        let operation = context_for_engine_operation(authority)
            .map_err(|_| CitedBlobReadError::AccessDenied)?;
        if !operation
            .authz()
            .capabilities()
            .admits(&self.operation, self.declaration)
        {
            return Err(CitedBlobReadError::AccessDenied);
        }
        self.read.authorize_delivery(authority, request).await
    }
}

/// Backend locator and immutable metadata for a trusted host delivery adapter.
///
/// This is deliberately not serializable and its debug output never includes
/// the locator. It is a host boundary, not a client download capability.
///
/// ```compile_fail
/// fn expose(target: proxiima_core::storage_ports::CitedBlobDeliveryTarget) {
///     let _ = serde_json::to_value(target);
/// }
/// ```
pub struct CitedBlobDeliveryTarget {
    pub cited_object_id: Uuid,
    pub owner: OwnerId,
    pub upload_id: Uuid,
    pub mounted_from_upload_id: Option<Uuid>,
    pub object_store: ObjectStoreId,
    pub bucket: String,
    pub object_key: String,
    pub content_hash: [u8; 32],
    pub sha256: [u8; 32],
    pub byte_len: u64,
    pub mime: String,
    pub filename: String,
}

impl std::fmt::Debug for CitedBlobDeliveryTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CitedBlobDeliveryTarget")
            .field("cited_object_id", &self.cited_object_id)
            .field("owner", &self.owner)
            .field("upload_id", &self.upload_id)
            .field("mounted_from_upload_id", &self.mounted_from_upload_id)
            .field("object_store", &"<redacted>")
            .field("byte_len", &self.byte_len)
            .field("mime", &self.mime)
            .field("filename", &self.filename)
            .field("locator", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// A single authorization decision, consumable by the trusted host adapter.
/// There is no public constructor, `Clone`, or serialization implementation.
///
/// ```
/// use proxiima_core::storage_ports::{AdmittedCitedBlobDelivery, CitedBlobDeliveryRequest, CitedBlobDeliveryTarget};
/// fn deliver(admission: AdmittedCitedBlobDelivery) -> (CitedBlobDeliveryRequest, CitedBlobDeliveryTarget) {
///     admission.into_parts()
/// }
/// ```
///
/// ```compile_fail
/// fn reuse(admission: proxiima_core::storage_ports::AdmittedCitedBlobDelivery) {
///     let _ = admission.clone();
/// }
/// ```
///
/// ```compile_fail
/// use proxiima_core::storage_ports::{AdmittedCitedBlobDelivery, CitedBlobDeliveryRequest, CitedBlobDeliveryTarget};
/// fn forge(request: CitedBlobDeliveryRequest, target: CitedBlobDeliveryTarget) -> AdmittedCitedBlobDelivery {
///     AdmittedCitedBlobDelivery::new(request, target)
/// }
/// ```
#[must_use = "consume this request admission in the trusted delivery adapter"]
pub struct AdmittedCitedBlobDelivery {
    request: CitedBlobDeliveryRequest,
    target: CitedBlobDeliveryTarget,
}

impl std::fmt::Debug for AdmittedCitedBlobDelivery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdmittedCitedBlobDelivery")
            .field("request", &self.request)
            .field("target", &self.target)
            .finish()
    }
}

impl AdmittedCitedBlobDelivery {
    pub(crate) fn new(request: CitedBlobDeliveryRequest, target: CitedBlobDeliveryTarget) -> Self {
        Self { request, target }
    }

    /// Consume this one-shot admission into the host's delivery parts.
    #[must_use]
    pub fn into_parts(self) -> (CitedBlobDeliveryRequest, CitedBlobDeliveryTarget) {
        (self.request, self.target)
    }
}
