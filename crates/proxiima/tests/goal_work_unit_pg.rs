//! A host command declared Goal work commits for every member who works the
//! Goal owner's Goals, together with the Facts it declares, owned by the
//! Goal's owner. The member's other writes keep the relation's ceiling.
#![allow(clippy::too_many_lines)]

#[path = "fixtures/host_state/mod.rs"]
#[allow(
    dead_code,
    reason = "this suite reuses only the host fixture's app and tables"
)]
mod host_state_fixture;
#[path = "fixtures/migrated_app.rs"]
mod migrated_app;
#[path = "fixtures/split_core_db.rs"]
mod split_core_db;

use std::sync::Arc;

use async_trait::async_trait;
use host_state_fixture::{AUXILIARY_TABLE, HostFixtureApp, PARTICIPANT};
use proxiima::host::{
    AgreedCommand, GoalWorkCommand, HostStateCommand, HostStateHandler, HostStatePayloadOwners,
    HostStateWritePermit, PgCommandDispatcher, StateSurfaceName,
};
use proxiima::{
    AuthPath, AuthzContext, ErrorCode, HostStateOutcome, PgHostStateSession, Proxiima,
    ServedSurface,
};
use proxiima_core::test_fixtures::authenticated_context;
use proxiima_core::verbs::goal_write::{
    GoalAuthorship, GoalCreateRequest, GoalTopologyWrite, IdempotencyKey,
};
use proxiima_core::{
    AgentNoteV1, DerivedMemory, GoalId, GroupId, InterpretationSubjectKind, InterpretationV1,
    MemoryTarget, OwnerId, OwnerRoles, Relation, Role, SeriesHandle, SimpleTextGoalV1,
    StorageError, UserId,
};
use proxiima_pg_testkit::db_url;
use proxiima_storage_pg::PgPlatformScope;
use split_core_db::clone_split_core_db;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const TABLES: &[StateSurfaceName] = &[StateSurfaceName::new(AUXILIARY_TABLE)];

/// Arms a Goal: one host row, keyed by the invocation, on the Goal's owner.
struct Arm {
    owner: OwnerId,
    goal: GoalId,
    invocation: Uuid,
}

impl HostStateCommand for Arm {
    const PARTICIPANT_ID: proxiima::HostStateParticipantId = PARTICIPANT;
    const TABLES: &'static [StateSurfaceName] = TABLES;
    type Outcome = ();

    fn owner(&self) -> OwnerId {
        self.owner
    }
}

impl HostStatePayloadOwners for Arm {
    fn payload_owners(&self) -> Vec<OwnerId> {
        Vec::new()
    }
}

impl GoalWorkCommand for Arm {
    type Fact = AgentNoteV1;

    fn goal(&self) -> GoalId {
        self.goal
    }
}

struct ArmHandler;

#[async_trait]
impl HostStateHandler<Arm> for ArmHandler {
    async fn handle(
        &self,
        session: &mut PgHostStateSession<'_, '_>,
        permit: &HostStateWritePermit,
        command: AgreedCommand<Arm>,
    ) -> Result<HostStateOutcome<()>, StorageError> {
        sqlx::query("INSERT INTO host_fixture.auxiliary (invocation_id, owner_id) VALUES ($1, $2)")
            .bind(command.into_inner().invocation)
            .bind(permit.owner().into_inner())
            .execute(&mut **session.transaction())
            .await
            .map_err(proxiima_storage_pg::map_err)?;
        Ok(HostStateOutcome::Permitted(()))
    }
}

fn member(user: UserId, roles: &[(OwnerId, Role)]) -> AuthzContext {
    authenticated_context(AuthzContext::server_resolved(
        OwnerRoles::for_subject(user, roles.iter().copied()).expect("member roles"),
        AuthPath::HostBearer,
    ))
}

fn note(title: &str) -> AgentNoteV1 {
    AgentNoteV1 {
        note_id: Uuid::now_v7(),
        title: title.into(),
        body: title.into(),
        tags: Vec::new(),
        idempotency_key: Some(title.into()),
    }
}

fn arm(owner: OwnerId, goal: GoalId) -> Arm {
    Arm {
        owner,
        goal,
        invocation: Uuid::now_v7(),
    }
}

fn goal_request(owner: OwnerId, key: &str) -> GoalCreateRequest<SimpleTextGoalV1> {
    GoalCreateRequest {
        owner,
        topology: GoalTopologyWrite::new(Vec::new(), Vec::new(), Vec::new()).expect("topology"),
        title: "Group goal".into(),
        text: "group work".into(),
        payload: SimpleTextGoalV1 {},
        request_id: IdempotencyKey::new(key).expect("key"),
        authorship: GoalAuthorship::User,
        author_self_perspective_id: None,
    }
}

#[tokio::test]
async fn a_member_who_works_goals_commits_declared_goal_work_and_writes_nothing_else() -> TestResult
{
    let db = clone_split_core_db("proxiima_goal_work_unit").await?;
    migrated_app::migrate::<HostFixtureApp>(db.platform_url())
        .await
        .map_err(|error| error.to_string())?;
    let participant =
        Arc::new(PgCommandDispatcher::new(PARTICIPANT, TABLES).register::<Arm, _>(ArmHandler)?);
    let built = Proxiima::<HostFixtureApp>::app()
        .configure(|b| {
            b.database_url(db.runtime_url())
                .platform_database_url(db.platform_url())
                .store_number(0)
                .owner(UserId::new(Uuid::now_v7()).into())
                .allow_insecure_single_owner()
                .served_surface(ServedSurface::All)
                .host_state_participant(participant)
        })
        .build()
        .await?;
    let engine = built.host().engine();
    let admin_pool = PgPool::connect(&db_url(db.name())).await?;
    let platform = PgPlatformScope::new(
        PgPool::connect(db.platform_url()).await?,
        &["proxiima_core", "host_fixture"],
    )
    .await?;

    let (admin_user, viewer_user, outsider_user) = (
        UserId::new(Uuid::now_v7()),
        UserId::new(Uuid::now_v7()),
        UserId::new(Uuid::now_v7()),
    );
    let (group, secret) = (GroupId::new(Uuid::now_v7()), GroupId::new(Uuid::now_v7()));
    let mut tx = platform.begin().await?;
    for (owned, members) in [
        (
            group,
            &[
                (admin_user, Relation::Admin),
                (viewer_user, Relation::Viewer),
            ][..],
        ),
        (secret, &[(admin_user, Relation::Admin)][..]),
    ] {
        proxiima::register_group(&mut tx, owned).await?;
        for (user, relation) in members {
            proxiima::add_group_member(&mut tx, owned, (*user).into(), *relation).await?;
        }
    }
    tx.commit().await?;
    let (group, secret) = (OwnerId::from(group), OwnerId::from(secret));
    let admin = member(
        admin_user,
        &[(group, Role::admin()), (secret, Role::admin())],
    );
    let viewer = member(viewer_user, &[(group, Role::viewer())]);
    let outsider = authenticated_context(AuthzContext::for_subject(
        outsider_user,
        AuthPath::HostBearer,
    ));
    let goal = engine
        .create_goal(&admin, goal_request(group, "group-goal"))
        .await?
        .goal_id;
    let secret_goal = engine
        .create_goal(&admin, goal_request(secret, "secret-goal"))
        .await?
        .goal_id;

    // The viewer arms the group's Goal: the host row and the declared Fact
    // commit together, both owned by the group.
    let armed = arm(group, goal);
    let invocation = armed.invocation;
    let mut unit = engine.goal_work_unit_of_work(&viewer, group, goal);
    let outcome = unit
        .apply_goal_work(
            armed,
            [proxiima::FactWrite::new(group, "goal-arm", &note("armed"))],
        )
        .await?;
    assert!(matches!(outcome.state, HostStateOutcome::Permitted(())));
    let [fact] = outcome.facts.as_slice() else {
        panic!("one declared Fact: {:?}", outcome.facts);
    };
    unit.commit().await?;
    let host_owner: Uuid =
        sqlx::query_scalar("SELECT owner_id FROM host_fixture.auxiliary WHERE invocation_id = $1")
            .bind(invocation)
            .fetch_one(&admin_pool)
            .await?;
    assert_eq!(
        host_owner,
        group.into_inner(),
        "the group owns the host state"
    );
    let fact_owner: Uuid =
        sqlx::query_scalar("SELECT owner_id FROM proxiima_core.memory WHERE t = $1")
            .bind(fact.memory_id.into_inner())
            .fetch_one(&admin_pool)
            .await?;
    assert_eq!(
        fact_owner,
        group.into_inner(),
        "the group owns the declared Fact"
    );

    // The same command undeclared, a plain Fact and a Perspective keep the
    // viewer's ceiling.
    let mut ordinary = engine.unit_of_work(&viewer).await?;
    let refused = ordinary
        .apply_host_state(arm(group, goal))
        .await
        .expect_err("an undeclared host write keeps the Fact ceiling");
    assert_eq!(refused.code, ErrorCode::Forbidden);
    drop(ordinary);
    let refused = engine
        .ingest_fact(
            &viewer,
            proxiima::FactWrite::new(group, "plain", &note("plain")),
        )
        .await
        .expect_err("a plain Fact keeps the ceiling");
    assert_eq!(refused.code, ErrorCode::Forbidden);
    let refused = engine
        .derive_memory(
            &viewer,
            DerivedMemory::interpretation(
                MemoryTarget::Series(SeriesHandle::new(Uuid::now_v7())),
                group,
                "viewer perspective",
                InterpretationV1 {
                    claim: "armed".into(),
                    confidence: 90,
                    subject_memory_ids: vec![fact.memory_id.into_inner()],
                    subject_kinds: vec![InterpretationSubjectKind::Fact],
                    model_id: "test".into(),
                    client_name: "test".into(),
                    client_version: "1".into(),
                },
            ),
        )
        .await
        .expect_err("a Perspective keeps the ceiling");
    assert_eq!(refused.code, ErrorCode::Forbidden);

    // Goal work names its own Goal and owner, and declares Facts on that
    // owner only.
    let mut unit = engine.goal_work_unit_of_work(&viewer, group, goal);
    assert!(
        unit.apply_goal_work(arm(group, secret_goal), [])
            .await
            .is_err(),
        "a command of another Goal is refused"
    );
    assert!(
        unit.apply_goal_work(
            arm(group, goal),
            [proxiima::FactWrite::new(
                viewer_user.into(),
                "elsewhere",
                &note("elsewhere")
            )],
        )
        .await
        .is_err(),
        "a declared Fact on another owner is refused"
    );
    assert!(
        unit.apply_goal_work(
            arm(group, goal),
            [proxiima::FactWrite::new(group, "pinned", &note("pinned")).refs([fact.memory_id])],
        )
        .await
        .is_err(),
        "a declared Fact pins no memory"
    );
    drop(unit);
    let mut unit = engine.goal_work_unit_of_work(&viewer, group, secret_goal);
    assert!(
        unit.apply_goal_work(arm(group, secret_goal), [])
            .await
            .is_err(),
        "a Goal of another owner is not the group's"
    );
    drop(unit);
    let mut unit = engine.goal_work_unit_of_work(&viewer, secret, secret_goal);
    let refused = unit
        .apply_goal_work(arm(secret, secret_goal), [])
        .await
        .expect_err("the viewer works no Goal of a group it is not in");
    assert_eq!(refused.code, ErrorCode::Forbidden);
    drop(unit);

    // A non-member works none of the group's Goals.
    let mut unit = engine.goal_work_unit_of_work(&outsider, group, goal);
    let refused = unit
        .apply_goal_work(
            arm(group, goal),
            [proxiima::FactWrite::new(
                group,
                "outsider",
                &note("outsider"),
            )],
        )
        .await
        .expect_err("a non-member works no group Goal");
    assert_eq!(refused.code, ErrorCode::Forbidden);
    drop(unit);

    // The admin works it too.
    let mut unit = engine.goal_work_unit_of_work(&admin, group, goal);
    let outcome = unit
        .apply_goal_work(
            arm(group, goal),
            [proxiima::FactWrite::new(group, "admin-arm", &note("admin"))],
        )
        .await?;
    assert_eq!(outcome.facts.len(), 1);
    unit.commit().await?;

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM host_fixture.auxiliary")
        .fetch_one(&admin_pool)
        .await?;
    assert_eq!(
        rows, 2,
        "only the viewer's and the admin's Goal work committed"
    );
    built.shutdown().await;
    admin_pool.close().await;
    drop(db);
    Ok(())
}
