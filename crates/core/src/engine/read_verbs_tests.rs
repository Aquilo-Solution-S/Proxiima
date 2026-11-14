use crate::authz::{AuthPath, AuthzContext};
use crate::error::ErrorCode;
use crate::verbs::query::{
    MemorySearchRequest, SearchMode, SearchOrder, SupersessionStatus, TagMatch,
};
use crate::{Engine, GroupId, MemoryId, OwnerId, SpiiceRegistry, UserId};

type ResolvedAuthz = AuthzContext;

use super::{
    FactCitationReadRequest, FactsCitingObjectReadRequest, GetGraphReadRequest,
    GetMemoryReadRequest, SearchReadRequest,
};

fn engine() -> Engine {
    Engine::new(
        SpiiceRegistry::new().freeze_or_panic_for_tests(),
        crate::StoragePorts::rejecting_for_tests(),
    )
}

fn tag_probe_ingress(
    _payload: &serde_json::Value,
) -> Result<crate::verbs::schema::ProtocolPayload, String> {
    Err("tag probe ingress is not used".to_owned())
}

const TAG_PROBE_SCHEMA: crate::spiice::contract::SchemaContract =
    crate::spiice::contract::SchemaContract {
        id: crate::spiice::contract::SchemaRef::new("tag-probe", "note", 1),
        kind: crate::verbs::schema::PayloadKind::Fact,
        sidecar_table: Some("proxiima_core.tag_probe_v1"),
        search: crate::spiice::contract::SearchProjectionDecl::None {
            why: "the tag probe has no lexical projection",
            tag_column: Some("labels"),
        },
        embedding: crate::spiice::contract::EmbeddingRecipe::Never {
            why: "the tag probe is metadata-only",
        },
        transfer: crate::spiice::contract::TransferRule::StaysOnKey,
        provenance: crate::spiice::contract::Provenance::None,
        surfaces: &[crate::spiice::spiice0::memory_sidecar(
            "proxiima_core.tag_probe_v1",
            None,
            None,
        )],
        natural_key_columns: crate::NaturalKeyDeclaration::EMPTY,
    };

static TAG_PROBE_SPIICE: crate::spiice::contract::SpiiceContract =
    crate::spiice::contract::SpiiceContract {
        retired_facts: &[],
        spiice_id: "tag-probe",
        ordinal: 92,
        schemas: &[TAG_PROBE_SCHEMA],
        state_surfaces: &[],
        scopes: &[],
        kernel_surfaces: &[],
        tools: &[],
        resources: &[],
        bespoke_erase_legs: &[],
        bespoke_transfer_legs: &[],
        projection: crate::spiice::contract::ProjectionDecl::None {
            why: "the tag probe has no search projection",
        },
        evolutions: &[],
    };

fn tag_probe_engine() -> Engine {
    let mut registry = SpiiceRegistry::new();
    registry.add_contract_or_panic_for_tests(&TAG_PROBE_SPIICE);
    let schema_id = crate::SchemaId::new("tag-probe/note-v1".to_owned());
    let schema_version = crate::SchemaVersion::new(1);
    registry.schemas.push(crate::verbs::schema::SchemaInfo {
        schema_id: schema_id.clone(),
        schema_version,
        kind: crate::verbs::schema::PayloadKind::Fact,
        filter_keys: Vec::new(),
        sidecar_table: Some("proxiima_core.tag_probe_v1".to_owned()),
        natural_key_columns: Vec::new(),
        tombstone: None,
        has_typed_ingress: true,
        listenable: false,
    });
    registry
        .protocol_ingress
        .push(crate::verbs::schema::ProtocolPayloadIngressEntry {
            schema_id,
            schema_version,
            kind: crate::verbs::schema::PayloadKind::Fact,
            ingress: tag_probe_ingress,
            json_schema: None,
            natural_key_declaration: crate::NaturalKeyDeclaration::EMPTY,
        });
    Engine::new(
        registry.freeze_or_panic_for_tests(),
        crate::StoragePorts::rejecting_for_tests(),
    )
}

/// #479 item 3: every core memory schema displays the field its search
/// declaration indexes as its body, and its declared `tag_column` as its
/// tags.
#[test]
fn core_schemas_display_their_declared_search_fields() {
    let specs = engine().memory_schema_specs();
    let fields = |id: &str| {
        let spec = specs
            .iter()
            .find(|spec| spec.schema_id.as_str() == id)
            .unwrap_or_else(|| panic!("{id} is a registered memory schema"));
        (spec.body_field.as_deref(), spec.tag_field.as_deref())
    };
    assert_eq!(fields("core/agent-note-v1"), (Some("body"), Some("tags")));
    assert_eq!(fields("core/utterance-v1"), (Some("text"), None));
    assert_eq!(fields("core/interpretation-v1"), (Some("claim"), None));
    assert_eq!(
        fields("core/agent-derivation-v1"),
        (Some("body"), Some("tags"))
    );
}

#[test]
fn non_search_schema_uses_its_declared_tag_field() {
    let engine = tag_probe_engine();
    assert!(
        engine
            .registry
            .search_projections()
            .iter()
            .all(|projection| projection.schema_id.as_str() != "tag-probe/note-v1")
    );
    let specs = engine.memory_schema_specs();
    let spec = specs
        .iter()
        .find(|spec| spec.schema_id.as_str() == "tag-probe/note-v1")
        .expect("tag probe schema is registered");
    assert_eq!(spec.tag_field.as_deref(), Some("labels"));
    assert_eq!(spec.display_tags(None), Vec::<String>::new());
}

fn owner() -> OwnerId {
    OwnerId::from(UserId::new(uuid::Uuid::now_v7()))
}

fn group_owner() -> OwnerId {
    OwnerId::from(GroupId::new(uuid::Uuid::now_v7()))
}

fn search_req(owner: &OwnerId) -> SearchReadRequest {
    SearchReadRequest {
        search: MemorySearchRequest {
            scope: crate::verbs::query::MemorySearchScope::Owner(*owner),
            read_owners: vec![*owner],
            query: "needle".into(),
            mode: SearchMode::Lexical,
            supersession: SupersessionStatus::HeadsOnly,
            limit: 8,
            kind: None,
            schema_id: None,
            tags: Vec::new(),
            tag_match: TagMatch::Any,
            since: None,
            until: None,
            order: SearchOrder::Relevance,
            min_score: None,
            semantic_weight: None,
            after: None,
            semantic: None,
        },
        include_body: false,
        include_neighbor_edges: false,
    }
}

fn assert_forbidden(err: &crate::ProtocolError) {
    assert_eq!(err.code, ErrorCode::Forbidden);
}

fn granted_authz(owner: &OwnerId) -> ResolvedAuthz {
    AuthzContext::single_owner(owner, AuthPath::HostBearer)
}

#[tokio::test]
async fn search_rejects_out_of_range_floor_weight_and_cursor() {
    use crate::verbs::query::{MAX_RELEVANCE_SEARCH_DEPTH, SearchCursor};

    let owner = owner();
    let engine = engine();
    let authz = granted_authz(&owner);

    let mut bad_floor = search_req(&owner);
    bad_floor.search.min_score = Some(1.5);
    let err = engine
        .search(&authz, &bad_floor)
        .await
        .expect_err("floor above 1.0 must fail");
    assert_eq!(err.code, ErrorCode::InvalidArgument);

    let mut nan_weight = search_req(&owner);
    // Hybrid, so this isolates the bounds rule from the mode rule below.
    nan_weight.search.mode = SearchMode::Hybrid;
    nan_weight.search.semantic_weight = Some(f32::NAN);
    let err = engine
        .search(&authz, &nan_weight)
        .await
        .expect_err("NaN weight must fail");
    assert_eq!(err.code, ErrorCode::InvalidArgument);

    let mut order_mismatch = search_req(&owner);
    order_mismatch.search.after = Some(SearchCursor::Recency {
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        memory_id: MemoryId::new(uuid::Uuid::now_v7()),
        seen: 8,
    });
    let err = engine
        .search(&authz, &order_mismatch)
        .await
        .expect_err("recency cursor with relevance order must fail");
    assert_eq!(err.code, ErrorCode::InvalidArgument);

    let mut too_deep = search_req(&owner);
    too_deep.search.after = Some(SearchCursor::Relevance {
        score_bits: 0.5_f32.to_bits(),
        memory_id: MemoryId::new(uuid::Uuid::now_v7()),
        seen: MAX_RELEVANCE_SEARCH_DEPTH + 1,
    });
    let err = engine
        .search(&authz, &too_deep)
        .await
        .expect_err("relevance depth beyond the bound must fail");
    assert_eq!(err.code, ErrorCode::InvalidArgument);
}

/// Only hybrid fuses a lexical and a semantic component, so only
/// hybrid reads the weight between them; storage discards it in the
/// other two modes. Accepting it there returns a ranking that ignored
/// an argument the caller set. `core_search_memories` has always
/// rejected the pairing — the verb it delegates to, which every
/// embedding host and spiice also enters through, did not.
#[tokio::test]
async fn search_rejects_a_fusion_weight_no_mode_would_use() {
    let owner = owner();
    let engine = engine();
    let authz = granted_authz(&owner);

    for mode in [SearchMode::Lexical, SearchMode::Semantic] {
        let mut req = search_req(&owner);
        req.search.mode = mode;
        req.search.semantic_weight = Some(0.5);
        let err = engine
            .search(&authz, &req)
            .await
            .expect_err("a weight outside hybrid must fail");
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(
            err.message.contains("mode=hybrid"),
            "the error must name the mode that would have used it, got {:?}",
            err.message
        );
    }
}

#[tokio::test]
async fn search_denies_a_context_without_access() {
    let owner = owner();
    let err = engine()
        .search(
            &crate::test_fixtures::no_access_context(),
            &search_req(&owner),
        )
        .await
        .expect_err("a context without access must fail");
    assert_forbidden(&err);
}

#[tokio::test]
async fn search_body_or_neighbor_hydration_requires_viewer_relation() {
    let caller = owner();
    let space = group_owner();
    let mut req = search_req(&space);
    req.include_neighbor_edges = true;
    let err = engine()
        .search(&granted_authz(&caller), &req)
        .await
        .expect_err("neighbor edges require viewer");
    assert_forbidden(&err);
}

#[tokio::test]
async fn get_memory_denies_a_context_without_access() {
    let req = GetMemoryReadRequest {
        memory_id: MemoryId::new(uuid::Uuid::now_v7()),
        include_neighbor_edges: false,
        at: crate::storage_ports::other_stores::ReadAt::Version,
    };
    let err = engine()
        .get_memory(&crate::test_fixtures::no_access_context(), &req)
        .await
        .expect_err("a context without access must fail");
    assert_forbidden(&err);
}

#[tokio::test]
async fn get_memory_absent_or_invisible_is_not_found() {
    let owner = owner();
    let req = GetMemoryReadRequest {
        memory_id: MemoryId::new(uuid::Uuid::now_v7()),
        include_neighbor_edges: false,
        at: crate::storage_ports::other_stores::ReadAt::Version,
    };
    let err = engine()
        .get_memory(&granted_authz(&owner), &req)
        .await
        .expect_err("missing inspect is not-found, not an entry Forbidden");
    assert_eq!(err.code, ErrorCode::NotFound);
    assert!(err.message.contains("memory not found"));
}

#[tokio::test]
async fn embedding_job_diagnostics_refuses_too_many_units_before_storage() {
    let owner = owner();
    let entities = (0..=crate::MAX_EMBEDDING_DIAGNOSTIC_UNITS)
        .map(|_| MemoryId::new(uuid::Uuid::now_v7()))
        .collect::<Vec<_>>();
    let err = engine()
        .embedding_job_diagnostics(&granted_authz(&owner), &owner, &entities)
        .await
        .expect_err("over the bound");
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(err.message.contains("256"), "{}", err.message);
}

#[tokio::test]
async fn embedding_job_diagnostics_of_no_units_is_empty_after_authorization() {
    let owner = owner();
    let rows = engine()
        .embedding_job_diagnostics(&granted_authz(&owner), &owner, &[])
        .await
        .expect("no units");
    assert!(rows.is_empty());
    let err = engine()
        .embedding_job_diagnostics(&crate::test_fixtures::no_access_context(), &owner, &[])
        .await
        .expect_err("a context without access");
    assert_forbidden(&err);
}

#[tokio::test]
async fn get_graph_denies_a_context_without_access() {
    let owner = owner();
    let req = GetGraphReadRequest { owner };
    let err = engine()
        .get_graph(&crate::test_fixtures::no_access_context(), &req)
        .await
        .expect_err("a context without access must fail");
    assert_forbidden(&err);
}

#[tokio::test]
async fn read_fact_citation_denies_a_context_without_access() {
    let req = FactCitationReadRequest {
        fact_memory_id: MemoryId::new(uuid::Uuid::now_v7()),
    };
    let err = engine()
        .read_fact_citation(&crate::test_fixtures::no_access_context(), &req)
        .await
        .expect_err("a context without access must fail");
    assert_forbidden(&err);
}

#[tokio::test]
async fn facts_citing_object_denies_a_context_without_access() {
    let req = FactsCitingObjectReadRequest {
        cited_object_id: uuid::Uuid::now_v7(),
        limit: 50,
        after: None,
    };
    let err = engine()
        .facts_citing_object(&crate::test_fixtures::no_access_context(), &req)
        .await
        .expect_err("a context without access must fail");
    assert_forbidden(&err);
}
