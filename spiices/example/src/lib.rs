//! `proxiima-example`: the small spiice the facade tests, dev-migrate and
//! `docs/build-a-spiice.md` build against. It is a test and tutorial
//! fixture (`publish = false`), not a product.
//!
//! Stateful note Fact ([`payload::NoteV1`]), digest Abstraction
//! ([`payload::DigestV1`]), stamp Fact that follows transfer
//! ([`payload::StampV1`]), a sealed stamp Fact that never transfers
//! ([`payload::SealedStampV1`]), a notebook scope registry, an ownerless settings
//! table, one read-only tool and one migration lane with its own ledger.
//!
//! The library depends on `proxiima-spiice` alone. The `host` feature links
//! `proxiima` to serve the example as an app; `testkit` builds on it.

pub mod contract;
mod heads;
pub mod payload;
pub mod tool;

#[cfg(feature = "testkit")]
pub mod testkit;

pub use contract::{EXAMPLE_BASELINE_CONTRACT, EXAMPLE_CONTRACT, PG_SCHEMA, SPIICE_ID};
pub use heads::note_heads;
pub use payload::{DigestV1, NoteV1, SealedStampV1, StampV1};
pub use tool::ListNotebooksTool;

/// The spiice's embedded migrations.
#[must_use]
pub fn migrator() -> sqlx::migrate::Migrator {
    sqlx::migrate!("./migrations")
}

/// The tutorial baseline: the lane's baseline without the stamp sidecar, for
/// tutorial variants that compose [`EXAMPLE_BASELINE_CONTRACT`]. The lane's
/// baseline begins with this text.
pub const BASELINE_SQL: &str = include_str!("../fixtures/20261018000011_baseline.sql");

const OWNER_RLS_BEGIN: &str = "-- owner-rls: begin\n";
const OWNER_RLS_END: &str = "-- owner-rls: end\n";

/// The baseline split around its owner-RLS section: before, the section
/// (a per-table call for each table that is not a sidecar, then the census),
/// after. The sidecars are classified by `install_sidecar` before it.
fn owner_rls_split() -> (&'static str, &'static str, &'static str) {
    let (before, rest) = BASELINE_SQL
        .split_once(OWNER_RLS_BEGIN)
        .expect("the baseline marks the start of its owner-RLS section");
    let (section, after) = rest
        .split_once(OWNER_RLS_END)
        .expect("the baseline marks the end of its owner-RLS section");
    (before, section, after)
}

/// The statements of the baseline's owner-RLS section, in order: the
/// per-table calls, then the census. The tutorial's variants replace them.
#[must_use]
pub fn installer_calls() -> Vec<&'static str> {
    owner_rls_split()
        .1
        .split(";\n")
        .filter(|statement| !statement.trim().is_empty())
        .collect()
}

/// The baseline with its owner-RLS section replaced by `calls`, in place,
/// for the tutorial's variants.
#[must_use]
pub fn baseline_sql_with_owner_rls(calls: &[&'static str]) -> proxiima_spiice::SqlText {
    let (before, _, after) = owner_rls_split();
    let mut sql = proxiima_spiice::SqlText::new(before);
    for call in calls {
        sql.push(call).push(";\n");
    }
    sql.push(after);
    sql
}

/// The baseline with no owner-RLS section at all.
#[must_use]
pub fn baseline_sql_without_installer() -> proxiima_spiice::SqlText {
    baseline_sql_with_owner_rls(&[])
}

proxiima_spiice::spiice_bundle! {
    bundle = ExampleSpiice,
    name = "proxiima-example",
    display_name = "Example",
    fact_schemas = [payload::NoteV1, payload::StampV1, payload::SealedStampV1],
    abstraction_schemas = [payload::DigestV1],
    mcp_tools = [tool::ListNotebooksTool],
    contract = &contract::EXAMPLE_CONTRACT,
    migrations = migrator(),
}

#[cfg(feature = "host")]
proxiima::spiice_app! {
    bundle = ExampleSpiice,
    id = "proxiima-example",
    title = "Proxiima example spiice (test fixture)",
}
