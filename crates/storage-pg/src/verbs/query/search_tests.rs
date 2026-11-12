use super::{
    EntityKind, MemorySearchRequest, OwnerId, SearchMode, SearchOrder, SupersessionStatus, TagMatch,
};

/// The plan pins test the migrated candidate functions, so the scan must
/// call them: with either one dropped, the search that reaches it fails.
/// An empty store reaches both arms, the substring one because the ranked
/// arm returns nothing for any schema.
#[tokio::test]
async fn each_scan_arm_calls_its_migrated_candidate_function() {
    use proxiima_core::storage_ports::MemoryReadPort;

    let owner = OwnerId::from(proxiima_core::UserId::new(uuid::Uuid::now_v7()));
    let scope = proxiima_core::test_fixtures::owner_scope_for_tests(&[owner]);
    let note = proxiima_core::SpiiceRegistry::new()
        .freeze_or_panic_for_tests()
        .search_projections()
        .iter()
        .find(|projection| projection.schema_id.as_str() == "core/agent-note-v1")
        .expect("the core note projection")
        .clone();
    let mut req = request_with_tags();
    req.read_owners = vec![owner];
    req.scope = proxiima_core::verbs::query::MemorySearchScope::Owner(owner);
    req.tags.clear();
    for function in [
        None,
        Some("search_lexical_candidates"),
        Some("search_substring_candidates"),
    ] {
        let (pg, _db) = crate::test_fixtures::fresh_pg("search_arm_fns").await;
        if let Some(function) = function {
            let mut drop = proxiima_core::SqlText::new(
                "DO $$ BEGIN EXECUTE 'DROP FUNCTION ' || (
                         SELECT oid::regprocedure::text FROM pg_proc
                          WHERE proname = '",
            );
            drop.push(function).push(
                "'
                            AND pronamespace = 'proxiima_core'::regnamespace); END $$",
            );
            sqlx::query(drop)
                .execute(pg.pool_for_tests())
                .await
                .expect("drop the candidate function");
        }
        let found = pg
            .search_memories(Some(&scope), &req, std::slice::from_ref(&note))
            .await;
        assert_eq!(
            found.is_err(),
            function.is_some(),
            "{function:?}: {found:?}"
        );
    }
}

/// The core contract's own projected sidecars stay declared alongside
/// the other spiices an unscoped request can reach.
#[test]
fn the_core_contract_declares_its_projected_sidecars() {
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    for projection in registry.search_projections().iter().filter(|projection| {
        proxiima_core::SPIICE_0.declares_sidecar_table(&projection.sidecar_table)
    }) {
        assert!(
            matches!(
                projection.band_comparability,
                proxiima_core::spiice::BandComparability::CoreBands
            ) && projection.rank_source.is_projection(),
            "spiice #0's own schemas must pass the gate they define"
        );
    }
    let mut by_contract = registry
        .search_projections()
        .iter()
        .map(|projection| projection.sidecar_table.as_str())
        .filter(|table| proxiima_core::SPIICE_0.declares_sidecar_table(table))
        .collect::<Vec<_>>();
    by_contract.sort_unstable();
    by_contract.dedup();
    assert_eq!(
        by_contract,
        vec![
            "proxiima_core.agent_derivation_v1",
            "proxiima_core.agent_note_v1",
            "proxiima_core.interpretation_v1",
            "proxiima_core.utterance_v1",
        ],
    );
    let mut by_prefix = registry
        .search_projections()
        .iter()
        .map(|projection| projection.sidecar_table.as_str())
        .filter(|table| table.starts_with("proxiima_core."))
        .collect::<Vec<_>>();
    by_prefix.sort_unstable();
    by_prefix.dedup();
    assert_eq!(
        by_contract, by_prefix,
        "the ordinal and the schema-name prefix must still agree on core's own sidecars"
    );
}

/// Where each arm's owner predicate sits, and that the placeholders did
/// not shift when the LIKE bind left the ranked arm.
///
/// The ranked arm does not join `memory` at all: its owner is
/// `p.owner_id`, on the projection, which is the only spelling the
/// composite `gin(owner_id, search_tsv)` can serve. The substring arm's
/// sits on `memory`, at candidate time and not only at admit — a
/// sidecar carries no `owner_id`, so the loop drives from the owner
/// index and probes the sidecar by `t`.
///
/// The numbering is the mechanical hazard: `$7` is the owner set and
/// `$8` the participating-schema array, and a shift by one puts each
/// where the other belongs. This asserts the EMITTED SQL, so a shift
/// fails here.
#[test]
fn every_bind_is_where_the_scan_binds_it() {
    let note = proxiima_core::SpiiceRegistry::new()
        .freeze_or_panic_for_tests()
        .search_projections()
        .iter()
        .find(|projection| projection.schema_id.as_str() == "core/agent-note-v1")
        .expect("core/agent-note-v1 is a search surface")
        .clone();
    let mut req = request_with_tags();
    let spiice = super::SpiiceScan::new(vec![&note]).expect("fixture head declares render bands");

    let ranked = super::ranked_projection_sql(&spiice, &req, true).expect("ranked");
    let ranked = ranked.as_str();
    assert!(ranked.contains("lexical_scrub($1)"), "$1 is the query");
    assert!(ranked.contains("LIMIT $2"), "$2 is the overfetch budget");
    assert!(
        ranked.contains("p.tag && $3::text[]"),
        "$3 is the tag array"
    );
    // By COMPARISON, not by cast. `>= $4` / `<= $5` and `>= $5` /
    // `<= $4` cast identically and bind identically; only the direction
    // tells `since` from `until`, and swapping them leaves every other
    // test in the workspace green.
    assert!(
        ranked.contains("proxiima_core.memory_created_at(p.memory_id) >= $4"),
        "$4 is `since` — a LOWER bound"
    );
    assert!(
        ranked.contains("proxiima_core.memory_created_at(p.memory_id) <= $5"),
        "$5 is `until` — an UPPER bound"
    );
    assert!(
        ranked.contains("($6::uuid IS NULL OR p.memory_id < $6)"),
        "$6 is the recency cursor, and it pages DOWNWARD"
    );
    assert!(
        ranked.contains("p.owner_id = ANY($7::uuid[])"),
        "$7 is the owner set, ON THE PROJECTION, for the composite GIN"
    );
    assert!(
        ranked.contains("p.schema_id = ANY($8::text[])"),
        "$8 is the participating schema set, a row predicate now"
    );
    assert!(
        !ranked.contains("$9"),
        "the ranked arm binds eight parameters; a ninth is a shift"
    );
    assert!(
        !ranked.contains("length("),
        "the unused-parameter guard went with the LIKE bind it kept alive"
    );
    // The ranked arm's OWNER never comes through a join — that is what
    // the composite `gin(owner_id, search_tsv)` needs. The head
    // restriction names `memory` and `memory_head` in an EXISTS, so the
    // property is asserted directly rather than by a blanket string
    // test for `proxiima_core.live_memory`.
    assert!(
        !ranked.contains("m.owner_id"),
        "the ranked arm's owner is on the projection, never reached through memory"
    );
    assert!(
        ranked.contains("FROM proxiima_core.projection p, q"),
        "the projection is the driving relation"
    );

    let substring = super::substring_sql_for_tests(&[&note], &req).expect("substring");
    let substring = substring.as_str();
    assert!(substring.contains("LIKE $1 ESCAPE"), "$1 is the pattern");
    assert!(substring.contains("LIMIT $2"), "$2 is the overfetch budget");
    assert!(substring.contains("c.tags && $3::text[]"), "$3 is the tags");
    assert!(
        substring.contains("proxiima_core.memory_created_at(m.t) >= $4"),
        "$4 is `since` — a LOWER bound"
    );
    assert!(
        substring.contains("proxiima_core.memory_created_at(m.t) <= $5"),
        "$5 is `until` — an UPPER bound"
    );
    assert!(
        substring.contains("($6::uuid IS NULL OR m.t < $6)"),
        "$6 is the recency cursor, and it pages DOWNWARD"
    );
    assert!(
        substring.contains("FROM proxiima_core.live_memory source")
            && substring.contains("ON c.t = m.t"),
        "a sidecar carries no owner; the leg must drive from memory"
    );
    assert!(
        substring.contains("source.owner_id = ANY($7::uuid[])"),
        "$7 is the owner set, at candidate time and not only at admit"
    );
    assert!(
        substring.contains("source.schema_id = ANY($8::text[])"),
        "$8 is the MISSING schema set"
    );
    assert!(
        !substring.contains("$9"),
        "the substring arm binds eight parameters; a ninth is a shift"
    );
    assert!(
        !substring.contains("websearch_to_tsquery"),
        "the substring arm referenced a query-side CTE it never read"
    );

    // Without tags the tag predicate is absent from both, and nothing
    // else moves: `$3` simply goes unmentioned.
    req.tags.clear();
    let untagged = super::ranked_projection_sql(&spiice, &req, true).expect("ranked");
    let untagged = untagged.as_str();
    assert!(!untagged.contains("p.tag"));
    assert!(untagged.contains("p.owner_id = ANY($7::uuid[])"));
}

/// One `EXISTS` probe per participating spiice, against THAT spiice's
/// projection table with THAT spiice's schema set, OR'ed — and no
/// table name reaches the SQL except through `PgIdent`.
#[test]
fn the_semantic_arm_probes_each_participating_spiice() {
    let note = proxiima_core::SpiiceRegistry::new()
        .freeze_or_panic_for_tests()
        .search_projections()
        .iter()
        .find(|projection| projection.schema_id.as_str() == "core/agent-note-v1")
        .expect("core/agent-note-v1 is a search surface")
        .clone();
    let mut req = request_with_tags();

    // Two spiices: one probe each, OR'ed, each with its own schema set.
    let mut docs = note.clone();
    docs.projection_table = "proxiima_docs.projection".into();
    let spiices = [
        super::SpiiceScan::new(vec![&note]).expect("fixture head declares render bands"),
        super::SpiiceScan::new(vec![&docs]).expect("fixture head declares render bands"),
    ];
    let two = super::semantic_candidate_admission_sql(&spiices, &req).expect("semantic");
    let two = two.as_str();
    let core = two
        .find("FROM proxiima_core.projection p")
        .expect("core probe");
    let foreign = two
        .find("FROM proxiima_docs.projection p")
        .expect("docs probe");
    assert!(core < foreign, "probes render in `spiices` order");
    assert!(two.contains(") OR EXISTS ("), "one probe per spiice, OR'ed");
    let first = two.find("p.schema_id = ANY($8::text[])").expect("$8");
    let second = two.find("p.schema_id = ANY($9::text[])").expect("$9");
    assert!(
        core < first && first < foreign && foreign < second,
        "$8 and $9 are the schema sets of the first and second spiice, in that order"
    );
    assert!(
        !two.contains("$10"),
        "two spiices bind nine parameters; a tenth is a shift"
    );

    // The table name reaches the SQL only through `PgIdent`.
    let mut spliced = note.clone();
    spliced.projection_table = "proxiima_docs.projection p; DROP TABLE x".into();
    let hostile =
        [super::SpiiceScan::new(vec![&spliced]).expect("fixture head declares render bands")];
    assert!(
        super::semantic_candidate_admission_sql(&hostile, &req).is_err(),
        "an invalid projection table identifier is refused, not spliced"
    );

    // Without tags the same per-spiice probes remain, with schema sets
    // starting at `$7`.
    req.tags.clear();
    let untagged = super::semantic_candidate_admission_sql(&spiices, &req).expect("semantic");
    let untagged = untagged.as_str();
    assert!(untagged.contains("FROM proxiima_core.projection p"));
    assert!(untagged.contains("p.schema_id = ANY($7::text[])"));
    assert!(untagged.contains("FROM proxiima_docs.projection p"));
    assert!(untagged.contains("p.schema_id = ANY($8::text[])"));
    assert!(!untagged.contains("p.tag"));
    assert!(super::semantic_candidate_admission_sql(&hostile, &req).is_err());
}

/// Admission's filters are on the CANDIDATE side, in both arms, and
/// only when the request asked for them.
///
/// `request_with_tags` is `HeadsOnly` with `kind = Fact` — the tool
/// default and what `core_recall` sends. `IncludeSuperseded` must render
/// no head restriction: that request wants the revisions, so restricting
/// its window would be a different bug in the other direction. The two
/// filters are INDEPENDENT: a kind-filtered `IncludeSuperseded` request
/// is reachable
/// (`core_search_memories` takes both parameters) and still needs its
/// kind mirror.
#[test]
fn the_candidate_window_is_not_spent_on_rows_admit_will_drop() {
    let note = proxiima_core::SpiiceRegistry::new()
        .freeze_or_panic_for_tests()
        .search_projections()
        .iter()
        .find(|projection| projection.schema_id.as_str() == "core/agent-note-v1")
        .expect("core/agent-note-v1 is a search surface")
        .clone();
    let mut req = request_with_tags();
    let spiice = super::SpiiceScan::new(vec![&note]).expect("fixture head declares render bands");

    let ranked = super::ranked_projection_sql(&spiice, &req, true).expect("ranked");
    let ranked = ranked.as_str();
    assert!(
        ranked.contains("JOIN proxiima_core.memory_head hh")
            && ranked.contains("ON hh.handle = hm.handle AND hh.t = hm.t")
            && ranked.contains("WHERE hm.t = p.memory_id"),
        "HeadsOnly must reach the head through the memory row's handle, \
             not spend the window on superseded revisions"
    );
    let substring = super::substring_sql_for_tests(&[&note], &req).expect("substring");
    let substring = substring.as_str();
    assert!(
        substring.contains("FROM proxiima_core.memory_head hh")
            && substring.contains("WHERE hh.handle = m.handle AND hh.t = m.t"),
        "the substring window is shared across the missing schemas now; \
             it must not be spent on superseded revisions either"
    );

    assert!(
        ranked.contains("AND hm.kind = 'fact'"),
        "the projection has no kind column, so the kind filter admit \
             applies has to be mirrored through the memory row"
    );
    assert!(
        substring.contains("AND m.kind = 'fact'"),
        "…and the substring leg already holds that row, so it is a plain \
             column predicate there"
    );

    req.supersession = SupersessionStatus::IncludeSuperseded;
    let ranked = super::ranked_projection_sql(&spiice, &req, true).expect("ranked");
    let ranked = ranked.as_str();
    assert!(
        !ranked.contains("memory_head"),
        "IncludeSuperseded keeps the unfiltered window"
    );
    assert!(
        ranked.contains("AND hm.kind = 'fact'"),
        "…but the kind filter is independent of it: this request shape is \
             reachable and admit still applies the kind"
    );
    let substring = super::substring_sql_for_tests(&[&note], &req).expect("substring");
    let substring = substring.as_str();
    assert!(
        !substring.contains("memory_head"),
        "IncludeSuperseded keeps the unfiltered window"
    );

    req.kind = None;
    let ranked = super::ranked_projection_sql(&spiice, &req, true).expect("ranked");
    let ranked = ranked.as_str();
    assert!(
        !ranked
            .split_whitespace()
            .any(|token| token == "proxiima_core.live_memory"),
        "with neither filter the ranked arm reads the projection ALONE, \
             as it always did"
    );

    // ONE schema id registered under TWO kinds: `schema_id` cannot
    // narrow it away, so only the kind predicate keeps the doomed rows
    // out of the window.
    req.kind = Some(EntityKind::Perspective);
    let ranked = super::ranked_projection_sql(&spiice, &req, true).expect("ranked");
    let ranked = ranked.as_str();
    assert!(
        ranked.contains("FROM proxiima_core.live_memory hm\n") && !ranked.contains("memory_head"),
        "a kind-only restriction probes the memory row and nothing else"
    );
    assert!(
        ranked.contains("AND hm.kind = 'perspective'"),
        "the literal is the REQUEST's kind, not a constant"
    );
}

/// How `search_admit_sql` spells a top-level conjunct: a newline, the
/// WHERE body's indentation, and `AND `. The `OR` continuation lines
/// inside a predicate are indented further, so they do not count.
const ADMIT_PREDICATE_PREFIX: &str = "\n            AND ";

/// Admit's filters: owner, kind, `schema_id`, since, until — plus the
/// candidate id set itself, which is `m.t = ANY($1)` in the WHERE and
/// therefore not one of these. Five, and supersession makes six by
/// changing the FROM clause rather than adding a predicate.
///
/// This number exists to be hard to raise by accident. Raising it is a
/// claim that the candidate window already applies whatever was added.
const ADMIT_PREDICATES: usize = 5;

/// The class guard: every filter `search_admit_sql` applies is mirrored
/// on the candidate side.
///
/// Admit narrowing the hit set by something the candidate window
/// already spent slots on is the defect class this closes, and a claim
/// that the remaining filters are mirrored is exactly what cannot be
/// trusted in prose. So: enumerate admit's binds, require each to be
/// accounted for, and FAIL when admit grows one more.
///
/// Counting BINDS is not enough — a filter needs no bind at all.
/// `AND m.forgotten_at IS NULL` adds a predicate and no `$n`, so the
/// PREDICATE count is pinned as well.
#[test]
fn every_admit_filter_is_mirrored_on_the_candidate_side() {
    // Admit's binds, and where the candidate side answers each.
    const MIRRORS: &[(&str, &str)] = &[
        ("$1", "the candidate id set itself — this IS the window"),
        ("$2", "m.owner_id — `p.owner_id = ANY($7::uuid[])`"),
        (
            "$3",
            "kind — `admit_side_restriction` renders `hm.kind`/`m.kind`",
        ),
        ("$4", "schema_id — `p.schema_id = ANY($8::text[])`"),
        (
            "$5",
            "since — a `proxiima_core.memory_created_at(...) >= $4` row predicate",
        ),
        (
            "$6",
            "until — a `proxiima_core.memory_created_at(...) <= $5` row predicate",
        ),
    ];
    for heads_only in [false, true] {
        let admit = super::search_admit_sql(heads_only);
        let admit = admit.as_str();
        for (bind, why) in MIRRORS {
            assert!(
                admit.contains(bind),
                "admit no longer binds {bind} ({why}); re-derive the mirror"
            );
        }
        assert!(
            !admit.contains("$7"),
            "admit grew a seventh filter. Mirror it in \
                 `admit_side_restriction` or prove the candidate side already \
                 applies it, then extend MIRRORS. Do not delete this line."
        );
        // The bind-free door. `search_admit_sql` renders every top-level
        // conjunct at one indentation, so counting that spelling counts
        // the filters — including the ones that carry no `$n` and would
        // otherwise walk past MIRRORS unnoticed.
        assert_eq!(
            admit.matches(ADMIT_PREDICATE_PREFIX).count(),
            ADMIT_PREDICATES,
            "admit's predicate count moved. A filter with no bind is \
                 still a filter the candidate window has to know about: \
                 mirror it in `admit_side_restriction`, or prove the \
                 candidate side already applies it, then update this count \
                 and MIRRORS together. Do not delete this line."
        );
    }
    // …and supersession, which is not a bind but a shape.
    assert_ne!(
        super::search_admit_sql(true),
        super::search_admit_sql(false),
        "supersession is admit's seventh filter, spelled as a FROM clause"
    );
    assert!(
        super::search_admit_sql(true)
            .as_str()
            .contains("EXISTS (SELECT 1 FROM proxiima_core.memory_head h"),
        "…and that is the shape `admit_side_restriction` mirrors"
    );
}

/// A request the builders can render every clause of.
pub(super) fn request_with_tags() -> MemorySearchRequest {
    MemorySearchRequest {
        read_owners: vec![OwnerId::from(proxiima_core::UserId::new(uuid::Uuid::nil()))],
        scope: proxiima_core::verbs::query::MemorySearchScope::Owner(OwnerId::from(
            proxiima_core::UserId::new(uuid::Uuid::nil()),
        )),
        query: "needle".into(),
        mode: SearchMode::Lexical,
        supersession: SupersessionStatus::HeadsOnly,
        limit: 8,
        kind: Some(EntityKind::Fact),
        schema_id: None,
        tags: vec!["bucket-0".into()],
        tag_match: TagMatch::Any,
        since: None,
        until: None,
        order: SearchOrder::Relevance,
        min_score: None,
        semantic_weight: None,
        after: None,
        semantic: None,
    }
}

/// Reachability follows the projection contract, including without tags.
/// Comparable scores and the supported renderer remain separate gates.
#[test]
fn the_merge_admits_only_spiices_that_declare_a_comparable_shape() {
    let frozen = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let note = frozen
        .search_projections()
        .iter()
        .find(|projection| projection.schema_id.as_str() == "core/agent-note-v1")
        .expect("the core note projection")
        .clone();
    let mut foreign = note.clone();
    foreign.schema_id = proxiima_core::SchemaId::new("search-notes/note-v1".into());
    foreign.sidecar_table = "search_notes.note_v1".into();
    foreign.projection_table = "search_notes.projection".into();
    foreign.tag_column = None;
    let mut divergent = foreign.clone();
    divergent.band_comparability = proxiima_core::spiice::BandComparability::Divergent {
        why: "scores have another scale",
    };
    let mut divergent_core = note.clone();
    divergent_core.band_comparability = divergent.band_comparability;
    let mut unsupported = foreign.clone();
    unsupported.rank_source = proxiima_core::spiice::RankSource::SidecarWithProjectionOwner {
        why: "the spiice supplies its own renderer",
    };
    unsupported.render_bands = None;
    let mut request = request_with_tags();
    request.tags.clear();
    let reaches =
        |request: &MemorySearchRequest,
         projection: &proxiima_core::verbs::schema::MemorySearchProjection| {
            !super::core_search_spiices(request, std::slice::from_ref(projection)).is_empty()
        };
    for (shape, projection, expected) in [
        ("core", &note, true),
        ("comparable foreign without tags", &foreign, true),
        ("divergent core", &divergent_core, false),
        ("divergent", &divergent, false),
        ("unsupported renderer", &unsupported, false),
    ] {
        assert_eq!(reaches(&request, projection), expected, "{shape}");
    }
    let projections = [note.clone(), foreign.clone()];
    assert_eq!(super::core_search_spiices(&request, &projections).len(), 2);
    request.schema_id = Some(foreign.schema_id.clone());
    assert!(reaches(&request, &foreign));
    request.schema_id = Some(note.schema_id.clone());
    assert!(
        !reaches(&request, &foreign),
        "schema selection still narrows"
    );
    request.schema_id = None;
    request.kind = Some(EntityKind::Abstraction);
    assert!(!reaches(&request, &foreign), "kind selection still narrows");
    request.kind = None;
    assert!(reaches(&request, &foreign));
    request.tags = vec!["bucket-0".into()];
    assert!(
        !reaches(&request, &foreign),
        "a tag filter needs a tag column"
    );
    foreign.tag_column = Some("tags".into());
    assert!(reaches(&request, &foreign));
}
