use super::*;
use crate::AuthzContext;
use crate::error::ErrorCode;
use crate::ids::UserId;
use crate::{
    AuthPath, FactPayload, OwnerId, PayloadKeyBuilder, PayloadReference, ReferenceBinding,
    SchemaId, SpiiceRegistry,
};
use serde::{Deserialize, Serialize};

fn test_owner() -> OwnerId {
    OwnerId::from(UserId::new(uuid::Uuid::now_v7()))
}

#[derive(Debug, Serialize, Deserialize)]
struct TestFact {
    fact_id: String,
}

impl FactPayload for TestFact {
    const SCHEMA_ID: &'static str = "test/ingest-stamp-fact";
    const SCHEMA_VERSION: u32 = 1;

    fn receipt_key(&self) -> Vec<u8> {
        let mut key = PayloadKeyBuilder::new(Self::SCHEMA_ID, Self::SCHEMA_VERSION);
        key.field_str("fact_id", &self.fact_id);
        key.finish()
    }

    fn render(&self) -> String {
        self.fact_id.clone()
    }
}

impl crate::PayloadProjection for TestFact {
    const KEY_FIELDS: &'static [crate::KeyField<Self>] = &[];
    fn display(&self) -> crate::PayloadDisplay {
        crate::PayloadDisplay {
            body: Some(self.fact_id.clone()),
            ..Default::default()
        }
    }
    fn durable(&self) -> Result<crate::DurablePayload, String> {
        let mut builder = crate::DurablePayloadBuilder::new();
        builder.field("fact_id", &self.fact_id)?;
        Ok(builder.finish())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReferencedTestFact {
    fact_id: String,
    targets: Vec<EdgeEndpoint>,
}

impl FactPayload for ReferencedTestFact {
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

impl crate::PayloadProjection for ReferencedTestFact {
    const KEY_FIELDS: &'static [crate::KeyField<Self>] = &[];
    fn display(&self) -> crate::PayloadDisplay {
        crate::PayloadDisplay {
            body: Some(self.fact_id.clone()),
            ..Default::default()
        }
    }
    fn durable(&self) -> Result<crate::DurablePayload, String> {
        let mut builder = crate::DurablePayloadBuilder::new();
        builder
            .field("fact_id", &self.fact_id)?
            .field("targets", &self.targets)?;
        Ok(builder.finish())
    }
}

fn referenced_draft(payload: &ReferencedTestFact) -> FactWriteCommand {
    FactWriteCommand::from_payload("test/references", payload)
}

fn reference_engine() -> Engine {
    Engine::compose_or_panic_for_tests(
        crate::StoragePorts::rejecting_for_tests(),
        SpiiceRegistry::add_fact_schema_or_panic_for_tests::<ReferencedTestFact>,
    )
}

#[tokio::test]
async fn raw_fact_refs_cannot_disagree_with_payload_refs() {
    let owner = test_owner();
    let typed = MemoryId::new(uuid::Uuid::now_v7());
    let raw = MemoryId::new(uuid::Uuid::now_v7());
    let payload = ReferencedTestFact {
        fact_id: "raw-mismatch".to_owned(),
        targets: vec![EdgeEndpoint::memory(EntityKind::Fact, typed)],
    };
    let sidecars = [SidecarPayload::fact(payload.clone())];
    let engine = reference_engine();
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);

    let error = engine
        .authorize_fact_ingest(
            &authz,
            referenced_draft(&payload).with_refs(vec![raw.into_inner()]),
            &sidecars,
        )
        .await
        .expect_err("raw refs must not replace typed declarations");

    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[tokio::test]
async fn uow_session_visible_fact_reference_checks_kind() {
    let owner = test_owner();
    let target = MemoryId::new(uuid::Uuid::now_v7());
    let payload = ReferencedTestFact {
        fact_id: "session-kind".to_owned(),
        targets: vec![EdgeEndpoint::memory(EntityKind::Fact, target)],
    };
    let sidecars = [SidecarPayload::fact(payload.clone())];
    let engine = reference_engine();
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);

    let error = engine
        .authorize_fact_ingest_visible(
            &authz,
            referenced_draft(&payload),
            &sidecars,
            &[target],
            &[(target, EntityKind::Abstraction)],
        )
        .await
        .expect_err("session-visible kind mismatch must fail closed");

    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[tokio::test]
async fn non_fact_source_cannot_mint_authorized_fact_write() {
    let owner = test_owner();
    let payload = ReferencedTestFact {
        fact_id: "wrong-source".to_owned(),
        targets: Vec::new(),
    };
    let engine = reference_engine();
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);
    let mut draft = referenced_draft(&payload);
    draft.kind = crate::MemoryKind::Abstraction;

    let error = async {
        let authorized = engine.authorize_fact_ingest(&authz, draft, &[]).await?;
        engine.ingest_authorized_fact(&authorized).await
    }
    .await
    .expect_err("a non-Fact source must not mint a Fact witness");

    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[tokio::test]
async fn malformed_goal_endpoint_stops_before_port() {
    let owner = test_owner();
    let payload = ReferencedTestFact {
        fact_id: "malformed-goal".to_owned(),
        targets: Vec::new(),
    };
    let engine = reference_engine();
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);
    let malformed = EdgeEndpoint {
        kind: EntityKind::Fact,
        entity: crate::EntityRef::Goal(crate::GoalId::new(uuid::Uuid::now_v7())),
    };

    let error = async {
        let authorized = engine
            .authorize_fact_ingest(
                &authz,
                referenced_draft(&payload).with_additional_references(vec![malformed]),
                &[],
            )
            .await?;
        engine.ingest_authorized_fact(&authorized).await
    }
    .await
    .expect_err("a malformed Goal endpoint must fail before persistence");

    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[tokio::test]
async fn authorize_fact_ingest_stamps_draft_owner_from_permit() {
    let owner = test_owner();
    let mut registry = SpiiceRegistry::new();
    registry.add_fact_schema_or_panic_for_tests::<TestFact>();
    let engine = Engine::new(
        registry.freeze_or_panic_for_tests(),
        crate::StoragePorts::rejecting_for_tests(),
    );
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);
    let draft = FactWriteCommand::from_payload(
        "test/source",
        &TestFact {
            fact_id: "fact-1".to_string(),
        },
    );

    let authorized = engine
        .authorize_fact_ingest(&authz, draft, &[])
        .await
        .expect("single-owner host context should authorize ingest");

    assert_eq!(authorized.permit().owner(), &owner);
}

#[tokio::test]
async fn fact_ingest_refuses_a_goal_change_a_goal_write_did_not_make() {
    let owner = test_owner();
    let engine = Engine::new(
        SpiiceRegistry::new().freeze_or_panic_for_tests(),
        crate::StoragePorts::rejecting_for_tests(),
    );
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);
    let forged = crate::GoalChangeV1 {
        series: uuid::Uuid::now_v7(),
        goal: crate::GoalId::new(uuid::Uuid::now_v7()),
        prior: None,
        revision: u32::MAX,
        state: crate::goal_write::GoalState::Abandoned,
        change: crate::GoalChange::Abandon,
    };
    let draft = FactWriteCommand::from_payload("test/source", &forged);

    let error = engine
        .authorize_fact_ingest(&authz, draft, &[SidecarPayload::fact(forged)])
        .await
        .expect_err("only a goal write may record a goal change");

    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[tokio::test]
async fn authorize_fact_ingest_denies_a_context_without_access() {
    let mut registry = SpiiceRegistry::new();
    registry.add_fact_schema_or_panic_for_tests::<TestFact>();
    let engine = Engine::new(
        registry.freeze_or_panic_for_tests(),
        crate::StoragePorts::rejecting_for_tests(),
    );
    let draft = FactWriteCommand::from_payload(
        "test/source",
        &TestFact {
            fact_id: "fact-1".to_string(),
        },
    );

    let err = engine
        .authorize_fact_ingest(&crate::test_fixtures::no_access_context(), draft, &[])
        .await
        .expect_err("a context without access must fail");

    assert_eq!(err.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn a_context_without_access_precedes_invalid_fact_kind() {
    let mut registry = SpiiceRegistry::new();
    registry.add_fact_schema_or_panic_for_tests::<TestFact>();
    let engine = Engine::new(
        registry.freeze_or_panic_for_tests(),
        crate::StoragePorts::rejecting_for_tests(),
    );
    let mut draft = FactWriteCommand::from_payload(
        "test/source",
        &TestFact {
            fact_id: "fact-1".to_owned(),
        },
    );
    draft.kind = crate::MemoryKind::Abstraction;

    let error = engine
        .authorize_fact_ingest(&crate::test_fixtures::no_access_context(), draft, &[])
        .await
        .expect_err("authorization must fail before validating the supplied kind");

    assert_eq!(error.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn authorize_fact_with_citation_denies_a_context_without_access() {
    let mut registry = SpiiceRegistry::new();
    registry.add_fact_schema_or_panic_for_tests::<TestFact>();
    let engine = Engine::new(
        registry.freeze_or_panic_for_tests(),
        crate::StoragePorts::rejecting_for_tests(),
    );
    let draft = FactWriteCommand::from_payload(
        "test/source",
        &TestFact {
            fact_id: "fact-1".to_string(),
        },
    );
    let cited_object = InlineCitedObjectDraft {
        schema_id: SchemaId::new("test/cited-object".into()),
        schema_version: SchemaVersion::new(1),
        payload_bytes: Vec::new(),
    };

    let err = engine
        .authorize_fact_with_citation(
            &crate::test_fixtures::no_access_context(),
            draft,
            cited_object,
            &[],
        )
        .await
        .expect_err("a context without access must fail before schema validation");

    assert_eq!(err.code, ErrorCode::Forbidden);
}

// ── publication resolution (review R6) ──────────────────────────────

/// An engine over the probe registry with a bound deployment source —
/// the only shape in which a listenable schema can be admitted at all.
fn listenable_engine() -> Engine {
    let source = crate::publication::PublicationSource::new("urn:proxiima:r6-tests")
        .expect("a URN is an absolute source");
    Engine::new(
        crate::test_fixtures::probe_registry(),
        crate::StoragePorts::rejecting_for_tests(),
    )
    .try_with_publication_config(crate::publication::PublicationConfig::new(source))
    .expect("a bound source boots")
}

fn listenable_probe(note: &str) -> crate::test_fixtures::ListenableProbeV1 {
    crate::test_fixtures::ListenableProbeV1 {
        probe_id: uuid::Uuid::now_v7(),
        note: note.to_owned(),
    }
}

fn listenable_draft(payload: &crate::test_fixtures::ListenableProbeV1) -> FactWriteCommand {
    FactWriteCommand::from_payload("probe/source", payload)
}

/// The capture is assembled from the DEPLOYMENT and the PERMIT, never
/// from the draft: the source is the configured installation identity,
/// the owner is the one authorization resolved, and the model label is
/// the one the authenticated edge certified.
#[tokio::test]
async fn a_listenable_admission_captures_the_deployment_source_permit_owner_and_trusted_model() {
    let owner = test_owner();
    let engine = listenable_engine();
    let payload = listenable_probe("the quay is sound");
    let sidecars = [SidecarPayload::fact(payload.clone())];
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer)
        .with_trusted_model_id("runner/pinned")
        .expect("a valid operator label");

    let authorized = engine
        .authorize_fact_ingest(&authz, listenable_draft(&payload), &sidecars)
        .await
        .expect("a typed listenable write authorizes");

    let plan = authorized
        .publication()
        .expect("a listenable schema carries a capture plan");
    assert_eq!(plan.draft.source.as_str(), "urn:proxiima:r6-tests");
    assert_eq!(
        plan.draft.owner, owner,
        "the permit's owner, not the draft's"
    );
    assert_eq!(plan.draft.model_id.as_deref(), Some("runner/pinned"));
    assert_eq!(plan.draft.schema_id.as_str(), "probe/listenable-v1");
    assert_eq!(
        plan.draft.data,
        serde_json::to_value(&payload).expect("the probe serializes"),
        "the export snapshot is the typed payload's own serde form"
    );
    assert_eq!(plan.limits, engine.publication_config().limits);
}

/// No certified model identity means no model identity on the wire.
///
/// The only value `resolve_publication` reads is
/// `AuthzContext::trusted_model_id`. The caller-supplied `model_id`
/// label lives on [`crate::catalog::ToolCaller`] and is never consulted
/// here, so an unauthenticated deployment publishes `None` rather than
/// whatever a caller wrote in its arguments.
#[tokio::test]
async fn an_uncertified_edge_captures_no_model_identity() {
    let owner = test_owner();
    let engine = listenable_engine();
    let payload = listenable_probe("no certified runner");
    let sidecars = [SidecarPayload::fact(payload.clone())];
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);
    assert_eq!(authz.trusted_model_id(), None);

    let authorized = engine
        .authorize_fact_ingest(&authz, listenable_draft(&payload), &sidecars)
        .await
        .expect("a typed listenable write authorizes");

    assert_eq!(
        authorized
            .publication()
            .expect("still captured")
            .draft
            .model_id,
        None
    );
}

/// A listenable Fact written through the untyped receipt-only route has
/// no typed payload to export, so there is nothing to publish. Refused
/// rather than published empty: a consumer that received an event with
/// no data could not tell it from a bug.
#[tokio::test]
async fn an_untyped_listenable_write_is_refused() {
    let owner = test_owner();
    let engine = listenable_engine();
    let payload = listenable_probe("no sidecar");
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);

    let err = engine
        .authorize_fact_ingest(&authz, listenable_draft(&payload), &[])
        .await
        .expect_err("a listenable schema without its typed payload must refuse");

    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(
        err.message.contains("probe/listenable-v1"),
        "the refusal names the schema: {}",
        err.message
    );
}

/// One admission, one event. Two typed payloads of the same listenable
/// schema give the capture no way to choose, and choosing the first
/// would silently drop the second.
#[tokio::test]
async fn the_same_listenable_schema_supplied_twice_is_refused() {
    let owner = test_owner();
    let engine = listenable_engine();
    let payload = listenable_probe("twice");
    let sidecars = [
        SidecarPayload::fact(payload.clone()),
        SidecarPayload::fact(listenable_probe("and again")),
    ];
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);

    let err = engine
        .authorize_fact_ingest(&authz, listenable_draft(&payload), &sidecars)
        .await
        .expect_err("two payloads of one listenable schema must refuse");

    assert!(
        err.message.contains("supplied twice"),
        "unexpected message: {}",
        err.message
    );
}
