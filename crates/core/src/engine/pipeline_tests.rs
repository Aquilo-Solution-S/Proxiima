use std::sync::{Arc, Mutex};

use crate::MemoryKind;
use crate::access::{AccessCeiling, Relation, Role};
use crate::authz::{
    AuthPath, AuthorizationHook, AuthzContext, AuthzInput, AuthzOperation, AuthzOutcome, AuthzVeto,
    OwnerResolver,
};
use crate::error::ErrorCode;
use crate::error::ProtocolError;
use crate::{GroupId, MemoryId, OwnerId, SpiiceRegistry, UserId};

use super::Engine;

type ResolvedAuthz = AuthzContext;

fn engine() -> Engine {
    Engine::new(
        SpiiceRegistry::new().freeze_or_panic_for_tests(),
        crate::StoragePorts::rejecting_for_tests(),
    )
}

fn engine_from_registry(registry: SpiiceRegistry) -> Engine {
    Engine::new(
        registry.freeze_or_panic_for_tests(),
        crate::StoragePorts::rejecting_for_tests(),
    )
}

fn owner() -> OwnerId {
    OwnerId::from(UserId::new(uuid::Uuid::now_v7()))
}

fn group_owner() -> OwnerId {
    OwnerId::from(GroupId::new(uuid::Uuid::now_v7()))
}

fn granted_context(owner: &OwnerId) -> ResolvedAuthz {
    AuthzContext::single_owner(owner, AuthPath::HostBearer)
}

/// Server-resolved caller holding `relation` on `group` (group access now
/// flows from host-resolved `OwnerRoles`, not from a per-request membership
/// storage lookup).
fn member_context(owner: &OwnerId, group: GroupId, relation: Relation) -> ResolvedAuthz {
    let subject = owner.as_user();
    AuthzContext::for_subject_with_role(
        subject,
        [(OwnerId::from(group), relation.role())],
        AuthPath::HostBearer,
    )
}

#[derive(Debug)]
struct StaticResolver {
    resolved: OwnerId,
}

impl OwnerResolver for StaticResolver {
    fn resolve(
        &self,
        _authz: &AuthzContext,
        _requested: &OwnerId,
    ) -> Result<OwnerId, ProtocolError> {
        Ok(self.resolved)
    }
}

#[derive(Debug)]
struct VetoHook;

impl AuthorizationHook for VetoHook {
    fn veto(&self, input: &AuthzInput<'_>) -> Result<(), AuthzVeto> {
        assert_eq!(input.requested, input.resolved);
        Err(AuthzVeto("test veto".into()))
    }
}

#[derive(Debug)]
struct RecordingHook {
    outcomes: Arc<Mutex<Vec<AuthzOutcome>>>,
}

impl AuthorizationHook for RecordingHook {
    fn observe(&self, input: &AuthzInput<'_>, outcome: AuthzOutcome) {
        assert!(matches!(
            input.authz.auth_path(),
            AuthPath::System | AuthPath::HostBearer
        ));
        assert_eq!(
            input.operation,
            AuthzOperation::Read {
                kind: MemoryKind::Fact
            }
        );
        self.outcomes.lock().expect("recorder lock").push(outcome);
    }
}

#[derive(Debug)]
struct RecordingAnyHook {
    outcomes: Arc<Mutex<Vec<AuthzOutcome>>>,
}

impl AuthorizationHook for RecordingAnyHook {
    fn observe(&self, _input: &AuthzInput<'_>, outcome: AuthzOutcome) {
        self.outcomes.lock().expect("recorder lock").push(outcome);
    }
}

#[tokio::test]
async fn single_owner_context_reads_its_owner() {
    let engine = engine();
    let owner = owner();
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);

    let resolved = engine
        .authorize_owner_read(&authz, &owner, MemoryKind::Perspective)
        .await
        .expect("single-owner context should authorize");

    assert_eq!(resolved, owner);
}

/// The one rule, at the gate: a role may act on a kind when its limit for
/// that direction is at least the kind, and Goal work and administration
/// are capabilities no limit grants. Every preset, and two roles whose
/// Goal work and write limit point opposite ways.
#[tokio::test]
async fn every_gate_follows_the_role_limit() {
    let engine = engine();
    let roles = [
        Role::viewer(),
        Role::ingest(),
        Role::editor(),
        Role::admin(),
        Role::new(AccessCeiling::Perspective, AccessCeiling::Abstraction).expect("role"),
        Role::new(AccessCeiling::Abstraction, AccessCeiling::Abstraction).expect("role"),
        Role::new(AccessCeiling::Perspective, AccessCeiling::Perspective).expect("role"),
        Role::new(AccessCeiling::Fact, AccessCeiling::Fact)
            .expect("role")
            .with_goals(crate::GoalAccess::Works),
    ];
    for role in roles {
        let subject = UserId::new(uuid::Uuid::now_v7());
        let group = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
        let authz =
            AuthzContext::for_subject_with_role(subject, [(group, role)], AuthPath::HostBearer);
        for kind in MemoryKind::ALL {
            let read = engine.authorize_owner_read(&authz, &group, kind).await;
            assert_eq!(read.is_ok(), role.may_read(kind), "{role:?} read {kind:?}");
            let write = engine.authorize_write(&authz, &group, kind).await;
            assert_eq!(
                write.is_ok(),
                role.may_write(kind),
                "{role:?} write {kind:?}"
            );
        }
        let goal_work = engine.authorize_goal_work(&authz, &group).await;
        assert_eq!(goal_work.is_ok(), role.works_goals(), "{role:?} Goal work");
        let admin = engine.authorize_owner_admin(&authz, &group).await;
        assert_eq!(admin.is_ok(), role.administers(), "{role:?} owner admin");
        let readers = engine
            .authorize_read(&authz)
            .await
            .expect("reads its own owner");
        assert!(readers.contains(&group), "{role:?} reads the group");
    }
}

#[tokio::test]
async fn authorize_write_allows_self_editor() {
    let p = owner();
    let engine = engine();
    let authz = granted_context(&p);

    let permit = engine
        .authorize_write(&authz, &p, MemoryKind::Perspective)
        .await
        .expect("the personal owner writes every kind");

    assert_eq!(permit.owner(), &p);
}

#[tokio::test]
async fn a_system_context_writes_only_when_for_system_built_it() {
    let (engine, authority) = engine().into_system_authority();
    let owner = owner();
    let subject = owner.as_user();
    let roles = crate::OwnerRoles::for_subject(subject, []).expect("roles");

    let permit = engine
        .authorize_owner_write(
            &AuthzContext::for_system(&authority, roles.clone()),
            &owner,
            MemoryKind::Perspective,
        )
        .await
        .expect("for_system proves the host holds the witness");
    assert_eq!(permit.owner(), &owner);
    assert_eq!(
        permit.authority(),
        crate::storage_ports::WriteAuthority::Memory(MemoryKind::Perspective)
    );

    let forged = AuthzContext::server_resolved(roles.clone(), AuthPath::System);
    let error = engine
        .authorize_owner_write(&forged, &owner, MemoryKind::Fact)
        .await
        .expect_err("a System context not from for_system writes nothing");
    assert_eq!(error.code, ErrorCode::Forbidden);
    assert_eq!(
        error.message,
        "System write authority requires a context from AuthzContext::for_system"
    );

    let (_, foreign) = self::engine().into_system_authority();
    let error = engine
        .authorize_owner_write(
            &AuthzContext::for_system(&foreign, roles),
            &owner,
            MemoryKind::Fact,
        )
        .await
        .expect_err("another Engine's witness stays powerless");
    assert_eq!(error.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn authorize_write_denies_viewer_for_editor() {
    let p = owner();
    let g1 = GroupId::new(uuid::Uuid::now_v7());
    let g1_owner = OwnerId::from(g1);
    let engine = engine();
    let authz = member_context(&p, g1, Relation::Viewer);

    let err = engine
        .authorize_write(&authz, &g1_owner, MemoryKind::Fact)
        .await
        .expect_err("a viewer writes nothing");

    assert_eq!(err.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn authorize_write_allows_editor_member_for_group() {
    let p = owner();
    let g1 = GroupId::new(uuid::Uuid::now_v7());
    let g1_owner = OwnerId::from(g1);
    let engine = engine();
    let authz = member_context(&p, g1, Relation::Editor);

    let permit = engine
        .authorize_write(&authz, &g1_owner, MemoryKind::Perspective)
        .await
        .expect("an editor writes Perspectives");

    assert_eq!(permit.owner(), &g1_owner);
}

#[tokio::test]
async fn authorize_write_veto_denies() {
    let p = owner();
    let mut registry = SpiiceRegistry::new();
    registry.add_authorization_hook(Arc::new(VetoHook));
    let engine = engine_from_registry(registry);
    let authz = granted_context(&p);

    let err = engine
        .authorize_write(&authz, &p, MemoryKind::Fact)
        .await
        .expect_err("veto should deny otherwise-allowed write");

    assert_eq!(err.code, ErrorCode::Forbidden);
    assert_eq!(err.message, "test veto");
}

#[tokio::test]
async fn authorize_write_refuses_a_context_without_access() {
    let p = owner();
    let engine = engine();
    let authz = crate::test_fixtures::no_access_context();

    let err = engine
        .authorize_write(&authz, &p, MemoryKind::Fact)
        .await
        .expect_err("a context without access should reject write authorization");

    assert_eq!(err.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn authorize_read_returns_personal_and_groups() {
    let p = owner();
    let g1 = GroupId::new(uuid::Uuid::now_v7());
    let g1_owner = OwnerId::from(g1);
    let engine = engine();
    let authz = member_context(&p, g1, Relation::Viewer);

    let read = engine
        .authorize_read(&authz)
        .await
        .expect("member context should resolve read owners");

    assert!(read.contains(&p));
    assert!(read.contains(&g1_owner));
    assert_eq!(read.len(), 2, "no owner beyond the caller's own read set");
}

/// Vetoes every operation on one owner and lets every other through.
#[derive(Debug)]
struct VetoOwner(OwnerId);

impl AuthorizationHook for VetoOwner {
    fn veto(&self, input: &AuthzInput<'_>) -> Result<(), AuthzVeto> {
        assert_eq!(
            input.requested, input.resolved,
            "set reads gate stored owners"
        );
        if *input.resolved == self.0 {
            return Err(AuthzVeto("owner vetoed".into()));
        }
        Ok(())
    }
}

/// Records every decision as `(resolved owner, operation, outcome)`.
#[derive(Debug)]
struct RecordingOwnerHook {
    events: Arc<Mutex<Vec<(OwnerId, AuthzOperation, AuthzOutcome)>>>,
}

impl AuthorizationHook for RecordingOwnerHook {
    fn observe(&self, input: &AuthzInput<'_>, outcome: AuthzOutcome) {
        self.events.lock().expect("recorder lock").push((
            *input.resolved,
            input.operation.clone(),
            outcome,
        ));
    }
}

const FACT_READ: AuthzOperation = AuthzOperation::Read {
    kind: MemoryKind::Fact,
};

#[tokio::test]
async fn authorize_read_drops_vetoed_owner() {
    let p = owner();
    let g1 = GroupId::new(uuid::Uuid::now_v7());
    let g1_owner = OwnerId::from(g1);
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut registry = SpiiceRegistry::new();
    registry.add_authorization_hook(Arc::new(VetoOwner(g1_owner)));
    registry.add_authorization_hook(Arc::new(RecordingOwnerHook {
        events: events.clone(),
    }));
    let engine = engine_from_registry(registry);
    let authz = member_context(&p, g1, Relation::Viewer);

    let read = engine
        .authorize_read(&authz)
        .await
        .expect("a veto narrows the read set, it does not refuse the read");

    assert_eq!(read, vec![p], "the vetoed group drops out of the read set");
    let mut seen = events.lock().expect("recorder lock").clone();
    seen.sort_by_key(|(owner, _, outcome)| (*owner == g1_owner, format!("{outcome:?}")));
    assert_eq!(
        seen,
        vec![
            (p, FACT_READ, AuthzOutcome::Allowed),
            (g1_owner, FACT_READ, AuthzOutcome::DeniedVeto),
        ],
        "one decision per owner, the denial included"
    );
}

#[tokio::test]
async fn authorize_read_all_vetoed_is_empty_not_forbidden() {
    let p = owner();
    let g1 = GroupId::new(uuid::Uuid::now_v7());
    let mut registry = SpiiceRegistry::new();
    registry.add_authorization_hook(Arc::new(VetoHook));
    let engine = engine_from_registry(registry);
    let authz = member_context(&p, g1, Relation::Viewer);

    let read = engine
        .authorize_read(&authz)
        .await
        .expect("an all-vetoed read set is empty, never an error that reveals the veto");

    assert!(read.is_empty());
}

/// Every engine read built on the read set answers an all-vetoed set
/// with empty results or not-found, never with an error that would
/// reveal the veto.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one assertion per engine read on the read set
async fn every_set_read_answers_an_all_vetoed_set_with_nothing() {
    use crate::verbs::query::{
        EdgeExistsRequest, EdgeReadRequest, MemoryLineageDirection, MemoryLineageRequest,
        QueryRequest,
    };
    let p = owner();
    let g1 = GroupId::new(uuid::Uuid::now_v7());
    let mut registry = SpiiceRegistry::new();
    registry.add_authorization_hook(Arc::new(VetoHook));
    let engine = engine_from_registry(registry);
    let authz = member_context(&p, g1, Relation::Viewer);
    let id = MemoryId::new(uuid::Uuid::now_v7());

    let err = engine
        .get_memory(
            &authz,
            &super::super::GetMemoryReadRequest {
                memory_id: id,
                include_neighbor_edges: true,
                at: crate::storage_ports::other_stores::ReadAt::Version,
            },
        )
        .await
        .expect_err("nothing readable");
    assert_eq!(err.code, ErrorCode::NotFound);
    let batch = engine
        .get_memories(
            &authz,
            &super::super::GetMemoriesReadRequest {
                memory_ids: vec![id],
            },
        )
        .await
        .expect("empty batch");
    assert!(batch.memories.is_empty());
    assert!(
        engine
            .load_sketches(&authz, &[id])
            .await
            .expect("sketches")
            .is_empty()
    );
    assert_eq!(
        engine.memory_counts(&authz).await.expect("counts"),
        crate::read_models::MemoryCounts::default()
    );
    assert!(
        engine
            .pin_nodes(&authz, &[id])
            .await
            .expect("pins")
            .is_empty()
    );
    assert!(
        engine
            .authorized_read_owners(&authz)
            .await
            .expect("owners")
            .is_empty()
    );
    let page = engine
        .query(&authz, &QueryRequest::readable())
        .await
        .expect("query");
    assert!(page.memories.is_empty() && page.goals.is_empty() && page.edges.is_empty());
    let edges = engine
        .read_edges(
            &authz,
            &EdgeReadRequest {
                filter: crate::verbs::query::EdgeFilter::default(),
                limit: 10,
                cursor: None,
            },
        )
        .await
        .expect("edges");
    assert!(edges.edges.is_empty());
    assert!(
        !engine
            .edge_exists(
                &authz,
                &EdgeExistsRequest {
                    filter: crate::verbs::query::EdgeFilter::default()
                }
            )
            .await
            .expect("exists")
            .exists
    );
    let lineage = engine
        .walk_memory_lineage(
            &authz,
            &MemoryLineageRequest {
                owner: p,
                start_memory_id: id,
                direction: MemoryLineageDirection::Ancestors,
                depth: 3,
                limit: 10,
                after: None,
            },
        )
        .await
        .expect("lineage");
    assert!(lineage.nodes.is_empty() && lineage.edges.is_empty());
    let citation = engine
        .read_fact_citation(
            &authz,
            &super::super::FactCitationReadRequest { fact_memory_id: id },
        )
        .await
        .expect_err("an unreadable fact is not found");
    assert_eq!(citation.message, super::ENTRY_NOT_FOUND_MESSAGE);
}

#[tokio::test]
async fn authorize_read_refuses_a_context_that_reads_no_owner() {
    let engine = engine();
    let authz = crate::test_fixtures::no_access_context();

    let err = engine
        .authorize_read(&authz)
        .await
        .expect_err("a context that reads no owner should reject read authorization");

    assert_eq!(err.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn a_context_without_access_returns_forbidden() {
    let engine = engine();
    let owner = owner();
    let authz = crate::test_fixtures::no_access_context();

    let err = engine
        .authorize_owner_read(&authz, &owner, MemoryKind::Fact)
        .await
        .expect_err("a context without access should reject authorization");

    assert_eq!(err.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn a_context_that_reads_no_owner_notifies_observers() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut registry = SpiiceRegistry::new();
    registry.add_authorization_hook(Arc::new(RecordingAnyHook {
        outcomes: outcomes.clone(),
    }));
    let engine = engine_from_registry(registry);
    let authz = crate::test_fixtures::no_access_context();

    let err = engine
        .authorize_read(&authz)
        .await
        .expect_err("a context that reads no owner should reject read authorization");

    assert_eq!(err.code, ErrorCode::Forbidden);
    assert_eq!(
        *outcomes.lock().expect("recorder lock"),
        vec![AuthzOutcome::DeniedResolution],
    );
}

#[tokio::test]
async fn resolver_remap_still_gates_resolved_owner() {
    let requested = owner();
    let hidden = owner();
    let mut registry = SpiiceRegistry::new();
    registry.set_owner_resolver_or_panic_for_tests(Arc::new(StaticResolver { resolved: hidden }));
    let engine = engine_from_registry(registry);
    let authz = AuthzContext::single_owner(&requested, AuthPath::System);

    let err = engine
        .authorize_owner_read(&authz, &requested, MemoryKind::Fact)
        .await
        .expect_err("resolved hidden owner should be denied");

    assert_eq!(err.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn veto_hook_denies_otherwise_allowed_request() {
    let owner = owner();
    let mut registry = SpiiceRegistry::new();
    registry.add_authorization_hook(Arc::new(VetoHook));
    let engine = engine_from_registry(registry);
    let authz = AuthzContext::single_owner(&owner, AuthPath::System);

    let err = engine
        .authorize_owner_read(&authz, &owner, MemoryKind::Fact)
        .await
        .expect_err("veto should deny otherwise-allowed request");

    assert_eq!(err.code, ErrorCode::Forbidden);
    assert_eq!(err.message, "test veto");
}

#[tokio::test]
async fn observer_records_allowed_and_denied_outcomes() {
    let owner = owner();
    let denied_owner = group_owner();
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let mut registry = SpiiceRegistry::new();
    registry.add_authorization_hook(Arc::new(RecordingHook {
        outcomes: outcomes.clone(),
    }));
    let engine = engine_from_registry(registry);
    let allowed = AuthzContext::single_owner(&owner, AuthPath::System);
    let denied = granted_context(&owner);

    engine
        .authorize_owner_read(&allowed, &owner, MemoryKind::Fact)
        .await
        .expect("allowed request should pass");
    let err = engine
        .authorize_owner_read(&denied, &denied_owner, MemoryKind::Fact)
        .await
        .expect_err("a context without a role on the owner should reject authorization");

    assert_eq!(err.code, ErrorCode::Forbidden);
    assert_eq!(
        *outcomes.lock().expect("recorder lock"),
        vec![AuthzOutcome::Allowed, AuthzOutcome::DeniedGrant],
    );
}
