//! The spiice plumbing a spiice crate no longer hand-writes, end to end, on
//! the example spiice (`spiices/example`, a test and tutorial fixture):
//!
//! - `spiice_bundle!` composes a spiice with no hand-written impl;
//! - its migration's owner RLS is one `proxiima_core.install_owner_rls` call,
//!   and the split-role boot's runtime RLS guard accepts the result;
//! - `NamedMigrator::spiice` gives it its own ledger;
//! - the test support comes from `proxiima::testkit` alone: split-role
//!   databases, an owner-scoped authz, and the trigger drift assertion.
//!
//! The canonical bundle is `proxiima_example::ExampleSpiice`; the negative
//! variants below compose the same contract over an edited baseline.

use std::borrow::Cow;

use proxiima::spiice::{PgSidecarRegistry, SpiiceBundle};
use proxiima::testkit::{SplitRoleDb, assert_trigger_migrations, scoped_authz};
use proxiima::{AppInfo, Proxiima, QueryRequest, ServedSurface, SpiiceApp, company_owner};
use proxiima_core::SqlText;
use proxiima_example::testkit::{register_notebook, remember_note};
use proxiima_example::{
    BASELINE_SQL, ExampleSpiice, NoteV1, SPIICE_ID, baseline_sql_with_owner_rls,
};
use sqlx::SqlSafeStr;
use sqlx::migrate::{Migration, MigrationType, Migrator};
use uuid::Uuid;

#[path = "fixtures/migrated_app.rs"]
mod migrated_app;

/// The example's baseline version (ordinal 11's lane).
const EXAMPLE_MIGRATION_VERSION: i64 = 20_261_018_000_011;

/// The baseline with every table but `settings` classified.
fn gap_sql() -> SqlText {
    baseline_sql_with_owner_rls(&[
        "SELECT proxiima_core.install_owner_rls('proxiima_example', ARRAY['notebooks'], \
         ARRAY['note_v1', 'digest_v1'], '{}')",
    ])
}

/// How a host that adds its tables one migration at a time installs the
/// owner RLS of the tables that are not sidecars (the baseline's sidecars
/// are classified by `install_sidecar`): one `install_owner_rls_table` call
/// per table.
#[derive(Clone, Copy)]
enum PerTableHost {
    /// Calls the installer for every table, then asserts the census.
    CallsEveryTable,
    /// Forgets `settings` and never asserts the census.
    ForgetsSettings,
    /// Forgets `settings`; the census at the end of the migration catches it.
    ForgetsSettingsAssertsCensus,
}

fn per_table_sql(host: PerTableHost) -> SqlText {
    const SETTINGS: &str =
        "SELECT proxiima_core.install_owner_rls_table('proxiima_example', 'settings', 'ownerless')";
    const CENSUS: &str = "SELECT proxiima_core.assert_owner_rls_census('proxiima_example')";
    let rest: &[&str] = match host {
        PerTableHost::CallsEveryTable => &[SETTINGS, CENSUS],
        PerTableHost::ForgetsSettings => &[],
        PerTableHost::ForgetsSettingsAssertsCensus => &[CENSUS],
    };
    let calls: Vec<&str> = [
        "SELECT proxiima_core.install_owner_rls_table('proxiima_example', 'notebooks', 'owner_id')",
    ]
    .iter()
    .chain(rest)
    .copied()
    .collect();
    baseline_sql_with_owner_rls(&calls)
}

fn migrator(description: &'static str, sql: SqlText) -> Migrator {
    Migrator {
        migrations: Cow::Owned(vec![Migration::new(
            EXAMPLE_MIGRATION_VERSION,
            Cow::Borrowed(description),
            MigrationType::Simple,
            sql.into_sql_str(),
            false,
        )]),
        ..Migrator::DEFAULT
    }
}

fn gap_migrator() -> Migrator {
    migrator("example baseline, one table unclassified", gap_sql())
}

fn per_table_migrator(host: PerTableHost) -> Migrator {
    migrator(
        "example baseline, one owner-RLS call per table",
        per_table_sql(host),
    )
}

fn view_migrator(security_invoker: bool) -> Migrator {
    let options = if security_invoker {
        " WITH (security_invoker = true)"
    } else {
        ""
    };
    let mut sql = SqlText::new(BASELINE_SQL);
    sql.push("\nCREATE VIEW proxiima_example.notebook_view")
        .push(options)
        .push(" AS SELECT owner_id, name FROM proxiima_example.notebooks");
    migrator("example baseline with a view", sql)
}

mod kit_definer_view {
    proxiima::spiice::spiice_bundle! {
        bundle = KitDefinerViewSpiice,
        name = "proxiima-example",
        fact_schemas = [proxiima_example::NoteV1],
        abstraction_schemas = [proxiima_example::DigestV1],
        mcp_tools = [proxiima_example::ListNotebooksTool],
        contract = &proxiima_example::EXAMPLE_BASELINE_CONTRACT,
        migrations = super::view_migrator(false),
    }

    proxiima::spiice_app! {
        bundle = KitDefinerViewSpiice,
        id = "proxiima-example",
        title = "Kit fixture with a definer view",
    }
}

mod kit_invoker_view {
    proxiima::spiice::spiice_bundle! {
        bundle = KitInvokerViewSpiice,
        name = "proxiima-example",
        fact_schemas = [proxiima_example::NoteV1],
        abstraction_schemas = [proxiima_example::DigestV1],
        mcp_tools = [proxiima_example::ListNotebooksTool],
        contract = &proxiima_example::EXAMPLE_BASELINE_CONTRACT,
        migrations = super::view_migrator(true),
    }

    proxiima::spiice_app! {
        bundle = KitInvokerViewSpiice,
        id = "proxiima-example",
        title = "Kit fixture with an invoker view",
    }
}

mod kit_gap {
    proxiima::spiice::spiice_bundle! {
        bundle = KitGapSpiice,
        name = "proxiima-example",
        fact_schemas = [proxiima_example::NoteV1],
        abstraction_schemas = [proxiima_example::DigestV1],
        mcp_tools = [proxiima_example::ListNotebooksTool],
        contract = &proxiima_example::EXAMPLE_BASELINE_CONTRACT,
        migrations = super::gap_migrator(),
    }

    proxiima::spiice_app! {
        bundle = KitGapSpiice,
        id = "proxiima-example",
        title = "Kit fixture, one table unclassified",
    }
}

mod kit_per_table {
    proxiima::spiice::spiice_bundle! {
        bundle = KitPerTableSpiice,
        name = "proxiima-example",
        fact_schemas = [proxiima_example::NoteV1],
        abstraction_schemas = [proxiima_example::DigestV1],
        mcp_tools = [proxiima_example::ListNotebooksTool],
        contract = &proxiima_example::EXAMPLE_BASELINE_CONTRACT,
        migrations = super::per_table_migrator(super::PerTableHost::CallsEveryTable),
    }

    proxiima::spiice_app! {
        bundle = KitPerTableSpiice,
        id = "proxiima-example",
        title = "Kit fixture, one owner-RLS call per table",
    }
}

mod kit_forgotten_table {
    proxiima::spiice::spiice_bundle! {
        bundle = KitForgottenTableSpiice,
        name = "proxiima-example",
        fact_schemas = [proxiima_example::NoteV1],
        abstraction_schemas = [proxiima_example::DigestV1],
        mcp_tools = [proxiima_example::ListNotebooksTool],
        contract = &proxiima_example::EXAMPLE_BASELINE_CONTRACT,
        migrations = super::per_table_migrator(super::PerTableHost::ForgetsSettings),
    }

    proxiima::spiice_app! {
        bundle = KitForgottenTableSpiice,
        id = "proxiima-example",
        title = "Kit fixture, one table without a call",
    }
}

mod kit_forgotten_table_census {
    proxiima::spiice::spiice_bundle! {
        bundle = KitForgottenTableCensusSpiice,
        name = "proxiima-example",
        fact_schemas = [proxiima_example::NoteV1],
        abstraction_schemas = [proxiima_example::DigestV1],
        mcp_tools = [proxiima_example::ListNotebooksTool],
        contract = &proxiima_example::EXAMPLE_BASELINE_CONTRACT,
        migrations = super::per_table_migrator(super::PerTableHost::ForgetsSettingsAssertsCensus),
    }

    proxiima::spiice_app! {
        bundle = KitForgottenTableCensusSpiice,
        id = "proxiima-example",
        title = "Kit fixture, one table without a call, census at the end",
    }
}

/// Every optional key, so each macro arm is expanded and type-checked.
mod kit_full {
    fn no_workers(
        _ctx: &proxiima::spiice::SpiiceWorkerContext,
    ) -> Vec<proxiima::spiice::SpiiceWorker> {
        Vec::new()
    }

    fn configure(builder: proxiima::RuntimeBuilder) -> proxiima::RuntimeBuilder {
        builder.served_surface(proxiima::ServedSurface::All)
    }

    #[allow(clippy::unnecessary_wraps, reason = "SpiiceApp::services signature")]
    fn services(
        _ctx: &proxiima::AppContext,
    ) -> Result<proxiima::spiice::SpiiceServices, proxiima::spiice::SpiiceServiceError> {
        Ok(proxiima::spiice::SpiiceServices::default())
    }

    proxiima::spiice::spiice_bundle! {
        bundle = KitFullSpiice,
        name = "proxiima-example",
        display_name = "Example",
        fact_schemas = [proxiima_example::NoteV1, proxiima_example::StampV1],
        abstraction_schemas = [proxiima_example::DigestV1],
        mcp_tools = [proxiima_example::ListNotebooksTool],
        contract = &proxiima_example::EXAMPLE_CONTRACT,
        migrations = proxiima_example::migrator(),
        workers = no_workers,
    }

    proxiima::spiice_app! {
        bundle = KitFullSpiice,
        id = "kit-full",
        title = "Kit, every key",
        version = "1.2.3",
        configure = configure,
        services = services,
    }
}

async fn ledger_versions(db: &SplitRoleDb, table: &str) -> Option<Vec<i64>> {
    let admin = sqlx::PgPool::connect(&db.admin_url())
        .await
        .expect("admin pool");
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(table)
        .fetch_one(&admin)
        .await
        .expect("ledger probe");
    let versions = if exists {
        let query = match table {
            "public._sqlx_migrations" => sqlx::query_scalar(
                "SELECT version FROM public._sqlx_migrations WHERE NOT (version = ANY($1)) ORDER BY version",
            ).bind(proxiima_storage_pg::core_migrator().iter()
                .map(|migration| migration.version).collect::<Vec<_>>()),
            "public._sqlx_migrations_proxiima_example" => sqlx::query_scalar(
                "SELECT version FROM public._sqlx_migrations_proxiima_example ORDER BY version",
            ),
            other => panic!("no ledger query for {other}"),
        };
        Some(query.fetch_all(&admin).await.expect("ledger rows"))
    } else {
        None
    };
    admin.close().await;
    versions
}

/// Whether the split runtime role may change `table` — it must not on any
/// ledger: a deleted ledger row makes the next boot re-run that migration.
async fn runtime_may_write(db: &SplitRoleDb, table: &str) -> bool {
    let admin = sqlx::PgPool::connect(&db.admin_url())
        .await
        .expect("admin pool");
    let writable: bool = sqlx::query_scalar(
        "SELECT has_table_privilege('proxiima_test_runtime', $1, 'INSERT')
             OR has_table_privilege('proxiima_test_runtime', $1, 'UPDATE')
             OR has_table_privilege('proxiima_test_runtime', $1, 'DELETE')
             OR has_table_privilege('proxiima_test_runtime', $1, 'TRUNCATE')",
    )
    .bind(table)
    .fetch_one(&admin)
    .await
    .expect("privilege probe");
    admin.close().await;
    writable
}

/// A registry's `Debug` entries in a stable order: its map iterates in hash
/// order, so two registries with the same entries can print them differently.
fn debug_entries(debug: &str) -> Vec<String> {
    let mut entries: Vec<String> = debug
        .split("PgSidecarEntry {")
        .skip(1)
        .map(|part| {
            let last_field = part
                .find("projection_language:")
                .expect("an entry ends with its projection language");
            let close = part[last_field..].find(" }").expect("and closes after it");
            part[..last_field + close].to_owned()
        })
        .collect();
    entries.sort();
    entries
}

#[test]
fn the_bundle_is_the_hand_written_one() {
    assert_eq!(
        ExampleSpiice::app_info(),
        AppInfo {
            id: "proxiima-example",
            title: "Proxiima example spiice (test fixture)",
            version: env!("CARGO_PKG_VERSION"),
        }
    );
    let migrators = <ExampleSpiice as SpiiceBundle>::migrators();
    assert_eq!(migrators.len(), 1);
    assert_eq!(migrators[0].source(), "proxiima-example");
    assert_eq!(
        migrators[0].migrator().table_name,
        "public._sqlx_migrations_proxiima_example"
    );
    assert!(!migrators[0].migrator().ignore_missing);
    assert_eq!(
        kit_full::KitFullSpiice::app_info(),
        AppInfo {
            id: "kit-full",
            title: "Kit, every key",
            version: "1.2.3",
        }
    );

    let mut registry = PgSidecarRegistry::new();
    <ExampleSpiice as SpiiceBundle>::register_pg_sidecars(&mut registry);
    let mut hand_written = PgSidecarRegistry::new();
    hand_written.add_fact::<NoteV1>();
    hand_written.add_fact::<proxiima_example::StampV1>();
    hand_written.add_fact::<proxiima_example::SealedStampV1>();
    hand_written.add_abstraction::<proxiima_example::DigestV1>();
    assert_eq!(
        debug_entries(&format!("{registry:?}")),
        debug_entries(&format!("{hand_written:?}"))
    );
}

/// The example's frozen sidecars: the registry the generators read.
fn frozen_example_sidecars() -> proxiima::spiice::PgSidecarRegistryFrozen {
    let mut registry = proxiima::spiice::SpiiceRegistry::new();
    ExampleSpiice::register(&mut registry).expect("the example spiice registers");
    let registry = registry.try_freeze().expect("and freezes");
    let mut sidecars = PgSidecarRegistry::new();
    proxiima::spiice::register_core_pg_sidecars(&mut sidecars);
    ExampleSpiice::register_pg_sidecars(&mut sidecars);
    sidecars
        .freeze_against(&registry)
        .expect("the example's PG registrations match its contract")
}

/// What the generators emit for the example, as a migration would carry it:
/// every declaration statement, then every presence-family statement, minus
/// those `keep` refuses.
fn generated_trigger_migration(keep: impl Fn(&str) -> bool) -> String {
    let sidecars = frozen_example_sidecars();
    let mut artifacts = sidecars
        .declaration_trigger_artifacts(SPIICE_ID)
        .expect("declaration triggers");
    artifacts.extend(
        sidecars
            .presence_trigger_artifacts(SPIICE_ID)
            .expect("presence triggers"),
    );
    artifacts
        .into_iter()
        .map(|artifact| artifact.forward.as_str().to_owned())
        .filter(|forward| keep(forward))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_trigger_drift_assertion_accepts_the_generators_own_output() {
    assert_trigger_migrations::<ExampleSpiice>(
        SPIICE_ID,
        &[&generated_trigger_migration(|_| true)],
    );
}

#[test]
#[should_panic(expected = "appear in no migration")]
fn the_trigger_drift_assertion_names_a_missing_trigger() {
    let without_presence =
        generated_trigger_migration(|forward| !forward.contains("assert_declared_sidecar_present"));
    assert_trigger_migrations::<ExampleSpiice>(SPIICE_ID, &[&without_presence]);
}

/// Boot `F` as a single owner, register a notebook, write a note into it
/// through the Engine and read it back.
async fn write_and_read_a_note<F: SpiiceApp + 'static>(db: &SplitRoleDb, expect: &str) {
    migrated_app::migrate::<F>(db.platform_url())
        .await
        .expect("explicit migration before serving");
    let owner = company_owner(Uuid::now_v7());
    let built = Proxiima::<F>::app()
        .configure(|b| {
            b.database_url(db.runtime_url())
                .store_number(0)
                .platform_database_url(db.platform_url())
                .owner(owner)
                .allow_insecure_single_owner()
                .served_surface(ServedSurface::All)
        })
        .build()
        .await
        .expect(expect);

    let authz = scoped_authz(owner);
    let runtime = sqlx::PgPool::connect(db.runtime_url()).await.unwrap();
    let notebook_id = register_notebook(
        &runtime,
        authz.owner_scope().expect("owner scope"),
        owner,
        "boot",
    )
    .await
    .expect("the owner-id write policy admits the owner's notebook");
    let engine = built.host().engine();
    let memory_id = remember_note(
        engine,
        &authz,
        owner,
        &NoteV1 {
            notebook_id,
            key: "boot".to_owned(),
            body: "written under owner RLS".to_owned(),
            written_at: time::OffsetDateTime::now_utc(),
        },
    )
    .await
    .expect("the FK-parent write policy admits the owner's sidecar row");
    let response = engine
        .query(&authz, &QueryRequest::readable())
        .await
        .expect("owner query");
    assert!(
        response
            .memories
            .iter()
            .any(|memory| memory.id == memory_id),
        "the owner reads its own note back"
    );
    runtime.close().await;
    built.shutdown().await;
}

#[tokio::test]
async fn an_installer_migration_boots_under_the_runtime_rls_guard() {
    let db = SplitRoleDb::create("proxiima_spiice_kit", &[])
        .await
        .expect("PG required");
    write_and_read_a_note::<ExampleSpiice>(
        &db,
        "an installer-only owner-RLS migration passes the runtime RLS guard",
    )
    .await;

    assert_eq!(
        ledger_versions(&db, "public._sqlx_migrations_proxiima_example").await,
        Some(
            proxiima_example::migrator()
                .iter()
                .map(|migration| migration.version)
                .collect()
        ),
        "the spiice records every shipped migration on its own ledger"
    );
    assert_eq!(
        ledger_versions(&db, "public._sqlx_migrations").await,
        Some(Vec::new()),
        "and not on core's"
    );
    for ledger in [
        "public._sqlx_migrations_proxiima_example",
        "public._sqlx_migrations",
    ] {
        assert!(
            !runtime_may_write(&db, ledger).await,
            "the runtime role reads {ledger} and cannot change it"
        );
    }
}

/// A host schema built only from per-table calls, ending with the census,
/// boots under the same runtime RLS guard and enforces the same policies.
#[tokio::test]
async fn per_table_installer_calls_boot_under_the_runtime_rls_guard() {
    let db = SplitRoleDb::create("proxiima_spiice_kit_per_table", &[])
        .await
        .expect("PG required");
    write_and_read_a_note::<kit_per_table::KitPerTableSpiice>(
        &db,
        "a schema of per-table owner-RLS calls passes the runtime RLS guard",
    )
    .await;
}

/// A table added with no call carries none of the policies, and the runtime
/// RLS guard refuses the boot by name, whether or not a call was made for
/// its neighbours.
#[tokio::test]
async fn a_table_added_without_a_call_refuses_boot() {
    let db = SplitRoleDb::create("proxiima_spiice_kit_forgotten", &[])
        .await
        .expect("PG required");
    migrated_app::migrate_app::<kit_forgotten_table::KitForgottenTableSpiice>(db.platform_url())
        .await;
    let refused = Proxiima::<kit_forgotten_table::KitForgottenTableSpiice>::app()
        .configure(|b| {
            b.database_url(db.runtime_url())
                .store_number(0)
                .platform_database_url(db.platform_url())
                .owner(company_owner(Uuid::now_v7()))
                .allow_insecure_single_owner()
                .served_surface(ServedSurface::All)
        })
        .build()
        .await
        .expect_err("a table without the policies must refuse the boot");
    let message = refused.to_string();
    assert!(message.contains("proxiima_example.settings"), "{message}");
    assert!(
        message.contains("ENABLE and FORCE ROW LEVEL SECURITY"),
        "{message}"
    );
}

/// The census a migration ends with raises at migration time, naming the
/// table.
#[tokio::test]
async fn the_census_refuses_a_migration_that_forgot_a_table() {
    let db = SplitRoleDb::create("proxiima_spiice_kit_census", &[])
        .await
        .expect("PG required");
    let refused =
        migrated_app::migrate::<kit_forgotten_table_census::KitForgottenTableCensusSpiice>(
            db.platform_url(),
        )
        .await
        .expect_err("the census must refuse a table without the policies");
    let message = refused.to_string();
    assert!(
        message.contains("table proxiima_example.settings lacks ENABLE/FORCE RLS"),
        "{message}"
    );
}

#[tokio::test]
async fn an_unclassified_table_refuses_the_migration() {
    let db = SplitRoleDb::create("proxiima_spiice_kit_gap", &[])
        .await
        .expect("PG required");
    let refused = migrated_app::migrate::<kit_gap::KitGapSpiice>(db.platform_url())
        .await
        .expect_err("an unclassified table must refuse the migration");
    let message = refused.to_string();
    assert!(
        message.contains("owner RLS classification missing for proxiima_example.settings"),
        "the refusal names the table: {message}"
    );
}

#[tokio::test]
async fn a_spiice_definer_view_refuses_boot_and_names_the_view() {
    let db = SplitRoleDb::create("proxiima_spiice_definer_view", &[])
        .await
        .expect("PG required");
    migrated_app::migrate_app::<kit_definer_view::KitDefinerViewSpiice>(db.platform_url()).await;
    let refused = Proxiima::<kit_definer_view::KitDefinerViewSpiice>::app()
        .configure(|b| {
            b.database_url(db.runtime_url())
                .store_number(0)
                .platform_database_url(db.platform_url())
                .owner(company_owner(Uuid::now_v7()))
                .allow_insecure_single_owner()
                .served_surface(ServedSurface::All)
        })
        .build()
        .await
        .expect_err("an ordinary definer view must refuse split-role boot");
    let message = refused.to_string();
    assert!(
        message.contains("proxiima_example.notebook_view"),
        "{message}"
    );
    assert!(message.contains("security_invoker=true"), "{message}");
}

#[tokio::test]
async fn a_spiice_invoker_view_boots_and_hides_other_owners() {
    let db = SplitRoleDb::create("proxiima_spiice_invoker_view", &[])
        .await
        .expect("PG required");
    migrated_app::migrate_app::<kit_invoker_view::KitInvokerViewSpiice>(db.platform_url()).await;
    let owner = company_owner(Uuid::now_v7());
    let other = company_owner(Uuid::now_v7());
    let built = Proxiima::<kit_invoker_view::KitInvokerViewSpiice>::app()
        .configure(|b| {
            b.database_url(db.runtime_url())
                .store_number(0)
                .platform_database_url(db.platform_url())
                .owner(owner)
                .allow_insecure_single_owner()
                .served_surface(ServedSurface::All)
        })
        .build()
        .await
        .expect("a security_invoker view passes split-role boot");
    let admin = sqlx::PgPool::connect(&db.admin_url()).await.unwrap();
    for (row_owner, name) in [(owner, "mine"), (other, "theirs")] {
        sqlx::query(
            "INSERT INTO proxiima_example.notebooks(notebook_id, owner_id, name) VALUES ($1, $2, $3)",
        )
        .bind(Uuid::now_v7())
        .bind(row_owner.into_inner())
        .bind(name)
            .execute(&admin)
            .await
            .unwrap();
    }
    let runtime = sqlx::PgPool::connect(db.runtime_url()).await.unwrap();
    let authz = scoped_authz(owner);
    let mut transaction = proxiima_storage_pg::begin_owner_transaction(
        &runtime,
        authz.owner_scope().expect("authenticated owner scope"),
    )
    .await
    .unwrap();
    let visible: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT owner_id, name FROM proxiima_example.notebook_view")
            .fetch_all(&mut *transaction)
            .await
            .unwrap();
    assert_eq!(visible, vec![(owner.into_inner(), "mine".to_owned())]);
    transaction.rollback().await.unwrap();
    runtime.close().await;
    admin.close().await;
    built.shutdown().await;
}

#[tokio::test]
async fn a_preexisting_invoker_view_moves_to_the_split_platform_owner() {
    let name = proxiima::testkit::unique_db_name("proxiima_spiice_legacy_view");
    proxiima::testkit::create_db(&name).await.unwrap();
    let database = proxiima::testkit::DbGuard::adopt(name);
    let admin = sqlx::PgPool::connect(&proxiima::testkit::db_url(database.name()))
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE SCHEMA proxiima_example;
         CREATE TABLE proxiima_example.legacy_table(owner_id uuid);
         CREATE VIEW proxiima_example.legacy_view WITH (security_invoker = true)
            AS SELECT owner_id FROM proxiima_example.legacy_table",
    )
    .execute(&admin)
    .await
    .unwrap();
    proxiima::testkit::split_role_urls_for(database.name(), &["proxiima_example"])
        .await
        .expect("the fixture transfers existing views along with their tables");
    let view_owner: String = sqlx::query_scalar(
        "SELECT r.rolname FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         JOIN pg_roles r ON r.oid = c.relowner
         WHERE n.nspname = 'proxiima_example' AND c.relname = 'legacy_view'",
    )
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(view_owner, "proxiima_test_platform");
    admin.close().await;
}
