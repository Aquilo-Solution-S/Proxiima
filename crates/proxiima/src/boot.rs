//! Storage and engine boot: connect, validate, compose.
//!
//! Internal. Hosts boot through [`crate::Proxiima`] / [`crate::RuntimeBuilder`];
//! this module turns one resolved [`RuntimeConfig`] plus its [`RuntimeParts`]
//! into a composed engine and the handles the runtime keeps from it.

use std::sync::Arc;

use proxiima_core::authz::SystemAuthority;
use proxiima_core::llm::{EmbeddingClient, EmbeddingRouter};
use proxiima_core::storage_ports::OwnerDropProofHandle;
use proxiima_core::storage_ports::publication::{OriginScope, PublicationOriginEligibilityPort};
use proxiima_core::{ColdObjectStore, Engine, SpiiceRegistry, StorageError, composed_schema_names};
use proxiima_storage_pg::{
    PgMigrationStorage, PgPlatformScope, PgSidecarRegistry, PgSidecarRegistryFrozen, PgStorage,
    VectorStorage, register_core_pg_sidecars,
};
use sqlx::PgPool;

use crate::bundle::SpiiceBundle;
use crate::migrations::run_core_and_spiice_migrations_with_policy;
use crate::runtime_grants::{self, RuntimeGrants};
use crate::{
    CitedBlobStore, LedgerConflict, MigrationError, NamedMigrator, ProxiimaError, RuntimeConfig,
    RuntimeParts, preflight_without_migrations_with_policy,
};

/// A composed engine, ready for host services and retirement before startup.
pub(crate) struct Booted {
    pub(crate) engine: Arc<Engine>,
    pub(crate) system_authority: std::sync::Arc<SystemAuthority>,
    pub(crate) host_state_maintenance_authority:
        Option<proxiima_core::engine::HostStateMaintenanceAuthority>,
    pub(crate) delegation_runtime_authority: proxiima_core::DelegationRuntimeAuthority,
    pub(crate) pool: PgPool,
    pub(crate) platform_scope: proxiima_storage_pg::PgPlatformScope,
    pub(crate) registry: Arc<proxiima_core::SpiiceRegistryFrozen>,
    pub(crate) pg_sidecars: Arc<PgSidecarRegistryFrozen>,
    pub(crate) erase_context: proxiima_storage_pg::PgHostStateEraseContext,
    pub(crate) publication_origin_eligibility: Arc<dyn PublicationOriginEligibilityPort>,
    /// This installation's minted identity, read once from the same
    /// database that answers `publication_origin_eligibility`.
    ///
    /// Both halves of the retained-copy rule have to come from one place:
    /// a host that forwards the outbox to its own broker stamps this on its
    /// copies and its cleaner refuses any message not carrying it, so "no
    /// origin row" is only ever read as a revocation over copies this
    /// database actually published.
    pub(crate) origin_scope: OriginScope,
    pub(crate) blobs: Option<CitedBlobStore>,
    /// The serving storage with the object store wired as its cold store:
    /// what the object-purge feature drains through.
    pub(crate) storage: PgStorage,
    /// The host-only drain over captured publication records.
    ///
    /// Deliberately absent from `StoragePorts`, `Engine` and `ToolCtx` — a
    /// spiice that could claim an outbox record could delay or suppress an
    /// export — so only the publisher task the runtime starts ever sees it.
    pub(crate) outbox: Arc<dyn proxiima_core::storage_ports::publication::PublicationOutboxPort>,
    /// Operator-only reclaim of records that were already DELIVERED, held
    /// under the same rule and for the same reason as `outbox`. A second
    /// handle rather than a method on the first: a drain loop must not be
    /// able to delete anything, and separate traits are how that is
    /// enforced rather than promised.
    pub(crate) outbox_retention:
        Arc<dyn proxiima_core::storage_ports::publication::PublicationRetentionPort>,
    /// Operator-only registry of outbox consumers, held under the same rule
    /// as `outbox`: retiring a consumer cancels every delivery it was owed.
    pub(crate) outbox_consumers:
        Arc<dyn proxiima_core::storage_ports::publication::PublicationConsumerPort>,
}

impl std::fmt::Debug for Booted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Booted")
            .field("blobs", &self.blobs)
            .finish_non_exhaustive()
    }
}

/// Bring the database of bundle `A` to this binary: apply every pending core
/// and spiice migration, initialize the store number and, when configured,
/// apply the runtime grants. The one write path to the schema; serving boot
/// and maintenance only validate it.
///
/// Everything that can be refused without a connection is refused first:
/// the registry freeze and the runtime-grant plan.
///
/// # Errors
///
/// [`ProxiimaError::Storage`] for connection, migration or runtime-grant
/// failures, [`ProxiimaError::Config`] for a refused grant plan, and
/// [`ProxiimaError::SchemaResetRequired`] when the target database does not
/// match the current core baseline (see `docs/contributing.md#migrations`).
pub(crate) async fn migrate<A: SpiiceBundle>(
    config: &RuntimeConfig,
    parts: &RuntimeParts,
) -> Result<(), ProxiimaError> {
    let migrators = A::migrators();
    let registry = compose_registry::<A>(config)?;
    let pg_sidecars = compose_pg_sidecars::<A>(&registry)?;
    let grants = config
        .runtime_grants
        .then(|| {
            RuntimeGrants::plan(
                &config.database_url,
                &config.platform_database_url,
                composed_schema_names(&registry),
                runtime_grants::ledger_names(&migrators),
            )
        })
        .transpose()?;
    connect_schema(
        config,
        migrators,
        &pg_sidecars,
        vector_storage(parts),
        SchemaStep::Migrate { grants },
    )
    .await
}

/// Serving storage over a validated schema, before any engine is composed.
pub(crate) struct ValidatedStorage {
    pub(crate) registry: proxiima_core::SpiiceRegistryFrozen,
    pub(crate) pg_sidecars: Arc<PgSidecarRegistryFrozen>,
    pub(crate) storage: PgStorage,
}

/// Validate the schema of bundle `A` and connect its serving storage, with
/// its sidecars and spiices wired. Issues no DDL and writes nothing: a
/// database the migrate step has not brought to this binary is refused.
///
/// # Errors
///
/// [`ProxiimaError::MigrationRequired`] for a database behind this binary or
/// without a store number, [`ProxiimaError::SchemaResetRequired`] for one off
/// the current baseline, and [`ProxiimaError::Storage`] for connection and
/// other validation failures.
pub(crate) async fn connect_storage<A: SpiiceBundle>(
    config: &RuntimeConfig,
    parts: &RuntimeParts,
) -> Result<ValidatedStorage, ProxiimaError> {
    let registry = compose_registry::<A>(config)?;
    let pg_sidecars = compose_pg_sidecars::<A>(&registry)?;
    connect_schema(
        config,
        A::migrators(),
        &pg_sidecars,
        vector_storage(parts),
        SchemaStep::Validate,
    )
    .await?;
    let pg = connect_serving_storage(config, &registry, parts.vector_index.clone()).await?;
    let pg = if let Some(participant) = parts.host_state_participant.clone() {
        pg.with_host_state_participant(participant)
    } else {
        pg
    };
    ensure_pg_sidecars(&pg, &registry, &pg_sidecars).await?;
    let storage = pg
        .with_sidecars(pg_sidecars.as_ref().clone())
        .await
        .map_err(|error| ProxiimaError::Storage(error.into()))?
        .try_with_spiices(&registry)
        .map_err(|error| ProxiimaError::Storage(error.into()))?
        .with_embedding_runtime_policy(config.embedding_runtime_policy);
    Ok(ValidatedStorage {
        registry,
        pg_sidecars,
        storage,
    })
}

/// Validate, connect, and compose the engine for bundle `A`.
///
/// Everything that can be refused without a connection is refused first:
/// the embedding width and the registry freeze.
///
/// # Errors
///
/// The errors of [`connect_storage`], [`ProxiimaError::Config`] for a
/// refused embedding client, and [`ProxiimaError::Engine`] when binding
/// runtime authorities fails.
pub(crate) async fn boot<A: SpiiceBundle>(
    config: &RuntimeConfig,
    parts: &RuntimeParts,
) -> Result<Booted, ProxiimaError> {
    let embedding_router =
        embedding_router_for(parts.embed_client.clone(), parts.embedding_router.clone())?;
    let ValidatedStorage {
        registry,
        pg_sidecars,
        storage: pg,
    } = connect_storage::<A>(config, parts).await?;
    let erase_context = pg
        .host_state_erase_context()
        .map_err(|error| ProxiimaError::Storage(error.into()))?;
    let (publication_origin_eligibility, origin_scope) = publication_origin_ports(&pg).await?;

    let pool = pg.clone_pool_for_backend();
    let platform_scope = pg.platform_scope_for_host();
    let (pg, blobs) = compose_blob_stores(pg, config.s3.clone())?;
    if let Some(store) = &blobs {
        // Stores do not move: debt owed to another one means this process
        // is configured for the wrong store. Refuse before serving.
        crate::object_maintenance::refuse_foreign_purge_debt_at_boot(&pg, store)
            .await
            .map_err(|error| ProxiimaError::Config(error.to_string()))?;
    }

    let engine = compose_engine(
        registry,
        &pg,
        config,
        embedding_router,
        parts.owner_drop_proof.clone(),
    )?;
    // The one handle on the captured outbox. Host-only: it is not in
    // `StoragePorts`, so no spiice, tool or write session can reach a
    // captured event, and a publisher task gets it from here.
    let outbox: Arc<dyn proxiima_core::storage_ports::publication::PublicationOutboxPort> =
        Arc::new(pg.clone());
    let outbox_retention: Arc<
        dyn proxiima_core::storage_ports::publication::PublicationRetentionPort,
    > = Arc::new(pg.clone());
    let outbox_consumers: Arc<
        dyn proxiima_core::storage_ports::publication::PublicationConsumerPort,
    > = Arc::new(pg.clone());
    register_outbox_consumers(config, outbox_consumers.as_ref()).await?;

    let (engine, system_authority, delegation_runtime_authority) =
        engine.into_runtime_authorities();
    let host_state_maintenance_authority = engine
        .host_state_maintenance_authority(&system_authority)
        .map_err(|error| ProxiimaError::Engine(error.into()))?;
    if let Some(store) = &blobs {
        store
            .bind_system_authority(&system_authority)
            .map_err(|error| ProxiimaError::Config(error.to_string()))?;
    }
    let engine = Arc::new(engine);
    let registry = Arc::new(engine.registry().clone());
    Ok(Booted {
        engine,
        system_authority: std::sync::Arc::new(system_authority),
        host_state_maintenance_authority,
        delegation_runtime_authority,
        pool,
        platform_scope,
        registry,
        pg_sidecars,
        erase_context,
        publication_origin_eligibility,
        origin_scope,
        blobs,
        storage: pg.clone(),
        outbox,
        outbox_retention,
        outbox_consumers,
    })
}

/// Serving authority is constructed only after the schema validated.
async fn connect_serving_storage(
    config: &RuntimeConfig,
    registry: &proxiima_core::SpiiceRegistryFrozen,
    vector_index: Option<Arc<dyn proxiima_core::storage_ports::VectorIndex>>,
) -> Result<PgStorage, ProxiimaError> {
    let schema_names = composed_schema_names(registry);
    let schema_refs: Vec<&str> = schema_names.iter().map(String::as_str).collect();
    let platform_pool = config
        .pg_pool_config
        .connect_lazy(&config.platform_database_url)
        .map_err(storage_error)?;
    let platform_scope = PgPlatformScope::new(platform_pool, &schema_refs)
        .await
        .map_err(|error| ProxiimaError::Storage(error.into()))?;
    let pg = match vector_index {
        Some(index) => {
            PgStorage::connect_with_vector_index(
                &config.database_url,
                config.pg_pool_config,
                config.pg_tuning,
                platform_scope,
                config.store_number,
                index,
            )
            .await
        }
        None => {
            PgStorage::connect_with_config(
                &config.database_url,
                config.pg_pool_config,
                config.pg_tuning,
                platform_scope,
                config.store_number,
            )
            .await
        }
    };
    pg.map_err(storage_error)
}

/// Register every configured outbox consumer, before any capture of this
/// process can fan out, so a record written from here on owes each of them a
/// delivery.
///
/// A consumer that is registered but not configured keeps receiving lanes it
/// never drains; only an explicit retire removes it, because a rolling deploy
/// briefly runs binaries with different consumer sets and must not drop a
/// backlog on the way. It is named in a warning so a forgotten one is found
/// before its lanes fill the outbox.
async fn register_outbox_consumers(
    config: &RuntimeConfig,
    port: &dyn proxiima_core::storage_ports::publication::PublicationConsumerPort,
) -> Result<(), ProxiimaError> {
    if config.outbox_consumers.is_empty() {
        return Ok(());
    }
    for name in config.outbox_consumers.names() {
        port.register(name).await.map_err(storage_error)?;
    }
    let configured: Vec<_> = config.outbox_consumers.names().collect();
    for name in port.registered().await.map_err(storage_error)? {
        if !configured.contains(&&name) {
            tracing::warn!(
                consumer = %name,
                "outbox consumer is registered but not configured in this process; \
                 its deliveries accumulate until it is configured again or retired"
            );
        }
    }
    Ok(())
}

/// The two halves of the retained-copy rule, taken from ONE database.
///
/// They are returned together because that is the invariant: the port
/// answers "does an origin row still exist", the scope answers "is this
/// message mine to ask that about", and a cleaner holding a port and a
/// scope from different databases would read a foreign stream as entirely
/// revoked. Deriving both here leaves no call site free to mix them.
///
/// An unreadable scope fails the boot. A publisher that cannot stamp
/// produces events no cleaner is ever allowed to act on — retained forever,
/// every cycle unhealthy — and entering that state silently is worse than
/// not starting. Migration 0012 writes the row, so its absence means the
/// schema was tampered with, not that a feature is switched off.
async fn publication_origin_ports(
    pg: &PgStorage,
) -> Result<(Arc<dyn PublicationOriginEligibilityPort>, OriginScope), ProxiimaError> {
    let scope = pg
        .origin_scope()
        .await
        .map_err(|error| ProxiimaError::Storage(error.into()))?;
    Ok((Arc::new(pg.clone()), scope))
}

/// What [`connect_schema`] does with the schema on the migration connection.
enum SchemaStep {
    /// Apply pending migrations, initialize the store number, then apply the
    /// planned runtime grants. Only [`migrate`] chooses it.
    Migrate { grants: Option<RuntimeGrants> },
    /// Refuse a schema that is not current, issuing no DDL and writing
    /// nothing. Serving boot and maintenance choose it.
    Validate,
}

/// Where a store of these parts keeps its vectors: in a host index when one
/// is configured, else in Postgres.
fn vector_storage(parts: &RuntimeParts) -> VectorStorage {
    match parts.vector_index {
        Some(_) => VectorStorage::HostIndex,
        None => VectorStorage::Local,
    }
}

/// Connect the migration storage and run `step` on its schema.
///
/// The migration pool connects to [`RuntimeConfig::migrations_url`]: the
/// platform role, reached directly when the serving platform pool goes
/// through a transaction-mode pooler. The migration run and the preflight
/// hold a session advisory lock and session settings for the whole run.
///
/// The runtime grants run on the migration (platform) pool after migrating;
/// a serving connect then asserts the runtime role under owner RLS.
async fn connect_schema(
    config: &RuntimeConfig,
    migrators: Vec<NamedMigrator>,
    pg_sidecars: &PgSidecarRegistryFrozen,
    vectors: VectorStorage,
    step: SchemaStep,
) -> Result<(), ProxiimaError> {
    let migration_pg = proxiima_storage_pg::PgMigrationStorage::connect_with_config(
        config.migrations_url(),
        config.pg_pool_config,
        config.pg_tuning,
    )
    .await
    .map_err(storage_error)?
    .with_vector_storage(vectors);
    let stepped = match step {
        SchemaStep::Migrate { grants } => {
            migrate_schema(
                config,
                &migration_pg,
                migrators,
                pg_sidecars,
                grants.as_ref(),
            )
            .await
        }
        SchemaStep::Validate => preflight_without_migrations_with_policy(
            &migration_pg,
            migrators,
            pg_sidecars,
            config.missing_migration_policy,
        )
        .await
        .map(drop)
        .map_err(validation_error),
    };
    migration_pg.clone_pool_for_backend().close().await;
    stepped
}

/// [`SchemaStep::Migrate`] on a connected migration storage.
async fn migrate_schema(
    config: &RuntimeConfig,
    migration_pg: &PgMigrationStorage,
    migrators: Vec<NamedMigrator>,
    pg_sidecars: &PgSidecarRegistryFrozen,
    grants: Option<&RuntimeGrants>,
) -> Result<(), ProxiimaError> {
    run_core_and_spiice_migrations_with_policy(
        migration_pg,
        migrators,
        pg_sidecars,
        config.missing_migration_policy,
    )
    .await
    .map_err(migration_error)?;
    migration_pg
        .initialize_store_number(config.store_number)
        .await
        .map_err(storage_error)?;
    if let Some(grants) = grants {
        grants.apply(&migration_pg.clone_pool_for_backend()).await?;
    }
    Ok(())
}

/// Run every linked spiice's registration callback and freeze the result.
///
/// With an outbox consumer configured, `core/goal-change-v1` is made
/// listenable before the freeze, so every goal write also feeds the outbox.
/// That makes the schema subject to the boot publication check: a consumer
/// without `PROXIIMA_PUBLICATION_SOURCE` is refused in `compose_engine`,
/// naming it.
fn compose_registry<A: SpiiceBundle>(
    config: &RuntimeConfig,
) -> Result<proxiima_core::SpiiceRegistryFrozen, ProxiimaError> {
    let mut registry = SpiiceRegistry::new();
    A::register(&mut registry)?;
    if !config.outbox_consumers.is_empty() {
        registry.publish_goal_changes();
    }
    Ok(registry.try_freeze()?)
}

/// Freeze the PG sidecar registry before connecting.
fn compose_pg_sidecars<A: SpiiceBundle>(
    registry: &proxiima_core::SpiiceRegistryFrozen,
) -> Result<Arc<PgSidecarRegistryFrozen>, ProxiimaError> {
    let mut pg_sidecars = PgSidecarRegistry::new();
    register_core_pg_sidecars(&mut pg_sidecars);
    A::register_pg_sidecars(&mut pg_sidecars);
    let pg_sidecars = pg_sidecars
        .freeze_against(registry)
        .map_err(storage_error)?;
    Ok(Arc::new(pg_sidecars))
}

/// Verify the deployed schema against the same registry used by migrations.
async fn ensure_pg_sidecars(
    pg: &PgStorage,
    registry: &proxiima_core::SpiiceRegistryFrozen,
    pg_sidecars: &PgSidecarRegistryFrozen,
) -> Result<(), ProxiimaError> {
    // The backend accessor, not `pool_for_tests`. This is a production boot
    // path on every embed; a `#[doc(hidden)]` handle whose name says "tests"
    // is not the contract it was reaching through.
    let pool = pg.clone_pool_for_backend();
    // Re-run the projection generator against the composed contracts and
    // compare it with the catalog. The migration carries the generator's
    // output verbatim; this is the half that notices a deployment whose
    // schema and whose linked spiices disagree — a spiice added without
    // its migrations, or a migration hand-edited away from the generator.
    proxiima_storage_pg::projection::ensure_projection_schema(&pool, registry.contracts())
        .await
        .map_err(storage_error)?;
    // The same half, one layer down. The projection check asks whether
    // the tables a search reads exist; this asks whether the guard that
    // keeps every registered memory sidecar reachable by forget, erase
    // and export is installed on it. Both read the catalog and issue no
    // DDL, so this remains valid for a split-role deploy, where this
    // process's role cannot create a trigger at all.
    proxiima_storage_pg::integrity::ensure_declaration_triggers(&pool, pg_sidecars)
        .await
        .map_err(storage_error)?;
    Ok(())
}

/// Wire the purge store over S3 when it is configured. A database-only host
/// keeps the in-process stub, whose `memory:` store id never matches an S3
/// store's debts: forget and restore need no object store (#488). Cited
/// uploads and the purge store must name the same store.
fn compose_blob_stores(
    pg: PgStorage,
    s3: Option<proxiima_blob_s3::S3RuntimeConfig>,
) -> Result<(PgStorage, Option<CitedBlobStore>), ProxiimaError> {
    let blobs = s3
        .map(|s3| {
            CitedBlobStore::new(
                pg.clone_pool_for_backend(),
                s3,
                pg.platform_scope_for_host(),
            )
        })
        .transpose()
        .map_err(|error| ProxiimaError::Config(error.to_string()))?;
    let pg = wire_cold_store(pg, blobs.as_ref())?;
    Ok((pg, blobs))
}

fn wire_cold_store(
    pg: PgStorage,
    blobs: Option<&CitedBlobStore>,
) -> Result<PgStorage, ProxiimaError> {
    let Some(store) = blobs else {
        return Ok(pg);
    };
    let cold = store.cold_store();
    // Assert the composition the purge queue's `object_store` column
    // depends on. Upload rows and every debt copied from them record the
    // cited store's identity, and the drain touches only rows naming the
    // wired purge store — so a purge store naming another store than the
    // one uploads publish to must fail here, at boot, not silently stop
    // reclaiming bytes.
    if ColdObjectStore::store_id(&cold) != store.store_id() {
        return Err(ProxiimaError::Config(format!(
            "object purge store is {} but cited uploads publish to {}; the purge \
             queue and the object store must name one store",
            ColdObjectStore::store_id(&cold),
            store.store_id(),
        )));
    }
    Ok(pg.with_cold(Arc::new(cold)))
}

/// Compose the engine over the frozen registry and wired storage, attaching
/// the deployment tool scope, the publication configuration, the host's
/// embedding router and the host's personal-owner drop proof.
///
/// `try_with_publication_config` is on the UNCONDITIONAL path, with the
/// parsed config even when it binds no source. That is the whole boot
/// guarantee: a bundle that freezes a listenable schema and a deployment
/// that never set `PROXIIMA_PUBLICATION_SOURCE` is refused here, naming the
/// schemas, instead of admitting writes that would each fail at capture.
fn compose_engine(
    registry: proxiima_core::SpiiceRegistryFrozen,
    pg: &PgStorage,
    config: &RuntimeConfig,
    embedding_router: Option<Arc<dyn EmbeddingRouter>>,
    owner_drop_proof: Option<OwnerDropProofHandle>,
) -> Result<Engine, ProxiimaError> {
    let mut storage_ports = Arc::new(pg.clone()).storage_ports_builder();
    if let Some(proof) = owner_drop_proof {
        storage_ports = storage_ports.owner_drop_proof(proof);
    }
    let mut engine = Engine::new(registry, storage_ports.build())
        .with_embedding_runtime_policy(config.embedding_runtime_policy)
        .try_with_publication_config(config.publication.clone())
        .map_err(|error| ProxiimaError::Config(error.to_string()))?;
    if let Some(router) = embedding_router {
        engine = engine.with_embedding_router(router);
    }
    Ok(engine)
}

/// The router the engine embeds through: the host's, or one client for
/// every Owner. Resolved before any connection, so an unusable client is a
/// boot refusal rather than a claimed-then-refused job queue.
fn embedding_router_for(
    embed_client: Option<Arc<dyn EmbeddingClient>>,
    embedding_router: Option<Arc<dyn EmbeddingRouter>>,
) -> Result<Option<Arc<dyn EmbeddingRouter>>, ProxiimaError> {
    match (embed_client, embedding_router) {
        (Some(_), Some(_)) => Err(ProxiimaError::Config(
            "set embed_client or embedding_router, not both".into(),
        )),
        (Some(client), None) => {
            // Fail fast on a width no lane indexes: jobs would be claimed
            // and then rejected at insert, silently burning the queue.
            let router = proxiima_core::llm::SingleClientRouter::bind(client)
                .map_err(|error| ProxiimaError::Config(error.to_string()))?;
            Ok(Some(Arc::new(router)))
        }
        (None, router) => Ok(router),
    }
}

/// Map a storage error onto [`ProxiimaError`], preserving the typed
/// [`StorageError::SchemaResetRequired`] and [`StorageError::MigrationRequired`]
/// signals instead of collapsing them into the generic
/// [`ProxiimaError::Storage`] string.
pub(crate) fn storage_error(err: StorageError) -> ProxiimaError {
    match err {
        StorageError::SchemaResetRequired { details } => {
            ProxiimaError::SchemaResetRequired { details }
        }
        StorageError::MigrationRequired { details } => ProxiimaError::MigrationRequired { details },
        other => ProxiimaError::Storage(other.into()),
    }
}

/// Map a migration-facade error onto [`ProxiimaError`], unwrapping the core
/// preflight check so a stale pre-lane database still surfaces as
/// [`ProxiimaError::SchemaResetRequired`], and a core or spiice lane behind
/// this binary as [`ProxiimaError::MigrationRequired`], rather than a generic
/// storage string.
fn migration_error(err: MigrationError) -> ProxiimaError {
    match err {
        MigrationError::CorePreflight(storage_err) => storage_error(storage_err),
        unapplied @ MigrationError::Ledger {
            conflict: LedgerConflict::Unapplied { .. },
            ..
        } => ProxiimaError::MigrationRequired {
            details: unapplied.to_string(),
        },
        other => ProxiimaError::Storage(other.into()),
    }
}

/// [`migration_error`] for the validating preflight, where generated sidecar
/// state the migrate step regenerates — owner-follows marks and indexes,
/// search definitions, a declared schema evolution — reports
/// [`StorageError::MigrationRequired`]: that surfaces as
/// [`ProxiimaError::MigrationRequired`] with the preflight's own message.
fn validation_error(err: MigrationError) -> ProxiimaError {
    match err {
        MigrationError::SidecarOwnerRls(StorageError::MigrationRequired { .. })
        | MigrationError::SearchDefinitions(StorageError::MigrationRequired { .. })
        | MigrationError::SchemaEvolution(StorageError::MigrationRequired { .. }) => {
            ProxiimaError::MigrationRequired {
                details: err.to_string(),
            }
        }
        other => migration_error(other),
    }
}
