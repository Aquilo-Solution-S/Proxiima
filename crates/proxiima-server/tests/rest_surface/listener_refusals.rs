//! The listener's own refusals in front of `/v1`: each one is a problem
//! document, and the same refusal answers the same `type` on `/mcp`.

use std::convert::Infallible;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use proxiima_core::{
    AuthError, AuthPath, Authenticator, AuthzContext, Credentials, OwnerId, RevalidationConfig,
    UserId,
};
use proxiima_server::{
    AuthorizationServers, EdgeAuth, HostAllowlist, MCP_PATH, ResourceServerMetadata,
    auth_layer_with_metadata, default_allowlist, guard_listener, owner_key,
    streamable_http_service,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::host;

const REST: &str = "/v1/tools";
const BODY_LIMIT: usize = 64;

/// Accepts exactly `good-token`, for one owner.
struct TokenAuth {
    owner: OwnerId,
}

#[async_trait]
impl Authenticator for TokenAuth {
    async fn authenticate(&self, creds: &Credentials) -> Result<AuthzContext, AuthError> {
        match creds {
            Credentials::Bearer(token) if token == "good-token" => Ok(AuthzContext::single_owner(
                &self.owner,
                AuthPath::HostBearer,
            )),
            Credentials::Bearer(_) => Err(AuthError::InvalidCredentials),
        }
    }
}

/// `/v1` and `/mcp` behind the production layer order: bearer auth with
/// its challenges, then the Origin, Host and body-size guards.
fn listener(owner: OwnerId) -> Router {
    let host = host();
    let origins = default_allowlist();
    let hosts = HostAllowlist::default();
    let service =
        streamable_http_service(host.clone(), &origins, &hosts, &CancellationToken::new());
    let metadata = ResourceServerMetadata {
        public_url: "https://p.example.com".into(),
        authorization_servers: AuthorizationServers::Issuers(vec![
            "https://idp.example.com".into(),
        ]),
        additional_scopes: Vec::new(),
    };
    let auth = EdgeAuth::headless().with_host(Arc::new(TokenAuth { owner }));
    guard_listener(
        proxiima_server::rest::router(host, None)
            .nest_service(MCP_PATH, service.clone())
            .layer(auth_layer_with_metadata(
                &service,
                Arc::new(auth),
                RevalidationConfig::default(),
                Some(&metadata),
            )),
        origins,
        hosts,
        BODY_LIMIT,
    )
}

/// A loopback request to `path` carrying `headers`.
fn request(method: Method, path: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "localhost");
    for &(name, value) in headers {
        builder = builder.header(name, value);
    }
    builder.body(Body::empty()).expect("request builds")
}

/// Send `request`; it answers `status` as a problem document of `slug`
/// whose `instance` is the request path. Returns the answer's headers.
async fn refused(
    app: &Router,
    request: Request<Body>,
    status: StatusCode,
    slug: &str,
) -> HeaderMap {
    let path = request.uri().path().to_owned();
    let response = app.clone().oneshot(request).await.expect("infallible");
    assert_eq!(response.status(), status, "{path}");
    let headers = response.headers().clone();
    assert_eq!(
        headers.get(header::CONTENT_TYPE).expect("content type"),
        "application/problem+json",
        "{path}"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collects");
    let problem: serde_json::Value = serde_json::from_slice(&body).expect("problem JSON");
    assert_eq!(
        problem["type"],
        format!("https://proxiima.dev/errors/{slug}"),
        "{path}: {problem}"
    );
    assert_eq!(problem["status"], status.as_u16(), "{path}");
    assert_eq!(problem["instance"], path.as_str(), "{path}");
    headers
}

/// [`refused`] on `/v1/tools` and on `/mcp` alike. Returns the REST
/// answer's headers.
async fn refused_alike(
    app: &Router,
    make: impl Fn(&str) -> Request<Body>,
    status: StatusCode,
    slug: &str,
) -> HeaderMap {
    refused(app, make(MCP_PATH), status, slug).await;
    refused(app, make(REST), status, slug).await
}

fn owner() -> OwnerId {
    OwnerId::from(UserId::new(uuid::Uuid::now_v7()))
}

#[tokio::test]
async fn a_host_the_allowlist_refuses_is_a_403_problem() {
    let app = listener(owner());
    refused_alike(
        &app,
        |path| {
            let mut request = request(Method::GET, path, &[]);
            request
                .headers_mut()
                .insert(header::HOST, header::HeaderValue::from_static("evil.test"));
            request
        },
        StatusCode::FORBIDDEN,
        "host-not-allowed",
    )
    .await;
}

#[tokio::test]
async fn a_missing_host_is_a_400_problem() {
    let app = listener(owner());
    refused_alike(
        &app,
        |path| {
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("request builds")
        },
        StatusCode::BAD_REQUEST,
        "invalid-host",
    )
    .await;
}

#[tokio::test]
async fn an_origin_the_allowlist_refuses_is_a_403_problem_that_varies_on_origin() {
    let app = listener(owner());
    let headers = refused_alike(
        &app,
        |path| request(Method::GET, path, &[("Origin", "https://evil.test")]),
        StatusCode::FORBIDDEN,
        "origin-not-allowed",
    )
    .await;
    assert_eq!(headers.get(header::VARY).expect("vary"), "Origin");
}

#[tokio::test]
async fn a_malformed_cors_preflight_is_a_400_problem() {
    let app = listener(owner());
    let headers = refused_alike(
        &app,
        |path| {
            request(
                Method::OPTIONS,
                path,
                &[
                    ("Origin", "http://localhost"),
                    ("Access-Control-Request-Method", "POST, DELETE"),
                ],
            )
        },
        StatusCode::BAD_REQUEST,
        "invalid-cors-preflight",
    )
    .await;
    assert!(!headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
}

#[tokio::test]
async fn a_missing_or_unknown_bearer_is_a_401_problem_with_its_challenge() {
    let app = listener(owner());
    for bearer in [None, Some("Bearer bad-token")] {
        let headers = refused_alike(
            &app,
            |path| match bearer {
                Some(bearer) => request(Method::GET, path, &[("Authorization", bearer)]),
                None => request(Method::GET, path, &[]),
            },
            StatusCode::UNAUTHORIZED,
            "auth-required",
        )
        .await;
        assert_eq!(
            headers
                .get(header::WWW_AUTHENTICATE)
                .expect("the 401 keeps its challenge"),
            "Bearer resource_metadata=\"https://p.example.com/.well-known/oauth-protected-resource\""
        );
    }
}

#[tokio::test]
async fn a_refused_owner_selection_is_a_403_problem() {
    let app = listener(owner());
    refused_alike(
        &app,
        |path| {
            request(
                Method::GET,
                path,
                &[
                    ("Authorization", "Bearer good-token"),
                    ("X-Proxiima-Owner", "not-an-owner"),
                ],
            )
        },
        StatusCode::FORBIDDEN,
        "owner-selection-refused",
    )
    .await;
}

#[tokio::test]
async fn an_unknown_session_is_a_404_problem() {
    let owner = owner();
    let owner_header = owner_key(owner);
    let app = listener(owner);
    refused_alike(
        &app,
        |path| {
            request(
                Method::GET,
                path,
                &[
                    ("Authorization", "Bearer good-token"),
                    ("X-Proxiima-Owner", &owner_header),
                    ("Mcp-Session-Id", "never-bound"),
                ],
            )
        },
        StatusCode::NOT_FOUND,
        "session-not-found",
    )
    .await;
}

#[tokio::test]
async fn a_body_over_the_limit_is_a_413_problem() {
    let app = listener(owner());
    let over = (BODY_LIMIT + 1).to_string();
    refused_alike(
        &app,
        |path| request(Method::POST, path, &[("Content-Length", &over)]),
        StatusCode::PAYLOAD_TOO_LARGE,
        "body-too-large",
    )
    .await;
}

/// An authenticated request, so the refusal under test is the route's.
fn authenticated(method: Method, path: &str, owner: OwnerId) -> Request<Body> {
    request(
        method,
        path,
        &[
            ("Authorization", "Bearer good-token"),
            ("X-Proxiima-Owner", &owner_key(owner)),
        ],
    )
}

#[tokio::test]
async fn a_body_streamed_past_the_limit_is_the_same_413_problem() {
    let owner = owner();
    let app = listener(owner);
    let mut request = authenticated(Method::POST, "/v1/tools/core_memory_spaces", owner);
    // A stream has no size hint, so the request carries no Content-Length
    // and only the body reader can see the cap crossed.
    *request.body_mut() = Body::from_stream(futures_util::stream::iter([Ok::<_, Infallible>(
        Bytes::from(vec![b'{'; BODY_LIMIT + 1]),
    )]));
    assert!(!request.headers().contains_key(header::CONTENT_LENGTH));
    refused(
        &app,
        request,
        StatusCode::PAYLOAD_TOO_LARGE,
        "body-too-large",
    )
    .await;
}

#[tokio::test]
async fn a_method_a_route_does_not_serve_is_a_405_problem_with_allow() {
    let owner = owner();
    let app = listener(owner);
    let headers = refused(
        &app,
        authenticated(Method::DELETE, REST, owner),
        StatusCode::METHOD_NOT_ALLOWED,
        "method-not-allowed",
    )
    .await;
    assert_eq!(headers.get(header::ALLOW).expect("Allow"), "GET,HEAD");
}

#[tokio::test]
async fn a_path_no_route_matches_is_a_404_problem() {
    let owner = owner();
    let app = listener(owner);
    for path in ["/v1/nope", "/v1/tools/core_goal/set/extra"] {
        refused(
            &app,
            authenticated(Method::GET, path, owner),
            StatusCode::NOT_FOUND,
            "not-found",
        )
        .await;
    }
}

#[tokio::test]
async fn a_path_segment_that_is_not_utf8_is_a_400_problem() {
    let owner = owner();
    let app = listener(owner);
    for path in ["/v1/tools/%FF", "/v1/tools/core_goal/%FF"] {
        refused(
            &app,
            authenticated(Method::POST, path, owner),
            StatusCode::BAD_REQUEST,
            "invalid-input",
        )
        .await;
    }
}
