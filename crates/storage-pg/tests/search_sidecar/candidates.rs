//! Direct candidate calls must authorize ids, scores, and counts before returning.

use std::collections::BTreeSet;
use std::time::Duration;

use super::*;
use proxiima_core::OwnerScope;
use proxiima_core::{AccessCeiling, GoalAccess, Role, SqlText};
use proxiima_core::{StorageError, StorageRule};
use proxiima_pg_testkit::DbGuard;
use proxiima_storage_pg::test_fixtures::{TestGoal, fresh_pg, insert_goal_grant};
use proxiima_storage_pg::verbs::query::ranked_candidate_sql_for_tests;
use proxiima_storage_pg::{begin_owner_transaction, map_err};
use sqlx::PgConnection;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
type CandidateRow = (Uuid, f32, i64);

#[derive(Clone, Copy)]
enum TextLeg {
    Lexical,
    Substring,
}

impl TextLeg {
    fn sql(self) -> &'static str {
        match self {
            Self::Lexical => {
                "SELECT t, lexical_score, count(*) OVER () FROM proxiima_core.search_lexical_candidates($1, $2, NULL, NULL, NULL, NULL, $3, ARRAY['core/agent-note-v1'], true, false, true, 'fact', false)"
            }
            Self::Substring => {
                "SELECT t, lexical_score, count(*) OVER () FROM proxiima_core.search_substring_candidates($1, $2, NULL, NULL, NULL, NULL, $3, ARRAY['core/agent-note-v1'], true, false, true, 'fact', false)"
            }
        }
    }

    fn query(self) -> &'static str {
        match self {
            Self::Lexical => "needle",
            Self::Substring => "%needle%",
        }
    }
}

async fn text_candidates(
    connection: &mut PgConnection,
    leg: TextLeg,
    owners: &[Uuid],
    window: Option<i64>,
) -> Result<Vec<CandidateRow>, sqlx::Error> {
    sqlx::query_as(leg.sql())
        .bind(leg.query())
        .bind(window)
        .bind(owners)
        .fetch_all(connection)
        .await
}

async fn vector_candidates(
    connection: &mut PgConnection,
    owners: &[Uuid],
    vector: &pgvector::Vector,
) -> Result<Vec<CandidateRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT memory_id, similarity_score, count(*) OVER ()
           FROM proxiima_core.vector_candidates_d1024($1, 'test-embed', $2, 8, true)",
    )
    .bind(owners)
    .bind(vector)
    .fetch_all(connection)
    .await
}

fn assert_code(error: &sqlx::Error, expected: &str) {
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some(expected),
        "{error}"
    );
}

fn assert_search_scope_refused(error: sqlx::Error) {
    match map_err(error) {
        StorageError::Refused { rule, .. } => {
            assert_eq!(rule, StorageRule::SearchScopeNotAuthorized);
        }
        other => panic!("expected a search scope refusal, got {other:?}"),
    }
}

struct Fixture {
    pg: PgStorage,
    owner: OwnerId,
    other: OwnerId,
    ours: Uuid,
    theirs: Uuid,
    _database: DbGuard,
}

impl Fixture {
    async fn new() -> TestResult<Self> {
        let (pg, database) = fresh_pg("direct_search_candidates").await;
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let other = OwnerId::from(UserId::new(Uuid::now_v7()));
        let ours = seed_note(
            &pg,
            owner,
            "An observation",
            "A needle surrounded by other words",
        )
        .await?;
        let theirs = seed_note(
            &pg,
            other,
            "needle needle needle needle",
            "needle needle needle needle needle needle needle needle",
        )
        .await?;
        seed_embedding(&pg, owner, ours, &embed_vector_xy(0.6, 0.8)).await?;
        seed_embedding(&pg, other, theirs, &embed_vector()).await?;
        Ok(Self {
            pg,
            owner,
            other,
            ours,
            theirs,
            _database: database,
        })
    }

    fn scope(&self) -> OwnerScope {
        owner_scope_for_tests(&[self.owner])
    }
}

#[tokio::test]
async fn direct_candidates_refuse_missing_forged_and_invalid_inputs() -> TestResult {
    let fixture = Fixture::new().await?;
    let runtime = fixture.pg.clone_pool_for_backend();
    let owners = [fixture.owner.into_inner()];
    let forged = [fixture.owner.into_inner(), fixture.other.into_inner()];
    let scope = fixture.scope();
    for leg in [TextLeg::Lexical, TextLeg::Substring] {
        let mut valid = begin_owner_transaction(&runtime, &scope).await?;
        let rows = text_candidates(valid.as_mut(), leg, &owners, Some(8)).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, fixture.ours);
        assert_eq!(rows[0].2, 1);
        valid.commit().await?;
        let mut missing = runtime.begin().await?;
        let error = text_candidates(missing.as_mut(), leg, &owners, Some(8))
            .await
            .unwrap_err();
        assert_search_scope_refused(error);
        missing.rollback().await?;
        let mut scoped = begin_owner_transaction(&runtime, &scope).await?;
        let error = text_candidates(scoped.as_mut(), leg, &forged, Some(8))
            .await
            .unwrap_err();
        assert_search_scope_refused(error);
        scoped.rollback().await?;
    }
    for window in [
        None,
        Some(-1),
        Some(0),
        Some(i64::from(note_projection().overfetch_k) + 1),
    ] {
        let mut tx = begin_owner_transaction(&runtime, &scope).await?;
        let error = text_candidates(tx.as_mut(), TextLeg::Lexical, &owners, window)
            .await
            .unwrap_err();
        assert_code(&error, "PX002");
        tx.rollback().await?;
    }
    let mut valid = begin_owner_transaction(&runtime, &scope).await?;
    let rows = vector_candidates(valid.as_mut(), &owners, &embed_vector()).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, fixture.ours);
    assert_eq!(rows[0].2, 1);
    valid.commit().await?;
    let mut missing = runtime.begin().await?;
    let error = vector_candidates(missing.as_mut(), &owners, &embed_vector())
        .await
        .unwrap_err();
    assert_search_scope_refused(error);
    missing.rollback().await?;
    let mut scoped = begin_owner_transaction(&runtime, &scope).await?;
    let error = vector_candidates(scoped.as_mut(), &forged, &embed_vector())
        .await
        .unwrap_err();
    assert_search_scope_refused(error);
    scoped.rollback().await?;
    let mut wrong_width = begin_owner_transaction(&runtime, &scope).await?;
    let error = vector_candidates(
        wrong_width.as_mut(),
        &owners,
        &pgvector::Vector::from(vec![1.0; 384]),
    )
    .await
    .unwrap_err();
    assert_code(&error, "PX002");
    wrong_width.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn stale_accelerator_owners_never_leak_candidate_ids_scores_or_counts() -> TestResult {
    let fixture = Fixture::new().await?;
    let runtime = fixture.pg.clone_pool_for_backend();
    let both = owner_scope_for_tests(&[fixture.owner, fixture.other]);
    let both_ids = [fixture.owner.into_inner(), fixture.other.into_inner()];
    let mut baseline = begin_owner_transaction(&runtime, &both).await?;
    let lexical = text_candidates(baseline.as_mut(), TextLeg::Lexical, &both_ids, Some(8)).await?;
    assert_eq!(
        lexical[0].0, fixture.theirs,
        "foreign lexical hit ranks first"
    );
    assert!(lexical[0].1 > lexical[1].1);
    let ours_score = lexical.iter().find(|row| row.0 == fixture.ours).unwrap().1;
    let semantic = vector_candidates(baseline.as_mut(), &both_ids, &embed_vector()).await?;
    assert_eq!(
        semantic[0].0, fixture.theirs,
        "foreign vector hit ranks first"
    );
    assert!(semantic[0].1 > semantic[1].1);
    baseline.commit().await?;
    let admin = fixture.pg.pool_for_tests();
    sqlx::query("UPDATE proxiima_core.projection SET owner_id = $1 WHERE memory_id = $2")
        .bind(fixture.owner.into_inner())
        .bind(fixture.theirs)
        .execute(admin)
        .await?;
    sqlx::query("UPDATE proxiima_core.embeddings SET owner_id = $1 WHERE entity_id = $2")
        .bind(fixture.owner.into_inner())
        .bind(fixture.theirs)
        .execute(admin)
        .await?;
    sqlx::query("UPDATE proxiima_core.embedding_heads SET owner_id = $1 WHERE entity_id = $2")
        .bind(fixture.owner.into_inner())
        .bind(fixture.theirs)
        .execute(admin)
        .await?;
    let memory_owner: Uuid =
        sqlx::query_scalar("SELECT owner_id FROM proxiima_core.memory WHERE t = $1")
            .bind(fixture.theirs)
            .fetch_one(admin)
            .await?;
    assert_eq!(memory_owner, fixture.other.into_inner());
    let scope = fixture.scope();
    let owners = [fixture.owner.into_inner()];
    // Requested owners also narrow a scope that can read both memories.
    for candidate_scope in [&scope, &both] {
        for leg in [TextLeg::Lexical, TextLeg::Substring] {
            let mut tx = begin_owner_transaction(&runtime, candidate_scope).await?;
            let rows = text_candidates(tx.as_mut(), leg, &owners, Some(8)).await?;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].0, fixture.ours);
            assert_eq!(rows[0].2, 1, "count contains only authorized candidates");
            if matches!(leg, TextLeg::Lexical) {
                assert_eq!(
                    rows[0].1.to_bits(),
                    ours_score.to_bits(),
                    "foreign score does not enter output"
                );
            }
            tx.commit().await?;
        }
        let mut tx = begin_owner_transaction(&runtime, candidate_scope).await?;
        let rows = vector_candidates(tx.as_mut(), &owners, &embed_vector()).await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, fixture.ours);
        assert_eq!(rows[0].2, 1);
        assert!((rows[0].1 - 0.6).abs() < 0.000_001);
        tx.commit().await?;
    }
    let mut req = search_req(fixture.owner, "needle");
    for mode in [
        SearchMode::Lexical,
        SearchMode::Semantic,
        SearchMode::Hybrid,
    ] {
        req.mode = mode;
        req.semantic = Some(test_semantic(embed_vector().to_vec()));
        let page = fixture
            .pg
            .search_memories(Some(&scope), &req, &[note_projection()])
            .await?;
        assert_eq!(page.results.len(), 1);
        assert_eq!(page.results[0].memory_id.into_inner(), fixture.ours);
        assert!(!page.has_more);
        if mode != SearchMode::Semantic {
            assert_eq!(
                page.results[0].lexical_score.to_bits(),
                ours_score.to_bits()
            );
        }
        if mode != SearchMode::Lexical {
            assert!((page.results[0].similarity_score - 0.6).abs() < 0.000_001);
        }
    }
    Ok(())
}

/// The substring function plans only the sidecars a request names; the rows
/// it returns are those of the named schemas whatever the ordering, and a
/// request naming none of them (or no schema at all) returns nothing.
#[tokio::test]
async fn substring_candidates_answer_for_the_requested_schemas_only() -> TestResult {
    let fixture = Fixture::new().await?;
    let runtime = fixture.pg.clone_pool_for_backend();
    let scope = fixture.scope();
    let owners = [fixture.owner.into_inner()];
    let note = "core/agent-note-v1".to_owned();
    let utterance = "core/utterance-v1".to_owned();
    let cases: [(Option<Vec<String>>, usize); 5] = [
        (Some(vec![note.clone()]), 1),
        (Some(vec![utterance.clone(), note.clone()]), 1),
        (Some(vec![utterance]), 0),
        (Some(Vec::new()), 0),
        (None, 0),
    ];
    for recency in [false, true] {
        for (schemas, expected) in &cases {
            let mut tx = begin_owner_transaction(&runtime, &scope).await?;
            let rows: Vec<(Uuid, f32)> = sqlx::query_as(
                "SELECT t, lexical_score FROM proxiima_core.search_substring_candidates(
                     '%needle%', 8, NULL, NULL, NULL, NULL, $1, $2, true, $3, true, 'fact', false)",
            )
            .bind(&owners[..])
            .bind(schemas.as_deref())
            .bind(recency)
            .fetch_all(tx.as_mut())
            .await?;
            assert_eq!(rows.len(), *expected, "{schemas:?} recency={recency}");
            if *expected == 1 {
                assert_eq!(rows[0].0, fixture.ours);
            }
            tx.commit().await?;
        }
    }
    Ok(())
}

/// The candidate role reads the sidecars without owner row security so the
/// trigram index can serve its `LIKE`; what a search returns is still gated by
/// `memory`. Another owner's note matches the partial word (the first
/// assertion shows it does) and does not come back to a scope that cannot read
/// it.
#[tokio::test]
async fn substring_fallback_never_returns_another_owners_match() -> TestResult {
    let fixture = Fixture::new().await?;
    let both = owner_scope_for_tests(&[fixture.owner, fixture.other]);
    let mut req = search_req(fixture.owner, "eedl");
    req.read_owners = vec![fixture.owner, fixture.other];
    let page = fixture
        .pg
        .search_memories(Some(&both), &req, &[note_projection()])
        .await?;
    let mut handles: Vec<Uuid> = page
        .results
        .iter()
        .map(|hit| hit.memory_id.into_inner())
        .collect();
    handles.sort_unstable();
    let mut expected = vec![fixture.ours, fixture.theirs];
    expected.sort_unstable();
    assert_eq!(handles, expected, "both notes match the partial word");

    let ours_only = fixture.scope();
    let page = fixture
        .pg
        .search_memories(
            Some(&ours_only),
            &search_req(fixture.owner, "eedl"),
            &[note_projection()],
        )
        .await?;
    assert_eq!(page.results.len(), 1);
    assert_eq!(page.results[0].memory_id.into_inner(), fixture.ours);
    Ok(())
}

/// How long a test waits for an event before it fails.
const PATIENCE: Duration = Duration::from_secs(30);

/// One request of the lexical candidate function, in its parameter order.
#[derive(Clone, Debug)]
pub(super) struct LexicalCall {
    pub(super) query: &'static str,
    pub(super) limit: i64,
    pub(super) tags: Option<Vec<String>>,
    pub(super) since: Option<time::OffsetDateTime>,
    pub(super) until: Option<time::OffsetDateTime>,
    pub(super) before: Option<Uuid>,
    pub(super) owners: Vec<Uuid>,
    pub(super) schemas: Vec<String>,
    pub(super) rescue: Option<bool>,
    pub(super) recency: Option<bool>,
    pub(super) heads_only: bool,
    pub(super) kind: Option<&'static str>,
    pub(super) tag_all: bool,
}

impl LexicalCall {
    /// A heads-only relevance call with rescue, for `owners`' memories of
    /// `kind` in `schemas`.
    pub(super) fn new(
        query: &'static str,
        owners: &[Uuid],
        schemas: &[&str],
        kind: &'static str,
    ) -> Self {
        Self {
            query,
            limit: 8,
            tags: None,
            since: None,
            until: None,
            before: None,
            owners: owners.to_vec(),
            schemas: schemas.iter().map(|schema| (*schema).to_owned()).collect(),
            rescue: Some(true),
            recency: Some(false),
            heads_only: true,
            kind: Some(kind),
            tag_all: false,
        }
    }
}

/// A candidate row: the memory, its schema and its score's bits.
pub(super) type LexicalRow = (Uuid, String, u32);

/// What the lexical function of `projections`' spiice returns for `call`, and
/// what its single pass returns, both read in one transaction as the
/// candidate role under `scope`.
pub(super) async fn lexical_and_single_pass(
    pg: &PgStorage,
    scope: &OwnerScope,
    projections: &[MemorySearchProjection],
    call: &LexicalCall,
) -> TestResult<(Vec<LexicalRow>, Vec<LexicalRow>)> {
    let head = projections.first().ok_or("no projection")?;
    let function =
        proxiima_storage_pg::search_candidates::function_name(&head.projection_table, false)?;
    let mut function_call = SqlText::new("SELECT t, schema_id, lexical_score FROM ");
    function_call.push_ident(&function).push(
        "($1::text, $2::bigint, $3::text[], $4::timestamptz, $5::timestamptz, $6::uuid, \
         $7::uuid[], $8::text[], $9::boolean, $10::boolean, $11::boolean, $12::text, $13::boolean)",
    );
    let single_pass = ranked_candidate_sql_for_tests(projections, call.recency.unwrap_or(false))?;
    let mut tx = begin_owner_transaction(pg.pool_for_tests(), scope).await?;
    sqlx::query("SET LOCAL ROLE proxiima_search")
        .execute(tx.as_mut())
        .await?;
    let function_rows: Vec<(Uuid, String, f32)> = sqlx::query_as(function_call)
        .bind(call.query)
        .bind(call.limit)
        .bind(call.tags.as_deref())
        .bind(call.since)
        .bind(call.until)
        .bind(call.before)
        .bind(&call.owners)
        .bind(&call.schemas)
        .bind(call.rescue)
        .bind(call.recency)
        .bind(call.heads_only)
        .bind(call.kind)
        .bind(call.tag_all)
        .fetch_all(tx.as_mut())
        .await?;
    let single_pass_rows: Vec<(Uuid, String, f32)> = sqlx::query_as(single_pass)
        .bind(call.query)
        .bind(call.limit)
        .bind(call.tags.as_deref())
        .bind(call.since)
        .bind(call.until)
        .bind(call.before)
        .bind(&call.owners)
        .bind(&call.schemas)
        .bind(call.rescue)
        .bind(call.heads_only)
        .bind(call.kind)
        .bind(call.tag_all)
        .fetch_all(tx.as_mut())
        .await?;
    tx.rollback().await?;
    let bits = |rows: Vec<(Uuid, String, f32)>| -> Vec<LexicalRow> {
        rows.into_iter()
            .map(|(t, schema, score)| (t, schema, score.to_bits()))
            .collect()
    };
    Ok((bits(function_rows), bits(single_pass_rows)))
}

/// Assert that the function returned the single pass's rows, each once.
pub(super) fn assert_single_pass_rows(
    rows: &[LexicalRow],
    single_pass: &[LexicalRow],
    call: &LexicalCall,
) {
    assert_eq!(rows, single_pass, "{call:?}");
    let distinct: BTreeSet<Uuid> = rows.iter().map(|row| row.0).collect();
    assert_eq!(distinct.len(), rows.len(), "no row twice: {call:?}");
}

/// The lowest exact-band score, as the statements render it.
fn exact_floor() -> f32 {
    note_projection()
        .render_bands
        .expect("core declares render bands")
        .exact
        .floor
}

/// The staged lexical function returns the single pass's rows, in its order
/// and with its scores: for a query whose exact matches fill the window and
/// one whose rescue matches fill what they leave, for a phrase, a negation,
/// stop words and a single word, for every value of `rescue` and `recency`,
/// and under every filter the function takes.
#[tokio::test]
async fn the_staged_lexical_function_returns_the_single_pass_rows() -> TestResult {
    let (pg, _database) = fresh_pg("staged_lexical_candidates").await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let scope = owner_scope_for_tests(&[owner]);
    let tags: [&[&str]; 3] = [&["red"], &["blue"], &["red", "blue"]];
    let mut harbor_lantern = Vec::new();
    for n in 1..=12 {
        let body = format!("{}lantern", "harbor ".repeat(n));
        harbor_lantern.push(
            seed_note_lang(
                &pg,
                owner,
                &format!("harbor lantern {n}"),
                &body,
                None,
                tags[n % 3],
            )
            .await?,
        );
    }
    for n in 1..=10 {
        let title = format!("harbor {n}");
        seed_note_lang(&pg, owner, &title, "harbor quay", None, tags[n % 3]).await?;
    }
    for n in 1..=6 {
        seed_note(&pg, owner, &format!("lantern {n}"), "lantern wick").await?;
    }
    for n in 1..=2 {
        seed_note(&pg, owner, &format!("meadow quartz {n}"), "meadow quartz").await?;
    }
    for n in 1..=5 {
        seed_note(&pg, owner, &format!("meadow {n}"), &"meadow ".repeat(n)).await?;
    }
    for n in 1..=4 {
        seed_note(&pg, owner, &format!("quartz {n}"), "quartz vein").await?;
    }
    seed_note(&pg, owner, "violet meadow", "a violet meadow at dawn").await?;
    seed_note(&pg, owner, "meadow violet", "the meadow turned violet").await?;
    let draft = TestNote::new("harbor lantern draft", "harbor lantern")
        .create(&pg, owner)
        .await?;
    TestNote::new("harbor lantern final", "harbor lantern again")
        .handle(draft.handle)
        .create(&pg, owner)
        .await?;

    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let projections = registry.search_projections();
    let core_schemas: Vec<&str> = projections
        .iter()
        .map(|projection| projection.schema_id.as_str())
        .collect();
    let owners = [owner.into_inner()];
    let base = LexicalCall::new("harbor lantern", &owners, &["core/agent-note-v1"], "fact");

    // Filled: three exact matches fill a window of three.
    let (rows, _) = lexical_and_single_pass(
        &pg,
        &scope,
        projections,
        &LexicalCall {
            limit: 3,
            ..base.clone()
        },
    )
    .await?;
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter()
            .all(|row| f32::from_bits(row.2) >= exact_floor())
    );
    // Under-filled: two exact matches, then rescue matches to the limit.
    let (rows, _) = lexical_and_single_pass(
        &pg,
        &scope,
        projections,
        &LexicalCall {
            query: "meadow quartz",
            ..base.clone()
        },
    )
    .await?;
    let exact: Vec<bool> = rows
        .iter()
        .map(|row| f32::from_bits(row.2) >= exact_floor())
        .collect();
    assert_eq!(
        exact,
        [true, true, false, false, false, false, false, false]
    );

    let now = time::OffsetDateTime::now_utc();
    let hour = time::Duration::hours(1);
    let mut middle = harbor_lantern.clone();
    middle.sort_unstable();
    let middle = middle[middle.len() / 2];
    let filters: Vec<LexicalCall> = vec![
        LexicalCall {
            tags: Some(vec!["red".into()]),
            ..base.clone()
        },
        LexicalCall {
            tags: Some(vec!["red".into(), "blue".into()]),
            tag_all: true,
            ..base.clone()
        },
        LexicalCall {
            schemas: vec!["core/utterance-v1".into()],
            ..base.clone()
        },
        LexicalCall {
            schemas: core_schemas
                .iter()
                .map(|schema| (*schema).to_owned())
                .collect(),
            ..base.clone()
        },
        LexicalCall {
            kind: None,
            ..base.clone()
        },
        LexicalCall {
            kind: Some("abstraction"),
            ..base.clone()
        },
        LexicalCall {
            since: Some(now - hour),
            until: Some(now + hour),
            ..base.clone()
        },
        LexicalCall {
            since: Some(now + hour),
            ..base.clone()
        },
        LexicalCall {
            until: Some(now - hour),
            ..base.clone()
        },
        LexicalCall {
            before: Some(middle),
            ..base.clone()
        },
        LexicalCall {
            heads_only: false,
            ..base.clone()
        },
    ];
    let mut calls = Vec::new();
    for query in [
        "harbor lantern",
        "meadow quartz",
        "\"violet meadow\"",
        "harbor -lantern",
        "-lantern",
        "the and of",
        "quartz",
        "lantern harbor wick",
    ] {
        for rescue in [Some(true), Some(false), None] {
            for recency in [Some(false), Some(true), None] {
                for limit in [3, 40] {
                    calls.push(LexicalCall {
                        query,
                        limit,
                        rescue,
                        recency,
                        ..base.clone()
                    });
                }
            }
        }
    }
    for query in ["harbor lantern", "meadow quartz"] {
        for filter in &filters {
            for recency in [Some(false), Some(true)] {
                for limit in [3, 40] {
                    calls.push(LexicalCall {
                        query,
                        limit,
                        recency,
                        ..filter.clone()
                    });
                }
            }
        }
    }
    for call in &calls {
        let (rows, single_pass) = lexical_and_single_pass(&pg, &scope, projections, call).await?;
        assert_single_pass_rows(&rows, &single_pass, call);
    }
    Ok(())
}

/// Recency order interleaves the two kinds of match: a newer rescue match
/// comes before an older exact one, which relevance order puts first.
#[tokio::test]
async fn recency_puts_a_newer_rescue_match_before_an_older_exact_one() -> TestResult {
    let (pg, _database) = fresh_pg("staged_lexical_recency").await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let scope = owner_scope_for_tests(&[owner]);
    let older_exact = seed_v7_note(
        pg.pool_for_tests(),
        owner,
        "harbor lantern",
        "harbor lantern",
    )
    .await?;
    let newer_rescue = seed_v7_note(pg.pool_for_tests(), owner, "harbor", "harbor quay").await?;
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let owners = [owner.into_inner()];
    let base = LexicalCall::new("harbor lantern", &owners, &["core/agent-note-v1"], "fact");
    for (recency, first) in [(false, older_exact), (true, newer_rescue)] {
        let call = LexicalCall {
            limit: 1,
            recency: Some(recency),
            ..base.clone()
        };
        let (rows, single_pass) =
            lexical_and_single_pass(&pg, &scope, registry.search_projections(), &call).await?;
        assert_single_pass_rows(&rows, &single_pass, &call);
        assert_eq!(
            rows.iter().map(|row| row.0).collect::<Vec<_>>(),
            [first],
            "{call:?}"
        );
    }
    Ok(())
}

/// A Goal audience reaches published matches through both stages: the
/// published exact match, then the published rescue match, and none of the
/// source owner's private ones.
#[tokio::test]
async fn goal_published_matches_reach_both_lexical_stages() -> TestResult {
    let (pg, _database) = fresh_pg("staged_lexical_goal").await;
    let source = OwnerId::from(UserId::new(Uuid::now_v7()));
    let guest = UserId::new(Uuid::now_v7());
    let exact = seed_note(&pg, source, "harbor lantern", "harbor lantern").await?;
    let rescue = seed_note(&pg, source, "harbor", "harbor quay").await?;
    seed_note(&pg, source, "harbor lantern private", "harbor lantern").await?;
    seed_note(&pg, source, "harbor private", "harbor quay").await?;
    create_user(pg.pool_for_tests(), guest).await?;
    let goal = TestGoal::new(&Uuid::now_v7().to_string())
        .title("published harbor")
        .create(&pg, source)
        .await?
        .handle;
    let mut platform = pg.platform_scope_for_host().begin().await?;
    insert_goal_grant(
        &mut *platform,
        goal,
        guest.into_inner(),
        source.into_inner(),
        Role::new(AccessCeiling::Fact, AccessCeiling::None)?.with_goals(GoalAccess::Reads),
    )
    .await?;
    for published in [exact, rescue] {
        // Raw INSERT: no write path for goal_memory_publication reachable from test fixtures.
        sqlx::query(
            "INSERT INTO proxiima_core.goal_memory_publication
                 (goal_handle, memory_t, source_owner, source_generation,
                  controller_generation, permitted_use, published_by)
             SELECT $1, $2, mh.owner_id, mh.authority_generation,
                    gh.authority_generation, 'read', $3
               FROM proxiima_core.memory_head mh
               JOIN proxiima_core.goal_head gh ON gh.handle = $1
              WHERE mh.t = $2 AND mh.owner_id = $3",
        )
        .bind(goal)
        .bind(published)
        .bind(source.into_inner())
        .execute(&mut *platform)
        .await?;
    }
    platform.commit().await?;
    let scope = proxiima_core::test_fixtures::authenticated_context(
        proxiima_core::AuthzContext::for_subject(guest, proxiima_core::AuthPath::HostBearer),
    )
    .owner_scope()
    .ok_or("direct guest scope")?
    .clone();
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let owners = [source.into_inner()];
    let base = LexicalCall::new("harbor lantern", &owners, &["core/agent-note-v1"], "fact");
    for recency in [Some(false), Some(true)] {
        let call = LexicalCall {
            recency,
            ..base.clone()
        };
        let (rows, single_pass) =
            lexical_and_single_pass(&pg, &scope, registry.search_projections(), &call).await?;
        assert_single_pass_rows(&rows, &single_pass, &call);
        let mut found: Vec<Uuid> = rows.iter().map(|row| row.0).collect();
        found.sort_unstable();
        let mut published = vec![exact, rescue];
        published.sort_unstable();
        assert_eq!(found, published, "{call:?}");
    }
    Ok(())
}

/// Every stage of the lexical function reads the snapshot of the statement
/// that called it (`provolatile = 's'`): an underfilled two-term query runs
/// both the exact and rescue stages, and a rescue-only row committed while
/// the call waits to run is not in its result. A `VOLATILE` copy, whose
/// statements each read a snapshot of their own under `READ COMMITTED`,
/// returns it.
#[tokio::test]
async fn the_lexical_stages_read_their_callers_snapshot() -> TestResult {
    let (pg, _database) = fresh_pg("staged_lexical_snapshot").await;
    let admin = pg.pool_for_tests();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let scope = owner_scope_for_tests(&[owner]);
    let initial_exact = seed_note(&pg, owner, "harbor lantern", "harbor lantern").await?;
    let signature = "proxiima_core.search_lexical_candidates(text, bigint, text[], timestamptz, \
                     timestamptz, uuid, uuid[], text[], boolean, boolean, boolean, text, boolean)";
    let volatility: String =
        sqlx::query_scalar("SELECT provolatile::text FROM pg_proc WHERE oid = to_regprocedure($1)")
            .bind(signature)
            .fetch_one(admin)
            .await?;
    assert_eq!(volatility, "s");
    for (index, (alter, sees_late_match)) in [
        (None, false),
        (
            Some(
                "ALTER FUNCTION proxiima_core.search_lexical_candidates(text, bigint, text[], \
                 timestamptz, timestamptz, uuid, uuid[], text[], boolean, boolean, boolean, \
                 text, boolean) VOLATILE",
            ),
            true,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(alter) = alter {
            sqlx::query(alter).execute(admin).await?;
        }
        // Hold the gate inside a transaction so any early error rolls it back
        // and releases the advisory lock, and derive a per-iteration key so
        // concurrent test databases never share it.
        let lock_key = i64::from_be_bytes(
            Uuid::now_v7().as_bytes()[8..16]
                .try_into()
                .expect("8 bytes"),
        ) ^ i64::try_from(index).expect("small index");
        let mut gate_tx = admin.begin().await?;
        let gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate_tx)
            .await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(lock_key)
            .execute(&mut *gate_tx)
            .await?;

        let runtime = pg.clone_pool_for_backend();
        let scope_clone = scope.clone();
        let owners = vec![owner.into_inner()];
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
        let call = tokio::spawn(async move {
            let mut tx = begin_owner_transaction(&runtime, &scope_clone).await?;
            let caller_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(tx.as_mut())
                .await?;
            let _ = pid_tx.send(caller_pid);
            // The call evaluates its query argument, and so waits on this lock,
            // after its statement took its snapshot and before either stage
            // runs. Two distinct terms with only one exact match guarantee that
            // the rescue stage executes.
            let rows: Vec<Uuid> = sqlx::query_scalar(
                "SELECT t FROM proxiima_core.search_lexical_candidates(
                     'harbor lantern' || pg_advisory_xact_lock_shared($2)::text, 8, NULL, NULL, NULL, NULL, $1,
                     ARRAY['core/agent-note-v1'], true, false, true, 'fact', false)",
            )
            .bind(owners)
            .bind(lock_key)
            .fetch_all(tx.as_mut())
            .await?;
            tx.commit().await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(rows)
        });
        let caller_pid = tokio::time::timeout(PATIENCE, pid_rx)
            .await
            .expect("spawned backend pid")?;
        tokio::time::timeout(PATIENCE, async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (
                         SELECT 1 FROM pg_locks
                          WHERE pid = $1
                            AND locktype = 'advisory'
                            AND NOT granted
                            AND $2 = ANY(pg_blocking_pids(pid))
                     )",
                )
                .bind(caller_pid)
                .bind(gate_pid)
                .fetch_one(admin)
                .await?;
                if waiting {
                    return Ok::<_, sqlx::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await??;
        let late_rescue =
            seed_note(&pg, owner, "late quay", "a late rescue-only lantern note").await?;
        gate_tx.rollback().await?;
        let rows = call.await?.map_err(|error| error.to_string())?;
        assert!(rows.contains(&initial_exact));
        assert_eq!(rows.contains(&late_rescue), sees_late_match, "{alter:?}");
    }
    Ok(())
}

/// PostgreSQL's `tsquery` `=` ignores prefix (`:*`) and weight modifiers
/// (`'index'::tsquery = 'index:*'::tsquery` is true), so the shortcut must
/// compare `tsquerysend(...)` values; otherwise a prefix-only rescue match
/// is dropped in both Relevance and Recency orders.
#[tokio::test]
async fn prefix_only_rescue_matches_survive_the_equality_shortcut() -> TestResult {
    let (pg, _database) = fresh_pg("staged_lexical_prefix_rescue").await;
    let admin = pg.pool_for_tests();
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let scope = owner_scope_for_tests(&[owner]);
    // Under english stemming `indexable` stems to `indexabl`, which matches
    // `'index':*` (prefix) and does NOT match `'index'` (exact).
    let prefix_only = seed_note(&pg, owner, "catalog", "indexable entry").await?;

    // Verify the underlying PostgreSQL quirk directly before testing the
    // generated candidate function against it.
    let (eq_ignores_prefix, send_distinguishes): (bool, bool) = sqlx::query_as(
        "SELECT 'index'::tsquery = 'index:*'::tsquery,
                tsquerysend('index'::tsquery) IS DISTINCT FROM tsquerysend('index:*'::tsquery)",
    )
    .fetch_one(admin)
    .await?;
    assert!(eq_ignores_prefix);
    assert!(send_distinguishes);

    // Replace `tsquery_or_agg` inside this isolated test database so the
    // unmodified `search_lexical_candidates` sees exact `'index'` and rescue
    // `'index':*` for `'index prefix'`, mirroring a custom thesaurus/synonym
    // configuration without modifying server-wide tsearch_data files.
    sqlx::query(
        "CREATE FUNCTION proxiima_core.test_prefix_tsquery_sfunc(acc tsquery, elem tsquery)
         RETURNS tsquery LANGUAGE sql IMMUTABLE AS $$
             SELECT CASE
                 WHEN elem::text LIKE '%&%' THEN 'index'::tsquery
                 WHEN elem::text LIKE '%|%' THEN 'index:*'::tsquery
                 ELSE COALESCE(acc, elem)
             END
         $$",
    )
    .execute(admin)
    .await?;
    sqlx::query(
        "CREATE OR REPLACE AGGREGATE proxiima_core.tsquery_or_agg(tsquery) (
             SFUNC = proxiima_core.test_prefix_tsquery_sfunc,
             STYPE = tsquery
         )",
    )
    .execute(admin)
    .await?;
    sqlx::raw_sql(
        "GRANT EXECUTE ON FUNCTION proxiima_core.test_prefix_tsquery_sfunc(tsquery, tsquery) TO proxiima_search;
         GRANT EXECUTE ON FUNCTION proxiima_core.tsquery_or_agg(tsquery) TO proxiima_search;",
    )
    .execute(admin)
    .await?;

    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let owners = [owner.into_inner()];
    let base = LexicalCall::new("index prefix", &owners, &["core/agent-note-v1"], "fact");
    for recency in [Some(false), Some(true)] {
        let call = LexicalCall {
            recency,
            ..base.clone()
        };
        let (rows, single_pass) =
            lexical_and_single_pass(&pg, &scope, registry.search_projections(), &call).await?;
        assert_single_pass_rows(&rows, &single_pass, &call);
        assert_eq!(
            rows.iter().map(|row| row.0).collect::<Vec<_>>(),
            [prefix_only],
            "prefix-only rescue match must survive in recency={recency:?}"
        );
    }
    Ok(())
}

/// When the exact tsquery is empty (`''::tsquery`) while the rescue tsquery is
/// non-empty — such as `1 OR 2` under a German configuration that drops `uint`
/// mappings, where `websearch_to_tsquery` consumes `OR` as an operator between
/// two unmapped numbers while `plainto_tsquery` retains `'or'` — both Relevance
/// and Recency orders match the single-pass result.
#[tokio::test]
async fn empty_exact_query_with_nonempty_rescue_matches_single_pass() -> TestResult {
    let (pg, _database) = fresh_pg("staged_lexical_german_empty_exact").await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let scope = owner_scope_for_tests(&[owner]);

    // Configure an isolated German text search configuration that drops `uint`
    // tokens, then make it the sole active language before seeding so the note
    // is indexed with it and carries `'or'`.
    sqlx::raw_sql(
        "CREATE TEXT SEARCH CONFIGURATION proxiima_core.german_words_only (COPY = pg_catalog.german);
         ALTER TEXT SEARCH CONFIGURATION proxiima_core.german_words_only DROP MAPPING FOR uint;",
    )
    .execute(pg.pool_for_tests())
    .await?;
    let mut platform = pg.platform_scope_for_host().begin().await?;
    sqlx::query(
        "INSERT INTO proxiima_core.lexical_languages (config)
         VALUES ('proxiima_core.german_words_only'::regconfig)
         ON CONFLICT DO NOTHING",
    )
    .execute(&mut *platform)
    .await?;
    sqlx::query(
        "UPDATE proxiima_core.lexical_default
            SET config = 'proxiima_core.german_words_only'::regconfig",
    )
    .execute(&mut *platform)
    .await?;
    sqlx::query(
        "DELETE FROM proxiima_core.lexical_languages
          WHERE config <> 'proxiima_core.german_words_only'::regconfig",
    )
    .execute(&mut *platform)
    .await?;
    platform.commit().await?;

    let target = seed_note(&pg, owner, "deutsch", "notiz oder or eintrag").await?;

    let (exact_tsq, rescue_tsq): (String, String) = sqlx::query_as(
        "WITH q AS (
             SELECT proxiima_core.lexical_query_text(
                 proxiima_core.lexical_config(),
                 proxiima_core.lexical_scrub($1)
             ) AS analyzed
         )
         SELECT websearch_to_tsquery('proxiima_core.german_words_only'::regconfig, q.analyzed)::text,
                plainto_tsquery('proxiima_core.german_words_only'::regconfig, q.analyzed)::text
           FROM q",
    )
    .bind("1 OR 2")
    .fetch_one(pg.pool_for_tests())
    .await?;
    assert_eq!(exact_tsq, "", "precondition: exact query must be empty");
    assert_eq!(
        rescue_tsq, "'or'",
        "precondition: rescue query must retain 'or'"
    );

    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let owners = [owner.into_inner()];
    let base = LexicalCall::new("1 OR 2", &owners, &["core/agent-note-v1"], "fact");
    for rescue in [Some(true), Some(false), None] {
        for recency in [Some(false), Some(true), None] {
            let call = LexicalCall {
                rescue,
                recency,
                ..base.clone()
            };
            let (rows, single_pass) =
                lexical_and_single_pass(&pg, &scope, registry.search_projections(), &call).await?;
            assert_single_pass_rows(&rows, &single_pass, &call);
            if rescue == Some(true) {
                assert_eq!(
                    rows.iter().map(|row| row.0).collect::<Vec<_>>(),
                    [target],
                    "{call:?}"
                );
            } else {
                assert!(rows.is_empty(), "{call:?}");
            }
        }
    }
    Ok(())
}
