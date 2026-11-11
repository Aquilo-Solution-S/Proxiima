//! Verified, in-process cited-blob byte reads.
//!
//! This capability is separate from [`super::CitedBlobPort`]. Host delivery
//! authorization and verified in-process reads share the same owner/Goal
//! boundary, while only verified reads collect bytes.

use std::num::NonZeroU64;
use std::sync::Arc;

use uuid::Uuid;

use super::cited_blob_delivery::{
    AdmittedCitedBlobDelivery, CitedBlobDeliveryPermit, CitedBlobDeliveryRequest,
    CitedBlobDeliveryTarget,
};
use crate::GoalHandle;
use crate::OwnerId;
use crate::authz::DelegationRuntimeBinding;
use crate::authz::{
    AuthzContext, DelegationRuntimeAuthority, EngineAuthority, context_for_engine_operation,
};

/// Which immutable property of a completed cited blob did not match its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CitedBlobIntegrityMismatch {
    ByteLength,
    ContentHash,
    Sha256,
}

/// Typed failure taxonomy for a verified cited-blob read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CitedBlobReadError {
    #[error("access denied")]
    AccessDenied,
    #[error("cited blob not found")]
    NotFound,
    #[error("cited blob is {byte_len} bytes; caller ceiling is {max_bytes}")]
    TooLarge { byte_len: u64, max_bytes: u64 },
    #[error("cited blob backend unavailable: {0}")]
    Unavailable(String),
    #[error("cited blob integrity mismatch: {0:?}")]
    IntegrityMismatch(CitedBlobIntegrityMismatch),
}

/// Completed cited-blob metadata plus bytes verified against it.
///
/// No storage coordinate or presigned locator crosses this boundary. The byte
/// vector is bounded by the non-zero ceiling supplied to
/// [`CitedBlobReadPort::collect_verified`].
#[derive(Debug, PartialEq, Eq)]
pub struct VerifiedCitedBlob {
    pub cited_object_id: Uuid,
    pub content_hash: [u8; 32],
    pub sha256: [u8; 32],
    pub byte_len: u64,
    pub mime: String,
    pub filename: String,
    pub bytes: Vec<u8>,
}

/// Cited-blob read admission and bounded, integrity-checked byte reads.
#[async_trait::async_trait]
pub trait CitedBlobReadPort: Send + Sync {
    /// Resolve one host delivery request without reading bytes. Implementors
    /// must authorize and resolve the locator atomically with the route.
    /// A backend without a delivery adapter refuses by default.
    ///
    /// # Errors
    ///
    /// Returns `AccessDenied` for missing authority, `NotFound` for an absent
    /// or non-canonical object or publication, and `Unavailable` for storage
    /// failures. No object-storage request may be made by this method.
    async fn authorize_delivery(
        &self,
        _permit: CitedBlobDeliveryPermit<'_>,
    ) -> Result<CitedBlobDeliveryTarget, CitedBlobReadError> {
        Err(CitedBlobReadError::AccessDenied)
    }

    /// Collect one completed blob only after verifying its length, BLAKE3
    /// content hash, and SHA-256 digest.
    ///
    /// Implementations MUST authorize Fact-read access before reading a
    /// storage locator or making a network call. `max_bytes` is required and
    /// non-zero; an implementation MUST stop once the stream exceeds it and
    /// MUST NOT return partial bytes on any error.
    ///
    /// # Errors
    ///
    /// Returns [`CitedBlobReadError::AccessDenied`] before backend access,
    /// [`CitedBlobReadError::NotFound`] for an absent or non-canonical owner
    /// row, [`CitedBlobReadError::TooLarge`] before streaming when stored
    /// metadata exceeds `max_bytes`, [`CitedBlobReadError::Unavailable`] for
    /// backend faults, and [`CitedBlobReadError::IntegrityMismatch`] when the
    /// complete bytes disagree with immutable metadata.
    async fn collect_verified(
        &self,
        authz: &AuthzContext,
        owner: OwnerId,
        cited_object_id: Uuid,
        max_bytes: NonZeroU64,
    ) -> Result<VerifiedCitedBlob, CitedBlobReadError>;

    /// Collect one cited blob admitted through an exact Goal route. The
    /// backend must re-check the route and the Fact/blob link under the same
    /// owner-scoped transaction; this is not an owner-role conversion.
    async fn collect_verified_for_goal(
        &self,
        _authz: &AuthzContext,
        _owner: OwnerId,
        _goal: GoalHandle,
        _fact_memory: crate::MemoryId,
        _cited_object_id: Uuid,
        _max_bytes: NonZeroU64,
    ) -> Result<VerifiedCitedBlob, CitedBlobReadError> {
        Err(CitedBlobReadError::AccessDenied)
    }
}

/// Shared verified-read handle published through the composed spiice services.
#[derive(Clone)]
pub struct CitedBlobReadService {
    port: Arc<dyn CitedBlobReadPort>,
    runtime_binding: Option<DelegationRuntimeBinding>,
}

impl std::fmt::Debug for CitedBlobReadService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CitedBlobReadService")
            .field(&"<dyn CitedBlobReadPort>")
            .finish()
    }
}

impl CitedBlobReadService {
    #[cfg(any(test, feature = "testkit"))]
    #[doc(hidden)]
    #[must_use]
    pub fn backend_identity_for_tests(&self) -> *const () {
        Arc::as_ptr(&self.port).cast::<()>()
    }

    /// Build an unbound service for ordinary authenticated calls and test
    /// fakes. Redeemed phases are rejected by an unbound service.
    #[must_use]
    pub fn new(port: Arc<dyn CitedBlobReadPort>) -> Self {
        Self {
            port,
            runtime_binding: None,
        }
    }

    /// Runtime composition seam binding this service to the same booted
    /// Engine as delegated phases.
    #[doc(hidden)]
    #[must_use]
    pub fn new_runtime(
        port: Arc<dyn CitedBlobReadPort>,
        authority: &DelegationRuntimeAuthority,
    ) -> Self {
        Self {
            port,
            runtime_binding: Some(authority.binding()),
        }
    }

    pub(super) fn is_bound_to(&self, authority: &DelegationRuntimeAuthority) -> bool {
        self.runtime_binding.as_ref() == Some(&authority.binding())
    }

    /// Collect verified bytes after checking an ordinary or redeemed phase
    /// authority at operation start.
    ///
    /// # Errors
    ///
    /// Returns `AccessDenied` for an invalid/expired authority before the
    /// backend observes owner, object id, or byte ceiling; otherwise forwards
    /// the verified-read taxonomy.
    pub async fn collect_verified<A>(
        &self,
        authority: &A,
        owner: OwnerId,
        cited_object_id: Uuid,
        max_bytes: NonZeroU64,
    ) -> Result<VerifiedCitedBlob, CitedBlobReadError>
    where
        A: EngineAuthority + ?Sized,
    {
        let operation = context_for_engine_operation(authority)
            .map_err(|_| CitedBlobReadError::AccessDenied)?;
        operation
            .validate_runtime_binding(self.runtime_binding.as_ref())
            .map_err(|_| CitedBlobReadError::AccessDenied)?;
        self.port
            .collect_verified(operation.authz(), owner, cited_object_id, max_bytes)
            .await
    }

    /// Admit one host-managed delivery request. The returned decision is
    /// one-shot and contains no client-facing URL or serialized credential.
    /// Call again for every new request, including cache hits, retries and
    /// ranges; this is not a reusable authorization grant. The host supplies
    /// the delivery transport and keeps the target out of client responses.
    ///
    /// # Errors
    ///
    /// Returns `AccessDenied` before invoking the backend for an expired or
    /// invalid authority or a mismatched runtime binding. Otherwise returns
    /// the backend's read refusal without exposing a locator.
    pub(crate) async fn authorize_delivery<A>(
        &self,
        authority: &A,
        request: CitedBlobDeliveryRequest,
    ) -> Result<AdmittedCitedBlobDelivery, CitedBlobReadError>
    where
        A: EngineAuthority + ?Sized,
    {
        let operation = context_for_engine_operation(authority)
            .map_err(|_| CitedBlobReadError::AccessDenied)?;
        operation
            .validate_runtime_binding(self.runtime_binding.as_ref())
            .map_err(|_| CitedBlobReadError::AccessDenied)?;
        let scope = operation
            .authz()
            .owner_scope()
            .ok_or(CitedBlobReadError::AccessDenied)?;
        if scope.is_expired() {
            return Err(CitedBlobReadError::AccessDenied);
        }
        let target = self
            .port
            .authorize_delivery(CitedBlobDeliveryPermit::new(
                operation.authz(),
                request.clone(),
            ))
            .await?;
        Ok(AdmittedCitedBlobDelivery::new(request, target))
    }

    /// Collect a cited blob through an exact, currently visible Goal route.
    /// The storage backend performs the atomic route, Fact, and blob checks.
    ///
    /// # Errors
    ///
    /// Returns [`CitedBlobReadError::AccessDenied`] when the authority is not
    /// a bound Goal principal or the runtime binding does not match, and
    /// otherwise forwards the backend's verified-read taxonomy.
    pub async fn collect_verified_for_goal<A>(
        &self,
        authority: &A,
        owner: OwnerId,
        goal: GoalHandle,
        fact_memory: crate::MemoryId,
        cited_object_id: Uuid,
        max_bytes: NonZeroU64,
    ) -> Result<VerifiedCitedBlob, CitedBlobReadError>
    where
        A: EngineAuthority + ?Sized,
    {
        let operation = context_for_engine_operation(authority)
            .map_err(|_| CitedBlobReadError::AccessDenied)?;
        operation
            .validate_runtime_binding(self.runtime_binding.as_ref())
            .map_err(|_| CitedBlobReadError::AccessDenied)?;
        if operation
            .authz()
            .owner_scope()
            .and_then(|scope| scope.goal_principal())
            .is_none()
        {
            return Err(CitedBlobReadError::AccessDenied);
        }
        self.port
            .collect_verified_for_goal(
                operation.authz(),
                owner,
                goal,
                fact_memory,
                cited_object_id,
                max_bytes,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::authz::{AuthPath, DelegatedPhase, DelegationRuntimeBinding};
    use crate::{GroupId, OwnerId, Role, UserId};

    #[derive(Debug, Default)]
    struct RecordingRead {
        calls: AtomicUsize,
    }

    fn target() -> CitedBlobDeliveryTarget {
        CitedBlobDeliveryTarget {
            cited_object_id: Uuid::now_v7(),
            owner: OwnerId::from(UserId::new(Uuid::now_v7())),
            upload_id: Uuid::now_v7(),
            mounted_from_upload_id: None,
            object_store: crate::ObjectStoreId::operator("test-store").expect("store id"),
            bucket: "private-bucket".into(),
            object_key: "private/key".into(),
            content_hash: [1; 32],
            sha256: [2; 32],
            byte_len: 3,
            mime: "application/octet-stream".into(),
            filename: "one.bin".into(),
        }
    }

    #[async_trait::async_trait]
    impl CitedBlobReadPort for RecordingRead {
        async fn authorize_delivery(
            &self,
            _permit: CitedBlobDeliveryPermit<'_>,
        ) -> Result<CitedBlobDeliveryTarget, CitedBlobReadError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(target())
        }

        async fn collect_verified(
            &self,
            _authz: &AuthzContext,
            _owner: OwnerId,
            cited_object_id: Uuid,
            _max_bytes: NonZeroU64,
        ) -> Result<VerifiedCitedBlob, CitedBlobReadError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(VerifiedCitedBlob {
                cited_object_id,
                content_hash: [0; 32],
                sha256: [0; 32],
                byte_len: 1,
                mime: "application/octet-stream".into(),
                filename: "one.bin".into(),
                bytes: vec![0],
            })
        }
    }

    #[tokio::test]
    async fn foreign_phase_is_denied_before_verified_read_backend() {
        let backend = Arc::new(RecordingRead::default());
        let runtime_authority = DelegationRuntimeAuthority::new(DelegationRuntimeBinding::fresh());
        let service = CitedBlobReadService::new_runtime(backend.clone(), &runtime_authority);
        let subject = UserId::new(Uuid::now_v7());
        let owner = OwnerId::from(GroupId::new(Uuid::now_v7()));
        let expires_at = SystemTime::now() + Duration::from_secs(30);
        let raw = AuthzContext::for_subject_with_role(
            subject,
            [(owner, Role::editor())],
            AuthPath::Delegated,
        )
        .with_expires_at(Some(expires_at));
        let foreign_phase = DelegatedPhase::new(raw, expires_at, DelegationRuntimeBinding::fresh());

        let error = service
            .collect_verified(
                &foreign_phase,
                owner,
                Uuid::now_v7(),
                NonZeroU64::new(1).expect("non-zero byte ceiling"),
            )
            .await
            .expect_err("foreign phase must fail before backend access");

        assert_eq!(error, CitedBlobReadError::AccessDenied);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn matching_phase_reaches_verified_read_backend() {
        let backend = Arc::new(RecordingRead::default());
        let binding = DelegationRuntimeBinding::fresh();
        let runtime_authority = DelegationRuntimeAuthority::new(binding.clone());
        let service = CitedBlobReadService::new_runtime(backend.clone(), &runtime_authority);
        let subject = UserId::new(Uuid::now_v7());
        let owner = OwnerId::from(GroupId::new(Uuid::now_v7()));
        let expires_at = SystemTime::now() + Duration::from_secs(30);
        let raw = AuthzContext::for_subject_with_role(
            subject,
            [(owner, Role::viewer())],
            AuthPath::Delegated,
        )
        .with_expires_at(Some(expires_at));
        let phase = DelegatedPhase::new(raw, expires_at, binding);
        let cited_object_id = Uuid::now_v7();

        let blob = service
            .collect_verified(
                &phase,
                owner,
                cited_object_id,
                NonZeroU64::new(1).expect("non-zero byte ceiling"),
            )
            .await
            .expect("matching delegated runtime reaches backend");

        assert_eq!(blob.cited_object_id, cited_object_id);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn delivery_rejects_expired_and_raw_or_foreign_phases_before_backend() {
        let backend = Arc::new(RecordingRead::default());
        let binding = DelegationRuntimeBinding::fresh();
        let runtime_authority = DelegationRuntimeAuthority::new(binding.clone());
        let service = CitedBlobReadService::new_runtime(backend.clone(), &runtime_authority);
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let expired = crate::test_fixtures::authenticated_context(
            AuthzContext::single_owner(&owner, AuthPath::HostBearer)
                .with_expires_at(Some(SystemTime::UNIX_EPOCH)),
        );
        assert!(expired.owner_scope().expect("sealed scope").is_expired());
        let request = CitedBlobDeliveryRequest::Owner {
            owner,
            cited_object_id: Uuid::now_v7(),
        };
        assert_eq!(
            service
                .authorize_delivery(&expired, request.clone())
                .await
                .expect_err("expired"),
            CitedBlobReadError::AccessDenied
        );
        let raw = AuthzContext::single_owner(&owner, AuthPath::Delegated);
        assert_eq!(
            service
                .authorize_delivery(&raw, request.clone())
                .await
                .expect_err("raw delegated"),
            CitedBlobReadError::AccessDenied
        );
        let expires_at = SystemTime::now() + Duration::from_secs(30);
        let foreign = DelegatedPhase::new(
            AuthzContext::single_owner(&owner, AuthPath::Delegated)
                .with_expires_at(Some(expires_at)),
            expires_at,
            DelegationRuntimeBinding::fresh(),
        );
        assert_eq!(
            service
                .authorize_delivery(&foreign, request.clone())
                .await
                .expect_err("foreign"),
            CitedBlobReadError::AccessDenied
        );
        let past = SystemTime::UNIX_EPOCH;
        let expired_phase = DelegatedPhase::new(
            crate::test_fixtures::authenticated_context(
                AuthzContext::single_owner(&owner, AuthPath::Delegated).with_expires_at(Some(past)),
            ),
            past,
            binding,
        );
        assert_eq!(
            service
                .authorize_delivery(&expired_phase, request)
                .await
                .expect_err("expired phase"),
            CitedBlobReadError::AccessDenied
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn matching_delivery_preserves_owner_and_goal_requests() {
        let backend = Arc::new(RecordingRead::default());
        let service = CitedBlobReadService::new(backend.clone());
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let object = Uuid::now_v7();
        let authz = crate::test_fixtures::authenticated_context(AuthzContext::single_owner(
            &owner,
            AuthPath::HostBearer,
        ));
        let owner_request = CitedBlobDeliveryRequest::Owner {
            owner,
            cited_object_id: object,
        };
        let admitted = service
            .authorize_delivery(&authz, owner_request.clone())
            .await
            .expect("owner admission");
        let (request, admitted_target) = admitted.into_parts();
        assert_eq!(request, owner_request);
        assert_eq!(admitted_target.bucket, "private-bucket");
        assert_eq!(admitted_target.object_key, "private/key");
        assert!(!format!("{admitted_target:?}").contains("private/key"));

        let goal_request = CitedBlobDeliveryRequest::GoalPublication {
            owner,
            goal: GoalHandle::new(Uuid::now_v7()),
            fact_memory: crate::MemoryId::new(Uuid::now_v7()),
            cited_object_id: object,
        };
        let admitted = service
            .authorize_delivery(&authz, goal_request.clone())
            .await
            .expect("Goal admission");
        let (request, _) = admitted.into_parts();
        assert_eq!(request, goal_request);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn delivery_phase_requires_the_matching_runtime() {
        let backend = Arc::new(RecordingRead::default());
        let binding = DelegationRuntimeBinding::fresh();
        let runtime = DelegationRuntimeAuthority::new(binding.clone());
        let bound = CitedBlobReadService::new_runtime(backend.clone(), &runtime);
        let unbound = CitedBlobReadService::new(backend.clone());
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let expires = SystemTime::now() + Duration::from_secs(30);
        let phase = DelegatedPhase::new(
            crate::test_fixtures::authenticated_context(
                AuthzContext::single_owner(&owner, AuthPath::Delegated)
                    .with_expires_at(Some(expires)),
            ),
            expires,
            binding,
        );
        let request = CitedBlobDeliveryRequest::Owner {
            owner,
            cited_object_id: Uuid::now_v7(),
        };
        assert_eq!(
            unbound
                .authorize_delivery(&phase, request.clone())
                .await
                .expect_err("unbound runtime"),
            CitedBlobReadError::AccessDenied
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        let admission = bound
            .authorize_delivery(&phase, request.clone())
            .await
            .expect("matching runtime");
        assert_eq!(admission.into_parts().0, request);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }
    fn bound_delivery(
        read: CitedBlobReadService,
        key: &str,
        authority: &DelegationRuntimeAuthority,
    ) -> super::super::CitedBlobDeliveryService {
        super::super::CitedBlobDeliveryService::bind(
            read,
            &crate::SpiiceRegistry::new().freeze_or_panic_for_tests(),
            crate::ScopeKey::parse(key).expect("fixture operation"),
            authority,
        )
        .expect("registered read operation")
    }

    #[test]
    fn delivery_binding_refuses_absent_write_and_dispatcher_keys() {
        let authority = DelegationRuntimeAuthority::new(DelegationRuntimeBinding::fresh());
        let read =
            CitedBlobReadService::new_runtime(Arc::new(RecordingRead::default()), &authority);
        let registry = crate::SpiiceRegistry::new().freeze_or_panic_for_tests();
        for key in [
            "absent_download",
            "core_upload:read_url",
            "core_upload:prepare",
            "core_search_memories:typo",
            "core_upload",
            "resource:memory",
        ] {
            assert!(
                super::super::CitedBlobDeliveryService::bind(
                    read.clone(),
                    &registry,
                    crate::ScopeKey::parse(key).expect("valid scope syntax"),
                    &authority,
                )
                .is_err(),
                "{key}"
            );
        }
        let _ = bound_delivery(read, "core_membership:list_members", &authority);
    }

    #[tokio::test]
    async fn bound_delivery_checks_operation_scope_before_backend() {
        let backend = Arc::new(RecordingRead::default());
        let authority = DelegationRuntimeAuthority::new(DelegationRuntimeBinding::fresh());
        let service = bound_delivery(
            CitedBlobReadService::new_runtime(backend.clone(), &authority),
            "core_memory_spaces",
            &authority,
        );
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let authz = crate::test_fixtures::authenticated_context(AuthzContext::single_owner(
            &owner,
            AuthPath::HostBearer,
        ));
        let request = CitedBlobDeliveryRequest::Owner {
            owner,
            cited_object_id: Uuid::now_v7(),
        };
        let search_only = authz
            .clone()
            .with_tool_scope(crate::ToolScope::Palette(vec![
                crate::ScopeKey::parse("core_search_memories").unwrap(),
            ]));
        assert_eq!(
            service
                .authorize_delivery(&search_only, request.clone())
                .await
                .expect_err("unrelated search scope"),
            CitedBlobReadError::AccessDenied
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        let admitted = authz
            .with_tool_scope(crate::ToolScope::Palette(vec![
                crate::ScopeKey::parse("core_memory_spaces").unwrap(),
            ]))
            .with_served_surface(crate::ServedSurface::Palette(Vec::new()));
        let result = service
            .authorize_delivery(&admitted, request.clone())
            .await
            .expect("bound read keeps existing mandatory-read policy");
        assert_eq!(result.into_parts().0, request);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn bound_delivery_confines_redeemed_phase_to_its_operation() {
        let backend = Arc::new(RecordingRead::default());
        let binding = DelegationRuntimeBinding::fresh();
        let runtime = DelegationRuntimeAuthority::new(binding.clone());
        let service = bound_delivery(
            CitedBlobReadService::new_runtime(backend.clone(), &runtime),
            "core_memory_spaces",
            &runtime,
        );
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let expires = SystemTime::now() + Duration::from_secs(30);
        let context = crate::test_fixtures::authenticated_context(
            AuthzContext::single_owner(&owner, AuthPath::Delegated).with_expires_at(Some(expires)),
        );
        let unrelated = DelegatedPhase::new(
            context
                .clone()
                .with_tool_scope(crate::ToolScope::Palette(vec![
                    crate::ScopeKey::parse("core_search_memories").unwrap(),
                ])),
            expires,
            binding.clone(),
        );
        let request = CitedBlobDeliveryRequest::Owner {
            owner,
            cited_object_id: Uuid::now_v7(),
        };
        assert_eq!(
            service
                .authorize_delivery(&unrelated, request.clone())
                .await
                .expect_err("unrelated phase command"),
            CitedBlobReadError::AccessDenied
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        let matching = DelegatedPhase::new(
            context.with_tool_scope(crate::ToolScope::Palette(vec![
                crate::ScopeKey::parse("core_memory_spaces").unwrap(),
            ])),
            expires,
            binding,
        );
        assert_eq!(
            service
                .authorize_delivery(&matching, request.clone())
                .await
                .expect("matching phase command")
                .into_parts()
                .0,
            request
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn shared_read_service_cannot_bind_with_a_fabricated_boot() {
        let registry = crate::SpiiceRegistry::new().freeze_or_panic_for_tests();
        let (_, _, real_boot) = crate::Engine::new(registry.clone()).into_runtime_authorities();
        let (_, _, other_boot) = crate::Engine::new(registry.clone()).into_runtime_authorities();
        let backend = Arc::new(RecordingRead::default());
        let shared_read = CitedBlobReadService::new_runtime(backend.clone(), &real_boot);
        let key = crate::ScopeKey::parse("core_memory_spaces").expect("registered read");
        assert!(matches!(
            super::super::CitedBlobDeliveryService::bind(
                shared_read.clone(),
                &registry,
                key.clone(),
                &other_boot
            ),
            Err(super::super::CitedBlobDeliveryBindingError::RuntimeMismatch)
        ));
        assert!(matches!(
            super::super::CitedBlobDeliveryService::bind(
                CitedBlobReadService::new(backend.clone()),
                &registry,
                key.clone(),
                &real_boot
            ),
            Err(super::super::CitedBlobDeliveryBindingError::RuntimeMismatch)
        ));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        let permitted =
            super::super::CitedBlobDeliveryService::bind(shared_read, &registry, key, &real_boot)
                .expect("matching boot");
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let authz = crate::test_fixtures::authenticated_context(AuthzContext::single_owner(
            &owner,
            AuthPath::HostBearer,
        ));
        let request = CitedBlobDeliveryRequest::Owner {
            owner,
            cited_object_id: Uuid::now_v7(),
        };
        assert_eq!(
            permitted
                .authorize_delivery(&authz, request.clone())
                .await
                .expect("real host admission")
                .into_parts()
                .0,
            request
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }
}
