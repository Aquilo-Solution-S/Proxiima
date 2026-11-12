//! The Engine's single-entry read gate over Postgres, seen through
//! `Engine::memory_kinds`: an absent entry, one outside the caller's read set
//! and one whose home owner a host veto denies are one answer, and each entry
//! of a batch gets its own host decision.

use std::sync::{Arc, Mutex};

use proxiima_core::authz::{
    AuthorizationHook, AuthzInput, AuthzOperation, AuthzOutcome, AuthzVeto,
};
use proxiima_core::test_fixtures::authenticated_context;
use proxiima_core::{
    AccessKind, AuthPath, AuthzContext, Engine, EntityKind, ErrorCode, GroupId, MemoryId, OwnerId,
    ProtocolError, Relation, SpiiceRegistry, UserId,
};
use proxiima_pg_testkit::DbGuard;
use proxiima_storage_pg::PgStorage;
use proxiima_storage_pg::test_fixtures::{TestMemory, create_group, fresh_pg};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Vetoes every operation on one owner and lets every other through.
#[derive(Debug)]
struct VetoOwner(OwnerId);

impl AuthorizationHook for VetoOwner {
    fn veto(&self, input: &AuthzInput<'_>) -> Result<(), AuthzVeto> {
        if *input.resolved == self.0 {
            return Err(AuthzVeto("owner vetoed".into()));
        }
        Ok(())
    }
}

type Decisions = Vec<(OwnerId, AccessKind, AuthzOutcome)>;

/// Records every read decision as `(resolved owner, kind, outcome)`.
#[derive(Debug, Default)]
struct ReadDecisions(Mutex<Decisions>);

impl AuthorizationHook for ReadDecisions {
    fn observe(&self, input: &AuthzInput<'_>, outcome: AuthzOutcome) {
        if let AuthzOperation::Read { kind } = input.operation {
            self.0
                .lock()
                .expect("decisions lock")
                .push((*input.resolved, kind, outcome));
        }
    }
}

struct World {
    pg: PgStorage,
    _db: DbGuard,
}

impl World {
    async fn new(prefix: &str) -> Self {
        let (pg, db) = fresh_pg(prefix).await;
        Self { pg, _db: db }
    }

    fn engine(&self, hooks: Vec<Arc<dyn AuthorizationHook>>) -> Engine {
        let mut registry = SpiiceRegistry::new();
        for hook in hooks {
            registry.add_authorization_hook(hook);
        }
        Engine::new(
            registry.freeze_or_panic_for_tests(),
            Arc::new(self.pg.clone()).storage_ports(),
        )
    }

    async fn fact_of(&self, owner: OwnerId) -> TestResult<MemoryId> {
        Ok(TestMemory::fact().create(&self.pg, owner).await?.memory_id)
    }

    async fn group(&self) -> TestResult<GroupId> {
        let group = GroupId::new(Uuid::now_v7());
        create_group(self.pg.pool_for_tests(), group).await?;
        Ok(group)
    }
}

fn user() -> OwnerId {
    OwnerId::from(UserId::new(Uuid::now_v7()))
}

/// The caller as a viewer member of `group`.
fn member_of(caller: OwnerId, group: GroupId) -> AuthzContext {
    authenticated_context(AuthzContext::for_subject_with_role(
        caller.as_user(),
        [(OwnerId::from(group), Relation::Viewer.role())],
        AuthPath::HostBearer,
    ))
}

fn personal(owner: OwnerId) -> AuthzContext {
    authenticated_context(AuthzContext::single_owner(&owner, AuthPath::HostBearer))
}

fn refusal(result: Result<Vec<(MemoryId, EntityKind)>, ProtocolError>) -> ProtocolError {
    result.expect_err("the entry must be refused")
}

#[tokio::test]
async fn a_readable_entry_answers_with_its_kind() -> TestResult {
    let w = World::new("engine_entry_readable").await;
    let owner = user();
    let memory = w.fact_of(owner).await?;

    let kinds = w
        .engine(Vec::new())
        .memory_kinds(&personal(owner), &[memory])
        .await?;

    assert_eq!(kinds, vec![(memory, EntityKind::Fact)]);
    Ok(())
}

#[tokio::test]
async fn an_absent_entry_is_not_found() -> TestResult {
    let w = World::new("engine_entry_absent").await;
    let owner = user();

    let err = refusal(
        w.engine(Vec::new())
            .memory_kinds(&personal(owner), &[MemoryId::new(Uuid::now_v7())])
            .await,
    );

    assert_eq!(err.code, ErrorCode::Forbidden);
    assert_eq!(err.message, "entry not found");
    Ok(())
}

#[tokio::test]
async fn an_entry_outside_the_read_set_is_not_found() -> TestResult {
    let w = World::new("engine_entry_foreign").await;
    let (caller, other) = (user(), user());
    let foreign = w.fact_of(other).await?;

    let err = refusal(
        w.engine(Vec::new())
            .memory_kinds(&personal(caller), &[foreign])
            .await,
    );

    assert_eq!(err.code, ErrorCode::Forbidden);
    assert_eq!(err.message, "entry not found");
    Ok(())
}

#[tokio::test]
async fn a_vetoed_home_owner_hides_the_entry_like_an_absent_one() -> TestResult {
    let w = World::new("engine_entry_vetoed").await;
    let caller = user();
    let group = w.group().await?;
    let group_owner = OwnerId::from(group);
    let held = w.fact_of(group_owner).await?;
    let authz = member_of(caller, group);

    let decisions = Arc::new(ReadDecisions::default());
    let vetoed = refusal(
        w.engine(vec![Arc::new(VetoOwner(group_owner)), decisions.clone()])
            .memory_kinds(&authz, &[held])
            .await,
    );
    let absent = refusal(
        w.engine(Vec::new())
            .memory_kinds(&authz, &[MemoryId::new(Uuid::now_v7())])
            .await,
    );

    assert_eq!(vetoed.message, "entry not found");
    assert_eq!(
        (vetoed.code, vetoed.message),
        (absent.code, absent.message),
        "a vetoed entry is byte-identical to an absent one"
    );
    assert_eq!(
        *decisions.0.lock().expect("decisions lock"),
        vec![(group_owner, AccessKind::Fact, AuthzOutcome::DeniedVeto)],
        "one decision, for the one owner involved, with the entry's real kind"
    );
    Ok(())
}

/// A batch decides each entry on its own, in order, and stops at the first
/// refusal: the entries after it are not decided.
#[tokio::test]
async fn a_batch_decides_each_entry_and_stops_at_the_first_refusal() -> TestResult {
    let w = World::new("engine_entry_batch").await;
    let caller = user();
    let group = w.group().await?;
    let group_owner = OwnerId::from(group);
    let held = [
        w.fact_of(group_owner).await?,
        w.fact_of(group_owner).await?,
        w.fact_of(group_owner).await?,
    ];
    let authz = member_of(caller, group);

    let allowed = Arc::new(ReadDecisions::default());
    let kinds = w
        .engine(vec![allowed.clone()])
        .memory_kinds(&authz, &held)
        .await?;
    assert_eq!(
        kinds,
        held.map(|memory| (memory, EntityKind::Fact)).to_vec(),
        "every entry is readable"
    );
    assert_eq!(allowed.0.lock().expect("decisions lock").len(), held.len());

    let vetoed = Arc::new(ReadDecisions::default());
    let err = refusal(
        w.engine(vec![Arc::new(VetoOwner(group_owner)), vetoed.clone()])
            .memory_kinds(&authz, &held)
            .await,
    );
    assert_eq!(err.message, "entry not found");
    assert_eq!(
        *vetoed.0.lock().expect("decisions lock"),
        vec![(group_owner, AccessKind::Fact, AuthzOutcome::DeniedVeto)],
        "one decision, and none for the entries after the refusal"
    );
    Ok(())
}
