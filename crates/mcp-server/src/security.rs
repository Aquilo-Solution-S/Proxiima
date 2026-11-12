use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, FromFnLayer, Next};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD,
    AUTHORIZATION, HOST, ORIGIN, VARY,
};
use http::{HeaderMap, HeaderName, HeaderValue};
use http_body::{Body as HttpBody, Frame};
use http_body_util::{BodyStream, StreamBody};
use proxiima_core::{OwnerId, RevalidationConfig, revalidate_stream};

use crate::McpServerError;
use crate::auth::McpEdgeAuth;
use crate::oauth::{ResourceChallenges, ResourceServerMetadata};
use crate::session::{McpSessionBindings, parse_owner_key};
use crate::transport::McpStreamableService;

const MCP_SESSION_ID_HEADER: &str = "Mcp-Session-Id";
pub(crate) const PROXIIMA_OWNER_HEADER: &str = "X-Proxiima-Owner";
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// Browser Origin allowlist: a portless entry accepts any port; an explicit
/// port also matches a browser Origin that omits it when it is the scheme
/// default (HTTP 80 or HTTPS 443).
#[derive(Clone, Debug)]
pub struct OriginAllowlist {
    patterns: Vec<OriginPattern>,
}

#[derive(Clone, Debug)]
struct OriginPattern {
    scheme: String,
    host: String,
    port: PortMatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PortMatch {
    Any,
    Exact(u16),
}

impl OriginAllowlist {
    /// # Panics
    ///
    /// Panics when a static pattern is empty, `"*"`, or unparseable.
    #[must_use]
    pub fn new(patterns: impl IntoIterator<Item = &'static str>) -> Self {
        Self::parse(patterns).expect("static origin pattern must parse")
    }

    /// # Errors
    ///
    /// Returns `InvalidOrigin` when an entry is empty, `"*"`, or not a
    /// concrete origin pattern.
    pub fn parse<I, S>(patterns: I) -> Result<Self, McpServerError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Ok(Self {
            patterns: patterns
                .into_iter()
                .map(|pattern| parse_pattern(pattern.as_ref()))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    #[must_use]
    pub fn allows(&self, headers: &HeaderMap) -> bool {
        let mut origins = headers.get_all(ORIGIN).iter();
        let Some(origin) = origins.next() else {
            return false;
        };
        if origins.next().is_some() {
            return false;
        }
        self.allows_origin(origin)
    }

    fn allows_origin(&self, origin: &HeaderValue) -> bool {
        let Some(origin) = origin.to_str().ok() else {
            return false;
        };
        let Some(parsed) = RequestOrigin::parse(origin) else {
            return false;
        };
        self.patterns.iter().any(|pattern| {
            pattern.scheme == parsed.scheme
                && pattern.host == parsed.host
                && match pattern.port {
                    PortMatch::Any => true,
                    PortMatch::Exact(expected) => parsed.port == Some(expected),
                }
        })
    }

    /// Origin patterns for rmcp, with any-port entries spelled as `:*`.
    #[must_use]
    pub fn origins(&self) -> Vec<String> {
        self.patterns
            .iter()
            .map(|pattern| {
                let base = format!("{}://{}", pattern.scheme, pattern.host);
                match pattern.port {
                    PortMatch::Any => format!("{base}:*"),
                    PortMatch::Exact(port) => format!("{base}:{port}"),
                }
            })
            .collect()
    }
}

/// Inbound `Host` / HTTP/2 `:authority` allowlist for the whole listener.
///
/// Loopback is always present. Additional entries are bare hostnames or
/// `host:port`; an entry without a port accepts that host on any port. The
/// matching rules deliberately mirror rmcp's private DNS-rebinding guard so
/// one value can configure the outer listener and rmcp's inner `/mcp` guard.
#[derive(Clone, Debug)]
pub struct HostAllowlist {
    hosts: Vec<String>,
    authorities: Vec<NormalizedAuthority>,
}

impl HostAllowlist {
    /// # Panics
    ///
    /// Panics when a static entry is not a host or `host:port`.
    #[must_use]
    pub fn new(additional_hosts: impl IntoIterator<Item = &'static str>) -> Self {
        Self::parse(additional_hosts).expect("static host entry must parse")
    }

    /// Build the listener allowlist from deployment-specific hosts.
    ///
    /// Loopback is added unconditionally and duplicate normalized authorities
    /// are removed. Empty/blank additional entries add nothing; the resulting
    /// allowlist is therefore never empty and can never enter rmcp's
    /// allow-all-on-empty state.
    ///
    /// # Errors
    ///
    /// Returns `InvalidHost` naming an entry that is not a hostname, an IP
    /// address or `host:port`: one with a scheme, a path or userinfo, or a
    /// port that is not a number. Such an entry could never match a `Host`
    /// header, so it is refused instead of left to surface as a 403.
    pub fn parse<I, S>(additional_hosts: I) -> Result<Self, McpServerError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut allowlist = Self {
            hosts: LOOPBACK_HOSTS.into_iter().map(str::to_string).collect(),
            authorities: LOOPBACK_HOSTS
                .into_iter()
                .map(|host| normalize_authority(host, None))
                .collect(),
        };
        for raw in additional_hosts {
            let raw = raw.as_ref().trim().to_ascii_lowercase();
            if raw.is_empty() {
                continue;
            }
            let authority = parse_allowed_authority(&raw)?;
            if !allowlist.authorities.contains(&authority) {
                allowlist.hosts.push(raw);
                allowlist.authorities.push(authority);
            }
        }
        Ok(allowlist)
    }

    /// Raw authority entries passed to rmcp's inner `/mcp` guard.
    ///
    /// The slice always contains the loopback defaults and therefore never
    /// activates rmcp's empty-list allow-all behavior.
    #[must_use]
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    /// Whether an inbound `Host` / `:authority` value (`host` or
    /// `host:port`) passes this allowlist — the listener guard's own rule,
    /// for a host that re-checks the authority of a request it forwards.
    /// A malformed value is refused.
    #[must_use]
    pub fn allows(&self, host: &str) -> bool {
        http::uri::Authority::try_from(host.trim()).is_ok_and(|authority| {
            self.allows_authority(&normalize_authority(authority.host(), authority.port_u16()))
        })
    }

    fn allows_authority(&self, authority: &NormalizedAuthority) -> bool {
        self.authorities.iter().any(|allowed| {
            allowed.host == authority.host
                && match allowed.port {
                    Some(port) => authority.port == Some(port),
                    None => true,
                }
        })
    }
}

impl Default for HostAllowlist {
    fn default() -> Self {
        Self::new([])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NormalizedAuthority {
    host: String,
    port: Option<u16>,
}

fn normalize_host(host: &str) -> String {
    host.trim_matches('[')
        .trim_matches(']')
        .to_ascii_lowercase()
}

fn normalize_authority(host: &str, port: Option<u16>) -> NormalizedAuthority {
    NormalizedAuthority {
        host: normalize_host(host),
        port,
    }
}

/// A non-blank allowlist entry as the authority it matches. A bare IPv6
/// address (no brackets, so no port) is a host; anything else must be a
/// `host[:port]` authority without userinfo.
fn parse_allowed_authority(allowed: &str) -> Result<NormalizedAuthority, McpServerError> {
    if allowed.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok(normalize_authority(allowed, None));
    }
    // `Authority` also accepts userinfo and a port that is not a number
    // (its `port_u16` is then `None`), so both are refused here.
    match http::uri::Authority::try_from(allowed) {
        Ok(authority)
            if !authority.as_str().contains('@')
                && (authority.port_u16().is_some()
                    || authority.as_str().len() == authority.host().len()) =>
        {
            Ok(normalize_authority(authority.host(), authority.port_u16()))
        }
        _ => Err(McpServerError::InvalidHost(format!(
            "allowlist entry {allowed:?} is not a host or host:port"
        ))),
    }
}

#[derive(Clone, Copy, Debug)]
enum HostRequestError {
    InvalidEncoding,
    InvalidHeader,
    MissingHeader,
}

impl IntoResponse for HostRequestError {
    fn into_response(self) -> Response {
        let message = match self {
            Self::InvalidEncoding => "Bad Request: Invalid Host header encoding",
            Self::InvalidHeader => "Bad Request: Invalid Host header",
            Self::MissingHeader => "Bad Request: missing Host header",
        };
        (StatusCode::BAD_REQUEST, message).into_response()
    }
}

fn request_authority(request: &Request) -> Result<NormalizedAuthority, HostRequestError> {
    if let Some(host) = request.headers().get(HOST) {
        let host = host.to_str().map_err(|_| {
            tracing::warn!(
                category = "non_utf8",
                "rejected request with non-UTF-8 Host header"
            );
            HostRequestError::InvalidEncoding
        })?;
        return parse_request_host(host);
    }
    let authority = request.uri().authority().ok_or_else(|| {
        tracing::warn!("rejected request with missing Host header and no :authority");
        HostRequestError::MissingHeader
    })?;
    Ok(normalize_authority(authority.host(), authority.port_u16()))
}

fn parse_request_host(host: &str) -> Result<NormalizedAuthority, HostRequestError> {
    let authority = http::uri::Authority::try_from(host).map_err(|_| {
        tracing::warn!(
            category = "malformed",
            "rejected request with malformed Host header"
        );
        HostRequestError::InvalidHeader
    })?;
    Ok(normalize_authority(authority.host(), authority.port_u16()))
}

/// Concrete return type for [`host_guard_layer`].
pub type HostGuardLayer = FromFnLayer<
    fn(
        State<HostAllowlist>,
        Request,
        Next,
    ) -> Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
    HostAllowlist,
    (State<HostAllowlist>, Request),
>;

/// Listener-wide DNS-rebinding guard.
///
/// Apply this after every route has been merged and outside bearer auth. It
/// protects `/mcp`, `/v1`, host-mounted routes, and public OAuth metadata with
/// one policy; rmcp keeps its own `/mcp` check as defense in depth.
#[must_use = "apply the returned layer to the complete HTTP listener"]
pub fn host_guard_layer(allowlist: HostAllowlist) -> HostGuardLayer {
    fn dispatch(
        state: State<HostAllowlist>,
        request: Request,
        next: Next,
    ) -> Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
        Box::pin(enforce_host(state, request, next))
    }
    middleware::from_fn_with_state(allowlist, dispatch as fn(_, _, _) -> _)
}

async fn enforce_host(
    State(allowlist): State<HostAllowlist>,
    request: Request,
    next: Next,
) -> Response {
    let authority = match request_authority(&request) {
        Ok(authority) => authority,
        Err(error) => return error.into_response(),
    };
    if !allowlist.allows_authority(&authority) {
        tracing::warn!(
            category = "not_allowed",
            "rejected request with disallowed Host header (possible DNS rebinding attempt)",
        );
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::from("Forbidden: Host header is not allowed"))
            .expect("static Host rejection response is valid");
    }
    next.run(request).await
}

/// Concrete return type for [`cors_layer`].
pub type CorsLayer = FromFnLayer<
    fn(
        State<OriginAllowlist>,
        Request,
        Next,
    ) -> Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
    OriginAllowlist,
    (State<OriginAllowlist>, Request),
>;

/// Listener-wide browser CORS and Origin guard.
///
/// Apply this after every route has been merged, outside bearer auth, and
/// inside the body-size and Host guards. A native request without `Origin`
/// keeps its normal route and authentication semantics. A browser preflight
/// (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is answered before
/// auth; malformed preflight fields fail with `400`, and the actual request
/// still requires its normal bearer credentials.
#[must_use = "apply the returned layer to the complete HTTP listener"]
pub fn cors_layer(allowlist: OriginAllowlist) -> CorsLayer {
    fn dispatch(
        state: State<OriginAllowlist>,
        request: Request,
        next: Next,
    ) -> Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
        Box::pin(enforce_cors(state, request, next))
    }
    middleware::from_fn_with_state(allowlist, dispatch as fn(_, _, _) -> _)
}

async fn enforce_cors(
    State(allowlist): State<OriginAllowlist>,
    request: Request,
    next: Next,
) -> Response {
    let mut origins = request.headers().get_all(ORIGIN).iter();
    let Some(origin) = origins.next().cloned() else {
        let mut response = next.run(request).await;
        append_vary(&mut response, "Origin");
        return response;
    };
    if origins.next().is_some() || !allowlist.allows_origin(&origin) {
        tracing::warn!("rejected request with disallowed or malformed Origin header");
        let mut response = (StatusCode::FORBIDDEN, "origin not allowed").into_response();
        append_vary(&mut response, "Origin");
        return response;
    }

    if request.method() == Method::OPTIONS {
        match parse_cors_preflight(request.headers()) {
            Ok(Some(preflight)) => {
                let mut response = StatusCode::NO_CONTENT.into_response();
                response
                    .headers_mut()
                    .insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
                response
                    .headers_mut()
                    .insert(ACCESS_CONTROL_ALLOW_METHODS, preflight.method);
                for request_headers in preflight.headers {
                    response
                        .headers_mut()
                        .append(ACCESS_CONTROL_ALLOW_HEADERS, request_headers);
                }
                append_vary(&mut response, "Origin");
                append_vary(&mut response, "Access-Control-Request-Method");
                append_vary(&mut response, "Access-Control-Request-Headers");
                return response;
            }
            Ok(None) => {}
            Err(()) => {
                tracing::warn!("rejected malformed CORS preflight headers");
                let mut response =
                    (StatusCode::BAD_REQUEST, "invalid CORS preflight").into_response();
                append_vary(&mut response, "Origin");
                append_vary(&mut response, "Access-Control-Request-Method");
                append_vary(&mut response, "Access-Control-Request-Headers");
                return response;
            }
        }
    }

    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    response.headers_mut().insert(
        ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Mcp-Session-Id, WWW-Authenticate"),
    );
    append_vary(&mut response, "Origin");
    response
}

struct CorsPreflight {
    method: HeaderValue,
    headers: Vec<HeaderValue>,
}

fn parse_cors_preflight(headers: &HeaderMap) -> Result<Option<CorsPreflight>, ()> {
    let mut methods = headers.get_all(ACCESS_CONTROL_REQUEST_METHOD).iter();
    let Some(method) = methods.next().cloned() else {
        return Ok(None);
    };
    if methods.next().is_some() || Method::from_bytes(method.as_bytes()).is_err() {
        return Err(());
    }

    let request_headers: Vec<_> = headers
        .get_all(ACCESS_CONTROL_REQUEST_HEADERS)
        .iter()
        .cloned()
        .collect();
    if request_headers
        .iter()
        .any(|value| !valid_cors_header_name_list(value))
    {
        return Err(());
    }

    Ok(Some(CorsPreflight {
        method,
        headers: request_headers,
    }))
}

fn valid_cors_header_name_list(value: &HeaderValue) -> bool {
    value.as_bytes().split(|byte| *byte == b',').all(|name| {
        let name = trim_http_ows(name);
        !name.is_empty() && HeaderName::from_bytes(name).is_ok()
    })
}

fn trim_http_ows(mut value: &[u8]) -> &[u8] {
    while value
        .first()
        .is_some_and(|byte| matches!(*byte, b' ' | b'\t'))
    {
        value = &value[1..];
    }
    while value
        .last()
        .is_some_and(|byte| matches!(*byte, b' ' | b'\t'))
    {
        value = &value[..value.len() - 1];
    }
    value
}

fn append_vary(response: &mut Response, value: &'static str) {
    response
        .headers_mut()
        .append(VARY, HeaderValue::from_static(value));
}

#[must_use]
pub fn default_allowlist() -> OriginAllowlist {
    OriginAllowlist::new([
        "http://localhost",
        "http://127.0.0.1",
        "http://[::1]",
        "tauri://localhost",
        "https://tauri.localhost",
    ])
}

/// # Errors
///
/// Returns `NonLoopbackBind` for any non-loopback address.
pub fn assert_loopback(addr: &SocketAddr) -> Result<(), McpServerError> {
    if !addr.ip().is_loopback() {
        return Err(McpServerError::NonLoopbackBind(addr.ip()));
    }
    Ok(())
}

/// Concrete return type for [`mcp_auth_layer_with_config`]. The extractor
/// tuple `(State, Request)` is locked here so the public signature
/// stays a single-line `McpAuthLayer` instead of a six-line
/// `FromFnLayer<...>` (per `clippy::type_complexity`).
pub type McpAuthLayer = FromFnLayer<
    fn(
        State<McpAuthLayerState>,
        Request,
        Next,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
    McpAuthLayerState,
    (State<McpAuthLayerState>, Request),
>;

#[derive(Clone, Debug)]
pub struct McpAuthLayerState {
    auth: Arc<McpEdgeAuth>,
    sessions: McpSessionBindings,
    revalidation: RevalidationConfig,
    challenges: Option<ResourceChallenges>,
}

/// Bearer-token middleware that resolves `Authorization: Bearer
/// <wire-token>` via MCP edge auth and injects the resolved context into
/// request extensions. Accepted bearer material is host-authenticated;
/// reserved local-token prefixes fail closed.
/// Missing or unknown tokens short-circuit with HTTP 401. Browser Origin and
/// preflight handling belongs to the listener-wide [`cors_layer`].
///
/// Returns a [`McpAuthLayer`] (alias of [`FromFnLayer`]) so callers
/// apply it directly with [`axum::Router::layer`].
///
/// The layer uses `service`'s session bindings, so a refused or evicted
/// binding closes that service's session.
pub fn mcp_auth_layer_with_config(
    service: &McpStreamableService,
    auth: Arc<McpEdgeAuth>,
    revalidation: RevalidationConfig,
) -> McpAuthLayer {
    mcp_auth_layer_with_metadata(service, auth, revalidation, None)
}

#[must_use = "apply the returned layer to the MCP router"]
pub fn mcp_auth_layer_with_metadata(
    service: &McpStreamableService,
    auth: Arc<McpEdgeAuth>,
    revalidation: RevalidationConfig,
    metadata: Option<&ResourceServerMetadata>,
) -> McpAuthLayer {
    mcp_auth_layer_with_sessions(auth, service.sessions().clone(), revalidation, metadata)
}

#[must_use = "apply the returned layer to the MCP router"]
pub(crate) fn mcp_auth_layer_with_sessions(
    auth: Arc<McpEdgeAuth>,
    sessions: McpSessionBindings,
    revalidation: RevalidationConfig,
    metadata: Option<&ResourceServerMetadata>,
) -> McpAuthLayer {
    fn dispatch(
        state: State<McpAuthLayerState>,
        request: Request,
        next: Next,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
        Box::pin(mcp_auth(state, request, next))
    }
    middleware::from_fn_with_state(
        McpAuthLayerState {
            auth,
            sessions,
            revalidation,
            challenges: metadata.and_then(ResourceChallenges::new),
        },
        dispatch as fn(_, _, _) -> _,
    )
}

async fn mcp_auth(
    State(state): State<McpAuthLayerState>,
    mut request: Request,
    next: Next,
) -> Response {
    // Which document a 401 points at follows the path the client asked for.
    let path = request.uri().path().to_owned();
    let Some(token) = extract_bearer(&request) else {
        return unauthorized(&state, &path);
    };
    // Validate the bearer BEFORE resolving owner/session so an invalid
    // token always yields 401 regardless of session/owner-header state.
    // This closes the 401/403 oracle: an unauthenticated caller learns
    // nothing about owner or session requirements. Owner binding happens
    // after selection, from this retained authentication result — plus, for
    // a Group owner its role map does not carry, one per-owner resolution
    // through the host's access port.
    let Some(resolved) = state.auth.resolve_unbound(&token).await else {
        return unauthorized(&state, &path);
    };
    let (selected_owner, bind_new_session, via_session) = match selected_owner(
        &state.sessions,
        request.headers(),
        resolved.principal(),
        resolved.default_owner(),
        resolved.verified_forwarded_owner(),
    )
    .await
    {
        OwnerSelection::Selected {
            owner,
            bind_new,
            via_session,
        } => (owner, bind_new, via_session),
        // An `Mcp-Session-Id` header that is present but not bound
        // (unknown or idle-evicted) answers 404, which standard
        // Streamable-HTTP clients transparently re-initialize on. Only
        // the token-valid path reaches here, so 404 never leaks to an
        // unauthenticated caller.
        OwnerSelection::UnknownSession => return session_not_found(),
        OwnerSelection::Missing => {
            return (StatusCode::FORBIDDEN, "owner selection required or invalid").into_response();
        }
    };
    let Some(ctx) = state.auth.bind_to_owner(resolved, selected_owner).await else {
        return if via_session {
            session_not_found()
        } else {
            unauthorized(&state, &path)
        };
    };
    let principal = ctx.authz.principal();
    let expires_at = ctx.authz.expires_at();
    request.extensions_mut().insert(ctx);
    let mut response = next.run(request).await;
    if bind_new_session
        && let Some(session_id) = response_header_str(response.headers(), MCP_SESSION_ID_HEADER)
    {
        let bound = state
            .sessions
            .bind(session_id, selected_owner, principal)
            .await;
        if !bound {
            response.headers_mut().remove(MCP_SESSION_ID_HEADER);
            *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        }
    }
    revalidate_response(response, expires_at, state.revalidation)
}

/// Outcome of resolving the caller's owner selection from the
/// `Mcp-Session-Id` / `X-Proxiima-Owner` headers.
enum OwnerSelection {
    /// A concrete owner was selected. `bind_new` requests binding the
    /// response's freshly minted session id to this owner. `via_session`
    /// records whether the owner came from an existing session binding.
    Selected {
        owner: OwnerId,
        bind_new: bool,
        via_session: bool,
    },
    /// An `Mcp-Session-Id` header is malformed, has no live binding, or
    /// selects an owner outside the current verified claim. Distinct from
    /// `Missing` so the transport answers the same opaque session 404.
    UnknownSession,
    /// No usable owner selection: no session id, no owner header and no
    /// host-named default; an invalid owner header; or an owner header that
    /// conflicts with the bound session.
    Missing,
}

/// A session binding wins for ordinary credentials, followed by the owner
/// header and host-named default. A forwarding claim is mandatory: a live
/// session and any owner header must agree with its owner. A present but
/// malformed owner header never falls back to a claim or default.
async fn selected_owner(
    sessions: &McpSessionBindings,
    headers: &HeaderMap,
    principal: OwnerId,
    default: Option<OwnerId>,
    forwarded: Option<OwnerId>,
) -> OwnerSelection {
    let owner_header = headers
        .get(PROXIIMA_OWNER_HEADER)
        .map(|raw| raw.to_str().ok().and_then(parse_owner_key));
    if let Some(raw_session_id) = headers.get(MCP_SESSION_ID_HEADER) {
        let Ok(session_id) = raw_session_id.to_str() else {
            return OwnerSelection::UnknownSession;
        };
        let Some(owner) = sessions.owner_for(session_id, principal).await else {
            return OwnerSelection::UnknownSession;
        };
        if forwarded.is_some_and(|claimed| claimed != owner) {
            return OwnerSelection::UnknownSession;
        }
        if owner_header.is_some_and(|requested| requested != Some(owner)) {
            return OwnerSelection::Missing;
        }
        return OwnerSelection::Selected {
            owner,
            bind_new: false,
            via_session: true,
        };
    }

    let owner = match owner_header {
        Some(requested) => requested,
        None => forwarded.or(default),
    };
    if forwarded.is_some_and(|claimed| Some(claimed) != owner) {
        return OwnerSelection::Missing;
    }
    match owner {
        Some(owner) => OwnerSelection::Selected {
            owner,
            bind_new: true,
            via_session: false,
        },
        None => OwnerSelection::Missing,
    }
}

/// 404 for a present-but-unbound `Mcp-Session-Id`. Matches rmcp's
/// unknown-session behavior: standard Streamable-HTTP clients auto-issue
/// a fresh `initialize` on 404.
fn session_not_found() -> Response {
    (StatusCode::NOT_FOUND, "unknown or expired MCP session").into_response()
}

fn unauthorized(state: &McpAuthLayerState, path: &str) -> Response {
    let mut resp = StatusCode::UNAUTHORIZED.into_response();
    if let Some(challenges) = &state.challenges {
        resp.headers_mut().insert(
            http::header::WWW_AUTHENTICATE,
            challenges.for_path(path).clone(),
        );
    }
    resp
}

fn revalidate_response(
    response: Response,
    expires_at: Option<SystemTime>,
    config: RevalidationConfig,
) -> Response {
    response.map(|body| Body::new(RevalidatedBody::new(body, expires_at, config)))
}

fn response_header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

type FrameStream<D, E> = Pin<Box<dyn Stream<Item = Result<Frame<D>, E>> + Send>>;
type RevalidatedInner<D, E> = Pin<Box<StreamBody<FrameStream<D, E>>>>;

struct RevalidatedBody<B>
where
    B: HttpBody + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Send + 'static,
{
    inner: RevalidatedInner<B::Data, B::Error>,
}

impl<B> RevalidatedBody<B>
where
    B: HttpBody + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Send + 'static,
{
    fn new(body: B, expires_at: Option<SystemTime>, config: RevalidationConfig) -> Self {
        Self {
            inner: Box::pin(StreamBody::new(revalidate_stream(
                BodyStream::new(body),
                expires_at,
                config,
            ))),
        }
    }
}

impl<B> std::fmt::Debug for RevalidatedBody<B>
where
    B: HttpBody + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Send + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RevalidatedBody").finish_non_exhaustive()
    }
}

impl<B> HttpBody for RevalidatedBody<B>
where
    B: HttpBody + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Send + 'static,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.inner.as_mut().poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        HttpBody::size_hint(&self.inner)
    }
}

fn extract_bearer(request: &Request) -> Option<String> {
    let header_value = request.headers().get(AUTHORIZATION)?;
    let raw = header_value.to_str().ok()?;
    Some(strip_bearer_scheme(raw)?.to_string())
}

/// Strip the `Bearer` auth scheme, returning the trimmed credential.
///
/// RFC 9110 §11.1 / RFC 6750 §2.1: the scheme token is
/// case-insensitive, so a spec-compliant `bearer <token>` must be
/// accepted alongside `Bearer <token>`. Matching is ASCII-only; the
/// whitespace contract is unchanged from the original (a single space
/// after the scheme), and an empty credential fails closed to `None`.
fn strip_bearer_scheme(raw: &str) -> Option<&str> {
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

fn parse_pattern(value: &str) -> Result<OriginPattern, McpServerError> {
    let pattern = value.trim();
    if pattern.is_empty() || pattern == "*" {
        return Err(invalid_origin(value));
    }
    let Some((scheme, host, port)) = parse_origin_parts(pattern) else {
        return Err(invalid_origin(value));
    };
    if scheme.is_empty() || host.is_empty() {
        return Err(invalid_origin(value));
    }
    Ok(OriginPattern {
        scheme,
        host,
        port: port.map_or(PortMatch::Any, PortMatch::Exact),
    })
}

fn invalid_origin(value: &str) -> McpServerError {
    McpServerError::InvalidOrigin(format!("origin pattern {value:?}"))
}

struct RequestOrigin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl RequestOrigin {
    fn parse(value: &str) -> Option<Self> {
        let (scheme, host, port) = parse_origin_parts(value)?;
        let port = port.or(match scheme.as_str() {
            "http" => Some(80),
            "https" => Some(443),
            _ => None,
        });
        Some(Self { scheme, host, port })
    }
}

fn parse_origin_parts(value: &str) -> Option<(String, String, Option<u16>)> {
    let (scheme, rest) = value.split_once("://")?;
    if rest.contains(['/', '?', '#', '@']) {
        return None;
    }
    let (host, port) = if let Some(rest) = rest.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = if let Some(port) = after.strip_prefix(':') {
            Some(port.parse().ok()?)
        } else if after.is_empty() {
            None
        } else {
            return None;
        };
        (format!("[{}]", host.to_ascii_lowercase()), port)
    } else if let Some((host, port)) = rest.rsplit_once(':') {
        (host.to_ascii_lowercase(), Some(port.parse().ok()?))
    } else {
        (rest.to_ascii_lowercase(), None)
    };
    Some((scheme.to_ascii_lowercase(), host, port))
}

#[cfg(test)]
#[path = "security/header_logging_tests.rs"]
mod header_logging_tests;

#[cfg(test)]
#[path = "security_tests.rs"]
mod tests;
