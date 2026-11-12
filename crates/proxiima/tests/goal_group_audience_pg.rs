//! A Goal grant's subject is a person or a group. A member holds a group's
//! grant as far as the member's own role allows, resolved at question time.

#[path = "fixtures/example_engine.rs"]
#[allow(dead_code, reason = "this suite uses part of the shared fixture")]
mod example_engine;

use example_engine::ExampleEngine;
use proxiima_core::test_fixtures::authenticated_context;
use proxiima_core::verbs::goal_write::{
    GoalAuthorship, GoalCreateRequest, GoalTopologyWrite, IdempotencyKey,
};
use proxiima_core::verbs::query::{EntityKind, QueryRequest};
use proxiima_core::{
    AuthPath, AuthzContext, DerivedMemory, FactWrite, GoalAudienceGrantInput, GoalHandle,
    GoalPublicationInput, GroupId, InterpretationSubjectKind, InterpretationV1, MemoryDestination,
    MemoryId, MemoryTarget, OwnerId, OwnerRoles, PublicationUse, Relation, Role, SeriesHandle,
    SimpleTextGoalV1, UserId,
};
use proxiima_example::NoteV1;
use proxiima_storage_pg::PgPlatformScope;
use uuid::Uuid;

fn direct(subject: UserId) -> AuthzContext {
    authenticated_context(AuthzContext::for_subject(subject, AuthPath::HostBearer))
}

fn interpretation(claim: &str, evidence: MemoryId) -> InterpretationV1 {
    InterpretationV1 {
        claim: claim.to_owned(),
        confidence: 90,
        subject_memory_ids: vec![evidence.into_inner()],
        subject_kinds: vec![InterpretationSubjectKind::Fact],
        model_id: "test".into(),
        client_name: "test".into(),
        client_version: "1".into(),
    }
}

async fn platform(h: &ExampleEngine) -> PgPlatformScope {
    let pool = example_engine::fixture_pool(h.db.platform_url(), 1)
        .await
        .expect("platform pool");
    PgPlatformScope::new(pool, &["proxiima_core", "proxiima_example"])
        .await
        .expect("platform scope")
}

/// Register `groups`, then add each `(group, member, relation)`. A member may
/// be a person or a registered group.
async fn membership(
    h: &ExampleEngine,
    groups: &[GroupId],
    members: &[(GroupId, OwnerId, Relation)],
) {
    let platform = platform(h).await;
    let mut tx = platform.begin().await.expect("platform transaction");
    for group in groups {
        proxiima::register_group(&mut tx, *group)
            .await
            .expect("register group");
    }
    for (group, member, relation) in members {
        proxiima::add_group_member(&mut tx, *group, *member, *relation)
            .await
            .expect("add group member");
    }
    tx.commit().await.expect("membership commit");
}

async fn remove_member(h: &ExampleEngine, group: GroupId, member: OwnerId) {
    let platform = platform(h).await;
    let mut tx = platform.begin().await.expect("platform transaction");
    proxiima::remove_group_member(&mut tx, group, member)
        .await
        .expect("remove member");
    tx.commit().await.expect("removal commit");
}

/// One owner's Goal with a published Fact and Perspective to work from.
struct Scenario {
    owner_auth: AuthzContext,
    goal: GoalHandle,
    fact: MemoryId,
}

async fn scenario(h: &ExampleEngine, key: &str) -> Scenario {
    let owner_user = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(owner_user);
    let owner_auth = direct(owner_user);
    let notebook = h.notebook(&owner).await;
    let mut unit = h.engine.unit_of_work(&owner_auth).await.expect("unit");
    let fact = unit
        .ingest_fact(FactWrite::new(
            owner,
            "group-audience",
            &NoteV1 {
                notebook_id: notebook,
                key: key.into(),
                body: "source evidence".into(),
                written_at: time::OffsetDateTime::now_utc(),
            },
        ))
        .await
        .expect("fact")
        .memory_id;
    let goal = unit
        .create_goal(GoalCreateRequest {
            owner,
            topology: GoalTopologyWrite::new(Vec::new(), Vec::new(), Vec::new()).expect("topology"),
            title: key.into(),
            text: "group audience goal".into(),
            payload: SimpleTextGoalV1 {},
            request_id: IdempotencyKey::new(key).expect("request key"),
            authorship: GoalAuthorship::User,
            author_self_perspective_id: None,
        })
        .await
        .expect("goal")
        .goal_handle;
    unit.commit().await.expect("commit");
    let perspective = h
        .engine
        .derive_memory(
            &owner_auth,
            DerivedMemory::interpretation(
                MemoryTarget::Series(SeriesHandle::new(Uuid::now_v7())),
                owner,
                "published",
                interpretation("published", fact),
            ),
        )
        .await
        .expect("perspective")
        .memory_id;
    for memory in [fact, perspective] {
        h.engine
            .publish_to_goal(
                &owner_auth,
                &GoalPublicationInput {
                    goal,
                    memory,
                    permitted_use: PublicationUse::Read,
                },
            )
            .await
            .expect("publish");
    }
    Scenario {
        owner_auth,
        goal,
        fact,
    }
}

impl Scenario {
    async fn grant(
        &self,
        h: &ExampleEngine,
        subject: OwnerId,
        role: Role,
    ) -> Result<(), proxiima_core::ProtocolError> {
        h.engine
            .grant_goal_access(
                &self.owner_auth,
                &GoalAudienceGrantInput {
                    goal: self.goal,
                    subject,
                    role,
                    expires_at: None,
                },
            )
            .await
    }

    async fn reads_goal(&self, h: &ExampleEngine, who: UserId) -> bool {
        let mut query = QueryRequest::readable();
        query.entity_kind = Some(EntityKind::Goal);
        !h.engine
            .query(&direct(who), &query)
            .await
            .expect("goal query")
            .goals
            .is_empty()
    }

    async fn reads_memory(&self, h: &ExampleEngine, who: UserId) -> bool {
        let mut query = QueryRequest::readable();
        query.entity_kind = Some(EntityKind::Perspective);
        !h.engine
            .query(&direct(who), &query)
            .await
            .expect("memory query")
            .memories
            .is_empty()
    }

    async fn writes(&self, h: &ExampleEngine, who: UserId) -> bool {
        h.engine
            .derive_memory(
                &direct(who),
                DerivedMemory::interpretation(
                    MemoryTarget::Series(SeriesHandle::new(Uuid::now_v7())),
                    MemoryDestination::WorkingGoal(self.goal),
                    "work",
                    interpretation("work", self.fact),
                ),
            )
            .await
            .is_ok()
    }

    /// Reads the Goal, reads its published memory, writes its work.
    async fn holds(&self, h: &ExampleEngine, who: UserId) -> (bool, bool, bool) {
        (
            self.reads_goal(h, who).await,
            self.reads_memory(h, who).await,
            self.writes(h, who).await,
        )
    }
}

fn person() -> UserId {
    UserId::new(Uuid::now_v7())
}

fn group() -> GroupId {
    GroupId::new(Uuid::now_v7())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_member_holds_a_group_grant_as_a_person_holds_the_same_grant() {
    let h = ExampleEngine::new("proxiima_goal_group_same", |_| {}).await;
    let (invited, member, outsider, team) = (person(), person(), person(), group());
    membership(
        &h,
        &[team],
        &[(team, OwnerId::from(member), Relation::Admin)],
    )
    .await;

    let by_person = scenario(&h, "by-person").await;
    by_person
        .grant(&h, invited.into(), Role::editor())
        .await
        .expect("grant person");
    let by_group = scenario(&h, "by-group").await;
    by_group
        .grant(&h, team.into(), Role::editor())
        .await
        .expect("grant group");

    let person_holds = by_person.holds(&h, invited).await;
    assert_eq!(person_holds, (true, true, true));
    assert_eq!(by_group.holds(&h, member).await, person_holds);
    assert_eq!(by_group.holds(&h, outsider).await, (false, false, false));
    h.pool.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_member_role_caps_what_the_group_grant_gives() {
    let h = ExampleEngine::new("proxiima_goal_group_caps", |_| {}).await;
    let (viewer, ingest, team) = (person(), person(), group());
    membership(
        &h,
        &[team],
        &[
            (team, OwnerId::from(viewer), Relation::Viewer),
            (team, OwnerId::from(ingest), Relation::Ingest),
        ],
    )
    .await;
    let s = scenario(&h, "caps").await;
    s.grant(&h, team.into(), Role::editor())
        .await
        .expect("grant group");

    // The grant writes Perspectives; a viewer member reads but writes none.
    assert_eq!(s.holds(&h, viewer).await, (true, true, false));
    // An ingest member's role reads no Goals, so it reads neither the Goal
    // nor the Perspective the grant publishes.
    assert_eq!(s.holds(&h, ingest).await, (false, false, false));
    h.pool.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_member_through_a_child_group_is_capped_by_the_edge_relation() {
    let h = ExampleEngine::new("proxiima_goal_group_nested", |_| {}).await;
    let (member, parent, child) = (person(), group(), group());
    membership(
        &h,
        &[parent, child],
        &[
            (child, OwnerId::from(member), Relation::Admin),
            (parent, OwnerId::from(child), Relation::Viewer),
        ],
    )
    .await;
    let s = scenario(&h, "nested").await;
    s.grant(&h, parent.into(), Role::editor())
        .await
        .expect("grant parent");

    assert_eq!(s.holds(&h, member).await, (true, true, false));

    let platform = platform(&h).await;
    let mut tx = platform.begin().await.expect("platform transaction");
    proxiima::replace_group_member_relation(
        &mut tx,
        parent,
        OwnerId::from(child),
        Relation::Viewer,
        Relation::Editor,
    )
    .await
    .expect("raise edge");
    tx.commit().await.expect("commit");
    assert_eq!(s.holds(&h, member).await, (true, true, true));

    remove_member(&h, parent, OwnerId::from(child)).await;
    assert_eq!(s.holds(&h, member).await, (false, false, false));
    h.pool.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_member_ends_access_and_leaves_the_grant_untouched() {
    let h = ExampleEngine::new("proxiima_goal_group_removal", |_| {}).await;
    let (leaving, staying, team) = (person(), person(), group());
    membership(
        &h,
        &[team],
        &[
            (team, OwnerId::from(leaving), Relation::Editor),
            (team, OwnerId::from(staying), Relation::Editor),
        ],
    )
    .await;
    let s = scenario(&h, "removal").await;
    s.grant(&h, team.into(), Role::editor())
        .await
        .expect("grant group");
    let stored = || async {
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT to_jsonb(a) FROM proxiima_core.goal_access_grant a WHERE goal_handle=$1 AND subject_id=$2",
        )
        .bind(s.goal.into_inner())
        .bind(team.into_inner())
        .fetch_one(&h.pool)
        .await
        .expect("stored grant")
    };
    let before = stored().await;
    assert_eq!(s.holds(&h, leaving).await, (true, true, true));

    remove_member(&h, team, OwnerId::from(leaving)).await;

    assert_eq!(s.holds(&h, leaving).await, (false, false, false));
    assert_eq!(s.holds(&h, staying).await, (true, true, true));
    assert_eq!(stored().await, before);
    h.pool.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_group_grant_carries_no_manage() {
    let h = ExampleEngine::new("proxiima_goal_group_manage", |_| {}).await;
    let (member, team) = (person(), group());
    membership(
        &h,
        &[team],
        &[(team, OwnerId::from(member), Relation::Admin)],
    )
    .await;
    let s = scenario(&h, "manage").await;

    let refused = s
        .grant(&h, team.into(), Role::editor().managing())
        .await
        .expect_err("a group cannot manage a Goal's audience");
    assert!(
        refused
            .to_string()
            .contains("a Goal grant to a group carries no manage"),
        "{refused}"
    );
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM proxiima_core.goal_access_grant WHERE subject_id=$1",
    )
    .bind(team.into_inner())
    .fetch_one(&h.pool)
    .await
    .expect("count");
    assert_eq!(stored, 0);
    assert!(!s.reads_goal(&h, member).await);

    // A person may still be granted `manage`.
    s.grant(&h, person().into(), Role::editor().managing())
        .await
        .expect("grant person manage");
    h.pool.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unregistered_subject_is_a_person_first_seen() {
    let h = ExampleEngine::new("proxiima_goal_group_unregistered", |_| {}).await;
    let s = scenario(&h, "unregistered").await;
    let unseen = OwnerId::new(Uuid::now_v7());
    s.grant(&h, unseen, Role::viewer())
        .await
        .expect("grant unseen id");
    let kind: String =
        sqlx::query_scalar("SELECT kind::text FROM proxiima_core.owners WHERE owner_id=$1")
            .bind(unseen.into_inner())
            .fetch_one(&h.pool)
            .await
            .expect("registered");
    assert_eq!(kind, "personal");
    h.pool.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_person_on_two_routes_holds_the_join_of_both() {
    let h = ExampleEngine::new("proxiima_goal_group_join", |_| {}).await;
    let (both, team) = (person(), group());
    membership(
        &h,
        &[team],
        &[(team, OwnerId::from(both), Relation::Editor)],
    )
    .await;
    let s = scenario(&h, "join").await;
    s.grant(&h, both.into(), Role::viewer())
        .await
        .expect("direct viewer");
    assert_eq!(s.holds(&h, both).await, (true, true, false));

    s.grant(&h, team.into(), Role::editor())
        .await
        .expect("group editor");
    assert_eq!(s.holds(&h, both).await, (true, true, true));

    h.engine
        .revoke_goal_access(&s.owner_auth, s.goal, team.into())
        .await
        .expect("revoke group");
    assert_eq!(s.holds(&h, both).await, (true, true, false));
    h.pool.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_may_not_name_the_person_whose_grants_it_reads() {
    let h = ExampleEngine::new("proxiima_goal_group_runtime", |_| {}).await;
    let runtime = example_engine::fixture_pool(h.db.runtime_url(), 1)
        .await
        .expect("runtime pool");
    sqlx::query("SELECT count(*) FROM proxiima_core.goal_grants_held()")
        .execute(&runtime)
        .await
        .expect("the bound person's own grants");
    sqlx::query("SELECT count(*) FROM proxiima_core.goal_grants_of(gen_random_uuid())")
        .execute(&runtime)
        .await
        .expect_err("another person's grants");
    runtime.close().await;
    h.pool.close().await;
}

/// A group's Goal that `user` administers, with roles the host resolved, so no
/// earlier statement of a grant takes a person's owner-kind key and the test
/// controls the first wait.
async fn host_resolved_goal(
    h: &ExampleEngine,
    user: UserId,
    key: &str,
) -> (AuthzContext, GoalHandle) {
    let team = group();
    membership(h, &[team], &[(team, OwnerId::from(user), Relation::Admin)]).await;
    let owner = OwnerId::from(team);
    let auth = authenticated_context(AuthzContext::server_resolved(
        OwnerRoles::for_subject(user, [(owner, Role::admin())]).expect("owner roles"),
        AuthPath::HostBearer,
    ));
    let mut unit = h.engine.unit_of_work(&auth).await.expect("unit");
    let goal = unit
        .create_goal(GoalCreateRequest {
            owner,
            topology: GoalTopologyWrite::new(Vec::new(), Vec::new(), Vec::new()).expect("topology"),
            title: key.into(),
            text: "cross grant".into(),
            payload: SimpleTextGoalV1 {},
            request_id: IdempotencyKey::new(key).expect("request key"),
            authorship: GoalAuthorship::User,
            author_self_perspective_id: None,
        })
        .await
        .expect("goal")
        .goal_handle;
    unit.commit().await.expect("commit");
    (auth, goal)
}

/// Resolve once `count` backends wait on an advisory lock in this database.
async fn wait_for_advisory_waiters(h: &ExampleEngine, count: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(DISTINCT pid) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted
                   AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
            )
            .fetch_one(&h.pool)
            .await
            .expect("lock probe");
            if waiting >= count {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the grants wait on the held key");
}

/// Two owners grant on each other's Goals while a third transaction holds one
/// of their owner-kind keys, so both grants queue on it. Each grant takes its
/// owner-kind keys in one sorted acquisition, so whichever key is held, the
/// queue drains and neither transaction is aborted as a deadlock victim.
#[tokio::test(flavor = "multi_thread")]
async fn owners_granting_on_each_others_goals_do_not_deadlock() {
    let h = ExampleEngine::new("proxiima_goal_group_cross", |_| {}).await;
    for hold_lower in [true, false] {
        let (a, b) = (person(), person());
        let (a_auth, a_goal) = host_resolved_goal(&h, a, &format!("cross-a-{hold_lower}")).await;
        let (b_auth, b_goal) = host_resolved_goal(&h, b, &format!("cross-b-{hold_lower}")).await;
        let (lower, upper) = if a.into_inner() < b.into_inner() {
            (a, b)
        } else {
            (b, a)
        };
        let held = if hold_lower { lower } else { upper };
        let mut gate = h.pool.begin().await.expect("gate");
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('proxiima-owner-kind:' || $1::uuid::text, 0))",
        )
            .bind(held.into_inner())
            .execute(&mut *gate)
            .await
            .expect("hold the key");
        let grant = |auth: AuthzContext, goal, subject: UserId| {
            let engine = &h.engine;
            async move {
                engine
                    .grant_goal_access(
                        &auth,
                        &GoalAudienceGrantInput {
                            goal,
                            subject: subject.into(),
                            role: Role::viewer(),
                            expires_at: None,
                        },
                    )
                    .await
            }
        };
        let release = async {
            wait_for_advisory_waiters(&h, 2).await;
            gate.commit().await.expect("release the key");
        };
        let (first, second, ()) =
            tokio::join!(grant(a_auth, a_goal, b), grant(b_auth, b_goal, a), release);
        first.expect("a grants b");
        second.expect("b grants a");
    }
    h.pool.close().await;
}

/// A grant reads the subject's kind under the key registration holds: a
/// grant to an id being registered as a group waits for that registration and
/// then sees a group.
#[tokio::test(flavor = "multi_thread")]
async fn a_grant_reads_the_subject_kind_under_the_registration_lock() {
    let h = ExampleEngine::new("proxiima_goal_group_kind_lock", |_| {}).await;
    let (owner, team) = (person(), group());
    let (auth, goal) = host_resolved_goal(&h, owner, "kind-lock").await;
    let platform = platform(&h).await;
    let mut registering = platform.begin().await.expect("platform transaction");
    proxiima::register_group(&mut registering, team)
        .await
        .expect("register group");
    let input = GoalAudienceGrantInput {
        goal,
        subject: team.into(),
        role: Role::editor().managing(),
        expires_at: None,
    };
    let grant = h.engine.grant_goal_access(&auth, &input);
    let commit = async {
        wait_for_advisory_waiters(&h, 1).await;
        registering.commit().await.expect("registration commit");
    };
    let (refused, ()) = tokio::join!(grant, commit);
    let refused = refused.expect_err("the committed kind is a group");
    assert!(
        refused
            .to_string()
            .contains("a Goal grant to a group carries no manage"),
        "{refused}"
    );
    h.pool.close().await;
}
