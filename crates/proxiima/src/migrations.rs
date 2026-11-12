//! Shared migration facade for embedded Proxiima hosts.
//!
//! Core records into `SQLx`'s default `public._sqlx_migrations`; each
//! spiice records into its own tracking table
//! (`public._sqlx_migrations_<spiice>` — in `public`, because destructive
//! spiice baselines drop the spiice schema and the ledger must survive
//! them). The facade pins the migration `search_path` to
//! `public`: core runs first, spiices run in composition order, and
//! duplicate versions fail before the database is touched.
//!
//! Every ledger is validated against the union of sources that record on
//! it before any DDL. Unknown versions fail unless the host explicitly uses
//! [`MissingMigrationPolicy::Allow`]. [`LedgerConflict`] also refuses a lane
//! that would repeat or reorder a different release's work.
//!
//! [`pending_migrations`] lists pending embedded SQL, core adoption and core definitions.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use proxiima_core::{PgIdent, SqlText};
use proxiima_core::{SchemaEvolution, StorageError};
use proxiima_spiice::NamedMigrator;
pub use proxiima_storage_pg::{MissingMigrationPolicy, VectorStorage};
use proxiima_storage_pg::{
    PgMigrationStorage, PgSidecarRegistryFrozen, PgStorage, core_migrator_for,
    ensure_core_ledger_compatible_on_connection_with_policy, is_core_ledger, parse_ledger_name,
    parse_name_parts,
};
use sqlx::migrate::{MigrateError, Migration};
use sqlx::pool::PoolConnection;
use sqlx::{Connection, PgConnection, PgPool, Postgres, Transaction};

const CORE_SOURCE: &str = "proxiima-core";

/// Migration lanes need only their pool; serving storage additionally carries
/// the validated platform authority. Keeping this boundary typed prevents a
/// migration connection from being used as a runtime store.
pub trait MigrationStorage {
    fn clone_migration_pool(&self) -> PgPool;

    /// Where the store keeps its vectors, which decides the core migrations
    /// and definitions it takes.
    fn vector_storage(&self) -> VectorStorage;
}

impl MigrationStorage for PgStorage {
    fn clone_migration_pool(&self) -> PgPool {
        self.clone_pool_for_backend()
    }

    fn vector_storage(&self) -> VectorStorage {
        PgStorage::vector_storage(self)
    }
}

impl MigrationStorage for PgMigrationStorage {
    fn clone_migration_pool(&self) -> PgPool {
        self.clone_pool_for_backend()
    }

    fn vector_storage(&self) -> VectorStorage {
        PgMigrationStorage::vector_storage(self)
    }
}

/// Validate and topologically order the evolution steps for one composed host.
/// Steps in one spiice lane are implicitly ordered by their migration anchor;
/// explicit dependencies and cross-schema producers add edges on top. SQL
/// execution uses `after_migration` as the owning lane's anchor; this helper
/// proves every anchor and dependency before the first write.
pub(crate) fn schema_evolution_schedule<'a>(
    sources: &[&NamedMigrator],
    evolutions: &'a [SchemaEvolution],
) -> Result<Vec<&'a SchemaEvolution>, MigrationError> {
    let mut by_id = BTreeMap::new();
    for evolution in evolutions {
        by_id.insert(evolution.id, evolution);
        let owner = evolution.sources.first().map(|source| source.id.spiice);
        if !sources.iter().any(|source| {
            Some(source.source()) == owner && source.has_version(evolution.after_migration)
        }) {
            return Err(MigrationError::EvolutionAnchorMissing {
                evolution_id: evolution.id,
                after_migration: evolution.after_migration,
            });
        }
        for required in evolution.requires {
            if !by_id.contains_key(required)
                && !evolutions.iter().any(|candidate| candidate.id == *required)
            {
                return Err(MigrationError::EvolutionDependencyMissing {
                    evolution_id: evolution.id,
                    required,
                });
            }
        }
    }
    let mut dependencies = BTreeMap::<&'static str, BTreeSet<&'static str>>::new();
    for evolution in evolutions {
        let entry = dependencies.entry(evolution.id).or_default();
        entry.extend(evolution.requires.iter().copied());
        for prior in evolutions {
            if prior.id != evolution.id
                && prior.targets.iter().any(|target| {
                    evolution
                        .sources
                        .iter()
                        .any(|source| source.id == target.id && source.kind == target.kind)
                })
            {
                entry.insert(prior.id);
            }
            let same_lane = prior
                .sources
                .first()
                .zip(evolution.sources.first())
                .is_some_and(|(prior_source, current_source)| {
                    prior_source.id.spiice == current_source.id.spiice
                });
            if same_lane && prior.after_migration < evolution.after_migration {
                entry.insert(prior.id);
            }
        }
    }
    let mut result = Vec::with_capacity(evolutions.len());
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for evolution in evolutions {
        visit_evolution(
            evolution,
            &by_id,
            &dependencies,
            &mut visiting,
            &mut visited,
            &mut result,
        )?;
    }
    Ok(result)
}

fn visit_evolution<'a>(
    evolution: &'a SchemaEvolution,
    by_id: &BTreeMap<&'static str, &'a SchemaEvolution>,
    dependencies: &BTreeMap<&'static str, BTreeSet<&'static str>>,
    visiting: &mut BTreeSet<&'static str>,
    visited: &mut BTreeSet<&'static str>,
    result: &mut Vec<&'a SchemaEvolution>,
) -> Result<(), MigrationError> {
    if visited.contains(evolution.id) {
        return Ok(());
    }
    if !visiting.insert(evolution.id) {
        return Err(MigrationError::EvolutionCycle {
            evolution_id: evolution.id,
        });
    }
    for required in dependencies.get(evolution.id).into_iter().flatten() {
        let dependency = by_id
            .get(required)
            .ok_or(MigrationError::EvolutionDependencyMissing {
                evolution_id: evolution.id,
                required,
            })?;
        visit_evolution(dependency, by_id, dependencies, visiting, visited, result)?;
    }
    visiting.remove(evolution.id);
    visited.insert(evolution.id);
    result.push(evolution);
    Ok(())
}

/// Successful migration run metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationRunReport {
    pub sources: Vec<&'static str>,
}

/// Errors raised by the framework migration facade.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error(
        "duplicate migration version {version}: {first_source} ({first_description}) and {second_source} ({second_description})"
    )]
    DuplicateVersion {
        version: i64,
        first_source: &'static str,
        first_description: String,
        second_source: &'static str,
        second_description: String,
    },
    #[error("failed to acquire migration connection: {0}")]
    Connection(#[source] sqlx::Error),
    #[error("failed to pin migration search_path to public: {0}")]
    PinSearchPath(#[source] sqlx::Error),
    #[error("failed to reset migration search_path after migrations: {0}")]
    ResetSearchPath(#[source] sqlx::Error),
    #[error("core migration preflight failed: {0}")]
    CorePreflight(#[source] StorageError),
    #[error("sidecar owner RLS deployment failed: {0}")]
    SidecarOwnerRls(#[source] StorageError),
    #[error("schema evolution failed: {0}")]
    SchemaEvolution(#[source] StorageError),
    #[error("composed search definitions do not match this binary: {0}")]
    SearchDefinitions(#[source] StorageError),
    #[error("core migrations failed: {0}")]
    Core(#[source] MigrateError),
    #[error("spiice migrations failed for {source}: {err}")]
    Spiice {
        source: &'static str,
        #[source]
        err: MigrateError,
    },
    #[error(
        "two spiices record on one ledger {table}: {first_source} and {second_source}; give each its own (spiice ids that differ only in `-`/`_` derive the same name)"
    )]
    DuplicateLedger {
        table: String,
        first_source: &'static str,
        second_source: &'static str,
    },
    #[error("migration ledger preparation failed for {source}: {err}")]
    SpiiceLedgerPreparation {
        source: &'static str,
        #[source]
        err: sqlx::Error,
    },
    #[error(
        "schema evolution {evolution_id:?} has no migration source for its anchor {after_migration}"
    )]
    EvolutionAnchorMissing {
        evolution_id: &'static str,
        after_migration: i64,
    },
    #[error("schema evolution dependency graph contains a cycle at {evolution_id:?}")]
    EvolutionCycle { evolution_id: &'static str },
    #[error("schema evolution {evolution_id:?} requires missing step {required:?}")]
    EvolutionDependencyMissing {
        evolution_id: &'static str,
        required: &'static str,
    },
    #[error("failed to read migration ledger {ledger} for {source}: {err}")]
    LedgerRead {
        source: &'static str,
        ledger: String,
        #[source]
        err: MigrateError,
    },
    #[error("{source} refuses migration ledger {ledger}: {conflict}")]
    Ledger {
        source: &'static str,
        ledger: String,
        #[source]
        conflict: LedgerConflict,
    },
}

/// Why a spiice's lane cannot run, or serve, against its ledger.
///
/// Every shipped version must be recorded on its composed source's ledger.
/// Missing-version policy never relaxes that assignment. A source with its
/// own ledger also receives lineage checks: even when missing versions are
/// allowed, work cannot repeat or reorder another release's migrations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LedgerConflict {
    /// A composed migration is recorded on a ledger other than the one its
    /// source currently uses. This includes legacy copies on both ledgers;
    /// those databases require a reset rather than a compatibility path.
    #[error(
        "migration {version} is recorded on {ledger}, but this source uses a different ledger. Reset the database to remove the conflicting ledger history, including legacy copies; allowing missing versions does not change ledger assignment"
    )]
    RecordedElsewhere { version: i64, ledger: String },
    /// The lane's first migration was never applied, yet the ledger records
    /// versions this binary does not ship: the database ran a different
    /// lane (a replaced baseline, in either direction), and applying this
    /// one would re-create what that one built.
    #[error(
        "its first migration {root} was never applied here, but the ledger records {unknown:?}, which this binary does not ship: the database ran a different lane (a replaced baseline). Retire the release that does not match, or reset the lane's schemas and ledger before this one first boots (docs/contributing.md#migrations, Ledger lineage)"
    )]
    Replaced { root: i64, unknown: Vec<i64> },
    /// A pending migration is older than a recorded one this binary does
    /// not ship: a later release squashed or renumbered the lane this
    /// binary still carries.
    #[error(
        "it would apply {pending:?} after {unknown:?}, which a release this binary does not ship already applied: a later release squashed or renumbered this lane. Retire this release rather than boot it against this database (docs/contributing.md#migrations, Ledger lineage)"
    )]
    Diverged {
        pending: Vec<i64>,
        unknown: Vec<i64>,
    },
    /// Serving validation found migrations that were never applied; it issues
    /// no DDL, so the lane would serve against an older schema.
    #[error("migrations {pending:?} are not applied, and serving validation issues no DDL")]
    Unapplied { pending: Vec<i64> },
}

/// Run core migrations followed by the provided spiice/host migrators.
///
/// `sidecars` is frozen against the same composition being migrated. The
/// runner derives Follows owner RLS from it before and after the spiice lanes,
/// under the migration lock; an empty SQL plan still reconciles that state.
/// The explicit run also installs generated spiice search definitions in its
/// final transaction. Serving startup validates those definitions instead.
///
/// # Errors
///
/// Returns `MigrationError::DuplicateVersion` before any database write
/// if two sources claim the same migration version. Returns `Connection`
/// or `PinSearchPath` if the pinned migration connection cannot be prepared.
/// Returns `Ledger`, before that source applies anything, if a spiice's
/// ledger records a lane its migrator does not continue ([`LedgerConflict`]).
/// Returns `Core` or `Spiice` with [`MigrateError::VersionMissing`] before
/// any DDL when no source on a ledger ships one of its recorded versions.
/// Returns `Core` or `Spiice` if `SQLx` fails while applying that source.
#[cfg(feature = "testkit")]
pub async fn run_core_and_spiice_migrations<P: MigrationStorage>(
    pg: &P,
    spiices: impl IntoIterator<Item = NamedMigrator>,
    sidecars: &PgSidecarRegistryFrozen,
) -> Result<MigrationRunReport, MigrationError> {
    run_core_and_spiice_migrations_with_policy(
        pg,
        spiices,
        sidecars,
        MissingMigrationPolicy::Reject,
    )
    .await
}

/// [`run_core_and_spiice_migrations`] with an explicit missing-version policy.
///
/// All ledgers are checked before any DDL while holding the migration lock
/// through the final source commit. Checksums and lane conflicts remain
/// mandatory even when missing versions are allowed.
///
/// # Errors
///
/// The errors of [`run_core_and_spiice_migrations`]. With
/// [`MissingMigrationPolicy::Reject`], an unknown recorded version returns
/// [`MigrateError::VersionMissing`] inside `MigrationError::Core` or `Spiice`.
pub async fn run_core_and_spiice_migrations_with_policy<P: MigrationStorage>(
    pg: &P,
    spiices: impl IntoIterator<Item = NamedMigrator>,
    sidecars: &PgSidecarRegistryFrozen,
    policy: MissingMigrationPolicy,
) -> Result<MigrationRunReport, MigrationError> {
    run_lineup_with_policy(pg, &lineup(spiices, pg.vector_storage()), sidecars, policy).await
}

/// [`run_core_and_spiice_migrations`] over a [`lineup`] the caller keeps: the
/// same refusals before any query, the same run. A test template builds from
/// the migrators it also fingerprints, without consuming them.
#[cfg(feature = "testkit")]
pub(crate) async fn run_lineup<P: MigrationStorage>(
    pg: &P,
    sources: &[NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
) -> Result<MigrationRunReport, MigrationError> {
    run_lineup_with_policy(pg, sources, sidecars, MissingMigrationPolicy::Reject).await
}

async fn run_lineup_with_policy<P: MigrationStorage>(
    pg: &P,
    sources: &[NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
    policy: MissingMigrationPolicy,
) -> Result<MigrationRunReport, MigrationError> {
    check_lineup(sources)?;
    run_lineup_with_pool(
        pg.clone_migration_pool(),
        sources,
        sidecars,
        policy,
        pg.vector_storage(),
    )
    .await
}

async fn run_lineup_with_pool(
    pool: PgPool,
    sources: &[NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
    policy: MissingMigrationPolicy,
    vectors: VectorStorage,
) -> Result<MigrationRunReport, MigrationError> {
    let report = MigrationRunReport::from_sources(sources);

    let mut conn = acquire_session_connection(&pool).await?;

    pin_migration_search_path(&mut conn)
        .await
        .map_err(MigrationError::PinSearchPath)?;

    let migration_result = run_locked_sources(&mut conn, sources, sidecars, policy, vectors).await;
    let reset_result = reset_migration_search_path(&mut conn).await;

    match (migration_result, reset_result) {
        (Ok(()), Ok(())) => Ok(report),
        (Err(err), Ok(())) => Err(err),
        (Ok(()), Err(err)) => Err(MigrationError::ResetSearchPath(err)),
        (Err(err), Err(reset_err)) => {
            tracing::warn!(
                error = %reset_err,
                "failed to reset migration search_path after migration error"
            );
            Err(err)
        }
    }
}

async fn run_locked_sources(
    conn: &mut PgConnection,
    sources: &[NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
    policy: MissingMigrationPolicy,
    vectors: VectorStorage,
) -> Result<(), MigrationError> {
    let lock = SessionLock::acquire(conn).await.map_err(execute_error)?;
    let result = async {
        validate_ledgers(
            lock.conn,
            &sources.iter().collect::<Vec<_>>(),
            policy,
            vectors,
        )
        .await?;
        run_sources_on_connection(lock.conn, sources, sidecars, vectors).await
    }
    .await;
    let released = lock.release().await;
    result?;
    released.map_err(execute_error)
}

/// A connection that is about to carry session state: the runner's disabled
/// `statement_timeout`, a plan's `lock_timeout` and session lock.
///
/// It is marked close-on-drop here, before the caller can await on it, so no
/// way out returns it to the pool: not an error, and not the caller dropping
/// the future while it waits for the migration lock. A pooled connection
/// would be granted that lock later and hold it, or carry the overrides into
/// request serving. Closed, its session ends and takes both with it.
async fn acquire_session_connection(
    pool: &PgPool,
) -> Result<PoolConnection<Postgres>, MigrationError> {
    let mut conn = pool.acquire().await.map_err(MigrationError::Connection)?;
    conn.close_on_drop();
    Ok(conn)
}

/// Run the pre-boot compatibility preflight **without applying any
/// migration DDL**.
///
/// Serving boot and maintenance run this (see docs/operate.md): the host's
/// migrate step ([`crate::Proxiima::migrate`]) applies migrations under a
/// DDL-capable role, and the long-running app then boots under a DML-only
/// role that cannot issue DDL.
/// This requires every embedded core migration and the current definitions
/// record, and rejects duplicate versions across composed sources, but never runs
/// `run_direct` / touches schema — so it succeeds against an already-migrated
/// database held by a narrow role.
///
/// Preflight waits for the runner's whole-run advisory lock, with a five-second
/// lock timeout. Complete the migrate step before starting the app; a boot
/// between source commits is refused.
///
/// # Errors
///
/// Returns `MigrationError::DuplicateVersion` if two sources claim the same
/// version, `MigrationError::CorePreflight` if the core schema is not current,
/// `MigrationError::Ledger` if a spiice's ledger
/// conflicts with its lane or lacks one of its migrations, or
/// `MigrationError::Connection` if the preflight pool cannot be reached.
pub async fn preflight_without_migrations<P: MigrationStorage>(
    pg: &P,
    spiices: impl IntoIterator<Item = NamedMigrator>,
    sidecars: &PgSidecarRegistryFrozen,
) -> Result<MigrationRunReport, MigrationError> {
    preflight_without_migrations_with_policy(pg, spiices, sidecars, MissingMigrationPolicy::Reject)
        .await
}

/// [`preflight_without_migrations`] with an explicit missing-version policy.
///
/// # Errors
///
/// The errors of [`preflight_without_migrations`]. Unknown versions return
/// [`MigrateError::VersionMissing`] unless the policy allows them.
pub async fn preflight_without_migrations_with_policy<P: MigrationStorage>(
    pg: &P,
    spiices: impl IntoIterator<Item = NamedMigrator>,
    sidecars: &PgSidecarRegistryFrozen,
    policy: MissingMigrationPolicy,
) -> Result<MigrationRunReport, MigrationError> {
    let vectors = pg.vector_storage();
    let sources = prepare_sources(spiices, vectors)?;
    let report = MigrationRunReport::from_sources(&sources);
    let pool = pg.clone_migration_pool();
    let mut conn = acquire_session_connection(&pool).await?;
    sqlx::query(MIGRATION_LOCK_TIMEOUT_SQL)
        .execute(&mut *conn)
        .await
        .map_err(execute_error)?;
    let mut lock = SessionLock::acquire(&mut conn)
        .await
        .map_err(execute_error)?;
    let mut snapshot = lock.begin_snapshot().await.map_err(execute_error)?;
    let result = async {
        validate_ledgers(
            &mut snapshot,
            &sources.iter().collect::<Vec<_>>(),
            policy,
            vectors,
        )
        .await?;
        let core_pending = proxiima_storage_pg::pending_core_schema(
            snapshot.as_mut(),
            MissingMigrationPolicy::Allow,
            vectors,
        )
        .await
        .map_err(MigrationError::CorePreflight)?;
        if !core_pending.is_empty() {
            return Err(MigrationError::CorePreflight(
                StorageError::MigrationRequired {
                    details: "core migrations or definitions are not current".into(),
                },
            ));
        }
        for source in sources
            .iter()
            .filter(|source| !is_core_ledger(&source.migrator().table_name))
        {
            let recorded =
                ledger_versions(&read_ledger(&mut snapshot, source, &source.ledger(), true).await?);
            let lane = source.lane_versions();
            if let Some(conflict) =
                ledger_conflict(&lane, &recorded).or_else(|| unapplied(&lane, &recorded))
            {
                return Err(source.ledger_error(conflict));
            }
        }
        sqlx::query("SELECT set_config('app.proxiima_scope', 'platform', true)")
            .execute(&mut *snapshot)
            .await
            .map_err(execute_error)?;
        sidecars
            .ensure_owner_rls(&mut snapshot)
            .await
            .map_err(MigrationError::SidecarOwnerRls)?;
        sidecars
            .ensure_search_definitions(&mut snapshot)
            .await
            .map_err(MigrationError::SearchDefinitions)?;
        sidecars
            .ensure_schema_evolutions(snapshot.as_mut())
            .await
            .map_err(MigrationError::SchemaEvolution)
    }
    .await;
    let ended = snapshot.rollback().await;
    let released = lock.release().await;
    result?;
    ended.map_err(execute_error)?;
    released.map_err(execute_error)?;
    Ok(report)
}

/// One pending embedded SQL or core-definitions operation.
///
/// Only [`pending_migrations`] constructs an operation read from the database.
///
/// ```compile_fail,E0639
/// let _planned = proxiima::host::PendingMigration::Sql {
///     ledger: "public._sqlx_migrations".to_owned(),
///     version: 33,
///     description: String::new(),
///     checksum: Vec::new(),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PendingMigration {
    /// Apply an embedded SQL migration and record its ledger row.
    #[non_exhaustive]
    Sql {
        ledger: String,
        version: i64,
        description: String,
        checksum: Vec<u8>,
    },
    /// Adopt a store whose core ledger is exactly the head of `release` into
    /// the core baseline, in place and with its data, before core's
    /// migrations run.
    #[non_exhaustive]
    AdoptPreviousHead { release: String },
    /// Replace the core functions, triggers and policies in place.
    #[non_exhaustive]
    CoreDefinitions { checksum: Vec<u8>, release: String },
    SchemaEvolution {
        spiice: &'static str,
        id: &'static str,
        from: Vec<String>,
        to: Vec<String>,
    },
}

/// Pending embedded SQL and core definitions, applying nothing.
///
/// `sources` is what `spiices` is to [`run_core_and_spiice_migrations`]; core
/// is always first. Core's adoption of a previous-head ledger, its migrations
/// and its definitions come first, then each source's migrations, with
/// pending schema evolutions placed where a run applies them. Inside a ledger
/// the run walks
/// the migrator (ascending by version for `sqlx::migrate!`). It follows the
/// runner's rules, from the runner's helpers:
///
/// - A spiice on its own ledger counts that ledger's rows, and an absent
///   ledger as empty. Nothing is created.
/// - A spiice still recording on core's ledger is read there.
/// - Down migrations are not listed.
///
/// Registry-derived Follows marks, policies and indexes are reconciled on
/// every run, including when this list is empty. Composed search definitions
/// are installed by an explicit migration run even when no versioned SQL is
/// pending. This list does not describe that registry-derived work. Use
/// [`preflight_without_migrations`] with the frozen sidecar registry to
/// validate that generated deployment state.
///
/// The plan runs under the migration lock a run holds through every source (the
/// `proxmigr` advisory key) and reads every ledger in one
/// `REPEATABLE READ, READ ONLY` transaction: a run in the middle of a source
/// has committed or not committed all of it when the plan reads. It needs
/// `SELECT` on the ledgers, which a run leaves to the runtime role, issues no
/// DDL and writes nothing. It is a snapshot, not a reservation: a run checks
/// everything again under the lock. The connection that waits for the lock is
/// closed, not pooled, if the caller drops the future.
///
/// # Errors
///
/// Returns `MigrationError::DuplicateVersion` or `DuplicateLedger` before any
/// query; a table is matched by the name Postgres resolves, not by its
/// spelling (`_sqlx_migrations_host` is `public."_sqlx_migrations_host"`), and
/// a spelling outside Postgres' identifier rules is compared as written. Returns `Connection` if no connection can be acquired.
/// Returns `CorePreflight` if core's ledger does not reconcile with this
/// binary. Returns `Ledger` if a spiice's ledger records a lane its migrator
/// does not continue ([`LedgerConflict`]). Returns `Core` or `Spiice` with
/// `MigrateError::Dirty` for a `success = false` row and
/// `MigrateError::VersionMismatch` for a changed checksum, as a run would, and
/// with `MigrateError::Execute` if the lock is not granted within the
/// runner's `lock_timeout` or a source holds a `no_tx` migration. Returns
/// `LedgerRead` if a ledger cannot be read. The answer is the whole list or
/// an error, never a partial list.
pub async fn pending_migrations(
    pool: &PgPool,
    sources: &[NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
    vectors: VectorStorage,
) -> Result<Vec<PendingMigration>, MigrationError> {
    pending_migrations_with_policy(
        pool,
        sources,
        sidecars,
        MissingMigrationPolicy::Reject,
        vectors,
    )
    .await
}

/// [`pending_migrations`] that waits `lock_timeout` for the runner's
/// advisory lock, then fails as a run would (`55P03`).
///
/// # Errors
///
/// The errors of [`pending_migrations`].
pub async fn pending_migrations_with_lock_timeout(
    pool: &PgPool,
    sources: &[NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
    vectors: VectorStorage,
    lock_timeout: Duration,
) -> Result<Vec<PendingMigration>, MigrationError> {
    pending_migrations_with_policy_and_lock_timeout(
        pool,
        sources,
        sidecars,
        MissingMigrationPolicy::Reject,
        vectors,
        lock_timeout,
    )
    .await
}

/// [`pending_migrations`] with an explicit missing-version policy.
///
/// # Errors
///
/// The errors of [`pending_migrations`]. Unknown versions return
/// [`MigrateError::VersionMissing`] unless the policy allows them.
pub async fn pending_migrations_with_policy(
    pool: &PgPool,
    sources: &[NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
    policy: MissingMigrationPolicy,
    vectors: VectorStorage,
) -> Result<Vec<PendingMigration>, MigrationError> {
    pending_migrations_with_policy_and_lock_timeout(
        pool,
        sources,
        sidecars,
        policy,
        vectors,
        Duration::from_secs(5),
    )
    .await
}

async fn pending_migrations_with_policy_and_lock_timeout(
    pool: &PgPool,
    sources: &[NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
    policy: MissingMigrationPolicy,
    vectors: VectorStorage,
    lock_timeout: Duration,
) -> Result<Vec<PendingMigration>, MigrationError> {
    let core = core_source(vectors);
    let lineup: Vec<&NamedMigrator> = std::iter::once(&core).chain(sources).collect();
    validate_sources(&lineup)?;
    let mut conn = acquire_session_connection(pool).await?;
    plan_under_lock(&mut conn, &lineup, sidecars, policy, vectors, lock_timeout).await
}

/// The lock wait and the snapshot's statements are a run's own, so their
/// failures are a run's: core's, the first source in the run.
fn execute_error(err: sqlx::Error) -> MigrationError {
    MigrationError::Core(MigrateError::Execute(err))
}

async fn plan_under_lock(
    conn: &mut PgConnection,
    lineup: &[&NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
    policy: MissingMigrationPolicy,
    vectors: VectorStorage,
    lock_timeout: Duration,
) -> Result<Vec<PendingMigration>, MigrationError> {
    sqlx::query("SELECT set_config('lock_timeout', $1, false)")
        .bind(format!("{}ms", lock_timeout.as_millis()))
        .execute(&mut *conn)
        .await
        .map_err(execute_error)?;
    let mut lock = SessionLock::acquire(conn).await.map_err(execute_error)?;
    let mut snapshot = lock.begin_snapshot().await.map_err(execute_error)?;
    let plan = plan_snapshot(&mut snapshot, lineup, sidecars, policy, vectors).await;
    // Nothing to commit: the snapshot only read.
    let ended = snapshot.rollback().await;
    let released = lock.release().await;
    let plan = plan?;
    ended.map_err(execute_error)?;
    released.map_err(execute_error)?;
    Ok(plan)
}

/// [`MIGRATION_LOCK_KEY`] held on one session, from ledger validation through
/// the run's commits or the plan's snapshot. It also excludes each source's
/// transaction lock (`pg_advisory_xact_lock`) on the same key.
///
/// Held until [`Self::release`] or until the connection closes. The
/// snapshot can only begin on a held lock: a lock taken inside a `REPEATABLE
/// READ` transaction is granted after its snapshot is fixed.
struct SessionLock<'c> {
    conn: &'c mut PgConnection,
}

impl<'c> SessionLock<'c> {
    async fn acquire(conn: &'c mut PgConnection) -> Result<Self, sqlx::Error> {
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(MIGRATION_LOCK_KEY)
            .execute(&mut *conn)
            .await?;
        Ok(Self { conn })
    }

    async fn begin_snapshot(&mut self) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
        let mut snapshot = self.conn.begin().await?;
        // Before any query: the snapshot is fixed by the first one.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *snapshot)
            .await?;
        Ok(snapshot)
    }

    async fn release(self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(MIGRATION_LOCK_KEY)
            .execute(&mut *self.conn)
            .await?;
        Ok(())
    }
}

/// Validate the composed ledgers, then walk sources in the order a run does:
/// each source's `no_tx` refusal, lineage check, and the dirty and checksum
/// refusals of `SQLx`'s run.
async fn plan_snapshot(
    snapshot: &mut PgConnection,
    lineup: &[&NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
    policy: MissingMigrationPolicy,
    vectors: VectorStorage,
) -> Result<Vec<PendingMigration>, MigrationError> {
    validate_ledgers(snapshot, lineup, policy, vectors).await?;
    let mut plan = Vec::new();
    let pending_evolutions = sidecars
        .pending_schema_evolutions(snapshot)
        .await
        .map_err(MigrationError::SchemaEvolution)?;
    let pending_ids = pending_evolutions
        .iter()
        .map(|evolution| evolution.id)
        .collect::<BTreeSet<_>>();
    for evolution in schema_evolution_schedule(lineup, sidecars.schema_evolutions())? {
        let spiice = evolution
            .sources
            .first()
            .map_or("", |source| source.id.spiice);
        if pending_ids.contains(evolution.id) {
            plan.push(PendingMigration::SchemaEvolution {
                spiice,
                id: evolution.id,
                from: evolution
                    .sources
                    .iter()
                    .map(|source| source.schema_id().to_string())
                    .collect(),
                to: evolution
                    .targets
                    .iter()
                    .map(|target| target.schema_id().to_string())
                    .collect(),
            });
        }
    }
    for source in lineup {
        if source.source() == CORE_SOURCE {
            source.require_transactional()?;
            let rows = read_ledger(snapshot, source, &source.ledger(), true).await?;
            unrecorded(&source.lane(), &rows).map_err(|err| source.run_error(err))?;
            let core = proxiima_storage_pg::pending_core_schema(
                snapshot,
                MissingMigrationPolicy::Allow,
                vectors,
            )
            .await
            .map_err(MigrationError::CorePreflight)?;
            plan.extend(core.into_iter().map(|action| match action {
                proxiima_storage_pg::CoreSchemaAction::Migration(migration) => {
                    PendingMigration::Sql {
                        ledger: source.ledger(),
                        version: migration.version,
                        description: migration.description.into_owned(),
                        checksum: migration.checksum.into_owned(),
                    }
                }
                proxiima_storage_pg::CoreSchemaAction::AdoptPreviousHead { release } => {
                    PendingMigration::AdoptPreviousHead { release }
                }
                proxiima_storage_pg::CoreSchemaAction::Definitions { checksum, release } => {
                    PendingMigration::CoreDefinitions { checksum, release }
                }
            }));
        } else {
            plan.extend(plan_source(snapshot, source).await?);
        }
    }
    let ordered = order_pending_operations(plan, lineup, sidecars.schema_evolutions())?;
    Ok(ordered)
}

fn order_pending_operations(
    plan: Vec<PendingMigration>,
    lineup: &[&NamedMigrator],
    evolutions: &[SchemaEvolution],
) -> Result<Vec<PendingMigration>, MigrationError> {
    if evolutions.is_empty() {
        return Ok(plan);
    }
    order_current_evolution_operations(plan, lineup, evolutions)
}

fn order_current_evolution_operations(
    current: Vec<PendingMigration>,
    lineup: &[&NamedMigrator],
    evolutions: &[SchemaEvolution],
) -> Result<Vec<PendingMigration>, MigrationError> {
    let mut core = Vec::new();
    let mut by_source = BTreeMap::<&'static str, Vec<PendingMigration>>::new();
    let mut pending_steps = BTreeMap::<&'static str, PendingMigration>::new();
    for operation in current {
        match &operation {
            PendingMigration::Sql { ledger, .. } => {
                let source = lineup
                    .iter()
                    .find(|source| source.ledger() == *ledger)
                    .expect("planned migration belongs to its source")
                    .source();
                if source == CORE_SOURCE {
                    core.push(operation);
                } else {
                    by_source.entry(source).or_default().push(operation);
                }
            }
            PendingMigration::AdoptPreviousHead { .. }
            | PendingMigration::CoreDefinitions { .. } => {
                core.push(operation);
            }
            PendingMigration::SchemaEvolution { id, .. } => {
                pending_steps.insert(id, operation);
            }
        }
    }
    let schedule = schema_evolution_schedule(lineup, evolutions)?;
    let mut ordered = core;
    let mut cursors = BTreeMap::<&'static str, usize>::new();
    for evolution in schedule {
        // check_lineup rejects duplicate versions across all source ledgers,
        // so this anchor identifies exactly one owning source.
        let Some(source) = lineup.iter().find(|source| {
            source.source() != CORE_SOURCE && source.has_version(evolution.after_migration)
        }) else {
            return Err(MigrationError::EvolutionAnchorMissing {
                evolution_id: evolution.id,
                after_migration: evolution.after_migration,
            });
        };
        let source_id = source.source();
        let cursor = cursors.entry(source_id).or_default();
        if let Some(pending) = by_source.get(source_id) {
            while *cursor < pending.len()
                && matches!(&pending[*cursor], PendingMigration::Sql { version, .. }
                    if *version <= evolution.after_migration)
            {
                ordered.push(pending[*cursor].clone());
                *cursor += 1;
            }
        }
        if let Some(step) = pending_steps.remove(evolution.id) {
            ordered.push(step);
        }
    }
    for (source, pending) in by_source {
        let cursor = cursors.entry(source).or_default();
        ordered.extend(pending.into_iter().skip(*cursor));
    }
    Ok(ordered)
}

async fn plan_source(
    snapshot: &mut PgConnection,
    source: &NamedMigrator,
) -> Result<Vec<PendingMigration>, MigrationError> {
    source.require_transactional()?;
    let rows = read_ledger(snapshot, source, &source.ledger(), true).await?;
    if !is_core_ledger(&source.migrator().table_name)
        && let Some(conflict) = ledger_conflict(&source.lane_versions(), &ledger_versions(&rows))
    {
        return Err(source.ledger_error(conflict));
    }
    let ledger = source.ledger();
    let pending = unrecorded(&source.lane(), &rows).map_err(|err| source.run_error(err))?;
    Ok(pending
        .into_iter()
        .map(|migration| PendingMigration::Sql {
            ledger: ledger.clone(),
            version: migration.version,
            description: migration.description.to_string(),
            checksum: migration.checksum.to_vec(),
        })
        .collect())
}

/// The migrations of `lane` that `rows` do not record, or the refusal
/// `SQLx`'s `run_direct` raises first for these rows: a `success = false` row
/// ([`MigrateError::Dirty`], the lowest version), then the first lane
/// migration whose recorded checksum differs
/// ([`MigrateError::VersionMismatch`]).
///
/// Missing versions are checked against the whole ledger's composed sources
/// before this source is planned or run; other sources can share its ledger.
fn unrecorded<'a>(
    lane: &[&'a Migration],
    rows: &[LedgerRow],
) -> Result<Vec<&'a Migration>, MigrateError> {
    if let Some(version) = rows
        .iter()
        .filter(|row| !row.success)
        .map(|row| row.version)
        .min()
    {
        return Err(MigrateError::Dirty(version));
    }
    let recorded: BTreeMap<i64, &LedgerRow> = rows.iter().map(|row| (row.version, row)).collect();
    let mut pending = Vec::new();
    for migration in lane {
        match recorded.get(&migration.version) {
            Some(row) if row.checksum.as_slice() != migration.checksum.as_ref() => {
                return Err(MigrateError::VersionMismatch(migration.version));
            }
            Some(_) => {}
            None => pending.push(*migration),
        }
    }
    Ok(pending)
}

impl MigrationRunReport {
    fn from_sources(sources: &[NamedMigrator]) -> Self {
        let sources = sources.iter().map(NamedMigrator::source).collect();
        Self { sources }
    }
}

async fn pin_migration_search_path(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SET search_path TO public")
        .execute(&mut *conn)
        .await?;
    // The pool's request-serving `statement_timeout`
    // must not abort a long schema migration (CREATE INDEX / backfill) mid-way.
    // Disable it for this boot connection; `acquire_session_connection` marks
    // it close-on-drop so the override never returns to the shared pool.
    sqlx::query("SET statement_timeout = 0")
        .execute(&mut *conn)
        .await?;
    // Waiting *for* a lock is not the same as holding one. A migration that
    // takes ACCESS EXCLUSIVE queues behind any
    // in-flight reader on the outgoing release — and in Postgres a queued
    // exclusive request blocks every reader that arrives after it. With no
    // lock_timeout that pile-up is unbounded, so a rolling upgrade stalls the
    // whole table instead of the migration failing and retrying on the next
    // pod. Fail fast; the work itself still runs untimed once the lock is in
    // hand.
    sqlx::query(MIGRATION_LOCK_TIMEOUT_SQL)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// How long a migration waits for a table lock before giving up.
///
/// Short on purpose: the cost of failing is one pod restart, the cost of
/// waiting is every reader queued behind the request.
const MIGRATION_LOCK_TIMEOUT_SQL: &str = "SET lock_timeout = '5s'";

async fn reset_migration_search_path(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("RESET search_path").execute(&mut *conn).await?;
    Ok(())
}

async fn run_sources_on_connection(
    conn: &mut PgConnection,
    sources: &[NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
    vectors: VectorStorage,
) -> Result<(), MigrationError> {
    if !sidecars.schema_evolutions().is_empty() {
        return run_evolution_sources(conn, sources, sidecars, vectors).await;
    }
    for source in sources {
        if source.source() != CORE_SOURCE {
            prepare_ledger(conn, source).await?;
        }
        run_source_with_contention_retry(conn, source, vectors).await?;
        if source.source() == CORE_SOURCE {
            prepare_ledger(conn, source).await?;
            sync_owner_follows(conn, sidecars)
                .await?
                .commit()
                .await
                .map_err(execute_error)?;
        }
    }
    let mut transaction = sync_owner_follows(conn, sidecars).await?;
    sidecars
        .apply_search_definitions(&mut transaction)
        .await
        .map_err(MigrationError::SearchDefinitions)?;
    sidecars
        .reinstall_owner_rls(&mut transaction)
        .await
        .map_err(MigrationError::SidecarOwnerRls)?;
    sidecars
        .ensure_owner_rls(&mut transaction)
        .await
        .map_err(MigrationError::SidecarOwnerRls)?;
    transaction.commit().await.map_err(execute_error)
}

async fn run_evolution_sources(
    conn: &mut PgConnection,
    sources: &[NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
    vectors: VectorStorage,
) -> Result<(), MigrationError> {
    let source_refs = sources.iter().collect::<Vec<_>>();
    let schedule = schema_evolution_schedule(&source_refs, sidecars.schema_evolutions())?;
    let mut transaction = proxiima_storage_pg::begin_migration_transaction(conn)
        .await
        .map_err(MigrationError::CorePreflight)?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *transaction)
        .await
        .map_err(execute_error)?;
    if sources.iter().any(|source| source.source() == CORE_SOURCE) {
        proxiima_storage_pg::run_core_migrations(
            &mut transaction,
            MissingMigrationPolicy::Allow,
            vectors,
        )
        .await
        .map_err(MigrationError::CorePreflight)?;
    }
    // Core creates the ledger on a first install. Spiice ledgers derive their
    // shape from it, and their preparation belongs to this same transaction.
    for source in sources {
        prepare_ledger(transaction.as_mut(), source).await?;
    }
    let mut applied = BTreeMap::<&'static str, i64>::new();
    sidecars
        .sync_owner_follows(&mut transaction)
        .await
        .map_err(MigrationError::SidecarOwnerRls)?;
    for evolution in schedule {
        // check_lineup rejects duplicate versions across all source ledgers,
        // so this anchor identifies exactly one owning source.
        let Some(source) = sources.iter().find(|source| {
            source.source() != CORE_SOURCE && source.has_version(evolution.after_migration)
        }) else {
            return Err(MigrationError::EvolutionAnchorMissing {
                evolution_id: evolution.id,
                after_migration: evolution.after_migration,
            });
        };
        let previous = applied.get(source.source()).copied().unwrap_or(i64::MIN);
        run_source_range(
            &mut transaction,
            source,
            vectors,
            previous,
            evolution.after_migration,
        )
        .await?;
        applied.insert(source.source(), evolution.after_migration);
        sidecars
            .run_schema_evolution(&mut transaction, evolution)
            .await
            .map_err(MigrationError::SchemaEvolution)?;
        sidecars
            .checkpoint_schema_evolution(&mut transaction)
            .await
            .map_err(MigrationError::SchemaEvolution)?;
    }
    sidecars
        .finish_schema_evolutions(&mut transaction)
        .await
        .map_err(MigrationError::SchemaEvolution)?;
    // Source rows and deferred sidecar obligations are drained before a
    // trailing spiice migration may remove their obsolete tables.
    for source in sources
        .iter()
        .filter(|source| source.source() != CORE_SOURCE)
    {
        let previous = applied.get(source.source()).copied().unwrap_or(i64::MIN);
        run_source_range(&mut transaction, source, vectors, previous, i64::MAX).await?;
    }
    sidecars
        .sync_owner_follows(&mut transaction)
        .await
        .map_err(MigrationError::SidecarOwnerRls)?;
    sidecars
        .apply_search_definitions(&mut transaction)
        .await
        .map_err(MigrationError::SearchDefinitions)?;
    sidecars
        .reinstall_owner_rls(&mut transaction)
        .await
        .map_err(MigrationError::SidecarOwnerRls)?;
    sidecars
        .ensure_owner_rls(&mut transaction)
        .await
        .map_err(MigrationError::SidecarOwnerRls)?;
    transaction.commit().await.map_err(execute_error)
}

async fn run_source_range(
    transaction: &mut Transaction<'_, Postgres>,
    source: &NamedMigrator,
    _vectors: VectorStorage,
    lower_exclusive: i64,
    upper_inclusive: i64,
) -> Result<(), MigrationError> {
    let migrations = source.migrator();
    let selected = migrations
        .iter()
        .filter(|migration| {
            migration.version > lower_exclusive && migration.version <= upper_inclusive
        })
        .cloned()
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Ok(());
    }
    let filtered = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(selected),
        ignore_missing: true,
        locking: migrations.locking,
        no_tx: migrations.no_tx,
        table_name: migrations.table_name.clone(),
        create_schemas: migrations.create_schemas.clone(),
    };
    filtered
        .run_direct(None, transaction.as_mut(), false)
        .await
        .map_err(|error| source.run_error(error))
}

async fn sync_owner_follows<'c>(
    conn: &'c mut PgConnection,
    sidecars: &PgSidecarRegistryFrozen,
) -> Result<Transaction<'c, Postgres>, MigrationError> {
    let mut transaction = proxiima_storage_pg::begin_migration_transaction(conn)
        .await
        .map_err(MigrationError::SidecarOwnerRls)?;
    sidecars
        .sync_owner_follows(&mut transaction)
        .await
        .map_err(MigrationError::SidecarOwnerRls)?;
    Ok(transaction)
}

/// Total attempts for one source when shared-catalog contention is the only
/// failure. Contention needs another migrator racing on the same cluster, and
/// every round settles one winner whose role DDL is then committed — N racing
/// boots need at most N rounds, and a genuine catalog problem still surfaces
/// instead of looping.
const CATALOG_CONTENTION_ATTEMPTS: u32 = 5;

/// Base delay between contention retries; multiplied by the attempt number so
/// concurrent losers decorrelate without a randomness source.
const CATALOG_CONTENTION_BACKOFF: Duration = Duration::from_millis(100);

/// Run one source, retrying when it loses a shared-catalog race.
///
/// Role DDL in a migration (`ALTER ROLE`, `GRANT <role> TO ...`) updates
/// cluster-shared catalogs (`pg_authid`, `pg_auth_members`). No lock taken on
/// this connection can exclude a concurrent migrator on a *sibling database*
/// of the same cluster: Postgres advisory locks — `SQLx`'s per-run migrator
/// lock included — are scoped to the database in their lock tag, so two boots
/// migrating two different databases hold their locks independently and still
/// collide on the one shared tuple. The loser gets `tuple concurrently
/// updated` (raised by Postgres' non-MVCC catalog update path).
///
/// Retrying the whole source run is safe: each migration applies inside its
/// own transaction together with its ledger row, so a failed run recorded
/// nothing for the failing version, and the retry skips already-recorded
/// versions and re-executes only the loser. A failed attempt also leaves this
/// session's migrator advisory lock stacked (`run_direct` skips its unlock on
/// error, and re-locking on retry stacks); that is contained because the
/// facade never returns the migration connection to the pool.
///
/// A spiice's ledger is checked against its lane ([`LedgerConflict`]) under
/// the same lock, so no replica booting alongside changes it in between.
async fn run_source_with_contention_retry(
    conn: &mut PgConnection,
    source: &NamedMigrator,
    vectors: VectorStorage,
) -> Result<(), MigrationError> {
    let mut attempt = 1;
    loop {
        source.require_transactional()?;
        let mut transaction = proxiima_storage_pg::begin_migration_transaction(conn)
            .await
            .map_err(|error| {
                source.run_error(MigrateError::Execute(sqlx::Error::Protocol(
                    error.to_string(),
                )))
            })?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(MIGRATION_LOCK_KEY)
            .execute(&mut *transaction)
            .await
            .map_err(|error| source.run_error(MigrateError::Execute(error)))?;
        if !is_core_ledger(&source.migrator().table_name) {
            let recorded = ledger_versions(
                &read_ledger(&mut transaction, source, &source.ledger(), false).await?,
            );
            if let Some(conflict) = ledger_conflict(&source.lane_versions(), &recorded) {
                transaction
                    .rollback()
                    .await
                    .map_err(|error| source.run_error(MigrateError::Execute(error)))?;
                return Err(source.ledger_error(conflict));
            }
        }
        if source.source() == CORE_SOURCE {
            ensure_core_ledger_compatible_on_connection_with_policy(
                &mut transaction,
                MissingMigrationPolicy::Allow,
            )
            .await
            .map_err(MigrationError::CorePreflight)?;
            let rows = read_ledger(&mut transaction, source, &source.ledger(), true).await?;
            unrecorded(&source.lane(), &rows).map_err(|err| source.run_error(err))?;
            proxiima_storage_pg::run_core_migrations(
                &mut transaction,
                MissingMigrationPolicy::Allow,
                vectors,
            )
            .await
            .map_err(MigrationError::CorePreflight)?;
            return transaction
                .commit()
                .await
                .map_err(|err| source.run_error(MigrateError::Execute(err)));
        }
        let result = source
            .migrator()
            .run_direct(None, &mut *transaction, false)
            .await;
        let result = match result {
            Ok(()) => transaction.commit().await.map_err(MigrateError::Execute),
            Err(error) => {
                transaction
                    .rollback()
                    .await
                    .map_err(|error| source.run_error(MigrateError::Execute(error)))?;
                Err(error)
            }
        };
        match result {
            Err(err)
                if attempt < CATALOG_CONTENTION_ATTEMPTS && is_shared_catalog_contention(&err) =>
            {
                tracing::warn!(
                    source = source.source(),
                    attempt,
                    error = %err,
                    "shared-catalog contention during migration; retrying"
                );
                tokio::time::sleep(CATALOG_CONTENTION_BACKOFF * attempt).await;
                attempt += 1;
            }
            result => return result.map_err(|err| source.run_error(err)),
        }
    }
}

/// Whether a migration failure is a lost race on a cluster-shared catalog.
///
/// The race has two server-side shapes. Updating an *existing* shared tuple
/// concurrently raises `tuple concurrently updated` — matched on the message
/// because Postgres files it under the catch-all `XX000` internal-error
/// SQLSTATE, which would sweep in genuine corruption (under a non-English
/// `lc_messages` the match misses and the boot fails exactly as it did
/// without the retry — degraded to the status quo, never looser). A
/// concurrent *first* write of a shared tuple instead surfaces as a
/// unique-key violation (`23505`) on the catalog's index — both sessions saw
/// no row and both insert — so `23505` counts only when the named table is
/// one of the role-DDL shared catalogs, where re-running converges (the
/// retry's write finds the winner's row and updates it, or reports the true
/// conflict, e.g. a duplicate `CREATE ROLE`).
fn is_shared_catalog_contention(err: &MigrateError) -> bool {
    let (MigrateError::Execute(sqlx::Error::Database(db))
    | MigrateError::ExecuteMigration(sqlx::Error::Database(db), _)) = err
    else {
        return false;
    };
    if matches!(
        db.message(),
        "tuple concurrently updated" | "tuple concurrently deleted"
    ) {
        return true;
    }
    db.code().as_deref() == Some("23505")
        && matches!(
            db.table(),
            Some("pg_authid" | "pg_auth_members" | "pg_db_role_setting")
        )
}

/// `CREATE SCHEMA IF NOT EXISTS` for a `create_schemas` entry, which is one
/// identifier part in Postgres' spelling rules. `None` for a spelling that is
/// not one such part.
fn create_schema_sql(schema: &str) -> Option<SqlText> {
    let [part] = <[String; 1]>::try_from(parse_name_parts(schema)?).ok()?;
    let mut sql = SqlText::new("CREATE SCHEMA IF NOT EXISTS ");
    sql.push_ident(&PgIdent::quoted(&part).ok()?);
    Some(sql)
}

/// Prepare a spiice's own ledger before its migrator first reads it: create
/// it and leave it read-only for every role but its owner. Runs on every migration
/// run; every step is idempotent. Core's ledger, which `SQLx` creates on
/// core's first run, gets the read-only step alone, after core migrates.
///
/// - Serialized. Pods booting together race on `CREATE SCHEMA`, `CREATE
///   TABLE` (`pg_type` unique index) and the ledger's ACL ("tuple
///   concurrently updated"), all before `SQLx`'s own migrator lock; the
///   transaction takes [`MIGRATION_LOCK_KEY`] first.
/// - Read-only. The platform role's default privileges hand every new
///   table's DML to the runtime role, and a runtime role that can delete a
///   ledger row makes the next boot re-run that migration — a destructive
///   baseline included. Every non-owner INSERT/UPDATE/DELETE/TRUNCATE grant
///   on the ledger is revoked (`CASCADE`, so grants passed on go too); SELECT
///   stays. A role that owns the ledger then refuses the boot if any such
///   grant is left; one that does not can revoke only what it granted.
async fn prepare_ledger(
    conn: &mut PgConnection,
    source: &NamedMigrator,
) -> Result<(), MigrationError> {
    let table_name = source.migrator().table_name.clone();
    let map_err = |err: sqlx::Error| MigrationError::SpiiceLedgerPreparation {
        source: source.source(),
        err,
    };
    let mut tx = conn.begin().await.map_err(map_err)?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(tx.as_mut())
        .await
        .map_err(map_err)?;
    let schemas = if is_core_ledger(&table_name) {
        &[][..]
    } else {
        &source.migrator().create_schemas[..]
    };
    for schema in schemas {
        // The entry is resolved by Postgres' identifier rules and spliced
        // quoted, so any spelling the parser reads names the schema it names
        // in SQL.
        let create = create_schema_sql(schema).ok_or_else(|| {
            map_err(sqlx::Error::Protocol(format!(
                "unreadable schema name: {schema:?}"
            )))
        })?;
        sqlx::query(create)
            .execute(tx.as_mut())
            .await
            .map_err(map_err)?;
    }
    sqlx::query("SELECT set_config('proxiima.spiice_ledger', $1, true)")
        .bind(&table_name)
        .execute(tx.as_mut())
        .await
        .map_err(map_err)?;
    sqlx::query(PREPARE_LEDGER)
        .execute(tx.as_mut())
        .await
        .map_err(map_err)?;
    tx.commit().await.map_err(map_err)
}

/// `pg_advisory_xact_lock` key taken first in every transaction this module
/// opens — each source's run and each ledger's preparation: ASCII
/// `proxmigr`, beside `runtime_grants`' `proxgrnt`. `SQLx`'s own migrator lock
/// is session-level and released inside `run_direct`, before the enclosing
/// transaction commits; alone, it let a replica booting alongside read the
/// ledger before the first one's rows were visible and re-apply them
/// (`schema "proxiima_core" already exists`). The wait is bounded by
/// [`MIGRATION_LOCK_TIMEOUT_SQL`], as `SQLx`'s is.
const MIGRATION_LOCK_KEY: i64 = i64::from_be_bytes(*b"proxmigr");

/// [`prepare_ledger`]'s statement. The ledger name arrives through a
/// transaction-local setting, so the statement text is fixed; it is the
/// spiice migrator's compiled-in table name, interpolated as `SQLx` itself
/// interpolates it in `ensure_migrations_table`.
const PREPARE_LEDGER: &str = r"DO $prepare_ledger$
DECLARE
    ledger text := current_setting('proxiima.spiice_ledger');
    ledger_oid regclass;
    grantee oid;
BEGIN
    IF to_regclass('public._sqlx_migrations') IS NULL THEN
        RAISE EXCEPTION 'core ledger public._sqlx_migrations is missing; core migrates first';
    END IF;
    IF to_regclass(ledger) IS DISTINCT FROM 'public._sqlx_migrations'::regclass THEN
        EXECUTE format('CREATE TABLE IF NOT EXISTS %s (LIKE public._sqlx_migrations INCLUDING ALL)', ledger);
    END IF;
    ledger_oid := ledger::regclass;
    FOR grantee IN
        SELECT DISTINCT acl.grantee
          FROM pg_class AS c, aclexplode(c.relacl) AS acl
         WHERE c.oid = ledger_oid AND acl.grantee <> c.relowner
           AND acl.privilege_type IN ('INSERT', 'UPDATE', 'DELETE', 'TRUNCATE')
    LOOP
        EXECUTE format('REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON %s FROM %s CASCADE', ledger_oid,
            CASE WHEN grantee = 0 THEN 'PUBLIC' ELSE quote_ident(pg_get_userbyid(grantee)) END);
    END LOOP;
    IF EXISTS (SELECT 1
                 FROM pg_class AS c, aclexplode(c.relacl) AS acl
                WHERE c.oid = ledger_oid AND pg_has_role(c.relowner, 'USAGE')
                  AND acl.grantee <> c.relowner
                  AND acl.privilege_type IN ('INSERT', 'UPDATE', 'DELETE', 'TRUNCATE')) THEN
        RAISE EXCEPTION 'migration ledger % is still writable by a role other than its owner', ledger_oid;
    END IF;
END
$prepare_ledger$";

/// What the migration runner reads of a [`NamedMigrator`]: its errors, its
/// ledger and the lane it applies. Private to the runner.
pub(crate) trait MigrationSource {
    fn run_error(&self, err: MigrateError) -> MigrationError;
    fn ledger_error(&self, conflict: LedgerConflict) -> MigrationError;
    fn require_transactional(&self) -> Result<(), MigrationError>;
    fn ledger(&self) -> String;
    fn ledger_identity(&self) -> LedgerIdentity;
    fn lane(&self) -> Vec<&Migration>;
    fn lane_versions(&self) -> Vec<i64>;
    fn has_version(&self, version: i64) -> bool;
}

impl MigrationSource for NamedMigrator {
    fn run_error(&self, err: MigrateError) -> MigrationError {
        if self.source() == CORE_SOURCE {
            MigrationError::Core(err)
        } else {
            MigrationError::Spiice {
                source: self.source(),
                err,
            }
        }
    }

    fn ledger_error(&self, conflict: LedgerConflict) -> MigrationError {
        MigrationError::Ledger {
            source: self.source(),
            ledger: self.migrator().table_name.to_string(),
            conflict,
        }
    }

    /// A run applies each source inside the transaction that also carries
    /// its scope and ledger rows, so it refuses a `no_tx` migration before
    /// it touches the ledger.
    fn require_transactional(&self) -> Result<(), MigrationError> {
        if self.migrator().iter().any(|migration| migration.no_tx) {
            return Err(self.run_error(MigrateError::Execute(sqlx::Error::Protocol(
                "platform-scoped migrations require transactional migration files".into(),
            ))));
        }
        Ok(())
    }

    /// The schema-qualified table this source records on, spelled as
    /// [`spiice_ledger_table`] spells a spiice's. A table declared without a
    /// schema lives in `public`, where a run pins `search_path`.
    fn ledger(&self) -> String {
        let table = self.migrator().table_name.as_ref();
        if table.contains('.') {
            table.to_owned()
        } else {
            format!("public.{table}")
        }
    }

    /// What two sources must not share: the table Postgres resolves
    /// [`Self::ledger`] to, whatever its spelling. Matching follows Postgres'
    /// identifier rules ([`parse_ledger_name`]); a spelling outside them is
    /// compared as written.
    fn ledger_identity(&self) -> LedgerIdentity {
        let name = self.migrator().table_name.as_ref();
        match parse_ledger_name(name) {
            Some((schema, table)) => LedgerIdentity::Resolved { schema, table },
            None => LedgerIdentity::Spelled(name.to_owned()),
        }
    }

    /// What a run applies for this source, in the order it applies it: every
    /// migration it ships but its down migrations, in the migrator's own
    /// order, which is what `SQLx`'s `run_direct` walks. `sqlx::migrate!`
    /// yields ascending versions; a migrator built by hand keeps the order it
    /// was given, and the plan, like the run, applies and refuses in that
    /// order. The one enumeration the ledger checks, [`pending_migrations`]
    /// and a test-template fingerprint share; [`Self::ledger`] names the table
    /// each entry records on.
    ///
    /// A version a source repeats is listed once, at its first place; a
    /// lineup with one is refused before a run or a plan reads the database
    /// ([`validate_sources`]).
    fn lane(&self) -> Vec<&Migration> {
        let mut seen = BTreeSet::new();
        self.migrator()
            .iter()
            .filter(|migration| !migration.migration_type.is_down_migration())
            .filter(|migration| seen.insert(migration.version))
            .collect()
    }

    /// The versions of [`Self::lane`], ascending: the lineage view of a
    /// ledger, where the order a run walks does not matter.
    fn lane_versions(&self) -> Vec<i64> {
        let versions: BTreeSet<i64> = self
            .lane()
            .into_iter()
            .map(|migration| migration.version)
            .collect();
        versions.into_iter().collect()
    }

    fn has_version(&self, version: i64) -> bool {
        self.lane()
            .iter()
            .any(|migration| migration.version == version)
    }
}

/// A ledger as two sources compare it. Private: only the shared-ledger check
/// reads it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LedgerIdentity {
    /// The table the name resolves to.
    Resolved { schema: String, table: String },
    /// A spelling [`parse_ledger_name`] does not read (a comment inside the
    /// name, say): all that is known is how it is written.
    Spelled(String),
}

impl std::fmt::Display for LedgerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resolved { schema, table } => write!(f, "{schema}.{table}"),
            Self::Spelled(name) => f.write_str(name),
        }
    }
}

/// `SELECT` of a ledger's rows. The spelling is resolved by
/// [`parse_ledger_name`] and spliced quoted. `None` for a spelling it does
/// not read.
fn ledger_select_sql(ledger: &str) -> Option<SqlText> {
    let (schema, table) = parse_ledger_name(ledger)?;
    let mut sql = SqlText::new("SELECT version, checksum, success FROM ");
    sql.push_ident(&PgIdent::quoted(&schema).ok()?)
        .push(".")
        .push_ident(&PgIdent::quoted(&table).ok()?)
        .push(" ORDER BY version");
    Some(sql)
}

/// One row of a migration ledger, as `SQLx` writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LedgerRow {
    version: i64,
    checksum: Vec<u8>,
    success: bool,
}

fn ledger_versions(rows: &[LedgerRow]) -> Vec<i64> {
    rows.iter().map(|row| row.version).collect()
}

/// The rows of `ledger`, ascending by version, on behalf of `source`. A
/// ledger that does not exist holds nothing when `may_be_absent`; the
/// migration run creates every spiice ledger before reading it.
///
/// The one ledger read of the runner's lineage check, the preflight and the
/// plan. It reads what `SQLx`'s run reads (`list_applied_migrations`, every
/// row whatever its `success`) and adds the `success` that its
/// `dirty_version` reads separately.
async fn read_ledger(
    conn: &mut PgConnection,
    source: &NamedMigrator,
    ledger: &str,
    may_be_absent: bool,
) -> Result<Vec<LedgerRow>, MigrationError> {
    let read_error = |err| MigrationError::LedgerRead {
        source: source.source(),
        ledger: ledger.to_owned(),
        err: MigrateError::Execute(err),
    };
    if may_be_absent {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(ledger)
            .fetch_one(&mut *conn)
            .await
            .map_err(read_error)?;
        if !exists {
            return Ok(Vec::new());
        }
    }
    // The migrator's table name is resolved by Postgres' identifier rules and
    // spliced quoted; a spelling the parser does not read is refused.
    let select = ledger_select_sql(ledger).ok_or_else(|| {
        read_error(sqlx::Error::Protocol(format!(
            "unreadable ledger name: {ledger:?}"
        )))
    })?;
    let rows: Vec<(i64, Vec<u8>, bool)> = sqlx::query_as(select)
        .fetch_all(&mut *conn)
        .await
        .map_err(read_error)?;
    Ok(rows
        .into_iter()
        .map(|(version, checksum, success)| LedgerRow {
            version,
            checksum,
            success,
        })
        .collect())
}

/// A ledger and every version its composed sources ship. The first source
/// owns errors for the shared table, as core does in the migration run.
struct ComposedLedger<'a> {
    source: &'a NamedMigrator,
    versions: BTreeSet<i64>,
}

fn composed_ledgers<'a>(sources: &[&'a NamedMigrator]) -> Vec<ComposedLedger<'a>> {
    let mut indices = BTreeMap::new();
    let mut ledgers: Vec<ComposedLedger<'a>> = Vec::new();
    for &source in sources {
        let next_index = ledgers.len();
        let index = *indices
            .entry(source.ledger_identity())
            .or_insert(next_index);
        if index == next_index {
            ledgers.push(ComposedLedger {
                source,
                versions: BTreeSet::new(),
            });
        }
        ledgers[index].versions.extend(source.lane_versions());
    }
    ledgers
}

/// Validate external rows once against all sources that use their ledger,
/// before any source can create schemas, prepare ledgers, or apply SQL.
/// The facade enforces `policy` on the composed version union and definitions
/// record here, then passes [`MissingMigrationPolicy::Allow`] to storage-pg's core path.
async fn validate_ledgers(
    conn: &mut PgConnection,
    sources: &[&NamedMigrator],
    policy: MissingMigrationPolicy,
    vectors: VectorStorage,
) -> Result<(), MigrationError> {
    // The facade owns the complete set of sources. The backend's artifact
    // and checksum checks remain mandatory, while only this union can decide
    // whether a version belongs to another source sharing core's ledger.
    ensure_core_ledger_compatible_on_connection_with_policy(conn, MissingMigrationPolicy::Allow)
        .await
        .map_err(MigrationError::CorePreflight)?;
    let assignments: BTreeMap<i64, &NamedMigrator> = sources
        .iter()
        .flat_map(|&source| {
            source
                .lane_versions()
                .into_iter()
                .map(move |version| (version, source))
        })
        .collect();
    for ledger in composed_ledgers(sources) {
        let rows = read_ledger(conn, ledger.source, &ledger.source.ledger(), true).await?;
        for row in &rows {
            if let Some(expected) = assignments.get(&row.version)
                && expected.ledger_identity() != ledger.source.ledger_identity()
            {
                return Err(expected.ledger_error(LedgerConflict::RecordedElsewhere {
                    version: row.version,
                    ledger: ledger.source.ledger(),
                }));
            }
        }
        // Core rows below the baseline are a previous head that the core
        // check above accepted for adoption; any other such row it refused.
        let rows = if ledger.source.source() == CORE_SOURCE {
            let baseline = ledger.source.lane_versions().first().copied();
            rows.into_iter()
                .filter(|row| baseline.is_none_or(|baseline| row.version >= baseline))
                .collect()
        } else {
            rows
        };
        check_recorded_versions(&ledger.versions, &rows, policy)
            .map_err(|err| ledger.source.run_error(err))?;
    }
    for &source in sources {
        source.require_transactional()?;
        let rows = read_ledger(conn, source, &source.ledger(), true).await?;
        unrecorded(&source.lane(), &rows).map_err(|err| source.run_error(err))?;
    }
    proxiima_storage_pg::definitions::current_matches_with_policy(conn, policy, vectors)
        .await
        .map_err(MigrationError::CorePreflight)?;
    Ok(())
}

fn check_recorded_versions(
    shipped: &BTreeSet<i64>,
    rows: &[LedgerRow],
    policy: MissingMigrationPolicy,
) -> Result<(), MigrateError> {
    match policy {
        MissingMigrationPolicy::Allow => Ok(()),
        MissingMigrationPolicy::Reject => {
            match rows
                .iter()
                .map(|row| row.version)
                .filter(|version| !shipped.contains(version))
                .min()
            {
                Some(version) => Err(MigrateError::VersionMissing(version)),
                None => Ok(()),
            }
        }
    }
}

/// Whether a lane (`lane`, the versions a binary ships) continues the one
/// its ledger records (`recorded`). Both ascending.
///
/// With missing versions explicitly allowed, unknown versions can represent
/// a newer release on the same database or a file the lane shed. They still
/// conflict beside pending work that would repeat or reorder theirs:
/// the lane's first migration still pending ([`LedgerConflict::Replaced`]),
/// or a pending migration older than one of them
/// ([`LedgerConflict::Diverged`]).
fn ledger_conflict(lane: &[i64], recorded: &[i64]) -> Option<LedgerConflict> {
    let shipped: BTreeSet<i64> = lane.iter().copied().collect();
    let applied: BTreeSet<i64> = recorded.iter().copied().collect();
    let unknown: Vec<i64> = applied.difference(&shipped).copied().collect();
    let pending: Vec<i64> = shipped.difference(&applied).copied().collect();
    let (&root, &newest_unknown, &oldest_pending) =
        (shipped.first()?, unknown.last()?, pending.first()?);
    if oldest_pending == root {
        return Some(LedgerConflict::Replaced { root, unknown });
    }
    if oldest_pending < newest_unknown {
        return Some(LedgerConflict::Diverged {
            pending: pending
                .into_iter()
                .filter(|version| *version < newest_unknown)
                .collect(),
            unknown: unknown
                .into_iter()
                .filter(|version| *version > oldest_pending)
                .collect(),
        });
    }
    None
}

/// The lane's migrations its ledger does not record, for a boot that
/// applies none.
fn unapplied(lane: &[i64], recorded: &[i64]) -> Option<LedgerConflict> {
    let pending: Vec<i64> = lane
        .iter()
        .copied()
        .filter(|version| recorded.binary_search(version).is_err())
        .collect();
    (!pending.is_empty()).then_some(LedgerConflict::Unapplied { pending })
}

fn core_source(vectors: VectorStorage) -> NamedMigrator {
    NamedMigrator::new(CORE_SOURCE, core_migrator_for(vectors))
}

fn prepare_sources(
    spiices: impl IntoIterator<Item = NamedMigrator>,
    vectors: VectorStorage,
) -> Result<Vec<NamedMigrator>, MigrationError> {
    let sources = lineup(spiices, vectors);
    check_lineup(&sources)?;
    Ok(sources)
}

/// Core followed by `spiices` in the given order, each prepared the way a
/// run needs it. `SQLx` ignores versions outside each individual source;
/// [`validate_ledgers`] checks the complete union before any DDL. Nothing is refused here: a run
/// validates the lineup ([`check_lineup`]) before it connects.
pub(crate) fn lineup(
    spiices: impl IntoIterator<Item = NamedMigrator>,
    vectors: VectorStorage,
) -> Vec<NamedMigrator> {
    let mut sources = vec![core_source(vectors)];

    for source in spiices {
        let id = source.source();
        let mut migrator = source.into_migrator();
        migrator.set_ignore_missing(true);
        let source = NamedMigrator::new(id, migrator);
        if is_core_ledger(&source.migrator().table_name) {
            tracing::warn!(
                source = source.source(),
                "spiice records its migrations in core's public._sqlx_migrations; \
                 build it with NamedMigrator::spiice to give it its own ledger \
                 (docs/build-a-spiice.md §Migrations)"
            );
        }
        sources.push(source);
    }
    sources
}

/// [`validate_sources`] over a [`lineup`]: the one check a run, a plan and a
/// test template make of the sources they are about to use.
pub(crate) fn check_lineup(sources: &[NamedMigrator]) -> Result<(), MigrationError> {
    validate_sources(&sources.iter().collect::<Vec<_>>())
}

/// The refusals that need no database, for core followed by the host's
/// sources: a run raises them before it connects, a plan likewise.
fn validate_sources(sources: &[&NamedMigrator]) -> Result<(), MigrationError> {
    reject_duplicate_versions(sources)?;
    reject_shared_spiice_ledgers(sources)
}

fn reject_shared_spiice_ledgers(sources: &[&NamedMigrator]) -> Result<(), MigrationError> {
    let mut seen: BTreeMap<LedgerIdentity, &'static str> = BTreeMap::new();
    for source in sources.iter().skip(1) {
        if is_core_ledger(&source.migrator().table_name) {
            continue;
        }
        // The table Postgres resolves the name to, not its spelling:
        // `_sqlx_migrations_host`, `public."_sqlx_migrations_host"` and
        // `PUBLIC._SQLX_Migrations_Host` are one table.
        let identity = source.ledger_identity();
        if let Some(first_source) = seen.insert(identity.clone(), source.source()) {
            return Err(MigrationError::DuplicateLedger {
                table: identity.to_string(),
                first_source,
                second_source: source.source(),
            });
        }
    }
    Ok(())
}

fn reject_duplicate_versions(sources: &[&NamedMigrator]) -> Result<(), MigrationError> {
    let mut seen: BTreeMap<i64, (&'static str, String)> = BTreeMap::new();

    for source in sources {
        for migration in source.migrator().iter() {
            let description = migration.description.to_string();
            if let Some((first_source, first_description)) =
                seen.insert(migration.version, (source.source(), description.clone()))
            {
                return Err(MigrationError::DuplicateVersion {
                    version: migration.version,
                    first_source,
                    first_description,
                    second_source: source.source(),
                    second_description: description,
                });
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
