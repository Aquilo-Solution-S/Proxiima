use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::routing::any;
use proxiima_core::{
    AuthError, AuthPath, Authenticator, AuthzContext, Credentials, GroupId, OwnerId,
    RevalidationConfig, Role, UserId,
};
use tower::ServiceExt;

use super::{MAX_REQUEST_BODY_BYTES, enforce_body_limit, guard_listener};
use crate::auth::EdgeAuth;
use crate::mcp::session::McpSessionBindings;
use crate::security::{
    HostAllowlist, auth_layer_with_metadata, auth_layer_with_sessions, default_allowlist,
};
use rmcp::transport::streamable_http_server::session::{
    SessionManager, local::LocalSessionManager,
};

/// The production stack over an OK `/mcp` stub. Auth is headless (rejects
/// every bearer), so any request that reaches auth returns 401 — letting
/// us prove the guards run first.
fn guarded_app() -> Router {
    guard_listener(
        Router::new()
            .route("/mcp", any(|| async { StatusCode::OK }))
            .layer(auth_layer_with_sessions(
                Arc::new(EdgeAuth::headless()),
                McpSessionBindings::default(),
                RevalidationConfig::default(),
                None,
            )),
        default_allowlist(),
        HostAllowlist::default(),
        MAX_REQUEST_BODY_BYTES,
    )
}

struct TransportPrincipalAuth {
    owner: OwnerId,
    first: UserId,
    second: UserId,
    outsider: UserId,
}

#[async_trait]
impl Authenticator for TransportPrincipalAuth {
    async fn authenticate(&self, credentials: &Credentials) -> Result<AuthzContext, AuthError> {
        let (subject, has_access) = match credentials {
            Credentials::Bearer(token) if token == "first-token" => (self.first, true),
            Credentials::Bearer(token) if token == "second-token" => (self.second, true),
            Credentials::Bearer(token) if token == "outsider-token" => (self.outsider, false),
            Credentials::Bearer(_) => return Err(AuthError::InvalidCredentials),
        };
        if !has_access {
            return Ok(AuthzContext::for_subject(subject, AuthPath::HostBearer));
        }
        Ok(AuthzContext::for_subject_with_role(
            subject,
            [(self.owner, Role::viewer())],
            AuthPath::HostBearer,
        ))
    }
}

/// The production `/mcp` service behind its derived auth layer, with the
/// bindings `bindings` builds, and the session store they own.
fn transport_app(
    owner: OwnerId,
    bindings: impl FnOnce(Arc<LocalSessionManager>) -> McpSessionBindings,
) -> (Router, Arc<LocalSessionManager>, OwnerId, OwnerId) {
    let first = UserId::new(uuid::Uuid::now_v7());
    let second = UserId::new(uuid::Uuid::now_v7());
    let outsider = UserId::new(uuid::Uuid::now_v7());
    let auth = EdgeAuth::headless().with_host(Arc::new(TransportPrincipalAuth {
        owner,
        first,
        second,
        outsider,
    }));
    let mut store = None;
    let service = super::service_over_sessions(
        stub_host(),
        &default_allowlist(),
        &HostAllowlist::default(),
        &tokio_util::sync::CancellationToken::new(),
        &super::McpTransportConfig::default(),
        |manager| {
            store = Some(Arc::clone(&manager));
            bindings(manager)
        },
    );
    let router =
        Router::new()
            .nest_service("/mcp", service.clone())
            .layer(auth_layer_with_metadata(
                &service,
                Arc::new(auth),
                RevalidationConfig::default(),
                None,
            ));
    (router, store.unwrap(), first.into(), second.into())
}

fn shared_transport_app(owner: OwnerId) -> (Router, OwnerId, OwnerId) {
    let (router, _, first, second) = transport_app(owner, McpSessionBindings::new);
    (router, first, second)
}

// An over-cap declared Content-Length is 413'd before auth.
#[tokio::test]
async fn oversized_content_length_is_rejected_before_auth() {
    let app = guarded_app();
    // No Authorization header: if auth ran first this would be 401.
    let request = Request::builder()
        .uri("/mcp")
        .header("Content-Length", (MAX_REQUEST_BODY_BYTES + 1).to_string())
        .body(Body::from("x"))
        .unwrap();
    let status = app.oneshot(request).await.unwrap().status();
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

// A configured cap replaces the default one.
#[tokio::test]
async fn configured_body_limit_is_enforced() {
    let app = Router::new()
        .route("/mcp", any(|| async { StatusCode::OK }))
        .layer(super::body_limit_layer(16));
    let over = Request::builder()
        .uri("/mcp")
        .header("Content-Length", "17")
        .body(Body::from("x"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(over).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let within = Request::builder()
        .uri("/mcp")
        .body(Body::from(vec![b'x'; 16]))
        .unwrap();
    assert_eq!(app.oneshot(within).await.unwrap().status(), StatusCode::OK);
}

// A foreign Host is refused before auth, whatever the bearer.
#[tokio::test]
async fn foreign_host_is_rejected_before_auth() {
    let request = Request::builder()
        .uri("/mcp")
        .header("Host", "evil.example")
        .body(Body::from("{}"))
        .unwrap();
    let status = guarded_app().oneshot(request).await.unwrap().status();
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// A small body flows through the guard and is then handled by auth.
#[tokio::test]
async fn in_limit_request_without_bearer_reaches_auth() {
    let app = guarded_app();
    let request = Request::builder()
        .uri("/mcp")
        .header("Host", "localhost")
        .header("Origin", "http://localhost")
        .body(Body::from("{}"))
        .unwrap();
    let status = app.oneshot(request).await.unwrap().status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// The production `/mcp` service (rmcp defaults: server-side sessions
/// for `initialize` clients, SSE answers) over `host`, with no listener
/// layers: the tests below drive rmcp's lifecycles against
/// [`crate::mcp::handler::DynamicHandler`], nothing in front of it.
fn mcp_service_for(host: crate::server::ToolHost) -> super::McpStreamableService {
    super::streamable_http_service(
        host,
        &default_allowlist(),
        &crate::security::HostAllowlist::default(),
        &tokio_util::sync::CancellationToken::new(),
    )
}

/// A host serving the core tools plus [`StubTool`].
fn stub_host() -> crate::server::ToolHost {
    let mut registry = proxiima_core::SpiiceRegistry::new();
    registry.add_tool_or_panic_for_tests::<StubTool>("proxiima-stub");
    crate::server::ToolHost::from_parts(
        Arc::new(registry.freeze_or_panic_for_tests()),
        proxiima_core::SpiiceServices::default(),
    )
}

fn mcp_service() -> super::McpStreamableService {
    mcp_service_for(stub_host())
}

fn transport_session_request(
    bearer: &str,
    owner: OwnerId,
    session: Option<&http::HeaderValue>,
    body: &serde_json::Value,
) -> Request<Body> {
    let mut request = rpc_request("2025-11-25", body);
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {bearer}").parse().unwrap());
    request
        .headers_mut()
        .insert("X-Proxiima-Owner", owner.to_string().parse().unwrap());
    if let Some(session) = session {
        request
            .headers_mut()
            .insert("mcp-session-id", session.clone());
    }
    request
}

fn authenticated_initialize(bearer: &str, owner: OwnerId, client_name: &str) -> Request<Body> {
    transport_session_request(
        bearer,
        owner,
        None,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": client_name, "version": "0" },
            },
        }),
    )
}

async fn open_authenticated_session(
    app: &Router,
    bearer: &str,
    owner: OwnerId,
    client_name: &str,
) -> http::HeaderValue {
    let initialize = authenticated_initialize(bearer, owner, client_name);
    let response = app.clone().oneshot(initialize).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let session = response.headers()["mcp-session-id"].clone();
    let _ = Messages::from_response(response).next().await;

    let initialized = transport_session_request(
        bearer,
        owner,
        Some(&session),
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }),
    );
    assert_eq!(
        app.clone().oneshot(initialized).await.unwrap().status(),
        StatusCode::ACCEPTED
    );
    session
}

#[tokio::test]
async fn streamable_transport_rejects_cross_principal_session_reuse() {
    let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let (app, first, second) = shared_transport_app(owner);
    let session = open_authenticated_session(&app, "first-token", owner, "first").await;
    let cross_principal = transport_session_request(
        "second-token",
        owner,
        Some(&session),
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {},
        }),
    );
    let cross_response = app.clone().oneshot(cross_principal).await.unwrap();
    let cross_headers = cross_response.headers().clone();
    let cross_body = axum::body::to_bytes(cross_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(cross_headers.get("mcp-session-id"), None);

    let unknown_session = transport_session_request(
        "second-token",
        owner,
        Some(&http::HeaderValue::from_static("unknown-session")),
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/list",
            "params": {},
        }),
    );
    let unknown_response = app.clone().oneshot(unknown_session).await.unwrap();
    let unknown_headers = unknown_response.headers().clone();
    let unknown_body = axum::body::to_bytes(unknown_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(cross_body, unknown_body);
    assert_eq!(
        cross_headers.get("content-type"),
        unknown_headers.get("content-type")
    );

    let cross_delete = Request::builder()
        .method("DELETE")
        .uri("http://localhost/mcp")
        .header("Host", "localhost")
        .header("Authorization", "Bearer second-token")
        .header("X-Proxiima-Owner", owner.to_string())
        .header("mcp-session-id", session.clone())
        .body(Body::empty())
        .unwrap();
    let delete_response = app.clone().oneshot(cross_delete).await.unwrap();
    assert_eq!(delete_response.status(), StatusCode::NOT_FOUND);

    let first_still_works = transport_session_request(
        "first-token",
        owner,
        Some(&session),
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/list",
            "params": {},
        }),
    );
    let message = first_router_message(app.clone(), first_still_works).await;
    assert!(message.get("result").is_some(), "{message}");

    let second_session = open_authenticated_session(&app, "second-token", owner, "second").await;
    let second_still_works = transport_session_request(
        "second-token",
        owner,
        Some(&second_session),
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/list",
            "params": {},
        }),
    );
    assert!(
        first_router_message(app.clone(), second_still_works)
            .await
            .get("result")
            .is_some()
    );

    let no_owner_access = transport_session_request(
        "outsider-token",
        owner,
        None,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/list",
            "params": {},
        }),
    );
    assert_eq!(
        app.oneshot(no_owner_access).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    // Keep the principals in the test's type-visible setup so a future
    // fixture change cannot silently collapse the two identities.
    assert_ne!(first, second);
}

/// The session store holds no session the binding table does not.
async fn store_len(store: &LocalSessionManager) -> usize {
    store.sessions.read().await.len()
}

async fn session_answers_404(app: &Router, owner: OwnerId, session: &http::HeaderValue) -> bool {
    let request = transport_session_request(
        "first-token",
        owner,
        Some(session),
        &serde_json::json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list", "params": {} }),
    );
    app.clone().oneshot(request).await.unwrap().status() == StatusCode::NOT_FOUND
}

/// A refused or evicted binding closes its session: past the per-principal
/// cap the oldest session is gone from rmcp's store, not only the table.
#[tokio::test]
async fn an_evicted_binding_closes_its_rmcp_session() {
    let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let (app, store, _, _) = transport_app(owner, McpSessionBindings::new);
    let oldest = open_authenticated_session(&app, "first-token", owner, "oldest").await;
    let oldest_id: Arc<str> = oldest.to_str().unwrap().into();
    for index in 1..=crate::mcp::session::MAX_SESSIONS_PER_PRINCIPAL {
        open_authenticated_session(&app, "first-token", owner, &format!("client-{index}")).await;
    }

    assert_eq!(
        store_len(&store).await,
        crate::mcp::session::MAX_SESSIONS_PER_PRINCIPAL
    );
    assert!(!store.has_session(&oldest_id).await.unwrap());
    assert!(session_answers_404(&app, owner, &oldest).await);
}

/// A refused binding closes its session: with the table full of another
/// principal's sessions, `initialize` answers 503 and leaves no session.
#[tokio::test]
async fn a_refused_binding_closes_its_rmcp_session() {
    let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let (app, store, _, _) = transport_app(owner, |manager| {
        McpSessionBindings::with_limits(manager, crate::mcp::session::SESSION_IDLE_TTL, 2)
    });
    open_authenticated_session(&app, "first-token", owner, "one").await;
    open_authenticated_session(&app, "first-token", owner, "two").await;

    let refused = app
        .clone()
        .oneshot(authenticated_initialize("second-token", owner, "newcomer"))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.headers().get("mcp-session-id"), None);
    assert_eq!(store_len(&store).await, 2);
}

/// Evicting a binding ends its rmcp session: the session's open
/// server-to-client stream ends.
#[tokio::test]
async fn evicting_a_binding_ends_its_open_stream() {
    let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let (app, _, _, _) = transport_app(owner, |manager| {
        McpSessionBindings::with_limits(manager, crate::mcp::session::SESSION_IDLE_TTL, 2)
    });
    let session = open_authenticated_session(&app, "first-token", owner, "streaming").await;
    let mut stream_request = Request::builder()
        .method("GET")
        .uri("http://localhost/mcp")
        .header("Host", "localhost")
        .header("Accept", "text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25")
        .header("Authorization", "Bearer first-token")
        .header("X-Proxiima-Owner", owner.to_string())
        .body(Body::empty())
        .unwrap();
    stream_request
        .headers_mut()
        .insert("mcp-session-id", session);
    let mut stream = Messages::from_response(app.clone().oneshot(stream_request).await.unwrap());

    open_authenticated_session(&app, "first-token", owner, "two").await;
    assert!(
        !stream.ends_within(QUIET).await,
        "an open stream stays open"
    );
    open_authenticated_session(&app, "first-token", owner, "three").await;
    assert!(stream.ends_within(std::time::Duration::from_secs(5)).await);
}

async fn first_router_message(app: Router, request: Request<Body>) -> serde_json::Value {
    let response = app.oneshot(request).await.unwrap();
    Messages::from_response(response).next().await
}

/// What the auth layer binds for a verified personal owner.
fn auth_for(owner: proxiima_core::OwnerId) -> crate::AuthContext {
    crate::AuthContext {
        owner,
        authz: proxiima_core::AuthzContext::single_owner(
            &owner,
            proxiima_core::AuthPath::HostBearer,
        )
        .with_tool_scope(proxiima_core::ToolScope::All),
    }
}

fn personal_owner() -> proxiima_core::OwnerId {
    proxiima_core::OwnerId::from(proxiima_core::UserId::new(uuid::Uuid::now_v7()))
}

fn rpc_request(version: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("http://localhost/mcp")
        .header("Host", "localhost")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", version)
        .header("Mcp-Method", body["method"].as_str().unwrap())
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A request in the per-request lifecycle (no `initialize`, no
/// session), naming `version` and the client in its `_meta` the way a
/// `2026-07-28` client does.
fn per_request_rpc(version: &str, method: &str, params: serde_json::Value) -> Request<Body> {
    let mut params = params;
    params["_meta"] = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "claude-code", "version": "2.1.0" },
    });
    rpc_request(
        version,
        &serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }),
    )
}

/// A flat read-only tool that answers with the client name its author
/// context recorded.
#[derive(Debug)]
struct StubTool;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct StubArgs {}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct StubOutput {
    client_name: String,
}

impl proxiima_core::Tool for StubTool {
    const TITLE: &'static str = "Test tool";
    const NAME: &'static str = "proxiima-stub_ping";
    const DESCRIPTION: &'static str = "Answers with the calling client's name.";
    const EFFECT: Option<proxiima_core::ToolEffect> = Some(proxiima_core::ToolEffect::ReadOnly);
    type Args = StubArgs;
    type Output = StubOutput;
    fn call(
        ctx: proxiima_core::ToolCtx,
        _: Self::Args,
    ) -> futures_util::future::BoxFuture<'static, Result<Self::Output, proxiima_core::ToolError>>
    {
        Box::pin(async move {
            Ok(StubOutput {
                client_name: ctx.caller().client_name.clone(),
            })
        })
    }
}

/// A host source scripted per test: its catalog, what its tools answer,
/// its instructions, and how often its catalog was read.
#[derive(Debug)]
struct HostFixture {
    catalog: Result<Vec<crate::HostTool>, &'static str>,
    /// `None` makes every call fail with `InvalidInput`.
    reply: Option<proxiima_core::ToolReply>,
    instructions: Option<String>,
    lists: Arc<std::sync::atomic::AtomicUsize>,
}

impl HostFixture {
    fn new(catalog: Vec<crate::HostTool>, reply: proxiima_core::ToolReply) -> Self {
        Self {
            catalog: Ok(catalog),
            reply: Some(reply),
            instructions: None,
            lists: Arc::default(),
        }
    }

    fn structured(output: serde_json::Value) -> Self {
        Self::new(
            vec![host_tool("host_output")],
            proxiima_core::ToolReply::Structured(output),
        )
    }

    fn lists(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::clone(&self.lists)
    }
}

/// A read-only host tool declaring an object output.
fn host_tool(name: &str) -> crate::HostTool {
    crate::HostTool::new(
        name,
        "Host fixture",
        serde_json::json!({"type": "object"}),
        proxiima_core::ToolEffect::ReadOnly,
    )
    .with_output_schema(serde_json::json!({"type": "object"}))
}

/// A host tool declared as a write: both policies gate its exact key.
fn write_host_tool(name: &str) -> crate::HostTool {
    crate::HostTool::new(
        name,
        "Host fixture",
        serde_json::json!({"type": "object"}),
        proxiima_core::ToolEffect::Additive(proxiima_core::catalog::Replay::NonIdempotent),
    )
    .with_output_schema(serde_json::json!({"type": "object"}))
}

/// The same tool with no output schema, so it may answer content.
fn content_tool(name: &str) -> crate::HostTool {
    crate::HostTool::new(
        name,
        "Host fixture",
        serde_json::json!({"type": "object"}),
        proxiima_core::ToolEffect::ReadOnly,
    )
}

#[async_trait::async_trait]
impl crate::HostTools for HostFixture {
    async fn list(
        &self,
        _: &crate::AuthContext,
    ) -> Result<Vec<crate::HostTool>, proxiima_core::ToolError> {
        self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.catalog
            .clone()
            .map_err(|message| proxiima_core::ToolError::Unavailable(message.into()))
    }

    async fn call(
        &self,
        _: proxiima_core::catalog::ToolCall,
    ) -> Result<proxiima_core::ToolReply, proxiima_core::ToolError> {
        self.reply
            .clone()
            .ok_or_else(|| proxiima_core::ToolError::InvalidInput("bad host input".into()))
    }

    async fn instructions(&self, _: &crate::AuthContext) -> Option<String> {
        self.instructions.clone()
    }
}

/// A `2026-07-28` `tools/call` for `name`, as `owner`.
fn tools_call(name: &str, owner: proxiima_core::OwnerId) -> Request<Body> {
    let mut request = per_request_rpc(
        "2026-07-28",
        "tools/call",
        serde_json::json!({"name": name, "arguments": {}}),
    );
    request
        .headers_mut()
        .insert("Mcp-Name", name.parse().unwrap());
    request.extensions_mut().insert(auth_for(owner));
    request
}

/// A `2026-07-28` request with no parameters, as an authenticated caller.
fn authed_rpc(method: &str, auth: crate::AuthContext) -> Request<Body> {
    let mut request = per_request_rpc("2026-07-28", method, serde_json::json!({}));
    request.extensions_mut().insert(auth);
    request
}

fn listed_names(message: &serde_json::Value) -> Vec<String> {
    message["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tools in {message}"))
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn tools_call_refuses_host_nonobject_outputs_and_preserves_object_outputs() {
    for output in [
        serde_json::json!(null),
        serde_json::json!("private-output"),
        serde_json::json!(42),
        serde_json::json!(true),
        serde_json::json!(["private-output"]),
        serde_json::json!({"answer": [42], "nested": {"value": null}}),
    ] {
        let host = stub_host().with_host_tools(Arc::new(HostFixture::structured(output.clone())));
        let mut request = per_request_rpc(
            "2026-07-28",
            "tools/call",
            serde_json::json!({"name": "host_output", "arguments": {}}),
        );
        request
            .headers_mut()
            .insert("Mcp-Name", "host_output".parse().unwrap());
        request.extensions_mut().insert(auth_for(personal_owner()));
        let message = first_message(mcp_service_for(host), request).await;
        if output.is_object() {
            assert!(message.get("error").is_none(), "{message}");
            assert_eq!(message["result"]["structuredContent"], output);
            let text = message["result"]["content"][0]["text"].as_str().unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(text).unwrap(),
                output
            );
        } else {
            assert!(message.get("result").is_none(), "{message}");
            assert_eq!(message["error"]["code"], -32603);
            assert_eq!(message["error"]["message"], "internal server error");
            assert!(message["error"].get("data").is_none());
            assert!(!message.to_string().contains("private-output"));
        }
    }
}

/// A host's content reply goes out as content blocks of the same names,
/// with no `structuredContent`; a failure is the same blocks with
/// `isError`, still a result and not a JSON-RPC error, and a structured
/// failure adds its object as `structuredContent`.
#[tokio::test]
async fn host_content_and_failure_replies_go_out_as_content_blocks() {
    use proxiima_core::{ToolContent, ToolReply};

    let blocks = vec![
        ToolContent::text("hello"),
        ToolContent::image([1_u8, 2, 3], "image/png"),
        ToolContent::audio([4_u8, 5], "audio/wav"),
        ToolContent::ResourceLink {
            uri: "file:///a.txt".into(),
            name: "a".into(),
            description: Some("the a file".into()),
            mime_type: Some("text/plain".into()),
        },
        ToolContent::TextResource {
            uri: "file:///b.txt".into(),
            mime_type: Some("text/plain".into()),
            text: "bee".into(),
        },
        ToolContent::BlobResource {
            uri: "file:///c.bin".into(),
            mime_type: None,
            blob: "AQID".into(),
        },
    ];
    let owner = personal_owner();

    let host = stub_host().with_host_tools(Arc::new(HostFixture::new(
        vec![content_tool("host_blocks")],
        ToolReply::Content(blocks.clone()),
    )));
    let message = first_message(mcp_service_for(host), tools_call("host_blocks", owner)).await;
    let result = &message["result"];
    assert_eq!(result["isError"], false, "{message}");
    assert!(result.get("structuredContent").is_none(), "{message}");
    let content = result["content"].as_array().expect("content blocks");
    let kinds: Vec<&str> = content
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "text",
            "image",
            "audio",
            "resource_link",
            "resource",
            "resource"
        ],
        "{message}"
    );
    assert_eq!(content[0]["text"], "hello");
    assert_eq!(content[1]["data"], "AQID");
    assert_eq!(content[1]["mimeType"], "image/png");
    assert_eq!(content[2]["data"], "BAU=");
    assert_eq!(content[3]["uri"], "file:///a.txt");
    assert_eq!(content[3]["description"], "the a file");
    assert_eq!(content[4]["resource"]["text"], "bee");
    assert_eq!(content[4]["resource"]["mimeType"], "text/plain");
    assert_eq!(content[5]["resource"]["blob"], "AQID");
    assert!(
        content[5]["resource"].get("mimeType").is_none(),
        "{message}"
    );

    // The same blocks as a failure, from a tool that declares a schema.
    let host = stub_host().with_host_tools(Arc::new(HostFixture::new(
        vec![host_tool("host_failing")],
        ToolReply::Failure(vec![ToolContent::text("quota exhausted; retry in 60s")]),
    )));
    let message = first_message(mcp_service_for(host), tools_call("host_failing", owner)).await;
    assert!(message.get("error").is_none(), "{message}");
    assert_eq!(message["result"]["isError"], true, "{message}");
    assert!(message["result"].get("structuredContent").is_none());
    assert_eq!(
        message["result"]["content"],
        serde_json::json!([{"type": "text", "text": "quota exhausted; retry in 60s"}])
    );

    // A structured failure adds `structuredContent`, from a tool that
    // declares no schema; the blocks are the host's, unchanged.
    let host = stub_host().with_host_tools(Arc::new(HostFixture::new(
        vec![content_tool("host_failing_structured")],
        ToolReply::StructuredFailure {
            structured: serde_json::Map::from_iter([
                ("code".to_owned(), serde_json::json!("quota_exhausted")),
                ("retry_after_s".to_owned(), serde_json::json!(60)),
            ]),
            content: vec![ToolContent::text("quota exhausted; retry in 60s")],
        },
    )));
    let message = first_message(
        mcp_service_for(host),
        tools_call("host_failing_structured", owner),
    )
    .await;
    assert!(message.get("error").is_none(), "{message}");
    assert_eq!(message["result"]["isError"], true, "{message}");
    assert_eq!(
        message["result"]["structuredContent"],
        serde_json::json!({"code": "quota_exhausted", "retry_after_s": 60}),
        "{message}"
    );
    assert_eq!(
        message["result"]["content"],
        serde_json::json!([{"type": "text", "text": "quota exhausted; retry in 60s"}])
    );
}

/// A call that cannot be answered stays a JSON-RPC error: a host's
/// `Err`, a refusal by the palette, an unknown tool, a catalog that
/// cannot be read. None becomes a `Failure` result.
#[tokio::test]
async fn refusals_and_host_errors_stay_json_rpc_errors() {
    let owner = personal_owner();
    let mut host_fixture = HostFixture::new(
        vec![write_host_tool("host_erring")],
        proxiima_core::ToolReply::Structured(serde_json::json!({})),
    );
    host_fixture.reply = None;
    let host = stub_host().with_host_tools(Arc::new(host_fixture));

    let message = first_message(
        mcp_service_for(host.clone()),
        tools_call("host_erring", owner),
    )
    .await;
    assert!(message.get("result").is_none(), "{message}");
    assert_eq!(message["error"]["code"], -32602, "{message}");
    assert_eq!(message["error"]["message"], "invalid input: bad host input");

    let message = first_message(
        mcp_service_for(host.clone()),
        tools_call("no_such_tool", owner),
    )
    .await;
    assert_eq!(message["error"]["code"], -32602, "{message}");

    let mut outside = auth_for(owner);
    outside.authz = outside
        .authz
        .with_tool_scope(proxiima_core::ToolScope::Palette(Vec::new()));
    let mut request = tools_call("host_erring", owner);
    request.extensions_mut().insert(outside);
    let message = first_message(mcp_service_for(host), request).await;
    assert!(message.get("result").is_none(), "{message}");
    assert_eq!(message["error"]["code"], -32600, "{message}");

    let mut down = HostFixture::structured(serde_json::json!({}));
    down.catalog = Err("catalog down");
    let host = stub_host().with_host_tools(Arc::new(down));
    for request in [
        tools_call("host_output", owner),
        authed_rpc("tools/list", auth_for(owner)),
    ] {
        let message = first_message(mcp_service_for(host.clone()), request).await;
        assert!(message.get("result").is_none(), "{message}");
        assert_eq!(message["error"]["code"], -32600, "{message}");
        assert_eq!(message["error"]["message"], "catalog down");
    }
}

/// A typed tool that answers how its call went: done, failed in its own
/// words, or refused its input.
struct SendTool;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SendOutcome {
    Done,
    Failed,
    Invalid,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SendArgs {
    /// How the call goes.
    outcome: SendOutcome,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct SendOutput {
    sent: bool,
}

const SEND_TOOL: &str = "proxiima-stub_send";

/// Not a string Proxiima would ever write: redaction or a reworded
/// message would not keep it.
const SEND_FAILURE: &str = "send refused: token sk-live-1234 rejected (retry after 60s)";

impl proxiima_core::Tool for SendTool {
    const NAME: &'static str = SEND_TOOL;
    const TITLE: &'static str = "Send";
    const DESCRIPTION: &'static str = "Answers the outcome it is asked for.";
    const EFFECT: Option<proxiima_core::ToolEffect> = Some(proxiima_core::ToolEffect::ReadOnly);
    type Args = SendArgs;
    type Output = SendOutput;
    fn call(
        _: proxiima_core::ToolCtx,
        args: SendArgs,
    ) -> futures_util::future::BoxFuture<'static, Result<SendOutput, proxiima_core::ToolError>>
    {
        Box::pin(async move {
            match args.outcome {
                SendOutcome::Done => Ok(SendOutput { sent: true }),
                SendOutcome::Failed => Err(proxiima_core::ToolError::Failure(SEND_FAILURE.into())),
                SendOutcome::Invalid => Err(proxiima_core::ToolError::InvalidInput(
                    "no recipient".into(),
                )),
            }
        })
    }
}

/// A typed tool's failure is MCP's tool-execution error: a result with
/// `isError: true` and the tool's text as its content, unchanged, never
/// a JSON-RPC error. Its success and its other errors keep their wire
/// forms, and a JSON-only surface gets the failure back with its text.
#[tokio::test]
async fn a_typed_tool_failure_goes_out_as_is_error_with_its_text() {
    let mut registry = proxiima_core::SpiiceRegistry::new();
    registry.add_tool_or_panic_for_tests::<SendTool>("proxiima-stub");
    let host = crate::server::ToolHost::from_parts(
        Arc::new(registry.freeze_or_panic_for_tests()),
        proxiima_core::SpiiceServices::default(),
    );
    let owner = personal_owner();
    let send = |outcome: &str| {
        let mut request = per_request_rpc(
            "2026-07-28",
            "tools/call",
            serde_json::json!({"name": SEND_TOOL, "arguments": {"outcome": outcome}}),
        );
        request
            .headers_mut()
            .insert("Mcp-Name", SEND_TOOL.parse().unwrap());
        request.extensions_mut().insert(auth_for(owner));
        first_message(mcp_service_for(host.clone()), request)
    };

    let message = send("failed").await;
    assert!(message.get("error").is_none(), "{message}");
    assert_eq!(message["result"]["isError"], true, "{message}");
    assert!(message["result"].get("structuredContent").is_none());
    assert_eq!(
        message["result"]["content"],
        serde_json::json!([{"type": "text", "text": SEND_FAILURE}])
    );

    let message = send("done").await;
    assert_eq!(message["result"]["isError"], false, "{message}");
    assert_eq!(
        message["result"]["structuredContent"],
        serde_json::json!({"sent": true})
    );

    let message = send("invalid").await;
    assert!(message.get("result").is_none(), "{message}");
    assert_eq!(message["error"]["code"], -32602, "{message}");
    assert_eq!(message["error"]["message"], "invalid input: no recipient");

    let json = host
        .call_tool(
            SEND_TOOL,
            serde_json::json!({"outcome": "failed"}),
            proxiima_core::ToolCaller::new("test", "test", "0"),
            Some(auth_for(owner)),
        )
        .await;
    let Err(crate::server::ToolInvocationError::Tool(error @ proxiima_core::ToolError::Failure(_))) =
        json
    else {
        panic!("a JSON-only surface answers the tool's failure: {json:?}");
    };
    assert_eq!(error.client_message(), SEND_FAILURE);
}

/// A host tool's `_meta` and `openWorldHint` reach `tools/list`; the
/// other hints are the effect's, and a tool that sets neither shows
/// neither.
#[tokio::test]
async fn host_tool_meta_and_open_world_hint_reach_tools_list() {
    let meta: serde_json::Map<String, serde_json::Value> =
        serde_json::from_value(serde_json::json!({"ui": {"resourceUri": "ui://panel"}})).unwrap();
    let write = crate::HostTool::new(
        "host_rich",
        "Host fixture",
        serde_json::json!({"type": "object"}),
        proxiima_core::ToolEffect::Destructive(proxiima_core::Replay::Idempotent),
    )
    .with_meta(meta)
    .with_open_world(true);
    let plain = host_tool("host_plain");
    let host = stub_host().with_host_tools(Arc::new(HostFixture::new(
        vec![write, plain],
        proxiima_core::ToolReply::Structured(serde_json::json!({})),
    )));
    let owner = personal_owner();
    let message = first_message(
        mcp_service_for(host),
        authed_rpc("tools/list", auth_for(owner)),
    )
    .await;
    let tools = message["result"]["tools"].as_array().unwrap();
    let find = |name: &str| tools.iter().find(|tool| tool["name"] == name).unwrap();

    let rich = find("host_rich");
    assert_eq!(
        rich["_meta"],
        serde_json::json!({"ui": {"resourceUri": "ui://panel"}})
    );
    assert_eq!(
        rich["annotations"],
        serde_json::json!({
            "readOnlyHint": false,
            "destructiveHint": true,
            "idempotentHint": true,
            "openWorldHint": true,
        })
    );
    assert!(rich.get("outputSchema").is_none(), "{rich}");

    let plain = find("host_plain");
    assert!(plain.get("_meta").is_none(), "{plain}");
    assert_eq!(
        plain["annotations"],
        serde_json::json!({"readOnlyHint": true})
    );
    assert_eq!(plain["outputSchema"]["type"], "object");
}

/// Every request reads the host catalog once, whether or not calls are
/// recorded: `tools/list` once, a host `tools/call` once, a registry
/// `tools/call` not at all.
#[tokio::test]
async fn a_request_reads_the_host_catalog_at_most_once() {
    let owner = personal_owner();
    let fixture = HostFixture::structured(serde_json::json!({"ok": true}));
    let lists = fixture.lists();
    let host = stub_host().with_host_tools(Arc::new(fixture));
    let service = mcp_service_for(host);
    let reads = || lists.load(std::sync::atomic::Ordering::SeqCst);

    first_message(service.clone(), authed_rpc("tools/list", auth_for(owner))).await;
    assert_eq!(reads(), 1, "tools/list");

    let message = first_message(service.clone(), tools_call("host_output", owner)).await;
    assert_eq!(message["result"]["isError"], false, "{message}");
    assert_eq!(reads(), 2, "host tools/call");

    let message = first_message(service.clone(), tools_call("proxiima-stub_ping", owner)).await;
    assert_eq!(message["result"]["isError"], false, "{message}");
    assert_eq!(reads(), 2, "registry tools/call");
}

/// Hides the named tools from every listing; counts what it is asked
/// and what it handles.
#[derive(Debug)]
struct Hiding {
    hidden: &'static [&'static str],
    asked: Arc<std::sync::atomic::AtomicUsize>,
    handled: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl proxiima_core::RequestBehavior for Hiding {
    async fn handle(
        &self,
        call: proxiima_core::catalog::ToolCall,
        next: proxiima_core::catalog::Next<'_>,
    ) -> Result<proxiima_core::ToolReply, proxiima_core::ToolError> {
        self.handled
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        next.run(call).await
    }

    fn visible(
        &self,
        _: &proxiima_core::ToolCtx,
        tool: &proxiima_core::ToolDescriptorView<'_>,
    ) -> bool {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        !self.hidden.contains(&tool.name())
    }
}

/// A host over [`StubTool`] whose registry carries `behavior`.
fn stub_host_behaving(
    behavior: impl proxiima_core::RequestBehavior + 'static,
) -> crate::server::ToolHost {
    let mut registry = proxiima_core::SpiiceRegistry::new();
    registry.add_tool_or_panic_for_tests::<StubTool>("proxiima-stub");
    registry.add_request_behavior(behavior);
    crate::server::ToolHost::from_parts(
        Arc::new(registry.freeze_or_panic_for_tests()),
        proxiima_core::SpiiceServices::default(),
    )
}

/// A tool a behavior hides is absent from `tools/list`, registry or
/// host; every other tool is listed; and a call to it is still judged by
/// the behavior's `handle`, never by `visible`.
#[tokio::test]
async fn behaviors_hide_tools_from_tools_list_but_not_from_calls() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let asked = Arc::new(AtomicUsize::new(0));
    let handled = Arc::new(AtomicUsize::new(0));
    let host = stub_host_behaving(Hiding {
        hidden: &["proxiima-stub_ping", "host_hidden"],
        asked: Arc::clone(&asked),
        handled: Arc::clone(&handled),
    })
    .with_host_tools(Arc::new(HostFixture::new(
        vec![host_tool("host_shown"), host_tool("host_hidden")],
        proxiima_core::ToolReply::Structured(serde_json::json!({})),
    )));
    let service = mcp_service_for(host);
    let owner = personal_owner();

    let message = first_message(service.clone(), authed_rpc("tools/list", auth_for(owner))).await;
    let names = listed_names(&message);
    assert!(names.contains(&"host_shown".to_owned()), "{names:?}");
    assert!(
        names.contains(&"core_search_memories".to_owned()),
        "{names:?}"
    );
    assert!(!names.contains(&"host_hidden".to_owned()), "{names:?}");
    assert!(
        !names.contains(&"proxiima-stub_ping".to_owned()),
        "{names:?}"
    );
    let asked_by_the_list = asked.load(Ordering::SeqCst);
    assert!(asked_by_the_list > 0);

    for hidden in ["proxiima-stub_ping", "host_hidden"] {
        let message = first_message(service.clone(), tools_call(hidden, owner)).await;
        assert_eq!(message["result"]["isError"], false, "{hidden}: {message}");
    }
    assert_eq!(handled.load(Ordering::SeqCst), 2, "handle still decides");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        asked_by_the_list,
        "a call never asks `visible`"
    );
}

/// A behavior that returns the default (`true`) can only leave the
/// palette's and the role's listing as it is.
#[tokio::test]
async fn a_default_visible_never_lists_outside_the_palette() {
    let host = stub_host_behaving(Hiding {
        hidden: &[],
        asked: Arc::default(),
        handled: Arc::default(),
    })
    .with_host_tools(Arc::new(HostFixture::new(
        vec![host_tool("host_shown"), write_host_tool("host_other")],
        proxiima_core::ToolReply::Structured(serde_json::json!({})),
    )));
    let mut auth = auth_for(personal_owner());
    auth.authz = auth
        .authz
        .with_tool_scope(proxiima_core::ToolScope::Palette(
            ["host_shown", "proxiima-stub_ping"]
                .map(|id| proxiima_core::ScopeKey::parse(id).expect("a scope key"))
                .to_vec(),
        ));
    let message = first_message(mcp_service_for(host), authed_rpc("tools/list", auth)).await;
    let names = listed_names(&message);
    assert!(names.iter().any(|name| name == "host_shown"), "{message}");
    assert!(
        names.iter().any(|name| name == "proxiima-stub_ping"),
        "{message}"
    );
    // Exact caller keys omit reads and writes alike.
    assert!(!names.iter().any(|name| name == "host_other"), "{message}");
    assert!(
        !names.iter().any(|name| name == "core_remember"),
        "{message}"
    );
    assert!(
        !names.iter().any(|name| name == "core_search_memories"),
        "{message}"
    );
}

#[tokio::test]
async fn dynamic_host_reads_use_the_same_exact_caller_keys_for_list_and_call() {
    let host = stub_host().with_host_tools(Arc::new(HostFixture::new(
        vec![host_tool("host_read"), write_host_tool("host_write")],
        proxiima_core::ToolReply::Structured(serde_json::json!({})),
    )));
    let service = mcp_service_for(host);
    let owner = personal_owner();
    for (scope, served, admitted) in [
        (
            proxiima_core::ToolScope::Palette(Vec::new()),
            proxiima_core::ServedSurface::All,
            false,
        ),
        (
            proxiima_core::ToolScope::Palette(vec![
                proxiima_core::ScopeKey::parse("host_read").unwrap(),
            ]),
            proxiima_core::ServedSurface::All,
            true,
        ),
        (
            proxiima_core::ToolScope::All,
            proxiima_core::ServedSurface::Palette(Vec::new()),
            true,
        ),
    ] {
        let mut auth = auth_for(owner);
        auth.authz = auth
            .authz
            .with_tool_scope(scope)
            .with_served_surface(served);
        let listed = first_message(service.clone(), authed_rpc("tools/list", auth.clone())).await;
        assert_eq!(
            listed_names(&listed).iter().any(|name| name == "host_read"),
            admitted,
            "{listed}"
        );
        assert!(
            !listed_names(&listed)
                .iter()
                .any(|name| name == "host_write"),
            "{listed}"
        );
        let mut call = tools_call("host_read", owner);
        call.extensions_mut().insert(auth);
        let called = first_message(service.clone(), call).await;
        if admitted {
            assert_eq!(called["result"]["isError"], false, "{called}");
        } else {
            assert_eq!(called["error"]["code"], -32600, "{called}");
        }
    }
}

#[tokio::test]
async fn exact_resource_keys_gate_mcp_static_templates_and_synthesized_guide() {
    let service = mcp_service();
    let owner = personal_owner();
    for admitted in [false, true] {
        let keys = if admitted {
            vec!["resource:tools", "resource:memory", "resource:how-to"]
        } else {
            vec!["proxiima-stub_ping"]
        };
        let mut auth = auth_for(owner);
        auth.authz = auth
            .authz
            .with_tool_scope(proxiima_core::ToolScope::Palette(
                keys.into_iter()
                    .map(|key| proxiima_core::ScopeKey::parse(key).unwrap())
                    .collect(),
            ));
        let listed =
            first_message(service.clone(), authed_rpc("resources/list", auth.clone())).await;
        let resources = listed["result"]["resources"].as_array().unwrap();
        assert_eq!(resources.len(), if admitted { 2 } else { 0 }, "{listed}");
        let templates = first_message(
            service.clone(),
            authed_rpc("resources/templates/list", auth.clone()),
        )
        .await;
        assert_eq!(
            templates["result"]["resourceTemplates"]
                .as_array()
                .unwrap()
                .len(),
            usize::from(admitted),
            "{templates}"
        );
        for uri in ["proxiima://tools", crate::selfdoc::HOW_TO_URI] {
            let mut request = per_request_rpc(
                "2026-07-28",
                "resources/read",
                serde_json::json!({"uri": uri}),
            );
            request
                .headers_mut()
                .insert("Mcp-Name", uri.parse().unwrap());
            request.extensions_mut().insert(auth.clone());
            let read = first_message(service.clone(), request).await;
            if admitted {
                assert!(read.get("result").is_some(), "{read}");
            } else {
                assert_eq!(read["error"]["code"], -32600, "{read}");
            }
        }
    }
}

/// The `instructions` of an `initialize` or `server/discover` answer.
fn instructions_of(message: &serde_json::Value) -> Option<String> {
    message["result"]
        .get("instructions")
        .map(|text| text.as_str().expect("instructions are text").to_owned())
}

fn initialize_request(auth: crate::AuthContext) -> Request<Body> {
    let mut request = rpc_request(
        "2025-11-25",
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" },
            },
        }),
    );
    request.extensions_mut().insert(auth);
    request
}

/// A host catalog that contributes `text` to the instructions.
fn instructing(text: &str) -> Arc<HostFixture> {
    let mut fixture = HostFixture::structured(serde_json::json!({}));
    fixture.instructions = Some(text.to_owned());
    Arc::new(fixture)
}

/// Both ways a client reaches the instructions, for one caller.
fn instruction_requests(auth: &crate::AuthContext) -> [(&'static str, Request<Body>); 2] {
    [
        ("discover", authed_rpc("server/discover", auth.clone())),
        ("initialize", initialize_request(auth.clone())),
    ]
}

/// The instructions `initialize` and `server/discover` serve: Proxiima's
/// text, then the host's after a blank line; the host's alone when
/// nothing of Proxiima's is advertised; Proxiima's alone when the host's
/// is over the cap.
#[tokio::test]
async fn host_instructions_reach_initialize_and_discover() {
    let owner = personal_owner();
    // Neither caller scope nor owner authority grants this caller a
    // Proxiima operation; the host's own text remains independent.
    let nothing_advertised = crate::AuthContext {
        owner,
        authz: proxiima_core::AuthzContext::for_subject(
            proxiima_core::UserId::new(uuid::Uuid::now_v7()),
            proxiima_core::AuthPath::HostBearer,
        )
        .with_tool_scope(proxiima_core::ToolScope::Palette(Vec::new())),
    };

    // The text Proxiima alone serves this caller.
    let bare = first_message(
        mcp_service_for(stub_host()),
        authed_rpc("server/discover", auth_for(owner)),
    )
    .await;
    let proxiima_text = instructions_of(&bare).expect("Proxiima advertises tools");

    for (what, request) in instruction_requests(&auth_for(owner)) {
        let host = stub_host().with_host_tools(instructing("Host rules."));
        let message = first_message(mcp_service_for(host), request).await;
        assert_eq!(
            instructions_of(&message),
            Some(format!("{proxiima_text}\n\nHost rules.")),
            "{what}: {message}"
        );
    }

    for (what, request) in instruction_requests(&nothing_advertised) {
        let host = stub_host().with_host_tools(instructing("Host rules."));
        let message = first_message(mcp_service_for(host), request).await;
        assert_eq!(
            instructions_of(&message).as_deref(),
            Some("Host rules."),
            "{what} alone: {message}"
        );
    }

    let over = "x".repeat(crate::MAX_HOST_INSTRUCTIONS_CHARS + 1);
    for (what, request) in instruction_requests(&auth_for(owner)) {
        let host = stub_host().with_host_tools(instructing(&over));
        let message = first_message(mcp_service_for(host), request).await;
        assert_eq!(
            instructions_of(&message),
            Some(proxiima_text.clone()),
            "{what}"
        );
    }
    let host = stub_host().with_host_tools(instructing(&over));
    let message = first_message(
        mcp_service_for(host),
        authed_rpc("server/discover", nothing_advertised),
    )
    .await;
    assert_eq!(
        instructions_of(&message),
        None,
        "over the cap, nothing else to say"
    );
}

/// Every piece of generated guidance one caller reads: the `instructions`
/// of `server/discover` and `initialize`, the `proxiima://how-to`
/// resource and (with `rest`) `GET /v1/how-to`.
async fn guidance_of(
    host: crate::server::ToolHost,
    owner: proxiima_core::OwnerId,
) -> Vec<(&'static str, String)> {
    let mut texts = Vec::new();
    for (what, request) in instruction_requests(&auth_for(owner)) {
        let message = first_message(mcp_service_for(host.clone()), request).await;
        texts.push((what, instructions_of(&message).expect("instructions")));
    }
    let mut read = per_request_rpc(
        "2026-07-28",
        "resources/read",
        serde_json::json!({"uri": crate::selfdoc::HOW_TO_URI}),
    );
    read.headers_mut()
        .insert("Mcp-Name", crate::selfdoc::HOW_TO_URI.parse().unwrap());
    read.extensions_mut().insert(auth_for(owner));
    let message = first_message(mcp_service_for(host.clone()), read).await;
    let body = message["result"]["contents"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no how-to in {message}"));
    texts.push(("proxiima://how-to", body.to_owned()));
    #[cfg(feature = "rest")]
    {
        let mut request = Request::builder()
            .method("GET")
            .uri("/v1/how-to")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(auth_for(owner));
        let response = crate::rest::router(host, None)
            .oneshot(request)
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        texts.push(("/v1/how-to", String::from_utf8(body.to_vec()).unwrap()));
    }
    texts
}

/// Generated guidance names only tools the caller sees. A behavior that
/// hides `core_remember` removes it from every text, the memory-spaces
/// sentence included, while `core_derive` and `core_memory_spaces` stay;
/// the default server still names it in each, so the absence means
/// something.
#[tokio::test]
async fn generated_guidance_never_names_a_tool_a_behavior_hides() {
    const REMEMBER: &str = "`core_remember`";
    const SPACES: &str = "Use a returned `space` key in `core_remember`, \
            `core_record_utterance`, `core_search_memories`, `core_derive`, and \
            `core_interpret`; hydrate a memory by reading `proxiima://memory/{id}`.";
    const SPACES_WITHOUT_REMEMBER: &str = "Use a returned `space` key in \
            `core_record_utterance`, `core_search_memories`, `core_derive`, and \
            `core_interpret`; hydrate a memory by reading `proxiima://memory/{id}`.";
    let owner = personal_owner();

    let plain = guidance_of(stub_host(), owner).await;
    let routes = if cfg!(feature = "rest") { 4 } else { 3 };
    assert_eq!(plain.len(), routes, "{plain:?}");
    for (what, text) in &plain {
        assert!(text.contains(REMEMBER), "{what} names it by default");
        assert!(text.contains(SPACES), "{what}: {text}");
    }

    let hiding = stub_host_behaving(Hiding {
        hidden: &["core_remember"],
        asked: Arc::default(),
        handled: Arc::default(),
    });
    let hidden = guidance_of(hiding, owner).await;
    assert_eq!(hidden.len(), plain.len());
    for (what, text) in &hidden {
        assert!(
            !text.contains(REMEMBER),
            "{what} names a hidden tool: {text}"
        );
        assert!(text.contains(SPACES_WITHOUT_REMEMBER), "{what}: {text}");
        assert!(text.contains("`core_memory_spaces`"), "{what}");
        assert!(text.contains("`core_derive`"), "{what}");
    }
}

/// Proxiima's instructions follow what behaviors show: behaviors that hide
/// every tool leave Proxiima nothing to say, and the host's text stands
/// alone.
#[tokio::test]
async fn instructions_follow_what_behaviors_show() {
    let host = stub_host_behaving(Hiding {
        hidden: &[
            "core_remember",
            "core_derive",
            "core_interpret",
            "core_search_memories",
            "core_recall",
            "core_think",
        ],
        asked: Arc::default(),
        handled: Arc::default(),
    })
    .with_host_tools(instructing("Host rules."));
    let message = first_message(
        mcp_service_for(host),
        authed_rpc("server/discover", auth_for(personal_owner())),
    )
    .await;
    assert_eq!(
        instructions_of(&message).as_deref(),
        Some("Host rules."),
        "{message}"
    );
}

/// The JSON-RPC messages of one response, read as they arrive, whether
/// rmcp answered with plain JSON or an SSE stream that stays open.
struct Messages {
    body: Body,
    text: String,
    seen: usize,
}

impl Messages {
    async fn of(service: super::McpStreamableService, request: Request<Body>) -> Self {
        let response = service.oneshot(request).await.unwrap();
        Self::from_response(response)
    }

    fn from_response<B>(response: http::Response<B>) -> Self
    where
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<axum::BoxError>,
    {
        Self {
            body: Body::new(response.into_body()),
            text: String::new(),
            seen: 0,
        }
    }

    /// The next message within `wait`, or `None` when none arrives.
    async fn next_within(&mut self, wait: std::time::Duration) -> Option<serde_json::Value> {
        use http_body_util::BodyExt;

        let read = async {
            loop {
                let parsed: Vec<serde_json::Value> = self
                    .text
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .filter_map(|data| serde_json::from_str(data.trim()).ok())
                    .collect();
                if let Some(message) = parsed.get(self.seen) {
                    self.seen += 1;
                    return Some(message.clone());
                }
                if self.seen == 0
                    && let Ok(message) = serde_json::from_str::<serde_json::Value>(&self.text)
                {
                    self.seen = 1;
                    return Some(message);
                }
                let frame = self.body.frame().await?.ok()?;
                if let Ok(data) = frame.into_data() {
                    self.text.push_str(std::str::from_utf8(&data).unwrap());
                }
            }
        };
        tokio::time::timeout(wait, read).await.ok().flatten()
    }

    /// Whether the body ends within `wait`, discarding what arrives.
    async fn ends_within(&mut self, wait: std::time::Duration) -> bool {
        use http_body_util::BodyExt;

        let drain = async { while let Some(Ok(_)) = self.body.frame().await {} };
        tokio::time::timeout(wait, drain).await.is_ok()
    }

    async fn next(&mut self) -> serde_json::Value {
        self.next_within(std::time::Duration::from_secs(5))
            .await
            .unwrap_or_else(|| panic!("no JSON-RPC message within 5s: {:?}", self.text))
    }
}

async fn first_message(
    service: super::McpStreamableService,
    request: Request<Body>,
) -> serde_json::Value {
    Messages::of(service, request).await.next().await
}

/// `2026-07-28` has no handshake, so an `initialize` naming it settles
/// on the newest revision that has one.
#[tokio::test]
async fn initialize_requesting_2026_07_28_is_answered_with_2025_11_25() {
    let request = rpc_request(
        "2026-07-28",
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" },
            },
        }),
    );
    let message = first_message(mcp_service(), request).await;
    assert_eq!(
        message["result"]["protocolVersion"], "2025-11-25",
        "{message}"
    );
}

/// Issue AQS/aquilo#9484: Claude Code on `2026-07-28` rejected
/// `tools/list` for missing SEP-2549 cache hints. Every list carries
/// them, on that revision and on the ones before it; each list is
/// projected from the caller's token, so it is private and never fresh.
#[tokio::test]
async fn list_results_carry_private_zero_ttl_cache_hints() {
    for version in ["2026-07-28", "2025-11-25"] {
        for method in ["tools/list", "resources/list", "resources/templates/list"] {
            let request = per_request_rpc(version, method, serde_json::json!({}));
            let message = first_message(mcp_service(), request).await;
            let context = format!("{version} {method}: {message}");
            assert_eq!(message["result"]["ttlMs"], 0, "{context}");
            assert_eq!(message["result"]["cacheScope"], "private", "{context}");
        }
    }
}

/// A revision past the ceiling is refused with the ones served, so the
/// client can fall back, never answered in a shape it cannot trust.
#[tokio::test]
async fn a_revision_newer_than_the_ceiling_is_refused() {
    let request = per_request_rpc("2027-01-01", "tools/list", serde_json::json!({}));
    let message = first_message(mcp_service(), request).await;
    assert_eq!(
        message["error"]["code"],
        rmcp::model::ErrorCode::UNSUPPORTED_PROTOCOL_VERSION.0,
        "{message}"
    );
    assert_eq!(
        message["error"]["data"]["supported"],
        serde_json::json!([
            "2024-11-05",
            "2025-03-26",
            "2025-06-18",
            "2025-11-25",
            "2026-07-28"
        ]),
        "{message}"
    );
}

/// `server/discover` replaces the handshake on `2026-07-28`: it lists
/// the revisions served and carries the same per-caller instructions
/// `initialize` does.
#[tokio::test]
async fn discover_serves_2026_07_28_with_instructions() {
    let request = per_request_rpc("2026-07-28", "server/discover", serde_json::json!({}));
    let message = first_message(mcp_service(), request).await;
    assert_eq!(
        message["result"]["supportedVersions"],
        serde_json::json!([
            "2024-11-05",
            "2025-03-26",
            "2025-06-18",
            "2025-11-25",
            "2026-07-28"
        ]),
        "{message}"
    );
    assert!(
        message["result"]["instructions"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "{message}"
    );
    assert_eq!(message["result"]["cacheScope"], "private", "{message}");
}

/// The call Centauri's direct forwarder sends a pack (aquilo
/// `serve_forward/direct/mcp_wire.rs`, contract `serve-mcp-forward-v1`
/// `mcpTransport`): one standalone `tools/call`, no `initialize`,
/// `MCP-Protocol-Version: 2026-07-28`, SEP-2243 headers and the two
/// required `_meta` keys. rmcp checks that `_meta` version against the
/// handler's supported revisions, so a ceiling below `2026-07-28` would
/// refuse every such call with -32022.
#[tokio::test]
async fn standalone_2026_07_28_tools_call_is_answered() {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "attempt-1",
        "method": "tools/call",
        "params": {
            "name": "proxiima-stub_ping",
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            },
        },
    });
    let mut request = rpc_request("2026-07-28", &body);
    request
        .headers_mut()
        .insert("Mcp-Name", "proxiima-stub_ping".parse().unwrap());
    request.extensions_mut().insert(auth_for(personal_owner()));
    let message = first_message(mcp_service(), request).await;
    assert_eq!(message["id"], "attempt-1", "{message}");
    assert_eq!(message["result"]["isError"], false, "{message}");
    // The forwarder names no client; nothing is invented for it.
    assert_eq!(
        message["result"]["structuredContent"],
        serde_json::json!({"client_name": "unknown"}),
        "{message}"
    );
}

/// A per-request caller has no handshake to take its name from; the
/// caller reads it from the request's own `_meta`.
#[tokio::test]
async fn a_per_request_callers_client_info_reaches_the_author_context() {
    let mut request = per_request_rpc(
        "2026-07-28",
        "tools/call",
        serde_json::json!({ "name": "proxiima-stub_ping", "arguments": {} }),
    );
    request
        .headers_mut()
        .insert("Mcp-Name", "proxiima-stub_ping".parse().unwrap());
    request.extensions_mut().insert(auth_for(personal_owner()));
    let message = first_message(mcp_service(), request).await;
    assert_eq!(
        message["result"]["structuredContent"],
        serde_json::json!({"client_name": "claude-code"}),
        "{message}"
    );
}

/// `tools.listChanged` is a promise to notify, so it is advertised only
/// by a host that attached a notifier.
#[tokio::test]
async fn list_changed_is_advertised_only_with_a_notifier() {
    let discover = || per_request_rpc("2026-07-28", "server/discover", serde_json::json!({}));
    let without = first_message(mcp_service(), discover()).await;
    assert!(
        without["result"]["capabilities"]["tools"]
            .get("listChanged")
            .is_none(),
        "{without}"
    );
    let host = stub_host().with_tool_list_notifier(crate::ToolListNotifier::new());
    let with = first_message(mcp_service_for(host), discover()).await;
    assert_eq!(
        with["result"]["capabilities"]["tools"]["listChanged"], true,
        "{with}"
    );
}

/// Opens an `initialize` session for `owner` and its server-to-client
/// stream, as a `2025-11-25` client does.
async fn open_session(
    service: &super::McpStreamableService,
    owner: proxiima_core::OwnerId,
) -> Messages {
    let mut initialize = rpc_request(
        "2025-11-25",
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" },
            },
        }),
    );
    initialize.extensions_mut().insert(auth_for(owner));
    let response = service.clone().oneshot(initialize).await.unwrap();
    let session = response.headers()["mcp-session-id"].clone();
    let answer = Messages::from_response(response).next().await;
    assert_eq!(
        answer["result"]["protocolVersion"], "2025-11-25",
        "{answer}"
    );

    let mut initialized = rpc_request(
        "2025-11-25",
        &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    );
    initialized
        .headers_mut()
        .insert("mcp-session-id", session.clone());
    let status = service.clone().oneshot(initialized).await.unwrap().status();
    assert_eq!(status, StatusCode::ACCEPTED);

    let stream = Request::builder()
        .method("GET")
        .uri("http://localhost/mcp")
        .header("Host", "localhost")
        .header("Accept", "text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25")
        .header("mcp-session-id", session)
        .body(Body::empty())
        .unwrap();
    Messages::from_response(service.clone().oneshot(stream).await.unwrap())
}

/// Opens a `2026-07-28` `subscriptions/listen` stream for `owner` and
/// waits for its acknowledgment.
async fn open_subscription(
    service: &super::McpStreamableService,
    owner: proxiima_core::OwnerId,
) -> Messages {
    let mut listen = per_request_rpc(
        "2026-07-28",
        "subscriptions/listen",
        serde_json::json!({ "notifications": { "toolsListChanged": true } }),
    );
    listen.extensions_mut().insert(auth_for(owner));
    let mut stream = Messages::of(service.clone(), listen).await;
    let ack = stream.next().await;
    assert_eq!(
        ack["method"], "notifications/subscriptions/acknowledged",
        "{ack}"
    );
    assert_eq!(
        ack["params"]["notifications"]["toolsListChanged"], true,
        "{ack}"
    );
    stream
}

/// `notify` until `owner`'s listener is registered: a subscription is
/// acknowledged a moment before `listen` registers it.
async fn notify_registered(notifier: &crate::ToolListNotifier, owner: &proxiima_core::OwnerId) {
    for _ in 0..100 {
        if notifier.notify(owner).await == 1 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no listener registered for the owner");
}

const QUIET: std::time::Duration = std::time::Duration::from_millis(300);

/// A tool-list change reaches the owner's `initialize` session and no
/// session of another owner: which tools exist is tenant information.
#[tokio::test]
async fn a_session_hears_only_its_owners_tool_list_change() {
    let notifier = crate::ToolListNotifier::new();
    let service = mcp_service_for(stub_host().with_tool_list_notifier(notifier.clone()));
    let (owner, other) = (personal_owner(), personal_owner());
    let mut session = open_session(&service, owner).await;
    let mut other_session = open_session(&service, other).await;

    assert_eq!(notifier.notify(&owner).await, 1);
    let heard = session.next().await;
    assert_eq!(
        heard["method"], "notifications/tools/list_changed",
        "{heard}"
    );
    assert_eq!(other_session.next_within(QUIET).await, None);
}

/// The `2026-07-28` path: a `subscriptions/listen` stream that asked for
/// `toolsListChanged` hears its owner's change, tagged with its
/// subscription, and another owner's stream hears nothing.
#[tokio::test]
async fn a_subscription_hears_only_its_owners_tool_list_change() {
    let notifier = crate::ToolListNotifier::new();
    let service = mcp_service_for(stub_host().with_tool_list_notifier(notifier.clone()));
    let (owner, other) = (personal_owner(), personal_owner());
    let mut stream = open_subscription(&service, owner).await;
    let mut other_stream = open_subscription(&service, other).await;

    notify_registered(&notifier, &owner).await;
    let heard = stream.next().await;
    assert_eq!(
        heard["method"], "notifications/tools/list_changed",
        "{heard}"
    );
    assert_eq!(
        heard["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"], 1,
        "{heard}"
    );
    assert_eq!(other_stream.next_within(QUIET).await, None);

    // Closing the stream ends the subscription and its registration.
    drop(stream);
    for _ in 0..100 {
        if notifier.notify(&owner).await == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("a closed subscription is still registered");
}

// A stream with no Content-Length that exceeds the cap errors when
// read, rather than buffering unbounded memory.
#[tokio::test]
async fn oversized_streamed_body_is_truncated_with_error() {
    let app = Router::new()
        .route(
            "/mcp",
            any(|request: Request<Body>| async move {
                match axum::body::to_bytes(request.into_body(), usize::MAX).await {
                    Ok(_) => StatusCode::OK,
                    Err(_) => StatusCode::PAYLOAD_TOO_LARGE,
                }
            }),
        )
        .layer(axum::middleware::from_fn(enforce_body_limit));

    let oversized = vec![0u8; MAX_REQUEST_BODY_BYTES + 1];
    let stream =
        futures_util::stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(oversized)) });
    let request = Request::builder()
        .uri("/mcp")
        .body(Body::from_stream(stream))
        .unwrap();
    let status = app.oneshot(request).await.unwrap().status();
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}
