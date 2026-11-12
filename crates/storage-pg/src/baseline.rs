//! The core baseline gate: what a store's core ledger says about it.
//!
//! The core ledger is outside data. [`classify`] reads it once into a
//! [`CoreLedger`]: a blank database installs the baseline, a store that
//! records the baseline continues from it, and a store whose ledger is exactly
//! the previous head is adopted into the baseline by [`adopt`] inside the
//! migration transaction. Every other ledger is refused with a typed reset
//! requirement that names the release to pass through.

use std::collections::BTreeSet;

use proxiima_core::StorageError;
use sqlx::PgConnection;
use sqlx::migrate::{Migration, Migrator};

use crate::VectorStorage;
use crate::error::internal;

/// The release whose head this release adopts; an older store migrates with it
/// first.
pub(crate) const PASS_THROUGH_RELEASE: &str = "v0.0.73";

/// The core ledger of the previous head, `(version, checksum)` as stores in the
/// field record them: `SHA-384` of each migration file of that head.
const PREVIOUS_HEAD: [(i64, &str); 12] = [
    (
        57,
        "001652ac5fe82d3f20e2680087b325d20315ee5216b05fca4d90a9d697da85ec8d2eabac0ab7f74bc82b5d2998d87a84",
    ),
    (
        58,
        "0305d41c669cd783b605e93e5f53a62daa5561629019c6f71088796f428892ffa6566b4f6cc8eb686521ca2822609fa1",
    ),
    (
        59,
        "c5012289528964c94a5e93cdfb72c083106ca2c07370c1ea1a117842038aa8f799452c47169785aa5fa1e075c523c640",
    ),
    (
        60,
        "6d978d7ae17e0a53c15faaa4e4357dd28582c3b95488b722338ac77a0271298a0c0b7c0ba6cce57e3f05d3a32bddff15",
    ),
    (
        61,
        "d5838d3266e20d5ffd6d952709b994defbc087b5aa13b63b0f38d87a25523b6c44d676ca6583bfe85abda4a3bfaf910b",
    ),
    (
        62,
        "ddd0ab23a73691dce602974cca93d5d782a81f10c0712a58b4790be8f3d29349ac4f29d138aaa173854e70d42dda2dac",
    ),
    (
        63,
        "dbea03093be296c291a8b14bbe3c1a78a026284d527acbe44bd1e13d3c16de965a1c8a8dabe5db5a67d2add5142042dd",
    ),
    (
        64,
        "dbaa3169f9eff2487878225e0459e49317724b7b39a2e7b42dd1235e167dc63522a0b7d0f03a140f9cca419fbdbc7863",
    ),
    (
        65,
        "5bd087f55ad773c29088999c007f9eb94e4712aea432ecf520cfe0123a0f9161c574ab5507ff82ba5444c533e3631e6f",
    ),
    (
        66,
        "64d45b5f44679a3d7f0de5a90619912d2e27e89c54070b8544dccb54ae80f3ead0413842995e7c59e89051245d13714c",
    ),
    (
        67,
        "e1f7a813cb3e4e7bd3c1be39c162778e5fba63c6a499c2b15aaf7cdb2b8c39663cf457731726813468194f1c7601e6b9",
    ),
    (
        68,
        "c55748bb24f07f592cf73e7b94d8944dd716a5eda42a4d3edb366c4c9260c34f7096a1b714cb8a756fc0074692227e4c",
    ),
];

/// The schema prefix of a store installed before any ledger this release
/// reads: a database that holds such a schema is not blank, whatever its
/// ledger says.
pub(crate) const EARLIER_SCHEMA_PREFIX: &str = "proxima_";

/// The previous head's pgvector migration, which a store whose vectors live in
/// a host index never recorded.
const PREVIOUS_HEAD_PGVECTOR: i64 = 58;

/// The difference between the previous head and the baseline.
const ADOPT_SCHEMA_SQL: &str =
    "ALTER TABLE proxiima_core.delegated_authority_grants DROP COLUMN auth_epoch";

/// One row of the core ledger.
#[derive(Debug, Clone)]
pub(crate) struct LedgerRow {
    pub(crate) version: i64,
    pub(crate) checksum: Vec<u8>,
    pub(crate) success: bool,
}

/// What a store's core ledger says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoreLedger {
    /// No core row and no core schema: install the baseline.
    Blank,
    /// The baseline is recorded, with these core versions.
    Current { recorded: BTreeSet<i64> },
    /// Exactly the previous head: adopt it into the baseline.
    PreviousHead(PreviousHead),
}

/// A store at the previous head, with or without its pgvector migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviousHead {
    WithPgvector,
    WithoutPgvector,
}

impl CoreLedger {
    /// The core versions recorded once this ledger is adopted, if it needs it.
    pub(crate) fn recorded(&self, migrator: &Migrator) -> BTreeSet<i64> {
        match self {
            Self::Blank => BTreeSet::new(),
            Self::Current { recorded } => recorded.clone(),
            Self::PreviousHead(head) => head
                .adopted_rows(migrator)
                .into_iter()
                .map(|migration| migration.version)
                .collect(),
        }
    }
}

impl PreviousHead {
    /// The embedded migrations whose rows replace the previous head's rows:
    /// the baseline, and the pgvector migration exactly when the previous
    /// head recorded its own.
    fn adopted_rows(self, migrator: &Migrator) -> Vec<&Migration> {
        let baseline = migrator.iter().next().expect("embedded core baseline");
        let pgvector = migrator
            .iter()
            .find(|migration| !VectorStorage::HostIndex.applies(migration))
            .expect("embedded core pgvector migration");
        match self {
            Self::WithPgvector => vec![baseline, pgvector],
            Self::WithoutPgvector => vec![baseline],
        }
    }
}

/// The previous head's ledger rows a store of this kind recorded, as
/// `(version, checksum)`.
#[cfg(any(test, feature = "testkit"))]
pub(crate) fn previous_head_rows(vectors: VectorStorage) -> Vec<(i64, Vec<u8>)> {
    PREVIOUS_HEAD
        .iter()
        .filter(|(version, _)| {
            vectors == VectorStorage::Local || *version != PREVIOUS_HEAD_PGVECTOR
        })
        .map(|(version, checksum)| (*version, decode_hex(checksum)))
        .collect()
}

#[cfg(any(test, feature = "testkit"))]
fn decode_hex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex checksum"))
        .collect()
}

fn reset_required() -> StorageError {
    StorageError::SchemaResetRequired {
        details: format!(
            "this release adopts a store at the head of release {PASS_THROUGH_RELEASE} and installs a blank database; migrate this store with {PASS_THROUGH_RELEASE} first, then with this release"
        ),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn previous_head(rows: &[LedgerRow]) -> Option<PreviousHead> {
    let pgvector = rows.iter().any(|row| row.version == PREVIOUS_HEAD_PGVECTOR);
    let expected = PREVIOUS_HEAD
        .iter()
        .filter(|(version, _)| pgvector || *version != PREVIOUS_HEAD_PGVECTOR);
    let exact = rows.len() == expected.clone().count()
        && rows.iter().zip(expected).all(|(row, (version, checksum))| {
            row.success && row.version == *version && hex(&row.checksum) == *checksum
        });
    exact.then_some(if pgvector {
        PreviousHead::WithPgvector
    } else {
        PreviousHead::WithoutPgvector
    })
}

/// Classify the core ledger `rows`, ordered by version.
///
/// `has_schema` says whether a core schema exists; `baseline_version` is the
/// first embedded core migration. Rows at or above the baseline have been
/// checked against the embedded files by the caller.
///
/// # Errors
///
/// [`StorageError::SchemaResetRequired`] for any ledger that is not blank,
/// does not record the baseline alone above every earlier version, and is not
/// exactly the previous head; and for a schema no ledger accounts for.
pub(crate) fn classify(
    rows: &[LedgerRow],
    has_schema: bool,
    baseline_version: i64,
) -> Result<CoreLedger, StorageError> {
    if rows.is_empty() {
        return if has_schema {
            Err(reset_required())
        } else {
            Ok(CoreLedger::Blank)
        };
    }
    if rows.iter().any(|row| row.version == baseline_version) {
        if rows.iter().any(|row| row.version < baseline_version) {
            return Err(reset_required());
        }
        return Ok(CoreLedger::Current {
            recorded: rows.iter().map(|row| row.version).collect(),
        });
    }
    previous_head(rows)
        .map(CoreLedger::PreviousHead)
        .ok_or_else(reset_required)
}

/// Adopt a store at the previous head into the baseline, inside the caller's
/// migration transaction and before any migration runs: apply the difference
/// between the previous head and the baseline, then replace the previous
/// head's ledger rows by the embedded baseline's row, and by the pgvector
/// migration's row exactly when the previous head recorded its own.
///
/// # Errors
///
/// Propagates database errors.
pub(crate) async fn adopt(
    connection: &mut PgConnection,
    head: PreviousHead,
    migrator: &Migrator,
) -> Result<(), StorageError> {
    sqlx::query(ADOPT_SCHEMA_SQL)
        .execute(&mut *connection)
        .await
        .map_err(internal)?;
    let versions: Vec<i64> = PREVIOUS_HEAD.iter().map(|(version, _)| *version).collect();
    sqlx::query("DELETE FROM public._sqlx_migrations WHERE version = ANY($1)")
        .bind(versions)
        .execute(&mut *connection)
        .await
        .map_err(internal)?;
    for migration in head.adopted_rows(migrator) {
        sqlx::query(
            "INSERT INTO public._sqlx_migrations
                 (version, description, success, checksum, execution_time)
             VALUES ($1, $2, true, $3, 0)",
        )
        .bind(migration.version)
        .bind(migration.description.as_ref())
        .bind(migration.checksum.as_ref())
        .execute(&mut *connection)
        .await
        .map_err(internal)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proxiima_core::StorageError;

    use super::{
        CoreLedger, LedgerRow, PASS_THROUGH_RELEASE, PreviousHead, classify, previous_head_rows,
    };
    use crate::VectorStorage;

    const BASELINE: i64 = 69;

    /// A recorded row as `SQLx` writes it: successful, with the given checksum.
    fn row(version: i64, checksum: Vec<u8>) -> LedgerRow {
        LedgerRow {
            version,
            checksum,
            success: true,
        }
    }

    fn previous_head(with_pgvector: bool) -> Vec<LedgerRow> {
        let vectors = if with_pgvector {
            VectorStorage::Local
        } else {
            VectorStorage::HostIndex
        };
        previous_head_rows(vectors)
            .into_iter()
            .map(|(version, checksum)| row(version, checksum))
            .collect()
    }

    fn ledger(versions: impl IntoIterator<Item = i64>) -> Vec<LedgerRow> {
        versions
            .into_iter()
            .map(|version| row(version, vec![0; 48]))
            .collect()
    }

    fn is_reset(result: &Result<CoreLedger, StorageError>) -> bool {
        matches!(
            result,
            Err(StorageError::SchemaResetRequired { details })
                if details.contains(PASS_THROUGH_RELEASE)
        )
    }

    #[test]
    fn a_blank_database_installs_the_baseline() {
        assert_eq!(classify(&[], false, BASELINE).unwrap(), CoreLedger::Blank);
    }

    #[test]
    fn a_baseline_ledger_is_current() {
        assert_eq!(
            classify(&ledger([69, 70]), true, BASELINE).unwrap(),
            CoreLedger::Current {
                recorded: BTreeSet::from([69, 70])
            }
        );
    }

    #[test]
    fn the_previous_head_is_adopted_with_and_without_its_pgvector_migration() {
        assert_eq!(
            classify(&previous_head(true), true, BASELINE).unwrap(),
            CoreLedger::PreviousHead(PreviousHead::WithPgvector)
        );
        assert_eq!(
            classify(&previous_head(false), true, BASELINE).unwrap(),
            CoreLedger::PreviousHead(PreviousHead::WithoutPgvector)
        );
    }

    #[test]
    fn a_previous_head_one_version_short_is_refused() {
        let mut rows = previous_head(true);
        rows.pop();
        assert!(is_reset(&classify(&rows, true, BASELINE)));
    }

    #[test]
    fn a_previous_head_with_an_extra_version_below_the_baseline_is_refused() {
        let mut rows = previous_head(true);
        rows.push(row(BASELINE - 1, vec![0; 48]));
        assert!(is_reset(&classify(&rows, true, BASELINE)));
    }

    #[test]
    fn a_previous_head_with_a_changed_checksum_is_refused() {
        let mut rows = previous_head(true);
        rows[3].checksum[0] ^= 1;
        assert!(is_reset(&classify(&rows, true, BASELINE)));
    }

    #[test]
    fn a_previous_head_with_an_unsuccessful_row_is_refused() {
        let mut rows = previous_head(false);
        rows[0].success = false;
        assert!(is_reset(&classify(&rows, true, BASELINE)));
    }

    #[test]
    fn a_schema_without_a_ledger_is_refused() {
        assert!(is_reset(&classify(&[], true, BASELINE)));
    }

    #[test]
    fn an_older_ledger_is_refused() {
        assert!(is_reset(&classify(&ledger(33..=57), true, BASELINE)));
    }

    #[test]
    fn earlier_versions_beside_the_baseline_are_refused() {
        let mut rows = previous_head(true);
        rows.push(row(BASELINE, vec![0; 48]));
        assert!(is_reset(&classify(&rows, true, BASELINE)));
    }
}
