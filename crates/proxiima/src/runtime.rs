use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;

use axum::Router;
use proxiima_blob_s3::CitedBlobStore;
use proxiima_core::authz::SystemAuthority;
use proxiima_core::storage_ports::{
    CitedBlobDeliveryBindingError, CitedBlobDeliveryService, CitedBlobOwnerReconcileService,
    CitedBlobReadService, CitedBlobService, DelegatedAuthorityService,
};
use proxiima_core::{
    Authenticator, AuthzContext, DelegationRuntimeAuthority, OwnerAccessPort, RevalidationConfig,
    ServedSurface, SpiiceRegistryFrozen, SpiiceServices,
};
use proxiima_mcp_server::{
    HostAllowlist, McpEdgeAuth, McpStreamableService, McpToolHost, OriginAllowlist,
    assert_loopback, default_allowlist, guard_listener,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::boot::Booted;
use crate::features::{
    BootReport, Feature, FeatureControl, FeatureDecision, FeatureInputs, Features,
};
use crate::owner_access::ForwarderPolicy;
use crate::runtime_config::cause_with;
use crate::{AppContext, ProxiimaError, ProxiimaHost, RuntimeBuilder, SpiiceApp};
#[cfg(test)]
use proxiima_storage_pg::PgPlatformScope;
use proxiima_storage_pg::{PgDelegationStore, PgOwnerAccessResolver};

/// Application runtime facade.
///
/// Configuration layers, lowest first: [`SpiiceApp::configure`] (the app's
/// defaults), the environment ([`Self::from_env`] / [`Self::from_lookup`]),
/// then this value's own builder calls — code wins. Every feature starts
/// when its config is present (docs/configuration.md §Runtime features); [`Self::build`]
/// and [`Self::run`] start the same ones.
pub struct Proxiima<A: SpiiceApp> {
    overlay: RuntimeBuilder,
    use_env: bool,
    injected_env: Option<RuntimeBuilder>,
    _app: PhantomData<A>,
}

impl<A: SpiiceApp> std::fmt::Debug for Proxiima<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxiima")
            .field("overlay", &self.overlay)
            .field("use_env", &self.use_env)
            .field("has_injected_env", &self.injected_env.is_some())
            .finish()
    }
}

impl<A: SpiiceApp + 'static> Proxiima<A> {
    #[must_use]
    pub fn app() -> Self {
        Self {
            overlay: RuntimeBuilder::default(),
            use_env: false,
            injected_env: None,
            _app: PhantomData,
        }
    }

    /// Resolve the environment layer from the process environment.
    ///
    /// Logs one warning for each `PROXIIMA_*` variable that neither the
    /// facade nor [`SpiiceApp::settings`] declares
    /// ([`crate::settings::warn_unknown_env`]).
    #[must_use]
    pub fn from_env(mut self) -> Self {
        crate::settings::warn_unknown_env(A::settings());
        self.use_env = true;
        self.injected_env = None;
        self
    }

    /// Resolve the environment layer through an injected lookup.
    ///
    /// This is the process-env-equivalent path for hosts whose configuration
    /// source is not global process state. Resolution happens once; storage
    /// boot consumes the resulting `RuntimeConfig` without another env read.
    ///
    /// # Errors
    ///
    /// Returns [`ProxiimaError::Config`] when a supplied value is malformed.
    pub fn from_lookup(
        mut self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ProxiimaError> {
        self.injected_env = Some(RuntimeBuilder::default().apply_lookup(lookup)?);
        self.use_env = false;
        Ok(self)
    }

    /// [`Self::from_lookup`] for a command that migrates or maintains
    /// storage and serves nothing: only the storage settings
    /// ([`RuntimeBuilder::apply_storage_lookup`]) are read, so such a command
    /// shares the server's environment without starting a listener, building
    /// an authenticator or delivering the outbox, and without being refused
    /// by a setting only serving checks. The object store is not read: a
    /// command that needs it sets it with [`RuntimeBuilder::s3`].
    ///
    /// # Errors
    ///
    /// Returns [`ProxiimaError::Config`] when a storage value is malformed.
    pub fn from_storage_lookup(
        mut self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ProxiimaError> {
        self.injected_env = Some(RuntimeBuilder::default().apply_storage_lookup(lookup)?);
        self.use_env = false;
        Ok(self)
    }

    /// Set runtime options through the [`RuntimeBuilder`]: the closure
    /// receives the options set so far and returns them with its own added,
    /// like [`SpiiceApp::configure`]. Options set here win over the
    /// environment layer and over [`SpiiceApp::configure`]; calling it again
    /// adds to the earlier calls.
    #[must_use]
    pub fn configure(mut self, f: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder) -> Self {
        self.overlay = f(self.overlay);
        self
    }

    /// Bring the database to this binary: the host's migrate step.
    ///
    /// Resolves the configuration as [`Self::build`] does, then, on
    /// [`crate::RuntimeConfig::migrations_url`], applies every pending core and
    /// spiice migration under the configured missing-migration policy,
    /// initializes the store number, and applies the runtime grants when
    /// [`RuntimeBuilder::runtime_grants`] or `PROXIIMA_RUNTIME_GRANTS` asks for them.
    /// Serving ([`Self::build`], [`Self::run`]) and [`Self::connect_storage`]
    /// only validate, and refuse a database this step has not brought
    /// current with [`ProxiimaError::MigrationRequired`].
    ///
    /// # Errors
    ///
    /// Returns config errors, [`ProxiimaError::SchemaResetRequired`] for a
    /// database off the current baseline, and storage errors for a failed
    /// connection, migration, store number or grant.
    pub async fn migrate(&self) -> Result<(), ProxiimaError> {
        let (config, parts) = self.resolve()?;
        crate::boot::migrate::<A>(&config, &parts).await
    }

    /// Resolve the configuration as [`Self::build`] does, validate the
    /// schema, and connect the serving storage, without composing an engine
    /// or starting anything: for a one-shot maintenance command.
    ///
    /// # Errors
    ///
    /// Returns config errors, [`ProxiimaError::MigrationRequired`] for a
    /// database [`Self::migrate`] has not brought to this binary,
    /// [`ProxiimaError::SchemaResetRequired`], and storage errors.
    pub async fn connect_storage(&self) -> Result<proxiima_storage_pg::PgStorage, ProxiimaError> {
        let (config, parts) = self.resolve()?;
        Ok(crate::boot::connect_storage::<A>(&config, &parts)
            .await?
            .storage)
    }

    /// Resolve, validate, boot, and start every configured feature, without
    /// binding a listener: with a bind address configured, MCP is assembled
    /// and handed to the host as [`BuiltProxiima::service`] /
    /// [`BuiltProxiima::mcp_edge`].
    ///
    /// # Errors
    ///
    /// Returns config, security, storage, engine, or MCP assembly errors.
    pub async fn build(self) -> Result<BuiltProxiima, ProxiimaError> {
        let BootedRuntime {
            config,
            parts,
            allowlist,
            booted,
            cancel,
            app_ctx,
            owner_access,
        } = Box::pin(self.boot_common()).await?;

        let (service, mcp_edge, mcp) = match (config.mcp, allowlist) {
            (Some(mcp), Some(allowlist)) => {
                let edge =
                    resolve_mcp_edge(&app_ctx, &parts, owner_access, allowlist, &cancel, &config);
                let service = build_router::<A>(
                    app_ctx.clone(),
                    &edge,
                    &booted.system_authority,
                    &cancel,
                    &config,
                );
                (
                    Some(service),
                    Some(edge),
                    FeatureDecision::mcp_router_handed(mcp.bind),
                )
            }
            _ => (None, None, FeatureDecision::mcp_off()),
        };
        let runtime = Runtime::start::<A>(booted, app_ctx.host, cancel, &config, mcp);
        Ok(BuiltProxiima {
            runtime,
            service,
            mcp_edge,
        })
    }

    /// Resolve, validate, boot, start every configured feature, and listen
    /// when a bind address is configured.
    ///
    /// # Errors
    ///
    /// Returns config, security, storage, engine, bind, or MCP serving errors.
    pub async fn run(self) -> Result<RunningProxiima, ProxiimaError> {
        let BootedRuntime {
            config,
            parts,
            allowlist,
            booted,
            cancel,
            app_ctx,
            owner_access,
        } = Box::pin(self.boot_common()).await?;

        let (mcp_addr, server, mcp) = if let (Some(mcp), Some(allowlist)) = (config.mcp, allowlist)
        {
            if !config.expose_network {
                assert_loopback(&mcp.bind)
                    .map_err(|err| ProxiimaError::Security(err.to_string()))?;
            }
            let edge =
                resolve_mcp_edge(&app_ctx, &parts, owner_access, allowlist, &cancel, &config);
            let app = build_router::<A>(
                app_ctx.clone(),
                &edge,
                &booted.system_authority,
                &cancel,
                &config,
            );
            let listener = tokio::net::TcpListener::bind(mcp.bind)
                .await
                .map_err(|err| ProxiimaError::Mcp(err.into()))?;
            let bound = listener
                .local_addr()
                .map_err(|err| ProxiimaError::Mcp(err.into()))?;
            let shutdown = cancel.clone();
            let server = tokio::spawn(async move {
                if let Err(err) = axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        shutdown.cancelled_owned().await;
                    })
                    .await
                {
                    tracing::warn!(error = %err, "proxiima facade server failed");
                }
            });
            booted
                .engine
                .set_mcp_url(format!("http://{bound}/mcp"))
                .await;
            (
                Some(bound),
                Some(server),
                FeatureDecision::mcp_listening(bound),
            )
        } else {
            (None, None, FeatureDecision::mcp_off())
        };

        let runtime = Runtime::start::<A>(booted, app_ctx.host, cancel, &config, mcp);
        Ok(RunningProxiima {
            runtime,
            mcp_addr,
            server,
        })
    }

    /// The boot prelude both entry points share, in the order boot has to
    /// happen: resolve configuration, resolve the origin allowlist, then
    /// touch storage.
    ///
    /// That order is the point of having one copy. `ListenerAllowlists::resolve`
    /// runs before `boot_app`, so a deployment naming an unparseable
    /// origin or host is refused before a connection is opened. The
    /// `build_refuses_*_before_storage` tests do not reach this: they pin
    /// refusals `resolve` itself raises, which precede storage whatever
    /// this function does. `run_refuses_an_unparseable_origin_before_storage`
    /// is what pins the ordering here, for both entry points at once.
    async fn boot_common(self) -> Result<BootedRuntime, ProxiimaError> {
        let (config, mut parts) = self.resolve()?;
        let allowlist = if config.mcp.is_some() {
            Some(ListenerAllowlists::resolve(&config)?)
        } else {
            None
        };
        let booted = boot_app::<A>(&config, &parts).await?;
        let cancel = CancellationToken::new();
        let mut app_ctx = AppContext {
            host: ProxiimaHost {
                engine: booted.engine.clone(),
                registry: booted.registry.clone(),
                pool: booted.pool.clone(),
                platform_scope: booted.platform_scope.clone(),
                pg_tuning: config.pg_tuning,
                pg_sidecars: booted.pg_sidecars.clone(),
                erase_context: booted.erase_context.clone(),
                origin_scope: booted.origin_scope,
                publication_origin_eligibility: booted.publication_origin_eligibility.clone(),
                blobs: booted.blobs.clone(),
                owner: config.owner,
                services: parts.services.clone(),
                object_purge_health: crate::object_maintenance::ObjectPurgeHealth::default(),
                ended_workers: crate::workers::EndedWorkers::default(),
            },
        };
        let owner_access = runtime_owner_access(
            &app_ctx,
            parts.owner_access.clone(),
            config.forwarder.clone(),
        );
        if let Some(late) = &parts.late_owner_access {
            late.bind(owner_access.clone());
        }
        if let Some(factory) = parts.platform_authenticator.take() {
            let platform_scope = app_ctx.host.platform_scope.clone();
            parts.authenticator = Some(factory(crate::PlatformAuthContext {
                platform_scope,
                owner_access: owner_access.clone(),
            })?);
        }
        let services = assemble_services::<A>(
            &app_ctx,
            &booted.registry,
            &config.served_surface,
            parts.authenticator.as_ref(),
            &owner_access,
            &booted.delegation_runtime_authority,
        )?;
        app_ctx.host.services = services;
        // Keep the one-time erase transaction state out of the serving future.
        Box::pin(drain_retired_facts(&booted)).await?;
        Ok(BootedRuntime {
            config,
            parts,
            allowlist,
            booted,
            cancel,
            app_ctx,
            owner_access,
        })
    }

    fn resolve(&self) -> Result<(crate::RuntimeConfig, crate::RuntimeParts), ProxiimaError> {
        let base = A::configure(RuntimeBuilder::default());
        let env_layer = if let Some(injected) = &self.injected_env {
            injected.clone()
        } else if self.use_env {
            RuntimeBuilder::default().apply_env()?
        } else {
            RuntimeBuilder::default()
        };
        let merged = self.overlay.clone().merge_over(env_layer.merge_over(base));
        merged.resolve()
    }
}

/// What `BuiltProxiima` and `RunningProxiima` share: the booted engine, its
/// authorities, and the features the runtime started and owns.
struct Runtime {
    host: ProxiimaHost,
    /// Shared with the object-purge worker, which sweeps every owner's
    /// expired uploads under it.
    system_authority: Arc<SystemAuthority>,
    /// Kept on host runtime handles, never on `AppContext` or shared services.
    delegation_runtime_authority: DelegationRuntimeAuthority,
    host_state_maintenance_authority: Option<proxiima_core::engine::HostStateMaintenanceAuthority>,
    /// The runtime's own token. Every task it started observes a child of
    /// it; [`Self::shutdown`] cancels it, and so does dropping the runtime
    /// (`_cancel_on_drop`), so no feature outlives its last handle.
    cancel: CancellationToken,
    _cancel_on_drop: tokio_util::sync::DropGuard,
    insecure_single_owner: bool,
    features: Features,
    /// The operator's registry of outbox consumers; see
    /// `runtime_handle_methods!`'s `outbox_consumers`.
    outbox_consumers: Arc<dyn proxiima_core::storage_ports::publication::PublicationConsumerPort>,
    /// The host-only outbox drain, kept for unit tests that drive a
    /// publisher over a wrapped port. Production code never holds it
    /// outside the publisher task.
    #[cfg(test)]
    outbox_for_tests: Arc<dyn proxiima_core::storage_ports::publication::PublicationOutboxPort>,
    /// Claim calls the runtime drain (and any test wrapping this handle)
    /// has issued, including empty idle passes.
    #[cfg(test)]
    outbox_claims_for_tests: Arc<AtomicUsize>,
}

impl Runtime {
    /// Start every configured feature over a booted engine. Infallible: it
    /// runs after the last fallible boot step.
    fn start<A: SpiiceApp>(
        booted: Booted,
        host: ProxiimaHost,
        cancel: CancellationToken,
        config: &crate::RuntimeConfig,
        mcp: FeatureDecision,
    ) -> Self {
        let system_authority = booted.system_authority;
        let objects = host
            .blobs
            .clone()
            .map(|blobs| crate::features::ObjectPurgeHandles {
                storage: booted.storage.clone(),
                blobs,
                authority: system_authority.clone(),
                health: host.object_purge_health.clone(),
            });
        #[cfg(test)]
        let (counted_outbox, outbox_claims_for_tests) =
            crate::in_process_outbox::CountingOutbox::wrap(booted.outbox.clone());
        let publication_outbox = {
            #[cfg(test)]
            {
                counted_outbox.clone()
            }
            #[cfg(not(test))]
            {
                booted.outbox.clone()
            }
        };
        let features = crate::features::start::<A>(
            mcp,
            &FeatureInputs {
                engine: &host.engine,
                services: &host.services,
                cancel: &cancel,
                ended: host.ended_workers.clone(),
                config,
                publication: crate::features::PublicationHandles {
                    outbox: publication_outbox,
                    retention: booted.outbox_retention,
                },
                objects,
            },
        );
        Self {
            host,
            system_authority,
            delegation_runtime_authority: booted.delegation_runtime_authority,
            host_state_maintenance_authority: booted.host_state_maintenance_authority,
            _cancel_on_drop: cancel.clone().drop_guard(),
            cancel,
            insecure_single_owner: config.insecure_single_owner,
            features,
            outbox_consumers: booted.outbox_consumers,
            #[cfg(test)]
            outbox_for_tests: counted_outbox,
            #[cfg(test)]
            outbox_claims_for_tests,
        }
    }

    fn single_owner_authz(&self) -> Option<AuthzContext> {
        self.insecure_single_owner
            .then_some(self.host.owner.as_ref())
            .flatten()
            .map(|owner| AuthzContext::for_insecure_single_owner(&self.system_authority, *owner))
    }

    /// Cancel every started feature and join it.
    async fn shutdown(self) {
        self.cancel.cancel();
        self.features.join().await;
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("host", &self.host)
            .field("insecure_single_owner", &self.insecure_single_owner)
            .field("features", &self.features)
            .finish_non_exhaustive()
    }
}

/// The handle methods `BuiltProxiima` and `RunningProxiima` share, spelled
/// once.
macro_rules! runtime_handle_methods {
    () => {
        /// The booted runtime's host accessors — the same [`ProxiimaHost`]
        /// [`crate::AppContext::host`] returns.
        #[must_use]
        pub const fn host(&self) -> &ProxiimaHost {
            &self.runtime.host
        }

        /// The S3 cited-blob store; `None` unless S3 is configured.
        ///
        /// This concrete backend accessor belongs to the boot handle. The
        /// authority-free `AppContext::host()` intentionally exposes only
        /// host-safe capabilities.
        #[must_use]
        pub const fn blobs(&self) -> Option<&CitedBlobStore> {
            self.runtime.host.blobs.as_ref()
        }

        #[must_use]
        pub fn system_authority(&self) -> &SystemAuthority {
            &self.runtime.system_authority
        }

        /// Bind host-managed download admission to this boot's registered read operation.
        ///
        /// Construct this handle once for the host's route. The retained boot
        /// witness keeps a spiice's shared read service from creating its own
        /// locator-bearing delivery handle with another engine or registry.
        /// Returns `None` when this runtime has no cited-blob backend.
        ///
        /// The request-facing `AppContext` carries no boot witness:
        ///
        /// ```compile_fail
        /// fn no_boot_delivery(context: &proxiima::AppContext, operation: proxiima::ScopeKey) {
        ///     let _ = context.host().blob_delivery(operation);
        /// }
        /// ```
        ///
        /// It also cannot reach the concrete object-store backend:
        ///
        /// ```compile_fail
        /// fn no_context_store(context: &proxiima::AppContext) {
        ///     let _: Option<&proxiima::CitedBlobStore> = context.host().blobs();
        /// }
        /// ```
        ///
        /// # Errors
        ///
        /// Returns a binding error for an absent, write or inexact operation
        /// key, or for a read service from another runtime.
        pub fn blob_delivery(
            &self,
            operation: proxiima_core::ScopeKey,
        ) -> Result<Option<CitedBlobDeliveryService>, CitedBlobDeliveryBindingError> {
            let host = &self.runtime.host;
            let Some(read) = host.services.get::<CitedBlobReadService>() else {
                return Ok(None);
            };
            CitedBlobDeliveryService::bind(
                (*read).clone(),
                &host.registry,
                operation,
                &self.runtime.delegation_runtime_authority,
            )
            .map(Some)
        }

        /// Every owner's retention rows, for the host's retention clock
        /// (docs/data-and-erasure.md).
        ///
        /// # Errors
        ///
        /// Storage faults.
        pub async fn retention(
            &self,
        ) -> Result<Vec<proxiima_core::Retention>, proxiima_core::ProtocolError> {
            self.runtime
                .host
                .engine
                .all_retention(&self.runtime.system_authority)
                .await
        }

        /// Boot-held authority for the registered host-state participant.
        /// Absent when the runtime booted without such a participant.
        #[must_use]
        pub const fn host_state_maintenance_authority(
            &self,
        ) -> Option<&proxiima_core::engine::HostStateMaintenanceAuthority> {
            self.runtime.host_state_maintenance_authority.as_ref()
        }

        /// The single owner's context in insecure single-owner mode; `None`
        /// otherwise.
        #[must_use]
        pub fn single_owner_authz(&self) -> Option<AuthzContext> {
            self.runtime.single_owner_authz()
        }

        /// Every feature's start decision for this boot, as logged.
        #[must_use]
        pub const fn boot_report(&self) -> &BootReport {
            &self.runtime.features.report
        }

        /// The pause and resume handle of a started claim-based feature.
        ///
        /// `Some` for a started [`Feature::EmbeddingWorker`] or
        /// [`Feature::OutboxPublisher`]. `None` for a feature that is off and
        /// for [`Feature::Mcp`] (serving continues by definition),
        /// [`Feature::SpiiceWorkers`] and [`Feature::PublishedRecordPrune`]: the
        /// runtime has no gate for them. The handle outlives `shutdown(self)`,
        /// after which every call on it reports `Stopped`.
        #[must_use]
        pub fn feature_control(&self, feature: Feature) -> Option<FeatureControl> {
            self.runtime.features.feature_control(feature)
        }

        /// The registry of outbox consumers (docs/host-integration.md §Consumers): list them,
        /// or retire one this deployment no longer runs.
        ///
        /// Boot registers every configured consumer; nothing but
        /// [`retire`](crate::PublicationConsumerPort::retire) removes one, so
        /// a consumer missing from one binary of a rolling deploy keeps its
        /// backlog. Retiring cancels every delivery the consumer was still
        /// owed, and a record no other consumer owes is published at once.
        /// Host-only, like the authorities above: no spiice reaches it.
        #[must_use]
        pub fn outbox_consumers(
            &self,
        ) -> Arc<dyn proxiima_core::storage_ports::publication::PublicationConsumerPort> {
            self.runtime.outbox_consumers.clone()
        }
    };
}

/// A booted app whose features run, without a bound listener.
///
/// Dropping it cancels every started feature; [`Self::shutdown`] also joins
/// them.
pub struct BuiltProxiima {
    runtime: Runtime,
    service: Option<Router>,
    /// What [`Self::service`] was layered with; see [`Self::mcp_edge`].
    mcp_edge: Option<crate::McpEdge>,
}

impl BuiltProxiima {
    runtime_handle_methods!();

    /// The MCP router — `/mcp`, `/v1` and the health probes as configured —
    /// for the host to serve; `None` without a bind address.
    #[must_use]
    pub const fn service(&self) -> Option<&Router> {
        self.service.as_ref()
    }

    /// The resolved MCP edge [`Self::service`] was built from, `None`
    /// without MCP: a host composing its own router takes the tool host,
    /// allowlists, revalidation and resource metadata from here, or
    /// [`McpEdge::router`](crate::McpEdge::router) for `/mcp` behind bearer
    /// auth beside its own unauthenticated routes.
    #[must_use]
    pub const fn mcp_edge(&self) -> Option<&crate::McpEdge> {
        self.mcp_edge.as_ref()
    }

    /// Stop and join every started feature.
    pub async fn shutdown(self) {
        self.runtime.shutdown().await;
    }

    /// The host-only outbox drain, for unit tests that wrap it.
    #[cfg(test)]
    pub(crate) fn outbox_for_tests(
        &self,
    ) -> Arc<dyn proxiima_core::storage_ports::publication::PublicationOutboxPort> {
        self.runtime.outbox_for_tests.clone()
    }

    /// Claim calls issued through [`Self::outbox_for_tests`] and the runtime
    /// drain, including empty idle passes.
    #[cfg(test)]
    pub(crate) fn outbox_claims_for_tests(&self) -> &Arc<AtomicUsize> {
        &self.runtime.outbox_claims_for_tests
    }
}

impl std::fmt::Debug for BuiltProxiima {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltProxiima")
            .field("has_service", &self.service.is_some())
            .field("runtime", &self.runtime)
            .finish_non_exhaustive()
    }
}

/// A booted app whose features run, listening when a bind is configured.
///
/// Dropping it cancels the listener and every started feature;
/// [`Self::shutdown`] also joins them.
pub struct RunningProxiima {
    runtime: Runtime,
    mcp_addr: Option<SocketAddr>,
    server: Option<JoinHandle<()>>,
}

impl RunningProxiima {
    runtime_handle_methods!();

    /// The bound MCP address; `None` without a bind address.
    #[must_use]
    pub const fn mcp_addr(&self) -> Option<SocketAddr> {
        self.mcp_addr
    }

    /// Serve until SIGTERM or SIGINT (Ctrl-C off Unix), then drain as
    /// [`Self::shutdown`] does: readiness turns unavailable, the listener
    /// stops accepting, in-flight requests and streams finish, every feature
    /// joins.
    ///
    /// Installs the process's signal handlers, so it belongs in a binary's
    /// `main`, never in a library.
    ///
    /// # Errors
    ///
    /// [`ProxiimaError::Mcp`] when the handlers cannot be installed, or when
    /// the listener stopped on its own before any signal — a process whose
    /// server died must not keep running as if healthy. The runtime is shut
    /// down on both paths.
    pub async fn until_shutdown_signal(self) -> Result<(), ProxiimaError> {
        self.until(shutdown_signal()).await
    }

    /// [`Self::until_shutdown_signal`] with the trigger supplied: shut down
    /// when `signal` resolves.
    ///
    /// # Errors
    ///
    /// As [`Self::until_shutdown_signal`], with `signal`'s own error.
    pub async fn until<F>(mut self, signal: F) -> Result<(), ProxiimaError>
    where
        F: std::future::Future<Output = Result<(), ProxiimaError>>,
    {
        let mut listener_stopped = false;
        let outcome = match self.server.as_mut() {
            Some(server) => tokio::select! {
                received = signal => received,
                joined = server => {
                    listener_stopped = true;
                    Err(ProxiimaError::Mcp(match joined {
                        Ok(()) => "the listener stopped before a shutdown signal".into(),
                        Err(err) => cause_with(format!("the listener task failed: {err}"), err),
                    }))
                }
            },
            None => signal.await,
        };
        if listener_stopped {
            self.server = None;
        }
        if outcome.is_ok() {
            tracing::info!("shutdown signal received; draining");
        }
        self.shutdown().await;
        outcome
    }

    /// Stop the listener, then stop and join every started feature.
    pub async fn shutdown(self) {
        self.runtime.cancel.cancel();
        if let Some(server) = self.server
            && let Err(err) = server.await
        {
            tracing::warn!(error = %err, "proxiima facade server join failed");
        }
        self.runtime.shutdown().await;
    }
}

impl std::fmt::Debug for RunningProxiima {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningProxiima")
            .field("mcp_addr", &self.mcp_addr)
            .field("has_server", &self.server.is_some())
            .field("runtime", &self.runtime)
            .finish_non_exhaustive()
    }
}

/// Run the configured app from process environment.
///
/// # Errors
///
/// Returns config, security, storage, engine, bind, or MCP serving errors.
pub async fn run<A: SpiiceApp + 'static>() -> Result<RunningProxiima, ProxiimaError> {
    Proxiima::<A>::app().from_env().run().await
}

/// [`run`], then serve until SIGTERM or SIGINT and drain — the whole `main`
/// of a stock host:
///
/// ```no_run
/// // `main` of a host composing `TheSpiice`: `serve::<(TheSpiice,)>()`.
/// async fn main_for<TheSpiice: proxiima::SpiiceApp + 'static>()
/// -> Result<(), proxiima::ProxiimaError> {
///     proxiima::serve::<(TheSpiice,)>().await
/// }
/// ```
///
/// # Errors
///
/// As [`run`] and [`RunningProxiima::until_shutdown_signal`].
pub async fn serve<A: SpiiceApp + 'static>() -> Result<(), ProxiimaError> {
    run::<A>().await?.until_shutdown_signal().await
}

/// SIGTERM (what an orchestrator sends) or SIGINT; Ctrl-C off Unix.
async fn shutdown_signal() -> Result<(), ProxiimaError> {
    let install = |err: std::io::Error| {
        ProxiimaError::Mcp(cause_with(
            format!("installing the shutdown signal handler: {err}"),
            err,
        ))
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).map_err(install)?;
        let mut interrupt = signal(SignalKind::interrupt()).map_err(install)?;
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.map_err(install)
    }
}

/// Compose rmcp and host-mounted routes behind one body, Host, and auth policy.
///
/// `host_allowlist` must also be passed to
/// [`streamable_http_service`](proxiima_mcp_server::streamable_http_service) so the
/// listener guard and rmcp's inner `/mcp` guard enforce the same authorities.
pub fn layered_router(
    mcp_service: McpStreamableService,
    app_router: Router,
    edge_auth: Arc<McpEdgeAuth>,
    allowlist: OriginAllowlist,
    host_allowlist: HostAllowlist,
) -> Router {
    layered_router_with_revalidation(
        mcp_service,
        app_router,
        edge_auth,
        allowlist,
        host_allowlist,
        RevalidationConfig::default(),
    )
}

/// Compose rmcp and host-mounted routes with explicit stream revalidation.
///
/// Layer order is body limit, listener-wide Host validation, browser CORS,
/// then bearer auth.
pub fn layered_router_with_revalidation(
    mcp_service: McpStreamableService,
    app_router: Router,
    edge_auth: Arc<McpEdgeAuth>,
    allowlist: OriginAllowlist,
    host_allowlist: HostAllowlist,
    revalidation: RevalidationConfig,
) -> Router {
    let auth_layer =
        proxiima_mcp_server::mcp_auth_layer_with_config(&mcp_service, edge_auth, revalidation);
    let router = Router::new()
        .nest_service(proxiima_mcp_server::MCP_PATH, mcp_service)
        .merge(app_router)
        .layer(auth_layer);
    guard_listener(
        router,
        allowlist,
        host_allowlist,
        proxiima_mcp_server::MAX_REQUEST_BODY_BYTES,
    )
}

/// Erase the concrete store once, then expose its disjoint capabilities over
/// one shared allocation. MCP, REST, and workers receive clones of the same
/// immutable [`SpiiceServices`] set; none can recover the concrete backend.
fn cited_blob_services(
    blobs: Option<&CitedBlobStore>,
    runtime_authority: &DelegationRuntimeAuthority,
) -> Option<(
    CitedBlobService,
    CitedBlobReadService,
    CitedBlobOwnerReconcileService,
)> {
    blobs.map(|store| {
        let store = Arc::new(store.clone());
        (
            CitedBlobService::new_runtime(store.clone(), runtime_authority),
            CitedBlobReadService::new_runtime(store.clone(), runtime_authority),
            CitedBlobOwnerReconcileService::new(store),
        )
    })
}

/// Everything [`Proxiima::boot_common`] produced, for the two tails that
/// diverge after it: `build` hands the router to the host, `run` binds a
/// listener and spawns the server. Both then start the same features.
///
/// A named struct rather than a tuple because the tail reads seven fields
/// of four visually similar types, and a 7-tuple is both unreadable at the
/// destructuring site and `clippy::type_complexity` on the signature.
struct BootedRuntime {
    config: crate::RuntimeConfig,
    parts: crate::RuntimeParts,
    /// `Some` exactly when MCP is configured — the two are resolved
    /// together so a tail can never serve MCP without its allowlists.
    allowlist: Option<ListenerAllowlists>,
    booted: Booted,
    cancel: CancellationToken,
    /// Its host carries the composed service set.
    app_ctx: AppContext,
    /// The one port the edge, the delegation service, and an environment
    /// OIDC authenticator resolve roles through.
    owner_access: Arc<dyn OwnerAccessPort>,
}

impl std::fmt::Debug for BootedRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootedRuntime")
            .field("config", &self.config)
            .field("parts", &self.parts)
            .field("booted", &self.booted)
            .field("app_ctx", &self.app_ctx)
            .finish_non_exhaustive()
    }
}

/// The host's owner-access port, else the Postgres resolver under this
/// runtime's platform authority, with the configured forwarding policy.
fn runtime_owner_access(
    app_ctx: &AppContext,
    host: Option<Arc<dyn OwnerAccessPort>>,
    forwarder: Option<ForwarderPolicy>,
) -> Arc<dyn OwnerAccessPort> {
    let resolver = {
        let host = app_ctx.host();
        Arc::new(PgOwnerAccessResolver::new(
            host.pool.clone(),
            host.platform_scope.clone(),
        ))
    };
    let port: Arc<dyn OwnerAccessPort> = host.unwrap_or_else(|| resolver.clone());
    match forwarder {
        // Registration is always this runtime's own PostgreSQL registry,
        // whatever port resolves ordinary memberships.
        Some(policy) => policy.wrap(port, resolver),
        None => port,
    }
}

/// Spiice services plus the substrate-owned services every composed
/// binary gets for free. When S3 is configured, the store is published as
/// [`CitedBlobService`] for presigned uploads, [`CitedBlobReadService`]
/// for bounded verified bytes, and the separately authorized
/// [`CitedBlobOwnerReconcileService`] for redacted owner reports. Tools,
/// REST, and spiice workers receive the same immutable service set.
///
/// The delegation service is published only with an authenticator, and
/// redeems through `owner_access`.
fn assemble_services<A: SpiiceApp>(
    app_ctx: &AppContext,
    registry: &Arc<SpiiceRegistryFrozen>,
    deployment_served_surface: &ServedSurface,
    authenticator: Option<&Arc<dyn Authenticator>>,
    owner_access: &Arc<dyn OwnerAccessPort>,
    runtime_authority: &DelegationRuntimeAuthority,
) -> Result<SpiiceServices, ProxiimaError> {
    let host = app_ctx.host();
    let mut services = host.services.clone();
    services.try_extend(A::services(app_ctx)?)?;
    debug_assert!(
        services
            .get::<proxiima_core::engine::HostStateMaintenanceAuthority>()
            .is_none(),
        "host-state maintenance authority must remain outside SpiiceServices"
    );
    if let Some((transfer, verified_read, owner_reconcile)) =
        cited_blob_services(host.blobs.as_ref(), runtime_authority)
    {
        services.try_insert(transfer)?;
        services.try_insert(verified_read)?;
        services.try_insert(owner_reconcile)?;
    }
    if services.get::<proxiima_core::RequestHeaders>().is_some() {
        return Err(ProxiimaError::Config(
            "a spiice published RequestHeaders as a boot service; it is request-scoped and \
             only the header allowlist may produce it"
                .into(),
        ));
    }
    if authenticator.is_some() {
        let store = Arc::new(PgDelegationStore::new(host.platform_scope.clone()));
        services.try_insert(DelegatedAuthorityService::new(
            store,
            owner_access.clone(),
            registry.clone(),
            deployment_served_surface.clone(),
            runtime_authority,
        ))?;
    }
    Ok(services)
}

/// Each page commits through the existing lifecycle inverse. Failures stop boot;
/// a later boot inventories the remaining admissions rather than replaying work.
async fn drain_retired_facts(booted: &Booted) -> Result<(), ProxiimaError> {
    for retired in booted.registry.retired_facts() {
        let retirement = booted
            .engine
            .retired_fact_erase(
                &booted.system_authority,
                retired.id.spiice,
                &retired.id.schema_id(),
            )
            .map_err(|error| ProxiimaError::Engine(error.into()))?;
        let mut after = None;
        loop {
            let owners = retirement
                .owners(after)
                .await
                .map_err(|error| ProxiimaError::Engine(error.into()))?;
            if owners.is_empty() {
                break;
            }
            for owner in owners {
                loop {
                    let page = retirement
                        .erase_page(owner)
                        .await
                        .map_err(|error| ProxiimaError::Engine(error.into()))?;
                    if !page.more_remaining {
                        break;
                    }
                }
                after = Some(owner);
            }
        }
    }
    Ok(())
}

async fn boot_app<A: SpiiceApp + 'static>(
    config: &crate::RuntimeConfig,
    parts: &crate::RuntimeParts,
) -> Result<Booted, ProxiimaError> {
    // Boot holds migration and role-census state. Keep that one-time future
    // out of every caller's runtime construction state machine.
    Box::pin(crate::boot::boot::<A>(config, parts)).await
}

/// Resolve the MCP edge both entry points serve: bearer auth over the
/// runtime's owner-access port, the tool host (engine, request headers,
/// host tools, call recording), the Host allowlist and the REST routes.
fn resolve_mcp_edge(
    app_ctx: &AppContext,
    parts: &crate::RuntimeParts,
    owner_access: Arc<dyn OwnerAccessPort>,
    allowlist: ListenerAllowlists,
    cancel: &CancellationToken,
    config: &crate::RuntimeConfig,
) -> crate::McpEdge {
    let engine = app_ctx.host().engine.clone();
    let services = app_ctx.host().services.clone();
    let mut edge_auth = McpEdgeAuth::headless().with_served_surface(config.served_surface.clone());
    if let Some(authenticator) = parts.authenticator.clone() {
        // The runtime's one owner-access port resolves ordinary memberships
        // and a forwarder's verified owner claim. Registration comes from
        // this runtime's authoritative registry, even with a host resolver.
        edge_auth = edge_auth
            .with_host(authenticator)
            .with_owner_access(owner_access);
    }
    let mut tool_host = McpToolHost::from_parts(Arc::new(engine.registry().clone()), services)
        .with_engine(engine)
        .with_request_headers(config.request_headers.clone());
    if let Some(host_tools) = parts.host_tools.clone() {
        tool_host = tool_host.with_host_tools(host_tools);
    }
    let rest_router = rest_router(&tool_host, config);
    crate::McpEdge {
        tool_host,
        edge_auth: Arc::new(edge_auth),
        origin_allowlist: allowlist.origins,
        host_allowlist: allowlist.hosts,
        revalidation: config.stream_revalidation,
        resource_metadata: config.resource_metadata.clone(),
        transport: config.mcp_transport,
        rest_router,
        cancel: cancel.clone(),
    }
}

fn build_router<A: SpiiceApp>(
    app_ctx: AppContext,
    edge: &crate::McpEdge,
    system_authority: &Arc<SystemAuthority>,
    cancel: &CancellationToken,
    config: &crate::RuntimeConfig,
) -> Router {
    let health_router = if config.health_endpoints {
        crate::health::router(
            app_ctx.host().pool.clone(),
            cancel.clone(),
            app_ctx.host().engine.clone(),
            app_ctx.host().object_purge_health.clone(),
            app_ctx.host().ended_workers.clone(),
        )
    } else {
        Router::new()
    };
    let retention_router =
        crate::retention_http::router(app_ctx.host().engine.clone(), Arc::clone(system_authority));
    let app_router = A::mount_http(Router::new(), app_ctx.clone());
    let public_router = A::mount_public_http(Router::new(), app_ctx);
    let mcp_service = edge.mcp_service();
    let auth_layer = proxiima_mcp_server::mcp_auth_layer_with_metadata(
        &mcp_service,
        Arc::clone(&edge.edge_auth),
        edge.revalidation,
        edge.resource_metadata.as_ref(),
    );
    let protected_router = Router::new()
        .nest_service(proxiima_mcp_server::MCP_PATH, mcp_service)
        .merge(edge.rest_router.clone())
        .merge(retention_router)
        .merge(app_router)
        .layer(auth_layer);
    // Merged after the auth layer, so these routes carry none. A method and
    // path already served above makes axum panic here, at boot, before
    // anything listens.
    let mut router = protected_router.merge(public_router);
    if let Some(md) = &edge.resource_metadata {
        router = router.merge(proxiima_mcp_server::public_discovery_router(md));
    }
    // Apply listener-wide guards only after anonymous OAuth metadata has been
    // merged, so CORS covers public metadata and preflights. Health probes sit
    // outside the guards: an orchestrator probes the pod address, not a
    // public host, and the probes disclose nothing a rebinding page could use.
    // Everything else, the fallback included, falls through to the guarded
    // router; a `merge` here would replace its guarded fallback with an
    // unguarded one.
    let guarded = guard_listener(
        router,
        edge.origin_allowlist.clone(),
        edge.host_allowlist.clone(),
        config.mcp_transport.max_request_body_bytes,
    );
    health_router.fallback_service(guarded)
}

/// The `/v1` REST surface, merged inside the shared auth, Host, and body-limit
/// layers. Its routes already carry the `/v1` prefix, so this is a `merge`, not
/// a `nest`: nesting would rewrite the inner request URI and strip the prefix
/// off every problem document's `instance`.
#[cfg(feature = "rest")]
fn rest_router(host: &McpToolHost, config: &crate::RuntimeConfig) -> Router {
    if !config.rest_enabled {
        return Router::new();
    }
    proxiima_mcp_server::rest::router(
        host.clone(),
        config
            .resource_metadata
            .as_ref()
            .map(|md| md.public_url.clone()),
    )
}

/// Feature-off shape. `PROXIIMA_REST_ENABLED` in a binary built without the
/// `rest` feature is an operator asking for a surface that is not in the
/// build; say so once at boot rather than serving 404s that look like a
/// routing bug.
#[cfg(not(feature = "rest"))]
fn rest_router(host: &McpToolHost, config: &crate::RuntimeConfig) -> Router {
    let _ = host;
    if config.rest_enabled {
        tracing::warn!(
            "PROXIIMA_REST_ENABLED is set but this binary was built without the `rest` \
             cargo feature; /v1 is not served"
        );
    }
    Router::new()
}

/// What the listener admits: the browser origins and the inbound `Host`
/// values, resolved together so a bad entry in either refuses boot.
struct ListenerAllowlists {
    origins: OriginAllowlist,
    /// Shared by the whole listener and rmcp.
    ///
    /// [`HostAllowlist`] always adds loopback (gateway rewrites and
    /// port-forwards keep working). Configured or derived public hosts are
    /// honored independently of bind address so a loopback listener behind a
    /// reverse proxy can preserve its public `Host`. Network exposure
    /// separately requires that public set to be non-empty in
    /// `RuntimeConfig::validate`.
    hosts: HostAllowlist,
}

impl ListenerAllowlists {
    fn resolve(config: &crate::RuntimeConfig) -> Result<Self, ProxiimaError> {
        let origins = if config.allowed_origins.is_empty() {
            default_allowlist()
        } else {
            OriginAllowlist::parse(&config.allowed_origins)
                .map_err(|err| ProxiimaError::Security(err.to_string()))?
        };
        let hosts = HostAllowlist::parse(config.public_allowed_hosts())
            .map_err(|err| ProxiimaError::Security(err.to_string()))?;
        Ok(Self { origins, hosts })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use proxiima_blob_s3::{S3KeyPair, S3RuntimeConfig};
    use proxiima_core::{
        AuthError, AuthPath, Authenticator, AuthzContext, Credentials, OwnerId, SpiiceRegistry,
        SpiiceRegistryError,
    };
    use proxiima_storage_pg::PgPoolConfig;
    use uuid::Uuid;

    use super::*;
    use crate::bundle::SpiiceBundle;
    use crate::{AppInfo, RuntimeBuilder, company_owner};

    mod alpha {
        proxiima_core::proxiima_spiice! {
            name = "proxiima-runtime-alpha",
            fact_schemas = [],
            abstraction_schemas = [],
            perspective_schemas = [],
            goal_schemas = [],
            mcp_tools = [],
        }
    }

    struct AlphaApp;

    impl SpiiceBundle for AlphaApp {
        fn register(registry: &mut SpiiceRegistry) -> Result<(), SpiiceRegistryError> {
            alpha::register(registry)
        }

        fn migrators() -> Vec<crate::NamedMigrator> {
            Vec::new()
        }
    }

    impl SpiiceApp for AlphaApp {
        fn app_info() -> AppInfo {
            AppInfo {
                id: "alpha",
                title: "Alpha",
                version: "1",
            }
        }

        fn configure(builder: RuntimeBuilder) -> RuntimeBuilder {
            builder
                .database_url("postgres://alpha/proxiima")
                .store_number(0)
                .platform_database_url("postgres://alpha/platform")
                .pg_pool_config(PgPoolConfig {
                    max_connections: 3,
                    ..PgPoolConfig::default()
                })
        }
    }

    #[test]
    fn setting_platform_url_keeps_the_supplied_store_number() {
        let app = || {
            Proxiima::<AlphaApp>::app().configure(|b| {
                b.database_url("postgres://unused/db")
                    .served_surface(ServedSurface::All)
            })
        };
        let missing = app()
            .configure(|b| b.platform_database_url("postgres://unused/platform"))
            .overlay
            .resolve()
            .expect_err("platform URL supplies no store identity");
        assert!(missing.to_string().contains("PROXIIMA_STORE_NUMBER"));
        for builder in [
            app().configure(|b| {
                b.store_number(3)
                    .platform_database_url("postgres://unused/platform")
            }),
            app().configure(|b| {
                b.platform_database_url("postgres://unused/platform")
                    .store_number(3)
            }),
        ] {
            assert_eq!(
                builder
                    .overlay
                    .resolve()
                    .expect("complete overlay")
                    .0
                    .store_number,
                3
            );
        }
        let (config, _) = app()
            .configure(|b| {
                b.store_number(3)
                    .platform_database_url("postgres://unused/platform")
            })
            .resolve()
            .expect("explicit store overrides application defaults");
        assert_eq!(config.store_number, 3);
    }

    #[tokio::test]
    async fn extension_assembly_publishes_owner_blob_reconcile_separately() {
        let pool = sqlx::PgPool::connect_lazy_with(sqlx::postgres::PgConnectOptions::new());
        let store = CitedBlobStore::new(
            pool,
            S3RuntimeConfig {
                bucket: "test-bucket".to_string(),
                region: "eu-central-1".to_string(),
                key_pair: S3KeyPair::new("test-access-key".into(), "test-secret-key".into()),
                endpoint_url: None,
                plaintext_hosts: proxiima_core::PlaintextHosts::none(),
                force_path_style: false,
                upload_ttl_seconds: 900,
                max_blob_bytes: None,
                store_id: None,
            },
            PgPlatformScope::for_tests(),
        )
        .expect("test store config");
        let (engine, _system, delegation_runtime) = proxiima_core::Engine::new(
            SpiiceRegistry::new().freeze_or_panic_for_tests(),
            proxiima_core::StoragePorts::rejecting_for_tests(),
        )
        .into_runtime_authorities();
        let registry = Arc::new(engine.registry().clone());
        let mut host = ProxiimaHost::for_tests(Arc::new(engine));
        host.blobs = Some(store);
        let app_ctx = AppContext { host };

        let services = assemble_services::<AlphaApp>(
            &app_ctx,
            &registry,
            &ServedSurface::All,
            None,
            &runtime_owner_access(&app_ctx, None, None),
            &delegation_runtime,
        )
        .expect("service assembly");

        assert!(services.get::<CitedBlobService>().is_some());
        assert!(
            services.get::<CitedBlobReadService>().is_some(),
            "S3 assembly must publish the bounded verified-read lane"
        );
        assert!(
            services.get::<CitedBlobOwnerReconcileService>().is_some(),
            "S3 assembly must publish the redacted owner reconcile lane"
        );
        assert!(
            services.get::<SystemAuthority>().is_none(),
            "global operator authority must never enter the spiice service set"
        );
        assert!(
            services
                .get::<proxiima_core::engine::HostStateMaintenanceAuthority>()
                .is_none(),
            "host-state maintenance authority must remain outside SpiiceServices"
        );
        assert!(
            services.get::<DelegatedAuthorityService>().is_none(),
            "delegation service requires a real authenticator"
        );
    }

    #[tokio::test]
    async fn delegation_service_is_authenticator_gated_and_shared_by_identity() {
        let (engine, _system, delegation_runtime) = proxiima_core::Engine::new(
            SpiiceRegistry::new().freeze_or_panic_for_tests(),
            proxiima_core::StoragePorts::rejecting_for_tests(),
        )
        .into_runtime_authorities();
        let engine = Arc::new(engine);
        let registry = Arc::new(engine.registry().clone());
        let app_ctx = AppContext {
            host: ProxiimaHost::for_tests(engine.clone()),
        };
        let authenticator: Arc<dyn Authenticator> = Arc::new(StubAuth { owner: owner() });
        let services = assemble_services::<AlphaApp>(
            &app_ctx,
            &registry,
            &ServedSurface::All,
            Some(&authenticator),
            &runtime_owner_access(&app_ctx, None, None),
            &delegation_runtime,
        )
        .expect("service assembly");
        let service = services
            .get::<DelegatedAuthorityService>()
            .expect("authenticated runtime publishes delegation service");
        let cloned = services.clone();
        let cloned_service = cloned
            .get::<DelegatedAuthorityService>()
            .expect("cloned services retain delegation service");
        assert!(Arc::ptr_eq(&service, &cloned_service));

        let worker =
            crate::workers::SpiiceWorkerContext::new_for_tests(engine, CancellationToken::new())
                .with_services(cloned);
        let worker_service = worker
            .service::<DelegatedAuthorityService>()
            .expect("worker sees composed delegation service");
        assert!(Arc::ptr_eq(&service, &worker_service));
        assert!(services.get::<SystemAuthority>().is_none());
        assert!(services.get::<DelegationRuntimeAuthority>().is_none());
    }

    struct StubAuth {
        owner: OwnerId,
    }

    #[async_trait]
    impl Authenticator for StubAuth {
        async fn authenticate(&self, _creds: &Credentials) -> Result<AuthzContext, AuthError> {
            let context = AuthzContext::for_subject(self.owner.as_user(), AuthPath::HostBearer);
            Ok(context
                .narrowed_to_owner(self.owner)
                .expect("stub owner is self-accessible"))
        }
    }

    fn owner() -> OwnerId {
        company_owner(Uuid::now_v7())
    }

    // --- Host-allowlist policy invariants (DNS-rebinding guard) ---
    //
    // rmcp's raw empty-list state is allow-all. The shared HostAllowlist type
    // makes that state unconstructible by adding loopback before either guard
    // sees the policy.

    #[test]
    fn listener_hosts_is_exactly_loopback_for_loopback_bind() {
        let owner = owner();
        let (config, _) = RuntimeBuilder::default()
            .database_url("postgres://unused/db")
            .store_number(0)
            .platform_database_url("postgres://unused/platform")
            .owner(owner)
            .served_surface(ServedSurface::All)
            .mcp_bind("127.0.0.1:31415".parse().unwrap())
            .authenticator(Arc::new(StubAuth { owner }))
            .resolve()
            .unwrap();

        assert!(!config.expose_network);
        assert_eq!(
            ListenerAllowlists::resolve(&config).unwrap().hosts.hosts(),
            ["localhost", "127.0.0.1", "::1"]
        );
    }

    #[test]
    fn listener_hosts_honors_explicit_host_on_loopback_bind() {
        let owner = owner();
        let (config, _) = RuntimeBuilder::default()
            .database_url("postgres://unused/db")
            .store_number(0)
            .platform_database_url("postgres://unused/platform")
            .owner(owner)
            .served_surface(ServedSurface::All)
            .mcp_bind("127.0.0.1:31415".parse().unwrap())
            .allowed_hosts(vec!["proxy.example.com:8443".to_string()])
            .authenticator(Arc::new(StubAuth { owner }))
            .resolve()
            .unwrap();

        assert!(!config.expose_network);
        assert!(
            ListenerAllowlists::resolve(&config)
                .unwrap()
                .hosts
                .hosts()
                .iter()
                .any(|host| host == "proxy.example.com:8443")
        );
    }

    #[test]
    fn listener_hosts_derives_public_hosts_on_loopback_bind() {
        let owner = owner();
        let (config, _) = RuntimeBuilder::default()
            .database_url("postgres://unused/db")
            .store_number(0)
            .platform_database_url("postgres://unused/platform")
            .owner(owner)
            .served_surface(ServedSurface::All)
            .mcp_bind("127.0.0.1:31415".parse().unwrap())
            .allowed_origins(vec!["https://app.example.com".to_string()])
            .resource_metadata(proxiima_mcp_server::ResourceServerMetadata {
                public_url: "https://proxy.example.com".to_string(),
                authorization_servers: proxiima_mcp_server::AuthorizationServers::Issuers(vec![
                    "https://idp.test".to_string(),
                ]),
                additional_scopes: Vec::new(),
            })
            .authenticator(Arc::new(StubAuth { owner }))
            .resolve()
            .unwrap();

        assert!(!config.expose_network);
        let allowlist = ListenerAllowlists::resolve(&config).unwrap().hosts;
        assert!(
            allowlist
                .hosts()
                .iter()
                .any(|host| host == "proxy.example.com")
        );
        assert!(
            allowlist
                .hosts()
                .iter()
                .any(|host| host == "app.example.com")
        );
    }

    #[test]
    fn listener_hosts_exposed_is_loopback_plus_public_and_never_empty() {
        let owner = owner();
        let (config, _) = RuntimeBuilder::default()
            .database_url("postgres://unused/db")
            .store_number(0)
            .platform_database_url("postgres://unused/platform")
            .owner(owner)
            .served_surface(ServedSurface::All)
            .mcp_bind("0.0.0.0:8080".parse().unwrap())
            .expose_network(true)
            .allowed_origins(vec!["https://app.example.com".to_string()])
            .authenticator(Arc::new(StubAuth { owner }))
            .resolve()
            .unwrap();

        let allowlist = ListenerAllowlists::resolve(&config).unwrap().hosts;
        let hosts = allowlist.hosts();
        // Loopback stays (gateway Host-rewrite + port-forward keep working)…
        assert!(hosts.iter().any(|host| host == "localhost"));
        assert!(hosts.iter().any(|host| host == "127.0.0.1"));
        assert!(hosts.iter().any(|host| host == "::1"));
        // …and the public host is present, but the list is NEVER empty —
        // so an exposed bind always hands rmcp a non-empty allowlist and
        // can never trip its allow-all state.
        assert!(hosts.iter().any(|host| host == "app.example.com"));
        assert!(!hosts.is_empty());
    }

    #[test]
    fn listener_hosts_includes_public_url_host_distinct_from_origins() {
        // Split deployment: browser app origin differs from the MCP host.
        // Deriving from origins alone would miss the real Host; the
        // public_url host must be in the allowlist.
        let owner = owner();
        let (config, _) = RuntimeBuilder::default()
            .database_url("postgres://unused/db")
            .store_number(0)
            .platform_database_url("postgres://unused/platform")
            .owner(owner)
            .served_surface(ServedSurface::All)
            .mcp_bind("0.0.0.0:8080".parse().unwrap())
            .expose_network(true)
            .allowed_origins(vec!["https://app.example.com".to_string()])
            .resource_metadata(proxiima_mcp_server::ResourceServerMetadata {
                public_url: "https://proxiima.example.com".to_string(),
                authorization_servers: proxiima_mcp_server::AuthorizationServers::Issuers(vec![
                    "https://idp.test".to_string(),
                ]),
                additional_scopes: Vec::new(),
            })
            .authenticator(Arc::new(StubAuth { owner }))
            .resolve()
            .unwrap();

        let allowlist = ListenerAllowlists::resolve(&config).unwrap().hosts;
        let hosts = allowlist.hosts();
        assert!(hosts.iter().any(|host| host == "proxiima.example.com"));
        assert!(hosts.iter().any(|host| host == "app.example.com"));
    }

    #[test]
    fn injected_lookup_replaces_the_ambient_process_env_source() {
        let (config, _) = Proxiima::<AlphaApp>::app()
            .from_env()
            .from_lookup(|key| {
                (key == "PROXIIMA_PG_MAX_CONNECTIONS")
                    .then(|| "4".to_string())
                    .or_else(|| {
                        (key == "PROXIIMA_PLATFORM_DATABASE_URL")
                            .then(|| "postgres://unused/platform".to_string())
                    })
            })
            .expect("injected pool lookup")
            .configure(|b| b.served_surface(ServedSurface::All))
            .resolve()
            .expect("resolved runtime config");

        assert_eq!(config.pg_pool_config.max_connections, 4);
    }

    #[tokio::test]
    async fn build_refuses_mcp_without_authenticator_or_insecure_mode_before_storage() {
        let err = Proxiima::<AlphaApp>::app()
            .configure(|b| {
                b.database_url("postgres://unused:5432/unused")
                    .store_number(0)
                    .platform_database_url("postgres://unused/platform")
                    .owner(owner())
                    .served_surface(ServedSurface::All)
                    .mcp_bind("127.0.0.1:31415".parse().unwrap())
            })
            .build()
            .await
            .unwrap_err();

        assert!(matches!(err, ProxiimaError::Security(_)));
    }

    #[tokio::test]
    async fn build_refuses_insecure_exposed_network_before_storage() {
        let err = Proxiima::<AlphaApp>::app()
            .configure(|b| {
                b.database_url("postgres://unused:5432/unused")
                    .store_number(0)
                    .platform_database_url("postgres://unused/platform")
                    .owner(owner())
                    .served_surface(ServedSurface::All)
                    .mcp_bind("127.0.0.1:31415".parse().unwrap())
                    .allow_insecure_single_owner()
                    .expose_network(true)
                    .allowed_origins(vec!["https://app.test".to_string()])
            })
            .build()
            .await
            .unwrap_err();

        assert!(matches!(err, ProxiimaError::Security(_)));
    }

    #[tokio::test]
    async fn build_refuses_non_loopback_insecure_mcp_before_storage() {
        let err = Proxiima::<AlphaApp>::app()
            .configure(|b| {
                b.database_url("postgres://unused:5432/unused")
                    .store_number(0)
                    .platform_database_url("postgres://unused/platform")
                    .owner(owner())
                    .served_surface(ServedSurface::All)
                    .mcp_bind("0.0.0.0:31415".parse().unwrap())
                    .allow_insecure_single_owner()
            })
            .build()
            .await
            .unwrap_err();

        assert!(matches!(err, ProxiimaError::Security(_)));
    }

    #[tokio::test]
    async fn build_refuses_exposed_network_without_origins_before_storage() {
        let owner = owner();
        let err = Proxiima::<AlphaApp>::app()
            .configure(|b| {
                b.database_url("postgres://unused:5432/unused")
                    .store_number(0)
                    .platform_database_url("postgres://unused/platform")
                    .owner(owner)
                    .served_surface(ServedSurface::All)
                    .mcp_bind("127.0.0.1:31415".parse().unwrap())
                    .expose_network(true)
                    .authenticator(Arc::new(StubAuth { owner }))
            })
            .build()
            .await
            .unwrap_err();

        assert!(matches!(err, ProxiimaError::Security(_)));
    }

    /// Every refusal above enters through `build`. The same fail-closed
    /// ordering — resolve the origin allowlist, and only then open a
    /// connection — has to hold for `run`, which is the entry point a
    /// deployment actually uses.
    ///
    /// Both now share `boot_common`, so the ordering is one statement
    /// rather than two copies that have to stay identical. This pins the
    /// claim from the other side: the database URL points nowhere, so a
    /// storage error here would mean the allowlist was resolved too late.
    #[tokio::test]
    async fn run_refuses_an_unparseable_origin_before_storage() {
        let owner = owner();
        let err = Proxiima::<AlphaApp>::app()
            .configure(|b| {
                b.database_url("postgres://unused:5432/unused")
                    .store_number(0)
                    .platform_database_url("postgres://unused/platform")
                    .owner(owner)
                    .served_surface(ServedSurface::All)
                    .mcp_bind("127.0.0.1:31415".parse().unwrap())
                    .allowed_origins(vec!["*".to_string()])
                    .authenticator(Arc::new(StubAuth { owner }))
            })
            .run()
            .await
            .unwrap_err();

        assert!(
            matches!(err, ProxiimaError::Security(_)),
            "a wildcard origin must be refused before storage is touched, got {err:?}"
        );
    }

    /// The Host allowlist is resolved with the origin one: an entry that
    /// could never match refuses boot before storage is touched.
    #[tokio::test]
    async fn run_refuses_an_unmatchable_host_before_storage() {
        let owner = owner();
        let err = Proxiima::<AlphaApp>::app()
            .configure(|b| {
                b.database_url("postgres://unused:5432/unused")
                    .store_number(0)
                    .platform_database_url("postgres://unused/platform")
                    .owner(owner)
                    .served_surface(ServedSurface::All)
                    .mcp_bind("127.0.0.1:31415".parse().unwrap())
                    .allowed_hosts(vec!["https://proxy.example.com".to_string()])
                    .authenticator(Arc::new(StubAuth { owner }))
            })
            .run()
            .await
            .unwrap_err();

        assert!(
            matches!(&err, ProxiimaError::Security(message) if message.contains("https://proxy.example.com")),
            "a URL in the host allowlist must be refused by name before storage, got {err:?}"
        );
    }

    #[tokio::test]
    async fn build_refuses_missing_served_surface_with_config_error() {
        // Fail-closed default: an embedding host that never calls
        // `.served_surface(...)` gets a config error instead of silently
        // advertising `ServedSurface::All`.
        let err = Proxiima::<AlphaApp>::app()
            .configure(|b| {
                b.database_url("postgres://unused:5432/unused")
                    .store_number(0)
                    .platform_database_url("postgres://unused/platform")
                    .owner(owner())
            })
            .build()
            .await
            .unwrap_err();

        assert!(matches!(err, ProxiimaError::Config(_)));
        assert!(err.to_string().contains("served_surface is required"));
    }
}
