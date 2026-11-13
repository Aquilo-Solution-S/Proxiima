//! The resolved MCP edge of a booted runtime, for a host composing its own
//! router around Proxiima's `/mcp` instead of serving [`BuiltProxiima::service`].
//!
//! [`BuiltProxiima::service`]: crate::BuiltProxiima::service

use std::sync::Arc;

use axum::Router;
use proxiima_core::RevalidationConfig;
use proxiima_server::{
    DynamicHandler, EdgeAuth, HostAllowlist, McpStreamableService, McpTransportConfig,
    OriginAllowlist, ResourceServerMetadata, ToolHost, guard_listener,
    streamable_http_service_wrapping,
};
use tokio_util::sync::CancellationToken;

/// What the runtime resolved for its MCP edge: the tool host (host tools,
/// call recording, request headers, engine), the bearer authentication, the
/// Origin and Host allowlists, stream revalidation, OAuth resource metadata
/// and the transport. [`BuiltProxiima::mcp_edge`](crate::BuiltProxiima::mcp_edge)
/// returns it when MCP is enabled, so a host never resolves the builder a
/// second time to learn them.
#[derive(Clone)]
pub struct McpEdge {
    pub(crate) tool_host: ToolHost,
    pub(crate) edge_auth: Arc<EdgeAuth>,
    pub(crate) origin_allowlist: OriginAllowlist,
    pub(crate) host_allowlist: HostAllowlist,
    pub(crate) revalidation: RevalidationConfig,
    pub(crate) resource_metadata: Option<ResourceServerMetadata>,
    pub(crate) transport: McpTransportConfig,
    pub(crate) rest_router: Router,
    pub(crate) cancel: CancellationToken,
}

impl std::fmt::Debug for McpEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpEdge")
            .field("tool_host", &self.tool_host)
            .field("edge_auth", &self.edge_auth)
            .field("origin_allowlist", &self.origin_allowlist)
            .field("host_allowlist", &self.host_allowlist)
            .field("revalidation", &self.revalidation)
            .field("resource_metadata", &self.resource_metadata.is_some())
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

impl McpEdge {
    /// The tool host `/mcp` serves: registry tools, host tools, recording.
    #[must_use]
    pub fn tool_host(&self) -> &ToolHost {
        &self.tool_host
    }

    /// The bearer authentication `/mcp` runs behind.
    #[must_use]
    pub fn edge_auth(&self) -> &Arc<EdgeAuth> {
        &self.edge_auth
    }

    /// Browser `Origin` allowlist (`PROXIIMA_ALLOWED_ORIGINS`). A portless
    /// entry accepts any port; an explicit port also matches a browser
    /// `Origin` that omits it when it is the scheme default (HTTP 80 or
    /// HTTPS 443).
    #[must_use]
    pub fn origin_allowlist(&self) -> &OriginAllowlist {
        &self.origin_allowlist
    }

    /// Inbound `Host` allowlist, loopback included.
    #[must_use]
    pub fn host_allowlist(&self) -> &HostAllowlist {
        &self.host_allowlist
    }

    /// Maximum lifetime of an authenticated stream.
    #[must_use]
    pub const fn revalidation(&self) -> RevalidationConfig {
        self.revalidation
    }

    /// OAuth protected-resource metadata, when configured.
    #[must_use]
    pub fn resource_metadata(&self) -> Option<&ResourceServerMetadata> {
        self.resource_metadata.as_ref()
    }

    /// rmcp transport settings; `max_request_body_bytes` is the body cap.
    #[must_use]
    pub const fn transport(&self) -> &McpTransportConfig {
        &self.transport
    }

    /// A fresh rmcp service for `/mcp` over [`Self::tool_host`], with this
    /// edge's allowlists and transport, cancelled with the runtime. It owns
    /// its sessions: derive the auth layer from this value with
    /// [`auth_layer_with_metadata`](proxiima_server::auth_layer_with_metadata).
    #[must_use]
    pub fn mcp_service(&self) -> McpStreamableService {
        self.mcp_service_wrapping(std::convert::identity)
    }

    /// [`Self::mcp_service`] serving each session by the handler `wrap`
    /// builds around the native [`DynamicHandler`]: a host's own
    /// [`rmcp::ServerHandler`]. The service owns its sessions the same way,
    /// so the auth layer is derived from this value too.
    #[must_use]
    pub fn mcp_service_wrapping<H>(
        &self,
        wrap: impl Fn(DynamicHandler) -> H + Send + Sync + 'static,
    ) -> McpStreamableService<H>
    where
        H: rmcp::ServerHandler + Send + 'static,
    {
        streamable_http_service_wrapping(
            self.tool_host.clone(),
            &self.origin_allowlist,
            &self.host_allowlist,
            &self.cancel,
            &self.transport,
            wrap,
        )
    }

    /// `/mcp` (and `/v1` when REST is enabled) behind this edge's bearer
    /// auth, `app_router` beside it **without** bearer auth, the whole
    /// listener behind the body cap, the Host guard and CORS; the OAuth
    /// metadata routes are public. `app_router` authenticates its own routes.
    pub fn router(&self, app_router: Router) -> Router {
        let service = self.mcp_service();
        let auth_layer = proxiima_server::auth_layer_with_metadata(
            &service,
            Arc::clone(&self.edge_auth),
            self.revalidation,
            self.resource_metadata.as_ref(),
        );
        let protected = Router::new()
            .nest_service(proxiima_server::MCP_PATH, service)
            .merge(self.rest_router.clone())
            .layer(auth_layer);
        let mut router = protected.merge(app_router);
        if let Some(metadata) = &self.resource_metadata {
            router = router.merge(proxiima_server::public_discovery_router(metadata));
        }
        guard_listener(
            router,
            self.origin_allowlist.clone(),
            self.host_allowlist.clone(),
            self.transport.max_request_body_bytes,
        )
    }
}

/// [`crate::layered_router_with_revalidation`] with bearer auth on `/mcp`
/// only: `app_router` is served beside it under the listener-wide body
/// limit, Host guard and CORS, and authenticates its own routes (or none).
/// Unlike [`McpEdge::router`] it mounts no resource-metadata routes and caps
/// bodies at the default [`MAX_REQUEST_BODY_BYTES`](proxiima_server::MAX_REQUEST_BODY_BYTES).
///
/// `host_allowlist` must also be passed to
/// [`streamable_http_service`](proxiima_server::streamable_http_service).
pub fn layered_router_mcp_only(
    mcp_service: McpStreamableService,
    app_router: Router,
    edge_auth: Arc<EdgeAuth>,
    allowlist: OriginAllowlist,
    host_allowlist: HostAllowlist,
    revalidation: RevalidationConfig,
) -> Router {
    let auth_layer = proxiima_server::auth_layer_with_config(&mcp_service, edge_auth, revalidation);
    let router = Router::new()
        .nest_service(proxiima_server::MCP_PATH, mcp_service)
        .layer(auth_layer)
        .merge(app_router);
    guard_listener(
        router,
        allowlist,
        host_allowlist,
        proxiima_server::MAX_REQUEST_BODY_BYTES,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use proxiima_core::{RevalidationConfig, SpiiceRegistry, SpiiceServices};
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn public_client_discovery_bypasses_auth_but_mcp_remains_protected() {
        let registry = SpiiceRegistry::new().freeze_or_panic_for_tests();
        let metadata = ResourceServerMetadata {
            public_url: "https://proxiima.example.com".to_string(),
            authorization_servers: proxiima_server::AuthorizationServers::PublicClient(
                proxiima_server::PublicClientFacade::try_new(
                    "public-client",
                    "https://idp.example.com/authorize",
                    "https://idp.example.com/token",
                )
                .unwrap(),
            ),
            additional_scopes: Vec::new(),
        };
        let edge = McpEdge {
            tool_host: ToolHost::from_engine(
                Arc::new(proxiima_core::Engine::new(
                    registry,
                    proxiima_core::StoragePorts::rejecting_for_tests(),
                )),
                SpiiceServices::default(),
            ),
            edge_auth: Arc::new(EdgeAuth::headless()),
            origin_allowlist: OriginAllowlist::parse(["https://proxiima.example.com"]).unwrap(),
            host_allowlist: HostAllowlist::parse(["proxiima.example.com"]).unwrap(),
            revalidation: RevalidationConfig::default(),
            resource_metadata: Some(metadata),
            transport: McpTransportConfig::default(),
            rest_router: Router::new(),
            cancel: CancellationToken::new(),
        };
        let app = edge.router(Router::new());

        for path in [
            "/.well-known/oauth-authorization-server",
            "/.well-known/oauth-authorization-server/mcp",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("host", "proxiima.example.com")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["issuer"], "https://proxiima.example.com");
        }

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/register")
                    .header("host", "proxiima.example.com")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"redirect_uris":["https://client.example/cb"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(proxiima_server::MCP_PATH)
                    .header("host", "proxiima.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
