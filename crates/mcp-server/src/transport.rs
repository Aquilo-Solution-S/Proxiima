//! rmcp 3.x entrypoint:
//! `rmcp::transport::streamable_http_server::StreamableHttpService`.
//!
//! The service is a Tower service composed into
//! `axum::Router::nest_service("/mcp", service)`. Proxiima wraps it
//! with shared listener-level Host and Origin validation. rmcp retains its
//! own `/mcp` DNS-rebinding guard as defense in depth.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use http_body_util::Limited;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::local::{LocalSessionManager, SessionConfig},
};
use tokio_util::sync::CancellationToken;

use crate::handler::DynamicHandler;
use crate::security::{HostAllowlist, OriginAllowlist, cors_layer, host_guard_layer};
use crate::server::McpToolHost;
use crate::session::McpSessionBindings;

/// The rmcp service `/mcp` is served by ([`streamable_http_service`]) with
/// the session bindings that own its sessions. Mount it as a tower service;
/// derive its auth layer with [`crate::mcp_auth_layer_with_metadata`].
#[derive(Clone)]
pub struct McpStreamableService {
    inner: StreamableHttpService<DynamicHandler, LocalSessionManager>,
    sessions: McpSessionBindings,
}

impl McpStreamableService {
    pub(crate) const fn sessions(&self) -> &McpSessionBindings {
        &self.sessions
    }
}

impl std::fmt::Debug for McpStreamableService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpStreamableService")
            .finish_non_exhaustive()
    }
}

impl<B> tower::Service<http::Request<B>> for McpStreamableService
where
    StreamableHttpService<DynamicHandler, LocalSessionManager>: tower::Service<http::Request<B>>,
{
    type Response =
        <StreamableHttpService<DynamicHandler, LocalSessionManager> as tower::Service<
            http::Request<B>,
        >>::Response;
    type Error = <StreamableHttpService<DynamicHandler, LocalSessionManager> as tower::Service<
        http::Request<B>,
    >>::Error;
    type Future = <StreamableHttpService<DynamicHandler, LocalSessionManager> as tower::Service<
        http::Request<B>,
    >>::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        self.inner.call(request)
    }
}

/// `host_allowlist` is the same non-empty policy applied at the outer
/// listener. Passing it into rmcp keeps `/mcp` independently guarded as
/// defense in depth; [`HostAllowlist`] owns the loopback defaults so this
/// function has no empty-list special case.
#[must_use]
pub fn streamable_http_service(
    server: McpToolHost,
    allowlist: &OriginAllowlist,
    host_allowlist: &HostAllowlist,
    cancel: &CancellationToken,
) -> McpStreamableService {
    streamable_http_service_with_transport(
        server,
        allowlist,
        host_allowlist,
        cancel,
        &McpTransportConfig::default(),
    )
}

/// [`streamable_http_service`] with explicit rmcp transport settings.
///
/// The security-bearing fields — Host and Origin allowlists, cancellation —
/// always come from Proxiima's own policy; `transport` only tunes the stream.
#[must_use]
pub fn streamable_http_service_with_transport(
    server: McpToolHost,
    allowlist: &OriginAllowlist,
    host_allowlist: &HostAllowlist,
    cancel: &CancellationToken,
    transport: &McpTransportConfig,
) -> McpStreamableService {
    service_over_sessions(
        server,
        allowlist,
        host_allowlist,
        cancel,
        transport,
        McpSessionBindings::new,
    )
}

/// The service over the bindings `bindings` builds around its session store.
fn service_over_sessions(
    server: McpToolHost,
    allowlist: &OriginAllowlist,
    host_allowlist: &HostAllowlist,
    cancel: &CancellationToken,
    transport: &McpTransportConfig,
    bindings: impl FnOnce(Arc<LocalSessionManager>) -> McpSessionBindings,
) -> McpStreamableService {
    let config = StreamableHttpServerConfig::default()
        .with_allowed_origins(allowlist.origins())
        .with_allowed_hosts(host_allowlist.hosts().iter().cloned())
        .with_cancellation_token(cancel.child_token())
        .with_sse_keep_alive(transport.sse_keep_alive)
        .with_sse_retry(transport.sse_retry)
        .with_legacy_session_mode(transport.legacy_session_mode)
        .with_json_response(transport.json_response)
        .with_max_request_body_bytes(transport.max_request_body_bytes);
    let mut sessions = LocalSessionManager::default();
    sessions.session_config.keep_alive = transport.session_idle_timeout;
    let progress_heartbeat = transport.progress_heartbeat();
    let sessions = Arc::new(sessions);
    let inner = StreamableHttpService::new(
        move || Ok(DynamicHandler::new(server.clone()).with_progress_heartbeat(progress_heartbeat)),
        Arc::clone(&sessions),
        config,
    );
    McpStreamableService {
        inner,
        sessions: bindings(sessions),
    }
}

/// The listener-wide guards over a router whose routes already carry their
/// own authentication, in the one order they are correct in. Layers run
/// outermost first: the body cap 413s an oversized request before anything
/// parses it, then the Host guard, then CORS/Origin. Authentication is applied
/// to the router before this call, so it runs after all three. Native CLI
/// clients commonly omit `Origin` and keep the bearer path.
#[must_use = "serve the returned router; the guards apply to it"]
pub fn guard_listener(
    router: axum::Router,
    origins: OriginAllowlist,
    hosts: HostAllowlist,
    max_body_bytes: usize,
) -> axum::Router {
    router
        .layer(cors_layer(origins))
        .layer(host_guard_layer(hosts))
        .layer(body_limit_layer(max_body_bytes))
}

/// Default cap on the accepted request-body size. Sits generously above the
/// largest legitimate MCP request; anything larger is a client error or
/// abuse and is refused before auth or JSON parsing runs.
pub const MAX_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

/// rmcp Streamable HTTP tuning a host may set (`PROXIIMA_MCP_*`).
///
/// Deliberately not rmcp's own config type: its Host/Origin allowlists and
/// cancellation are security policy Proxiima derives itself, so a host
/// cannot hand in a config that widens them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McpTransportConfig {
    /// SSE ping interval; `None` disables pings.
    pub sse_keep_alive: Option<Duration>,
    /// Close a session after this long without session traffic; `None`
    /// never closes it (rmcp's `SessionConfig::keep_alive`). SSE pings are
    /// not traffic and neither is a tool call still running: the progress
    /// heartbeat is, for a call that carries a progress token.
    pub session_idle_timeout: Option<Duration>,
    /// SSE priming-event retry interval; `None` sends none.
    pub sse_retry: Option<Duration>,
    /// Keep a server-side session per client that opens with `initialize`;
    /// per-request (`_meta`-versioned) calls are always stateless.
    pub legacy_session_mode: bool,
    /// Prefer `application/json` for simple request/response calls when
    /// sessions are off.
    pub json_response: bool,
    /// Largest accepted request body, in bytes. Enforced by the listener
    /// guard ([`body_limit_layer`]) and by rmcp; the facade feeds both from
    /// one value.
    pub max_request_body_bytes: usize,
}

impl Default for McpTransportConfig {
    /// rmcp's defaults, with Proxiima's body cap.
    fn default() -> Self {
        Self {
            sse_keep_alive: Some(Duration::from_secs(15)),
            session_idle_timeout: Some(SessionConfig::DEFAULT_KEEP_ALIVE),
            sse_retry: Some(Duration::from_secs(3)),
            legacy_session_mode: true,
            json_response: false,
            max_request_body_bytes: MAX_REQUEST_BODY_BYTES,
        }
    }
}

impl McpTransportConfig {
    /// How often a running tool call that carries a progress token sends
    /// `notifications/progress`: [`DEFAULT_PROGRESS_HEARTBEAT`], or half the
    /// session idle timeout when that is shorter, so the session never sits
    /// idle for a whole timeout while the call runs.
    #[must_use]
    pub fn progress_heartbeat(&self) -> Duration {
        self.session_idle_timeout
            .map_or(DEFAULT_PROGRESS_HEARTBEAT, |idle| {
                DEFAULT_PROGRESS_HEARTBEAT.min(idle / 2)
            })
            .max(MIN_PROGRESS_HEARTBEAT)
    }
}

/// Progress heartbeat interval when the idle timeout allows it.
pub const DEFAULT_PROGRESS_HEARTBEAT: Duration = Duration::from_secs(30);

/// Floor for the heartbeat, so a tiny idle timeout cannot spin it.
const MIN_PROGRESS_HEARTBEAT: Duration = Duration::from_millis(100);

/// Outermost MCP guard: reject oversized bodies with 413 before auth or parsing.
/// A declared `Content-Length` over the cap is refused immediately; the body
/// is otherwise wrapped in [`Limited`] so a chunked or length-lying stream
/// errors past the cap instead of buffering unbounded memory.
///
/// Uses [`MAX_REQUEST_BODY_BYTES`]; [`body_limit_layer`] takes the cap as a
/// value.
pub async fn enforce_body_limit(request: Request<Body>, next: Next) -> Response {
    limit_body(MAX_REQUEST_BODY_BYTES, request, next).await
}

/// Concrete return type for [`body_limit_layer`].
pub type BodyLimitLayer = middleware::FromFnLayer<
    fn(
        State<usize>,
        Request<Body>,
        Next,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
    usize,
    (State<usize>, Request<Body>),
>;

/// [`enforce_body_limit`] with a configured cap, in bytes.
#[must_use = "apply the returned layer to the complete HTTP listener"]
pub fn body_limit_layer(max_bytes: usize) -> BodyLimitLayer {
    fn dispatch(
        State(max_bytes): State<usize>,
        request: Request<Body>,
        next: Next,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
        Box::pin(limit_body(max_bytes, request, next))
    }
    middleware::from_fn_with_state(max_bytes, dispatch as fn(_, _, _) -> _)
}

async fn limit_body(max_bytes: usize, request: Request<Body>, next: Next) -> Response {
    if let Some(len) = request
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| raw.parse::<usize>().ok())
        && len > max_bytes
    {
        return payload_too_large();
    }
    let (parts, body) = request.into_parts();
    let limited = Body::new(Limited::new(body, max_bytes));
    next.run(Request::from_parts(parts, limited)).await
}

fn payload_too_large() -> Response {
    (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response()
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
