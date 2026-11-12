//! Reference admission of a Fact write over Postgres: the declared targets
//! must exist, be readable by the caller and be of the kind the declaration
//! names, and what the write goes on to carry is exactly what was admitted.

use std::sync::Arc;

use proxiima_core::authz::{AuthorizationHook, AuthzInput, AuthzOperation, AuthzOutcome};
use proxiima_core::test_fixtures::authenticated_context;
use proxiima_core::verbs::fact_ingest::FactWriteCommand;
use proxiima_core::{
    AuthPath, AuthzContext, EdgeEndpoint, Engine, EntityKind, ErrorCode, FactPayload, GoalId,
    MemoryId, OwnerId, PayloadKeyBuilder, PayloadReference, ReferenceBinding, SidecarPayload,
    SpiiceRegistry, UserId,
};
use proxiima_pg_testkit::DbGuard;
use proxiima_storage_pg::PgStorage;
use proxiima_storage_pg::test_fixtures::{TestDerived, TestGoal, TestMemory, fresh_pg};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// A Fact that declares its reference targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReferencedFact {
    fact_id: String,
    targets: Vec<EdgeEndpoint>,
}

impl FactPayload for ReferencedFact {
    const SCHEMA_ID: &'static str = "test/ingest-referenced-fact";
    const SCHEMA_VERSION: u32 = 1;

    fn receipt_key(&self) -> Vec<u8> {
        let mut key = PayloadKeyBuilder::new(Self::SCHEMA_ID, Self::SCHEMA_VERSION);
        key.field_str("fact_id", &self.fact_id);
        key.finish()
    }

    fn render(&self) -> String {
        self.fact_id.clone()
    }

    fn references(&self) -> Vec<PayloadReference> {
        self.targets
            .iter()
            .copied()
            .map(|target| PayloadReference {
                field: "target",
                binding: ReferenceBinding::Pin,
                target,
            })
            .collect()
    }
}

impl proxiima_core::PayloadProjection for ReferencedFact {
    const KEY_FIELDS: &'static [proxiima_core::KeyField<Self>] = &[];

    fn display(&self) -> proxiima_core::PayloadDisplay {
        proxiima_core::PayloadDisplay {
            body: Some(self.fact_id.clone()),
            ..Default::default()
        }
    }

    fn durable(&self) -> Result<proxiima_core::DurablePayload, String> {
        let mut builder = proxiima_core::DurablePayloadBuilder::new();
        builder
            .field("fact_id", &self.fact_id)?
            .field("targets", &self.targets)?;
        Ok(builder.finish())
    }
}

/// Counts the read decisions the engine reports to host hooks.
#[derive(Debug, Default)]
struct ReadDecisions(std::sync::atomic::AtomicUsize);

impl AuthorizationHook for ReadDecisions {
    fn observe(&self, input: &AuthzInput<'_>, _: AuthzOutcome) {
        if matches!(input.operation, AuthzOperation::Read { .. }) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        Self::with_hook(prefix, None).await
    }

    async fn with_hook(prefix: &str, hook: Option<Arc<dyn AuthorizationHook>>) -> Self {
        let (pg, db) = fresh_pg(prefix).await;
        let mut registry = SpiiceRegistry::new();
        if let Some(hook) = hook {
            registry.add_authorization_hook(hook);
        }
        registry.add_fact_schema_or_panic_for_tests::<ReferencedFact>();
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
        Ok(TestMemory::fact().create(&self.pg, owner).await?.memory_id)
    }

    async fn goal(&self, name: &str) -> TestResult<GoalId> {
        Ok(GoalId::new(
            TestGoal::new(name).create(&self.pg, self.owner).await?.t,
        ))
    }
}

fn payload(name: &str, targets: Vec<EdgeEndpoint>) -> ReferencedFact {
    ReferencedFact {
        fact_id: name.to_owned(),
        targets,
    }
}

fn draft(payload: &ReferencedFact) -> FactWriteCommand {
    FactWriteCommand::from_payload("test/references", payload)
}

fn fact_target(memory: MemoryId) -> EdgeEndpoint {
    EdgeEndpoint::memory(EntityKind::Fact, memory)
}

#[tokio::test]
async fn fact_payload_refs_are_the_authorized_links() -> TestResult {
    let w = World::new("engine_refs_links").await;
    let fact = w.fact(w.owner).await?;
    let goal = w.goal("refs-links").await?;
    let payload = payload(
        "typed-links",
        vec![
            fact_target(fact),
            EdgeEndpoint::goal(goal),
            fact_target(fact),
        ],
    );
    let sidecars = [SidecarPayload::fact(payload.clone())];

    let authorized = w
        .engine
        .authorize_fact_ingest(&w.authz, draft(&payload), &sidecars)
        .await?;

    assert_eq!(
        authorized.links().references(),
        &[fact_target(fact), EdgeEndpoint::goal(goal)]
    );
    Ok(())
}

/// `additional_references` reaches admission exactly as the caller wrote it,
/// unlike payload references, which are folded before they get there: this is
/// the path a repeated declaration can reach.
#[tokio::test]
async fn duplicate_additional_refs_are_admitted_once() -> TestResult {
    let decisions = Arc::new(ReadDecisions::default());
    let w = World::with_hook("engine_refs_duplicates", Some(decisions.clone())).await;
    let (first, second, third) = (
        w.fact(w.owner).await?,
        w.fact(w.owner).await?,
        w.fact(w.owner).await?,
    );
    let payload = payload("duplicate-refs", Vec::new());
    let sidecars = [SidecarPayload::fact(payload.clone())];

    let authorized = w
        .engine
        .authorize_fact_ingest(
            &w.authz,
            draft(&payload).with_additional_references(vec![
                fact_target(first),
                fact_target(second),
                fact_target(first),
                fact_target(third),
            ]),
            &sidecars,
        )
        .await?;

    assert!(authorized.links().origins().is_empty());
    assert_eq!(
        authorized.links().references(),
        &[fact_target(first), fact_target(second), fact_target(third)]
    );
    // Four declarations, three distinct endpoints: the repeat is admitted
    // once, so it is decided once.
    assert_eq!(decisions.0.load(std::sync::atomic::Ordering::SeqCst), 3);
    Ok(())
}

#[tokio::test]
async fn authorized_sidecars_cannot_change_reference_declaration() -> TestResult {
    let w = World::new("engine_refs_bound").await;
    let (first, second) = (w.fact(w.owner).await?, w.fact(w.owner).await?);
    let admitted = payload("bound-sidecars", vec![fact_target(first)]);
    let sidecars = [SidecarPayload::fact(admitted.clone())];

    let authorized = w
        .engine
        .authorize_fact_ingest(&w.authz, draft(&admitted), &sidecars)
        .await?;

    let bound = authorized
        .sidecar_payloads()
        .first()
        .expect("authorization binds the typed Fact sidecar")
        .references();
    assert_eq!(bound[0].target, fact_target(first));
    assert_ne!(bound[0].target, fact_target(second));
    Ok(())
}

#[tokio::test]
async fn fact_reference_wrong_stored_kind_writes_nothing() -> TestResult {
    let w = World::new("engine_refs_wrong_kind").await;
    let origin = w.fact(w.owner).await?;
    let abstraction = TestDerived::abstraction(origin)
        .create(&w.pg, w.owner)
        .await?
        .memory_id;
    let before = w.engine.memory_counts(&w.authz).await?;

    let error = async {
        let authorized = w
            .engine
            .authorize_fact_ingest(
                &w.authz,
                draft(&payload("wrong-kind", Vec::new())).with_refs(vec![abstraction.into_inner()]),
                &[],
            )
            .await?;
        w.engine.ingest_authorized_fact(&authorized).await
    }
    .await
    .expect_err("a raw Fact endpoint must match the stored kind");

    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert_eq!(w.engine.memory_counts(&w.authz).await?, before);
    Ok(())
}

#[tokio::test]
async fn fact_reference_unreadable_writes_nothing() -> TestResult {
    let w = World::new("engine_refs_unreadable").await;
    let foreign = w.fact(OwnerId::from(UserId::new(Uuid::now_v7()))).await?;
    let before = w.engine.memory_counts(&w.authz).await?;

    let error = async {
        let authorized = w
            .engine
            .authorize_fact_ingest(
                &w.authz,
                draft(&payload("unreadable", Vec::new())).with_refs(vec![foreign.into_inner()]),
                &[],
            )
            .await?;
        w.engine.ingest_authorized_fact(&authorized).await
    }
    .await
    .expect_err("an unreadable target must fail before persistence");

    assert_eq!(error.code, ErrorCode::Forbidden);
    assert_eq!(w.engine.memory_counts(&w.authz).await?, before);
    Ok(())
}
