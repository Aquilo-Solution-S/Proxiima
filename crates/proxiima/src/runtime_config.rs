use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use proxiima_blob_s3::S3RuntimeConfig;
use proxiima_core::publication::PublicationConfig;
use proxiima_core::publication::intake::DurableIntake;
use proxiima_core::storage_ports::OwnerDropProofHandle;
use proxiima_core::storage_ports::publication::ConsumerName;
use proxiima_core::{
    Authenticator, EmbeddingClient, EmbeddingRouter, EmbeddingRuntimePolicy, OwnerAccessPort,
    OwnerId, RequestHeaders, RevalidationConfig, ServedSurface, Setting, SpiiceServiceError,
    SpiiceServices, is_loopback_host,
};
use proxiima_server::{
    HostTools, McpTransportConfig, RequestHeaderAllowlist, ResourceServerMetadata,
};
use proxiima_storage_pg::{PgHostStateParticipant, PgPlatformScope, PgPoolConfig, PgTuning};

use crate::MissingMigrationPolicy;
use crate::config::{
    parse_bool_value, pg_pool_config_from_lookup, pg_tuning_from_lookup,
    publication_config_from_lookup, s3_from_lookup,
};
use crate::owner_access::{ForwarderPolicy, LateOwnerAccess, forwarder_from_lookup};
use crate::settings;

/// What an authenticator built at boot receives
/// ([`RuntimeBuilder::authenticator_with_platform_scope`]).
#[derive(Clone)]
pub struct PlatformAuthContext {
    /// The runtime's platform scope, validated at boot
    /// ([`PgPlatformScope::new`]: platform role, ownership, `FORCE RLS`
    /// census) — the one way to run platform-scoped queries.
    pub platform_scope: PgPlatformScope,
    /// The runtime's owner-access port (host port or the Postgres resolver,
    /// under the forwarder policy when one is configured).
    pub owner_access: Arc<dyn OwnerAccessPort>,
}

impl std::fmt::Debug for PlatformAuthContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformAuthContext")
            .finish_non_exhaustive()
    }
}

/// Builds the host authenticator once the platform scope exists.
pub type PlatformAuthenticatorFactory =
    Arc<dyn Fn(PlatformAuthContext) -> Result<Arc<dyn Authenticator>, ProxiimaError> + Send + Sync>;

pub(crate) const DUPLICATE_HOST_STATE_PARTICIPANT: &str = "a host-state participant is already registered and a unit of work dispatches to exactly one; register one participant that serves every command type (docs/host-integration.md)";

/// Runtime configuration builder for host applications.
#[derive(Clone, Default)]
pub struct RuntimeBuilder {
    database_url: Option<String>,
    store_number: Option<u16>,
    platform_database_url: Option<String>,
    migration_database_url: Option<String>,
    s3: Option<S3RuntimeConfig>,
    owner: Option<OwnerId>,
    mcp_bind: Option<SocketAddr>,
    expose_network: Option<bool>,
    allowed_origins: Option<Vec<String>>,
    allowed_hosts: Option<Vec<String>>,
    served_surface: Option<ServedSurface>,
    stream_max_lifetime: Option<Duration>,
    insecure_single_owner: bool,
    rest_enabled: Option<bool>,
    missing_migration_policy: Option<MissingMigrationPolicy>,
    pg_pool_config: Option<PgPoolConfig>,
    pg_tuning: Option<PgTuning>,
    authenticator: Option<Arc<dyn Authenticator>>,
    platform_authenticator: Option<PlatformAuthenticatorFactory>,
    host_tools: Option<Arc<dyn HostTools>>,
    vector_index: Option<Arc<dyn proxiima_core::storage_ports::VectorIndex>>,
    resource_metadata: Option<ResourceServerMetadata>,
    embed_client: Option<Arc<dyn EmbeddingClient>>,
    embedding_router: Option<Arc<dyn EmbeddingRouter>>,
    embedding_runtime_policy: Option<EmbeddingRuntimePolicy>,
    publication: Option<PublicationConfig>,
    /// Horizon after which a DELIVERED outbox record is reclaimed. `None`
    /// leaves the default; zero keeps published records forever, as
    /// `PROXIIMA_OUTBOX_PUBLISHED_RETENTION_SECS=0` spells.
    published_retention: Option<Duration>,
    /// How often the runtime sweeps expired uploads and drains object purge
    /// debt. `None` leaves the default; zero turns it off.
    object_purge_interval: Option<Duration>,
    outbox_consumers: OutboxConsumers,
    /// The built-in HTTP sink `PROXIIMA_OUTBOX_HTTP_URL` configures; added
    /// as the consumer `http` at [`Self::resolve`].
    outbox_http: Option<Arc<crate::outbox_http::HttpIntake>>,
    host_state_participant: Option<Arc<dyn PgHostStateParticipant>>,
    /// A second participant was registered; [`Self::resolve`] refuses.
    duplicate_host_state_participant: bool,
    runtime_grants: Option<bool>,
    request_headers: Option<Vec<String>>,
    owner_access: Option<Arc<dyn OwnerAccessPort>>,
    owner_drop_proof: Option<OwnerDropProofHandle>,
    forwarder: Option<ForwarderPolicy>,
    /// Every host bag handed to [`Self::services`], across layers; merged
    /// with [`SpiiceServices::try_extend`] at resolve so a type two layers
    /// both publish is a boot error, not a silent winner.
    services: Vec<SpiiceServices>,
    health_endpoints: Option<bool>,
    max_request_body_bytes: Option<usize>,
    mcp_transport: Option<McpTransportConfig>,
    /// The `PROXIIMA_OIDC_*` values the environment layer saw, kept so the
    /// authenticator is built at resolve only when no layer set one.
    oidc_env: Option<std::collections::BTreeMap<&'static str, String>>,
}

impl std::fmt::Debug for RuntimeBuilder {
    #[allow(clippy::too_many_lines)] // one line per field, destructured exhaustively
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            database_url,
            store_number,
            platform_database_url,
            migration_database_url,
            s3,
            owner,
            mcp_bind,
            expose_network,
            allowed_origins,
            allowed_hosts,
            served_surface,
            stream_max_lifetime,
            insecure_single_owner,
            rest_enabled,
            missing_migration_policy,
            pg_pool_config,
            pg_tuning,
            authenticator,
            platform_authenticator,
            host_tools,
            vector_index,
            resource_metadata,
            embed_client,
            embedding_router,
            embedding_runtime_policy,
            publication,
            published_retention,
            object_purge_interval,
            outbox_consumers: _,
            outbox_http,
            host_state_participant,
            duplicate_host_state_participant,
            runtime_grants,
            request_headers,
            owner_access,
            owner_drop_proof,
            forwarder,
            services,
            health_endpoints,
            max_request_body_bytes,
            mcp_transport,
            oidc_env,
        } = self;
        f.debug_struct("RuntimeBuilder")
            .field("has_database_url", &database_url.is_some())
            .field("store_number", &store_number)
            .field(
                "has_platform_database_url",
                &platform_database_url.is_some(),
            )
            .field(
                "has_migration_database_url",
                &migration_database_url.is_some(),
            )
            .field("s3", &s3)
            .field("owner", &owner)
            .field("mcp_bind", &mcp_bind)
            .field("expose_network", &expose_network)
            .field("allowed_origins", &allowed_origins)
            .field("allowed_hosts", &allowed_hosts)
            .field("served_surface", &served_surface)
            .field("stream_max_lifetime", &stream_max_lifetime)
            .field("insecure_single_owner", &insecure_single_owner)
            .field("rest_enabled", &rest_enabled)
            .field("missing_migration_policy", &missing_migration_policy)
            .field("pg_pool_config", &pg_pool_config)
            .field("pg_tuning", &pg_tuning)
            .field("has_authenticator", &authenticator.is_some())
            .field(
                "has_platform_authenticator",
                &platform_authenticator.is_some(),
            )
            .field("host_tools", &host_tools)
            .field("has_vector_index", &vector_index.is_some())
            .field("has_resource_metadata", &resource_metadata.is_some())
            .field("has_embed_client", &embed_client.is_some())
            .field("has_embedding_router", &embedding_router.is_some())
            .field("embedding_runtime_policy", &embedding_runtime_policy)
            .field("publication", &publication)
            .field("published_retention", &published_retention)
            .field("object_purge_interval", &object_purge_interval)
            .field("outbox_http", &outbox_http)
            .field(
                "has_host_state_participant",
                &host_state_participant.is_some(),
            )
            .field(
                "duplicate_host_state_participant",
                &duplicate_host_state_participant,
            )
            .field("runtime_grants", &runtime_grants)
            .field("request_headers", &request_headers)
            .field("has_owner_access", &owner_access.is_some())
            .field("has_owner_drop_proof", &owner_drop_proof.is_some())
            .field("forwarder", &forwarder)
            .field("services", &services)
            .field("health_endpoints", &health_endpoints)
            .field("max_request_body_bytes", &max_request_body_bytes)
            .field("mcp_transport", &mcp_transport)
            .field("has_oidc_env", &oidc_env.is_some())
            .finish_non_exhaustive()
    }
}

impl RuntimeBuilder {
    #[must_use]
    pub(crate) fn merge_over(self, base: Self) -> Self {
        // `authenticator` and `authenticator_with_platform_scope` are one
        // slot: whichever this layer set replaces whichever the base set.
        let (authenticator, platform_authenticator) =
            if self.authenticator.is_some() || self.platform_authenticator.is_some() {
                (self.authenticator, self.platform_authenticator)
            } else {
                (base.authenticator, base.platform_authenticator)
            };
        Self {
            database_url: self.database_url.or(base.database_url),
            store_number: self.store_number.or(base.store_number),
            platform_database_url: self.platform_database_url.or(base.platform_database_url),
            migration_database_url: self.migration_database_url.or(base.migration_database_url),
            s3: self.s3.or(base.s3),
            owner: self.owner.or(base.owner),
            mcp_bind: self.mcp_bind.or(base.mcp_bind),
            expose_network: self.expose_network.or(base.expose_network),
            allowed_origins: self.allowed_origins.or(base.allowed_origins),
            allowed_hosts: self.allowed_hosts.or(base.allowed_hosts),
            served_surface: self.served_surface.or(base.served_surface),
            stream_max_lifetime: self.stream_max_lifetime.or(base.stream_max_lifetime),
            insecure_single_owner: self.insecure_single_owner || base.insecure_single_owner,
            rest_enabled: self.rest_enabled.or(base.rest_enabled),
            missing_migration_policy: self
                .missing_migration_policy
                .or(base.missing_migration_policy),
            pg_pool_config: self.pg_pool_config.or(base.pg_pool_config),
            pg_tuning: self.pg_tuning.or(base.pg_tuning),
            authenticator,
            platform_authenticator,
            host_tools: self.host_tools.or(base.host_tools),
            vector_index: self.vector_index.or(base.vector_index),
            resource_metadata: self.resource_metadata.or(base.resource_metadata),
            embed_client: self.embed_client.or(base.embed_client),
            embedding_router: self.embedding_router.or(base.embedding_router),
            embedding_runtime_policy: self
                .embedding_runtime_policy
                .or(base.embedding_runtime_policy),
            publication: self.publication.or(base.publication),
            published_retention: self.published_retention.or(base.published_retention),
            object_purge_interval: self.object_purge_interval.or(base.object_purge_interval),
            outbox_consumers: base.outbox_consumers.overlaid_by(self.outbox_consumers),
            outbox_http: self.outbox_http.or(base.outbox_http),
            duplicate_host_state_participant: self.duplicate_host_state_participant
                || base.duplicate_host_state_participant
                || (self.host_state_participant.is_some() && base.host_state_participant.is_some()),
            host_state_participant: self.host_state_participant.or(base.host_state_participant),
            runtime_grants: self.runtime_grants.or(base.runtime_grants),
            request_headers: self.request_headers.or(base.request_headers),
            owner_access: self.owner_access.or(base.owner_access),
            owner_drop_proof: self.owner_drop_proof.or(base.owner_drop_proof),
            forwarder: self.forwarder.or(base.forwarder),
            services: base.services.into_iter().chain(self.services).collect(),
            health_endpoints: self.health_endpoints.or(base.health_endpoints),
            max_request_body_bytes: self.max_request_body_bytes.or(base.max_request_body_bytes),
            mcp_transport: self.mcp_transport.or(base.mcp_transport),
            oidc_env: self.oidc_env.or(base.oidc_env),
        }
    }

    /// Set the Postgres connection string. Env equivalent: `DATABASE_URL`.
    #[must_use]
    pub fn database_url(mut self, database_url: impl Into<String>) -> Self {
        self.database_url = Some(database_url.into());
        self
    }

    /// Assign this store its host-selected number. Required; the first boot
    /// persists it and every later boot must supply the same value.
    /// Environment equivalent: `PROXIIMA_STORE_NUMBER`.
    #[must_use]
    pub const fn store_number(mut self, number: u16) -> Self {
        self.store_number = Some(number);
        self
    }

    /// Set the separate migration/platform connection string.
    /// Environment equivalent: `PROXIIMA_PLATFORM_DATABASE_URL`.
    #[must_use]
    pub fn platform_database_url(mut self, url: impl Into<String>) -> Self {
        self.platform_database_url = Some(url.into());
        self
    }

    /// Set the direct/session-mode connection used for schema validation and
    /// the migrate step, when it must differ from
    /// [`Self::platform_database_url`]: the platform URL goes through a
    /// transaction-mode pooler, this one reaches Postgres directly or
    /// through a session-mode pool. Migrations hold a session advisory lock
    /// and session settings for the whole run, which a transaction-mode
    /// pooler cannot carry. Same role and database as the platform URL.
    /// Unset: use the platform URL. Environment equivalent:
    /// `PROXIIMA_MIGRATION_DATABASE_URL`.
    #[must_use]
    pub fn migration_database_url(mut self, url: impl Into<String>) -> Self {
        self.migration_database_url = Some(url.into());
        self
    }

    /// Configure the cited-blob S3 store. Env equivalent: the `PROXIIMA_S3_*` block.
    #[must_use]
    pub fn s3(mut self, s3: S3RuntimeConfig) -> Self {
        self.s3 = Some(s3);
        self
    }

    /// Set the engine `Owner` (= principal) explicitly.
    #[must_use]
    pub fn owner(mut self, owner: OwnerId) -> Self {
        self.owner = Some(owner);
        self
    }

    /// Bind the deployment's publication source and capture bounds.
    /// Env equivalent: `PROXIIMA_PUBLICATION_SOURCE`,
    /// `PROXIIMA_OUTBOX_MAX_PENDING`, `PROXIIMA_OUTBOX_MAX_PAYLOAD_BYTES`.
    ///
    /// Retention of DELIVERED records is deliberately NOT here. It is host
    /// housekeeping, not engine configuration — the engine never prunes —
    /// so it lives on [`Self::published_retention`] and is read from
    /// `PROXIIMA_OUTBOX_PUBLISHED_RETENTION_SECS` outside this block's guard.
    ///
    /// Setting this suppresses the environment read for the whole block —
    /// one parser, as with the S3 and Postgres blocks.
    #[must_use]
    pub fn publication(mut self, publication: PublicationConfig) -> Self {
        self.publication = Some(publication);
        self
    }

    /// How long an ALREADY-PUBLISHED outbox record is kept before the host
    /// reclaims its storage. Env equivalent:
    /// `PROXIIMA_OUTBOX_PUBLISHED_RETENTION_SECS`.
    ///
    /// The default is seven days; `Duration::ZERO` keeps delivered records
    /// forever. The horizon can never reach a `pending` or `claimed` record whatever its age: an
    /// undelivered event is a promise this deployment has not kept, and no
    /// retention policy may quietly cancel it (docs/host-integration.md, kernel OB-9).
    ///
    /// Setting this suppresses the environment read, so a host that composes
    /// programmatically is not overridden by a stray variable. A non-zero
    /// value under the one-minute floor is refused at `build`, exactly as
    /// the environment path refuses it at boot.
    #[must_use]
    pub const fn published_retention(mut self, horizon: Duration) -> Self {
        self.published_retention = Some(horizon);
        self
    }

    /// How often the runtime sweeps expired uploads and drains object purge
    /// debt while an object store is configured. Env equivalent:
    /// `PROXIIMA_OBJECT_PURGE_INTERVAL_SECS`; default 60 s.
    ///
    /// `Duration::ZERO` turns the in-process drain off, for a deployment
    /// that runs `proxiima-mcp maintain-storage` on its own schedule instead.
    /// Setting this suppresses the environment read.
    #[must_use]
    pub const fn object_purge_interval(mut self, interval: Duration) -> Self {
        self.object_purge_interval = Some(interval);
        self
    }

    /// Deliver the captured outbox to `intake`, in this process: the
    /// one-consumer case of [`Self::outbox_consumer`], registered as the
    /// consumer `default` ([`ConsumerName::DEFAULT`]). Set, the outbox
    /// publisher starts and calls [`DurableIntake::accept`] once per captured
    /// event (docs/host-integration.md §Delivery). With no consumer configured nothing drains
    /// the outbox: Facts are still captured, the backlog is bounded by
    /// `max_pending`, and a later boot with an intake delivers it.
    ///
    /// Delivery is at-least-once: a crash between `accept` and the outbox
    /// recording it delivers the event again with a higher
    /// [`ReceivedEvent::delivered_count`], so the intake deduplicates on
    /// [`ReceivedEvent::id`]. `Ok` (`Accepted` or `Rejected`) records the
    /// event as published. An [`IntakeError`], or an `accept` that does not
    /// return within five seconds, releases it to be tried again after the
    /// drain's backoff (the poll interval, doubling up to 30 s while a pass
    /// delivers nothing). The runtime keeps no per-event state of its own.
    ///
    /// No environment variables: the drain's batch (64), lease (30 s) and poll
    /// interval (500 ms) are fixed. A later call replaces an earlier one.
    /// Published-record retention and the feature's quiesce/resume
    /// ([`crate::Feature::OutboxPublisher`]) apply to the drain.
    ///
    /// [`ReceivedEvent::delivered_count`]: proxiima_core::publication::intake::ReceivedEvent::delivered_count
    /// [`ReceivedEvent::id`]: proxiima_core::publication::intake::ReceivedEvent::id
    /// [`IntakeError`]: proxiima_core::publication::intake::IntakeError
    #[must_use]
    pub fn outbox_intake(self, intake: Arc<dyn DurableIntake>) -> Self {
        self.outbox_consumer(ConsumerName::default(), intake)
    }

    /// Deliver the captured outbox to `intake` as the consumer `name`, in
    /// this process (docs/host-integration.md §Consumers).
    ///
    /// Each configured consumer gets its own drain and its own delivery lane
    /// per record: every listenable Fact captured after the consumer was first
    /// registered reaches it at least once, whatever the other consumers do.
    /// One that fails or lags holds back only its own lanes. Boot registers
    /// every configured name; a name stays registered — and keeps being owed
    /// every new record — until a host retires it
    /// ([`crate::BuiltProxiima::outbox_consumers`]). A later call for the same
    /// name replaces the earlier intake.
    ///
    /// The delivery contract is [`Self::outbox_intake`]'s, per consumer.
    #[must_use]
    pub fn outbox_consumer(mut self, name: ConsumerName, intake: Arc<dyn DurableIntake>) -> Self {
        self.outbox_consumers.0.insert(name, intake);
        self
    }

    /// Register a typed host-state participant on the [`crate::UnitOfWork`]
    /// write session. Hosts that register none keep existing Fact/sidecar/lock
    /// behavior with no extra configuration.
    ///
    /// Exactly one: a second registration — here, through
    /// [`Self::host_state_participant`], or from another tuple
    /// element's [`crate::SpiiceApp::configure`] — keeps the first and makes
    /// [`Self::resolve`] refuse, instead of silently replacing it.
    #[must_use]
    pub fn host_state_participant(mut self, participant: Arc<dyn PgHostStateParticipant>) -> Self {
        if self.host_state_participant.is_some() {
            self.duplicate_host_state_participant = true;
        } else {
            self.host_state_participant = Some(participant);
        }
        self
    }

    /// The MCP listener's bind address: set, MCP starts — `run()` listens
    /// on it, `build()` hands the router to the host. Unset, MCP is off.
    /// Env equivalent: `PROXIIMA_MCP_BIND`. A spiice that serves MCP by
    /// default sets it in [`crate::SpiiceApp::configure`].
    #[must_use]
    pub fn mcp_bind(mut self, bind: SocketAddr) -> Self {
        self.mcp_bind = Some(bind);
        self
    }

    /// Allow non-loopback MCP exposure (requires an authenticator and
    /// allowed origins). Env equivalent: `PROXIIMA_EXPOSE_NETWORK`.
    #[must_use]
    pub fn expose_network(mut self, expose_network: bool) -> Self {
        self.expose_network = Some(expose_network);
        self
    }

    /// Set the CORS origin allowlist for exposed MCP. A portless entry accepts
    /// any port; an explicit port also matches a browser `Origin` that omits
    /// it when it is the scheme default (HTTP 80 or HTTPS 443). Env equivalent
    /// (comma-separated): `PROXIIMA_ALLOWED_ORIGINS`.
    #[must_use]
    pub fn allowed_origins(mut self, allowed_origins: Vec<String>) -> Self {
        self.allowed_origins = Some(allowed_origins);
        self
    }

    /// Set the inbound `Host` allowlist for the shared HTTP listener.
    /// Entries are bare hostnames or `host:port`; loopback is always added
    /// on top. The same non-empty allowlist guards every listener route and
    /// rmcp's inner `/mcp` service. When unset, public hosts are derived from
    /// `PROXIIMA_PUBLIC_URL` and the allowed origins. Env equivalent
    /// (comma-separated): `PROXIIMA_ALLOWED_HOSTS`.
    #[must_use]
    pub fn allowed_hosts(mut self, allowed_hosts: Vec<String>) -> Self {
        self.allowed_hosts = Some(allowed_hosts);
        self
    }

    /// Set the deployment's served commands. Declared reads remain served;
    /// caller authorization independently requires each exact scope key.
    #[must_use]
    pub fn served_surface(mut self, served_surface: ServedSurface) -> Self {
        self.served_surface = Some(served_surface);
        self
    }

    /// Set the max authenticated stream lifetime.
    ///
    /// The environment equivalent is `PROXIIMA_STREAM_MAX_LIFETIME`,
    /// parsed as integer seconds.
    #[must_use]
    pub fn stream_max_lifetime(mut self, duration: Duration) -> Self {
        self.stream_max_lifetime = Some(duration);
        self
    }

    /// Opt into loopback-only single-owner mode (no authenticator).
    /// Never combined with network exposure; programmatic only.
    #[must_use]
    pub fn allow_insecure_single_owner(mut self) -> Self {
        self.insecure_single_owner = true;
        self
    }

    /// Serve the `/v1` REST rendering of the tool manifest beside `/mcp`
    /// (see docs/protocol.md). Off by default, and only effective when the crate is
    /// built with the `rest` feature. Env equivalent:
    /// `PROXIIMA_REST_ENABLED`.
    #[must_use]
    pub fn rest_enabled(mut self, rest_enabled: bool) -> Self {
        self.rest_enabled = Some(rest_enabled);
        self
    }

    /// Choose whether migration ledgers may contain versions absent from the
    /// composed binary. The default is [`MissingMigrationPolicy::Reject`].
    /// Explicitly choose [`MissingMigrationPolicy::Allow`] when releases
    /// share a database during a rolling deployment. Checksums, dirty rows,
    /// and divergent migration lanes are still checked. Environment equivalent:
    /// `PROXIIMA_MISSING_MIGRATION_POLICY` (`reject` or `allow`).
    #[must_use]
    pub fn missing_migration_policy(mut self, policy: MissingMigrationPolicy) -> Self {
        self.missing_migration_policy = Some(policy);
        self
    }

    /// Grant the runtime role its DML privileges in the migrate step
    /// ([`crate::Proxiima::migrate`]), after migrating. Serving never grants.
    /// Opt-in; off by default. Env equivalent: `PROXIIMA_RUNTIME_GRANTS`.
    #[must_use]
    pub const fn runtime_grants(mut self, runtime_grants: bool) -> Self {
        self.runtime_grants = Some(runtime_grants);
        self
    }

    /// Copy these inbound request headers to tools as opaque
    /// [`RequestHeaders`] on the served paths (`/mcp`, `/v1`). Entries are
    /// header names or `prefix*`; credential headers are refused. Empty —
    /// the default — publishes nothing. Env equivalent (comma-separated):
    /// `PROXIIMA_REQUEST_HEADERS`.
    #[must_use]
    pub fn request_headers(mut self, request_headers: Vec<String>) -> Self {
        self.request_headers = Some(request_headers);
        self
    }

    /// The owner-access port the served runtime resolves roles through: the
    /// edge's per-Group probe behind `X-Proxiima-Owner` and the delegation
    /// service. Defaults to the Postgres resolver over the runtime pool. A
    /// host authenticator that resolves roles through its own port should
    /// pass that same port here, or the two can answer differently for the
    /// same subject.
    #[must_use]
    pub fn owner_access(mut self, owner_access: Arc<dyn OwnerAccessPort>) -> Self {
        self.owner_access = Some(owner_access);
        self
    }

    /// The host's proof that a person was dropped
    /// ([`proxiima_core::OwnerDropProofPort`]), which `erase_personal_owner` and
    /// `erase_personal_source_scope` require before they erase anything. With
    /// none installed both refuse with `DropProofPortUnavailable`.
    #[must_use]
    pub fn owner_drop_proof(mut self, owner_drop_proof: OwnerDropProofHandle) -> Self {
        self.owner_drop_proof = Some(owner_drop_proof);
        self
    }

    /// Trusted forwarder subjects and their fixed role for a verified,
    /// registered owner; wraps the owner-access port. Env equivalent:
    /// `PROXIIMA_FORWARDER_SUBJECTS` + `PROXIIMA_FORWARDER_ROLE`.
    #[must_use]
    pub fn forwarder(mut self, policy: ForwarderPolicy) -> Self {
        self.forwarder = Some(policy);
        self
    }

    /// Host services published beside the spiices' own: visible to
    /// [`crate::SpiiceApp::services`] through
    /// [`crate::ProxiimaHost::services`], and to every tool, request
    /// behavior, route, and worker through the composed set. Repeatable; a
    /// type published twice — by two calls or by a host and a spiice — is a
    /// boot error.
    #[must_use]
    pub fn services(mut self, services: SpiiceServices) -> Self {
        self.services.push(services);
        self
    }

    /// Read memory stores beside this one through `port` (issue #462):
    /// `core_search_memories` fans out to them, `core_derive` accepts
    /// bare UUID sources from another store, and `proxiima://memory/<uuid>`
    /// reads through it using the number embedded in that UUID. Each store gets
    /// the default 2 s bound; publish an
    /// [`OtherStoresService`] with `with_timeout` through [`Self::services`]
    /// for another. Without a port every surface behaves as before.
    ///
    /// [`OtherStoresService`]: proxiima_core::storage_ports::other_stores::OtherStoresService
    #[must_use]
    pub fn other_stores(
        self,
        port: Arc<dyn proxiima_core::storage_ports::other_stores::OtherStores>,
    ) -> Self {
        self.services(SpiiceServices::with(
            proxiima_core::storage_ports::other_stores::OtherStoresService::new(port),
        ))
    }

    /// Serve anonymous `GET /healthz` (process up) and `GET /readyz`
    /// (database reachable and not shutting down) on the MCP listener.
    /// Observed embedding configuration faults keep readiness at HTTP 200
    /// with a sanitized degraded body; database failure or shutdown returns 503.
    /// Both probes sit outside the Host guard and bearer auth so an orchestrator
    /// probe needs neither. Off by default: a host that mounts its own would
    /// otherwise collide. Env equivalent: `PROXIIMA_HEALTH_ENDPOINTS`.
    #[must_use]
    pub const fn health_endpoints(mut self, health_endpoints: bool) -> Self {
        self.health_endpoints = Some(health_endpoints);
        self
    }

    /// Largest accepted request body on the listener, in bytes (default
    /// 4 MiB). Env equivalent: `PROXIIMA_MAX_REQUEST_BODY_BYTES`.
    #[must_use]
    pub const fn max_request_body_bytes(mut self, bytes: usize) -> Self {
        self.max_request_body_bytes = Some(bytes);
        self
    }

    /// rmcp Streamable HTTP tuning. Its body cap is replaced by
    /// [`Self::max_request_body_bytes`] when that is set. Env equivalents:
    /// `PROXIIMA_MCP_SSE_KEEP_ALIVE_SECS`, `PROXIIMA_MCP_SESSION_IDLE_SECS`,
    /// `PROXIIMA_MCP_SSE_RETRY_SECS`, `PROXIIMA_MCP_SESSIONS`,
    /// `PROXIIMA_MCP_JSON_RESPONSE`.
    #[must_use]
    pub const fn mcp_transport(mut self, transport: McpTransportConfig) -> Self {
        self.mcp_transport = Some(transport);
        self
    }

    /// Set Postgres pool and per-connection timeout policy.
    ///
    /// Env equivalents are `PROXIIMA_PG_MAX_CONNECTIONS`,
    /// `PROXIIMA_PG_STATEMENT_TIMEOUT_MS`, `PROXIIMA_PG_ACQUIRE_TIMEOUT_SECS`,
    /// `PROXIIMA_PG_IDLE_TIMEOUT_SECS`, and `PROXIIMA_PG_MAX_LIFETIME_SECS`.
    #[must_use]
    pub fn pg_pool_config(mut self, pg_pool_config: PgPoolConfig) -> Self {
        self.pg_pool_config = Some(pg_pool_config);
        self
    }

    /// Set the Postgres query tuning, bypassing the `PROXIIMA_PG_*` search
    /// variables a deployment would otherwise read. Its defaults are this
    /// release's shipped behaviour, so a host that never calls this and an
    /// environment that sets nothing are the same deployment.
    #[must_use]
    pub fn pg_tuning(mut self, pg_tuning: PgTuning) -> Self {
        self.pg_tuning = Some(pg_tuning);
        self
    }

    /// Install the host authenticator used to resolve MCP credentials.
    #[must_use]
    pub fn authenticator(mut self, authenticator: Arc<dyn Authenticator>) -> Self {
        self.authenticator = Some(authenticator);
        self
    }

    /// Build the host authenticator at boot, after the runtime validated
    /// its platform scope, from that scope and the runtime's owner-access
    /// port — for an authenticator whose resolver runs platform-scoped
    /// queries. Requires `platform_database_url`; exclusive with
    /// [`Self::authenticator`] and the `PROXIIMA_OIDC_*` environment.
    #[must_use]
    pub fn authenticator_with_platform_scope<F>(mut self, factory: F) -> Self
    where
        F: Fn(PlatformAuthContext) -> Result<Arc<dyn Authenticator>, ProxiimaError>
            + Send
            + Sync
            + 'static,
    {
        self.platform_authenticator = Some(Arc::new(factory));
        self
    }

    /// Serve `tools` on `/mcp` beside the registry's tools, through the same
    /// scope gate and request behaviors ([`HostTools`]).
    #[must_use]
    pub fn host_tools(mut self, tools: Arc<dyn HostTools>) -> Self {
        self.host_tools = Some(tools);
        self
    }

    /// Keep the store's chunk vectors in `index` instead of its own database
    /// (#519). The store keeps each memory's head and pays index debt as
    /// [`proxiima_core::storage_ports::VectorIndex`] describes; the network
    /// form is [`crate::vector_index_http`]. Without it the store's pgvector
    /// index serves.
    ///
    /// The setting also picks the schema the boot installs
    /// ([`crate::host::VectorStorage::HostIndex`]): the store's database
    /// needs no `vector` extension. The kind is chosen when the store is
    /// installed.
    #[must_use]
    pub fn vector_index(
        mut self,
        index: Arc<dyn proxiima_core::storage_ports::VectorIndex>,
    ) -> Self {
        self.vector_index = Some(index);
        self
    }

    /// Advertise OAuth protected-resource metadata (enables the public
    /// discovery route + `WWW-Authenticate` on 401).
    #[must_use]
    pub fn resource_metadata(mut self, metadata: ResourceServerMetadata) -> Self {
        self.resource_metadata = Some(metadata);
        self
    }

    /// One embedding client for every Owner. Mutually exclusive with
    /// [`Self::embedding_router`].
    #[must_use]
    pub fn embed_client(mut self, client: Arc<dyn EmbeddingClient>) -> Self {
        self.embed_client = Some(client);
        self
    }

    /// Per-Owner embedding routing (`Engine::with_embedding_router`).
    /// Mutually exclusive with [`Self::embed_client`].
    #[must_use]
    pub fn embedding_router(mut self, router: Arc<dyn EmbeddingRouter>) -> Self {
        self.embedding_router = Some(router);
        self
    }

    /// Set provider batching, request timeout, worker cadence, durable claim
    /// lifecycle, and optional search concurrency as one validated policy block.
    #[must_use]
    pub fn embedding_runtime_policy(mut self, policy: EmbeddingRuntimePolicy) -> Self {
        self.embedding_runtime_policy = Some(policy);
        self
    }

    /// Apply process environment variables to unset fields.
    ///
    /// # Errors
    ///
    /// Returns `ProxiimaError::Config` when an environment value is malformed.
    pub fn apply_env(self) -> Result<Self, ProxiimaError> {
        self.apply_lookup(proxiima_core::process_env)
    }

    /// Apply injected environment values to unset fields: the storage
    /// block of [`Self::apply_storage_lookup`] and the serving block beside it.
    ///
    /// Empty or whitespace-only is unset; values are trimmed.
    /// `PROXIIMA_ALLOWED_ORIGINS=` therefore does not override a
    /// programmatically set base — it leaves the field untouched.
    ///
    /// # Errors
    ///
    /// Returns `ProxiimaError::Config` when a supplied value is malformed.
    pub fn apply_lookup(
        self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ProxiimaError> {
        let mut builder = self.apply_storage_lookup(&lookup)?;
        builder.apply_serving_lookup(&lookup)?;
        Ok(builder)
    }

    /// Apply only the storage settings of the environment to unset fields:
    /// the database URLs, store number, missing-migration policy, runtime
    /// grants, the `PROXIIMA_PG_*` pool and tuning, and the embedding runtime
    /// policy the storage applies. A command that migrates or maintains
    /// storage reads these and leaves every serving setting — listener,
    /// authentication, transport, object store, publication and outbox
    /// delivery — unread, so a server's environment neither configures nor
    /// refuses it. A command that needs the object store sets it itself.
    ///
    /// # Errors
    ///
    /// Returns `ProxiimaError::Config` when a supplied value is malformed.
    pub fn apply_storage_lookup(
        mut self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ProxiimaError> {
        if self.store_number.is_none() {
            self.store_number = settings::PROXIIMA_STORE_NUMBER
                .read(&lookup)
                .map(|value| parse_store_number(&value))
                .transpose()?;
        }
        if self.database_url.is_none() {
            self.database_url = settings::DATABASE_URL.read(&lookup);
        }
        if self.platform_database_url.is_none() {
            self.platform_database_url = settings::PROXIIMA_PLATFORM_DATABASE_URL.read(&lookup);
        }
        if self.migration_database_url.is_none() {
            self.migration_database_url = settings::PROXIIMA_MIGRATION_DATABASE_URL.read(&lookup);
        }
        if self.missing_migration_policy.is_none() {
            self.missing_migration_policy = missing_migration_policy_from_lookup(&lookup)?;
        }
        if self.runtime_grants.is_none() {
            self.runtime_grants = settings::PROXIIMA_RUNTIME_GRANTS
                .read(&lookup)
                .map(|raw| parse_bool_value(&settings::PROXIIMA_RUNTIME_GRANTS, &raw))
                .transpose()?;
        }
        if self.pg_pool_config.is_none() {
            self.pg_pool_config = pg_pool_config_from_lookup(&lookup)?;
        }
        if self.pg_tuning.is_none() {
            self.pg_tuning = pg_tuning_from_lookup(&lookup)?;
        }
        // The storage applies this policy to its reclaim and stale-claim
        // reads, which maintenance reports too.
        if self.embedding_runtime_policy.is_none() && embedding_runtime_policy_env_is_set(&lookup) {
            self.embedding_runtime_policy = Some(embedding_runtime_policy_from_lookup(&lookup)?);
        }
        Ok(self)
    }

    /// The serving block of [`Self::apply_lookup`]: everything
    /// [`Self::apply_storage_lookup`] leaves unread.
    fn apply_serving_lookup(
        &mut self,
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<(), ProxiimaError> {
        if self.s3.is_none() {
            self.s3 = s3_from_lookup(lookup)?;
        }
        if self.mcp_bind.is_none()
            && let Some(raw) = settings::PROXIIMA_MCP_BIND.read(lookup)
        {
            self.mcp_bind = Some(raw.parse().map_err(|_| {
                ProxiimaError::Config(format!(
                    "PROXIIMA_MCP_BIND must be a socket address, got {raw:?}"
                ))
            })?);
        }
        if self.expose_network.is_none() {
            self.expose_network = settings::PROXIIMA_EXPOSE_NETWORK
                .read(lookup)
                .map(|raw| parse_bool_value(&settings::PROXIIMA_EXPOSE_NETWORK, &raw))
                .transpose()?;
        }
        if self.rest_enabled.is_none() {
            self.rest_enabled = settings::PROXIIMA_REST_ENABLED
                .read(lookup)
                .map(|raw| parse_bool_value(&settings::PROXIIMA_REST_ENABLED, &raw))
                .transpose()?;
        }
        self.apply_served_path_lookup(lookup)?;
        if self.allowed_origins.is_none() {
            self.allowed_origins = settings::PROXIIMA_ALLOWED_ORIGINS
                .read(lookup)
                .map(|raw| parse_allowed_origins(&raw));
        }
        if self.allowed_hosts.is_none() {
            self.allowed_hosts = settings::PROXIIMA_ALLOWED_HOSTS
                .read(lookup)
                .map(|raw| parse_allowed_hosts(&raw));
        }
        if self.stream_max_lifetime.is_none() {
            self.stream_max_lifetime = settings::PROXIIMA_STREAM_MAX_LIFETIME
                .read(lookup)
                .map(|raw| parse_duration_seconds(&settings::PROXIIMA_STREAM_MAX_LIFETIME, &raw))
                .transpose()?;
        }
        // Unconditional: the block is read (and validated) on every boot.
        if self.publication.is_none() {
            self.publication = Some(publication_config_from_lookup(lookup)?);
        }
        // Read outside the `publication` guard on purpose: retention is a
        // host-only housekeeping horizon, not part of the engine's
        // publication configuration, so a host that bound the engine block
        // programmatically has said nothing about it.
        if self.published_retention.is_none() {
            self.published_retention = crate::config::published_retention_from_lookup(lookup)?;
        }
        if self.object_purge_interval.is_none() {
            self.object_purge_interval = crate::config::object_purge_interval_from_lookup(lookup)?;
        }
        if self.outbox_http.is_none() {
            self.outbox_http = crate::outbox_http::HttpIntake::from_lookup(lookup)?.map(Arc::new);
        }
        Ok(())
    }

    /// The served-path block of [`Self::apply_lookup`]: request headers,
    /// forwarder policy, health probes, transport, and the OIDC capture.
    fn apply_served_path_lookup(
        &mut self,
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<(), ProxiimaError> {
        if self.request_headers.is_none() {
            self.request_headers = settings::PROXIIMA_REQUEST_HEADERS
                .read(lookup)
                .map(|raw| raw.split(',').map(ToOwned::to_owned).collect());
        }
        if self.forwarder.is_none() {
            self.forwarder = forwarder_from_lookup(lookup)?;
        }
        if self.health_endpoints.is_none() {
            self.health_endpoints = settings::PROXIIMA_HEALTH_ENDPOINTS
                .read(lookup)
                .map(|raw| parse_bool_value(&settings::PROXIIMA_HEALTH_ENDPOINTS, &raw))
                .transpose()?;
        }
        if self.max_request_body_bytes.is_none() {
            self.max_request_body_bytes = settings::PROXIIMA_MAX_REQUEST_BODY_BYTES
                .read(lookup)
                .map(|raw| {
                    raw.parse::<usize>().map_err(|_| {
                        ProxiimaError::Config(format!(
                            "PROXIIMA_MAX_REQUEST_BODY_BYTES must be a byte count, got {raw:?}"
                        ))
                    })
                })
                .transpose()?;
        }
        if self.mcp_transport.is_none() {
            self.mcp_transport = mcp_transport_from_lookup(lookup)?;
        }
        if self.oidc_env.is_none() && settings::PROXIIMA_OIDC_ISSUER.read(lookup).is_some() {
            self.oidc_env = Some(
                oidc_env_keys()
                    .iter()
                    .filter_map(|setting| setting.read(lookup).map(|value| (setting.name(), value)))
                    .collect(),
            );
        }
        Ok(())
    }

    /// The environment's OIDC authenticator fills in only when no layer set
    /// one in code, and resolves through a port bound at boot — the same one
    /// the edge and the delegation service get.
    #[cfg(feature = "auth-oidc")]
    fn env_authenticator(&mut self) -> Result<Option<LateOwnerAccess>, ProxiimaError> {
        if self.authenticator.is_some() || self.platform_authenticator.is_some() {
            return Ok(None);
        }
        let Some(oidc_env) = self.oidc_env.take() else {
            return Ok(None);
        };
        let late = LateOwnerAccess::default();
        let lookup = |key: &str| oidc_env.get(key).cloned();
        let Some((authenticator, metadata)) =
            crate::auth::oidc_from_lookup(&lookup, Arc::new(late.clone()))?
        else {
            return Ok(None);
        };
        self.authenticator = Some(authenticator);
        self.resource_metadata.get_or_insert(metadata);
        Ok(Some(late))
    }

    /// Without `auth-oidc` there is no authenticator to build: an issuer the
    /// operator set is a request for authentication this binary cannot
    /// honour, refused rather than ignored.
    #[cfg(not(feature = "auth-oidc"))]
    fn env_authenticator(&mut self) -> Result<Option<LateOwnerAccess>, ProxiimaError> {
        if self.authenticator.is_none()
            && self.platform_authenticator.is_none()
            && self.oidc_env.take().is_some()
        {
            return Err(ProxiimaError::Config(
                "PROXIIMA_OIDC_ISSUER is set but this binary was built without the `auth-oidc` \
                 cargo feature; enable it or install an authenticator in code"
                    .into(),
            ));
        }
        Ok(None)
    }

    /// [`Self::authenticator_with_platform_scope`] runs at boot against the
    /// platform scope; refuse before storage what could never reach it.
    fn check_platform_authenticator(&self) -> Result<(), ProxiimaError> {
        if self.platform_authenticator.is_none() {
            return Ok(());
        }
        if self.authenticator.is_some() {
            return Err(ProxiimaError::Config(
                "set either authenticator or authenticator_with_platform_scope, not both".into(),
            ));
        }
        if self.platform_database_url.is_none() {
            return Err(ProxiimaError::Config(
                "authenticator_with_platform_scope needs the platform scope: set \
                 platform_database_url (PROXIIMA_PLATFORM_DATABASE_URL)"
                    .into(),
            ));
        }
        Ok(())
    }

    /// The platform URL [`Self::resolve`] requires, and the migration URL
    /// checked against it.
    fn take_platform_urls(&mut self) -> Result<(String, Option<String>), ProxiimaError> {
        let platform = self.platform_database_url.take().ok_or_else(|| {
            ProxiimaError::Config(
                "PROXIIMA_PLATFORM_DATABASE_URL is required for owner-RLS boot".into(),
            )
        })?;
        let migration = self.migration_database_url.take();
        if let Some(migration) = &migration {
            check_migration_database_url(migration, &platform)?;
        }
        Ok((platform, migration))
    }

    /// The served-path half of [`Self::resolve`]: the environment OIDC
    /// authenticator, the request-header allowlist, the merged host service
    /// bag, and the transport with its one body cap.
    fn take_served_path(&mut self) -> Result<ServedPath, ProxiimaError> {
        let late_owner_access = self.env_authenticator()?;
        let request_headers =
            RequestHeaderAllowlist::parse(self.request_headers.as_deref().unwrap_or_default())
                .map_err(|err| ProxiimaError::Config(err.to_string()))?;
        let mut services = SpiiceServices::new();
        for bag in std::mem::take(&mut self.services) {
            services.try_extend(bag)?;
        }
        if services.get::<RequestHeaders>().is_some() {
            return Err(ProxiimaError::Config(
                "RequestHeaders is request-scoped; publish headers with request_headers \
                     (PROXIIMA_REQUEST_HEADERS), not as a boot service"
                    .into(),
            ));
        }
        let mut mcp_transport = self.mcp_transport.unwrap_or_default();
        if let Some(bytes) = self.max_request_body_bytes {
            mcp_transport.max_request_body_bytes = bytes;
        }
        if mcp_transport.max_request_body_bytes == 0 {
            return Err(ProxiimaError::Config(
                "the request body limit must be at least 1 byte".into(),
            ));
        }
        Ok(ServedPath {
            late_owner_access,
            request_headers,
            services,
            mcp_transport,
        })
    }

    /// The built-in HTTP sink joins as the consumer `http`. A host that also
    /// registered `http` itself is told so rather than one of the two
    /// silently winning.
    fn register_outbox_http(&mut self) -> Result<(), ProxiimaError> {
        let Some(http) = self.outbox_http.take() else {
            return Ok(());
        };
        let name = crate::outbox_http::consumer_name();
        if self.outbox_consumers.0.contains_key(&name) {
            return Err(ProxiimaError::Config(format!(
                "{} is set and the host also registered an outbox consumer named \
                 `http`; rename the host consumer or unset the variable",
                settings::PROXIIMA_OUTBOX_HTTP_URL
            )));
        }
        self.outbox_consumers.0.insert(name, http);
        Ok(())
    }

    /// One floor, both doors. The environment path refuses a sub-minute
    /// horizon while parsing; a host that set it programmatically would
    /// otherwise get a prune loop that runs every minute over a horizon
    /// shorter than its own interval.
    fn check_published_retention(&self) -> Result<(), ProxiimaError> {
        if let Some(horizon) = self.published_retention
            && !horizon.is_zero()
            && horizon < crate::config::MIN_PUBLISHED_RETENTION
        {
            return Err(ProxiimaError::Config(format!(
                "published retention is {}s, under the {}s floor; zero keeps \
                 published records forever",
                horizon.as_secs(),
                crate::config::MIN_PUBLISHED_RETENTION.as_secs(),
            )));
        }
        Ok(())
    }

    /// The storage half of both resolves: the host-state participant, the
    /// database URLs, the store number and the defaults of the remaining
    /// storage settings. Not yet validated ([`StorageConfig::validate`]).
    fn take_storage(&mut self) -> Result<(StorageConfig, StorageParts), ProxiimaError> {
        if self.duplicate_host_state_participant {
            return Err(ProxiimaError::Config(
                DUPLICATE_HOST_STATE_PARTICIPANT.into(),
            ));
        }
        let database_url = self
            .database_url
            .take()
            .ok_or_else(|| ProxiimaError::Config("DATABASE_URL is required".into()))?;
        let (platform_database_url, migration_database_url) = self.take_platform_urls()?;
        let store_number = self.store_number.ok_or_else(|| {
            ProxiimaError::Config(
                "PROXIIMA_STORE_NUMBER is required: assign this store a number".into(),
            )
        })?;
        let config = StorageConfig {
            database_url,
            store_number,
            platform_database_url,
            migration_database_url,
            missing_migration_policy: self.missing_migration_policy.unwrap_or_default(),
            runtime_grants: self.runtime_grants.unwrap_or(false),
            pg_pool_config: self.pg_pool_config.unwrap_or_default(),
            pg_tuning: self.pg_tuning.unwrap_or_default(),
            embedding_runtime_policy: self.embedding_runtime_policy.unwrap_or_default(),
        };
        let parts = StorageParts {
            vector_index: self.vector_index.take(),
            host_state_participant: self.host_state_participant.take(),
        };
        Ok((config, parts))
    }

    /// Resolve only the storage settings, for a command that migrates or
    /// connects storage and serves nothing. Nothing serving refuses it: no
    /// listener, authenticator, served surface, transport, publication,
    /// outbox or object-store setting is read.
    ///
    /// # Errors
    ///
    /// Returns `ProxiimaError::Config` for a missing database URL, platform
    /// URL or store number, a second host-state participant, an invalid pool
    /// configuration, or a migration URL that does not name the platform
    /// URL's role and database.
    pub fn resolve_storage(mut self) -> Result<(StorageConfig, StorageParts), ProxiimaError> {
        let (config, parts) = self.take_storage()?;
        config.validate()?;
        Ok((config, parts))
    }

    /// Resolve the builder into pure config plus host-provided runtime parts.
    ///
    /// # Errors
    ///
    /// Returns `ProxiimaError::Config` for missing required config and
    /// `ProxiimaError::Security` for fail-closed validation failures.
    pub fn resolve(mut self) -> Result<(RuntimeConfig, RuntimeParts), ProxiimaError> {
        self.check_platform_authenticator()?;
        let (storage_config, storage_parts) = self.take_storage()?;
        self.register_outbox_http()?;
        let ServedPath {
            late_owner_access,
            request_headers,
            services,
            mcp_transport,
        } = self.take_served_path()?;
        let default_revalidation = RevalidationConfig::default();
        let stream_revalidation = RevalidationConfig {
            max_stream_lifetime: self
                .stream_max_lifetime
                .unwrap_or(default_revalidation.max_stream_lifetime),
        };
        validate_revalidation_config(stream_revalidation)?;
        self.check_published_retention()?;
        let served_surface = self.served_surface.ok_or_else(|| {
            ProxiimaError::Config(
                "served_surface is required: pass ServedSurface::All to serve all registered commands \
                 or ServedSurface::Palette([...]) to restrict configured commands; declared reads \
                 remain served and each caller needs its exact scope keys"
                    .into(),
            )
        })?;
        let parts = RuntimeParts {
            authenticator: self.authenticator,
            platform_authenticator: self.platform_authenticator,
            host_tools: self.host_tools,
            storage: storage_parts,
            embed_client: self.embed_client,
            embedding_router: self.embedding_router,
            owner_access: self.owner_access,
            owner_drop_proof: self.owner_drop_proof,
            services,
            late_owner_access,
        };
        let config = RuntimeConfig {
            storage: storage_config,
            s3: self.s3,
            owner: self.owner,
            mcp: self.mcp_bind.map(|bind| McpSettings { bind }),
            expose_network: self.expose_network.unwrap_or(false),
            allowed_origins: self.allowed_origins.unwrap_or_default(),
            allowed_hosts: self.allowed_hosts.unwrap_or_default(),
            served_surface,
            stream_revalidation,
            insecure_single_owner: self.insecure_single_owner,
            rest_enabled: self.rest_enabled.unwrap_or(false),
            request_headers,
            forwarder: self.forwarder,
            health_endpoints: self.health_endpoints.unwrap_or(false),
            mcp_transport,
            auth: RuntimeAuthState {
                has_host_authenticator: parts.has_authenticator(),
            },
            resource_metadata: self.resource_metadata,
            publication: self.publication.unwrap_or_default(),
            published_retention: Some(
                self.published_retention
                    .unwrap_or(crate::config::DEFAULT_PUBLISHED_RETENTION),
            )
            .filter(|horizon| !horizon.is_zero()),
            object_purge_interval: Some(
                self.object_purge_interval
                    .unwrap_or(crate::config::DEFAULT_OBJECT_PURGE_INTERVAL),
            )
            .filter(|interval| !interval.is_zero()),
            outbox_consumers: self.outbox_consumers,
        };
        config.validate()?;
        Ok((config, parts))
    }
}

/// What [`RuntimeBuilder::take_served_path`] resolves.
struct ServedPath {
    late_owner_access: Option<LateOwnerAccess>,
    request_headers: RequestHeaderAllowlist,
    services: SpiiceServices,
    mcp_transport: McpTransportConfig,
}

/// The storage settings of the environment's storage block
/// ([`RuntimeBuilder::apply_storage_lookup`]): all that
/// [`crate::Proxiima::migrate`] and [`crate::Proxiima::connect_storage`] read.
#[derive(Clone)]
pub struct StorageConfig {
    pub database_url: String,
    /// Host-assigned immutable store identity, persisted by the first
    /// migrate step and checked by every boot.
    pub store_number: u16,
    pub platform_database_url: String,
    /// The connection the migrate step migrates and boot validates the
    /// schema on (`PROXIIMA_MIGRATION_DATABASE_URL`); `None` uses
    /// [`Self::platform_database_url`]. See [`Self::migrations_url`].
    pub migration_database_url: Option<String>,
    /// Missing-version handling for normal boot and read-only preflight.
    pub missing_migration_policy: MissingMigrationPolicy,
    /// Grant the runtime role its DML privileges in the migrate step
    /// (`PROXIIMA_RUNTIME_GRANTS`), after migrating. Off by default: a
    /// DBA-managed deployment grants out-of-band (docs/operate.md).
    pub runtime_grants: bool,
    /// Postgres pool size and timeout policy. Resolved once before storage
    /// construction; the canonical boot path never re-reads process env.
    pub pg_pool_config: PgPoolConfig,
    /// Postgres query tuning (`PROXIIMA_PG_*`). Defaults are this release's
    /// shipped behaviour, so an unset environment is production.
    pub pg_tuning: PgTuning,
    pub embedding_runtime_policy: EmbeddingRuntimePolicy,
}

impl StorageConfig {
    /// The URL the migrate step migrates and boot validates the schema on:
    /// [`Self::migration_database_url`] when set, otherwise
    /// [`Self::platform_database_url`].
    #[must_use]
    pub fn migrations_url(&self) -> &str {
        self.migration_database_url
            .as_deref()
            .unwrap_or(&self.platform_database_url)
    }

    /// Validate the pool configuration and the migration URL.
    ///
    /// # Errors
    ///
    /// Returns `ProxiimaError::Config` for an invalid pool configuration or a
    /// migration URL that does not name the platform URL's role and database.
    pub fn validate(&self) -> Result<(), ProxiimaError> {
        self.pg_pool_config
            .validate()
            .map_err(|error| ProxiimaError::Config(error.to_string()))?;
        if let Some(migration) = &self.migration_database_url {
            check_migration_database_url(migration, &self.platform_database_url)?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageConfig")
            .field("database_url", &"<redacted>")
            .field("store_number", &self.store_number)
            .field("platform_database_url", &"<redacted>")
            .field(
                "migration_database_url",
                &self.migration_database_url.as_ref().map(|_| "<redacted>"),
            )
            .field("missing_migration_policy", &self.missing_migration_policy)
            .field("runtime_grants", &self.runtime_grants)
            .field("pg_pool_config", &self.pg_pool_config)
            .field("pg_tuning", &self.pg_tuning)
            .field("embedding_runtime_policy", &self.embedding_runtime_policy)
            .finish()
    }
}

/// The host-provided parts storage boot reads.
#[derive(Clone, Default)]
pub struct StorageParts {
    /// The store's host vector index ([`RuntimeBuilder::vector_index`]).
    pub vector_index: Option<Arc<dyn proxiima_core::storage_ports::VectorIndex>>,
    /// The host-state participant of the write session
    /// ([`RuntimeBuilder::host_state_participant`]).
    pub host_state_participant: Option<Arc<dyn PgHostStateParticipant>>,
}

impl std::fmt::Debug for StorageParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageParts")
            .field("has_vector_index", &self.vector_index.is_some())
            .field(
                "has_host_state_participant",
                &self.host_state_participant.is_some(),
            )
            .finish()
    }
}

/// Pure, validated runtime config.
///
/// The booleans are independent deployment gates — expose the network,
/// serve `/v1`, allow insecure single-owner — not the
/// states of one machine. Folding them into enums would invent
/// relationships between them that do not exist, and each already carries
/// its own env var and its own validation rule.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone)]
pub struct RuntimeConfig {
    /// The storage settings, which [`RuntimeBuilder::resolve_storage`]
    /// resolves alone.
    pub storage: StorageConfig,
    pub s3: Option<S3RuntimeConfig>,
    pub owner: Option<OwnerId>,
    pub mcp: Option<McpSettings>,
    pub expose_network: bool,
    /// CORS origin allowlist for exposed MCP. A portless entry accepts any
    /// port; an explicit port also matches a browser `Origin` that omits it
    /// when it is the scheme default (HTTP 80 or HTTPS 443).
    pub allowed_origins: Vec<String>,
    /// Explicit inbound `Host` allowlist (`PROXIIMA_ALLOWED_HOSTS`). Empty
    /// ⇒ derive from `resource_metadata.public_url` + `allowed_origins`.
    /// Bare hostnames or `host:port`; the listener always adds loopback and
    /// shares the resulting non-empty allowlist with rmcp's inner guard.
    pub allowed_hosts: Vec<String>,
    pub served_surface: ServedSurface,
    pub stream_revalidation: RevalidationConfig,
    pub insecure_single_owner: bool,
    /// Serve `/v1` beside `/mcp` on the same listener, inside the same Host,
    /// auth, and body-limit layers (docs/protocol.md). Two gates, both required: the
    /// `rest` cargo feature compiles the module, this flag serves it. A second
    /// transport projection is opt-in twice on purpose — compiling it is a
    /// build decision, exposing it is a deployment one.
    pub rest_enabled: bool,
    /// Inbound headers copied to tools as `RequestHeaders`
    /// (`PROXIIMA_REQUEST_HEADERS`). Empty publishes nothing.
    pub request_headers: RequestHeaderAllowlist,
    /// Trusted forwarder subjects and their fixed verified-owner role
    /// (`PROXIIMA_FORWARDER_*`), wrapped around the owner-access port.
    pub forwarder: Option<ForwarderPolicy>,
    /// Anonymous `/healthz` and `/readyz` on the MCP listener
    /// (`PROXIIMA_HEALTH_ENDPOINTS`).
    pub health_endpoints: bool,
    /// rmcp tuning plus the one body cap both the listener guard and rmcp
    /// enforce (`PROXIIMA_MAX_REQUEST_BODY_BYTES`, `PROXIIMA_MCP_*`).
    pub mcp_transport: McpTransportConfig,
    pub auth: RuntimeAuthState,
    pub resource_metadata: Option<ResourceServerMetadata>,
    /// The deployment's `CloudEvents` producer identity and capture bounds
    /// (`PROXIIMA_PUBLICATION_SOURCE`, `PROXIIMA_OUTBOX_*`; docs/host-integration.md).
    ///
    /// Always present, never optional: it is handed to the engine builder
    /// on every boot, and its `limits` are the ones enforced at capture —
    /// no backend holds a second copy.
    pub publication: PublicationConfig,
    /// How long a DELIVERED record is kept before the publisher task
    /// reclaims it (`PROXIIMA_OUTBOX_PUBLISHED_RETENTION_SECS`; docs/host-integration.md
    /// §Retention).
    ///
    /// Seven days unless set; `None` keeps published records forever, which
    /// `0` spells explicitly. Host housekeeping, deliberately NOT part of [`Self::publication`]:
    /// the engine never sees it, and the only port that can act on it is
    /// host-held.
    pub published_retention: Option<Duration>,
    /// How often the runtime sweeps expired uploads and drains object purge
    /// debt while an object store is configured
    /// (`PROXIIMA_OBJECT_PURGE_INTERVAL_SECS`, default 60 s). `None` when
    /// turned off with `0`.
    pub object_purge_interval: Option<Duration>,
    /// The host intakes the captured outbox is delivered to, in this
    /// process, by consumer name ([`RuntimeBuilder::outbox_consumer`],
    /// [`RuntimeBuilder::outbox_intake`]).
    ///
    /// Empty — the default — leaves the outbox captured and undrained,
    /// which is a safe state: nothing is lost, the backlog is bounded, and
    /// a later boot with an intake delivers the same records.
    pub outbox_consumers: OutboxConsumers,
}

/// The configured outbox consumers, by name, in name order.
#[derive(Clone, Default)]
pub struct OutboxConsumers(std::collections::BTreeMap<ConsumerName, Arc<dyn DurableIntake>>);

impl OutboxConsumers {
    /// Whether any consumer is configured: the outbox publisher's start
    /// rule, and what makes `core/goal-change-v1` listenable.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every configured consumer and its intake, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&ConsumerName, &Arc<dyn DurableIntake>)> {
        self.0.iter()
    }

    /// The configured names, in order.
    pub fn names(&self) -> impl Iterator<Item = &ConsumerName> {
        self.0.keys()
    }

    /// `self`, with every name `overlay` configures replaced by its intake.
    fn overlaid_by(mut self, overlay: Self) -> Self {
        self.0.extend(overlay.0);
        self
    }
}

impl std::fmt::Debug for OutboxConsumers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.keys()).finish()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RuntimeAuthState {
    pub has_host_authenticator: bool,
}

impl std::fmt::Debug for RuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeConfig")
            .field("storage", &self.storage)
            .field("s3", &self.s3)
            .field("owner", &self.owner)
            .field("mcp", &self.mcp)
            .field("expose_network", &self.expose_network)
            .field("allowed_origins", &self.allowed_origins)
            .field("allowed_hosts", &self.allowed_hosts)
            .field("served_surface", &self.served_surface)
            .field("stream_revalidation", &self.stream_revalidation)
            .field("insecure_single_owner", &self.insecure_single_owner)
            .field("rest_enabled", &self.rest_enabled)
            .field("request_headers", &self.request_headers)
            .field("forwarder", &self.forwarder)
            .field("health_endpoints", &self.health_endpoints)
            .field("mcp_transport", &self.mcp_transport)
            .field("auth", &self.auth)
            .field("resource_metadata", &self.resource_metadata)
            .field("publication", &self.publication)
            .field("published_retention", &self.published_retention)
            .field("object_purge_interval", &self.object_purge_interval)
            .field("outbox_consumers", &self.outbox_consumers)
            .finish_non_exhaustive()
    }
}

impl RuntimeConfig {
    /// Validate the fail-closed network/auth matrix.
    ///
    /// # Errors
    ///
    /// Returns `ProxiimaError::Security` when transport exposure is unsafe,
    /// and `ProxiimaError::Config` for an invalid pool configuration or a
    /// migration URL that does not name the platform URL's role and database.
    pub fn validate(&self) -> Result<(), ProxiimaError> {
        self.storage.validate()?;
        let Some(mcp) = &self.mcp else {
            return Ok(());
        };

        if self.insecure_single_owner {
            return Err(ProxiimaError::Security(
                "insecure single-owner mode cannot serve MCP; configure host auth".into(),
            ));
        }
        if self.expose_network && self.allowed_origins.is_empty() {
            return Err(ProxiimaError::Security(
                "network exposure requires at least one allowed origin".into(),
            ));
        }
        if self.expose_network && self.allowed_origins.iter().any(|origin| origin == "*") {
            return Err(ProxiimaError::Security(
                "network exposure forbids wildcard allowed origins".into(),
            ));
        }
        if self.allowed_hosts.iter().any(|host| host.contains('*')) {
            // The shared Host guard has no wildcard semantics — a `*` entry
            // matches nothing and fails closed, locking the operator out silently.
            // Reject it loudly instead of letting it look like "allow all".
            return Err(ProxiimaError::Security(
                "MCP Host allowlist forbids wildcard allowed hosts; list each host explicitly \
                 (PROXIIMA_ALLOWED_HOSTS), or rely on PROXIIMA_PUBLIC_URL"
                    .into(),
            ));
        }
        if self.expose_network && self.public_allowed_hosts().is_empty() {
            return Err(ProxiimaError::Security(
                "network exposure requires a resolvable public host: set PROXIIMA_ALLOWED_HOSTS, \
                 PROXIIMA_PUBLIC_URL, or a non-loopback host in PROXIIMA_ALLOWED_ORIGINS"
                    .into(),
            ));
        }
        if self.expose_network && !self.auth.has_host_authenticator {
            return Err(ProxiimaError::Security(
                "network exposure requires a host authenticator".into(),
            ));
        }
        if !self.expose_network && !mcp.bind.ip().is_loopback() {
            return Err(ProxiimaError::Security(
                "non-exposed MCP bind must be loopback".into(),
            ));
        }
        if !self.auth.has_host_authenticator {
            return Err(ProxiimaError::Security(
                "MCP requires a host authenticator".into(),
            ));
        }

        Ok(())
    }

    /// Non-loopback public hosts for the inbound `Host` allowlist.
    ///
    /// Explicit `allowed_hosts` (`PROXIIMA_ALLOWED_HOSTS`) win verbatim;
    /// otherwise hosts are derived from `resource_metadata.public_url`
    /// (i.e. `PROXIIMA_PUBLIC_URL`) and `allowed_origins`, with loopback
    /// dropped — loopback is added unconditionally by `HostAllowlist`, so
    /// it never counts as a resolvable *public* host here. Empty ⇒ a
    /// network-exposed deployment would 403 every real request.
    #[must_use]
    pub fn public_allowed_hosts(&self) -> Vec<String> {
        if !self.allowed_hosts.is_empty() {
            return dedup_hosts(
                self.allowed_hosts
                    .iter()
                    .map(|host| host.trim().to_ascii_lowercase()),
            );
        }
        let derived = self
            .resource_metadata
            .as_ref()
            .and_then(|md| host_of_url(&md.public_url))
            .into_iter()
            .chain(self.allowed_origins.iter().filter_map(|o| host_of_url(o)))
            .filter(|host| !is_loopback_host(host));
        dedup_hosts(derived)
    }
}

/// MCP transport settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpSettings {
    pub bind: SocketAddr,
}

/// Host-provided runtime objects kept out of pure config.
#[derive(Clone, Default)]
pub struct RuntimeParts {
    pub authenticator: Option<Arc<dyn Authenticator>>,
    /// Builds [`Self::authenticator`] at boot from the platform scope.
    pub platform_authenticator: Option<PlatformAuthenticatorFactory>,
    /// Host tools served on `/mcp` ([`RuntimeBuilder::host_tools`]).
    pub host_tools: Option<Arc<dyn HostTools>>,
    /// The parts storage boot reads.
    pub storage: StorageParts,
    pub embed_client: Option<Arc<dyn EmbeddingClient>>,
    pub embedding_router: Option<Arc<dyn EmbeddingRouter>>,
    /// The host's owner-access port; `None` uses the runtime-pool resolver.
    pub owner_access: Option<Arc<dyn OwnerAccessPort>>,
    /// The host's personal-owner drop proof ([`RuntimeBuilder::owner_drop_proof`]).
    pub owner_drop_proof: Option<OwnerDropProofHandle>,
    /// The merged host service bag ([`RuntimeBuilder::services`]).
    pub services: SpiiceServices,
    /// Bound at boot to the runtime's owner-access port when the
    /// authenticator came from the `PROXIIMA_OIDC_*` environment.
    pub(crate) late_owner_access: Option<LateOwnerAccess>,
}

impl RuntimeParts {
    /// An authenticator is installed, or built at boot from the platform
    /// scope.
    #[must_use]
    pub fn has_authenticator(&self) -> bool {
        self.authenticator.is_some() || self.platform_authenticator.is_some()
    }
}

impl std::fmt::Debug for RuntimeParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeParts")
            .field("has_authenticator", &self.authenticator.is_some())
            .field(
                "has_platform_authenticator",
                &self.platform_authenticator.is_some(),
            )
            .field("host_tools", &self.host_tools)
            .field("storage", &self.storage)
            .field("has_embed_client", &self.embed_client.is_some())
            .field("has_embedding_router", &self.embedding_router.is_some())
            .field("has_owner_access", &self.owner_access.is_some())
            .field("has_owner_drop_proof", &self.owner_drop_proof.is_some())
            .field("services", &self.services)
            .field("env_authenticator", &self.late_owner_access.is_some())
            .finish()
    }
}

/// The error a facade failure wraps. A message alone converts with
/// `.into()`.
pub type Cause = Box<dyn std::error::Error + Send + Sync>;

/// Name a failure in the facade's words and keep the failure as the source.
pub fn cause_with(
    message: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> Cause {
    #[derive(Debug)]
    struct Named {
        message: String,
        source: Cause,
    }
    impl std::fmt::Display for Named {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.message)
        }
    }
    impl std::error::Error for Named {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.source.as_ref())
        }
    }
    Box::new(Named {
        message: message.into(),
        source: Box::new(source),
    })
}

/// Errors from the application-facing facade.
#[derive(Debug, thiserror::Error)]
pub enum ProxiimaError {
    #[error("config: {0}")]
    Config(String),
    #[error("registry: {0}")]
    Registry(#[from] proxiima_core::SpiiceRegistryError),
    #[error("spiice services: {0}")]
    SpiiceServices(#[from] SpiiceServiceError),
    /// Carries the error the storage layer returned, so a host can walk
    /// [`std::error::Error::source`]; the message reads as that error does.
    #[error("storage: {0}")]
    Storage(#[source] Cause),
    #[error("engine: {0}")]
    Engine(#[source] Cause),
    #[error("security: {0}")]
    Security(String),
    #[error("mcp: {0}")]
    Mcp(#[source] Cause),
    /// The target database does not match this binary's schema and must
    /// be reset. Distinct from [`Self::Storage`] so hosts can match on it.
    #[error(
        "database schema does not match this binary; reset required (see docs/contributing.md#migrations): {details}"
    )]
    SchemaResetRequired { details: String },
    /// The target database is behind this binary: a migration is not
    /// applied, or the store number was never initialized. Serving and
    /// maintenance only validate; the host's `migrate` step brings the
    /// database current ([`crate::Proxiima::migrate`]).
    #[error(
        "database is not migrated for this binary; run the host's `migrate` command first (docs/operate.md#postgresql-and-migrations): {details}"
    )]
    MigrationRequired { details: String },
}

fn parse_allowed_origins(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_allowed_hosts(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Parse an explicitly configured missing-version policy.
///
/// `PROXIIMA_MISSING_MIGRATION_POLICY` accepts `reject` or `allow`. An unset
/// value returns `None` so configuration layers preserve the host's choice;
/// callers without layers use [`MissingMigrationPolicy::default`].
///
/// # Errors
///
/// Returns [`ProxiimaError::Config`] for any other nonempty value.
pub fn missing_migration_policy_from_lookup(
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<Option<MissingMigrationPolicy>, ProxiimaError> {
    settings::PROXIIMA_MISSING_MIGRATION_POLICY
        .read(lookup)
        .map(|raw| match raw.as_str() {
            "reject" => Ok(MissingMigrationPolicy::Reject),
            "allow" => Ok(MissingMigrationPolicy::Allow),
            _ => Err(ProxiimaError::Config(
                "PROXIIMA_MISSING_MIGRATION_POLICY must be reject or allow".into(),
            )),
        })
        .transpose()
}

/// Extract the bare, lowercased host from a URL or origin — scheme,
/// userinfo, port, and path stripped; IPv6 brackets removed. Returns
/// `None` for input with no host part.
fn host_of_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let after_scheme = raw.split_once("://").map_or(raw, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split_once(']').map(|(host, _)| host)?
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    let host = host.trim().to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

fn dedup_hosts(hosts: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    hosts
        .into_iter()
        .filter(|host| !host.is_empty())
        .filter(|host| seen.insert(host.clone()))
        .collect()
}

pub(crate) fn parse_duration_seconds(key: &Setting, raw: &str) -> Result<Duration, ProxiimaError> {
    let trimmed = raw.trim();
    let seconds = trimmed.parse::<u64>().map_err(|_| {
        ProxiimaError::Config(format!("{key} must be integer seconds, got {raw:?}"))
    })?;
    Ok(Duration::from_secs(seconds))
}

/// The variables the environment layer captures for the OIDC authenticator
/// built at resolve.
#[cfg(feature = "auth-oidc")]
fn oidc_env_keys() -> &'static [Setting] {
    &crate::auth::OIDC_ENV_KEYS
}

/// Only the issuer: its presence is what a build without `auth-oidc` refuses.
#[cfg(not(feature = "auth-oidc"))]
fn oidc_env_keys() -> &'static [Setting] {
    &[settings::PROXIIMA_OIDC_ISSUER]
}

/// The `PROXIIMA_MCP_*` transport block: `None` when none is set, else
/// rmcp's defaults with the set values applied. Seconds of `0` disable the
/// SSE ping, the session idle timeout, or the retry hint.
fn mcp_transport_from_lookup(
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<Option<McpTransportConfig>, ProxiimaError> {
    use settings::{
        PROXIIMA_MCP_JSON_RESPONSE as JSON_RESPONSE,
        PROXIIMA_MCP_SESSION_IDLE_SECS as SESSION_IDLE, PROXIIMA_MCP_SESSIONS as SESSIONS,
        PROXIIMA_MCP_SSE_KEEP_ALIVE_SECS as KEEP_ALIVE, PROXIIMA_MCP_SSE_RETRY_SECS as RETRY,
    };
    if [KEEP_ALIVE, SESSION_IDLE, RETRY, SESSIONS, JSON_RESPONSE]
        .iter()
        .all(|setting| setting.read(lookup).is_none())
    {
        return Ok(None);
    }
    let optional_secs = |setting: &Setting| -> Result<Option<Option<Duration>>, ProxiimaError> {
        setting
            .read(lookup)
            .map(|raw| parse_duration_seconds(setting, &raw))
            .transpose()
            .map(|parsed| parsed.map(|duration| (!duration.is_zero()).then_some(duration)))
    };
    let mut transport = McpTransportConfig::default();
    if let Some(keep_alive) = optional_secs(&KEEP_ALIVE)? {
        transport.sse_keep_alive = keep_alive;
    }
    if let Some(idle) = optional_secs(&SESSION_IDLE)? {
        transport.session_idle_timeout = idle;
    }
    if let Some(retry) = optional_secs(&RETRY)? {
        transport.sse_retry = retry;
    }
    if let Some(raw) = SESSIONS.read(lookup) {
        transport.legacy_session_mode = parse_bool_value(&SESSIONS, &raw)?;
    }
    if let Some(raw) = JSON_RESPONSE.read(lookup) {
        transport.json_response = parse_bool_value(&JSON_RESPONSE, &raw)?;
    }
    Ok(Some(transport))
}

fn embedding_runtime_policy_env_is_set(lookup: &impl Fn(&str) -> Option<String>) -> bool {
    use proxiima_core::env::{
        PROXIIMA_EMBED_BATCH_SIZE, PROXIIMA_EMBED_DRAIN_CONCURRENCY,
        PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS, PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS,
        PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS,
    };
    [
        PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS,
        PROXIIMA_EMBED_BATCH_SIZE,
        PROXIIMA_EMBED_DRAIN_CONCURRENCY,
        PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS,
        PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS,
    ]
    .iter()
    .any(|setting| setting.read(lookup).is_some())
}

/// `PROXIIMA_MIGRATION_DATABASE_URL` is another way to reach the platform
/// scope, not another scope: migrations create objects owned by the role
/// they run as, and the platform role owns the schema. Refuse a URL that
/// names another role or database; host, port and connection parameters
/// may differ. Neither URL is echoed: it could carry a password.
fn check_migration_database_url(migration: &str, platform: &str) -> Result<(), ProxiimaError> {
    use std::str::FromStr as _;

    let parse = |url: &str, variable: &str| {
        sqlx::postgres::PgConnectOptions::from_str(url)
            .map_err(|_| ProxiimaError::Config(format!("{variable} is not a valid Postgres URL")))
    };
    let migration = parse(migration, "PROXIIMA_MIGRATION_DATABASE_URL")?;
    let platform = parse(platform, "PROXIIMA_PLATFORM_DATABASE_URL")?;
    if migration.get_username() != platform.get_username() {
        return Err(ProxiimaError::Config(
            "PROXIIMA_MIGRATION_DATABASE_URL must connect as the role of \
             PROXIIMA_PLATFORM_DATABASE_URL (the schema owner)"
                .into(),
        ));
    }
    if migration.get_database() != platform.get_database() {
        return Err(ProxiimaError::Config(
            "PROXIIMA_MIGRATION_DATABASE_URL must name the database of \
             PROXIIMA_PLATFORM_DATABASE_URL"
                .into(),
        ));
    }
    Ok(())
}

/// Parse the host-assigned store number at the configuration boundary.
///
/// # Errors
///
/// Returns `ProxiimaError::Config` for a non-integer or a value outside `u16`.
pub fn parse_store_number(raw: &str) -> Result<u16, ProxiimaError> {
    raw.trim().parse().map_err(|_| {
        ProxiimaError::Config("PROXIIMA_STORE_NUMBER must be an integer from 0 to 65535".into())
    })
}

/// Parse the canonical generic embedding policy block through an injected
/// lookup. Shared by the facade env layer and host binaries that must build a
/// concrete embedding adapter before calling the facade.
///
/// # Errors
///
/// Rejects malformed, zero, out-of-range, and unsafe policy values.
pub fn embedding_runtime_policy_from_lookup(
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<EmbeddingRuntimePolicy, ProxiimaError> {
    EmbeddingRuntimePolicy::from_lookup(lookup)
        .map_err(|err| ProxiimaError::Config(err.to_string()))
}

fn validate_revalidation_config(config: RevalidationConfig) -> Result<(), ProxiimaError> {
    if config.max_stream_lifetime.is_zero() {
        return Err(ProxiimaError::Config(
            "stream max lifetime must be greater than 0 seconds".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "runtime_config_tests.rs"]
mod tests;
