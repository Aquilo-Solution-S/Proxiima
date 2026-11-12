//! A spiice's migration source: its migrator, named, on its own ledger.

use proxiima_storage_pg::is_core_ledger;
use sqlx::migrate::Migrator;

/// One named `SQLx` migration source in a composite Proxiima binary.
#[derive(Debug)]
pub struct NamedMigrator {
    source: &'static str,
    migrator: Migrator,
}

impl NamedMigrator {
    /// Build a named migrator. Use the spiice id or host app id as
    /// `source`, e.g. `fixture-spiice`.
    ///
    /// The migrator keeps whatever tracking table it declares; a spiice
    /// that declares none records into core's `public._sqlx_migrations`.
    /// Prefer [`Self::spiice`].
    #[must_use]
    pub fn new(source: &'static str, migrator: Migrator) -> Self {
        Self { source, migrator }
    }

    /// A spiice's migrator on its own ledger: [`spiice_ledger_table`]`(id)`,
    /// unless the migrator already declares a table of its own, which it
    /// keeps (a ledger is never renamed under a live database).
    ///
    /// A spiice ledger lives in `public` because a destructive spiice
    /// baseline drops the spiice schema and the ledger must survive it. A
    /// database whose rows for this spiice sit in a different ledger must
    /// be reset before changing its tracking table.
    ///
    /// # Panics
    ///
    /// When `id` is not a spiice id (see [`spiice_ledger_table`]); ids are
    /// compile-time constants, so this is a programming error.
    #[must_use]
    pub fn spiice(id: &'static str, mut migrator: Migrator) -> Self {
        let derived = spiice_ledger_table(id);
        if is_core_ledger(&migrator.table_name) {
            migrator.dangerous_set_table_name(derived);
        }
        Self {
            source: id,
            migrator,
        }
    }

    /// Source id used in reports and errors.
    #[must_use]
    pub fn source(&self) -> &'static str {
        self.source
    }

    /// Borrow the underlying `SQLx` migrator.
    #[must_use]
    pub fn migrator(&self) -> &Migrator {
        &self.migrator
    }

    /// The migrator, by value, for the host's migration runner, which
    /// prepares each source before it runs it. Host wiring.
    #[must_use]
    pub fn into_migrator(self) -> Migrator {
        self.migrator
    }
}

/// The tracking table [`NamedMigrator::spiice`] gives spiice `id`:
/// `public._sqlx_migrations_<id>`, `-` spelled `_`
/// (`fixture-spiice` → `public._sqlx_migrations_fixture_spiice`).
///
/// # Panics
///
/// When `id` is empty, longer than 40 bytes, or not lowercase ASCII
/// letters, digits, `-` and `_` starting with a letter. The derived name is
/// interpolated into DDL by `SQLx` and ledger preparation, so only a name
/// that needs no quoting is accepted, and the bound keeps it within
/// the 63-byte `PostgreSQL` identifier limit.
#[must_use]
pub fn spiice_ledger_table(id: &str) -> String {
    assert!(
        is_spiice_ledger_id(id),
        "spiice id {id:?} must be 1-40 bytes of [a-z0-9_-] starting with a letter"
    );
    format!("public._sqlx_migrations_{}", id.replace('-', "_"))
}

/// Whether [`spiice_ledger_table`] accepts `id`. `const`, so
/// `spiice_bundle!` checks its `name` at compile time.
#[must_use]
pub const fn is_spiice_ledger_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 40 || !bytes[0].is_ascii_lowercase() {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_') {
            return false;
        }
        index += 1;
    }
    true
}
