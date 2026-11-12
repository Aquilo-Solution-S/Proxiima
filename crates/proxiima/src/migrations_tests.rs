use std::borrow::Cow;
use std::collections::BTreeSet;

use proxiima_core::spiice::{
    EmbeddingRecipe, Provenance, SchemaContract, SchemaEvolution, SchemaRef, SearchProjectionDecl,
    TransferRule,
};
use proxiima_core::verbs::schema::PayloadKind;
use proxiima_storage_pg::PgSidecarRegistryFrozen;
use sqlx::SqlSafeStr;
use sqlx::migrate::{MigrateError, Migration, MigrationType, Migrator};

use proxiima_core::SqlText;

use proxiima_spiice::{NamedMigrator, is_spiice_ledger_id, spiice_ledger_table};

use super::{
    LedgerConflict, LedgerRow, MigrationError, MigrationSource, MissingMigrationPolicy,
    PendingMigration, VectorStorage, check_recorded_versions, composed_ledgers, core_source,
    ledger_conflict, order_pending_operations, pending_migrations, prepare_sources,
    schema_evolution_schedule, unapplied, unrecorded,
};

fn select_version(version: i64) -> SqlText {
    let mut sql = SqlText::new("SELECT ");
    sql.push_u64(u64::try_from(version).expect("a test version is positive"))
        .push(";");
    sql
}

const fn evolution_schema(name: &'static str, version: u32) -> SchemaContract {
    SchemaContract {
        id: SchemaRef::new("owner", name, version),
        kind: PayloadKind::Fact,
        sidecar_table: None,
        search: SearchProjectionDecl::None {
            why: "test",
            tag_column: None,
        },
        embedding: EmbeddingRecipe::Never { why: "test" },
        transfer: TransferRule::StaysOnKey,
        provenance: Provenance::None,
        surfaces: &[],
        natural_key_columns: proxiima_core::NaturalKeyDeclaration::EMPTY,
    }
}

const OWNER_V1: SchemaContract = evolution_schema("record", 1);
const OWNER_V2: SchemaContract = evolution_schema("record", 2);
const OWNER_V3: SchemaContract = evolution_schema("record", 3);

static OWNER_V1_SOURCES: &[SchemaContract] = &[OWNER_V1];
static OWNER_V2_SOURCES: &[SchemaContract] = &[OWNER_V2];
static OWNER_V2_TARGETS: &[SchemaContract] = &[OWNER_V2];
static OWNER_V3_TARGETS: &[SchemaContract] = &[OWNER_V3];

const TEST_SPIICE_VERSION: i64 = 20_260_612_000_010;

fn migrator(versions: &[i64]) -> Migrator {
    let migrations = versions
        .iter()
        .map(|version| {
            Migration::new(
                *version,
                Cow::Owned(format!("test {version}")),
                MigrationType::Simple,
                select_version(*version).into_sql_str(),
                false,
            )
        })
        .collect();
    Migrator {
        migrations: Cow::Owned(migrations),
        ..Migrator::DEFAULT
    }
}

#[test]
fn duplicate_versions_fail_before_run() {
    let err = prepare_sources(
        [
            NamedMigrator::new("alpha", migrator(&[TEST_SPIICE_VERSION])),
            NamedMigrator::new("beta", migrator(&[TEST_SPIICE_VERSION])),
        ],
        VectorStorage::Local,
    )
    .expect_err("duplicate migration version should fail");

    assert!(matches!(
        err,
        MigrationError::DuplicateVersion {
            version: TEST_SPIICE_VERSION,
            first_source: "alpha",
            second_source: "beta",
            ..
        }
    ));
}

#[test]
fn evolution_schedule_orders_implicit_chain_independent_of_declaration_order() {
    static SECOND: SchemaEvolution = SchemaEvolution::new(
        "second",
        OWNER_V2_SOURCES,
        OWNER_V3_TARGETS,
        TEST_SPIICE_VERSION,
        &[],
        "test",
        "test",
    );
    static FIRST: SchemaEvolution = SchemaEvolution::new(
        "first",
        OWNER_V1_SOURCES,
        OWNER_V2_TARGETS,
        TEST_SPIICE_VERSION,
        &[],
        "test",
        "test",
    );
    let source = NamedMigrator::spiice("owner", migrator(&[TEST_SPIICE_VERSION]));
    let sources = [&source];
    let steps = [SECOND, FIRST];
    let ordered =
        schema_evolution_schedule(&sources, &steps).expect("anchors and implicit edge are valid");
    assert_eq!(
        ordered.iter().map(|step| step.id).collect::<Vec<_>>(),
        ["first", "second"]
    );
}

#[test]
fn evolution_schedule_orders_independent_same_lane_steps_by_anchor() {
    const FIRST_ANCHOR: i64 = TEST_SPIICE_VERSION + 6;
    const SECOND_ANCHOR: i64 = TEST_SPIICE_VERSION + 7;
    const OTHER_V1: SchemaContract = evolution_schema("other-record", 1);
    const OTHER_V2: SchemaContract = evolution_schema("other-record", 2);
    static OTHER_SOURCES: &[SchemaContract] = &[OTHER_V1];
    static OTHER_TARGETS: &[SchemaContract] = &[OTHER_V2];
    static SECOND: SchemaEvolution = SchemaEvolution::new(
        "anchor-second",
        OTHER_SOURCES,
        OTHER_TARGETS,
        SECOND_ANCHOR,
        &[],
        "test",
        "test",
    );
    static FIRST: SchemaEvolution = SchemaEvolution::new(
        "anchor-first",
        OWNER_V1_SOURCES,
        OWNER_V2_TARGETS,
        FIRST_ANCHOR,
        &[],
        "test",
        "test",
    );
    let source = NamedMigrator::spiice("owner", migrator(&[FIRST_ANCHOR, SECOND_ANCHOR]));
    let steps = [SECOND, FIRST];
    let ordered = schema_evolution_schedule(&[&source], &steps)
        .expect("same-lane anchors provide an implicit order");
    assert_eq!(
        ordered.iter().map(|step| step.id).collect::<Vec<_>>(),
        [FIRST.id, SECOND.id]
    );
}

#[test]
fn evolution_schedule_rejects_dependency_backwards_against_lane_anchor() {
    const FIRST_ANCHOR: i64 = TEST_SPIICE_VERSION + 8;
    const SECOND_ANCHOR: i64 = TEST_SPIICE_VERSION + 9;
    static SECOND: SchemaEvolution = SchemaEvolution::new(
        "backwards-second",
        &[evolution_schema("other-record", 1)],
        &[evolution_schema("other-record", 2)],
        SECOND_ANCHOR,
        &[],
        "test",
        "test",
    );
    static FIRST: SchemaEvolution = SchemaEvolution::new(
        "backwards-first",
        OWNER_V1_SOURCES,
        OWNER_V2_TARGETS,
        FIRST_ANCHOR,
        &["backwards-second"],
        "test",
        "test",
    );
    let source = NamedMigrator::spiice("owner", migrator(&[FIRST_ANCHOR, SECOND_ANCHOR]));
    let steps = [FIRST, SECOND];
    assert!(matches!(
        schema_evolution_schedule(&[&source], &steps),
        Err(MigrationError::EvolutionCycle { .. })
    ));
}

#[test]
fn evolution_schedule_rejects_anchor_on_the_wrong_spiice_lane() {
    static STEP: SchemaEvolution = SchemaEvolution::new(
        "wrong-owner",
        OWNER_V1_SOURCES,
        OWNER_V2_TARGETS,
        TEST_SPIICE_VERSION,
        &[],
        "test",
        "test",
    );
    let source = NamedMigrator::spiice("other", migrator(&[TEST_SPIICE_VERSION]));
    assert!(matches!(
        schema_evolution_schedule(&[&source], &[STEP]),
        Err(MigrationError::EvolutionAnchorMissing { .. })
    ));
}

#[test]
fn evolution_preview_orders_source_sql_around_steps() {
    const FIRST_ANCHOR: i64 = TEST_SPIICE_VERSION + 1;
    const SECOND_ANCHOR: i64 = TEST_SPIICE_VERSION + 2;
    const TRAILING: i64 = TEST_SPIICE_VERSION + 3;
    static SECOND: SchemaEvolution = SchemaEvolution::new(
        "preview-second",
        OWNER_V2_SOURCES,
        OWNER_V3_TARGETS,
        SECOND_ANCHOR,
        &[],
        "test",
        "test",
    );
    static FIRST: SchemaEvolution = SchemaEvolution::new(
        "preview-first",
        OWNER_V1_SOURCES,
        OWNER_V2_TARGETS,
        FIRST_ANCHOR,
        &[],
        "test",
        "test",
    );
    let source = NamedMigrator::spiice("owner", migrator(&[FIRST_ANCHOR, SECOND_ANCHOR, TRAILING]));
    let ledger = source.ledger();
    let sql = |version| PendingMigration::Sql {
        ledger: ledger.clone(),
        version,
        description: version.to_string(),
        checksum: Vec::new(),
    };
    let plan = order_pending_operations(
        vec![
            PendingMigration::SchemaEvolution {
                spiice: "owner",
                id: SECOND.id,
                from: vec!["record@2".to_owned()],
                to: vec!["record@3".to_owned()],
            },
            sql(FIRST_ANCHOR),
            PendingMigration::SchemaEvolution {
                spiice: "owner",
                id: FIRST.id,
                from: vec!["record@1".to_owned()],
                to: vec!["record@2".to_owned()],
            },
            sql(SECOND_ANCHOR),
            sql(TRAILING),
        ],
        &[&source],
        &[SECOND, FIRST],
    )
    .expect("preview schedule");
    assert_eq!(
        plan.iter()
            .map(|operation| match operation {
                PendingMigration::Sql { version, .. } => version.to_string(),
                PendingMigration::SchemaEvolution { id, .. } => (*id).to_owned(),
                PendingMigration::AdoptPreviousHead { .. } => "adopt".to_owned(),
                PendingMigration::CoreDefinitions { .. } => "definitions".to_owned(),
            })
            .collect::<Vec<_>>(),
        [
            FIRST_ANCHOR.to_string(),
            FIRST.id.to_owned(),
            SECOND_ANCHOR.to_string(),
            SECOND.id.to_owned(),
            TRAILING.to_string(),
        ]
    );
}

#[test]
fn evolution_preview_skips_completed_prerequisite() {
    const FIRST_ANCHOR: i64 = TEST_SPIICE_VERSION + 4;
    const SECOND_ANCHOR: i64 = TEST_SPIICE_VERSION + 5;
    static SECOND: SchemaEvolution = SchemaEvolution::new(
        "preview-completed-second",
        OWNER_V2_SOURCES,
        OWNER_V3_TARGETS,
        SECOND_ANCHOR,
        &[],
        "test",
        "test",
    );
    static FIRST: SchemaEvolution = SchemaEvolution::new(
        "preview-completed-first",
        OWNER_V1_SOURCES,
        OWNER_V2_TARGETS,
        FIRST_ANCHOR,
        &[],
        "test",
        "test",
    );
    let source = NamedMigrator::spiice("owner", migrator(&[FIRST_ANCHOR, SECOND_ANCHOR]));
    let ledger = source.ledger();
    let plan = order_pending_operations(
        vec![
            PendingMigration::SchemaEvolution {
                spiice: "owner",
                id: SECOND.id,
                from: vec!["record@2".to_owned()],
                to: vec!["record@3".to_owned()],
            },
            PendingMigration::Sql {
                ledger,
                version: SECOND_ANCHOR,
                description: String::new(),
                checksum: Vec::new(),
            },
        ],
        &[&source],
        &[FIRST, SECOND],
    )
    .expect("preview schedule");
    assert!(plan.iter().all(|operation| !matches!(
        operation,
        PendingMigration::SchemaEvolution { id, .. } if *id == FIRST.id
    )));
    assert!(matches!(
        plan.as_slice(),
        [PendingMigration::Sql { version, .. }, PendingMigration::SchemaEvolution { id, .. }]
            if *version == SECOND_ANCHOR && *id == SECOND.id
    ));
}

#[test]
fn evolution_schedule_does_not_link_producers_across_payload_kinds() {
    const FACT: SchemaContract = OWNER_V1;
    const ABS: SchemaContract = SchemaContract {
        kind: PayloadKind::Abstraction,
        ..OWNER_V1
    };
    static FACT_SOURCES: &[SchemaContract] = &[FACT];
    static ABS_SOURCES: &[SchemaContract] = &[ABS];
    static FACT_STEP: SchemaEvolution = SchemaEvolution::new(
        "kind-fact",
        FACT_SOURCES,
        OWNER_V2_TARGETS,
        TEST_SPIICE_VERSION,
        &[],
        "test",
        "test",
    );
    static ABS_STEP: SchemaEvolution = SchemaEvolution::new(
        "kind-abstraction",
        ABS_SOURCES,
        &[SchemaContract {
            kind: PayloadKind::Abstraction,
            ..OWNER_V2
        }],
        TEST_SPIICE_VERSION,
        &[],
        "test",
        "test",
    );
    let source = NamedMigrator::spiice("owner", migrator(&[TEST_SPIICE_VERSION]));
    let steps = [ABS_STEP, FACT_STEP];
    let ordered = schema_evolution_schedule(&[&source], &steps)
        .expect("different payload kinds are independent");
    assert_eq!(
        ordered.iter().map(|step| step.id).collect::<Vec<_>>(),
        [ABS_STEP.id, FACT_STEP.id]
    );
}

#[test]
fn a_declared_ledger_is_kept_and_two_spiices_never_share_one() {
    let mut declared = migrator(&[TEST_SPIICE_VERSION]);
    declared.dangerous_set_table_name("acme._sqlx_migrations");
    assert_eq!(
        NamedMigrator::spiice("acme", declared)
            .migrator()
            .table_name,
        "acme._sqlx_migrations",
        "a ledger already in use is never renamed under a live database"
    );

    let err = prepare_sources(
        [
            NamedMigrator::spiice("a-b", migrator(&[TEST_SPIICE_VERSION])),
            NamedMigrator::spiice("a_b", migrator(&[TEST_SPIICE_VERSION + 1])),
        ],
        VectorStorage::Local,
    )
    .expect_err("two spiices on one ledger");
    assert!(matches!(
        err,
        MigrationError::DuplicateLedger {
            first_source: "a-b",
            second_source: "a_b",
            ..
        }
    ));
    prepare_sources(
        [
            NamedMigrator::new("alpha", migrator(&[TEST_SPIICE_VERSION])),
            NamedMigrator::new("beta", migrator(&[TEST_SPIICE_VERSION + 1])),
        ],
        VectorStorage::Local,
    )
    .expect("spiices still on core's shared ledger are a warning, not a refusal");
}

#[test]
fn a_spiice_id_that_would_need_quoting_is_refused() {
    for id in [
        "",
        "Acme",
        "9acme",
        "acme.forge",
        "acme forge",
        "acme\"",
        &"a".repeat(41),
    ] {
        let result = std::panic::catch_unwind(|| spiice_ledger_table(id));
        assert!(result.is_err(), "{id:?} must be refused");
        assert!(!is_spiice_ledger_id(id));
    }
}

/// Every shape a spiice ledger meets in practice, against the lane a
/// binary ships. Versions stand for dated files: 1 is the oldest.
#[test]
fn a_lane_that_does_not_continue_its_ledger_is_refused() {
    let continues: [(&str, &[i64], &[i64]); 8] = [
        ("a fresh ledger", &[1, 2], &[]),
        ("an upgrade", &[1, 2, 3], &[1, 2]),
        ("the same release", &[1, 2], &[1, 2]),
        (
            "an older release's fleet beside a newer one",
            &[1, 2],
            &[1, 2, 3],
        ),
        (
            "a shed first file, a new one pending",
            &[2, 3, 4],
            &[1, 2, 3],
        ),
        (
            "a shed middle file, a new one pending",
            &[1, 3, 4],
            &[1, 2, 3],
        ),
        ("a branch merged under a newer version", &[1, 2, 3], &[1, 3]),
        // Not caught here: the kept first migration looks like an
        // upgrade, so only a publish-time check sees the squash.
        ("a lane squashed in part", &[1, 9], &[1, 2, 3]),
    ];
    for (shape, lane, recorded) in continues {
        assert_eq!(ledger_conflict(lane, recorded), None, "{shape}");
    }

    assert_eq!(
        ledger_conflict(
            &[20_260_924_000_060],
            &[20_260_822_000_060, 20_260_904_000_060]
        ),
        Some(LedgerConflict::Replaced {
            root: 20_260_924_000_060,
            unknown: vec![20_260_822_000_060, 20_260_904_000_060],
        }),
        "a new baseline over the lane it replaced (forgejo 0.0.7 over 0.0.5)"
    );
    assert_eq!(
        ledger_conflict(
            &[20_260_822_000_060, 20_260_904_000_060],
            &[20_260_924_000_060]
        ),
        Some(LedgerConflict::Replaced {
            root: 20_260_822_000_060,
            unknown: vec![20_260_924_000_060],
        }),
        "the replaced lane's fleet meeting the database the new baseline built"
    );
    assert_eq!(
        ledger_conflict(&[1, 2, 3, 4], &[1, 9]),
        Some(LedgerConflict::Diverged {
            pending: vec![2, 3, 4],
            unknown: vec![9],
        }),
        "an older release after a later one squashed its files 2-4 into 9"
    );
    assert_eq!(
        ledger_conflict(&[1, 2, 5, 12], &[1, 2, 7, 9, 11]),
        Some(LedgerConflict::Diverged {
            pending: vec![5],
            unknown: vec![7, 9, 11],
        }),
        "only the pending versions below an unknown one, and the unknown ones above them"
    );
}

#[test]
fn a_boot_that_skips_migrations_names_what_is_not_applied() {
    assert_eq!(unapplied(&[1, 2], &[1, 2, 3]), None);
    assert_eq!(
        unapplied(&[1, 2, 3], &[1]),
        Some(LedgerConflict::Unapplied {
            pending: vec![2, 3]
        })
    );
}

fn recorded(migration: &Migration) -> LedgerRow {
    LedgerRow {
        version: migration.version,
        checksum: migration.checksum.to_vec(),
        success: true,
    }
}

fn pending_versions(source: &NamedMigrator, rows: &[LedgerRow]) -> Result<Vec<i64>, MigrateError> {
    unrecorded(&source.lane(), rows).map(|pending| {
        pending
            .into_iter()
            .map(|migration| migration.version)
            .collect()
    })
}

#[test]
fn a_source_enumerates_what_a_run_applies() {
    let mut migrations = migrator(&[3, 1, 2]).migrations.into_owned();
    migrations.push(Migration::new(
        2,
        Cow::Borrowed("test 2 down"),
        MigrationType::ReversibleDown,
        SqlText::new("SELECT 2;").into_sql_str(),
        false,
    ));
    let source = NamedMigrator::new(
        "alpha",
        Migrator {
            migrations: Cow::Owned(migrations),
            ..Migrator::DEFAULT
        },
    );

    assert_eq!(
        source.lane_versions(),
        vec![1, 2, 3],
        "the lineage view is ascending, and a down migration is not applied"
    );
    assert_eq!(
        source
            .lane()
            .iter()
            .map(|migration| migration.description.as_ref())
            .collect::<Vec<_>>(),
        ["test 3", "test 1", "test 2"],
        "the order the migrator holds, which `run_direct` walks; the up migration of version 2, not its down"
    );
}

#[test]
fn a_source_out_of_order_is_planned_and_refused_in_the_order_a_run_walks_it() {
    let source = NamedMigrator::new("alpha", migrator(&[3, 1, 2]));
    assert_eq!(
        pending_versions(&source, &[]).expect("empty"),
        [3, 1, 2],
        "pending in the order `run_direct` applies"
    );

    let lane = source.lane();
    let mut rows: Vec<LedgerRow> = lane.iter().map(|m| recorded(m)).collect();
    rows.sort_by_key(|row| row.version);
    for row in &mut rows {
        if row.version != 2 {
            row.checksum = vec![0];
        }
    }
    assert!(
        matches!(
            pending_versions(&source, &rows),
            Err(MigrateError::VersionMismatch(3))
        ),
        "the first changed checksum in the order the run walks, not the lowest version"
    );
}

/// Two sources on `tables`, the second a version above the first.
fn on_tables(first: &str, second: &str) -> Vec<NamedMigrator> {
    [("first", first, 0), ("second", second, 1)]
        .into_iter()
        .map(|(source, table, step)| {
            let mut declared = migrator(&[TEST_SPIICE_VERSION + step]);
            declared.dangerous_set_table_name(table.to_owned());
            NamedMigrator::new(source, declared)
        })
        .collect()
}

/// The runner's first step, then the plan's: both before any query, the
/// plan on a pool nothing listens behind.
async fn lineup_refusals(
    sources: impl Fn() -> Vec<NamedMigrator>,
) -> (Result<(), MigrationError>, Result<(), MigrationError>) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(300))
        .connect_lazy("postgres://nobody@127.0.0.1:1/none")
        .expect("a lazy pool parses its URL and connects later");
    (
        prepare_sources(sources(), VectorStorage::Local).map(drop),
        pending_migrations(
            &pool,
            &sources(),
            &PgSidecarRegistryFrozen::default(),
            VectorStorage::Local,
        )
        .await
        .map(drop),
    )
}

#[tokio::test]
async fn a_ledger_is_matched_by_the_table_postgres_resolves_not_by_its_spelling() {
    let one_table = |err: Result<(), MigrationError>| match err {
        Err(MigrationError::DuplicateLedger {
            table,
            first_source: "first",
            second_source: "second",
        }) => table,
        other => panic!("expected DuplicateLedger, got {other:?}"),
    };
    let long = "y".repeat(70);
    for (first, second, table) in [
        (
            "_sqlx_migrations_host",
            "public._sqlx_migrations_host",
            "public._sqlx_migrations_host",
        ),
        (
            "public._sqlx_migrations_host",
            "public.\"_sqlx_migrations_host\"",
            "public._sqlx_migrations_host",
        ),
        (
            "_SQLX_Migrations_Host",
            "_sqlx_migrations_host",
            "public._sqlx_migrations_host",
        ),
        (
            "\"_sqlx_migrations_host\"",
            "PUBLIC._SQLX_MIGRATIONS_HOST",
            "public._sqlx_migrations_host",
        ),
        (
            "_sqlx_migrations_host",
            "public . _sqlx_migrations_host",
            "public._sqlx_migrations_host",
        ),
        ("\"Host\"", "public.\"Host\"", "public.Host"),
        ("\"a\"\"b\"", "public.\"a\"\"b\"", "public.a\"b"),
        (
            &format!("x{long}a"),
            &format!("x{long}b"),
            &format!("public.x{}", &long[..62]),
        ),
    ] {
        let (run, plan) = lineup_refusals(|| on_tables(first, second)).await;
        assert_eq!(one_table(run), table, "{first} and {second}");
        assert_eq!(one_table(plan), table, "{first} and {second}");
    }
}

#[tokio::test]
async fn ledgers_that_resolve_to_different_tables_are_not_one_ledger() {
    for (first, second) in [
        ("\"Host\"", "\"host\""),
        ("\"Host\"", "host"),
        ("a.t", "b.t"),
        ("\"a.t\"", "a.t"),
        // Schema `a.b`, table `c` and schema `a`, table `b.c` print alike.
        ("\"a.b\".c", "a.\"b.c\""),
    ] {
        let (run, plan) = lineup_refusals(|| on_tables(first, second)).await;
        run.unwrap_or_else(|err| panic!("{first} and {second}: {err:?}"));
        assert!(
            matches!(plan, Err(MigrationError::Connection(_))),
            "{first} and {second} reach for the database: {plan:?}"
        );
    }
}

/// A lineup `main` completes is never refused for how a ledger is
/// spelled: a name the parser does not read is compared as written.
#[tokio::test]
async fn a_spelling_outside_postgres_identifier_rules_is_compared_as_written() {
    for name in [
        "\"unclosed",
        "public.",
        ".host",
        "a..b",
        "a.b.c",
        "\"\"",
        "a-b",
        "a/* comment */.b",
        "\"a\"b",
        "",
    ] {
        assert_eq!(super::parse_ledger_name(name), None, "{name:?}");
        let (run, plan) = lineup_refusals(|| on_tables(name, "public._sqlx_migrations_ok")).await;
        run.unwrap_or_else(|err| panic!("{name:?}: {err:?}"));
        assert!(
            matches!(plan, Err(MigrationError::Connection(_))),
            "{name:?} reaches for the database: {plan:?}"
        );

        let (run, plan) = lineup_refusals(|| on_tables(name, name)).await;
        for same in [run, plan] {
            assert!(
                matches!(
                    &same,
                    Err(MigrationError::DuplicateLedger { table, .. }) if table == name
                ),
                "the same spelling twice is one ledger: {same:?}"
            );
        }
    }
}

#[tokio::test]
async fn whitespace_around_the_parts_of_a_ledger_name_is_accepted() {
    let (run, plan) =
        lineup_refusals(|| on_tables("public . _sqlx_migrations_host", "other.t")).await;
    run.expect("the runner takes it");
    assert!(
        matches!(plan, Err(MigrationError::Connection(_))),
        "the plan reaches for the database: {plan:?}"
    );
}

#[test]
fn a_ledger_select_splices_the_resolved_quoted_name() {
    let sql = |ledger: &str| super::ledger_select_sql(ledger).map(|s| s.as_str().to_owned());
    let select = |table: &str| {
        Some(format!(
            "SELECT version, checksum, success FROM {table} ORDER BY version"
        ))
    };
    for (ledger, table) in [
        (
            "public._sqlx_migrations_host",
            "\"public\".\"_sqlx_migrations_host\"",
        ),
        ("public.\"Host\"", "\"public\".\"Host\""),
        ("\"a\"\"b\"", "\"public\".\"a\"\"b\""),
        (
            "PUBLIC._SQLX_MIGRATIONS_HOST",
            "\"public\".\"_sqlx_migrations_host\"",
        ),
        ("public . x", "\"public\".\"x\""),
    ] {
        assert_eq!(sql(ledger), select(table), "{ledger:?}");
    }
    for ledger in ["a.b.c", "x y", "", "\"\""] {
        assert_eq!(sql(ledger), None, "{ledger:?}");
    }
}

#[test]
fn a_create_schema_splices_the_resolved_quoted_name() {
    let sql = |schema: &str| super::create_schema_sql(schema).map(|s| s.as_str().to_owned());
    for (schema, quoted) in [
        ("host", "\"host\""),
        ("Host", "\"host\""),
        ("\"My Schema\"", "\"My Schema\""),
        ("\"Host\"", "\"Host\""),
        ("\"a\"\"b\"", "\"a\"\"b\""),
    ] {
        assert_eq!(
            sql(schema),
            Some(format!("CREATE SCHEMA IF NOT EXISTS {quoted}")),
            "{schema:?}"
        );
    }
    for schema in ["a.b", "", "x y"] {
        assert_eq!(sql(schema), None, "{schema:?}");
    }
}

#[test]
fn a_ledger_name_is_read_as_postgres_reads_an_identifier() {
    let identity = |name: &str| super::parse_ledger_name(name).expect(name);
    let pair = |schema: &str, table: &str| (schema.to_owned(), table.to_owned());
    assert_eq!(identity("host"), pair("public", "host"));
    assert_eq!(
        identity(" public\t.\n\"Host\" "),
        pair("public", "Host"),
        "whitespace around parts and dots"
    );
    assert_eq!(identity("Acme.Host"), pair("acme", "host"));
    assert_eq!(identity("\"Acme\".\"Host\""), pair("Acme", "Host"));
    assert_eq!(
        identity("\"a.b\""),
        pair("public", "a.b"),
        "a dot inside quotes"
    );
    assert_eq!(identity("\"a\"\"b\""), pair("public", "a\"b"));
    assert_eq!(identity("a$b"), pair("public", "a$b"));
    assert_eq!(
        identity("\u{c9}t\u{e9}"),
        pair("public", "\u{c9}t\u{e9}"),
        "ASCII only"
    );
    let clipped = format!("{}\u{e9}", "a".repeat(62));
    assert_eq!(
        identity(&clipped),
        pair("public", &"a".repeat(62)),
        "cut to 63 bytes on a character boundary"
    );
}

#[test]
fn a_ledger_is_named_as_spiice_ledger_table_names_it() {
    assert_eq!(
        core_source(VectorStorage::Local).ledger(),
        "public._sqlx_migrations"
    );
    assert_eq!(
        NamedMigrator::spiice("fixture-spiice", migrator(&[1])).ledger(),
        spiice_ledger_table("fixture-spiice")
    );
    let mut declared = migrator(&[1]);
    declared.dangerous_set_table_name("acme._sqlx_migrations");
    assert_eq!(
        NamedMigrator::spiice("acme", declared).ledger(),
        "acme._sqlx_migrations"
    );
    let mut unqualified = migrator(&[1]);
    unqualified.dangerous_set_table_name("_sqlx_migrations_host");
    assert_eq!(
        NamedMigrator::new("host", unqualified).ledger(),
        "public._sqlx_migrations_host",
        "a run pins `search_path` to public"
    );
}

#[test]
fn the_rows_of_a_ledger_leave_what_a_run_would_apply() {
    let source = NamedMigrator::new("alpha", migrator(&[1, 2, 3, 4]));
    let lane = source.lane();
    let first_two: Vec<LedgerRow> = lane[..2].iter().map(|m| recorded(m)).collect();

    assert_eq!(pending_versions(&source, &[]).expect("empty"), [1, 2, 3, 4]);
    assert_eq!(
        pending_versions(&source, &first_two).expect("behind"),
        [3, 4]
    );
    let mut with_unknown = first_two.clone();
    with_unknown.push(LedgerRow {
        version: 9,
        checksum: vec![9],
        success: true,
    });
    assert_eq!(
        pending_versions(&source, &with_unknown).expect("a recorded version it does not ship"),
        [3, 4],
        "the per-source calculation follows the composed ledger's validation"
    );

    let mut changed_second = first_two.clone();
    changed_second[1].checksum = vec![0];
    let mut changed_both = changed_second.clone();
    changed_both[0].checksum = vec![0];
    assert!(matches!(
        pending_versions(&source, &changed_second),
        Err(MigrateError::VersionMismatch(2))
    ));
    assert!(
        matches!(
            pending_versions(&source, &changed_both),
            Err(MigrateError::VersionMismatch(1))
        ),
        "the first in the order the run walks"
    );

    let mut dirty = changed_both.clone();
    dirty.push(LedgerRow {
        version: 7,
        checksum: vec![7],
        success: false,
    });
    dirty.push(LedgerRow {
        version: 5,
        checksum: vec![5],
        success: false,
    });
    assert!(
        matches!(
            pending_versions(&source, &dirty),
            Err(MigrateError::Dirty(5))
        ),
        "a dirty row is refused before any checksum, lowest version first"
    );
}

#[test]
fn a_ledger_recognizes_only_the_union_of_its_composed_sources() {
    let core = NamedMigrator::new("core", migrator(&[1, 2]));
    let mut host_migrator = migrator(&[3, 4]);
    host_migrator.dangerous_set_table_name("public.\"_sqlx_migrations\"");
    let host = NamedMigrator::new("host", host_migrator);
    let own = NamedMigrator::spiice("own", migrator(&[9]));
    let ledgers = composed_ledgers(&[&core, &host, &own]);
    assert_eq!(ledgers.len(), 2);
    assert_eq!(ledgers[0].versions, BTreeSet::from([1, 2, 3, 4]));
    assert_eq!(ledgers[1].versions, BTreeSet::from([9]));
    let row = |version, checksum: u8| LedgerRow {
        version,
        checksum: vec![checksum],
        success: true,
    };
    let known = vec![row(1, 1), row(3, 3)];
    check_recorded_versions(&ledgers[0].versions, &known, MissingMigrationPolicy::Reject)
        .expect("both sources' versions belong to the shared ledger");
    let unknown = vec![row(1, 1), row(9, 9)];
    assert!(matches!(
        check_recorded_versions(
            &ledgers[0].versions,
            &unknown,
            MissingMigrationPolicy::Reject
        ),
        Err(MigrateError::VersionMissing(9))
    ));
    check_recorded_versions(
        &ledgers[0].versions,
        &unknown,
        MissingMigrationPolicy::Allow,
    )
    .expect("the host explicitly permits missing versions");
    assert_eq!(
        MissingMigrationPolicy::default(),
        MissingMigrationPolicy::Reject
    );
}

#[tokio::test]
async fn a_plan_refuses_a_lineup_before_it_issues_a_query() {
    // Nothing listens here: a query would fail as `Connection` or
    // `LedgerRead`, not as the refusals below.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect_lazy("postgres://nobody@127.0.0.1:1/none")
        .expect("a lazy pool parses its URL and connects later");

    let duplicate = pending_migrations(
        &pool,
        &[
            NamedMigrator::new("alpha", migrator(&[TEST_SPIICE_VERSION])),
            NamedMigrator::new("beta", migrator(&[TEST_SPIICE_VERSION])),
        ],
        &PgSidecarRegistryFrozen::default(),
        VectorStorage::Local,
    )
    .await
    .expect_err("duplicate migration version");
    assert!(matches!(
        duplicate,
        MigrationError::DuplicateVersion {
            version: TEST_SPIICE_VERSION,
            first_source: "alpha",
            second_source: "beta",
            ..
        }
    ));

    let shared = pending_migrations(
        &pool,
        &[
            NamedMigrator::spiice("a-b", migrator(&[TEST_SPIICE_VERSION])),
            NamedMigrator::spiice("a_b", migrator(&[TEST_SPIICE_VERSION + 1])),
        ],
        &PgSidecarRegistryFrozen::default(),
        VectorStorage::Local,
    )
    .await
    .expect_err("two spiices on one ledger");
    assert!(matches!(shared, MigrationError::DuplicateLedger { .. }));

    let reachable = pending_migrations(
        &pool,
        &[],
        &PgSidecarRegistryFrozen::default(),
        VectorStorage::Local,
    )
    .await;
    assert!(
        matches!(reachable, Err(MigrationError::Connection(_))),
        "a valid lineup does reach for the database: {reachable:?}"
    );
}
