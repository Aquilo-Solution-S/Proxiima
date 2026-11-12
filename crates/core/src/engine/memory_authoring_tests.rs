use std::sync::Arc;

use crate::authz::AuthzContext;
use crate::error::ErrorCode;
use crate::{
    AbstractionPayload, AgentDerivationV1, EntityKind, InterpretationV1, MemoryKind, OwnerId,
    SpiiceRegistry, UserId,
};

use super::*;

fn owner() -> OwnerId {
    OwnerId::from(UserId::new(uuid::Uuid::now_v7()))
}

fn engine() -> Engine {
    Engine::new(
        SpiiceRegistry::new().freeze_or_panic_for_tests(),
        crate::StoragePorts::rejecting_for_tests(),
    )
}

fn derivation_sidecar() -> SidecarPayload {
    SidecarPayload::abstraction(AgentDerivationV1 {
        title: "Derived".into(),
        body: "Body".into(),
        tags: Vec::new(),
        idempotency_key: None,
        source_memory_ids: Vec::new(),
        model_id: "test-model".into(),
        client_name: "test".into(),
        client_version: "1".into(),
    })
}

fn request(owner: OwnerId, derived_from: &[EdgeEndpoint]) -> RawDerivedTestRequest<'_> {
    RawDerivedTestRequest {
        memory_id: MemoryId::new(uuid::Uuid::now_v7()),
        owner,
        kind: EntityKind::Abstraction,
        text: "body".into(),
        schema_id: SchemaId::new(<AgentDerivationV1 as AbstractionPayload>::SCHEMA_ID.into()),
        schema_version: SchemaVersion::new(
            <AgentDerivationV1 as AbstractionPayload>::SCHEMA_VERSION,
        ),
        operator_id: OperatorId::new(uuid::Uuid::now_v7()),
        input_contract_id: InputContractId::new(uuid::Uuid::now_v7()),
        sidecar_payload: derivation_sidecar(),
        derived_from,
        extra_refs: &[],
        supersedes: None,
        lexical_language: None,
    }
}

/// A memory of another store joins local origins when the phase admits
/// its kind, whoever builds the request; an interpretation's other-store
/// subjects have no kind rule.
#[test]
fn other_store_origins_are_of_an_admitted_kind() {
    use crate::storage_ports::other_stores::ReadOrigin;
    let fact_id = crate::new_memory_uuid(5);
    let abstraction_id = crate::new_memory_uuid(5);
    let perspective_id = crate::new_memory_uuid(5);
    let facts = [EdgeEndpoint::memory(
        EntityKind::Fact,
        MemoryId::new(crate::new_memory_uuid(3)),
    )];
    let mixed = admitted_external_origins(
        OperatorPhase::ToA,
        &facts,
        &[
            ReadOrigin::for_tests(fact_id, EntityKind::Fact),
            ReadOrigin::for_tests(abstraction_id, EntityKind::Abstraction),
        ],
    )
    .expect("Facts and Abstractions beside Facts");
    assert_eq!(mixed, vec![fact_id, abstraction_id]);
    let err = admitted_external_origins(
        OperatorPhase::ToA,
        &facts,
        &[ReadOrigin::for_tests(
            perspective_id,
            EntityKind::Perspective,
        )],
    )
    .expect_err("a Perspective is no origin");
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains(&perspective_id.to_string()), "{err:?}");
    let abstractions = [EdgeEndpoint::memory(
        EntityKind::Abstraction,
        MemoryId::new(crate::new_memory_uuid(3)),
    )];
    admitted_external_origins(
        OperatorPhase::AtoP,
        &abstractions,
        &[ReadOrigin::for_tests(fact_id, EntityKind::Fact)],
    )
    .expect_err("a Perspective derives from Abstractions only");
    let subjects = admitted_external_origins(
        OperatorPhase::AtoP,
        &[],
        &[ReadOrigin::for_tests(
            perspective_id,
            EntityKind::Perspective,
        )],
    )
    .expect("an interpretation's subject");
    assert_eq!(subjects, [perspective_id]);
}

/// The output kind alone names the phase; mixed Fact and Abstraction
/// origins are one to-Abstraction derivation.
#[test]
fn the_output_kind_names_the_derivation_phase() {
    let fact = EdgeEndpoint::memory(EntityKind::Fact, MemoryId::new(crate::new_memory_uuid(3)));
    let abstraction = EdgeEndpoint::memory(
        EntityKind::Abstraction,
        MemoryId::new(crate::new_memory_uuid(3)),
    );
    assert_eq!(
        derivation_phase(EntityKind::Abstraction, &[fact, abstraction]).expect("mixed"),
        OperatorPhase::ToA
    );
    assert_eq!(
        derivation_phase(EntityKind::Perspective, &[abstraction]).expect("A to P"),
        OperatorPhase::AtoP
    );
    assert_eq!(
        derivation_phase(EntityKind::Perspective, &[]).expect("interpretation"),
        OperatorPhase::AtoP
    );
    for (kind, origins) in [
        (EntityKind::Perspective, vec![fact]),
        (EntityKind::Perspective, vec![abstraction, fact]),
        (EntityKind::Abstraction, vec![]),
        (EntityKind::Fact, vec![fact]),
    ] {
        let err = derivation_phase(kind, &origins).expect_err("refused");
        assert_eq!(err.code, ErrorCode::InvalidArgument);
    }
}

/// A to-Abstraction derive that names its inputs carries a well-formed
/// manifest and reaches storage. There is no batch input left on the
/// request for a validator to demand: the derived-write API declares
/// origins and nothing else about the operator run.
#[tokio::test]
async fn author_derived_toa_with_origins_passes_manifest_validation() {
    let engine = engine();
    let owner = owner();
    let permit = OwnerWritePermit::new(
        owner,
        crate::storage_ports::WriteAuthority::Memory(crate::MemoryKind::Perspective),
        None,
        crate::BoundReadSet::default(),
    );
    let origins = [EdgeEndpoint::memory(
        EntityKind::Fact,
        MemoryId::new(uuid::Uuid::now_v7()),
    )];
    let err = engine
        .author_derived(&permit, request(owner, &origins), &[])
        .await
        .expect_err("the fake storage port refuses every write");
    assert!(
        !matches!(&err, StorageError::ConstraintViolation(_)),
        "a well-formed to-Abstraction manifest must reach storage: {err}"
    );
}

/// A write that declares no derivation has no operator invocation to
/// prove — the manifest's nonempty-inputs obligation is about
/// derivations, and an interpretation Perspective is not one. The
/// write proceeds to storage (which the test engine refuses) rather
/// than being rejected as a malformed manifest.
#[tokio::test]
async fn a_write_with_no_origins_carries_no_operator_manifest() {
    let engine = engine();
    let owner = owner();
    let permit = OwnerWritePermit::new(
        owner,
        crate::storage_ports::WriteAuthority::Memory(crate::MemoryKind::Perspective),
        None,
        crate::BoundReadSet::default(),
    );
    let mut req = request(owner, &[]);
    req.kind = EntityKind::Perspective;
    let err = engine
        .author_derived(&permit, req, &[])
        .await
        .expect_err("the fake storage port refuses every write");
    assert!(
        !matches!(&err, StorageError::ConstraintViolation(msg) if msg.contains("inputs")),
        "an origin-free write must not be judged as an operator invocation: {err}"
    );
}

#[tokio::test]
async fn derive_memory_denies_denied_context() {
    let engine = engine();
    let owner = owner();
    let err = engine
        .derive_memory(
            &AuthzContext::denied_for_owner(&owner),
            request(owner, &[]).into_typed().expect("synthetic request"),
        )
        .await
        .expect_err("denied context must fail before storage");

    assert_eq!(err.code, ErrorCode::Forbidden);
}

#[tokio::test]
async fn write_embedding_spaces_preserve_schema_exclusion_and_route_policy() {
    let owner = owner();
    let router = Arc::new(crate::test_fixtures::TestEmbeddingRouter::default());
    router.refuse(owner, "owner route is not configured");
    let engine = engine().with_embedding_router(router.clone());
    assert!(
        engine
            .embedding_spaces_for_write(&owner, "core/write-act-v1")
            .await
            .expect("a nonembeddable schema does not need a route")
            .is_empty()
    );
    let error = engine
        .embedding_spaces_for_write(&owner, "core/agent-derivation-v1")
        .await
        .expect_err("routing policy still refuses an embeddable write");
    assert_eq!(error.code, ErrorCode::Internal);
    router.set_owner(owner, None);
    assert!(
        engine
            .embedding_spaces_for_write(&owner, "core/agent-derivation-v1")
            .await
            .expect("a route naming no clients requests no jobs")
            .is_empty()
    );
}

/// Public origin declarations carry row IDs, not caller-selected kinds.
#[test]
fn no_public_write_input_accepts_an_edge_kind() {
    let source = MemoryId::new(uuid::Uuid::now_v7());
    let req = DerivedMemory::abstraction(
        MemoryTarget::Series(SeriesHandle::new(uuid::Uuid::now_v7())),
        owner(),
        "conclusion",
        AgentDerivationV1 {
            title: "conclusion".into(),
            body: "body".into(),
            tags: Vec::new(),
            idempotency_key: None,
            source_memory_ids: vec![source.into_inner()],
            model_id: "fixture".into(),
            client_name: "fixture".into(),
            client_version: "1".into(),
        },
        [source, source],
        DerivationIdentity::new(
            OperatorId::new(uuid::Uuid::now_v7()),
            InputContractId::new(uuid::Uuid::now_v7()),
        ),
    )
    .expect("typed public constructor");
    let targets: &[MemoryId] = &req.origins;
    assert_eq!(
        targets,
        [source],
        "duplicate origins do not duplicate invocation inputs"
    );
    assert_eq!(req.origins.len(), 1);
}

#[test]
fn guided_interpretation_keeps_exact_goal_versions_and_deduplicates() {
    let first = crate::GoalId::new(uuid::Uuid::now_v7());
    let second = crate::GoalId::new(uuid::Uuid::now_v7());
    let memory = DerivedMemory::guided_interpretation(
        MemoryTarget::Series(SeriesHandle::new(uuid::Uuid::now_v7())),
        owner(),
        "claim",
        InterpretationV1 {
            claim: "claim".into(),
            confidence: 80,
            subject_memory_ids: vec![uuid::Uuid::now_v7()],
            subject_kinds: vec![MemoryKind::Fact],
            model_id: "fixture".into(),
            client_name: "fixture".into(),
            client_version: "1".into(),
        },
        [first, first, second],
    );
    assert_eq!(memory.guided_by, vec![first, second]);
}

/// What storage is handed, verbatim: the `derived_from` declaration
/// arrives as `origins`, the payload's declared fields arrive as
/// `references`, and the supersession target arrives as a pointer
/// with no edge attached.
#[tokio::test]
async fn a_node_write_hands_storage_its_origins_and_references() {
    let recorder = Arc::new(RecordingAuthoring::default());
    let engine = Engine::new(
        SpiiceRegistry::new().freeze_or_panic_for_tests(),
        crate::StoragePorts::rejecting_with_memory_authoring(recorder.clone()),
    );
    let owner = owner();
    let permit = OwnerWritePermit::new(
        owner,
        crate::storage_ports::WriteAuthority::Memory(crate::MemoryKind::Perspective),
        None,
        crate::BoundReadSet::default(),
    );
    let prior = MemoryId::new(uuid::Uuid::now_v7());
    let origins = [
        EdgeEndpoint::memory(EntityKind::Fact, MemoryId::new(uuid::Uuid::now_v7())),
        EdgeEndpoint::memory(EntityKind::Fact, MemoryId::new(uuid::Uuid::now_v7())),
    ];
    let references = [EdgeEndpoint::memory(
        EntityKind::Abstraction,
        MemoryId::new(uuid::Uuid::now_v7()),
    )];
    let mut req = request(owner, &origins);
    req.supersedes = Some(prior);
    engine
        .author_derived(&permit, req, &references)
        .await
        .expect("the recording port accepts the write");

    let seen = recorder.seen.lock().expect("recorder");
    let seen = seen.as_ref().expect("one write recorded");
    assert_eq!(seen.origins, origins);
    assert_eq!(seen.references, references);
    assert_eq!(seen.supersedes, Some(prior));
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedWrite {
    origins: Vec<EdgeEndpoint>,
    references: Vec<EdgeEndpoint>,
    supersedes: Option<MemoryId>,
}

#[derive(Debug, Default)]
struct RecordingAuthoring {
    seen: std::sync::Mutex<Option<RecordedWrite>>,
}

#[async_trait::async_trait]
impl crate::MemoryAuthoringPort for RecordingAuthoring {
    async fn author_derived(
        &self,
        req: &AuthorDerivedRequest<'_>,
        _permit: &OwnerWritePermit,
        _proof: crate::storage_ports::OperatorWriteProof,
    ) -> Result<AuthorDerivedOutcome, StorageError> {
        *self.seen.lock().expect("recorder") = Some(RecordedWrite {
            origins: req.origins.to_vec(),
            references: req.references.to_vec(),
            supersedes: req.supersedes,
        });
        Ok(AuthorDerivedOutcome {
            memory_id: req.memory_id,
            idempotent_replay: false,
            forgotten: false,
            edge_count: req.origins.len() + req.references.len(),
            embedding_deferred: false,
        })
    }

    async fn load_memory_kinds(
        &self,
        _scope: Option<&crate::OwnerScope>,
        _owner: &OwnerId,
        _memory_ids: &[MemoryId],
    ) -> Result<Vec<crate::MemoryKindRow>, StorageError> {
        Ok(Vec::new())
    }

    async fn forget_memory(
        &self,
        _permit: &OwnerWritePermit,
        _memory_id: MemoryId,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    async fn restore_memory(
        &self,
        _permit: &OwnerWritePermit,
        _memory_id: MemoryId,
        _embedding_spaces: &[crate::EmbeddingSpace],
    ) -> Result<(), StorageError> {
        Ok(())
    }
}
