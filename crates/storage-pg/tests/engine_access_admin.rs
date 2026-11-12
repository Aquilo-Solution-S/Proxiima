//! The Engine's membership and transfer authority over Postgres: which
//! context may add and remove members, move an entity between owners, and
//! where the destination's consent comes from.

use std::sync::{Arc, Mutex};

use proxiima_core::authz::{
    AuthorizationHook, AuthzInput, AuthzOperation, AuthzOutcome, MembershipChange,
};
use proxiima_core::test_fixtures::authenticated_context;
use proxiima_core::{
    AccessCeiling, AuthPath, AuthzContext, Engine, EntityId, ErrorCode, GoalAccess, GroupId,
    MemoryId, OwnerId, Relation, Role, SpiiceRegistry, UserId,
};
use proxiima_pg_testkit::DbGuard;
use proxiima_storage_pg::PgStorage;
use proxiima_storage_pg::test_fixtures::{
    TestMemory, add_member, create_group, create_user, fresh_pg, memory_owner,
};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Records every membership change the engine asks hooks about.
#[derive(Debug, Default)]
struct MembershipChanges(Mutex<Vec<(MembershipChange, AuthzOutcome)>>);

impl AuthorizationHook for MembershipChanges {
    fn observe(&self, input: &AuthzInput<'_>, outcome: AuthzOutcome) {
        if let AuthzOperation::Membership { change, .. } = &input.operation {
            self.0
                .lock()
                .expect("changes lock")
                .push((*change, outcome));
        }
    }
}

struct World {
    pg: PgStorage,
    engine: Engine,
    _db: DbGuard,
}

impl World {
    async fn new(prefix: &str) -> Self {
        Self::with_hooks(prefix, Vec::new()).await
    }

    async fn with_hooks(prefix: &str, hooks: Vec<Arc<dyn AuthorizationHook>>) -> Self {
        let (pg, db) = fresh_pg(prefix).await;
        let mut registry = SpiiceRegistry::new();
        for hook in hooks {
            registry.add_authorization_hook(hook);
        }
        let engine = Engine::new(
            registry.freeze_or_panic_for_tests(),
            Arc::new(pg.clone()).storage_ports(),
        );
        Self {
            pg,
            engine,
            _db: db,
        }
    }

    /// A group registered the way a host creates one.
    async fn group(&self) -> TestResult<GroupId> {
        let group = GroupId::new(Uuid::now_v7());
        create_group(self.pg.pool_for_tests(), group).await?;
        Ok(group)
    }

    async fn memory_of(&self, owner: OwnerId) -> TestResult<MemoryId> {
        Ok(TestMemory::fact().create(&self.pg, owner).await?.memory_id)
    }

    async fn owner_of(&self, memory: MemoryId) -> TestResult<OwnerId> {
        Ok(memory_owner(self.pg.pool_for_tests(), memory).await?)
    }
}

fn holding(subject: UserId, roles: impl IntoIterator<Item = (OwnerId, Role)>) -> AuthzContext {
    authenticated_context(AuthzContext::for_subject_with_role(
        subject,
        roles,
        AuthPath::HostBearer,
    ))
}

/// Writes, but does not manage: it administers without the manage bit.
fn write_only() -> Role {
    Role::new(AccessCeiling::Perspective, AccessCeiling::Perspective)
        .expect("write == read is a valid role")
        .with_goals(GoalAccess::Works)
        .administering()
}

fn user() -> UserId {
    UserId::new(Uuid::now_v7())
}

#[tokio::test]
async fn access_admin_membership_hook_records_add_and_remove_direction() -> TestResult {
    let changes = Arc::new(MembershipChanges::default());
    let w = World::with_hooks("engine_access_hook", vec![changes.clone()]).await;
    let (group, admin, member) = (w.group().await?, user(), user());
    create_user(w.pg.pool_for_tests(), member).await?;
    let authz = holding(admin, [(OwnerId::from(group), Role::admin())]);

    w.engine
        .add_member(&authz, group, member.into(), Relation::Viewer)
        .await?;
    w.engine.remove_member(&authz, group, member.into()).await?;

    assert_eq!(
        *changes.0.lock().expect("changes lock"),
        vec![
            (MembershipChange::Add, AuthzOutcome::Allowed),
            (MembershipChange::Remove, AuthzOutcome::Allowed),
        ]
    );
    Ok(())
}

/// The destination gate has two paths and they must never merge.
///
/// When the request context already carries a resolved role for the
/// destination, that role is the answer, including when the answer is no.
/// Falling through to the membership re-read on a refusal would let a stored
/// Admin row overrule a host that deliberately handed out a narrowed role.
///
/// The control is what gives the refusals meaning: the same destination, with
/// the same stored Admin row all along, takes the transfer when the context is
/// silent about it.
#[tokio::test]
async fn a_carried_role_that_cannot_manage_never_falls_through_to_the_membership_row() -> TestResult
{
    let w = World::new("engine_access_carried").await;
    let subject = user();
    let owner = OwnerId::from(subject);
    let destination = w.group().await?;
    add_member(w.pg.pool_for_tests(), destination, subject, Relation::Admin).await?;

    let silent = authenticated_context(AuthzContext::for_subject(subject, AuthPath::HostBearer));
    let control = w.memory_of(owner).await?;
    w.engine
        .transfer_to_owner(&silent, EntityId::Memory(control), destination.into())
        .await?;
    assert_eq!(w.owner_of(control).await?, destination.into());

    // Present but insufficient. `Role::personal()` is the sharp one: it may
    // write Goals, so only `manages()` separates it from Admin.
    for role in [
        Role::viewer(),
        Role::ingest(),
        Role::editor(),
        Role::personal(),
    ] {
        let carried = holding(subject, [(destination.into(), role)]);
        let memory = w.memory_of(owner).await?;
        let err = w
            .engine
            .transfer_to_owner(&carried, EntityId::Memory(memory), destination.into())
            .await
            .expect_err("a carried role that cannot manage must refuse");
        assert_eq!(err.code, ErrorCode::Forbidden, "carried role {role:?}");
        assert_eq!(w.owner_of(memory).await?, owner, "carried role {role:?}");
    }
    Ok(())
}

#[tokio::test]
async fn access_admin_transfer_collapses_missing_and_foreign_to_not_found() -> TestResult {
    let w = World::new("engine_access_collapse").await;
    let caller = user();
    let authz = authenticated_context(AuthzContext::for_subject(caller, AuthPath::HostBearer));
    let destination = OwnerId::from(w.group().await?);
    let foreign = w.memory_of(OwnerId::from(user())).await?;

    let missing = w
        .engine
        .transfer_to_owner(
            &authz,
            EntityId::Memory(MemoryId::new(Uuid::now_v7())),
            destination,
        )
        .await
        .expect_err("an absent entity must not be transferable");
    let foreign_err = w
        .engine
        .transfer_to_owner(&authz, EntityId::Memory(foreign), destination)
        .await
        .expect_err("a foreign entity must be indistinguishable from absent");

    assert_eq!(missing.code, ErrorCode::NotFound);
    assert_eq!(foreign_err.code, ErrorCode::NotFound);
    assert_eq!(missing.message, foreign_err.message);
    assert_ne!(w.owner_of(foreign).await?, destination);
    Ok(())
}

#[tokio::test]
async fn access_admin_membership_and_transfer_require_manage_not_just_write() -> TestResult {
    let w = World::new("engine_access_manage").await;
    let (caller, member) = (user(), user());
    create_user(w.pg.pool_for_tests(), member).await?;
    let group = w.group().await?;
    let destination = w.group().await?;
    let entity = w.memory_of(group.into()).await?;
    let (group_owner, destination_owner) = (OwnerId::from(group), OwnerId::from(destination));
    let write_only_authz = holding(caller, [(group_owner, write_only())]);

    let denied = w
        .engine
        .add_member(&write_only_authz, group, member.into(), Relation::Viewer)
        .await
        .expect_err("write-without-manage must not add members");
    assert_eq!(denied.code, ErrorCode::Forbidden);

    add_member(w.pg.pool_for_tests(), group, member, Relation::Viewer).await?;
    let denied = w
        .engine
        .remove_member(&write_only_authz, group, member.into())
        .await
        .expect_err("write-without-manage must not remove members");
    assert_eq!(denied.code, ErrorCode::Forbidden);

    // Transfer OUT of a group-owned entity: the same manage gate on the
    // source side.
    let denied = w
        .engine
        .transfer_to_owner(
            &write_only_authz,
            EntityId::Memory(entity),
            destination_owner,
        )
        .await
        .expect_err("write-without-manage must not transfer a group entity");
    assert_eq!(denied.code, ErrorCode::Forbidden);

    // Admin on the source but nothing on the destination: the receiving side
    // refuses. The caller holds no stored membership either, so neither path
    // consents.
    let source_only = holding(caller, [(group_owner, Role::admin())]);
    let denied = w
        .engine
        .transfer_to_owner(&source_only, EntityId::Memory(entity), destination_owner)
        .await
        .expect_err("no authority on the destination must refuse the transfer");
    assert_eq!(denied.code, ErrorCode::Forbidden);

    // Admin on the SOURCE, write-without-manage on the DESTINATION: only the
    // destination's `require_group_manage` stands between this caller and a
    // transfer into a group that never consented.
    let write_only_destination = holding(
        caller,
        [
            (group_owner, Role::admin()),
            (destination_owner, write_only()),
        ],
    );
    let denied = w
        .engine
        .transfer_to_owner(
            &write_only_destination,
            EntityId::Memory(entity),
            destination_owner,
        )
        .await
        .expect_err("write-without-manage on the destination is not receiving-side consent");
    assert_eq!(denied.code, ErrorCode::Forbidden, "{}", denied.message);
    assert!(
        denied.message.contains("requires manage on this owner"),
        "refusal must name the manage bit: {}",
        denied.message
    );
    assert_eq!(w.owner_of(entity).await?, group_owner, "nothing moved");

    // manage == true (Role::admin) passes both gates and the writes land.
    let admin_authz = holding(
        caller,
        [
            (group_owner, Role::admin()),
            (destination_owner, Role::admin()),
        ],
    );
    w.engine
        .remove_member(&admin_authz, group, member.into())
        .await?;
    w.engine
        .add_member(&admin_authz, group, member.into(), Relation::Viewer)
        .await?;
    w.engine
        .transfer_to_owner(&admin_authz, EntityId::Memory(entity), destination_owner)
        .await?;
    assert_eq!(w.owner_of(entity).await?, destination_owner);
    Ok(())
}

/// The served surface narrows every request to ONE owner, so a transfer's
/// context can never name both sides. This is that context: `Role::admin()` on
/// the source group and literally nothing on the destination. The
/// destination's consent comes from the membership rows instead, so an Admin
/// row for this caller on the destination group opens the gate and a
/// non-manage row does not.
#[tokio::test]
async fn access_admin_transfer_resolves_a_narrowed_destination_out_of_band() -> TestResult {
    let w = World::new("engine_access_narrowed").await;
    let caller = user();
    let source = OwnerId::from(w.group().await?);
    let narrowed = holding(caller, [(source, Role::admin())]);
    assert!(
        narrowed
            .role_for_owner(&OwnerId::from(w.group().await?))
            .is_none(),
        "a narrowed context must carry no role for the destination"
    );

    for (relation, consents) in [
        (Relation::Admin, true),
        (Relation::Editor, false),
        (Relation::Viewer, false),
        (Relation::Ingest, false),
    ] {
        let destination = w.group().await?;
        add_member(w.pg.pool_for_tests(), destination, caller, relation).await?;
        let entity = w.memory_of(source).await?;
        let moved = w
            .engine
            .transfer_to_owner(&narrowed, EntityId::Memory(entity), destination.into())
            .await;
        if consents {
            moved.expect("an Admin row on the destination is receiving-side consent");
            assert_eq!(w.owner_of(entity).await?, destination.into());
        } else {
            let denied = moved.expect_err("a non-manage row is not consent");
            assert_eq!(
                denied.code,
                ErrorCode::Forbidden,
                "{relation:?} on the destination: {}",
                denied.message
            );
            assert_eq!(w.owner_of(entity).await?, source, "{relation:?}");
        }
    }
    Ok(())
}

/// A delegated grant decodes `manage = false`, so it must not reach the
/// out-of-band lookup and borrow its own subject's Admin membership.
#[tokio::test]
async fn access_admin_transfer_refuses_a_delegated_context_before_the_membership_lookup()
-> TestResult {
    let w = World::new("engine_access_delegated").await;
    let caller = user();
    let source = OwnerId::from(w.group().await?);
    let destination = w.group().await?;
    // Admin on the destination in storage: the authority the grant must NOT be
    // able to borrow.
    add_member(w.pg.pool_for_tests(), destination, caller, Relation::Admin).await?;
    let narrowed = |path| {
        authenticated_context(AuthzContext::for_subject_with_role(
            caller,
            [(source, Role::admin())],
            path,
        ))
    };

    let control = w.memory_of(source).await?;
    w.engine
        .transfer_to_owner(
            &narrowed(AuthPath::HostBearer),
            EntityId::Memory(control),
            destination.into(),
        )
        .await?;
    assert_eq!(w.owner_of(control).await?, destination.into());

    let entity = w.memory_of(source).await?;
    let denied = w
        .engine
        .transfer_to_owner(
            &narrowed(AuthPath::Delegated),
            EntityId::Memory(entity),
            destination.into(),
        )
        .await
        .expect_err("a raw delegated context is not Engine authority");
    assert_eq!(denied.code, ErrorCode::Forbidden);
    assert_eq!(w.owner_of(entity).await?, source);
    Ok(())
}
