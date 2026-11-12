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
                .storage
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
    assert_eq!(config.storage.store_number, 3);
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

    assert_eq!(config.storage.pg_pool_config.max_connections, 4);
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
