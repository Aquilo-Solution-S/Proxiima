use std::net::{IpAddr, Ipv4Addr};

use async_trait::async_trait;
use proxiima_core::env_from_pairs as lookup;
use proxiima_core::{AuthError, AuthzContext, Credentials, GroupId, OwnerId};
use proxiima_mcp_server::AuthorizationServers;

use super::*;

#[test]
fn a_wrapped_failure_keeps_its_message_and_its_source() {
    let io = std::io::Error::other("disk gone");
    let error = ProxiimaError::Storage(cause_with("reading the ledger", io));
    assert_eq!(error.to_string(), "storage: reading the ledger");
    let cause = std::error::Error::source(&error).expect("the cause is the source");
    assert_eq!(
        std::error::Error::source(cause)
            .expect("the failure")
            .to_string(),
        "disk gone"
    );
    let plain = ProxiimaError::Mcp("listener disabled".into());
    assert_eq!(plain.to_string(), "mcp: listener disabled");
}

#[test]
fn missing_migrations_require_an_explicit_host_policy() {
    let resolve = |builder: RuntimeBuilder| {
        builder
            .database_url("postgres://runtime/proxiima")
            .store_number(0)
            .platform_database_url("postgres://platform/proxiima")
            .served_surface(ServedSurface::All)
            .resolve()
            .expect("host config")
            .0
    };
    assert_eq!(
        resolve(RuntimeBuilder::default())
            .storage
            .missing_migration_policy,
        MissingMigrationPolicy::Reject
    );
    let rolling =
        || RuntimeBuilder::default().missing_migration_policy(MissingMigrationPolicy::Allow);
    assert_eq!(
        resolve(rolling()).storage.missing_migration_policy,
        MissingMigrationPolicy::Allow
    );
    assert_eq!(
        resolve(RuntimeBuilder::default().merge_over(rolling()))
            .storage
            .missing_migration_policy,
        MissingMigrationPolicy::Allow
    );
    assert_eq!(
        resolve(
            RuntimeBuilder::default()
                .missing_migration_policy(MissingMigrationPolicy::Reject)
                .merge_over(rolling())
        )
        .storage
        .missing_migration_policy,
        MissingMigrationPolicy::Reject
    );
}

#[test]
fn missing_migration_policy_is_parsed_once_at_the_configuration_boundary() {
    for (raw, expected) in [
        ("allow", Some(MissingMigrationPolicy::Allow)),
        (" reject ", Some(MissingMigrationPolicy::Reject)),
        (" ", None),
    ] {
        let configured = [("PROXIIMA_MISSING_MIGRATION_POLICY", raw)];
        let from = lookup(&configured);
        assert_eq!(
            missing_migration_policy_from_lookup(&from).expect("valid policy"),
            expected
        );
        assert_eq!(
            RuntimeBuilder::default()
                .apply_lookup(&from)
                .expect("valid configuration")
                .missing_migration_policy,
            expected
        );
    }
    let invalid = lookup(&[("PROXIIMA_MISSING_MIGRATION_POLICY", "sometimes")]);
    assert!(missing_migration_policy_from_lookup(&invalid).is_err());
    assert!(RuntimeBuilder::default().apply_lookup(&invalid).is_err());
    assert_eq!(
        RuntimeBuilder::default()
            .missing_migration_policy(MissingMigrationPolicy::Reject)
            .apply_lookup(&invalid)
            .expect("explicit host choice wins")
            .missing_migration_policy,
        Some(MissingMigrationPolicy::Reject)
    );
}

#[test]
fn store_number_is_required_and_parsed_at_configuration() {
    let base = || {
        RuntimeBuilder::default()
            .database_url("postgres://unused/db")
            .platform_database_url("postgres://unused/platform")
            .served_surface(ServedSurface::All)
    };
    let missing = base().resolve().expect_err("a store needs a number");
    assert!(missing.to_string().contains("PROXIIMA_STORE_NUMBER"));
    for raw in ["-1", "65536", "three", "1.5"] {
        let error = base()
            .apply_lookup(|key| (key == "PROXIIMA_STORE_NUMBER").then(|| raw.to_owned()))
            .expect_err("invalid outside store number");
        assert!(
            error.to_string().contains("PROXIIMA_STORE_NUMBER"),
            "{error}"
        );
    }
    for number in [0, 3, u16::MAX] {
        let (config, _) = base()
            .apply_lookup(|key| (key == "PROXIIMA_STORE_NUMBER").then(|| number.to_string()))
            .expect("valid store number")
            .resolve()
            .expect("number supplied");
        assert_eq!(config.storage.store_number, number);
    }
    let (config, _) = base()
        .store_number(3)
        .apply_lookup(|key| (key == "PROXIIMA_STORE_NUMBER").then(|| "4".into()))
        .expect("explicit value takes precedence")
        .resolve()
        .expect("complete config");
    assert_eq!(config.storage.store_number, 3);
}

#[test]
fn platform_database_url_is_required_at_resolve() {
    let error = RuntimeBuilder::default()
        .database_url("postgres://runtime/proxiima")
        .served_surface(ServedSurface::All)
        .resolve()
        .expect_err("owner-RLS boot must require the platform URL");
    assert!(
        error.to_string().contains("PROXIIMA_PLATFORM_DATABASE_URL"),
        "{error}"
    );
}
fn with_platform(builder: RuntimeBuilder) -> RuntimeBuilder {
    builder
        .database_url("postgres://runtime@db/proxiima")
        .platform_database_url("postgres://platform@pooler:6432/proxiima")
        .store_number(0)
        .served_surface(ServedSurface::All)
}

#[test]
fn migrations_use_the_platform_url_unless_a_migration_url_is_set() {
    let (config, _) = with_platform(RuntimeBuilder::default())
        .resolve()
        .expect("resolves");
    assert_eq!(config.storage.migration_database_url, None);
    assert_eq!(
        config.storage.migrations_url(),
        "postgres://platform@pooler:6432/proxiima"
    );

    let (config, _) = with_platform(
        RuntimeBuilder::default()
            .apply_lookup(lookup(&[(
                "PROXIIMA_MIGRATION_DATABASE_URL",
                "postgres://platform@db:5432/proxiima",
            )]))
            .expect("env layer"),
    )
    .resolve()
    .expect("a direct URL for the same role and database resolves");
    assert_eq!(
        config.storage.migrations_url(),
        "postgres://platform@db:5432/proxiima"
    );
    assert_eq!(
        config.storage.platform_database_url,
        "postgres://platform@pooler:6432/proxiima"
    );
    assert!(
        !format!("{config:?}").contains("db:5432"),
        "Debug redacts the migration URL"
    );
}

#[test]
fn storage_resolves_without_the_serving_settings_that_refuse_a_full_resolve() {
    let builder = RuntimeBuilder::default()
        .database_url("postgres://runtime@db/proxiima")
        .platform_database_url("postgres://platform@db/proxiima")
        .store_number(5)
        .runtime_grants(true)
        .mcp_bind(addr([127, 0, 0, 1]));

    let error = builder.clone().resolve().expect_err("serving is refused");
    assert!(error.to_string().contains("served_surface is required"));

    let (storage, parts) = builder.resolve_storage().expect("storage resolves");
    assert_eq!(storage.store_number, 5);
    assert_eq!(storage.database_url, "postgres://runtime@db/proxiima");
    assert_eq!(
        storage.platform_database_url,
        "postgres://platform@db/proxiima"
    );
    assert!(storage.runtime_grants);
    assert!(parts.vector_index.is_none() && parts.host_state_participant.is_none());

    let error = RuntimeBuilder::default()
        .database_url("postgres://runtime@db/proxiima")
        .platform_database_url("postgres://platform@db/proxiima")
        .store_number(5)
        .mcp_bind(addr([127, 0, 0, 1]))
        .served_surface(ServedSurface::All)
        .resolve()
        .expect_err("MCP without an authenticator is refused");
    assert!(
        error
            .to_string()
            .contains("MCP requires a host authenticator")
    );
}

#[test]
fn storage_resolution_refuses_bad_storage_input() {
    let complete = || {
        RuntimeBuilder::default()
            .database_url("postgres://runtime@db/proxiima")
            .platform_database_url("postgres://platform@db/proxiima")
            .store_number(0)
    };
    let error = complete()
        .migration_database_url("postgres://admin@db:5432/proxiima")
        .resolve_storage()
        .expect_err("a migration URL naming another role is refused");
    assert!(
        error
            .to_string()
            .contains("PROXIIMA_MIGRATION_DATABASE_URL")
    );

    let error = RuntimeBuilder::default()
        .platform_database_url("postgres://platform@db/proxiima")
        .store_number(0)
        .resolve_storage()
        .expect_err("a database URL is required");
    assert!(error.to_string().contains("DATABASE_URL is required"));
}

#[test]
fn a_migration_url_must_name_the_platform_role_and_database() {
    for (url, names) in [
        ("postgres://admin@db:5432/proxiima", "role"),
        ("postgres://platform@db:5432/other", "database"),
        ("not a url", "valid Postgres URL"),
    ] {
        let error = with_platform(RuntimeBuilder::default().migration_database_url(url))
            .resolve()
            .expect_err("refused");
        let message = error.to_string();
        assert!(
            message.contains("PROXIIMA_MIGRATION_DATABASE_URL") && message.contains(names),
            "{url}: {message}"
        );
        assert!(
            !message.contains("db:5432"),
            "the URL is not echoed: {message}"
        );
    }
}

use crate::company_owner;

fn owner(id: uuid::Uuid) -> OwnerId {
    OwnerId::from(GroupId::new(id))
}

/// Every declared default of the facade is what its readers do unset.
///
/// The builder records which fields the environment set, so the
/// comparison is of the resolved [`RuntimeConfig`], whose fields are the
/// effective values (its `Debug` prints every one; the URLs, which carry
/// no default, are redacted). The two OIDC rows with defaults are read
/// by the authenticator, which needs a reachable issuer and an owner
/// port to build; their two readers are compared directly instead.
#[cfg(feature = "auth-oidc")]
#[test]
fn declared_defaults_are_what_the_readers_do_unset() {
    let base = [
        ("DATABASE_URL", "postgres://localhost/proxiima"),
        (
            "PROXIIMA_PLATFORM_DATABASE_URL",
            "postgres://localhost/platform",
        ),
        ("PROXIIMA_STORE_NUMBER", "0"),
    ];
    let id = uuid::Uuid::now_v7();
    proxiima_core::test_fixtures::assert_declared_defaults(settings::SETTINGS, &base, |lookup| {
        let config = RuntimeBuilder::default()
            .owner(owner(id))
            .served_surface(ServedSurface::All)
            .apply_lookup(lookup)
            .and_then(RuntimeBuilder::resolve)
            .map(|(config, _)| config);
        format!(
            "{config:?} {:?} {:?}",
            crate::auth::oidc_http_timeout(&lookup),
            crate::auth::oidc_require_access_token_typ(&lookup),
        )
    });
}

struct NoopParticipant;

#[async_trait]
impl PgHostStateParticipant for NoopParticipant {
    fn participant_id(&self) -> proxiima_core::storage_ports::HostStateParticipantId {
        proxiima_core::storage_ports::HostStateParticipantId::new("noop")
    }

    fn declared_tables(&self) -> &'static [proxiima_core::storage_ports::StateSurfaceName] {
        &[]
    }

    async fn apply(
        &self,
        _session: &mut proxiima_storage_pg::PgHostStateSession<'_, '_>,
        _permit: &proxiima_core::storage_ports::HostStateWritePermit,
        _request: proxiima_core::storage_ports::HostStateRequest,
    ) -> Result<proxiima_core::storage_ports::HostStateReply, proxiima_core::StorageError> {
        unreachable!("registration-only fixture")
    }
}

#[test]
fn a_second_host_state_participant_refuses_instead_of_replacing_the_first() {
    let with = |builder: RuntimeBuilder| builder.host_state_participant(Arc::new(NoopParticipant));
    let base = || {
        RuntimeBuilder::default()
            .database_url("postgres://localhost/proxiima")
            .store_number(0)
            .platform_database_url("postgres://localhost/platform")
            .served_surface(ServedSurface::All)
    };

    let (_, parts) = with(base()).resolve().expect("one participant resolves");
    assert!(parts.storage.host_state_participant.is_some());

    let twice = with(with(base()))
        .resolve()
        .expect_err("two on one builder");
    assert!(twice.to_string().contains("already registered"), "{twice}");

    // An overlay's participant over a `SpiiceApp::configure` one, as a
    // tuple app or `RuntimeBuilder::host_state_participant` composes them.
    let layered = with(RuntimeBuilder::default())
        .merge_over(with(base()))
        .resolve()
        .expect_err("two across layers");
    assert!(
        layered.to_string().contains("already registered"),
        "{layered}"
    );
}

/// The built-in HTTP sink (#462): registered as `http` only when its URL
/// is set; a bad URL fails boot; a host consumer named `http` clashes.
#[test]
fn the_http_outbox_sink_registers_only_when_its_url_is_set() {
    let base = || {
        RuntimeBuilder::default()
            .database_url("postgres://localhost/proxiima")
            .store_number(0)
            .platform_database_url("postgres://localhost/platform")
            .served_surface(ServedSurface::All)
    };
    let names = |config: &RuntimeConfig| {
        config
            .outbox_consumers
            .names()
            .map(|name| name.as_str().to_owned())
            .collect::<Vec<_>>()
    };

    let (unset, _) = base()
        .apply_lookup(lookup(&[]))
        .expect("no outbox env parses")
        .resolve()
        .expect("resolves");
    assert!(names(&unset).is_empty(), "URL unset: no consumer");

    let (set, _) = base()
        .apply_lookup(lookup(&[(
            "PROXIIMA_OUTBOX_HTTP_URL",
            "https://receiver.example/events",
        )]))
        .expect("a URL parses")
        .resolve()
        .expect("resolves");
    assert_eq!(names(&set), ["http"]);

    for bad in ["not a url", "ftp://receiver.example/", "https://"] {
        base()
            .apply_lookup(lookup(&[("PROXIIMA_OUTBOX_HTTP_URL", bad)]))
            .expect_err(bad);
    }
    let clear_text_token = base()
        .apply_lookup(lookup(&[
            ("PROXIIMA_OUTBOX_HTTP_URL", "http://receiver.example/"),
            ("PROXIIMA_OUTBOX_HTTP_TOKEN_FILE", "/nonexistent"),
        ]))
        .expect_err("a token is never sent over http://");
    assert!(
        clear_text_token.to_string().contains("https"),
        "{clear_text_token}"
    );
    let (plaintext, _) = base()
        .apply_lookup(lookup(&[
            ("PROXIIMA_OUTBOX_HTTP_URL", "http://receiver.example/"),
            ("PROXIIMA_OUTBOX_HTTP_TOKEN_FILE", "/nonexistent"),
            ("PROXIIMA_PLAINTEXT_HOSTS", "receiver.example"),
        ]))
        .expect("a named host takes a token over http://")
        .resolve()
        .expect("resolves");
    assert_eq!(names(&plaintext), ["http"]);
    for hosts in ["*", "10.0.0.0/8"] {
        base()
            .apply_lookup(lookup(&[("PROXIIMA_PLAINTEXT_HOSTS", hosts)]))
            .expect_err("a bad plaintext host fails boot without a sink");
    }
    base()
        .apply_lookup(lookup(&[
            ("PROXIIMA_OUTBOX_HTTP_URL", "https://receiver.example/"),
            ("PROXIIMA_OUTBOX_HTTP_CA_FILE", "/nonexistent/ca.pem"),
        ]))
        .expect_err("an unreadable CA fails boot");

    let clash = base()
        .outbox_consumer(
            crate::outbox_http::consumer_name(),
            Arc::new(crate::outbox_http::tests::Never),
        )
        .apply_lookup(lookup(&[(
            "PROXIIMA_OUTBOX_HTTP_URL",
            "https://receiver.example/events",
        )]))
        .expect("parses")
        .resolve()
        .expect_err("a host consumer named http clashes");
    assert!(clash.to_string().contains("`http`"), "{clash}");
}

/// MCP's start rule at resolve: a bind address is the whole switch.
#[test]
fn mcp_is_configured_exactly_when_a_bind_address_is() {
    let base = || {
        RuntimeBuilder::default()
            .database_url("postgres://localhost/proxiima")
            .store_number(0)
            .platform_database_url("postgres://localhost/platform")
            .served_surface(ServedSurface::All)
            .authenticator(Arc::new(TestAuthenticator))
    };
    let (off, _) = base().resolve().expect("no bind resolves");
    assert!(off.mcp.is_none(), "no bind address means MCP is off");

    let (from_env, _) = base()
        .apply_lookup(lookup(&[("PROXIIMA_MCP_BIND", "127.0.0.1:0")]))
        .expect("bind env parses")
        .resolve()
        .expect("env bind resolves");
    assert_eq!(
        from_env.mcp.map(|mcp| mcp.bind),
        Some(addr_port([127, 0, 0, 1], 0))
    );

    // Code wins over the environment.
    let (code, _) = base()
        .mcp_bind(addr_port([127, 0, 0, 1], 7))
        .apply_lookup(lookup(&[("PROXIIMA_MCP_BIND", "127.0.0.1:0")]))
        .expect("bind env parses")
        .resolve()
        .expect("code bind resolves");
    assert_eq!(
        code.mcp.map(|mcp| mcp.bind),
        Some(addr_port([127, 0, 0, 1], 7))
    );
}

/// The storage block reads the storage settings and leaves every serving
/// setting unread: serving values that are malformed, or that serving
/// refuses (an issuer without its block, or in a build without
/// `auth-oidc`), neither fail nor configure it.
#[test]
fn the_storage_lookup_reads_storage_settings_and_no_serving_setting() {
    let env = lookup(&[
        ("DATABASE_URL", "postgres://runtime@localhost/db"),
        (
            "PROXIIMA_PLATFORM_DATABASE_URL",
            "postgres://platform@localhost/db",
        ),
        ("PROXIIMA_STORE_NUMBER", "7"),
        ("PROXIIMA_RUNTIME_GRANTS", "true"),
        ("PROXIIMA_PG_MAX_CONNECTIONS", "3"),
        ("PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS", "1800"),
        ("PROXIIMA_MCP_BIND", "not-a-socket"),
        ("PROXIIMA_EXPOSE_NETWORK", "maybe"),
        ("PROXIIMA_OIDC_ISSUER", "http://127.0.0.1:1/issuer"),
        ("PROXIIMA_MAX_REQUEST_BODY_BYTES", "many"),
        ("PROXIIMA_S3_BUCKET", "a-bucket-without-its-block"),
        ("PROXIIMA_OUTBOX_HTTP_URL", "not a url"),
    ]);
    assert!(
        RuntimeBuilder::default().apply_lookup(&env).is_err(),
        "the full lookup reads the serving settings"
    );
    let (config, parts) = RuntimeBuilder::default()
        .served_surface(ServedSurface::Palette(Vec::new()))
        .apply_storage_lookup(&env)
        .expect("the storage block parses")
        .resolve()
        .expect("nothing serving is configured");
    assert_eq!(config.storage.store_number, 7);
    assert!(config.storage.runtime_grants);
    assert_eq!(config.storage.pg_pool_config.max_connections, 3);
    assert_eq!(
        config
            .storage
            .embedding_runtime_policy
            .stale_claim_timeout(),
        Duration::from_mins(30)
    );
    assert!(config.s3.is_none());
    assert!(config.mcp.is_none());
    assert!(!config.expose_network);
    assert!(!parts.has_authenticator());
    assert!(config.outbox_consumers.is_empty());
}

/// The storage block parses what it reads: a malformed embedding policy
/// is refused there, and the object store it leaves unread is malformed
/// for the full lookup.
#[test]
fn the_storage_lookup_refuses_a_malformed_embedding_policy() {
    assert!(matches!(
        RuntimeBuilder::default().apply_storage_lookup(lookup(&[(
            "PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS",
            "soon"
        )])),
        Err(ProxiimaError::Config(_))
    ));
    assert!(
        RuntimeBuilder::default()
            .apply_lookup(lookup(&[(
                "PROXIIMA_S3_BUCKET",
                "a-bucket-without-its-block"
            )]))
            .is_err(),
        "a bucket alone is a malformed object-store block"
    );
}

#[derive(Debug)]
struct TestAuthenticator;

#[async_trait]
impl Authenticator for TestAuthenticator {
    async fn authenticate(&self, _credentials: &Credentials) -> Result<AuthzContext, AuthError> {
        Err(AuthError::InvalidCredentials)
    }
}

fn base_config(mcp: Option<SocketAddr>) -> RuntimeConfig {
    RuntimeConfig {
        storage: StorageConfig {
            database_url: "postgres://localhost/proxiima".to_string(),
            store_number: 0,
            platform_database_url: "postgres://unused/platform".into(),
            migration_database_url: None,
            missing_migration_policy: MissingMigrationPolicy::default(),
            runtime_grants: false,
            pg_pool_config: PgPoolConfig::default(),
            pg_tuning: PgTuning::default(),
            embedding_runtime_policy: EmbeddingRuntimePolicy::default(),
        },
        publication: PublicationConfig::default(),
        published_retention: None,
        object_purge_interval: Some(crate::config::DEFAULT_OBJECT_PURGE_INTERVAL),
        outbox_consumers: OutboxConsumers::default(),
        s3: None,
        owner: Some(company_owner(uuid::Uuid::now_v7())),
        mcp: mcp.map(|bind| McpSettings { bind }),
        expose_network: false,
        allowed_origins: Vec::new(),
        allowed_hosts: Vec::new(),
        served_surface: ServedSurface::All,
        stream_revalidation: RevalidationConfig::default(),
        insecure_single_owner: false,
        rest_enabled: false,
        request_headers: RequestHeaderAllowlist::default(),
        forwarder: None,
        health_endpoints: false,
        mcp_transport: McpTransportConfig::default(),
        auth: RuntimeAuthState {
            has_host_authenticator: true,
        },
        resource_metadata: None,
    }
}

fn addr(ip: [u8; 4]) -> SocketAddr {
    addr_port(ip, 31415)
}

fn addr_port(ip: [u8; 4], port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port)
}

#[test]
fn runtime_builder_debug_redacts_database_url() {
    let builder = RuntimeBuilder::default().database_url("postgres://user:secret@host/db");
    let debug = format!("{builder:?}");

    assert!(debug.contains("has_database_url"));
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("postgres://user"));
}

#[test]
fn runtime_config_debug_redacts_database_url() {
    let config = base_config(None);
    let debug = format!("{config:?}");

    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("postgres://localhost"));
}

#[test]
fn validate_mcp_requires_authenticator() {
    let mut config = base_config(Some(addr([127, 0, 0, 1])));
    config.auth.has_host_authenticator = false;

    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("host authenticator"));
}

#[test]
fn validate_insecure_single_owner_cannot_serve_mcp() {
    let mut config = base_config(Some(addr([127, 0, 0, 1])));
    config.insecure_single_owner = true;
    config.expose_network = true;
    config.allowed_origins = vec!["https://app.test".to_string()];

    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("cannot serve MCP"));
}

#[test]
fn validate_insecure_single_owner_rejected_even_on_loopback() {
    let mut config = base_config(Some(addr([0, 0, 0, 0])));
    config.insecure_single_owner = true;
    config.auth.has_host_authenticator = false;

    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("cannot serve MCP"));
}

#[test]
fn validate_exposed_network_requires_allowed_origins() {
    let mut config = base_config(Some(addr([127, 0, 0, 1])));
    config.expose_network = true;

    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("at least one allowed origin"));
}

#[test]
fn validate_exposed_network_rejects_wildcard_allowed_origin() {
    let mut config = base_config(Some(addr([127, 0, 0, 1])));
    config.expose_network = true;
    config.allowed_origins = vec!["*".to_string()];

    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("wildcard"));
}

#[test]
fn validate_exposed_network_requires_host_authenticator() {
    let mut config = base_config(Some(addr([127, 0, 0, 1])));
    config.expose_network = true;
    config.allowed_origins = vec!["https://app.test".to_string()];
    config.auth.has_host_authenticator = false;

    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("host authenticator"));
}

#[test]
fn validate_non_exposed_mcp_requires_loopback_bind() {
    let config = base_config(Some(addr([0, 0, 0, 0])));

    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("must be loopback"));
}

#[test]
fn validate_without_mcp_is_ok_without_authenticator() {
    let mut config = base_config(None);
    config.auth.has_host_authenticator = false;
    config.expose_network = true;

    config.validate().unwrap();
}

#[test]
fn validate_mcp_rejects_wildcard_allowed_host_on_loopback() {
    // `*` has no rmcp wildcard meaning; it must be rejected loudly,
    // not silently lock out a loopback-bound reverse proxy.
    let mut config = base_config(Some(addr([127, 0, 0, 1])));
    config.allowed_hosts = vec!["*".to_string()];

    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("wildcard allowed hosts"));
}

#[test]
fn validate_exposed_network_requires_resolvable_public_host() {
    // Exposed, origins present (so the empty-origins rule passes) but
    // loopback-only and no public_url ⇒ no resolvable public host, so
    // every real request would 403. Must fail closed at startup.
    let mut config = base_config(Some(addr([127, 0, 0, 1])));
    config.expose_network = true;
    config.allowed_origins = vec!["http://localhost:8080".to_string()];

    let err = config.validate().unwrap_err();
    assert!(err.to_string().contains("resolvable public host"));
}

#[test]
fn validate_exposed_network_accepts_public_url_host() {
    let mut config = base_config(Some(addr([127, 0, 0, 1])));
    config.expose_network = true;
    config.allowed_origins = vec!["http://localhost:8080".to_string()];
    config.resource_metadata = Some(ResourceServerMetadata {
        public_url: "https://proxiima.example.com".to_string(),
        authorization_servers: AuthorizationServers::Issuers(vec!["https://idp.test".to_string()]),
        additional_scopes: Vec::new(),
    });

    config.validate().unwrap();
}

#[test]
fn public_allowed_hosts_derives_from_public_url_and_origins() {
    let mut config = base_config(Some(addr([127, 0, 0, 1])));
    config.resource_metadata = Some(ResourceServerMetadata {
        public_url: "https://proxiima.example.com".to_string(),
        authorization_servers: AuthorizationServers::Issuers(vec![]),
        additional_scopes: Vec::new(),
    });
    config.allowed_origins = vec![
        "https://app.test".to_string(),
        "http://localhost:5173".to_string(),
    ];

    // public_url host first, then non-loopback origin hosts; loopback dropped.
    assert_eq!(
        config.public_allowed_hosts(),
        vec!["proxiima.example.com".to_string(), "app.test".to_string()]
    );
}

#[test]
fn allowed_origins_are_split_trimmed_and_empty_values_dropped() {
    let builder = RuntimeBuilder::default()
        .apply_lookup(lookup(&[(
            "PROXIIMA_ALLOWED_ORIGINS",
            " https://a.test, ,https://b.test, ",
        )]))
        .unwrap();

    assert_eq!(
        builder.allowed_origins.unwrap(),
        ["https://a.test".to_string(), "https://b.test".to_string()]
    );
}

#[test]
fn stream_revalidation_env_parses_integer_seconds() {
    let (config, _) = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .served_surface(ServedSurface::All)
        .apply_lookup(lookup(&[("PROXIIMA_STREAM_MAX_LIFETIME", "7")]))
        .unwrap()
        .resolve()
        .unwrap();

    assert_eq!(
        config.stream_revalidation.max_stream_lifetime,
        Duration::from_secs(7)
    );
}

#[test]
fn stream_revalidation_zero_duration_rejected_at_resolve() {
    let err = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .stream_max_lifetime(Duration::ZERO)
        .resolve()
        .unwrap_err();

    assert!(err.to_string().contains("greater than 0"));
}

#[test]
fn stream_revalidation_defaults_when_unset() {
    let (config, _) = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .served_surface(ServedSurface::All)
        .resolve()
        .unwrap();

    assert_eq!(config.stream_revalidation, RevalidationConfig::default());
}

#[test]
fn embedding_runtime_policy_env_is_typed_and_programmatic_override_wins() {
    let env_policy = RuntimeBuilder::default()
        .apply_lookup(lookup(&[
            (
                proxiima_core::env::PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS.name(),
                "30",
            ),
            (proxiima_core::env::PROXIIMA_EMBED_BATCH_SIZE.name(), "12"),
            (
                proxiima_core::env::PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS.name(),
                "7",
            ),
            (
                proxiima_core::env::PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS.name(),
                "90",
            ),
        ]))
        .expect("valid env policy");
    assert_eq!(
        env_policy
            .embedding_runtime_policy
            .expect("env policy present")
            .batch_size(),
        12
    );

    let explicit = EmbeddingRuntimePolicy::new(
        Duration::from_secs(20),
        4,
        Duration::from_secs(2),
        Duration::from_mins(1),
    )
    .expect("valid explicit policy");
    let merged =
        env_policy.merge_over(RuntimeBuilder::default().embedding_runtime_policy(explicit));
    assert_eq!(
        merged.embedding_runtime_policy.expect("merged policy"),
        EmbeddingRuntimePolicy::new(
            Duration::from_secs(30),
            12,
            Duration::from_secs(7),
            Duration::from_secs(90),
        )
        .expect("valid expected policy")
    );

    let resolved = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .served_surface(ServedSurface::All)
        .embedding_runtime_policy(explicit)
        .resolve()
        .expect("programmatic policy resolves")
        .0;
    assert_eq!(resolved.storage.embedding_runtime_policy, explicit);
}

#[test]
fn drain_concurrency_alone_sets_the_embedding_policy_block() {
    let builder = RuntimeBuilder::default()
        .apply_lookup(lookup(&[(
            proxiima_core::env::PROXIIMA_EMBED_DRAIN_CONCURRENCY.name(),
            "4",
        )]))
        .expect("explicit drain concurrency");
    assert_eq!(
        builder
            .embedding_runtime_policy
            .expect("policy block present")
            .drain_concurrency(),
        4
    );
    assert!(
        RuntimeBuilder::default()
            .apply_lookup(lookup(&[(
                proxiima_core::env::PROXIIMA_EMBED_DRAIN_CONCURRENCY.name(),
                "0"
            )]))
            .is_err()
    );
}

#[test]
fn unset_embedding_policy_env_does_not_override_programmatic_base() {
    let explicit = EmbeddingRuntimePolicy::new(
        Duration::from_secs(20),
        4,
        Duration::from_secs(2),
        Duration::from_mins(1),
    )
    .expect("valid explicit policy");
    let env_layer = RuntimeBuilder::default()
        .apply_lookup(lookup(&[]))
        .expect("unset env");
    let merged = env_layer.merge_over(RuntimeBuilder::default().embedding_runtime_policy(explicit));
    assert_eq!(merged.embedding_runtime_policy, Some(explicit));
}

#[test]
fn malformed_mcp_bind_errors() {
    let err = RuntimeBuilder::default()
        .apply_lookup(lookup(&[("PROXIIMA_MCP_BIND", "not-a-socket")]))
        .unwrap_err();

    assert!(err.to_string().contains("PROXIIMA_MCP_BIND"));
}

#[test]
fn pg_pool_config_reads_the_injected_env_block() {
    let (config, _) = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .served_surface(ServedSurface::All)
        .apply_lookup(lookup(&[
            ("PROXIIMA_PG_MAX_CONNECTIONS", "4"),
            ("PROXIIMA_PG_ACQUIRE_TIMEOUT_SECS", "9"),
        ]))
        .unwrap()
        .resolve()
        .unwrap();

    assert_eq!(config.storage.pg_pool_config.max_connections, 4);
    assert_eq!(
        config.storage.pg_pool_config.acquire_timeout,
        Duration::from_secs(9)
    );
}

#[test]
fn invalid_pg_pool_config_fails_at_resolution() {
    let lookup_error = RuntimeBuilder::default()
        .apply_lookup(lookup(&[("PROXIIMA_PG_MAX_CONNECTIONS", "many")]))
        .expect_err("malformed injected pool config must fail");
    assert!(
        lookup_error
            .to_string()
            .contains("PROXIIMA_PG_MAX_CONNECTIONS=many")
    );

    let programmatic_error = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .served_surface(ServedSurface::All)
        .pg_pool_config(PgPoolConfig {
            max_connections: 0,
            ..PgPoolConfig::default()
        })
        .resolve()
        .expect_err("zero programmatic pool size must fail resolution");
    assert!(
        programmatic_error
            .to_string()
            .contains("at least one connection")
    );
}

#[test]
fn a_silent_lookup_does_not_override_configured_pool_policy() {
    let configured = PgPoolConfig {
        max_connections: 3,
        ..PgPoolConfig::default()
    };
    let injected = RuntimeBuilder::default()
        .apply_lookup(lookup(&[("PROXIIMA_REST_ENABLED", "true")]))
        .expect("silent pool lookup");

    let merged = injected.merge_over(RuntimeBuilder::default().pg_pool_config(configured));

    assert_eq!(merged.pg_pool_config, Some(configured));
}

#[test]
fn an_explicit_default_pool_env_overrides_configured_pool_policy() {
    let configured = PgPoolConfig {
        max_connections: 3,
        ..PgPoolConfig::default()
    };
    let injected = RuntimeBuilder::default()
        .apply_lookup(lookup(&[("PROXIIMA_PG_MAX_CONNECTIONS", "5")]))
        .expect("explicit default-valued pool env");
    let configured_base = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .served_surface(ServedSurface::All)
        .pg_pool_config(configured);

    let (config, _) = injected
        .merge_over(configured_base)
        .resolve()
        .expect("resolved pool policy");

    assert_eq!(config.storage.pg_pool_config.max_connections, 5);
}

#[test]
fn malformed_pg_tuning_errors() {
    let err = RuntimeBuilder::default()
        .apply_lookup(lookup(&[("PROXIIMA_PG_HNSW_ITERATIVE_SCAN", "relaxed")]))
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("PROXIIMA_PG_HNSW_ITERATIVE_SCAN=relaxed")
    );
}

/// Trailing newlines survive here-docs and mounted secrets. Trimming is
/// what lets the same value work here and in `PROXIIMA_S3_*`, which has
/// trimmed since it was written.
#[test]
fn surrounding_whitespace_is_trimmed_before_parsing() {
    let builder = RuntimeBuilder::default()
        .apply_lookup(lookup(&[
            ("DATABASE_URL", " postgres://localhost/proxiima\n"),
            ("PROXIIMA_EXPOSE_NETWORK", " true\n"),
            ("PROXIIMA_MCP_BIND", " 127.0.0.1:31415\n"),
        ]))
        .expect("a trailing newline must not be a parse error");

    assert_eq!(
        builder.database_url.as_deref(),
        Some("postgres://localhost/proxiima")
    );
    assert_eq!(builder.expose_network, Some(true));
    assert_eq!(
        builder.mcp_bind.map(|bind| bind.to_string()).as_deref(),
        Some("127.0.0.1:31415")
    );
}

/// Empty means unset, so it does not override a programmatically set base.
#[test]
fn an_empty_allowlist_does_not_override_a_configured_one() {
    let from_env = RuntimeBuilder::default()
        .apply_lookup(lookup(&[("PROXIIMA_ALLOWED_ORIGINS", "")]))
        .expect("empty allowlist is unset");
    let base = RuntimeBuilder::default().allowed_origins(vec!["https://app.test".to_string()]);

    let merged = from_env.merge_over(base);

    assert_eq!(
        merged.allowed_origins.as_deref(),
        Some(["https://app.test".to_string()].as_slice())
    );
}

#[test]
fn resolve_without_served_surface_fails_closed_with_actionable_message() {
    // No embedding host may silently advertise the full tool surface
    // (core_transfer/core_membership included) by omitting `.served_surface(...)`.
    let err = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .resolve()
        .unwrap_err();

    let message = err.to_string();
    assert!(message.contains("served_surface is required"));
    assert!(message.contains("ServedSurface::All"));
    assert!(message.contains("ServedSurface::Palette"));
}

#[test]
fn resolve_with_explicit_served_surface_all_builds_as_before() {
    let (config, _) = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .served_surface(ServedSurface::All)
        .resolve()
        .unwrap();

    assert_eq!(config.served_surface, ServedSurface::All);
}

/// The one-minute floor guards BOTH doors. The environment path refuses
/// a sub-minute horizon while parsing; a host that composes
/// programmatically must hit the same wall, or the knob would mean two
/// different things depending on how it was set.
#[test]
fn a_programmatic_retention_horizon_is_floored_like_the_env_one() {
    let refused = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .served_surface(ServedSurface::All)
        .published_retention(Duration::from_secs(59))
        .resolve()
        .expect_err("a sub-minute horizon must be refused");
    assert!(
        matches!(refused, ProxiimaError::Config(ref message) if message.contains("floor")),
        "{refused}"
    );

    let (config, _) = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .served_surface(ServedSurface::All)
        .published_retention(Duration::from_mins(1))
        .resolve()
        .expect("the floor itself is acceptable");
    assert_eq!(config.published_retention, Some(Duration::from_mins(1)));

    let (default_config, _) = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .served_surface(ServedSurface::All)
        .resolve()
        .expect("no horizon is the default");
    assert_eq!(
        default_config.published_retention,
        Some(crate::config::DEFAULT_PUBLISHED_RETENTION),
        "unset reclaims delivered records after the default horizon"
    );

    let (keep_forever, _) = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .served_surface(ServedSurface::All)
        .published_retention(Duration::ZERO)
        .resolve()
        .expect("zero keeps delivered records forever");
    assert_eq!(keep_forever.published_retention, None);
}

#[test]
fn runtime_config_accepts_custom_authenticator_without_separate_owner_access() {
    let (config, _) = RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .owner(owner(uuid::Uuid::now_v7()))
        .served_surface(ServedSurface::All)
        .mcp_bind(addr([127, 0, 0, 1]))
        .authenticator(Arc::new(TestAuthenticator))
        .resolve()
        .unwrap();

    assert_eq!(config.mcp.unwrap().bind, addr([127, 0, 0, 1]));
}

/// The two ways to install an authenticator are one slot: an overlay
/// setting either replaces the base's either, instead of both surviving
/// the merge and refusing boot as "not both".
#[test]
fn an_overlay_authenticator_replaces_a_base_platform_authenticator() {
    let platform = || {
        RuntimeBuilder::default()
            .authenticator_with_platform_scope(|_| Ok(Arc::new(TestAuthenticator)))
    };
    let merged = RuntimeBuilder::default()
        .authenticator(Arc::new(TestAuthenticator))
        .merge_over(platform());
    assert!(merged.authenticator.is_some() && merged.platform_authenticator.is_none());

    let merged =
        platform().merge_over(RuntimeBuilder::default().authenticator(Arc::new(TestAuthenticator)));
    assert!(merged.authenticator.is_none() && merged.platform_authenticator.is_some());

    // One layer naming both is still ambiguous.
    let refused = platform()
        .authenticator(Arc::new(TestAuthenticator))
        .database_url("postgres://unused/proxiima")
        .store_number(0)
        .platform_database_url("postgres://unused/platform")
        .served_surface(ServedSurface::All)
        .resolve()
        .expect_err("both in one layer");
    assert!(refused.to_string().contains("not both"), "{refused}");
}

fn served_builder() -> RuntimeBuilder {
    RuntimeBuilder::default()
        .database_url("postgres://localhost/proxiima")
        .store_number(0)
        .platform_database_url("postgres://localhost/platform")
        .served_surface(ServedSurface::All)
}

#[test]
fn malformed_served_path_values_are_refused() {
    for pairs in [
        [("PROXIIMA_REQUEST_HEADERS", "authorization")],
        [("PROXIIMA_MAX_REQUEST_BODY_BYTES", "8MiB")],
        [("PROXIIMA_MAX_REQUEST_BODY_BYTES", "0")],
        [("PROXIIMA_MCP_SSE_RETRY_SECS", "soon")],
        [("PROXIIMA_HEALTH_ENDPOINTS", "maybe")],
    ] {
        let resolved = served_builder()
            .apply_lookup(lookup(&pairs))
            .and_then(RuntimeBuilder::resolve);
        assert!(resolved.is_err(), "{pairs:?} must be refused");
    }
}

#[test]
fn session_idle_env_sets_the_timeout_and_zero_disables_it() {
    let transport = |value: &str| {
        served_builder()
            .apply_lookup(lookup(&[("PROXIIMA_MCP_SESSION_IDLE_SECS", value)]))
            .and_then(RuntimeBuilder::resolve)
            .map(|(config, _)| config.mcp_transport)
    };
    let set = transport("40").expect("40 s resolves");
    assert_eq!(set.session_idle_timeout, Some(Duration::from_secs(40)));
    assert_eq!(set.progress_heartbeat(), Duration::from_secs(20));
    let never = transport("0").expect("0 resolves");
    assert_eq!(never.session_idle_timeout, None);
    assert_eq!(
        never.progress_heartbeat(),
        proxiima_mcp_server::DEFAULT_PROGRESS_HEARTBEAT
    );
    assert!(
        transport("soon").is_err(),
        "non-integer seconds are refused"
    );
}

#[test]
fn host_service_bags_merge_across_layers_and_refuse_duplicates() {
    #[derive(Debug)]
    struct Budget;
    #[derive(Debug)]
    struct Ticketing;

    let base = RuntimeBuilder::default().services(SpiiceServices::with(Budget));
    let (_, parts) = served_builder()
        .services(SpiiceServices::with(Ticketing))
        .merge_over(base)
        .resolve()
        .expect("disjoint bags merge");
    assert!(parts.services.get::<Budget>().is_some());
    assert!(parts.services.get::<Ticketing>().is_some());

    let duplicate = served_builder()
        .services(SpiiceServices::with(Budget))
        .merge_over(RuntimeBuilder::default().services(SpiiceServices::with(Budget)))
        .resolve();
    assert!(matches!(duplicate, Err(ProxiimaError::SpiiceServices(_))));

    let forged = served_builder()
        .services(SpiiceServices::with(RequestHeaders::default()))
        .resolve();
    assert!(
        matches!(forged, Err(ProxiimaError::Config(message)) if message.contains("request-scoped"))
    );
}

#[cfg(feature = "auth-oidc")]
#[tokio::test]
async fn the_env_oidc_authenticator_fills_in_only_without_one_in_code() {
    let map = format!(
        r#"[{{"iss":"https://idp.test","sub":"subject","user_id":"{}"}}]"#,
        uuid::Uuid::now_v7()
    );
    let oidc = [
        ("PROXIIMA_OIDC_ISSUER", "https://idp.test"),
        ("PROXIIMA_OIDC_AUDIENCE", "proxiima"),
        ("PROXIIMA_PUBLIC_URL", "https://mcp.test"),
        ("PROXIIMA_OIDC_SUBJECT_MAP_JSON", map.as_str()),
    ];
    let (config, parts) = served_builder()
        .apply_lookup(lookup(&oidc))
        .unwrap()
        .resolve()
        .expect("env OIDC resolves");
    assert!(parts.authenticator.is_some());
    assert!(parts.late_owner_access.is_some());
    assert!(config.auth.has_host_authenticator);
    assert_eq!(
        config.resource_metadata.map(|md| md.public_url),
        Some("https://mcp.test".to_owned())
    );

    let (_, parts) = served_builder()
        .authenticator(Arc::new(TestAuthenticator))
        .apply_lookup(lookup(&oidc))
        .unwrap()
        .resolve()
        .unwrap();
    assert!(
        parts.late_owner_access.is_none(),
        "a code authenticator wins; the env one is never built"
    );
}
