use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use super::Engine;
use crate::access::AccessError;
use crate::authz::{AuthPath, AuthzContext};
use crate::owner_inverse::{
    EraseAuthorization, ExportAuthorization, OwnerEraseCounts, OwnerEraseOutcome,
    OwnerEraseRefusal, OwnerExportBundle,
};
use crate::storage::StorageError;
use crate::storage_ports::{
    OwnerDropProofPort, OwnerEraseAuthorityPort, OwnerInversePort, StoragePorts,
};
use crate::{GroupId, OwnerId, SourceId, UserId};

#[derive(Debug, Default)]
struct PermissiveAdmin {
    calls: AtomicUsize,
}

#[async_trait]
impl OwnerEraseAuthorityPort for PermissiveAdmin {
    async fn may_erase_owner(
        &self,
        _authz: &AuthzContext,
        _target: &crate::owner_inverse::OwnerEraseTarget,
    ) -> Result<bool, AccessError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }

    async fn may_perform_operator_maintenance(
        &self,
        _authz: &AuthzContext,
    ) -> Result<bool, AccessError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
}

/// `OwnerInversePort` whose erase verbs return a fixed outcome.
#[derive(Debug)]
struct FixedOutcomeErase {
    outcome: OwnerEraseOutcome,
    /// Bumped by every erase verb, so a test can assert an attempt was
    /// turned away before storage rather than merely reporting a refusal.
    erase_calls: AtomicUsize,
}

impl FixedOutcomeErase {
    fn completed() -> Self {
        Self::completed_outcome()
    }

    fn completed_with_cold_pending() -> Self {
        Self {
            outcome: OwnerEraseOutcome::Completed {
                operation_id: uuid::Uuid::now_v7(),
                counts: OwnerEraseCounts::default(),
                host_state_deleted: std::collections::BTreeMap::default(),
                host_state_scrubbed: std::collections::BTreeMap::default(),
                cold_object_purge_pending: true,
            },
            erase_calls: AtomicUsize::new(0),
        }
    }

    fn completed_outcome() -> Self {
        Self {
            outcome: OwnerEraseOutcome::Completed {
                operation_id: uuid::Uuid::now_v7(),
                counts: OwnerEraseCounts::default(),
                host_state_deleted: std::collections::BTreeMap::default(),
                host_state_scrubbed: std::collections::BTreeMap::default(),
                cold_object_purge_pending: false,
            },
            erase_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl OwnerInversePort for FixedOutcomeErase {
    async fn erase_group_owner(
        &self,
        _auth: &EraseAuthorization,
        _group_id: GroupId,
        _tables: &crate::owner_inverse::OwnerSurfaces,
    ) -> Result<OwnerEraseOutcome, StorageError> {
        self.erase_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.outcome.clone())
    }

    async fn erase_personal_owner(
        &self,
        _auth: &EraseAuthorization,
        _user_id: UserId,
        _tables: &crate::owner_inverse::OwnerSurfaces,
    ) -> Result<OwnerEraseOutcome, StorageError> {
        self.erase_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.outcome.clone())
    }

    async fn erase_group_source_scope(
        &self,
        _auth: &EraseAuthorization,
        _group_id: GroupId,
        _source_id: &SourceId,
        _tables: &crate::owner_inverse::OwnerSurfaces,
    ) -> Result<OwnerEraseOutcome, StorageError> {
        self.erase_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.outcome.clone())
    }

    async fn erase_personal_source_scope(
        &self,
        _auth: &EraseAuthorization,
        _user_id: UserId,
        _source_id: &SourceId,
        _tables: &crate::owner_inverse::OwnerSurfaces,
    ) -> Result<OwnerEraseOutcome, StorageError> {
        self.erase_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.outcome.clone())
    }

    async fn export_owner_bundle(
        &self,
        _auth: &ExportAuthorization,
        _tables: &crate::owner_inverse::OwnerSurfaces,
    ) -> Result<OwnerExportBundle, StorageError> {
        Err(StorageError::Internal("export not used in test".into()))
    }
}

/// `OwnerDropProofPort` that always verifies the drop (personal-owner path).
#[derive(Debug)]
struct DropVerified;

#[async_trait]
impl OwnerDropProofPort for DropVerified {
    async fn verify_personal_owner_dropped(
        &self,
        _user_id: UserId,
        _drop_event_id: &str,
    ) -> Result<bool, AccessError> {
        Ok(true)
    }
}

/// `OwnerDropProofPort` that answers "this owner was not dropped".
#[derive(Debug)]
struct DropUnverified;

#[async_trait]
impl OwnerDropProofPort for DropUnverified {
    async fn verify_personal_owner_dropped(
        &self,
        _user_id: UserId,
        _drop_event_id: &str,
    ) -> Result<bool, AccessError> {
        Ok(false)
    }
}

/// `OwnerDropProofPort` that cannot be asked.
#[derive(Debug)]
struct DropPortFails;

#[async_trait]
impl OwnerDropProofPort for DropPortFails {
    async fn verify_personal_owner_dropped(
        &self,
        _user_id: UserId,
        _drop_event_id: &str,
    ) -> Result<bool, AccessError> {
        Err(AccessError::Resolution("resolver down".into()))
    }
}

fn system_authz() -> AuthzContext {
    AuthzContext::for_subject(UserId::new(uuid::Uuid::now_v7()), AuthPath::System)
}

fn engine_with(erase: Arc<FixedOutcomeErase>, drop_proof: Option<Arc<DropVerified>>) -> Engine {
    let drop_proof = drop_proof.map(|d| d as Arc<dyn OwnerDropProofPort>);
    let storage = StoragePorts::rejecting_with_owner_inverse(erase, drop_proof);
    Engine::compose_or_panic_for_tests(storage, |_| {})
}

#[tokio::test]
async fn raw_delegated_maintenance_is_denied_before_admin_or_storage() {
    let admin = Arc::new(PermissiveAdmin::default());
    let storage = StoragePorts::rejecting_with_erase_authority(admin.clone());
    let engine = Engine::compose_or_panic_for_tests(storage, |_| {});
    let subject = UserId::new(uuid::Uuid::now_v7());
    let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let raw = AuthzContext::for_subject_with_role(
        subject,
        [(owner, crate::Role::admin())],
        AuthPath::Delegated,
    );

    let error = engine
        .sweep_orphan_embedding_rows(&raw)
        .await
        .expect_err("raw delegated authority must not reach maintenance ports");

    assert_eq!(error.code, crate::ErrorCode::Forbidden);
    assert_eq!(admin.calls.load(Ordering::SeqCst), 0);
}

/// The ONE external-debt receipt. Cited upload objects and cold Memory
/// objects share `cold_purge_pending`, so the storage verb's answer is
/// the whole answer: the engine adds no post-commit object-store pass of
/// its own to report on. The vestigial `CitedObjectErasePort` that used
/// to run here enumerated rows the same transaction had just deleted, so
/// its `false` was vacuous rather than earned.
#[tokio::test]
async fn owner_erase_reports_the_storage_verbs_cold_debt_verbatim() {
    let group = GroupId::new(uuid::Uuid::now_v7());
    let engine = engine_with(
        Arc::new(FixedOutcomeErase::completed_with_cold_pending()),
        None,
    );

    let outcome = engine
        .erase_group_owner(&system_authz(), group)
        .await
        .expect("erase returns an outcome");
    assert!(matches!(
        outcome,
        OwnerEraseOutcome::Completed {
            cold_object_purge_pending: true,
            ..
        }
    ));

    let engine = engine_with(
        Arc::new(FixedOutcomeErase::completed()),
        Some(Arc::new(DropVerified)),
    );
    let outcome = engine
        .erase_personal_owner(
            &system_authz(),
            UserId::new(uuid::Uuid::now_v7()),
            "drop-1".into(),
        )
        .await
        .expect("erase returns an outcome");
    assert!(matches!(
        outcome,
        OwnerEraseOutcome::Completed {
            cold_object_purge_pending: false,
            ..
        }
    ));
}

#[tokio::test]
async fn completed_source_scope_erase_returns_a_completed_outcome() {
    let group = GroupId::new(uuid::Uuid::now_v7());
    let source = SourceId::new("source/a");
    let engine = engine_with(Arc::new(FixedOutcomeErase::completed()), None);

    let outcome = engine
        .erase_group_source_scope(&system_authz(), group, source)
        .await
        .expect("source-scope erase completes");

    assert!(matches!(outcome, OwnerEraseOutcome::Completed { .. }));
}

/// Drive all four erase entry points against one engine, so a gate that
/// only some of them apply cannot pass.
async fn attempt_every_scope(engine: &Engine, authz: &AuthzContext) -> Vec<OwnerEraseOutcome> {
    let group = GroupId::new(uuid::Uuid::now_v7());
    let user = UserId::new(uuid::Uuid::now_v7());
    let source = SourceId::new("source/a");
    vec![
        engine
            .erase_group_owner(authz, group)
            .await
            .expect("group owner erase returns an outcome"),
        engine
            .erase_personal_owner(authz, user, "drop-1".into())
            .await
            .expect("personal owner erase returns an outcome"),
        engine
            .erase_group_source_scope(authz, group, source.clone())
            .await
            .expect("group source scope erase returns an outcome"),
        engine
            .erase_personal_source_scope(authz, user, source, "drop-1".into())
            .await
            .expect("personal source scope erase returns an outcome"),
    ]
}

#[tokio::test]
async fn every_scope_turns_an_unauthorized_attempt_away_before_storage() {
    // No `OwnerEraseAuthorityPort` and a caller who is not `System`: nothing
    // can vouch for this attempt, so none of the four may reach storage.
    let erase = Arc::new(FixedOutcomeErase::completed());
    let engine = engine_with(erase.clone(), Some(Arc::new(DropVerified)));
    let authz = AuthzContext::for_subject(UserId::new(uuid::Uuid::now_v7()), AuthPath::HostBearer);

    let outcomes = attempt_every_scope(&engine, &authz).await;

    for outcome in &outcomes {
        assert!(
            matches!(outcome, OwnerEraseOutcome::Unauthorized { .. }),
            "expected an unauthorized outcome, got {outcome:?}"
        );
    }
    assert_eq!(
        erase.erase_calls.load(Ordering::SeqCst),
        0,
        "an unauthorized attempt must be turned away before storage"
    );
    for outcome in &outcomes {
        let (OwnerEraseOutcome::Unauthorized { operation_id }
        | OwnerEraseOutcome::Refused { operation_id, .. }) = outcome
        else {
            panic!("expected a refusal carrying an operation id, got {outcome:?}");
        };
        assert!(
            !operation_id.is_nil(),
            "a refusal the host must answer for still names the operation"
        );
    }
}

#[tokio::test]
async fn the_personal_scopes_refuse_before_storage_without_a_verified_drop() {
    // Authorized, but the drop the request names is not one the host will
    // confirm. Both personal scopes must stop; both group scopes, which
    // name no drop event, must still reach storage.
    for (drop_proof, expected) in [
        (
            Some(Arc::new(DropUnverified) as Arc<dyn OwnerDropProofPort>),
            OwnerEraseRefusal::PersonalDropNotVerified,
        ),
        (None, OwnerEraseRefusal::DropProofPortUnavailable),
        (
            Some(Arc::new(DropPortFails) as Arc<dyn OwnerDropProofPort>),
            OwnerEraseRefusal::DropProofPortUnavailable,
        ),
    ] {
        let erase = Arc::new(FixedOutcomeErase::completed());
        let storage = StoragePorts::rejecting_with_owner_inverse(erase.clone(), drop_proof);
        let engine = Engine::compose_or_panic_for_tests(storage, |_| {});

        let outcomes = attempt_every_scope(&engine, &system_authz()).await;

        for outcome in [&outcomes[1], &outcomes[3]] {
            let OwnerEraseOutcome::Refused { reason, .. } = outcome else {
                panic!("expected a refusal for a personal scope, got {outcome:?}");
            };
            assert_eq!(*reason, expected);
        }
        assert_eq!(
            erase.erase_calls.load(Ordering::SeqCst),
            2,
            "only the two group scopes may reach storage without a drop event"
        );
    }
}
