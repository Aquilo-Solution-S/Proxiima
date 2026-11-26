use super::{
    LaneArm, MemoryKind, MergeKind, OwnerLane, PageCursor, SearchMemoriesArgs, SearchMemoriesMode,
    SearchMemoriesSupersession, cursor_positions, decode_cursor, encode_cursor, truncate_body,
    validate_body_max_chars, validate_list_caps, validate_listing_args, validate_score_args,
};
use crate::ToolError;
use crate::catalog::contract::Tool;
use crate::verbs::query::{
    SearchCursor, SearchMode, SearchOrder, TagMatch,
    hybrid_degraded_to_lexical as degraded_to_lexical,
};
use crate::{MemoryId, OwnerId};
use crate::{SpiiceServices, ToolCaller};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn args(mode: SearchMemoriesMode) -> SearchMemoriesArgs {
    SearchMemoriesArgs {
        query: Some("needle".into()),
        mode: Some(mode),
        limit: 8,
        supersession: SearchMemoriesSupersession::HeadsOnly,
        kind: None,
        schema_id: None,
        exclude_schema_ids: Vec::new(),
        tags: Vec::new(),
        tag_match: TagMatch::Any,
        since: None,
        until: None,
        order: SearchOrder::Relevance,
        min_score: None,
        semantic_weight: None,
        include_neighbor_edges: false,
        include_body: false,
        body_max_chars: None,
        spaces: Vec::new(),
        cursor: None,
        stores: None,
    }
}

#[test]
fn listing_requires_recency_and_rejects_query_only_knobs() {
    let mut listing = args(SearchMemoriesMode::Lexical);
    listing.query = None;
    listing.mode = None;
    listing.order = SearchOrder::Relevance;
    assert!(validate_listing_args(&listing, false).is_err());
    listing.order = SearchOrder::Recency;
    listing.min_score = Some(0.1);
    assert!(validate_listing_args(&listing, false).is_err());
    listing.min_score = None;
    listing.semantic_weight = Some(0.5);
    assert!(validate_listing_args(&listing, false).is_err());
    listing.semantic_weight = None;
    listing.mode = Some(SearchMemoriesMode::Lexical);
    assert!(validate_listing_args(&listing, false).is_err());
}

#[test]
fn stores_require_numbers_in_the_store_number_range() {
    for stores in [
        serde_json::json!(["wiki"]),
        serde_json::json!([65536]),
        serde_json::json!([-1]),
    ] {
        assert!(
            serde_json::from_value::<SearchMemoriesArgs>(
                serde_json::json!({"query": "needle", "stores": stores})
            )
            .is_err()
        );
    }
}

/// Another store's hit names the owner its store reported, as a read of that
/// memory does, and an embedder parses the hit back into its type.
#[test]
fn another_stores_hit_names_the_owner_its_store_reported() {
    use crate::storage_ports::other_stores::RemoteHit;
    use crate::verbs::query::EntityKind;

    let owner = crate::OwnerId::new(uuid::Uuid::from_u128(7));
    let hit = super::remote_hit_output(
        1,
        RemoteHit {
            handle: crate::new_memory_uuid(1),
            owner,
            writer: crate::WriterAttribution {
                by: uuid::Uuid::from_u128(3),
                via: None,
            },
            kind: EntityKind::Fact,
            schema_id: "remote/note".into(),
            snippet: "stored remote note".into(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            tags: Vec::new(),
            body: None,
            citation: None,
        },
        false,
        crate::MAX_TEXT_CAP_CHARS,
    );
    let wire = serde_json::to_value(&hit).expect("wire output");
    assert_eq!(wire["space"], serde_json::json!(owner.to_string()));
    assert_eq!(wire["owner"], serde_json::json!(owner.into_inner()));
    assert_eq!(wire["store"], serde_json::json!(1));
    let parsed: super::SearchMemoryOutput =
        serde_json::from_value(wire).expect("an embedder parses the hit");
    assert_eq!(parsed.owner, owner.into_inner());
}

#[test]
fn local_and_remote_search_outputs_keep_the_recorded_writer() {
    use crate::storage_ports::other_stores::RemoteHit;
    use crate::verbs::query::{EntityKind, MemorySearchResult};
    use crate::{SchemaId, WriterAttribution};

    let store = 1;
    for via in [None, Some(uuid::Uuid::from_u128(3))] {
        let writer = WriterAttribution {
            by: uuid::Uuid::from_u128(2),
            via,
        };
        let local = super::search_memory_output(
            &crate::catalog::core_tools::memory_spaces::ResolvedMemorySpace {
                key: "current".into(),
                label: "Current owner".into(),
                owner: crate::OwnerId::new(uuid::Uuid::from_u128(4)),
            },
            MemorySearchResult {
                memory_id: MemoryId::new(crate::new_memory_uuid(0)),
                writer,
                kind: EntityKind::Fact,
                schema_id: SchemaId::new("core/agent-note-v1".into()),
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                occurred_at: Some(time::OffsetDateTime::UNIX_EPOCH),
                snippet: "stored note".into(),
                score: 1.0,
                lexical_score: 1.0,
                similarity_score: 0.0,
            },
            Vec::new(),
            None,
            None,
            None,
        );
        let remote = super::remote_hit_output(
            store,
            RemoteHit {
                handle: crate::new_memory_uuid(store),
                owner: crate::OwnerId::new(uuid::Uuid::from_u128(2)),
                writer,
                kind: EntityKind::Fact,
                schema_id: "remote/note".into(),
                snippet: "stored remote note".into(),
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                tags: Vec::new(),
                body: None,
                citation: None,
            },
            false,
            crate::MAX_TEXT_CAP_CHARS,
        );
        for output in [local, remote] {
            let wire = serde_json::to_value(output).expect("wire output");
            assert_eq!(wire["by"], serde_json::json!(writer.by));
            assert_eq!(wire.get("via"), Some(&serde_json::json!(writer.via)));
        }
    }
}

#[test]
fn a_citing_hit_names_its_cited_object_and_an_uncited_hit_has_no_citation() {
    use crate::storage_ports::other_stores::RemoteHit;
    use crate::verbs::query::{EntityKind, FactCitationReadback, MemorySearchResult};
    use crate::{SchemaId, WriterAttribution};

    let writer = WriterAttribution {
        by: uuid::Uuid::from_u128(2),
        via: None,
    };
    let cited = FactCitationReadback {
        cited_object_id: uuid::Uuid::from_u128(9),
        cited_object_schema_id: SchemaId::new("core/uploaded-blob-v1".into()),
    };
    let local = |citation: Option<&FactCitationReadback>| {
        super::search_memory_output(
            &crate::catalog::core_tools::memory_spaces::ResolvedMemorySpace {
                key: "current".into(),
                label: "Current owner".into(),
                owner: crate::OwnerId::new(uuid::Uuid::from_u128(4)),
            },
            MemorySearchResult {
                memory_id: MemoryId::new(crate::new_memory_uuid(0)),
                writer,
                kind: EntityKind::Fact,
                schema_id: SchemaId::new("core/agent-note-v1".into()),
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                occurred_at: Some(time::OffsetDateTime::UNIX_EPOCH),
                snippet: "stored note".into(),
                score: 1.0,
                lexical_score: 1.0,
                similarity_score: 0.0,
            },
            Vec::new(),
            None,
            None,
            citation,
        )
    };
    let remote = |citation: Option<FactCitationReadback>| {
        super::remote_hit_output(
            1,
            RemoteHit {
                handle: crate::new_memory_uuid(1),
                owner: crate::OwnerId::new(uuid::Uuid::from_u128(2)),
                writer,
                kind: EntityKind::Fact,
                schema_id: "remote/note".into(),
                snippet: "stored remote note".into(),
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                tags: Vec::new(),
                body: None,
                citation,
            },
            false,
            crate::MAX_TEXT_CAP_CHARS,
        )
    };
    let expected = serde_json::json!({
        "cited_object_id": cited.cited_object_id.to_string(),
        "cited_object_schema_id": "core/uploaded-blob-v1",
    });
    for (citing, uncited) in [
        (local(Some(&cited)), local(None)),
        (remote(Some(cited.clone())), remote(None)),
    ] {
        let citing = serde_json::to_value(citing).expect("wire output");
        assert_eq!(citing["citation"], expected);
        let uncited = serde_json::to_value(uncited).expect("wire output");
        assert_eq!(uncited.get("citation"), None);
    }
}

#[test]
fn search_output_schema_requires_the_nullable_caller() {
    let schema = crate::catalog::schema::mcp_output_schema::<super::SearchMemoryOutput>()
        .expect("search output schema");
    let required = schema["required"].as_array().expect("required fields");
    assert!(required.contains(&serde_json::json!("by")));
    assert!(required.contains(&serde_json::json!("via")));
    assert_eq!(schema["properties"]["by"]["type"], "string");
    assert_eq!(
        schema["properties"]["via"]["type"],
        serde_json::json!(["string", "null"])
    );
}

#[test]
fn score_args_reject_out_of_range_and_non_hybrid_weight() {
    let mut floor_too_high = args(SearchMemoriesMode::Hybrid);
    floor_too_high.min_score = Some(1.01);
    assert!(matches!(
        validate_score_args(&floor_too_high),
        Err(ToolError::InvalidInput(message)) if message.contains("min_score")
    ));

    let mut nan_floor = args(SearchMemoriesMode::Hybrid);
    nan_floor.min_score = Some(f32::NAN);
    assert!(validate_score_args(&nan_floor).is_err());

    let mut weight_out_of_range = args(SearchMemoriesMode::Hybrid);
    weight_out_of_range.semantic_weight = Some(-0.1);
    assert!(matches!(
        validate_score_args(&weight_out_of_range),
        Err(ToolError::InvalidInput(message)) if message.contains("semantic_weight")
    ));

    // An explicit weight outside hybrid mode is a contradiction, not a
    // silently ignored knob.
    let mut lexical_weight = args(SearchMemoriesMode::Lexical);
    lexical_weight.semantic_weight = Some(0.5);
    assert!(matches!(
        validate_score_args(&lexical_weight),
        Err(ToolError::InvalidInput(message)) if message.contains("mode=hybrid")
    ));

    let mut valid = args(SearchMemoriesMode::Hybrid);
    valid.min_score = Some(0.25);
    valid.semantic_weight = Some(1.0);
    assert!(validate_score_args(&valid).is_ok());

    let mut omitted_mode = valid;
    omitted_mode.mode = None;
    assert!(validate_score_args(&omitted_mode).is_ok());
}

#[test]
fn cursor_round_trips_and_rejects_foreign_or_garbled_tokens() {
    let cursor = SearchCursor::Relevance {
        score_bits: 0.75_f32.to_bits(),
        memory_id: MemoryId::new(crate::new_memory_uuid(0)),
        seen: 42,
    };
    let token = encode_cursor(&PageCursor::Keyset(cursor), "fp-aaaa");
    assert_eq!(
        decode_cursor(&token, "fp-aaaa").unwrap(),
        PageCursor::Keyset(cursor)
    );

    // Same token replayed against a different query shape.
    match decode_cursor(&token, "fp-bbbb") {
        Err(ToolError::InvalidInput(message)) => {
            assert!(message.contains("does not match this query"), "{message}");
        }
        other => panic!("expected fingerprint mismatch, got {other:?}"),
    }

    // Not base64 / not our envelope.
    for garbage in ["%%%", "bm90LWpzb24"] {
        match decode_cursor(garbage, "fp-aaaa") {
            Err(ToolError::InvalidInput(message)) => {
                assert!(message.contains("malformed cursor"), "{message}");
            }
            other => panic!("expected malformed cursor error, got {other:?}"),
        }
    }
}

#[test]
fn fingerprint_binds_query_presence_and_canonical_exclusions() {
    let lanes = Vec::new();
    let mut query_args = args(SearchMemoriesMode::Lexical);
    query_args.exclude_schema_ids = vec!["b".into(), "a".into(), "a".into()];
    let with_query = super::query_fingerprint(Some("needle"), &query_args, None, None, &lanes);
    let reordered = {
        query_args.exclude_schema_ids = vec!["a".into(), "b".into()];
        super::query_fingerprint(Some("needle"), &query_args, None, None, &lanes)
    };
    assert_eq!(with_query, reordered);
    let without_query = super::query_fingerprint(None, &query_args, None, None, &lanes);
    assert_ne!(with_query, without_query);
    query_args.exclude_schema_ids = vec!["c".into()];
    assert_ne!(
        without_query,
        super::query_fingerprint(None, &query_args, None, None, &lanes)
    );
}

#[test]
fn exclusion_length_is_bounded() {
    let mut request = args(SearchMemoriesMode::Lexical);
    request.exclude_schema_ids = (0..=super::MAX_EXCLUDED_SCHEMA_IDS)
        .map(|index| format!("schema-{index}"))
        .collect();
    assert!(matches!(
        validate_list_caps(&request),
        Err(ToolError::InvalidInput(message)) if message.contains("exclude_schema_ids")
    ));
}

#[tokio::test]
async fn listing_does_not_call_embedding_client() {
    #[derive(Debug)]
    struct CountingEmbedding(AtomicUsize);
    #[async_trait::async_trait]
    impl crate::embedding::EmbeddingClient for CountingEmbedding {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::embedding::LlmError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(vec![0.0; 384])
        }
        fn model_id(&self) -> &'static str {
            "listing-test"
        }
        fn dim(&self) -> usize {
            384
        }
    }
    let client = Arc::new(CountingEmbedding(AtomicUsize::new(0)));
    let bound = crate::embedding::BoundEmbeddingClient::bind(client.clone()).expect("test client");
    let router = Arc::new(crate::test_fixtures::TestEmbeddingRouter::default());
    router.set_default(bound);
    let registry = crate::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let engine = Arc::new(
        crate::Engine::new_or_panic_for_tests(
            registry.clone(),
            crate::StoragePorts::rejecting_for_tests(),
        )
        .with_embedding_router(router),
    );
    let owner = crate::OwnerId::from(crate::UserId::new(uuid::Uuid::now_v7()));
    let authz = crate::AuthzContext::single_owner(&owner, crate::AuthPath::HostBearer);
    let ctx = || {
        crate::test_fixtures::tool_ctx_on(
            owner,
            authz.clone(),
            engine.clone(),
            SpiiceServices::default(),
            ToolCaller::new("test", "test", "0"),
        )
    };
    let mut listing = args(SearchMemoriesMode::Lexical);
    listing.query = None;
    listing.mode = None;
    listing.order = SearchOrder::Recency;
    let listed = <super::SearchMemoriesTool as Tool>::call(ctx(), listing)
        .await
        .expect("a listing passes validation");
    assert_eq!(listed.mode, None, "a listing reports no ranking mode");
    assert_eq!(
        client.0.load(Ordering::SeqCst),
        0,
        "a listing embeds nothing"
    );
    // The control: a hybrid query on the same fixture embeds its query once.
    let _ =
        <super::SearchMemoriesTool as Tool>::call(ctx(), args(SearchMemoriesMode::Hybrid)).await;
    assert_eq!(
        client.0.load(Ordering::SeqCst),
        1,
        "a hybrid query embeds once"
    );
}

#[test]
fn search_mode_and_supersession_accept_mixed_case() {
    assert!(matches!(
        serde_json::from_value::<SearchMemoriesMode>(serde_json::json!("Hybrid")).unwrap(),
        SearchMemoriesMode::Hybrid
    ));
    assert!(matches!(
        serde_json::from_value::<SearchMemoriesMode>(serde_json::json!("SEMANTIC")).unwrap(),
        SearchMemoriesMode::Semantic
    ));
    assert!(matches!(
        serde_json::from_value::<SearchMemoriesMode>(serde_json::json!("lexical")).unwrap(),
        SearchMemoriesMode::Lexical
    ));
    assert!(matches!(
        serde_json::from_value::<SearchMemoriesSupersession>(serde_json::json!("HeadsOnly"))
            .unwrap(),
        SearchMemoriesSupersession::HeadsOnly
    ));
    assert!(matches!(
        serde_json::from_value::<SearchMemoriesSupersession>(serde_json::json!("all")).unwrap(),
        SearchMemoriesSupersession::All
    ));
}

#[test]
fn score_based_spiice_warning_keeps_its_existing_contract() {
    // The public spiice helper retains its score heuristic. Served memory
    // search instead observes route availability, covered by its PG test.
    assert!(degraded_to_lexical(SearchMode::Hybrid, false, false));
    // Hybrid with a real semantic score → healthy.
    assert!(!degraded_to_lexical(SearchMode::Hybrid, false, true));
    // Hybrid with no results at all → a genuine no-match, not degradation.
    assert!(!degraded_to_lexical(SearchMode::Hybrid, true, false));
    // Pure Semantic never reports lexical degradation (no lexical branch runs).
    assert!(!degraded_to_lexical(SearchMode::Semantic, false, false));
    // Lexical is never degraded.
    assert!(!degraded_to_lexical(SearchMode::Lexical, false, false));
}

fn lane(owner: u128, space: Option<&str>) -> OwnerLane {
    let owner = OwnerId::from(crate::UserId::new(uuid::Uuid::from_u128(owner)));
    OwnerLane {
        space: crate::catalog::core_tools::memory_spaces::ResolvedMemorySpace {
            key: owner.to_string(),
            label: owner.to_string(),
            owner,
        },
        arm: space.map_or(LaneArm::Lexical, |model| {
            LaneArm::Semantic(
                crate::SpaceVector::new(
                    crate::EmbeddingSpace::new(model, crate::EmbeddingDim::D1024),
                    vec![0.0; 1024],
                )
                .expect("1024 wide"),
            )
        }),
    }
}

#[test]
fn only_a_fusing_lane_binds_the_hybrid_window() {
    let bind = |lanes: &[OwnerLane], window| super::bind_hybrid_window("fp".into(), lanes, window);
    let mut fusing = lane(2, Some("m"));
    let LaneArm::Semantic(query) = fusing.arm else {
        unreachable!("a semantic lane")
    };
    fusing.arm = LaneArm::Hybrid(query);
    let fusing = [lane(1, None), fusing];
    assert_ne!(bind(&fusing, 200), bind(&fusing, 1_000));
    assert_ne!(bind(&fusing, 200), "fp");
    let degraded = OwnerLane {
        arm: LaneArm::Degraded,
        ..lane(3, None)
    };
    for unfused in [
        Vec::new(),
        vec![lane(1, None)],
        vec![lane(1, Some("m"))],
        vec![degraded],
    ] {
        assert_eq!(bind(&unfused, 200), "fp");
        assert_eq!(bind(&unfused, 1_000), "fp");
    }
}

#[test]
fn a_cursor_of_the_other_merge_kind_is_rejected() {
    let lanes = [lane(1, Some("m"))];
    let keyset = PageCursor::Keyset(crate::verbs::query::SearchCursor::Relevance {
        score_bits: 0.5_f32.to_bits(),
        memory_id: MemoryId::new(uuid::Uuid::from_u128(1)),
        seen: 1,
    });
    assert!(matches!(
        cursor_positions(Some(&keyset), MergeKind::Rank, &lanes),
        Err(ToolError::InvalidInput(message)) if message.contains("malformed cursor")
    ));
    assert_eq!(
        cursor_positions(Some(&keyset), MergeKind::Keyset, &lanes)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn truncate_body_respects_smaller_caller_cap() {
    assert_eq!(truncate_body("abcdef", 3), ("abc".to_string(), true));
}

#[test]
fn zero_body_max_chars_is_rejected_not_hydrated_empty() {
    assert!(validate_body_max_chars(Some(0)).is_err());
    assert!(validate_body_max_chars(Some(1)).is_ok());
    assert!(validate_body_max_chars(None).is_ok());
}

#[test]
fn kind_filter_accepts_only_the_wire_spelling() {
    let parse = |kind: &str| {
        serde_json::from_value::<SearchMemoriesArgs>(
            serde_json::json!({"query": "needle", "kind": kind}),
        )
    };
    for kind in MemoryKind::ALL {
        let args = parse(kind.entity_kind().as_str()).expect("the wire spelling parses");
        assert_eq!(args.kind, Some(kind));
    }
    for bad in ["fact", "FACT", "abstraction", "Goal", "Note"] {
        assert!(parse(bad).is_err(), "{bad} must be refused");
    }
}

#[test]
fn oversized_space_and_tag_lists_are_rejected() {
    let mut too_many_spaces = args(SearchMemoriesMode::Lexical);
    too_many_spaces.spaces = (0..17)
        .map(|i| uuid::Uuid::from_u128(i).to_string())
        .collect();
    assert!(matches!(
        validate_list_caps(&too_many_spaces),
        Err(ToolError::InvalidInput(message)) if message.contains("16 spaces")
    ));

    let mut too_many_tags = args(SearchMemoriesMode::Lexical);
    too_many_tags.tags = (0..17).map(|i| format!("tag-{i}")).collect();
    assert!(matches!(
        validate_list_caps(&too_many_tags),
        Err(ToolError::InvalidInput(message)) if message.contains("16 tags")
    ));

    // Exactly at the cap passes; the caps bound work, they are not
    // off-by-one traps.
    let mut at_cap = args(SearchMemoriesMode::Lexical);
    at_cap.spaces = (0..16)
        .map(|i| uuid::Uuid::from_u128(i).to_string())
        .collect();
    at_cap.tags = (0..16).map(|i| format!("tag-{i}")).collect();
    assert!(validate_list_caps(&at_cap).is_ok());
}

#[test]
fn spaces_dedup_by_resolved_owner_not_raw_key() {
    let subject = crate::UserId::new(uuid::Uuid::now_v7());
    let ctx = crate::catalog::core_tools::memory_spaces::test_ctx::ctx_for(subject, vec![]);
    // `current` and the caller's own bare uuid are two spellings of the
    // same space; a page must not search it twice and return every
    // hit doubled.
    let both_spellings = vec!["current".to_string(), subject.into_inner().to_string()];
    let deduped = super::resolve_search_spaces(&ctx, &both_spellings).expect("valid spaces");
    assert_eq!(deduped.len(), 1, "one owner, one search");
    assert_eq!(deduped[0].owner, OwnerId::from(subject));

    // Both spellings must also continue each other's cursors: the
    // fingerprint is computed over resolved owners, so after dedup
    // it matches the single-spelling query exactly.
    let single = super::resolve_search_spaces(&ctx, &["current".to_string()]).expect("valid space");
    let lexical = |spaces: Vec<super::ResolvedMemorySpace>| -> Vec<OwnerLane> {
        spaces
            .into_iter()
            .map(|space| OwnerLane {
                space,
                arm: LaneArm::Lexical,
            })
            .collect()
    };
    let query_args = args(SearchMemoriesMode::Lexical);
    assert_eq!(
        super::query_fingerprint(Some("needle"), &query_args, None, None, &lexical(deduped)),
        super::query_fingerprint(Some("needle"), &query_args, None, None, &lexical(single)),
    );
}

mod other_stores {
    use std::sync::Arc;

    use futures::future::BoxFuture;

    use super::super::{
        MemoryKind, SearchMemoriesMode, SearchMemoriesOutput, SearchMemoriesSupersession,
        SearchMemoriesTool, SearchMemoryOutput, SearchRanking, UnansweredStore,
        caller_input_schema, merge_store_hits,
    };
    use crate::Tool;
    use crate::storage_ports::other_stores::{
        OtherStores, OtherStoresError, OtherStoresService, RemoteBody, RemoteEdge,
        RemoteEdgeTarget, RemoteHit, RemoteRead, RemoteSearch, RemoteSearchResult, StoreCaller,
        StoreInfo,
    };
    use crate::verbs::query::{EntityKind, SearchMode, SearchOrder, SupersessionStatus, TagMatch};

    fn local_hit() -> SearchMemoryOutput {
        let memory_id = crate::new_memory_uuid(0);
        SearchMemoryOutput {
            memory_id,
            memory: memory_id.to_string(),
            space: "current".into(),
            owner: uuid::Uuid::from_u128(4),
            by: uuid::Uuid::from_u128(10),
            via: None,
            kind: crate::EntityKind::Fact,
            schema_id: "core/agent-note-v1".into(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            occurred_at: Some(time::OffsetDateTime::UNIX_EPOCH),
            snippet: String::new(),
            score: 1.0,
            lexical_score: 1.0,
            similarity_score: 0.0,
            tags: Vec::new(),
            body: None,
            body_truncated: None,
            store: None,
            citation: None,
        }
    }

    fn remote_hit(store: u16) -> RemoteHit {
        RemoteHit {
            handle: crate::new_memory_uuid(store),
            owner: crate::OwnerId::new(uuid::Uuid::from_u128(2)),
            writer: crate::WriterAttribution {
                by: uuid::Uuid::from_u128(20),
                via: Some(uuid::Uuid::from_u128(21)),
            },
            kind: EntityKind::Fact,
            schema_id: "remote/note".into(),
            snippet: String::new(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            tags: Vec::new(),
            body: None,
            citation: None,
        }
    }

    fn remote_request(limit: u32, order: SearchOrder) -> RemoteSearch {
        RemoteSearch {
            query: Some("needle".into()),
            limit,
            kind: None,
            schema_id: None,
            exclude_schema_ids: Vec::new(),
            tags: Vec::new(),
            tag_match: TagMatch::Any,
            mode: SearchMode::Lexical,
            since: None,
            until: None,
            order,
            supersession: SupersessionStatus::HeadsOnly,
            min_score: None,
            semantic_weight: None,
            include_body: false,
            body_max_chars: crate::MAX_TEXT_CAP_CHARS,
            include_neighbor_edges: false,
        }
    }

    #[test]
    fn the_merged_page_is_cut_to_the_limit() {
        let local: Vec<_> = (0..3).map(|_| local_hit()).collect();
        let wiki: Vec<_> = (0..3).map(|_| remote_hit(1)).collect();
        let crm: Vec<_> = (0..3).map(|_| remote_hit(2)).collect();
        let expected = [
            local[0].memory.clone(),
            wiki[0].handle.to_string(),
            crm[0].handle.to_string(),
            local[1].memory.clone(),
        ];
        let mut output = SearchMemoriesOutput {
            mode: Some(SearchMode::Lexical),
            degraded_to_lexical: false,
            ranking: SearchRanking::Score,
            memories: local,
            neighbor_edges: Vec::new(),
            next_cursor: None,
            has_more: false,
            unanswered: None,
            inventory_unanswered: None,
        };
        merge_store_hits(
            &mut output,
            vec![
                (
                    1,
                    RemoteSearchResult {
                        hits: wiki,
                        degraded_to_lexical: false,
                        neighbor_edges: Vec::new(),
                    },
                ),
                (
                    2,
                    RemoteSearchResult {
                        hits: crm,
                        degraded_to_lexical: false,
                        neighbor_edges: Vec::new(),
                    },
                ),
            ],
            vec![UnansweredStore {
                store: 3,
                why: "deadline has elapsed".into(),
            }],
            &remote_request(4, SearchOrder::Relevance),
            0,
        );
        let order: Vec<&str> = output
            .memories
            .iter()
            .map(|hit| hit.memory.as_str())
            .collect();
        // Rank 0 of every list, then rank 1 of the first: RRF, cut at 4.
        assert_eq!(order, expected);
        assert!(output.memories.iter().all(|hit| hit.store.is_some()));
        assert_eq!(output.ranking, SearchRanking::Rank);
        assert_eq!(
            output.unanswered,
            Some(vec![UnansweredStore {
                store: 3,
                why: "deadline has elapsed".into(),
            }])
        );
    }

    #[test]
    fn recency_orders_all_store_candidates_before_the_global_limit() {
        let mut local = vec![local_hit(), local_hit()];
        local[0].created_at += time::Duration::seconds(100);
        local[1].created_at += time::Duration::seconds(50);
        let mut wiki = vec![remote_hit(1), remote_hit(1)];
        wiki[0].created_at += time::Duration::seconds(300);
        wiki[1].created_at += time::Duration::seconds(100);
        let mut crm = remote_hit(2);
        crm.created_at += time::Duration::seconds(200);
        let expected = [
            wiki[0].handle.to_string(),
            crm.handle.to_string(),
            local[0].memory.clone(),
            wiki[1].handle.to_string(),
        ];
        let mut output = SearchMemoriesOutput {
            mode: Some(SearchMode::Hybrid),
            degraded_to_lexical: true,
            ranking: SearchRanking::Score,
            memories: local,
            neighbor_edges: Vec::new(),
            next_cursor: Some("opaque-local-cursor".into()),
            has_more: true,
            unanswered: None,
            inventory_unanswered: None,
        };
        merge_store_hits(
            &mut output,
            vec![
                (
                    1,
                    RemoteSearchResult {
                        hits: wiki,
                        degraded_to_lexical: false,
                        neighbor_edges: Vec::new(),
                    },
                ),
                (
                    2,
                    RemoteSearchResult {
                        hits: vec![crm],
                        degraded_to_lexical: false,
                        neighbor_edges: Vec::new(),
                    },
                ),
            ],
            Vec::new(),
            &remote_request(4, SearchOrder::Recency),
            0,
        );
        assert_eq!(
            output
                .memories
                .iter()
                .map(|hit| hit.memory.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(output.memories.iter().all(|hit| hit.score > 0.0));
        assert_eq!(output.ranking, SearchRanking::Rank);
        assert_eq!(output.next_cursor.as_deref(), Some("opaque-local-cursor"));
        assert!(output.has_more);
        assert!(output.degraded_to_lexical);
    }

    #[test]
    fn federated_recency_drops_edges_when_remote_hits_displace_every_local_endpoint() {
        let local = local_hit();
        let mut remote = remote_hit(1);
        remote.created_at = local.created_at + time::Duration::seconds(1);
        let remote_id = remote.handle;
        let mut output = SearchMemoriesOutput {
            mode: Some(SearchMode::Lexical),
            degraded_to_lexical: false,
            ranking: SearchRanking::Score,
            neighbor_edges: vec![super::super::NeighborEdge {
                source: local.memory.clone(),
                target: crate::new_memory_uuid(0).to_string(),
                kind: crate::EdgeKind::Reference,
            }],
            memories: vec![local],
            next_cursor: Some("local-cursor".into()),
            has_more: true,
            unanswered: None,
            inventory_unanswered: None,
        };
        merge_store_hits(
            &mut output,
            vec![(
                1,
                RemoteSearchResult {
                    hits: vec![remote],
                    degraded_to_lexical: false,
                    neighbor_edges: Vec::new(),
                },
            )],
            Vec::new(),
            &remote_request(1, SearchOrder::Recency),
            0,
        );
        assert_eq!(output.memories[0].memory_id, remote_id);
        assert!(output.neighbor_edges.is_empty());
        assert_eq!(output.next_cursor.as_deref(), Some("local-cursor"));
        assert!(output.has_more);
    }

    #[test]
    fn neighbor_edge_filter_matches_ledger_vectors() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../../../tests/ledger_vectors.json")).unwrap();
        let rows = vectors["Proxiima.Search.neighborEdgeEligible"]
            .as_array()
            .unwrap();
        for row in rows {
            let source = local_hit();
            let target = local_hit();
            let edge = || super::super::NeighborEdge {
                source: source.memory.clone(),
                target: target.memory.clone(),
                kind: crate::EdgeKind::Reference,
            };
            let mut edges = if row["in"][2].as_bool().unwrap() {
                vec![edge(), edge()]
            } else {
                vec![edge()]
            };
            let mut page = Vec::new();
            if row["in"][0].as_bool().unwrap() {
                page.push(source);
            }
            if row["in"][1].as_bool().unwrap() {
                page.push(target);
            }
            let prior_present = row["in"][2].as_bool().unwrap() && !page.is_empty();
            super::super::retain_surviving_neighbor_edges(&page, &mut edges);
            let expected = usize::from(prior_present) + usize::from(row["out"].as_bool().unwrap());
            assert_eq!(edges.len(), expected, "{row}");
        }
    }

    struct CapturingStore {
        listings: std::sync::atomic::AtomicUsize,
        requests: std::sync::Mutex<Vec<RemoteSearch>>,
        reply: Result<RemoteSearchResult, OtherStoresError>,
    }

    impl CapturingStore {
        fn new(reply: Result<RemoteSearchResult, OtherStoresError>) -> Self {
            Self {
                listings: std::sync::atomic::AtomicUsize::new(0),
                requests: std::sync::Mutex::new(Vec::new()),
                reply,
            }
        }
    }

    impl OtherStores for CapturingStore {
        fn stores<'a>(
            &'a self,
            _caller: StoreCaller<'a>,
        ) -> BoxFuture<'a, Result<Vec<StoreInfo>, OtherStoresError>> {
            self.listings
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                Ok(vec![StoreInfo {
                    number: 1,
                    name: "wiki".into(),
                    description: String::new(),
                    schema_ids: vec!["remote/note".into()],
                }])
            })
        }
        fn search<'a>(
            &'a self,
            _caller: StoreCaller<'a>,
            _store: u16,
            args: &'a RemoteSearch,
        ) -> BoxFuture<'a, Result<RemoteSearchResult, OtherStoresError>> {
            self.requests.lock().unwrap().push(args.clone());
            Box::pin(async { self.reply.clone() })
        }
        fn read<'a>(
            &'a self,
            _caller: StoreCaller<'a>,
            _handle: uuid::Uuid,
            _at: crate::storage_ports::other_stores::ReadAt,
        ) -> BoxFuture<'a, Result<RemoteRead, OtherStoresError>> {
            Box::pin(async { Ok(RemoteRead::NotFound) })
        }
    }

    fn remote_ctx(port: &Arc<CapturingStore>) -> crate::ToolCtx {
        let mut ctx = super::super::super::memory_spaces::test_ctx::ctx_for(
            crate::UserId::new(uuid::Uuid::now_v7()),
            Vec::new(),
        );
        ctx.try_insert_service(OtherStoresService::new(port.clone()))
            .expect("the fixture context holds no service");
        ctx
    }

    fn remote_args(mode: SearchMemoriesMode) -> super::super::SearchMemoriesArgs {
        let mut request = super::args(mode);
        request.stores = Some(vec![1]);
        request
    }

    #[tokio::test]
    async fn remote_search_receives_every_filter_and_the_requested_mode() {
        let since = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let until = since + time::Duration::days(1);
        for mode in [SearchMemoriesMode::Hybrid, SearchMemoriesMode::Semantic] {
            let port = Arc::new(CapturingStore::new(Ok(RemoteSearchResult {
                hits: Vec::new(),
                degraded_to_lexical: false,
                neighbor_edges: Vec::new(),
            })));
            let mut request = remote_args(mode);
            request.limit = 100;
            request.kind = Some(MemoryKind::Fact);
            request.schema_id = Some("remote/note".into());
            request.tags = vec![" Rust ".into(), "rust".into(), String::new(), "ZED".into()];
            request.tag_match = TagMatch::All;
            request.since = Some(
                since
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
            );
            request.until = Some(
                until
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
            );
            request.order = SearchOrder::Recency;
            request.supersession = SearchMemoriesSupersession::All;
            request.min_score = Some(0.4);
            request.semantic_weight = matches!(mode, SearchMemoriesMode::Hybrid).then_some(0.7);
            request.include_body = matches!(mode, SearchMemoriesMode::Hybrid);
            request.include_neighbor_edges = request.include_body;
            request.body_max_chars = request.include_body.then_some(3);
            let semantic_weight = request.semantic_weight;
            SearchMemoriesTool::call(remote_ctx(&port), request)
                .await
                .unwrap();
            assert_eq!(
                *port.requests.lock().unwrap(),
                vec![RemoteSearch {
                    query: Some("needle".into()),
                    limit: 50,
                    kind: Some(EntityKind::Fact),
                    schema_id: Some("remote/note".into()),
                    exclude_schema_ids: Vec::new(),
                    tags: vec!["rust".into(), "zed".into()],
                    tag_match: TagMatch::All,
                    mode: SearchMode::from(mode),
                    since: Some(since),
                    until: Some(until),
                    order: SearchOrder::Recency,
                    supersession: SupersessionStatus::IncludeSuperseded,
                    min_score: Some(0.4),
                    semantic_weight,
                    include_body: matches!(mode, SearchMemoriesMode::Hybrid),
                    body_max_chars: if matches!(mode, SearchMemoriesMode::Hybrid) {
                        3
                    } else {
                        crate::MAX_TEXT_CAP_CHARS
                    },
                    include_neighbor_edges: matches!(mode, SearchMemoriesMode::Hybrid),
                }]
            );
        }
    }

    #[tokio::test]
    async fn remote_listing_forwards_absent_query_and_exclusions() {
        let port = Arc::new(CapturingStore::new(Ok(RemoteSearchResult {
            hits: vec![remote_hit(1)],
            degraded_to_lexical: false,
            neighbor_edges: Vec::new(),
        })));
        let mut request = remote_args(SearchMemoriesMode::Lexical);
        request.query = None;
        request.mode = None;
        request.order = SearchOrder::Recency;
        request.exclude_schema_ids = vec!["b".into(), "a".into()];
        let output = SearchMemoriesTool::call(remote_ctx(&port), request)
            .await
            .expect("listing reaches the remote port");
        let captured = &port.requests.lock().unwrap()[0];
        assert_eq!(captured.query, None);
        assert_eq!(captured.exclude_schema_ids, vec!["b", "a"]);
        assert_eq!(captured.mode, SearchMode::Lexical);
        assert_eq!(output.mode, None);
        assert_eq!(output.ranking, SearchRanking::Score);
        assert_eq!(output.memories.len(), 1);
        assert_eq!(output.memories[0].score, 0.0);
        assert_eq!(output.memories[0].lexical_score, 0.0);
        assert_eq!(output.memories[0].similarity_score, 0.0);
    }

    #[tokio::test]
    async fn remote_body_is_capped_and_absent_when_not_requested() {
        let mut hit = remote_hit(1);
        hit.body = Some(RemoteBody {
            text: "αβγδεζ".into(),
            truncated: false,
        });
        let port = Arc::new(CapturingStore::new(Ok(RemoteSearchResult {
            hits: vec![hit],
            degraded_to_lexical: false,
            neighbor_edges: Vec::new(),
        })));
        let mut request = remote_args(SearchMemoriesMode::Lexical);
        request.include_body = true;
        request.body_max_chars = Some(3);
        let output = SearchMemoriesTool::call(remote_ctx(&port), request)
            .await
            .expect("remote body reaches output");
        assert_eq!(output.memories[0].body.as_deref(), Some("αβγ"));
        assert_eq!(output.memories[0].body_truncated, Some(true));

        let mut hit = remote_hit(1);
        hit.body = Some(RemoteBody {
            text: "αβ".into(),
            truncated: true,
        });
        let port = Arc::new(CapturingStore::new(Ok(RemoteSearchResult {
            hits: vec![hit],
            degraded_to_lexical: false,
            neighbor_edges: Vec::new(),
        })));
        let mut request = remote_args(SearchMemoriesMode::Lexical);
        request.include_body = true;
        request.body_max_chars = Some(3);
        let output = SearchMemoriesTool::call(remote_ctx(&port), request)
            .await
            .expect("upstream truncation reaches output");
        assert_eq!(output.memories[0].body.as_deref(), Some("αβ"));
        assert_eq!(output.memories[0].body_truncated, Some(true));

        let mut hit = remote_hit(1);
        hit.body = Some(RemoteBody {
            text: "αβγ".into(),
            truncated: false,
        });
        let port = Arc::new(CapturingStore::new(Ok(RemoteSearchResult {
            hits: vec![hit],
            degraded_to_lexical: false,
            neighbor_edges: Vec::new(),
        })));
        let mut request = remote_args(SearchMemoriesMode::Lexical);
        request.include_body = true;
        request.body_max_chars = Some(3);
        let output = SearchMemoriesTool::call(remote_ctx(&port), request)
            .await
            .expect("a body at the cap is whole");
        assert_eq!(output.memories[0].body.as_deref(), Some("αβγ"));
        assert_eq!(output.memories[0].body_truncated, Some(false));

        let mut request = remote_args(SearchMemoriesMode::Lexical);
        request.include_body = false;
        let output = SearchMemoriesTool::call(remote_ctx(&port), request)
            .await
            .expect("remote body can be omitted");
        assert_eq!(output.memories[0].body, None);
        assert_eq!(output.memories[0].body_truncated, None);
    }

    #[test]
    fn a_stores_edges_follow_its_own_surviving_hits() {
        for include_neighbor_edges in [true, false] {
            let local = local_hit();
            let local_id = local.memory_id;
            let (first, second) = (remote_hit(1), remote_hit(1));
            let (first_id, second_id) = (first.handle, second.handle);
            let edge = |source, target| RemoteEdge {
                source,
                target,
                kind: crate::EdgeKind::Reference,
            };
            let mut output = SearchMemoriesOutput {
                mode: Some(SearchMode::Lexical),
                degraded_to_lexical: false,
                ranking: SearchRanking::Score,
                memories: vec![local],
                neighbor_edges: Vec::new(),
                next_cursor: None,
                has_more: false,
                unanswered: None,
                inventory_unanswered: None,
            };
            let request = RemoteSearch {
                include_neighbor_edges,
                ..remote_request(2, SearchOrder::Relevance)
            };
            merge_store_hits(
                &mut output,
                vec![(
                    1,
                    RemoteSearchResult {
                        hits: vec![first, second],
                        degraded_to_lexical: false,
                        neighbor_edges: vec![
                            edge(first_id, RemoteEdgeTarget::Redacted),
                            edge(second_id, RemoteEdgeTarget::Visible(first_id)),
                            edge(second_id, RemoteEdgeTarget::Redacted),
                            edge(second_id, RemoteEdgeTarget::Visible(local_id)),
                        ],
                    },
                )],
                Vec::new(),
                &request,
                0,
            );
            let mut kept: Vec<_> = output.memories.iter().map(|hit| hit.memory_id).collect();
            let mut expected_kept = vec![local_id, first_id];
            kept.sort_unstable();
            expected_kept.sort_unstable();
            assert_eq!(kept, expected_kept, "the limit cuts `second`");
            let edges: Vec<_> = output
                .neighbor_edges
                .iter()
                .map(|edge| (edge.source.clone(), edge.target.clone(), edge.kind))
                .collect();
            let expected = if include_neighbor_edges {
                // Out of and into a kept hit of store 1 stay; an edge of the
                // cut hit, and store 1's edge onto store 0's hit, go.
                vec![
                    (
                        first_id.to_string(),
                        "redacted target".to_owned(),
                        crate::EdgeKind::Reference,
                    ),
                    (
                        second_id.to_string(),
                        first_id.to_string(),
                        crate::EdgeKind::Reference,
                    ),
                ]
            } else {
                Vec::new()
            };
            assert_eq!(edges, expected);
        }
    }

    #[tokio::test]
    async fn invalid_remote_only_timestamps_are_refused_before_any_port_call() {
        for (since, until, field) in [
            (Some("not-a-time".into()), None, "since"),
            (None, Some("not-a-time".into()), "until"),
        ] {
            let port = Arc::new(CapturingStore::new(Ok(RemoteSearchResult {
                hits: Vec::new(),
                degraded_to_lexical: false,
                neighbor_edges: Vec::new(),
            })));
            let mut request = remote_args(SearchMemoriesMode::Lexical);
            request.since = since;
            request.until = until;
            let result = SearchMemoriesTool::call(remote_ctx(&port), request).await;
            assert!(
                matches!(result, Err(crate::ToolError::InvalidInput(message)) if message.contains(field))
            );
            assert_eq!(port.listings.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(port.requests.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn remote_fallback_is_reported_with_empty_or_capped_hits() {
        let first = remote_hit(1);
        let first_id = first.handle;
        for hits in [Vec::new(), vec![first, remote_hit(1)]] {
            let expected = hits.len().min(1);
            let port = Arc::new(CapturingStore::new(Ok(RemoteSearchResult {
                hits,
                degraded_to_lexical: true,
                neighbor_edges: Vec::new(),
            })));
            let mut request = remote_args(SearchMemoriesMode::Hybrid);
            request.limit = 1;
            let result = SearchMemoriesTool::call(remote_ctx(&port), request)
                .await
                .unwrap();
            assert_eq!(result.memories.len(), expected);
            if let Some(hit) = result.memories.first() {
                assert_eq!(hit.memory, first_id.to_string());
                assert_eq!(hit.memory_id, first_id);
            }
            assert!(result.degraded_to_lexical);
            assert_eq!(result.unanswered, Some(Vec::new()));
            assert_eq!(port.requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn unsupported_remote_search_is_unanswered_without_a_weaker_retry() {
        let port = Arc::new(CapturingStore::new(Err(
            OtherStoresError::UnsupportedSearch("all-tag matching".into()),
        )));
        let mut request = remote_args(SearchMemoriesMode::Hybrid);
        request.tags = vec!["rust".into(), "guide".into()];
        request.tag_match = TagMatch::All;
        let result = SearchMemoriesTool::call(remote_ctx(&port), request)
            .await
            .unwrap();
        assert_eq!(
            result.unanswered,
            Some(vec![UnansweredStore {
                store: 1,
                why: "store cannot honor search: all-tag matching".into(),
            }])
        );
        assert!(result.memories.is_empty());
        let requests = port.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].tag_match, TagMatch::All);
        assert_eq!(requests[0].tags, ["guide", "rust"]);
    }

    struct TwoStores;

    #[tokio::test]
    async fn unsupported_body_or_edges_leave_one_store_unanswered() {
        let ctx = super::super::super::memory_spaces::test_ctx::ctx_for(
            crate::UserId::new(uuid::Uuid::now_v7()),
            Vec::new(),
        );
        let mut ctx = ctx;
        ctx.try_insert_service(OtherStoresService::new(Arc::new(TwoStores)))
            .expect("service slot is empty");
        let mut request = remote_args(SearchMemoriesMode::Lexical);
        request.stores = Some(vec![1, 2]);
        request.include_body = true;
        request.include_neighbor_edges = true;
        let output = SearchMemoriesTool::call(ctx, request).await.unwrap();
        assert_eq!(output.memories.len(), 1);
        assert_eq!(output.memories[0].store, Some(2));
        assert_eq!(
            output.unanswered,
            Some(vec![UnansweredStore {
                store: 1,
                why: "store cannot honor search: body and edges".into(),
            }])
        );
    }

    impl OtherStores for TwoStores {
        fn stores<'a>(
            &'a self,
            _caller: StoreCaller<'a>,
        ) -> BoxFuture<'a, Result<Vec<StoreInfo>, OtherStoresError>> {
            Box::pin(async {
                Ok(vec![
                    StoreInfo {
                        number: 1,
                        name: "wiki".into(),
                        description: "Team wiki pages".into(),
                        schema_ids: Vec::new(),
                    },
                    StoreInfo {
                        number: 2,
                        name: "crm".into(),
                        description: "Customer records".into(),
                        schema_ids: Vec::new(),
                    },
                ])
            })
        }
        fn search<'a>(
            &'a self,
            _caller: StoreCaller<'a>,
            store: u16,
            _args: &'a RemoteSearch,
        ) -> BoxFuture<'a, Result<RemoteSearchResult, OtherStoresError>> {
            Box::pin(async move {
                if store == 1 {
                    return Err(OtherStoresError::UnsupportedSearch("body and edges".into()));
                }
                Ok(RemoteSearchResult {
                    hits: vec![remote_hit(2)],
                    degraded_to_lexical: false,
                    neighbor_edges: Vec::new(),
                })
            })
        }
        fn read<'a>(
            &'a self,
            _caller: StoreCaller<'a>,
            _handle: uuid::Uuid,
            _at: crate::storage_ports::other_stores::ReadAt,
        ) -> BoxFuture<'a, Result<RemoteRead, OtherStoresError>> {
            Box::pin(async { Ok(RemoteRead::NotFound) })
        }
    }

    fn static_schema() -> serde_json::Value {
        crate::catalog::schema::mcp_tool_schema::<super::super::SearchMemoriesArgs>().schema
    }

    #[tokio::test]
    async fn without_a_port_the_schema_is_the_static_one() {
        let ctx = super::super::super::memory_spaces::test_ctx::ctx_for(
            crate::UserId::new(uuid::Uuid::now_v7()),
            Vec::new(),
        );
        let schema = static_schema();
        assert!(schema["properties"].get("stores").is_none(), "{schema}");
        assert_eq!(
            caller_input_schema(&ctx, schema.clone())
                .await
                .expect("no port"),
            schema
        );
    }

    #[tokio::test]
    async fn with_a_port_stores_is_an_enum_of_the_callers_stores() {
        let mut ctx = super::super::super::memory_spaces::test_ctx::ctx_for(
            crate::UserId::new(uuid::Uuid::now_v7()),
            Vec::new(),
        );
        ctx.try_insert_service(OtherStoresService::new(Arc::new(TwoStores)))
            .expect("the fixture context holds no service");
        let schema = caller_input_schema(&ctx, static_schema())
            .await
            .expect("valid inventory");
        let stores = &schema["properties"]["stores"];
        assert_eq!(stores["type"], "array");
        assert_eq!(stores["items"]["enum"], serde_json::json!([0, 1, 2]));
        assert_eq!(stores["items"]["oneOf"][1]["const"], 1);
        assert_eq!(
            stores["items"]["oneOf"][1]["description"],
            "Team wiki pages"
        );
        let prose = stores["description"].as_str().expect("description");
        assert!(prose.contains("`2` (crm): Customer records"), "{prose}");
        // Everything else is the static schema.
        let mut without = schema.clone();
        without["properties"]
            .as_object_mut()
            .expect("props")
            .remove("stores");
        assert_eq!(without, static_schema());
    }
}

#[test]
fn effective_weight_matches_ledger_vectors() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../../tests/ledger_vectors.json")).unwrap();
    let rows = vectors["Proxiima.Search.keepSemanticWeight"]
        .as_array()
        .unwrap();
    for row in rows {
        let mode = match row["in"][0].as_str() {
            Some("lexical") => SearchMode::Lexical,
            Some("semantic") => SearchMode::Semantic,
            Some("hybrid") => SearchMode::Hybrid,
            other => panic!("unexpected search mode {other:?}"),
        };
        let requested = row["in"][1].as_bool().unwrap().then_some(0.75);
        let expected = row["out"].as_bool().unwrap().then_some(0.75);
        assert_eq!(
            super::weight_for_effective_mode(requested, mode),
            expected,
            "{row}"
        );
    }
}

#[test]
fn degradation_warning_matches_ledger_vectors() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../../tests/ledger_vectors.json")).unwrap();
    let rows = vectors["Proxiima.Search.warnsOnLexicalDegradation"]
        .as_array()
        .unwrap();
    for row in rows {
        let mode = serde_json::from_value(row["in"][0].clone()).unwrap();
        let no_rows = row["in"][1].as_bool().unwrap();
        let positive_similarity = row["in"][2].as_bool().unwrap();
        assert_eq!(
            degraded_to_lexical(mode, no_rows, positive_similarity),
            row["out"].as_bool().unwrap(),
            "{row}",
        );
    }
}

#[test]
fn relevance_order_matches_ledger_vectors() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../../tests/ledger_vectors.json")).unwrap();
    let rows = vectors["Proxiima.Search.relevanceOrder"]
        .as_array()
        .unwrap();
    for row in rows {
        let lanes = match row["in"][0].as_str() {
            Some("single") => vec![lane(1, None)],
            Some("equalScale") => vec![lane(1, Some("same")), lane(2, Some("same"))],
            Some("unequalScale") => vec![lane(1, Some("first")), lane(2, Some("second"))],
            other => panic!("unexpected search lanes {other:?}"),
        };
        let expected = match row["out"].as_str() {
            Some("score") => super::SearchRanking::Score,
            Some("perLaneRank") => super::SearchRanking::Rank,
            other => panic!("unexpected relevance order {other:?}"),
        };
        assert_eq!(super::ranking_for(&lanes), expected, "{row}");
    }
}
