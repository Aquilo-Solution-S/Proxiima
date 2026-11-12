pub mod args;
pub mod settings;
mod telemetry;

pub use args::{
    ArgsError, DEFAULT_BIND, DEFAULT_DATABASE_URL, MAINTAIN_BLOBS_USAGE, MAINTAIN_USAGE,
    MIGRATE_USAGE, MaintainBlobsConfig, MaintainConfig, McpConfig, MigrateConfig, ReconcileScope,
    STORAGE_MAINTENANCE_USAGE, StorageMaintenanceConfig, parse_args, parse_maintain_args,
    parse_maintain_blobs_args, parse_migrate_args, parse_storage_maintenance_args, usage,
};

use proxiima_blob_s3::{BlobError, CitedBlobStore, S3RuntimeConfig};

use std::collections::{BTreeSet, HashSet};
use std::num::NonZeroU32;
use std::sync::Arc;

use proxiima::spiice::SpiiceBundle;
use proxiima::{
    AppContext, AppInfo, EmbeddingRuntimePolicy, Proxiima, ProxiimaError, RunningProxiima,
    RuntimeBuilder, Setting, SpiiceApp, SpiiceServiceError, SpiiceServices,
    embedding_runtime_policy_from_lookup,
};
use proxiima_core::llm::{
    BoundEmbeddingClient, QueryInstruction, QueryInstructions, QueryTask, SingleClientRouter,
};
use proxiima_core::protocol::profile as protocol_profile;
use proxiima_core::{
    ScopeKey, ServedSurface, SpiiceRegistry, SpiiceRegistryError, all_core_actions,
    all_core_resources,
};
use proxiima_llm_openai_compat::OpenAiCompatEmbeddingClient;
use proxiima_storage_pg::{
    ColdPurgeRetryOptions, ColdPurgeRetryOutcome, EmbeddingReconcileOptions,
    EmbeddingReconcileScope, PgStorage,
};

use settings::{
    PROXIIMA_EMBED_API_KEY, PROXIIMA_EMBED_BASE_URL, PROXIIMA_EMBED_DIM, PROXIIMA_EMBED_MATRYOSHKA,
    PROXIIMA_EMBED_MAX_INPUT_CHARS, PROXIIMA_EMBED_MODEL, PROXIIMA_EMBED_QUERY_INSTRUCTION,
    PROXIIMA_TOOL_ALLOW, PROXIIMA_TOOL_DENY, PROXIIMA_TOOL_PROFILE,
};

const DEFAULT_EMBED_MAX_INPUT_CHARS: NonZeroU32 =
    NonZeroU32::new(16_384).expect("embedding input default is positive");

/// Tool/action scope keys advertised by the `memory` profile. Flat entries
/// reference the owning tool's `Tool::NAME`; grouped tools contribute
/// action leaf keys from their manifest.
///
/// Reads `all_core_actions()` because it is a curated allow-list — a
/// deployment decision about which substrate actions the memory profile
/// advertises — not an enumeration of what exists; that is
/// [`registered_tool_ids`], which reads the descriptors. The two agreeing on
/// the substrate actions they both name is pinned by
/// `core_action_meta_decorates_only_declared_actions` in
/// `crates/core/tests/spiice_registry.rs`.
fn memory_keep_set() -> Vec<&'static str> {
    use proxiima_core::Tool;
    use proxiima_core::catalog::core_tools::{
        CoreFactTool, CoreGoalTool, CoreUploadTool, DeriveTool, EpisodeCommitTool, ForgetTool,
        InterpretTool, MemorySpacesTool, RecallTool, RecordUtteranceTool, RememberTool,
        RestoreTool, SearchMemoriesTool, ThinkTool,
    };

    let mut ids = vec![
        // authoring
        RememberTool::NAME,
        EpisodeCommitTool::NAME,
        ForgetTool::NAME,
        RestoreTool::NAME,
        DeriveTool::NAME,
        InterpretTool::NAME,
        RecordUtteranceTool::NAME,
        // retrieval
        SearchMemoriesTool::NAME,
        RecallTool::NAME,
        ThinkTool::NAME,
        MemorySpacesTool::NAME,
    ];
    // The memory profile carries the full goal lifecycle, non-destructive
    // fact/citation actions, and the cited-blob upload lane (an upload is
    // memory authoring: it exists to be cited by a Fact).
    // Retention/cleanup are host/config-only and stay out.
    ids.extend(
        all_core_actions()
            .filter(|action| {
                action.tool == CoreGoalTool::NAME
                    || action.tool == CoreFactTool::NAME
                    || action.tool == CoreUploadTool::NAME
            })
            .map(|action| action.scope_key),
    );
    ids.extend(all_core_resources().map(|resource| resource.scope_key));
    ids
}

/// proxiima-mcp serves the core only: it links no spiice. A host that wants a
/// spiice composes its own app with it.
type LinkedSpiices = ();

#[derive(Debug)]
pub struct ProxiimaMcpApp;

impl SpiiceBundle for ProxiimaMcpApp {
    fn register(registry: &mut SpiiceRegistry) -> Result<(), SpiiceRegistryError> {
        <LinkedSpiices as SpiiceBundle>::register(registry)
    }

    fn register_pg_sidecars(registry: &mut proxiima::spiice::PgSidecarRegistry) {
        <LinkedSpiices as SpiiceBundle>::register_pg_sidecars(registry);
    }

    fn migrators() -> Vec<proxiima::NamedMigrator> {
        <LinkedSpiices as SpiiceBundle>::migrators()
    }
}

impl SpiiceApp for ProxiimaMcpApp {
    fn app_info() -> AppInfo {
        AppInfo {
            id: "proxiima-mcp",
            title: "Proxiima MCP",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn configure(builder: RuntimeBuilder) -> RuntimeBuilder {
        builder.database_url(DEFAULT_DATABASE_URL).mcp_bind(
            DEFAULT_BIND
                .parse()
                .expect("DEFAULT_BIND must be a valid SocketAddr"),
        )
    }

    fn settings() -> Vec<&'static Setting> {
        settings::SETTINGS.iter().collect()
    }

    fn services(_ctx: &AppContext) -> Result<SpiiceServices, SpiiceServiceError> {
        Ok(SpiiceServices::default())
    }
}

/// Headless composition used by `migrate` and the maintenance commands.
///
/// It links the same compiled spiices and migrations as the serving app but
/// deliberately contributes no MCP configuration, and [`maintenance_app`]
/// reads only the storage settings, so these commands share the server's
/// environment without serving. The maintenance commands' storage validates the schema
/// as serving does and migrates nothing. `maintain-blobs` boots it through
/// the facade, which binds the runtime-provided store and `SystemAuthority`
/// to the same boot.
struct MaintenanceApp;

impl SpiiceBundle for MaintenanceApp {
    fn register(registry: &mut SpiiceRegistry) -> Result<(), SpiiceRegistryError> {
        <ProxiimaMcpApp as SpiiceBundle>::register(registry)
    }

    fn register_pg_sidecars(registry: &mut proxiima::spiice::PgSidecarRegistry) {
        <ProxiimaMcpApp as SpiiceBundle>::register_pg_sidecars(registry);
    }

    fn migrators() -> Vec<proxiima::NamedMigrator> {
        <ProxiimaMcpApp as SpiiceBundle>::migrators()
    }
}

impl SpiiceApp for MaintenanceApp {
    fn app_info() -> AppInfo {
        AppInfo {
            id: "proxiima-mcp-maintenance",
            title: "Proxiima MCP Maintenance",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    /// Supply the required deployment policy for this maintenance runtime,
    /// which starts without an MCP listener.
    fn configure(builder: RuntimeBuilder) -> RuntimeBuilder {
        builder.served_surface(ServedSurface::Palette(Vec::new()))
    }
}

/// The frozen registry of this build: the substrate and the spiices
/// [`ProxiimaMcpApp`] links.
fn build_frozen_registry() -> Result<proxiima_core::SpiiceRegistryFrozen, ProxiimaError> {
    let mut registry = SpiiceRegistry::new();
    <ProxiimaMcpApp as SpiiceBundle>::register(&mut registry)?;
    Ok(registry.try_freeze()?)
}

/// Where the process writes its log.
#[derive(Debug, Clone, Copy)]
enum LogSink {
    /// `serve`: the log is the process output.
    Stdout,
    /// A maintenance command: stdout carries the report operators read, so
    /// the log goes beside it.
    Stderr,
}

/// Install the log output of this process, filtered by `RUST_LOG` (default
/// `info`), and warn through it about each `PROXIIMA_*` variable of the
/// process environment that no setting declares: every command reads the
/// process environment through a lookup, which cannot list its names.
fn init_logging(sink: LogSink) {
    // rmcp 3.x logs idle-session keep-alive expiry and the resulting
    // session-cleanup race at ERROR; both are clean lifecycle events
    // (`quit_reason=Closed`), so those targets are pinned to `warn`.
    let builder = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new(
                "info,rmcp::transport::worker=warn,\
                 rmcp::transport::streamable_http_server::tower=warn",
            )
        }),
    );
    match sink {
        LogSink::Stdout => builder.try_init(),
        LogSink::Stderr => builder.with_writer(std::io::stderr).try_init(),
    }
    .ok();
    proxiima::settings::warn_unknown_env(settings::SETTINGS);
}

/// The [`CliError`] of a subcommand's argument failure: a request for help
/// is answered with that subcommand's `usage`.
fn cli_error(usage: fn() -> String) -> impl Fn(ArgsError) -> CliError {
    move |err| match err {
        ArgsError::Help => CliError::Help(usage()),
        other @ ArgsError::Invalid(_) => CliError::Args(other),
    }
}

/// # Errors
///
/// Returns argument, facade boot, or runtime failures (including the MCP transport).
pub async fn run<I: IntoIterator<Item = String>>(args: I) -> Result<(), CliError> {
    let mut args: Vec<String> = args.into_iter().collect();
    if args.first().is_some_and(|arg| arg == "migrate") {
        args.remove(0);
        let config = parse_migrate_args(args).map_err(cli_error(|| MIGRATE_USAGE.to_owned()))?;
        init_logging(LogSink::Stderr);
        return run_migrate(config, &proxiima_core::process_env).await;
    }
    if args.first().is_some_and(|arg| arg == "maintain-embeddings") {
        args.remove(0);
        let config = parse_maintain_args(args).map_err(cli_error(|| MAINTAIN_USAGE.to_owned()))?;
        init_logging(LogSink::Stderr);
        return run_maintain(config, &proxiima_core::process_env).await;
    }
    if args.first().is_some_and(|arg| arg == "maintain-storage") {
        args.remove(0);
        let config = parse_storage_maintenance_args(args)
            .map_err(cli_error(|| STORAGE_MAINTENANCE_USAGE.to_owned()))?;
        init_logging(LogSink::Stderr);
        return run_maintain_storage(config, &proxiima_core::process_env).await;
    }
    if args.first().is_some_and(|arg| arg == "maintain-blobs") {
        args.remove(0);
        let config = parse_maintain_blobs_args(args)
            .map_err(cli_error(|| MAINTAIN_BLOBS_USAGE.to_owned()))?;
        init_logging(LogSink::Stderr);
        return run_maintain_blobs(config, &proxiima_core::process_env).await;
    }
    if args.first().is_some_and(|arg| arg == "serve") {
        args.remove(0);
    }
    let config = parse_args(args).map_err(cli_error(usage))?;
    init_logging(LogSink::Stdout);
    // Installed before the runtime starts, which records through the global
    // meter, and shut down after the runtime has, so its last interval is
    // exported.
    let metrics = telemetry::install(&proxiima_core::process_env)?;
    let served = serve(config).await;
    if let Some(metrics) = metrics {
        metrics.shutdown();
    }
    served
}

async fn serve(config: McpConfig) -> Result<(), CliError> {
    let running = run_with_handle(config).await?;
    let addr = running
        .mcp_addr()
        .ok_or_else(|| CliError::Runtime(ProxiimaError::Mcp("MCP listener disabled".into())))?;
    tracing::info!(addr = %addr, "proxiima-mcp listening; POST http://{addr}/mcp");
    // The runtime started every configured feature — the embedding worker
    // with an embedding client — and joins them all on the way out. This
    // binary hands the runtime no outbox intake, so captured Facts wait in
    // the outbox for a host that embeds Proxiima and supplies one (docs/18).
    running.until_shutdown_signal().await?;
    Ok(())
}

/// Bring the database to this binary: the migrate step a deployment runs
/// before it starts or replaces the servers. Serving and maintenance only
/// validate the schema it leaves.
async fn run_migrate(
    config: MigrateConfig,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<(), CliError> {
    maintenance_app(config.database_url, lookup)?
        .migrate()
        .await?;
    println!(
        "migrate: the database is current for proxiima-mcp {}",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}

/// Reconcile the blob store against the rows that name it, and print what
/// disagrees.
///
/// NO ADVISORY LOCK, unlike `maintain-embeddings`. That pass mutates —
/// it deletes orphan rows and enqueues jobs — so two of them racing would
/// duplicate work. This one only reads, so concurrent passes are merely
/// redundant, and a lock would add a failure mode (a crashed holder blocking
/// the health check) to a verb whose whole job is telling you something is
/// wrong.
///
/// EXITS 0 EVEN WHEN ARTEFACTS ARE MISSING, matching `maintain-embeddings`
/// and `maintain-storage`. A non-zero exit here would mean "the pass
/// failed", and the pass did not fail — it succeeded, and the news is bad.
/// Alert on the `missing=` number.
async fn run_maintain_blobs(
    config: MaintainBlobsConfig,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<(), CliError> {
    let s3 = required_s3(lookup).map_err(|err| {
        ProxiimaError::Config(format!(
            "maintain-blobs needs the host's PROXIIMA_S3_* block: {err}"
        ))
    })?;
    let built = maintenance_app(config.database_url, lookup)?
        .configure(|b| b.s3(s3))
        .build()
        .await?;
    let outcome = match built.blobs() {
        Some(store) => store
            .reconcile_all(built.system_authority())
            .await
            .map_err(|err| ProxiimaError::Storage(err.into())),
        None => Err(ProxiimaError::Config(
            "maintain-blobs booted without its required S3 store".into(),
        )),
    };
    built.shutdown().await;
    let outcome = outcome?;

    println!(
        "reconcile-blobs: rows={} objects={} missing={} orphans={} owed={} newly-owed={} stale-transfers={} foreign={}",
        outcome.rows_scanned,
        outcome.objects_scanned,
        outcome.missing_objects,
        outcome.orphan_objects,
        outcome.owed_objects,
        outcome.newly_owed_objects,
        outcome.stale_transfer_objects,
        outcome.foreign_locators,
    );
    // The samples are what an operator acts on, and they are bounded by
    // `MAX_RECONCILE_SAMPLE` rather than by anything decided here.
    for missing in &outcome.missing_sample {
        println!(
            "  missing: {} ({} bytes, {}) cited_object_id={}",
            missing.object_key, missing.byte_len, missing.filename, missing.cited_object_id,
        );
    }
    for orphan in &outcome.orphan_sample {
        println!("  orphan: {orphan}");
    }
    for stale in &outcome.stale_transfer_sample {
        println!("  stale-transfer: {stale}");
    }
    for foreign in &outcome.foreign_sample {
        println!("  foreign: {foreign}");
    }
    if outcome.is_intact() {
        println!("every artefact this store is responsible for has its object");
    }
    Ok(())
}

/// One embedding self-healing pass: orphan sweep, reconcile enqueue,
/// health report. Serialized across processes by a
/// maintenance lease — an overlapping pass skips with exit 0 so cron
/// overlap is harmless by construction, not by scheduling discipline.
async fn run_maintain(
    config: MaintainConfig,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<(), CliError> {
    let space = maintenance_embedding_space(config.model, lookup)?;
    let storage = maintenance_app(config.database_url, lookup)?
        .connect_storage()
        .await?;

    let Some(lock) = storage
        .try_embedding_maintenance_lock()
        .await
        .map_err(|err| ProxiimaError::Storage(err.into()))?
    else {
        println!("skipped: another embedding maintenance pass holds the lease");
        return Ok(());
    };

    let pass: Result<(), CliError> = async {
        let sweep = storage
            .sweep_orphan_embedding_rows()
            .await
            .map_err(|err| ProxiimaError::Storage(err.into()))?;
        println!(
            "orphan-sweep: embeddings={} heads={} jobs={}",
            sweep.embeddings_deleted, sweep.heads_deleted, sweep.jobs_deleted
        );

        // The non-embeddable schemas are those this build declares: core's
        // own (`core/upload-v1`) and the spiices it links. A host that links
        // other spiices must reconcile through its own binary, or this pass
        // would re-enqueue theirs. Printed below, so the scope of the pass is
        // visible in its own output.
        let registry = build_frozen_registry()?;
        let non_embeddable = registry.non_embeddable_schema_ids();
        let outcome = storage
            .reconcile_embeddings(EmbeddingReconcileOptions {
                space: &space,
                owners: None,
                scope: reconcile_scope(config.scope),
                // Omitted `--limit` is the operator full pass. Process boot
                // never takes this path; it uses EMBEDDING_RECONCILE_DEFAULT_LIMIT.
                limit: Some(config.limit.unwrap_or(i64::MAX)),
                non_embeddable_schemas: non_embeddable,
            })
            .await
            .map_err(|err| ProxiimaError::Storage(err.into()))?;
        println!(
            "reconcile: scanned={} enqueued={} skipped={} not-embeddable=[{}]",
            outcome.scanned,
            outcome.enqueued,
            outcome.skipped,
            non_embeddable.join(", ")
        );

        let health = storage
            .embedding_ann_observability()
            .await
            .map_err(|err| ProxiimaError::Storage(err.into()))?;
        println!(
            "backlog: pending={} processing={} failed={} stale-processing={}",
            health.backlog.pending,
            health.backlog.processing,
            health.backlog.failed,
            health.stale_processing_jobs
        );
        println!(
            "orphans-remaining: embeddings={} heads={} jobs={}",
            health.orphan_rows.embeddings, health.orphan_rows.heads, health.orphan_rows.jobs
        );
        match health.recall_canary {
            Some(canary) => {
                println!(
                    "recall-canary: samples={}/{} recall@{}={:.3} minimum={:.3}",
                    canary.sample_count,
                    canary.available_strata,
                    canary.k,
                    canary.recall_at_k,
                    canary.minimum_recall_at_k
                );
                for stratum in canary.strata {
                    println!(
                        "  model={} dim={} samples={} recall={:.3} minimum={:.3}",
                        stratum.model_id,
                        stratum.dim,
                        stratum.sample_count,
                        stratum.recall_at_k,
                        stratum.minimum_recall_at_k
                    );
                }
            }
            None => println!("recall-canary: none (no embeddings yet)"),
        }
        Ok(())
    }
    .await;
    if let Err(err) = lock.release().await {
        tracing::debug!(error = %err, "embedding maintenance lease release failed; it expires instead");
    }
    pass
}

/// The runtime of `migrate` and the maintenance commands on `database_url`:
/// configured from the storage settings of `lookup` only, so the server's
/// own environment neither starts nor refuses anything here.
fn maintenance_app(
    database_url: String,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<Proxiima<MaintenanceApp>, CliError> {
    Ok(Proxiima::<MaintenanceApp>::app()
        .from_storage_lookup(lookup)?
        .configure(|b| b.database_url(database_url)))
}

/// The `PROXIIMA_S3_*` block a command that cannot run without it requires.
fn required_s3(lookup: &impl Fn(&str) -> Option<String>) -> Result<S3RuntimeConfig, BlobError> {
    S3RuntimeConfig::from_lookup(lookup)?
        .ok_or_else(|| BlobError::Config("missing PROXIIMA_S3_BUCKET".to_string()))
}

/// One storage-maintenance pass: the expired-upload sweep, when
/// `--sweep-expired-uploads` asks for it, then the cold-object purge drain,
/// when `--retry-cold-object-purges` does. The same lease serializes it with
/// the in-process drain of every Proxiima server on this database. Serialized across
/// processes by its own maintenance lease — an overlapping pass skips
/// with exit 0, like `maintain-embeddings`.
async fn run_maintain_storage(
    config: StorageMaintenanceConfig,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<(), CliError> {
    let s3 = ((config.retry_cold_object_purges && !config.dry_run) || config.sweep_expired_uploads)
        .then(|| {
            required_s3(lookup).map_err(|err| {
                ProxiimaError::Config(format!(
                    "storage maintenance object-store actions need the host's \
                     PROXIIMA_S3_* block: {err}"
                ))
            })
        })
        .transpose()?;

    let storage = maintenance_app(config.database_url.clone(), lookup)?
        .connect_storage()
        .await?;

    let (storage, sweeper) = if let Some(s3) = s3 {
        let registry = build_frozen_registry()?;
        let blobs = CitedBlobStore::new(
            storage.clone_pool_for_backend(),
            s3,
            storage.platform_scope_for_host(),
        )
        .map_err(|err| ProxiimaError::Config(err.to_string()))?;
        // This command is its own boot: its engine's authority is the one
        // the store answers host-wide operations to.
        let (_, authority) =
            proxiima::Engine::new(registry, Arc::new(storage.clone()).storage_ports())
                .into_system_authority();
        blobs
            .bind_system_authority(&authority)
            .map_err(|err| ProxiimaError::Config(err.to_string()))?;
        let storage = storage.with_cold(Arc::new(blobs.cold_store()));
        (storage, Some((blobs, authority)))
    } else {
        (storage, None)
    };

    let Some(lock) = storage
        .try_storage_maintenance_lock()
        .await
        .map_err(|err| ProxiimaError::Storage(err.into()))?
    else {
        println!("skipped: another storage maintenance pass holds the lease");
        return Ok(());
    };

    // Release the lease on every exit, so the next cron fire need not wait
    // for it to expire.
    let pass = storage_maintenance_steps(&config, &storage, sweeper.as_ref(), &lock).await;
    if let Err(err) = lock.release().await {
        tracing::debug!(error = %err, "storage maintenance lease release failed; it expires instead");
    }
    pass
}

/// The steps of one storage-maintenance pass, run under its lease.
async fn storage_maintenance_steps(
    config: &StorageMaintenanceConfig,
    storage: &PgStorage,
    sweeper: Option<&(CitedBlobStore, proxiima_core::authz::SystemAuthority)>,
    lock: &proxiima_storage_pg::StorageMaintenanceLock,
) -> Result<(), CliError> {
    let dry_run_suffix = if config.dry_run { " (dry-run)" } else { "" };

    if config.sweep_expired_uploads {
        let (blobs, authority) = sweeper.ok_or_else(|| {
            ProxiimaError::Config("the expired-upload sweep needs the PROXIIMA_S3_* block".into())
        })?;
        let limit = config.batch_size.get();
        let mut expired = 0_u64;
        let mut owed = 0_u64;
        let remaining = loop {
            lock.renew()
                .await
                .map_err(|err| ProxiimaError::Storage(err.into()))?;
            let batch = blobs
                .sweep_expired_uploads(authority, limit)
                .await
                .map_err(|err| ProxiimaError::Storage(err.into()))?;
            expired += batch.expired;
            owed += batch.owed;
            if batch.expired == 0 || batch.remaining == 0 {
                break batch.remaining;
            }
        };
        println!("upload-expiry-sweep: expired={expired} owed={owed} remaining={remaining}");
    }

    if config.retry_cold_object_purges {
        lock.renew()
            .await
            .map_err(|err| ProxiimaError::Storage(err.into()))?;
        let outcome = storage
            .retry_cold_object_purges(
                ColdPurgeRetryOptions::default()
                    .batch_size(i64::from(config.batch_size.get()))
                    .dry_run(config.dry_run),
            )
            .await
            .map_err(|err| ProxiimaError::Storage(err.into()))?;
        for line in cold_purge_retry_report(&outcome, dry_run_suffix) {
            println!("{line}");
        }
        if outcome.foreign > 0 {
            // The drain left them in place; the operator still has to wire
            // the store they are owed to, so this pass is not a success.
            return Err(ProxiimaError::Storage(
                format!(
                    "purge queue holds {} debt(s) owed to object stores this process is not \
                     wired to; run the drain with the PROXIIMA_S3_* settings (or \
                     PROXIIMA_S3_STORE_ID) of the store each is owed to",
                    outcome.foreign
                )
                .into(),
            )
            .into());
        }
    }
    Ok(())
}

/// The report lines for one drain: the counts, then each stuck debt and each
/// other store owed debt.
fn cold_purge_retry_report(outcome: &ColdPurgeRetryOutcome, dry_run_suffix: &str) -> Vec<String> {
    let mut lines = vec![format!(
        "cold-object-purge: selected={} purged={} absent={} failed={} due={} deferred={} \
         stuck={} foreign={}{dry_run_suffix}",
        outcome.selected,
        outcome.purged,
        outcome.absent,
        outcome.failed,
        outcome.due,
        outcome.deferred,
        outcome.stuck_count,
        outcome.foreign,
    )];
    for debt in &outcome.stuck {
        lines.push(stuck_debt_line(
            &debt.object_key,
            debt.owner_id.as_ref(),
            debt.attempts,
            &debt.next_attempt_at,
            debt.last_error.as_deref(),
        ));
    }
    for (store, count) in &outcome.foreign_stores {
        lines.push(format!(
            "cold-object-purge: foreign store={store} debts={count}"
        ));
    }
    lines
}

/// One stuck debt, named with its owner (`-` once the owner is erased) and
/// its last error.
fn stuck_debt_line(
    object_key: &str,
    owner: Option<&proxiima_core::OwnerId>,
    attempts: u32,
    next_attempt_at: &dyn std::fmt::Display,
    last_error: Option<&str>,
) -> String {
    let owner = owner.map_or_else(|| "-".to_owned(), ToString::to_string);
    format!(
        "cold-object-purge: stuck key={object_key} owner={owner} attempts={attempts} \
         next_attempt_at={next_attempt_at} last_error={}",
        last_error.unwrap_or("-"),
    )
}

fn reconcile_scope(scope: ReconcileScope) -> EmbeddingReconcileScope {
    match scope {
        ReconcileScope::MissingOnly => EmbeddingReconcileScope::MissingOnly,
        ReconcileScope::IncludeStale => EmbeddingReconcileScope::IncludeStale,
        ReconcileScope::Since(since) => EmbeddingReconcileScope::Since(since),
    }
}

/// # Errors
///
/// Returns facade config, storage, schema validation, engine, bind, or
/// transport failures.
pub async fn run_with_handle(config: McpConfig) -> Result<RunningProxiima, CliError> {
    let app = build_app(config, proxiima_core::process_env)?;
    app.run().await.map_err(Into::into)
}

fn build_app(
    config: McpConfig,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Proxiima<ProxiimaMcpApp>, CliError> {
    let registered = registered_tool_ids()?;
    let served_surface = served_surface_from_env(&lookup, &registered)?;
    let embedding_policy = embedding_runtime_policy_from_lookup(&lookup)?;
    let mut app = Proxiima::<ProxiimaMcpApp>::app()
        .from_lookup(&lookup)?
        .configure(|b| {
            b.database_url(config.database_url)
                .served_surface(served_surface)
                .embedding_runtime_policy(embedding_policy)
        });
    if let Some(bind) = config.bind {
        app = app.configure(|b| b.mcp_bind(bind));
    }
    if let Some(client) = embedding_client_from_env(&lookup, embedding_policy)? {
        let router = Arc::new(embedding_router_from_env(&lookup, client)?);
        app = app.configure(|b| b.embedding_router(router));
    }
    Ok(app)
}

/// Every Owner embedded through `client`, its search queries worded by the
/// configured query instruction.
fn embedding_router_from_env(
    lookup: &impl Fn(&str) -> Option<String>,
    client: OpenAiCompatEmbeddingClient,
) -> Result<SingleClientRouter, CliError> {
    let bound = BoundEmbeddingClient::bind(Arc::new(client))
        .map_err(|err| CliError::Runtime(ProxiimaError::Config(err.to_string())))?
        .with_query_instructions(query_instructions_from_env(lookup)?);
    Ok(SingleClientRouter::new(bound))
}

/// The instruction `PROXIIMA_EMBED_QUERY_INSTRUCTION` wraps every search
/// query in before it is embedded. Unset embeds the query as sent.
fn query_instructions_from_env(
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<QueryInstructions, CliError> {
    let mut instructions = QueryInstructions::default();
    if let Some(instruction) =
        query_instruction_from_env(lookup, &PROXIIMA_EMBED_QUERY_INSTRUCTION)?
    {
        instructions = instructions.with_instruction(QueryTask::DEFAULT, instruction);
    }
    Ok(instructions)
}

/// One query instruction template. `\n` in the value is a newline: formats
/// such as Qwen3's need one, and an environment value cannot easily hold it.
fn query_instruction_from_env(
    lookup: &impl Fn(&str) -> Option<String>,
    key: &Setting,
) -> Result<Option<QueryInstruction>, CliError> {
    key.read(lookup)
        .map(|raw| {
            QueryInstruction::new(raw.replace("\\n", "\n"))
                .map_err(|err| CliError::Runtime(ProxiimaError::Config(format!("{key}: {err}"))))
        })
        .transpose()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolProfile {
    Full,
    Memory,
}

/// Every key deployment configuration may name: one id per flat tool, one
/// `tool:action` leaf per dispatcher action, plus the resource keys.
///
/// The descriptor's `action_arg_specs` are the enumeration, so a spiice
/// dispatcher contributes one leaf per action rather than a single bare id.
/// That is what lets `PROXIIMA_TOOL_ALLOW` name one of its leaves and
/// `PROXIIMA_TOOL_PROFILE=full` grant it leaf by leaf, exactly as it does for
/// a substrate dispatcher.
fn registered_tool_ids() -> Result<RegisteredKeys, CliError> {
    let frozen = build_frozen_registry()?;
    Ok(RegisteredKeys {
        all: proxiima_core::canonical_scope_keys(&frozen),
        served_reads: proxiima_core::canonical_served_read_keys(&frozen),
    })
}

/// What this build registers, as the env profile is resolved against it.
#[derive(Debug, Default)]
struct RegisteredKeys {
    /// Every canonical key: what `PROXIIMA_TOOL_ALLOW` and `PROXIIMA_TOOL_DENY`
    /// may name.
    all: Vec<ScopeKey>,
    /// Every declared read and core resource: a
    /// `PROXIIMA_TOOL_DENY` entry naming one is refused.
    served_reads: Vec<ScopeKey>,
}

fn served_surface_from_env(
    lookup: &impl Fn(&str) -> Option<String>,
    registered: &RegisteredKeys,
) -> Result<ServedSurface, CliError> {
    resolve_served_surface(
        PROXIIMA_TOOL_PROFILE.read(lookup).as_deref(),
        PROXIIMA_TOOL_ALLOW.read(lookup).as_deref(),
        PROXIIMA_TOOL_DENY.read(lookup).as_deref(),
        registered,
    )
}

fn resolve_served_surface(
    profile_name: Option<&str>,
    allow_raw: Option<&str>,
    deny_raw: Option<&str>,
    registered: &RegisteredKeys,
) -> Result<ServedSurface, CliError> {
    // Fail closed. With no PROXIIMA_TOOL_PROFILE set, default to `memory` —
    // which excludes owner transfer and membership changes. Declared reads
    // remain served. Operators opt into the dangerous surface with
    // an explicit PROXIIMA_TOOL_PROFILE=full.
    let profile = match profile_name {
        Some(name) => parse_tool_profile(name)?,
        None => ToolProfile::Memory,
    };
    let registered_ids = registered.all.as_slice();
    let allow = parse_tool_id_csv(allow_raw, &PROXIIMA_TOOL_ALLOW)?;
    let deny = parse_tool_id_csv(deny_raw, &PROXIIMA_TOOL_DENY)?;
    reject_unknown_tool_ids(&allow, registered_ids, &PROXIIMA_TOOL_ALLOW)?;
    reject_unknown_tool_ids(&deny, registered_ids, &PROXIIMA_TOOL_DENY)?;
    reject_served_read_denials(&deny, &registered.served_reads)?;

    if profile == ToolProfile::Full && allow.is_empty() && deny.is_empty() {
        tracing::warn!(
            "PROXIIMA_TOOL_PROFILE=full advertises the whole tool surface, including \
             core_transfer (owner transfer of a memory series) and membership changes; \
             set PROXIIMA_TOOL_PROFILE=memory for agent-facing hosts"
        );
        return Ok(ServedSurface::All);
    }

    let mut palette: BTreeSet<ScopeKey> = match profile {
        ToolProfile::Full => registered_ids.iter().cloned().collect(),
        ToolProfile::Memory => keep_set_keys(memory_keep_set()),
    };
    palette.extend(allow);
    for key in deny {
        palette.remove(&key);
    }
    Ok(ServedSurface::Palette(palette.into_iter().collect()))
}

fn parse_tool_profile(raw: &str) -> Result<ToolProfile, CliError> {
    match raw.trim() {
        protocol_profile::FULL => Ok(ToolProfile::Full),
        protocol_profile::MEMORY => Ok(ToolProfile::Memory),
        other => Err(CliError::Runtime(ProxiimaError::Config(format!(
            "unknown {PROXIIMA_TOOL_PROFILE} {other:?}; expected \"{}\" or \"{}\"",
            protocol_profile::FULL,
            protocol_profile::MEMORY,
        )))),
    }
}

/// A keep set is code: constants of the tools, actions and resources it
/// names. They are keys by construction, and the profile tests resolve every
/// one of them.
fn keep_set_keys(ids: impl IntoIterator<Item = &'static str>) -> BTreeSet<ScopeKey> {
    ids.into_iter()
        .map(|id| ScopeKey::parse(id).expect("a keep-set id is a scope key"))
        .collect()
}

/// The comma-separated ids of `env_var`, each parsed into a [`ScopeKey`]
/// where it enters the process.
///
/// # Errors
///
/// A config error naming the entry that is not a scope key.
fn parse_tool_id_csv(raw: Option<&str>, env_var: &Setting) -> Result<Vec<ScopeKey>, CliError> {
    raw.map_or_else(
        || Ok(Vec::new()),
        |raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(|id| {
                    ScopeKey::parse(id).map_err(|reason| {
                        CliError::Runtime(ProxiimaError::Config(format!(
                            "{env_var} contains invalid tool id {id:?}: {reason}"
                        )))
                    })
                })
                .collect()
        },
    )
}

/// Fail closed on a `PROXIIMA_TOOL_ALLOW`/`PROXIIMA_TOOL_DENY` entry that does
/// not name a registered tool/action/resource id in this build (e.g. a
/// typo). A silently-ignored unknown `DENY` entry is a fail-open deployment
/// bug: the operator believes a tool is disabled when it never was.
fn reject_unknown_tool_ids(
    ids: &[ScopeKey],
    registered_ids: &[ScopeKey],
    env_var: &Setting,
) -> Result<(), CliError> {
    let registered: HashSet<&ScopeKey> = registered_ids.iter().collect();
    for id in ids {
        if !registered.contains(id) {
            let id = id.to_string();
            return Err(CliError::Runtime(ProxiimaError::Config(format!(
                "{env_var} contains unknown tool id {id:?}; not registered in this build"
            ))));
        }
    }
    Ok(())
}

/// Refuse a deployment deny entry naming a mandatory declared read or resource.
/// Accepting it would imply the deployment had disabled an operation it serves.
fn reject_served_read_denials(
    deny: &[ScopeKey],
    served_reads: &[ScopeKey],
) -> Result<(), CliError> {
    match deny.iter().find(|id| served_reads.contains(id)) {
        Some(id) => Err(CliError::Runtime(ProxiimaError::Config(format!(
            "{PROXIIMA_TOOL_DENY} contains {:?}, a mandatory read: a served surface narrows configured commands, never \
             declared reads, so denying it would disable nothing",
            id.to_string()
        )))),
        None => Ok(()),
    }
}

/// Build the embedding client from env, or `None` for degraded mode.
///
/// Any OpenAI-compatible `/embeddings` endpoint. Base URL and model are
/// explicit; hosted endpoints may also set `PROXIIMA_EMBED_API_KEY`.
///
/// The model's vectors must be `PROXIIMA_EMBED_DIM` wide (default 1024), and
/// that width must be one the store indexes (see
/// [`proxiima_core::llm::EmbeddingDim`]). Set `PROXIIMA_EMBED_MATRYOSHKA=true`
/// for a nested-prefix model (qwen3-embedding, text-embedding-3-*) so the
/// request asks for that width rather than the model's native one.
///
/// `PROXIIMA_EMBED_MAX_INPUT_CHARS` bounds what is *sent*. It defaults to
/// 16,384 characters; set it explicitly when a provider needs a different
/// client-side limit. See docs/10 §Bounding embedding input.
fn embedding_client_from_env(
    lookup: impl Fn(&str) -> Option<String>,
    policy: EmbeddingRuntimePolicy,
) -> Result<Option<OpenAiCompatEmbeddingClient>, CliError> {
    // Parsed first so a wildcard or range in the setting fails boot whether
    // or not embeddings are configured.
    let plaintext_hosts = proxiima_core::PlaintextHosts::from_lookup(&lookup)
        .map_err(|err| CliError::Runtime(ProxiimaError::Config(err.to_string())))?;
    let api_key = PROXIIMA_EMBED_API_KEY.read(&lookup);
    let base_url = PROXIIMA_EMBED_BASE_URL.read(&lookup);
    let model = embedding_model_from_env(&lookup);
    let matryoshka_is_set = PROXIIMA_EMBED_MATRYOSHKA.read(&lookup).is_some();
    let dim_is_set = PROXIIMA_EMBED_DIM.read(&lookup).is_some();
    let max_input_is_set = PROXIIMA_EMBED_MAX_INPUT_CHARS.read(&lookup).is_some();
    let query_instruction_is_set = PROXIIMA_EMBED_QUERY_INSTRUCTION.read(&lookup).is_some();

    if base_url.is_none()
        && model.is_none()
        && api_key.is_none()
        && !matryoshka_is_set
        && !dim_is_set
        && !max_input_is_set
        && !query_instruction_is_set
    {
        return Ok(None);
    }

    let base_url = base_url.ok_or_else(|| {
        CliError::Runtime(ProxiimaError::Config(
            "PROXIIMA_EMBED_BASE_URL is required when any PROXIIMA_EMBED_* setting is set".into(),
        ))
    })?;
    let model = model.ok_or_else(|| {
        CliError::Runtime(ProxiimaError::Config(
            "PROXIIMA_EMBED_MODEL is required when any PROXIIMA_EMBED_* setting is set".into(),
        ))
    })?;

    let matryoshka = parse_bool_env(&lookup, &PROXIIMA_EMBED_MATRYOSHKA)?;
    let dim = embedding_dim_from_env(&lookup)?.width_u32();

    let max_input_chars = parse_positive_u32_env(&lookup, &PROXIIMA_EMBED_MAX_INPUT_CHARS)?
        .unwrap_or(DEFAULT_EMBED_MAX_INPUT_CHARS);
    let caps = proxiima_core::models::EmbedCaps::new(dim, matryoshka)
        .with_max_input_chars(max_input_chars);

    OpenAiCompatEmbeddingClient::new(
        model,
        caps,
        proxiima_llm_openai_compat::OpenAiCompatConfig::new(base_url, api_key)
            .with_timeout(policy.request_timeout())
            .with_plaintext_hosts(plaintext_hosts),
    )
    .map(Some)
    .map_err(|err| CliError::Runtime(ProxiimaError::Config(err.to_string())))
}

/// Parse an optional positive integer setting.
///
/// Zero is refused rather than treated as "unset": a cap of zero would
/// reject every input, and the omitted variable uses the finite default.
fn parse_positive_u32_env(
    lookup: &impl Fn(&str) -> Option<String>,
    key: &Setting,
) -> Result<Option<NonZeroU32>, CliError> {
    let Some(raw) = key.read(lookup) else {
        return Ok(None);
    };
    raw.parse::<NonZeroU32>().map(Some).map_err(|_| {
        CliError::Runtime(ProxiimaError::Config(format!(
            "{key} must be a positive integer, got {raw:?}"
        )))
    })
}

fn parse_bool_env(
    lookup: &impl Fn(&str) -> Option<String>,
    key: &Setting,
) -> Result<bool, CliError> {
    match key.read(lookup) {
        None => Ok(false),
        Some(raw) => proxiima_core::parse_bool_word(&raw).ok_or_else(|| {
            CliError::Runtime(ProxiimaError::Config(format!(
                "{key} must be a boolean, got {raw:?}"
            )))
        }),
    }
}

fn embedding_model_from_env(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    PROXIIMA_EMBED_MODEL.read(&lookup)
}

/// `PROXIIMA_EMBED_DIM`, or 1024 when unset.
fn embedding_dim_from_env(
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<proxiima_core::llm::EmbeddingDim, CliError> {
    let Some(raw) = PROXIIMA_EMBED_DIM.read(lookup) else {
        return Ok(proxiima_core::llm::EmbeddingDim::D1024);
    };
    let width = raw.parse::<usize>().map_err(|_| {
        CliError::Runtime(ProxiimaError::Config(format!(
            "{PROXIIMA_EMBED_DIM} must be an integer, got {raw:?}"
        )))
    })?;
    proxiima_core::llm::EmbeddingDim::try_from(width).map_err(|err| {
        CliError::Runtime(ProxiimaError::Config(format!(
            "{PROXIIMA_EMBED_DIM}: {err}"
        )))
    })
}

/// The space a maintenance pass reconciles: `--model` or
/// `PROXIIMA_EMBED_MODEL`, at `PROXIIMA_EMBED_DIM`.
fn maintenance_embedding_space(
    explicit: Option<String>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<proxiima_core::EmbeddingSpace, CliError> {
    let dim = embedding_dim_from_env(&lookup)?;
    let model = explicit
        .or_else(|| embedding_model_from_env(lookup))
        .ok_or_else(|| {
            CliError::Runtime(ProxiimaError::Config(
                "maintain-embeddings requires --model or PROXIIMA_EMBED_MODEL".into(),
            ))
        })?;
    Ok(proxiima_core::EmbeddingSpace::new(model, dim))
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("help requested")]
    Help(String),
    #[error(transparent)]
    Args(#[from] ArgsError),
    #[error(transparent)]
    Runtime(#[from] ProxiimaError),
}

impl CliError {
    #[must_use]
    pub fn is_help(&self) -> bool {
        match self {
            Self::Help(_) => true,
            Self::Args(args) => args.is_help(),
            Self::Runtime(_) => false,
        }
    }

    #[must_use]
    pub fn help_text(&self) -> Option<String> {
        match self {
            Self::Help(usage) => Some(usage.clone()),
            Self::Args(args) if args.is_help() => Some(usage()),
            Self::Args(_) | Self::Runtime(_) => None,
        }
    }
}

#[cfg(test)]
mod tests;
