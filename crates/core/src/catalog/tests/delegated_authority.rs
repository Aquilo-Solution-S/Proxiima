use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::access::AccessKind;
use crate::catalog::{ActionArgSpec, OwnerNeed, Replay, ToolAudience, ToolEffect};
use crate::error::ErrorCode;
use crate::query::QueryRequest;
use crate::storage_ports::delegated_authority::MAX_TOOL_ID_CHARS;
use crate::storage_ports::{
    DelegatedAuthorityError, DelegatedAuthorityService, DelegatedCommand, DelegationGrant,
    DelegationGrantStorage, DelegationId, DelegationMutationPermit, DelegationStorePort,
};
use crate::{
    AccessError, ActionName, AuthPath, AuthzContext, OwnerAccessPort, OwnerId, OwnerRoles, Role,
    ScopeKey, ServedSurface, SpiiceRegistryFrozen, StorageError, ToolScope, UserId,
    WriterAttribution,
};
use crate::{FactPayload, GroupId, PayloadKeyBuilder, SpiiceRegistry};
use crate::{Tool, ToolCtx, ToolError};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::SystemTime;
use uuid::Uuid;

const TOOL_NAME: &str = "test-delegation_worker";
const DISPATCHER_TOOL_NAME: &str = "test-delegation_dispatcher";

#[derive(schemars::JsonSchema, Deserialize)]
struct WorkerArgs {}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct WorkerOutput {}

struct WorkerTool;

impl Tool for WorkerTool {
    const TITLE: &'static str = "Test tool";
    const NAME: &'static str = TOOL_NAME;
    const DESCRIPTION: &'static str = "delegation test worker";
    const EFFECT: Option<ToolEffect> = Some(ToolEffect::Additive(Replay::NonIdempotent));
    type Args = WorkerArgs;
    type Output = WorkerOutput;

    fn call(
        _ctx: ToolCtx,
        _args: WorkerArgs,
    ) -> BoxFuture<'static, Result<Self::Output, ToolError>> {
        Box::pin(async { Ok(WorkerOutput {}) })
    }
}

#[derive(schemars::JsonSchema, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum DispatcherArgs {
    Run {},
    Other {},
}

struct DispatcherTool;

impl Tool for DispatcherTool {
    const TITLE: &'static str = "Test tool";
    const NAME: &'static str = DISPATCHER_TOOL_NAME;
    const DESCRIPTION: &'static str = "delegation dispatcher test worker";
    const ACTION_ARG_SPECS: &'static [ActionArgSpec] = &[
        ActionArgSpec {
            action: "run",
            allowed_fields: &[],
            required_fields: &[],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
            title: "Test action",
        },
        ActionArgSpec {
            action: "other",
            allowed_fields: &[],
            required_fields: &[],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
            title: "Test action",
        },
    ];
    type Args = DispatcherArgs;
    type Output = WorkerOutput;

    fn call(
        _ctx: ToolCtx,
        _args: DispatcherArgs,
    ) -> BoxFuture<'static, Result<Self::Output, ToolError>> {
        Box::pin(async { Ok(WorkerOutput {}) })
    }
}

#[derive(Debug, serde::Serialize, Deserialize)]
struct TestFact {
    value: String,
}

impl FactPayload for TestFact {
    const SCHEMA_ID: &'static str = "test/delegation-fact";
    const SCHEMA_VERSION: u32 = 1;

    fn receipt_key(&self) -> Vec<u8> {
        let mut key = PayloadKeyBuilder::new(Self::SCHEMA_ID, Self::SCHEMA_VERSION);
        key.field_str("value", &self.value);
        key.finish()
    }

    fn render(&self) -> String {
        self.value.clone()
    }
}

impl crate::PayloadProjection for TestFact {
    const KEY_FIELDS: &'static [crate::KeyField<Self>] = &[];
    fn display(&self) -> crate::PayloadDisplay {
        crate::PayloadDisplay {
            body: Some(self.value.clone()),
            ..Default::default()
        }
    }
    fn durable(&self) -> Result<crate::DurablePayload, String> {
        let mut builder = crate::DurablePayloadBuilder::new();
        builder.field("value", &self.value)?;
        Ok(builder.finish())
    }
}

#[derive(Debug, Default)]
struct MemoryStore {
    grant: Mutex<Option<DelegationGrant>>,
    inserts: AtomicUsize,
    loads: AtomicUsize,
    revokes: AtomicUsize,
}

#[async_trait]
impl DelegationStorePort for MemoryStore {
    async fn insert(
        &self,
        _permit: &DelegationMutationPermit,
        grant: &DelegationGrant,
    ) -> Result<(), StorageError> {
        self.inserts.fetch_add(1, Ordering::SeqCst);
        *self.grant.lock().expect("grant mutex") = Some(grant.clone());
        Ok(())
    }

    async fn load(
        &self,
        _permit: &DelegationMutationPermit,
        _delegation_id: DelegationId,
        _expected_owner: OwnerId,
    ) -> Result<Option<DelegationGrant>, StorageError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        Ok(self.grant.lock().expect("grant mutex").clone())
    }

    async fn revoke(
        &self,
        _permit: &DelegationMutationPermit,
        _delegation_id: DelegationId,
        _expected_owner: OwnerId,
        _revoked_at: SystemTime,
        _revoked_by: UserId,
    ) -> Result<bool, StorageError> {
        self.revokes.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
}

#[derive(Debug)]
struct TestOwnerAccess {
    requested_subject: UserId,
    returned_subject: Mutex<UserId>,
    owner: OwnerId,
    role: Mutex<Option<Role>>,
}

#[async_trait]
impl OwnerAccessPort for TestOwnerAccess {
    async fn resolve_roles_for_subject(&self, subject: UserId) -> Result<OwnerRoles, AccessError> {
        assert_eq!(subject, self.requested_subject);
        let returned_subject = *self.returned_subject.lock().expect("subject mutex");
        let role = *self.role.lock().expect("role mutex");
        OwnerRoles::for_subject(
            returned_subject,
            role.into_iter().map(|role| (self.owner, role)),
        )
    }
}

fn registry(with_tool: bool) -> SpiiceRegistryFrozen {
    let mut registry = SpiiceRegistry::new();
    registry.add_fact_schema_or_panic_for_tests::<TestFact>();
    if with_tool {
        registry.add_tool_or_panic_for_tests::<WorkerTool>("test-delegation");
    }
    registry.freeze_or_panic_for_tests()
}

fn dispatcher_registry() -> SpiiceRegistryFrozen {
    let mut registry = SpiiceRegistry::new();
    registry.add_tool_or_panic_for_tests::<DispatcherTool>("test-delegation");
    registry.freeze_or_panic_for_tests()
}

fn fact() -> crate::FactWriteCommand {
    crate::FactWriteCommand::from_payload(
        "test/source",
        &TestFact {
            value: "one".into(),
        },
    )
}

fn source(
    subject: UserId,
    owner: OwnerId,
    expires_at: SystemTime,
    scope: ToolScope,
) -> AuthzContext {
    AuthzContext::for_subject_with_role(subject, [(owner, Role::editor())], AuthPath::HostBearer)
        .with_expires_at(Some(expires_at))
        .with_tool_scope(scope)
        .seal_verified_scope()
        .expect("synthetic verified identity")
}

fn runtime(
    frozen: SpiiceRegistryFrozen,
    store: Arc<MemoryStore>,
    owner_access: Arc<TestOwnerAccess>,
    deployment_scope: ServedSurface,
) -> (crate::Engine, DelegatedAuthorityService, DelegatedCommand) {
    let (engine, _system, delegation_runtime) =
        crate::Engine::new(frozen, crate::StoragePorts::rejecting_for_tests())
            .into_runtime_authorities();
    let registry = Arc::new(engine.registry().clone());
    let command = DelegatedCommand::parse(TOOL_NAME, &registry).expect("registered command");
    let service = DelegatedAuthorityService::new(
        store,
        owner_access,
        registry,
        deployment_scope,
        &delegation_runtime,
    );
    (engine, service, command)
}

fn fixtures() -> (UserId, OwnerId, Arc<MemoryStore>, Arc<TestOwnerAccess>) {
    let subject = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(GroupId::new(Uuid::now_v7()));
    (
        subject,
        owner,
        Arc::new(MemoryStore::default()),
        Arc::new(TestOwnerAccess {
            requested_subject: subject,
            returned_subject: Mutex::new(subject),
            owner,
            role: Mutex::new(Some(Role::editor())),
        }),
    )
}

fn palette(ids: &[&str]) -> ToolScope {
    ToolScope::Palette(
        ids.iter()
            .map(|id| ScopeKey::parse(id).expect("a scope key"))
            .collect(),
    )
}

#[tokio::test]
async fn phase_allows_bounded_fact_authorization_and_raw_delegated_is_denied() {
    let (subject, owner, store, access) = fixtures();
    let (engine, service, command) = runtime(registry(true), store, access, ServedSurface::All);
    let caller = source(
        subject,
        owner,
        SystemTime::now() + Duration::from_secs(30),
        palette(&[TOOL_NAME]),
    );
    let issued = service
        .issue(&caller, owner, command.clone(), Role::editor())
        .await
        .expect("issue");
    let phase = service
        .redeem_phase(issued.id, owner, &command)
        .await
        .expect("redeem");
    engine
        .authorize_fact_ingest(&phase, fact(), &[])
        .await
        .expect("phase may authorize configured Fact write");

    let raw = AuthzContext::for_subject_with_role(
        subject,
        [(owner, Role::editor())],
        AuthPath::Delegated,
    );
    let write_err = engine
        .authorize_owner_write(&raw, &owner, AccessKind::Fact)
        .await
        .expect_err("raw delegated context must not mint a permit");
    assert_eq!(write_err.code, ErrorCode::Forbidden);
    let query_err = engine
        .query(&raw, &QueryRequest::readable())
        .await
        .expect_err("unconverted query must reject raw delegated context");
    assert_eq!(query_err.code, ErrorCode::Forbidden);
    let transfer_err = engine
        .transfer_to_owner(
            &raw,
            crate::access::EntityId::Memory(crate::MemoryId::new(Uuid::now_v7())),
            OwnerId::from(crate::GroupId::new(Uuid::now_v7())),
        )
        .await
        .expect_err("raw delegated context must be denied before owner lookup");
    assert_eq!(transfer_err.code, ErrorCode::Forbidden);
    let backfill_err = engine
        .backfill_missing_embeddings(&raw, &owner, 1)
        .await
        .expect_err("missing embedder still rejects raw delegated context");
    assert_eq!(backfill_err.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn writer_survives_durable_delegation_and_redemption() {
    let (subject, owner, store, access) = fixtures();
    let (engine, service, command) =
        runtime(registry(true), store.clone(), access, ServedSurface::All);
    for via in [None, Some(Uuid::now_v7())] {
        let writer = WriterAttribution {
            by: Uuid::now_v7(),
            via,
        };
        let caller = AuthzContext::for_subject_with_role(
            subject,
            [(owner, Role::editor())],
            AuthPath::HostBearer,
        )
        .with_attested_writer(writer)
        .unwrap()
        .with_expires_at(Some(SystemTime::now() + Duration::from_mins(1)))
        .seal_verified_scope()
        .unwrap();
        let issued = service
            .issue(&caller, owner, command.clone(), Role::editor())
            .await
            .unwrap();
        let stored = store.grant.lock().unwrap().clone().unwrap();
        assert_eq!(stored.writer(), writer);
        assert_eq!(stored.subject(), subject);
        let decoded = DelegationGrant::from_storage(
            service.mutation_permit_for_tests(),
            &DelegationGrantStorage {
                delegation_id: stored.delegation_id(),
                subject: stored.subject(),
                writer: stored.writer(),
                owner: stored.owner(),
                tool: stored.command().tool().as_str().to_owned(),
                action: stored
                    .command()
                    .action()
                    .map(|action| action.as_str().to_owned()),
                role_ceiling: stored.role_ceiling(),
                expires_at: stored.expires_at(),
                issued_at: stored.issued_at(),
                revoked_at: stored.revoked_at(),
                revoked_by: stored.revoked_by(),
            },
        )
        .unwrap();
        assert_eq!(decoded.writer(), writer);
        *store.grant.lock().unwrap() = Some(decoded);
        let phase = service
            .redeem_phase(issued.id, owner, &command)
            .await
            .unwrap();
        let operation = crate::authz::context_for_engine_operation(&phase).unwrap();
        assert_eq!(operation.authz().writer(), Some(writer));
        assert_eq!(operation.authz().owner_scope().unwrap().writer(), writer);
        assert_eq!(operation.authz().subject(), Some(subject));
        let authorized = engine
            .authorize_fact_ingest(&phase, fact(), &[])
            .await
            .unwrap();
        assert_eq!(
            authorized
                .owner_write_permit()
                .owner_scope()
                .unwrap()
                .writer(),
            writer
        );
    }
}

#[tokio::test]
async fn split_fact_witness_rechecks_expiry_before_commit() {
    let (subject, owner, store, access) = fixtures();
    let (engine, service, command) = runtime(registry(true), store, access, ServedSurface::All);
    let caller = source(
        subject,
        owner,
        SystemTime::now() + Duration::from_secs(30),
        ToolScope::All,
    );
    let issued = service
        .issue(&caller, owner, command.clone(), Role::editor())
        .await
        .expect("issue");
    let mut phase = service
        .redeem_phase(issued.id, owner, &command)
        .await
        .expect("redeem");
    let mut authorized = engine
        .authorize_fact_ingest(&phase, fact(), &[])
        .await
        .expect("authorize before expiry");
    authorized.expire_delegated_write_for_test();
    let err = engine
        .ingest_authorized_fact(&authorized)
        .await
        .expect_err("expired split witness must fail before storage");
    assert_eq!(err.code, ErrorCode::Forbidden);
    phase.expire_for_test();
    let err = engine
        .authorize_fact_ingest(&phase, fact(), &[])
        .await
        .expect_err("phase itself expires at every operation start");
    assert_eq!(err.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn foreign_runtime_phase_is_denied_before_engine_work() {
    let (subject, owner, store, access) = fixtures();
    let (foreign_engine, service, command) =
        runtime(registry(true), store, access, ServedSurface::All);
    let target_engine = crate::Engine::new(
        foreign_engine.registry().clone(),
        crate::StoragePorts::rejecting_for_tests(),
    );
    let caller = source(
        subject,
        owner,
        SystemTime::now() + Duration::from_secs(30),
        ToolScope::All,
    );
    let issued = service
        .issue(&caller, owner, command.clone(), Role::editor())
        .await
        .expect("issue");
    let phase = service
        .redeem_phase(issued.id, owner, &command)
        .await
        .expect("redeem");
    let err = target_engine
        .authorize_fact_ingest(&phase, fact(), &[])
        .await
        .expect_err("foreign phase must fail before schema/storage work");
    assert_eq!(err.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn denials_precede_store_mutation_and_adapter_subject_is_rechecked() {
    let (subject, owner, store, access) = fixtures();
    let (_engine, service, command) = runtime(
        registry(true),
        store.clone(),
        access.clone(),
        ServedSurface::All,
    );
    let caller = source(
        subject,
        owner,
        SystemTime::now() + Duration::from_secs(30),
        ToolScope::All,
    );
    let restricted = caller
        .clone()
        .with_served_surface(ServedSurface::Palette(Vec::new()));
    assert!(matches!(
        service
            .issue(&restricted, owner, command.clone(), Role::editor())
            .await,
        Err(DelegatedAuthorityError::ToolScopeDenied(_))
    ));
    assert_eq!(store.inserts.load(Ordering::SeqCst), 0);
    *access.returned_subject.lock().expect("subject mutex") = UserId::new(Uuid::now_v7());
    assert!(matches!(
        service.issue(&caller, owner, command, Role::editor()).await,
        Err(DelegatedAuthorityError::Access(AccessError::Resolution(_)))
    ));
    assert_eq!(store.inserts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn redemption_rechecks_owner_id_profile_registry_and_role() {
    let (subject, owner, store, access) = fixtures();
    let (_engine, service, command) = runtime(
        registry(true),
        store.clone(),
        access.clone(),
        ServedSurface::All,
    );
    let caller = source(
        subject,
        owner,
        SystemTime::now() + Duration::from_secs(30),
        ToolScope::All,
    );
    let issued = service
        .issue(&caller, owner, command.clone(), Role::editor())
        .await
        .expect("issue");
    let foreign_owner = OwnerId::from(GroupId::new(Uuid::now_v7()));
    assert!(matches!(
        service
            .redeem_phase(issued.id, foreign_owner, &command)
            .await,
        Err(DelegatedAuthorityError::NotFound)
    ));
    assert!(matches!(
        service
            .redeem_phase(DelegationId::from_uuid(Uuid::now_v7()), owner, &command)
            .await,
        Err(DelegatedAuthorityError::NotFound)
    ));

    let (_contracted_engine, contracted, _) = runtime(
        registry(true),
        store.clone(),
        access.clone(),
        ServedSurface::Palette(Vec::new()),
    );
    assert!(matches!(
        contracted.redeem_phase(issued.id, owner, &command).await,
        Err(DelegatedAuthorityError::ToolScopeDenied(_))
    ));

    let (no_tool_engine, _system, no_tool_runtime) =
        crate::Engine::new(registry(false), crate::StoragePorts::rejecting_for_tests())
            .into_runtime_authorities();
    let no_tool_service = DelegatedAuthorityService::new(
        store,
        access.clone(),
        Arc::new(no_tool_engine.registry().clone()),
        ServedSurface::All,
        &no_tool_runtime,
    );
    assert!(matches!(
        no_tool_service
            .redeem_phase(issued.id, owner, &command)
            .await,
        Err(DelegatedAuthorityError::CommandUnavailable(_))
    ));

    *access.role.lock().expect("role mutex") = Some(Role::ingest());
    assert!(matches!(
        service.redeem_phase(issued.id, owner, &command).await,
        Err(DelegatedAuthorityError::RoleCeilingNoLongerHeld)
    ));
    *access.role.lock().expect("role mutex") = None;
    assert!(matches!(
        service.redeem_phase(issued.id, owner, &command).await,
        Err(DelegatedAuthorityError::MembershipRevoked)
    ));
}

#[tokio::test]
async fn expired_bearer_cannot_revoke_or_touch_store() {
    let (subject, owner, store, access) = fixtures();
    let (_engine, service, _command) =
        runtime(registry(true), store.clone(), access, ServedSurface::All);
    let expired = source(
        subject,
        owner,
        SystemTime::now() - Duration::from_secs(1),
        ToolScope::All,
    );
    assert!(matches!(
        service
            .revoke(&expired, DelegationId::from_uuid(Uuid::now_v7()), owner,)
            .await,
        Err(DelegatedAuthorityError::Expired)
    ));
    assert_eq!(store.loads.load(Ordering::SeqCst), 0);
    assert_eq!(store.revokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn issue_rejects_non_host_authority_before_store_access() {
    let (subject, owner, store, access) = fixtures();
    let (_engine, service, command) =
        runtime(registry(true), store.clone(), access, ServedSurface::All);
    let system =
        AuthzContext::for_subject_with_role(subject, [(owner, Role::editor())], AuthPath::System)
            .with_expires_at(Some(SystemTime::now() + Duration::from_secs(30)))
            .with_tool_scope(ToolScope::All);

    assert!(matches!(
        service.issue(&system, owner, command, Role::editor()).await,
        Err(DelegatedAuthorityError::HostBearerRequired)
    ));
    assert_eq!(store.inserts.load(Ordering::SeqCst), 0);
    assert_eq!(store.loads.load(Ordering::SeqCst), 0);
}

struct ForwardedAccess(Arc<TestOwnerAccess>);

#[async_trait]
impl OwnerAccessPort for ForwardedAccess {
    async fn resolve_roles_for_subject(&self, subject: UserId) -> Result<OwnerRoles, AccessError> {
        self.0.resolve_roles_for_subject(subject).await
    }

    fn is_forwarder(&self, subject: UserId) -> Result<bool, AccessError> {
        Ok(subject == self.0.requested_subject)
    }

    async fn resolve_forwarded_owner_role(
        &self,
        subject: UserId,
        owner: OwnerId,
    ) -> Result<Option<crate::ForwarderRole>, AccessError> {
        Ok(
            (subject == self.0.requested_subject && owner == self.0.owner)
                .then_some(crate::ForwarderRole::Editor),
        )
    }
}

#[tokio::test]
async fn forwarded_owner_cannot_delegate_even_with_ordinary_membership() {
    let (subject, owner, store, access) = fixtures();
    let policy = ForwardedAccess(access.clone());
    let pending = AuthzContext::from_verified_forwarded_owner(
        subject,
        owner,
        WriterAttribution {
            by: owner.into_inner(),
            via: Some(Uuid::now_v7()),
        },
    )
    .with_expires_at(Some(SystemTime::now() + Duration::from_mins(1)))
    .seal_verified_scope()
    .unwrap();
    let forwarded = pending
        .with_authenticated_forwarded_owner(&policy, owner)
        .await
        .unwrap();
    assert!(forwarded.may_write(&owner, AccessKind::Fact));
    let (_engine, service, command) =
        runtime(registry(true), store.clone(), access, ServedSurface::All);
    assert!(matches!(
        service
            .issue(&forwarded, owner, command, Role::editor())
            .await,
        Err(DelegatedAuthorityError::HostBearerRequired)
    ));
    assert_eq!(store.inserts.load(Ordering::SeqCst), 0);
    assert_eq!(store.loads.load(Ordering::SeqCst), 0);
}

/// The command is a scope key, parsed once from the text a caller sends
/// and once from the two stored columns. Both springs refuse what is no
/// registered tool or leaf, and a command prints as the string it was
/// parsed from.
#[test]
fn a_command_is_a_registered_tool_or_leaf_from_text_and_from_storage() {
    let flat_registry = registry(true);
    let dispatcher_registry = dispatcher_registry();
    let leaf = format!("{DISPATCHER_TOOL_NAME}:run");

    let flat = DelegatedCommand::parse(TOOL_NAME, &flat_registry).expect("a flat tool");
    assert_eq!((flat.tool().as_str(), flat.action()), (TOOL_NAME, None));
    assert_eq!(flat.to_string(), TOOL_NAME);
    assert_eq!(
        serde_json::to_value(&flat).expect("serializes"),
        serde_json::json!(TOOL_NAME)
    );
    let parsed = DelegatedCommand::parse(&leaf, &dispatcher_registry).expect("a leaf");
    assert_eq!(parsed.to_string(), leaf);
    assert_eq!(parsed.action().map(ActionName::as_str), Some("run"));

    for text in [
        "",
        ":",
        "a:b:c",
        &format!("{TOOL_NAME}:"),
        &format!("{DISPATCHER_TOOL_NAME}:run:extra"),
        &format!("{DISPATCHER_TOOL_NAME}:bogus"),
        // a dispatcher is named with its action, and a flat tool has none
        DISPATCHER_TOOL_NAME,
        &format!("{TOOL_NAME}:run"),
        "resource:memory",
        "test delegation",
        "test/delegation",
    ] {
        let both = (
            DelegatedCommand::parse(text, &flat_registry),
            DelegatedCommand::parse(text, &dispatcher_registry),
        );
        assert!(
            both.0.is_err() && both.1.is_err(),
            "{text:?} is a command in neither registry: {both:?}"
        );
    }

    assert_eq!(
        DelegatedCommand::from_storage_parts_for_tests(DISPATCHER_TOOL_NAME, Some("run"))
            .expect("stored leaf"),
        parsed
    );
    assert_eq!(
        DelegatedCommand::from_storage_parts_for_tests(TOOL_NAME, None).expect("stored flat tool"),
        flat
    );
    for (tool, action) in [
        ("", None),
        (":", None),
        ("a:b", None),
        ("resource", Some("memory")),
        ("a b", None),
        ("a/b", None),
        (TOOL_NAME, Some("")),
        (TOOL_NAME, Some("a:b")),
        (TOOL_NAME, Some("a b")),
    ] {
        assert!(
            matches!(
                DelegatedCommand::from_storage_parts_for_tests(tool, action),
                Err(StorageError::ConstraintViolation(_))
            ),
            "{tool:?} {action:?} is not a stored command"
        );
    }
    let too_long = "a".repeat(MAX_TOOL_ID_CHARS + 1);
    assert!(DelegatedCommand::from_storage_parts_for_tests(&too_long, None).is_err());
}

#[tokio::test]
async fn dispatcher_command_requires_an_exact_leaf_for_caller_and_deployment() {
    let (subject, owner, store, access) = fixtures();
    let frozen = dispatcher_registry();
    let (engine, _system, runtime_authority) =
        crate::Engine::new(frozen, crate::StoragePorts::rejecting_for_tests())
            .into_runtime_authorities();
    let registry = Arc::new(engine.registry().clone());
    let command = DelegatedCommand::parse(format!("{DISPATCHER_TOOL_NAME}:run"), registry.as_ref())
        .expect("registered dispatcher action");

    let leaf = format!("{DISPATCHER_TOOL_NAME}:run");
    for (caller_key, served_key, allowed) in [
        (DISPATCHER_TOOL_NAME, leaf.as_str(), false),
        (leaf.as_str(), DISPATCHER_TOOL_NAME, false),
        (leaf.as_str(), leaf.as_str(), true),
    ] {
        let service = DelegatedAuthorityService::new(
            store.clone(),
            access.clone(),
            registry.clone(),
            ServedSurface::Palette(vec![ScopeKey::parse(served_key).expect("a scope key")]),
            &runtime_authority,
        );
        let caller = source(
            subject,
            owner,
            SystemTime::now() + Duration::from_secs(30),
            palette(&[caller_key]),
        );
        let issued = service
            .issue(&caller, owner, command.clone(), Role::editor())
            .await;
        if allowed {
            let issued = issued.expect("the exact leaf permits issuance");
            let _phase = service
                .redeem_phase(issued.id, owner, &command)
                .await
                .expect("the exact leaf permits redemption");
        } else {
            assert!(
                matches!(issued, Err(DelegatedAuthorityError::ToolScopeDenied(_))),
                "{caller_key} / {served_key}"
            );
        }
    }

    assert!(
        DelegatedCommand::parse(format!("{DISPATCHER_TOOL_NAME}:bogus"), registry.as_ref(),)
            .is_err()
    );
    let service = DelegatedAuthorityService::new(
        store,
        access,
        registry,
        ServedSurface::All,
        &runtime_authority,
    );
    let other_action = source(
        subject,
        owner,
        SystemTime::now() + Duration::from_secs(30),
        palette(&[&format!("{DISPATCHER_TOOL_NAME}:other")]),
    );
    assert!(matches!(
        service
            .issue(&other_action, owner, command, Role::editor())
            .await,
        Err(DelegatedAuthorityError::ToolScopeDenied(_))
    ));
}
#[tokio::test]
async fn a_delegated_read_needs_its_exact_caller_key_and_keeps_only_that_key() {
    let (subject, owner, store, access) = fixtures();
    let (engine, _system, authority) =
        crate::Engine::new(registry(true), crate::StoragePorts::rejecting_for_tests())
            .into_runtime_authorities();
    let registry = Arc::new(engine.registry().clone());
    let read = crate::protocol::tool::CORE_SEARCH_MEMORIES;
    let command = DelegatedCommand::parse(read, &registry).expect("registered read");
    let surface = ServedSurface::Palette(Vec::new());
    let service =
        DelegatedAuthorityService::new(store, access, registry, surface.clone(), &authority);
    let caller = source(
        subject,
        owner,
        SystemTime::now() + Duration::from_secs(30),
        palette(&[read]),
    );
    let omitted = caller.clone().with_tool_scope(palette(&[TOOL_NAME]));
    assert!(matches!(
        service
            .issue(&omitted, owner, command.clone(), Role::viewer())
            .await,
        Err(DelegatedAuthorityError::ToolScopeDenied(_))
    ));
    let issued = service
        .issue(&caller, owner, command.clone(), Role::viewer())
        .await
        .expect("deployment always serves a declared read");
    let phase = service
        .redeem_phase(issued.id, owner, &command)
        .await
        .expect("read remains served");
    let authority = crate::authz::context_for_engine_operation(&phase).expect("a redeemed phase");
    assert_eq!(authority.authz().tool_scope(), &palette(&[read]));
    assert_eq!(authority.authz().served_surface(), &surface);
    assert!(!authority.authz().capabilities().admits(
        &ScopeKey::parse(crate::protocol::resource::MEMORY).expect("resource key"),
        crate::authz::KeyDeclaration::shared(Some(ToolEffect::ReadOnly))
    ));
}
