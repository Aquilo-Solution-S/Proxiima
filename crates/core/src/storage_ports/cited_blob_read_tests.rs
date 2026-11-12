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
    let foreign_phase = DelegatedPhase::new(raw, DelegationRuntimeBinding::fresh());

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
    let phase = DelegatedPhase::new(raw, binding);
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
        AuthzContext::single_owner(&owner, AuthPath::Delegated).with_expires_at(Some(expires_at)),
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
            AuthzContext::single_owner(&owner, AuthPath::Delegated).with_expires_at(Some(expires)),
        ),
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
    let read = CitedBlobReadService::new_runtime(Arc::new(RecordingRead::default()), &authority);
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
    let (_, _, real_boot) =
        crate::Engine::new(registry.clone(), crate::StoragePorts::rejecting_for_tests())
            .into_runtime_authorities();
    let (_, _, other_boot) =
        crate::Engine::new(registry.clone(), crate::StoragePorts::rejecting_for_tests())
            .into_runtime_authorities();
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
