//! The Engine's Goal writes over Postgres: a Perspective the caller does not
//! own is refused as a motivation target and as the author's Self, and a host
//! hook judges each write under the need it declares.

use std::sync::{Arc, Mutex};

use proxiima_core::authz::{
    AuthorizationHook, AuthzInput, AuthzOperation, AuthzOutcome, AuthzVeto,
};
use proxiima_core::test_fixtures::authenticated_context;
use proxiima_core::verbs::goal_write::{
    ChildGoalDraft, GoalAuthorship, GoalEvidenceRef, GoalMotivationRef, GoalPayloadWrite,
    GoalTopologyWrite, GoalTransitionKind, GoalWriteOutcome, IdempotencyKey,
};
use proxiima_core::verbs::query::QueryRequest;
use proxiima_core::{
    AccessKind, AuthPath, AuthzContext, Engine, ErrorCode, GoalCreatePayloadWriteRequest,
    GoalDecomposeRequest, GoalMarkAchievedRequest, GoalModifyRequest, GoalTransitionRequest,
    MemoryId, OwnerId, PendingWork, SimpleTextGoalV1, SpiiceRegistry, UserId,
};
use proxiima_pg_testkit::DbGuard;
use proxiima_storage_pg::PgStorage;
use proxiima_storage_pg::test_fixtures::{
    TestDerived, TestNote, fresh_pg, goal_authorship_category,
};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Records the write need each gate reports to a host hook.
#[derive(Debug, Default)]
struct WriteNeeds(Mutex<Vec<AuthzOperation>>);

impl WriteNeeds {
    fn take(&self) -> Vec<AuthzOperation> {
        std::mem::take(&mut *self.0.lock().expect("write need recorder lock"))
    }
}

impl AuthorizationHook for WriteNeeds {
    fn observe(&self, input: &AuthzInput<'_>, _: AuthzOutcome) {
        if matches!(
            input.operation,
            AuthzOperation::Write { .. } | AuthzOperation::GoalWork | AuthzOperation::OwnerAdmin
        ) {
            self.0
                .lock()
                .expect("write need recorder lock")
                .push(input.operation.clone());
        }
    }
}

struct World {
    pg: PgStorage,
    engine: Engine,
    owner: OwnerId,
    authz: AuthzContext,
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
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        Self {
            pg,
            engine,
            owner,
            authz: authenticated_context(AuthzContext::single_owner(&owner, AuthPath::HostBearer)),
            _db: db,
        }
    }

    async fn fact(&self, owner: OwnerId) -> TestResult<MemoryId> {
        Ok(TestNote::new("note", "note")
            .create(&self.pg, owner)
            .await?
            .memory_id)
    }

    /// A Perspective of `owner`, over an Abstraction over a Fact.
    async fn perspective(&self, owner: OwnerId) -> TestResult<MemoryId> {
        let fact = self.fact(owner).await?;
        let abstraction = TestDerived::abstraction(fact)
            .create(&self.pg, owner)
            .await?
            .memory_id;
        Ok(TestDerived::perspective(abstraction)
            .create(&self.pg, owner)
            .await?
            .memory_id)
    }

    fn create_request(
        &self,
        key: &str,
        motivated_by: Vec<GoalMotivationRef>,
        self_perspective: Option<MemoryId>,
    ) -> GoalCreatePayloadWriteRequest {
        GoalCreatePayloadWriteRequest {
            owner: self.owner,
            topology: topology(motivated_by),
            payload: payload_write("created"),
            request_id: request_id(key),
            authorship: GoalAuthorship::User,
            author_self_perspective_id: self_perspective,
        }
    }

    async fn create_goal(&self, key: &str) -> TestResult<GoalWriteOutcome> {
        Ok(self
            .engine
            .create_goal_from_payload_write(
                &self.authz,
                &self.create_request(key, Vec::new(), None),
            )
            .await?)
    }

    async fn goal_ids(&self) -> TestResult<Vec<proxiima_core::GoalId>> {
        let mut ids: Vec<_> = self
            .engine
            .query(&self.authz, &QueryRequest::readable())
            .await?
            .goals
            .iter()
            .map(|goal| goal.id)
            .collect();
        ids.sort();
        Ok(ids)
    }

    async fn goal_count(&self) -> TestResult<usize> {
        Ok(self
            .engine
            .query(&self.authz, &QueryRequest::readable())
            .await?
            .goals
            .len())
    }
}

fn topology(motivated_by: Vec<GoalMotivationRef>) -> GoalTopologyWrite {
    GoalTopologyWrite::new(motivated_by, Vec::new(), Vec::new()).expect("test topology")
}

fn payload_write(title: &str) -> GoalPayloadWrite {
    GoalPayloadWrite::from_payload(title, "Test goal", SimpleTextGoalV1 {}).expect("test payload")
}

fn request_id(key: &str) -> IdempotencyKey {
    IdempotencyKey::new(key).expect("valid idempotency key")
}

fn foreign_owner() -> OwnerId {
    OwnerId::from(UserId::new(Uuid::now_v7()))
}

#[tokio::test]
async fn author_self_perspective_denies_foreign_write_owner() -> TestResult {
    let w = World::new("engine_goal_self_foreign").await;
    let foreign = w.perspective(foreign_owner()).await?;

    let err = w
        .engine
        .create_goal_from_payload_write(
            &w.authz,
            &w.create_request("self-foreign", Vec::new(), Some(foreign)),
        )
        .await
        .expect_err("a foreign author Self perspective must be refused");

    assert_eq!(err.code, ErrorCode::Forbidden);
    assert_eq!(w.goal_count().await?, 0);
    Ok(())
}

#[tokio::test]
async fn author_self_perspective_allows_writable_perspective() -> TestResult {
    let w = World::new("engine_goal_self_own").await;
    let own = w.perspective(w.owner).await?;

    w.engine
        .create_goal_from_payload_write(
            &w.authz,
            &w.create_request("self-own", Vec::new(), Some(own)),
        )
        .await?;

    assert_eq!(w.goal_count().await?, 1);
    Ok(())
}

#[tokio::test]
async fn create_goal_from_payload_write_rejects_foreign_perspective_target() -> TestResult {
    let w = World::new("engine_goal_create_foreign").await;
    let foreign = w.perspective(foreign_owner()).await?;

    let err = w
        .engine
        .create_goal_from_payload_write(
            &w.authz,
            &w.create_request(
                "create-foreign-target",
                vec![GoalMotivationRef::perspective(foreign)],
                None,
            ),
        )
        .await
        .expect_err("a foreign target Perspective must be rejected before the write");

    assert_eq!(err.code, ErrorCode::Forbidden);
    assert_eq!(w.goal_count().await?, 0);
    Ok(())
}

#[tokio::test]
async fn decompose_goal_rejects_foreign_perspective_target() -> TestResult {
    let w = World::new("engine_goal_decompose_foreign").await;
    let parent = w.create_goal("decompose-parent").await?;
    let foreign = w.perspective(foreign_owner()).await?;

    let err = w
        .engine
        .decompose_goal(
            &w.authz,
            &GoalDecomposeRequest {
                owner: w.owner,
                parent_goal_id: parent.goal_id,
                authorship: GoalAuthorship::User,
                topology: topology(vec![GoalMotivationRef::perspective(foreign)]),
                children: vec![ChildGoalDraft {
                    payload: payload_write("child"),
                    evidence: Vec::new(),
                    request_id: request_id("decompose-foreign-target"),
                }],
                author_self_perspective_id: None,
            },
        )
        .await
        .expect_err("a foreign target Perspective must be rejected before the write");

    assert_eq!(err.code, ErrorCode::Forbidden);
    assert_eq!(w.goal_count().await?, 1, "only the parent exists");
    Ok(())
}

/// A host hook judges each Goal write under the need it declares: set, modify
/// and transition are Goal work, achieve and decompose are owner
/// administration, and none of them is a memory write.
#[tokio::test]
async fn a_hook_sees_each_goal_write_under_its_declared_need() -> TestResult {
    let needs = Arc::new(WriteNeeds::default());
    let w = World::with_hooks("engine_goal_needs", vec![needs.clone()]).await;

    let created = w.create_goal("need-create").await?;
    assert_eq!(needs.take(), [AuthzOperation::GoalWork], "set");

    let modified = w
        .engine
        .modify_goal(
            &w.authz,
            &GoalModifyRequest {
                owner: w.owner,
                prior_goal_id: created.goal_id,
                replacement: payload_write("modified"),
                authorship: GoalAuthorship::User,
                request_id: request_id("need-modify"),
                evidence: None,
                motivated_by: None,
                author_self_perspective_id: None,
            },
        )
        .await?;
    assert_eq!(needs.take(), [AuthzOperation::GoalWork], "modify");

    w.engine
        .transition_goal(
            &w.authz,
            &GoalTransitionRequest {
                owner: w.owner,
                prior_goal_id: modified.goal_id,
                transition: GoalTransitionKind::Pause {
                    pending: PendingWork::Keep,
                },
                authorship: GoalAuthorship::User,
                request_id: request_id("need-transition"),
                author_self_perspective_id: None,
            },
        )
        .await?;
    assert_eq!(needs.take(), [AuthzOperation::GoalWork], "transition");

    let to_achieve = w.create_goal("need-to-achieve").await?;
    let evidence = w.fact(w.owner).await?;
    needs.take();
    w.engine
        .mark_goal_achieved(
            &w.authz,
            &GoalMarkAchievedRequest {
                owner: w.owner,
                prior_goal_id: to_achieve.goal_id,
                authorship: GoalAuthorship::User,
                request_id: request_id("need-achieve"),
                evidence: vec![GoalEvidenceRef::new(evidence)],
                author_self_perspective_id: None,
            },
        )
        .await?;
    assert_eq!(needs.take(), [AuthzOperation::OwnerAdmin], "achieve");

    let parent = w.create_goal("need-parent").await?;
    needs.take();
    w.engine
        .decompose_goal(
            &w.authz,
            &GoalDecomposeRequest {
                owner: w.owner,
                parent_goal_id: parent.goal_id,
                authorship: GoalAuthorship::User,
                topology: topology(Vec::new()),
                children: vec![ChildGoalDraft {
                    payload: payload_write("child"),
                    evidence: Vec::new(),
                    request_id: request_id("need-decompose-child"),
                }],
                author_self_perspective_id: None,
            },
        )
        .await?;
    assert_eq!(needs.take(), [AuthzOperation::OwnerAdmin], "decompose");
    Ok(())
}

/// Refuses reads of one memory kind while it is set.
#[derive(Debug, Default)]
struct RefuseKind(Mutex<Option<AccessKind>>);

impl RefuseKind {
    fn set(&self, kind: Option<AccessKind>) {
        *self.0.lock().expect("refused kind lock") = kind;
    }
}

impl AuthorizationHook for RefuseKind {
    fn veto(&self, input: &AuthzInput<'_>) -> Result<(), AuthzVeto> {
        match input.operation {
            AuthzOperation::Read { kind }
                if Some(kind) == *self.0.lock().expect("refused kind lock") =>
            {
                Err(AuthzVeto("evidence refused".into()))
            }
            _ => Ok(()),
        }
    }
}

/// Evidence carried by omission is read-authorized per target: a host hook
/// that refuses either carried target (the Fact or the Abstraction) refuses
/// the modify and leaves the Goal as it was; with neither refused it succeeds.
#[tokio::test]
async fn modify_authorizes_each_carried_evidence_target() -> TestResult {
    let hook = Arc::new(RefuseKind::default());
    let w = World::with_hooks("engine_goal_carried", vec![hook.clone()]).await;
    let fact = w.fact(w.owner).await?;
    let abstraction = TestDerived::abstraction(fact)
        .create(&w.pg, w.owner)
        .await?
        .memory_id;
    let goal = w
        .engine
        .create_goal_from_payload_write(
            &w.authz,
            &GoalCreatePayloadWriteRequest {
                topology: GoalTopologyWrite::new(
                    Vec::new(),
                    Vec::new(),
                    vec![
                        GoalEvidenceRef::new(fact),
                        GoalEvidenceRef::new(abstraction),
                    ],
                )?,
                ..w.create_request("carried-create", Vec::new(), None)
            },
        )
        .await?;
    let modify = |prior, key: &str| GoalModifyRequest {
        owner: w.owner,
        prior_goal_id: prior,
        replacement: payload_write("modified"),
        authorship: GoalAuthorship::User,
        request_id: request_id(key),
        evidence: None,
        motivated_by: None,
        author_self_perspective_id: None,
    };

    for (refused, key) in [
        (AccessKind::Fact, "carried-fact"),
        (AccessKind::Abstraction, "carried-abstraction"),
    ] {
        hook.set(Some(refused));
        let before = w.goal_ids().await?;
        let err = w
            .engine
            .modify_goal(&w.authz, &modify(goal.goal_id, key))
            .await
            .expect_err("a refused carried target refuses the modify");
        assert_eq!(err.code, ErrorCode::Forbidden, "refused {refused:?}");
        assert_eq!(w.goal_ids().await?, before, "refused {refused:?}");
    }

    hook.set(None);
    w.engine
        .modify_goal(&w.authz, &modify(goal.goal_id, "carried-control"))
        .await?;
    Ok(())
}

/// The author category the store keeps for a user-authored write is `user`.
#[tokio::test]
async fn a_user_authored_goal_stores_the_user_category() -> TestResult {
    let w = World::new("engine_goal_user_category").await;
    let goal = w.create_goal("user-category").await?;

    assert_eq!(
        goal_authorship_category(w.pg.pool_for_tests(), goal.goal_id).await?,
        "user"
    );
    Ok(())
}
