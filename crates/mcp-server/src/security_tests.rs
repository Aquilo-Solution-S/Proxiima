use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures_util::stream;
use proxiima_core::{
    AccessError, AccessKind, AuthError, AuthPath, Authenticator, AuthzContext, Credentials,
    ForwarderRole, GroupId, OwnerAccessPort, OwnerId, OwnerRoles, RevalidationConfig, Role, UserId,
};
use tokio::sync::mpsc;
use tokio::time;
use tower::ServiceExt;

use super::{
    HostAllowlist, OriginAllowlist, RevalidatedBody, cors_layer, default_allowlist,
    host_guard_layer, mcp_auth_layer_with_sessions,
};
use crate::McpServerError;
use crate::auth::{McpAuthContext, McpEdgeAuth};
use crate::session::{McpSessionBindings, owner_key};

/// Host authenticator that accepts exactly `good-token` for `owner`.
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

struct SharedOwnerTokenAuth {
    owner: OwnerId,
    first: UserId,
    second: UserId,
}

#[async_trait]
impl Authenticator for SharedOwnerTokenAuth {
    async fn authenticate(&self, creds: &Credentials) -> Result<AuthzContext, AuthError> {
        let subject = match creds {
            Credentials::Bearer(token) if token == "first-token" => self.first,
            Credentials::Bearer(token) if token == "second-token" => self.second,
            Credentials::Bearer(_) => return Err(AuthError::InvalidCredentials),
        };
        Ok(AuthzContext::for_subject_with_role(
            subject,
            [(self.owner, Role::viewer())],
            AuthPath::HostBearer,
        ))
    }
}

struct CountingTokenAuth {
    owner: OwnerId,
    calls: Arc<AtomicU64>,
}

#[async_trait]
impl Authenticator for CountingTokenAuth {
    async fn authenticate(&self, creds: &Credentials) -> Result<AuthzContext, AuthError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match creds {
            Credentials::Bearer(token) if token == "good-token" => Ok(AuthzContext::single_owner(
                &self.owner,
                AuthPath::HostBearer,
            )),
            Credentials::Bearer(_) => Err(AuthError::InvalidCredentials),
        }
    }
}

fn user_owner() -> OwnerId {
    OwnerId::from(UserId::new(uuid::Uuid::now_v7()))
}

/// Router mirroring the production stack: the auth layer over an OK
/// `/mcp` stub, seeded with `sessions` and a `TokenAuth` for `owner`.
fn auth_app(owner: OwnerId, sessions: McpSessionBindings) -> Router {
    auth_app_with_auth(Arc::new(TokenAuth { owner }), sessions)
}

fn auth_app_with_auth(
    authenticator: Arc<dyn Authenticator>,
    sessions: McpSessionBindings,
) -> Router {
    let auth = McpEdgeAuth::headless().with_host(authenticator);
    Router::new()
        .route("/mcp", any(|| async { StatusCode::OK }))
        .layer(mcp_auth_layer_with_sessions(
            Arc::new(auth),
            sessions,
            RevalidationConfig::default(),
            None,
        ))
}

/// An ordinary subject whose Group memberships are resolved on demand
/// rather than enumerated in its initial role map.
struct GroupMemberAuth {
    subject: UserId,
}

#[async_trait]
impl Authenticator for GroupMemberAuth {
    async fn authenticate(&self, creds: &Credentials) -> Result<AuthzContext, AuthError> {
        match creds {
            Credentials::Bearer(token) if token == "good-token" => Ok(AuthzContext::for_subject(
                self.subject,
                AuthPath::HostBearer,
            )),
            Credentials::Bearer(_) => Err(AuthError::InvalidCredentials),
        }
    }
}

/// Access port that answers one owner at a time and never enumerates:
/// its eager map is empty, so every accepted party in these tests was
/// resolved through [`OwnerAccessPort::resolve_group_role`].
struct PerOwnerAccess {
    subject: UserId,
    roles: HashMap<OwnerId, Role>,
    calls: Arc<AtomicU64>,
}

impl PerOwnerAccess {
    fn new(subject: UserId, roles: impl IntoIterator<Item = (OwnerId, Role)>) -> Self {
        Self {
            subject,
            roles: roles.into_iter().collect(),
            calls: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[async_trait]
impl OwnerAccessPort for PerOwnerAccess {
    async fn resolve_roles_for_subject(&self, subject: UserId) -> Result<OwnerRoles, AccessError> {
        OwnerRoles::for_subject(subject, [])
    }

    async fn resolve_group_role(
        &self,
        subject: UserId,
        group: GroupId,
    ) -> Result<Option<Role>, AccessError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if subject != self.subject {
            return Ok(None);
        }
        Ok(self.roles.get(&OwnerId::from(group)).copied())
    }
}

/// Production stack with an access port attached, over a stub that
/// reports the effective owners available to the request.
fn group_access_app(
    authenticator: Arc<dyn Authenticator>,
    owner_access: Arc<dyn OwnerAccessPort>,
    sessions: McpSessionBindings,
) -> Router {
    let auth = McpEdgeAuth::headless()
        .with_host(authenticator)
        .with_owner_access(owner_access);
    Router::new()
        .route("/mcp", any(effective_owners))
        .layer(mcp_auth_layer_with_sessions(
            Arc::new(auth),
            sessions,
            RevalidationConfig::default(),
            None,
        ))
}

/// Body = the owners this request may read, so a test can see the
/// narrowing and not just the status code.
async fn effective_owners(request: Request<Body>) -> Response {
    let Some(ctx) = request.extensions().get::<McpAuthContext>() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut owners: Vec<String> = ctx
        .authz
        .readable_owners(AccessKind::Perspective)
        .into_iter()
        .map(owner_key)
        .collect();
    owners.sort();
    (StatusCode::OK, owners.join(",")).into_response()
}

async fn effective_owners_response(app: Router, request: Request<Body>) -> (StatusCode, Bytes) {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, body)
}

struct VerifiedForwarderAuth {
    subject: UserId,
    claims: HashMap<String, OwnerId>,
}

#[async_trait]
impl Authenticator for VerifiedForwarderAuth {
    async fn authenticate(&self, creds: &Credentials) -> Result<AuthzContext, AuthError> {
        let Credentials::Bearer(token) = creds;
        let owner = self
            .claims
            .get(token)
            .ok_or(AuthError::InvalidCredentials)?;
        Ok(AuthzContext::from_verified_forwarded_owner(
            self.subject,
            *owner,
            proxiima_core::WriterAttribution {
                by: uuid::Uuid::from_u128(509),
                via: None,
            },
        ))
    }
}

struct ForwardedOwnerRegistry {
    subject: UserId,
    owners: Vec<OwnerId>,
    probes: Arc<AtomicU64>,
}

#[async_trait]
impl OwnerAccessPort for ForwardedOwnerRegistry {
    async fn resolve_roles_for_subject(&self, subject: UserId) -> Result<OwnerRoles, AccessError> {
        OwnerRoles::for_subject(subject, [])
    }

    fn is_forwarder(&self, subject: UserId) -> Result<bool, AccessError> {
        Ok(subject == self.subject)
    }

    async fn resolve_forwarded_owner_role(
        &self,
        subject: UserId,
        owner: OwnerId,
    ) -> Result<Option<ForwarderRole>, AccessError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        Ok((subject == self.subject && self.owners.contains(&owner))
            .then_some(ForwarderRole::Editor))
    }
}

fn verified_forwarder_app(
    subject: UserId,
    claims: impl IntoIterator<Item = (&'static str, OwnerId)>,
    owners: Vec<OwnerId>,
    sessions: McpSessionBindings,
) -> (Router, Arc<AtomicU64>) {
    let probes = Arc::new(AtomicU64::new(0));
    let auth = McpEdgeAuth::headless()
        .with_host(Arc::new(VerifiedForwarderAuth {
            subject,
            claims: claims
                .into_iter()
                .map(|(token, owner)| (token.to_owned(), owner))
                .collect(),
        }))
        .with_owner_access(Arc::new(ForwardedOwnerRegistry {
            subject,
            owners,
            probes: probes.clone(),
        }));
    let app = Router::new()
        .route(
            "/mcp",
            any(|request: Request<Body>| async move {
                let mut response = effective_owners(request).await;
                response
                    .headers_mut()
                    .insert("Mcp-Session-Id", "sess-forwarded".parse().unwrap());
                response
            }),
        )
        .layer(mcp_auth_layer_with_sessions(
            Arc::new(auth),
            sessions,
            RevalidationConfig::default(),
            None,
        ));
    (app, probes)
}

#[tokio::test]
async fn forwarded_owner_claim_selects_without_header_and_accepts_only_equal_header() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    for owner in [user_owner(), group_owner()] {
        let sessions = McpSessionBindings::default();
        let (app, probes) = verified_forwarder_app(
            subject,
            [("good-token", owner)],
            vec![owner],
            sessions.clone(),
        );
        for selected in [None, Some(owner_key(owner))] {
            let mut request = mcp_request("Bearer good-token");
            if let Some(selected) = selected {
                request = request.header("X-Proxiima-Owner", selected);
            }
            assert_eq!(
                effective_owners_response(app.clone(), request.body(Body::empty()).unwrap()).await,
                (StatusCode::OK, Bytes::from(owner_key(owner)))
            );
        }
        assert_eq!(
            sessions.owner_for("sess-forwarded", subject.into()).await,
            Some(owner)
        );
        assert_eq!(probes.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn forwarded_owner_header_mismatch_and_malformed_bytes_refuse_before_registry_probe() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let owner = user_owner();
    let (app, probes) = verified_forwarder_app(
        subject,
        [("good-token", owner)],
        vec![owner],
        McpSessionBindings::default(),
    );
    let headers = [
        owner_key(user_owner()).parse().unwrap(),
        owner_key(group_owner()).parse().unwrap(),
        http::HeaderValue::from_static("user:not-a-uuid"),
        http::HeaderValue::from_bytes(&[0xff]).unwrap(),
    ];
    for header in headers {
        let request = mcp_request("Bearer good-token")
            .header("X-Proxiima-Owner", header)
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(app.clone(), request).await, StatusCode::FORBIDDEN);
    }
    assert_eq!(probes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn forwarded_owner_live_session_must_match_the_current_token_claim() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let owner = user_owner();
    let other = group_owner();
    let sessions = McpSessionBindings::default();
    sessions.bind("live-session", owner, subject.into()).await;
    let (app, probes) = verified_forwarder_app(
        subject,
        [("good-token", owner), ("other-token", other)],
        vec![owner, other],
        sessions.clone(),
    );

    let matching = mcp_request("Bearer good-token")
        .header("Mcp-Session-Id", "live-session")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        effective_owners_response(app.clone(), matching).await,
        (StatusCode::OK, Bytes::from(owner_key(owner)))
    );
    assert_eq!(probes.load(Ordering::SeqCst), 1);

    for session in ["live-session", "unknown-session"] {
        let mismatching = mcp_request("Bearer other-token")
            .header("Mcp-Session-Id", session)
            .header("X-Proxiima-Owner", owner_key(other))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            effective_owners_response(app.clone(), mismatching).await,
            (
                StatusCode::NOT_FOUND,
                Bytes::from_static(b"unknown or expired MCP session")
            )
        );
    }
    assert_eq!(probes.load(Ordering::SeqCst), 1);
    assert_eq!(
        sessions.owner_for("live-session", subject.into()).await,
        Some(owner)
    );
}

#[tokio::test]
async fn a_shared_owner_does_not_allow_cross_principal_session_reuse() {
    let owner = group_owner();
    let first = UserId::new(uuid::Uuid::now_v7());
    let second = UserId::new(uuid::Uuid::now_v7());
    let sessions = McpSessionBindings::default();
    sessions.bind("first-session", owner, first.into()).await;
    let app = auth_app_with_auth(
        Arc::new(SharedOwnerTokenAuth {
            owner,
            first,
            second,
        }),
        sessions.clone(),
    );

    let request = mcp_request("Bearer second-token")
        .header("Mcp-Session-Id", "first-session")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        status_of(app, request).await,
        StatusCode::NOT_FOUND,
        "principal mismatch is indistinguishable from an unknown session"
    );
    assert_eq!(
        sessions.owner_for("first-session", first.into()).await,
        Some(owner)
    );
}

#[tokio::test]
async fn forwarded_owner_session_header_disagreement_refuses_before_registry_probe() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let owner = group_owner();
    let sessions = McpSessionBindings::default();
    sessions.bind("live-session", owner, subject.into()).await;
    let (app, probes) =
        verified_forwarder_app(subject, [("good-token", owner)], vec![owner], sessions);
    for header in [
        owner_key(user_owner()).parse().unwrap(),
        http::HeaderValue::from_bytes(&[0xff]).unwrap(),
    ] {
        let request = mcp_request("Bearer good-token")
            .header("Mcp-Session-Id", "live-session")
            .header("X-Proxiima-Owner", header)
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(app.clone(), request).await, StatusCode::FORBIDDEN);
    }
    assert_eq!(probes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn forwarded_owner_unknown_session_never_falls_back_to_claim_or_header() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let owner = user_owner();
    let (app, probes) = verified_forwarder_app(
        subject,
        [("good-token", owner)],
        vec![owner],
        McpSessionBindings::default(),
    );
    for session in [
        http::HeaderValue::from_static("unknown-session"),
        http::HeaderValue::from_bytes(&[0xff]).unwrap(),
    ] {
        for header in [None, Some(owner_key(owner))] {
            let mut request =
                mcp_request("Bearer good-token").header("Mcp-Session-Id", session.clone());
            if let Some(header) = header {
                request = request.header("X-Proxiima-Owner", header);
            }
            assert_eq!(
                status_of(app.clone(), request.body(Body::empty()).unwrap()).await,
                StatusCode::NOT_FOUND
            );
        }
    }
    let invalid_token = mcp_request("Bearer bad-token")
        .header("Mcp-Session-Id", "unknown-session")
        .header("X-Proxiima-Owner", "invalid")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        status_of(app, invalid_token).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(probes.load(Ordering::SeqCst), 0);
}

fn group_owner() -> OwnerId {
    OwnerId::from(GroupId::new(uuid::Uuid::now_v7()))
}

// One ordinary subject keeps every owner role carried by the credential;
// the selected party remains the request's default destination.
#[tokio::test]
async fn an_ordinary_group_subject_keeps_effective_roles_per_request() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let first = group_owner();
    let second = group_owner();
    let unresolved = group_owner();
    let app = group_access_app(
        Arc::new(GroupMemberAuth { subject }),
        Arc::new(PerOwnerAccess::new(
            subject,
            [(first, Role::editor()), (second, Role::viewer())],
        )),
        McpSessionBindings::default(),
    );

    for party in [first, second, first] {
        let request = mcp_request("Bearer good-token")
            .header("X-Proxiima-Owner", owner_key(party))
            .body(Body::empty())
            .unwrap();
        let (status, body) = effective_owners_response(app.clone(), request).await;
        assert_eq!(status, StatusCode::OK);
        let expected = {
            let mut owners = [owner_key(party), owner_key(OwnerId::from(subject))];
            owners.sort();
            owners.join(",")
        };
        assert_eq!(body, Bytes::from(expected));
    }

    let request = mcp_request("Bearer good-token")
        .header("X-Proxiima-Owner", owner_key(unresolved))
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(app, request).await, StatusCode::UNAUTHORIZED);
}

// A session binding selects one party, and the access port resolves its
// role. An ordinary subject can select another party on a session-less
// request without changing the earlier binding.
#[tokio::test]
async fn a_bound_session_and_a_later_owner_header_each_resolve_per_request() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let bound = group_owner();
    let other = group_owner();
    let sessions = McpSessionBindings::default();
    sessions.bind("sess-1", bound, subject.into()).await;
    let app = group_access_app(
        Arc::new(GroupMemberAuth { subject }),
        Arc::new(PerOwnerAccess::new(
            subject,
            [(bound, Role::editor()), (other, Role::editor())],
        )),
        sessions,
    );
    let expected_for = |owner| {
        let mut owners = [owner_key(owner), owner_key(OwnerId::from(subject))];
        owners.sort();
        owners.join(",")
    };

    let via_session = mcp_request("Bearer good-token")
        .header("Mcp-Session-Id", "sess-1")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        effective_owners_response(app.clone(), via_session).await,
        (StatusCode::OK, Bytes::from(expected_for(bound)))
    );

    let via_header = mcp_request("Bearer good-token")
        .header("X-Proxiima-Owner", owner_key(other))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        effective_owners_response(app.clone(), via_header).await,
        (StatusCode::OK, Bytes::from(expected_for(other)))
    );

    let back_via_session = mcp_request("Bearer good-token")
        .header("Mcp-Session-Id", "sess-1")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        effective_owners_response(app, back_via_session).await,
        (StatusCode::OK, Bytes::from(expected_for(bound))),
        "the default binding is untouched by the other party's request"
    );
}

// A session whose bound party the port refuses answers 404, the
// session-path refusal this edge already gave.
#[tokio::test]
async fn a_session_bound_to_an_unresolvable_party_is_not_found() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let sessions = McpSessionBindings::default();
    let owner = group_owner();
    sessions.bind("sess-1", owner, owner).await;
    let app = group_access_app(
        Arc::new(GroupMemberAuth { subject }),
        Arc::new(PerOwnerAccess::new(subject, [])),
        sessions,
    );

    let request = mcp_request("Bearer good-token")
        .header("Mcp-Session-Id", "sess-1")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(app, request).await, StatusCode::NOT_FOUND);
}

// Without an access port the edge is what it was: the authenticated
// role map is the only answer, and a Group it lacks is refused.
#[tokio::test]
async fn an_edge_without_an_access_port_refuses_an_unmapped_party() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let party = group_owner();
    let auth = McpEdgeAuth::headless().with_host(Arc::new(GroupMemberAuth { subject }));
    let app =
        Router::new()
            .route("/mcp", any(effective_owners))
            .layer(mcp_auth_layer_with_sessions(
                Arc::new(auth),
                McpSessionBindings::default(),
                RevalidationConfig::default(),
                None,
            ));

    let request = mcp_request("Bearer good-token")
        .header("X-Proxiima-Owner", owner_key(party))
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(app, request).await, StatusCode::UNAUTHORIZED);
}

// An eager-map host is unchanged: an owner the authenticated map
// already carries is answered from it, and the port is never asked.
#[tokio::test]
async fn an_owner_the_eager_map_carries_never_reaches_the_access_port() {
    let owner = user_owner();
    let access = PerOwnerAccess::new(UserId::new(uuid::Uuid::now_v7()), []);
    let calls = Arc::clone(&access.calls);
    let app = group_access_app(
        Arc::new(TokenAuth { owner }),
        Arc::new(access),
        McpSessionBindings::default(),
    );

    let request = mcp_request("Bearer good-token")
        .header("X-Proxiima-Owner", owner_key(owner))
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(app, request).await, StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// A host that serves one tenant: its authenticator names the tenant
/// Group as the default, as a native OAuth client sends no owner header.
struct TenantAuth {
    subject: UserId,
    tenant: OwnerId,
    name_default: bool,
}

#[async_trait]
impl Authenticator for TenantAuth {
    async fn authenticate(&self, creds: &Credentials) -> Result<AuthzContext, AuthError> {
        let Credentials::Bearer(token) = creds;
        if token != "good-token" {
            return Err(AuthError::InvalidCredentials);
        }
        let ctx = AuthzContext::for_subject_with_role(
            self.subject,
            [(self.tenant, Role::editor())],
            AuthPath::HostBearer,
        );
        if !self.name_default {
            return Ok(ctx);
        }
        ctx.with_default_owner(self.tenant)
            .ok_or(AuthError::InvalidCredentials)
    }
}

/// Stub that mints a session like rmcp does on `initialize` and reports
/// the owners effective for the request.
fn tenant_app(auth: TenantAuth, sessions: McpSessionBindings) -> Router {
    let edge = McpEdgeAuth::headless().with_host(Arc::new(auth));
    Router::new()
        .route(
            "/mcp",
            any(|request: Request<Body>| async move {
                let mut response = effective_owners(request).await;
                response
                    .headers_mut()
                    .insert("Mcp-Session-Id", "sess-new".parse().unwrap());
                response
            }),
        )
        .layer(mcp_auth_layer_with_sessions(
            Arc::new(edge),
            sessions,
            RevalidationConfig::default(),
            None,
        ))
}

// A native client sends neither header: the host-named default is
// selected and bound to the new session while effective roles remain.
#[tokio::test]
async fn a_request_that_selects_no_owner_acts_for_the_host_named_default() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let tenant = group_owner();
    let sessions = McpSessionBindings::default();
    let app = tenant_app(
        TenantAuth {
            subject,
            tenant,
            name_default: true,
        },
        sessions.clone(),
    );

    let initialize = mcp_request("Bearer good-token")
        .body(Body::empty())
        .unwrap();
    let expected = {
        let mut owners = [owner_key(tenant), owner_key(OwnerId::from(subject))];
        owners.sort();
        owners.join(",")
    };
    assert_eq!(
        effective_owners_response(app, initialize).await,
        (StatusCode::OK, Bytes::from(expected)),
        "the default owner does not erase the credential's personal role"
    );
    assert_eq!(
        sessions.owner_for("sess-new", subject.into()).await,
        Some(tenant)
    );
}

// An explicit header still wins over the default.
#[tokio::test]
async fn an_owner_header_overrides_the_default() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let personal = OwnerId::from(subject);
    let tenant = group_owner();
    let sessions = McpSessionBindings::default();
    let app = tenant_app(
        TenantAuth {
            subject,
            tenant,
            name_default: true,
        },
        sessions.clone(),
    );

    let request = mcp_request("Bearer good-token")
        .header("X-Proxiima-Owner", owner_key(personal))
        .body(Body::empty())
        .unwrap();
    let expected = {
        let mut owners = [owner_key(personal), owner_key(tenant)];
        owners.sort();
        owners.join(",")
    };
    assert_eq!(
        effective_owners_response(app, request).await,
        (StatusCode::OK, Bytes::from(expected))
    );
    assert_eq!(
        sessions.owner_for("sess-new", subject.into()).await,
        Some(personal)
    );
}

// No default, no selection: the 403 this edge always gave. A malformed
// header never falls back to the default.
#[tokio::test]
async fn without_a_default_or_with_a_bad_header_the_selection_is_refused() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let tenant = group_owner();
    let without = tenant_app(
        TenantAuth {
            subject,
            tenant,
            name_default: false,
        },
        McpSessionBindings::default(),
    );
    let request = mcp_request("Bearer good-token")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(without, request).await, StatusCode::FORBIDDEN);

    let with = tenant_app(
        TenantAuth {
            subject,
            tenant,
            name_default: true,
        },
        McpSessionBindings::default(),
    );
    let request = mcp_request("Bearer good-token")
        .header("X-Proxiima-Owner", "group:not-a-uuid")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(with, request).await, StatusCode::FORBIDDEN);
}

fn mcp_request(bearer: &str) -> axum::http::request::Builder {
    Request::builder()
        .uri("/mcp")
        .header("Origin", "http://localhost")
        .header("Authorization", bearer)
}

async fn status_of(app: Router, request: Request<Body>) -> StatusCode {
    app.oneshot(request).await.unwrap().status()
}

fn cors_app(calls: Arc<AtomicU64>) -> Router {
    Router::new()
        .route(
            "/ok",
            any(move || {
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        )
        .layer(cors_layer(default_allowlist()))
}

#[tokio::test]
async fn allowed_preflight_is_no_content_and_skips_the_route() {
    let calls = Arc::new(AtomicU64::new(0));
    let response = cors_app(Arc::clone(&calls))
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/ok")
                .header(header::ORIGIN, "http://localhost:5173")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "authorization,content-type,x-proxiima-owner",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&header::HeaderValue::from_static("http://localhost:5173"))
    );
    assert_eq!(
        response.headers().get(header::ACCESS_CONTROL_ALLOW_METHODS),
        Some(&header::HeaderValue::from_static("POST"))
    );
    assert_eq!(
        response.headers().get(header::ACCESS_CONTROL_ALLOW_HEADERS),
        Some(&header::HeaderValue::from_static(
            "authorization,content-type,x-proxiima-owner"
        ))
    );
    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
    );
    let vary: Vec<_> = response
        .headers()
        .get_all(header::VARY)
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    assert_eq!(
        vary,
        [
            "Origin",
            "Access-Control-Request-Method",
            "Access-Control-Request-Headers"
        ]
    );
}

#[tokio::test]
async fn repeated_request_header_lists_are_all_reflected() {
    let calls = Arc::new(AtomicU64::new(0));
    let mut request = Request::builder()
        .method(Method::OPTIONS)
        .uri("/ok")
        .header(header::ORIGIN, "http://localhost")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization")
        .body(Body::empty())
        .unwrap();
    request.headers_mut().append(
        header::ACCESS_CONTROL_REQUEST_HEADERS,
        header::HeaderValue::from_static("content-type, x-proxiima-owner"),
    );

    let response = cors_app(Arc::clone(&calls)).oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let allowed_headers: Vec<_> = response
        .headers()
        .get_all(header::ACCESS_CONTROL_ALLOW_HEADERS)
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    assert_eq!(
        allowed_headers,
        ["authorization", "content-type, x-proxiima-owner"]
    );
}

#[tokio::test]
async fn malformed_or_repeated_preflight_fields_fail_closed() {
    let calls = Arc::new(AtomicU64::new(0));
    let app = cors_app(Arc::clone(&calls));

    let mut repeated_method = Request::builder()
        .method(Method::OPTIONS)
        .uri("/ok")
        .header(header::ORIGIN, "http://localhost")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .body(Body::empty())
        .unwrap();
    repeated_method.headers_mut().append(
        header::ACCESS_CONTROL_REQUEST_METHOD,
        header::HeaderValue::from_static("DELETE"),
    );
    let malformed_method = Request::builder()
        .method(Method::OPTIONS)
        .uri("/ok")
        .header(header::ORIGIN, "http://localhost")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST, DELETE")
        .body(Body::empty())
        .unwrap();
    let malformed_headers = Request::builder()
        .method(Method::OPTIONS)
        .uri("/ok")
        .header(header::ORIGIN, "http://localhost")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(
            header::ACCESS_CONTROL_REQUEST_HEADERS,
            "authorization, not a header",
        )
        .body(Body::empty())
        .unwrap();

    for request in [repeated_method, malformed_method, malformed_headers] {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            !response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        );
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            Bytes::from_static(b"invalid CORS preflight")
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn allowed_actual_response_echoes_origin_and_exposes_browser_headers() {
    let calls = Arc::new(AtomicU64::new(0));
    let response = cors_app(Arc::clone(&calls))
        .oneshot(
            Request::builder()
                .uri("/ok")
                .header(header::ORIGIN, "http://127.0.0.1:5173")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&header::HeaderValue::from_static("http://127.0.0.1:5173"))
    );
    assert_eq!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_EXPOSE_HEADERS),
        Some(&header::HeaderValue::from_static(
            "Mcp-Session-Id, WWW-Authenticate"
        ))
    );
    assert_eq!(
        response.headers().get(header::VARY),
        Some(&header::HeaderValue::from_static("Origin"))
    );
}

#[tokio::test]
async fn browser_default_port_origin_passes_cors_and_preflight() {
    let app = Router::new()
        .route("/ok", any(|| async { StatusCode::OK }))
        .layer(cors_layer(OriginAllowlist::new([
            "https://example.com:443",
        ])));

    for (method, expected) in [
        (Method::GET, StatusCode::OK),
        (Method::OPTIONS, StatusCode::NO_CONTENT),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/ok")
                    .header(header::ORIGIN, "https://example.com")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), expected);
        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&header::HeaderValue::from_static("https://example.com"))
        );
        assert_eq!(
            response.headers().get(header::VARY),
            Some(&header::HeaderValue::from_static("Origin"))
        );
    }
}

#[tokio::test]
async fn cors_appends_vary_without_replacing_route_values() {
    let app = Router::new()
        .route(
            "/ok",
            any(|| async {
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::VARY, "Accept-Encoding")
                    .body(Body::empty())
                    .unwrap()
            }),
        )
        .layer(cors_layer(default_allowlist()));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/ok")
                .header(header::ORIGIN, "http://localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let vary: Vec<_> = response
        .headers()
        .get_all(header::VARY)
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    assert_eq!(vary, ["Accept-Encoding", "Origin"]);
}

#[tokio::test]
async fn disallowed_or_repeated_origin_is_rejected_before_the_route() {
    let calls = Arc::new(AtomicU64::new(0));
    let app = cors_app(Arc::clone(&calls));
    let disallowed = Request::builder()
        .uri("/ok")
        .header(header::ORIGIN, "https://evil.test")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(disallowed).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );

    let mut repeated = Request::builder()
        .uri("/ok")
        .header(header::ORIGIN, "http://localhost")
        .body(Body::empty())
        .unwrap();
    repeated.headers_mut().append(
        header::ORIGIN,
        header::HeaderValue::from_static("http://127.0.0.1"),
    );
    assert_eq!(
        app.oneshot(repeated).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn no_origin_and_non_preflight_options_keep_route_semantics() {
    let calls = Arc::new(AtomicU64::new(0));
    let app = cors_app(Arc::clone(&calls));
    let native = app
        .clone()
        .oneshot(Request::builder().uri("/ok").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(native.status(), StatusCode::OK);
    assert!(
        !native
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
    );
    assert_eq!(
        native.headers().get(header::VARY),
        Some(&header::HeaderValue::from_static("Origin"))
    );

    let options = app
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/ok")
                .header(header::ORIGIN, "http://localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(options.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        options.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&header::HeaderValue::from_static("http://localhost"))
    );
}

fn host_guard_app(additional_hosts: &[&str]) -> Router {
    Router::new()
        .route("/ok", any(|| async { StatusCode::OK }))
        .layer(host_guard_layer(
            HostAllowlist::parse(additional_hosts).unwrap(),
        ))
}

fn request_with_host(uri: &str, host: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(http::header::HOST, host)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn host_guard_default_is_loopback_only_and_port_agnostic() {
    let app = host_guard_app(&[]);
    for host in [
        "localhost",
        "LOCALHOST:31415",
        "127.0.0.1:31415",
        "[::1]",
        "[::1]:31415",
    ] {
        assert_eq!(
            status_of(app.clone(), request_with_host("/ok", host)).await,
            StatusCode::OK,
            "loopback Host {host:?} must be accepted"
        );
    }
    for host in ["localhost.evil", "evil.test"] {
        assert_eq!(
            status_of(app.clone(), request_with_host("/ok", host)).await,
            StatusCode::FORBIDDEN,
            "foreign Host {host:?} must be rejected"
        );
    }
}

#[tokio::test]
async fn host_guard_matches_bare_hosts_and_explicit_ports_like_rmcp() {
    let app = host_guard_app(&["example.test", "port.test:8443"]);
    for host in ["example.test", "example.test:443", "port.test:8443"] {
        assert_eq!(
            status_of(app.clone(), request_with_host("/ok", host)).await,
            StatusCode::OK,
            "allowed Host {host:?} must be accepted"
        );
    }
    for host in ["port.test", "port.test:443"] {
        assert_eq!(
            status_of(app.clone(), request_with_host("/ok", host)).await,
            StatusCode::FORBIDDEN,
            "wrong explicit port for {host:?} must be rejected"
        );
    }
}

#[tokio::test]
async fn host_guard_normalizes_ipv6_and_preserves_explicit_ports() {
    let app = host_guard_app(&["2001:db8::1", "[2001:db8::2]:8443"]);
    for host in ["[2001:DB8::1]", "[2001:db8::1]:443", "[2001:db8::2]:8443"] {
        assert_eq!(
            status_of(app.clone(), request_with_host("/ok", host)).await,
            StatusCode::OK,
            "allowed IPv6 authority {host:?} must be accepted"
        );
    }
    for host in ["[2001:db8::2]", "[2001:db8::2]:443"] {
        assert_eq!(
            status_of(app.clone(), request_with_host("/ok", host)).await,
            StatusCode::FORBIDDEN,
            "wrong IPv6 port for {host:?} must be rejected"
        );
    }
}

#[tokio::test]
async fn host_header_wins_and_uri_authority_is_the_http2_fallback() {
    let app = host_guard_app(&[]);
    assert_eq!(
        status_of(
            app.clone(),
            request_with_host("http://localhost/ok", "evil.test"),
        )
        .await,
        StatusCode::FORBIDDEN
    );
    let authority_only = Request::builder()
        .uri("http://LOCALHOST:31415/ok")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(app.clone(), authority_only).await, StatusCode::OK);
    let foreign_authority = Request::builder()
        .uri("http://evil.test/ok")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        status_of(app, foreign_authority).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn missing_or_malformed_host_is_bad_request_with_rmcp_text() {
    let app = host_guard_app(&[]);
    let missing = Request::builder().uri("/ok").body(Body::empty()).unwrap();
    let missing = app.clone().oneshot(missing).await.unwrap();
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        missing.headers().get(http::header::CONTENT_TYPE).unwrap(),
        "text/plain; charset=utf-8"
    );
    assert_eq!(
        to_bytes(missing.into_body(), usize::MAX).await.unwrap(),
        Bytes::from_static(b"Bad Request: missing Host header")
    );

    let malformed = request_with_host("http://localhost/ok", "bad host");
    let malformed = app.clone().oneshot(malformed).await.unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        to_bytes(malformed.into_body(), usize::MAX).await.unwrap(),
        Bytes::from_static(b"Bad Request: Invalid Host header")
    );

    let mut non_utf8 = Request::builder()
        .uri("http://localhost/ok")
        .body(Body::empty())
        .unwrap();
    non_utf8.headers_mut().insert(
        http::header::HOST,
        http::HeaderValue::from_bytes(&[0xff]).unwrap(),
    );
    let non_utf8 = app.oneshot(non_utf8).await.unwrap();
    assert_eq!(non_utf8.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        to_bytes(non_utf8.into_body(), usize::MAX).await.unwrap(),
        Bytes::from_static(b"Bad Request: Invalid Host header encoding")
    );
}

#[tokio::test]
async fn disallowed_host_uses_rmcp_forbidden_response_shape() {
    let response = host_guard_app(&[])
        .oneshot(request_with_host("/ok", "evil.test"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!response.headers().contains_key(http::header::CONTENT_TYPE));
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        Bytes::from_static(b"Forbidden: Host header is not allowed")
    );
}

// A bad token yields 401 even when a valid owner is selected.
#[tokio::test]
async fn bad_token_with_valid_owner_is_unauthorized() {
    let owner = user_owner();
    let app = auth_app(owner, McpSessionBindings::default());
    let request = mcp_request("Bearer bad-token")
        .header("X-Proxiima-Owner", owner_key(owner))
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(app, request).await, StatusCode::UNAUTHORIZED);
}

// A bad token with a present-but-unbound session id yields
// 401 (not 404 and not 403) — token validity is checked first.
#[tokio::test]
async fn bad_token_with_unbound_session_is_unauthorized() {
    let owner = user_owner();
    let app = auth_app(owner, McpSessionBindings::default());
    let request = mcp_request("Bearer bad-token")
        .header("Mcp-Session-Id", "never-bound")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(app, request).await, StatusCode::UNAUTHORIZED);
}

// A valid token with a present-but-unbound session id yields 404
// so a standard Streamable-HTTP client re-initializes.
#[tokio::test]
async fn valid_token_with_unbound_session_is_not_found() {
    let owner = user_owner();
    let app = auth_app(owner, McpSessionBindings::default());
    let request = mcp_request("Bearer good-token")
        .header("Mcp-Session-Id", "never-bound")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(app, request).await, StatusCode::NOT_FOUND);
}

// A valid token with no session id and no owner header yields 403:
// owner selection is required once the token is known-valid.
#[tokio::test]
async fn valid_token_without_owner_selection_is_forbidden() {
    let owner = user_owner();
    let app = auth_app(owner, McpSessionBindings::default());
    let request = mcp_request("Bearer good-token")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(app, request).await, StatusCode::FORBIDDEN);
}

// A valid token + valid owner header authorizes (no session yet).
#[tokio::test]
async fn valid_token_with_owner_header_authorizes() {
    let owner = user_owner();
    let app = auth_app(owner, McpSessionBindings::default());
    let request = mcp_request("Bearer good-token")
        .header("X-Proxiima-Owner", owner_key(owner))
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(app, request).await, StatusCode::OK);
}

// A valid token against an already-bound session authorizes without an
// owner header.
#[tokio::test]
async fn valid_token_with_bound_session_authorizes() {
    let owner = user_owner();
    let sessions = McpSessionBindings::default();
    sessions.bind("sess-1", owner, owner).await;
    let app = auth_app(owner, sessions);
    let request = mcp_request("Bearer good-token")
        .header("Mcp-Session-Id", "sess-1")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status_of(app, request).await, StatusCode::OK);
}

#[tokio::test]
async fn successful_requests_authenticate_bearer_exactly_once_each() {
    let owner = user_owner();
    let calls = Arc::new(AtomicU64::new(0));
    let app = auth_app_with_auth(
        Arc::new(CountingTokenAuth {
            owner,
            calls: calls.clone(),
        }),
        McpSessionBindings::default(),
    );

    for expected_calls in 1..=2 {
        let request = mcp_request("Bearer good-token")
            .header("X-Proxiima-Owner", owner_key(owner))
            .body(Body::empty())
            .unwrap();
        assert_eq!(status_of(app.clone(), request).await, StatusCode::OK);
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
    }
}

#[tokio::test]
async fn foreign_owner_session_matches_unknown_session_response() {
    let authorized_owner = user_owner();
    let foreign_owner = user_owner();
    let sessions = McpSessionBindings::default();
    sessions
        .bind("foreign-session", foreign_owner, foreign_owner)
        .await;
    let app = auth_app(authorized_owner, sessions);

    let foreign_request = mcp_request("Bearer good-token")
        .header("Mcp-Session-Id", "foreign-session")
        .body(Body::empty())
        .unwrap();
    let foreign_response = app.clone().oneshot(foreign_request).await.unwrap();
    let unknown_request = mcp_request("Bearer good-token")
        .header("Mcp-Session-Id", "never-bound")
        .body(Body::empty())
        .unwrap();
    let unknown_response = app.oneshot(unknown_request).await.unwrap();

    assert_eq!(foreign_response.status(), StatusCode::NOT_FOUND);
    assert_eq!(foreign_response.status(), unknown_response.status());
    assert_eq!(foreign_response.headers(), unknown_response.headers());
    let foreign_body = to_bytes(foreign_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let unknown_body = to_bytes(unknown_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(foreign_body, unknown_body);
    assert_eq!(
        foreign_body,
        Bytes::from_static(b"unknown or expired MCP session")
    );
}

#[tokio::test]
async fn foreign_owner_header_remains_unauthorized() {
    let authorized_owner = user_owner();
    let foreign_owner = user_owner();
    let app = auth_app(authorized_owner, McpSessionBindings::default());
    let request = mcp_request("Bearer good-token")
        .header("X-Proxiima-Owner", owner_key(foreign_owner))
        .body(Body::empty())
        .unwrap();

    assert_eq!(status_of(app, request).await, StatusCode::UNAUTHORIZED);
}

#[test]
fn origins_render_explicit_port_rules_for_rmcp() {
    let allowlist = OriginAllowlist::parse([
        "http://LOCALHOST",
        "https://example.com:443",
        "https://example.com:8443",
        "http://[::1]",
        "https://[::1]:443",
    ])
    .unwrap();

    assert_eq!(
        allowlist.origins(),
        vec![
            "http://localhost:*".to_string(),
            "https://example.com:443".to_string(),
            "https://example.com:8443".to_string(),
            "http://[::1]:*".to_string(),
            "https://[::1]:443".to_string(),
        ]
    );
}

#[test]
fn explicit_default_ports_match_portless_browser_origins() {
    for (pattern, origin) in [
        ("https://example.com:443", "https://example.com"),
        ("http://example.com:80", "http://example.com"),
        ("https://[::1]:443", "https://[::1]"),
        ("http://[::1]:80", "http://[::1]"),
        ("HTTPS://EXAMPLE.COM:443", "https://example.com"),
    ] {
        let allowlist = OriginAllowlist::parse([pattern]).unwrap();
        assert!(
            allowlist.allows_origin(&header::HeaderValue::from_static(origin)),
            "{pattern} should allow {origin}"
        );
    }
}

#[test]
fn explicit_ports_match_only_the_request_port() {
    for (pattern, origin, allowed) in [
        ("https://example.com:8443", "https://example.com", false),
        ("http://example.com:8080", "http://example.com", false),
        ("https://example.com:443", "https://example.com:443", true),
        ("https://example.com:443", "https://example.com:8443", false),
        ("https://example.com:8443", "https://example.com:8443", true),
        ("https://example.com:8443", "https://example.com:443", false),
        ("http://example.com:80", "http://example.com:80", true),
        ("http://example.com:80", "http://example.com:8080", false),
        ("https://[::1]:443", "https://[::1]:8443", false),
        ("custom://example.com:443", "custom://example.com", false),
        ("custom://example.com:443", "custom://example.com:443", true),
    ] {
        let allowlist = OriginAllowlist::parse([pattern]).unwrap();
        assert_eq!(
            allowlist.allows_origin(&header::HeaderValue::from_static(origin)),
            allowed,
            "{pattern} vs {origin}"
        );
    }
}

#[test]
fn portless_patterns_allow_any_request_port() {
    for pattern in [
        "http://example.com",
        "https://example.com",
        "https://[::1]",
        "custom://example.com",
    ] {
        let allowlist = OriginAllowlist::parse([pattern]).unwrap();
        for suffix in ["", ":80", ":443", ":8443"] {
            let origin = format!("{pattern}{suffix}");
            assert!(
                allowlist.allows_origin(&header::HeaderValue::from_str(&origin).unwrap()),
                "{pattern} should allow {origin}"
            );
        }
    }
}

#[test]
fn parse_rejects_star() {
    for pattern in ["*", "https://example.com:*", "https://[::1]:*"] {
        assert_invalid_origin(pattern);
    }
}

#[test]
fn parse_rejects_garbage() {
    assert_invalid_origin("not an origin");
}

#[test]
fn parse_rejects_empty_entry() {
    assert_invalid_origin("  ");
}

#[test]
fn parse_rejects_an_origin_with_anything_after_the_authority() {
    for pattern in [
        "http://localhost/",
        "https://app.example.com/path",
        "https://app.example.com?x=1",
        "https://app.example.com#top",
        "https://user@app.example.com",
    ] {
        assert_invalid_origin(pattern);
    }
}

#[test]
fn an_origin_header_with_a_path_matches_no_pattern() {
    let allowlist = OriginAllowlist::parse(["http://localhost"]).unwrap();
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::ORIGIN,
        http::HeaderValue::from_static("http://localhost/"),
    );

    assert!(!allowlist.allows(&headers));
}

#[test]
fn host_parse_refuses_an_entry_that_could_never_match() {
    for entry in [
        "https://app.example.com",
        "app.example.com/path",
        "host:abc",
        "user@app.example.com",
        "app example.com",
    ] {
        let err = HostAllowlist::parse(["ok.example.com", entry]).unwrap_err();
        let McpServerError::InvalidHost(message) = err else {
            panic!("expected invalid host for {entry:?}");
        };
        assert!(message.contains(entry), "{message}");
    }
}

#[test]
fn host_parse_accepts_hosts_ports_ip_literals_and_skips_blanks() {
    let allowlist = HostAllowlist::parse([
        " App.Example.com ",
        "",
        "  ",
        "api.example.com:8443",
        "[2001:db8::1]:443",
        "2001:db8::2",
        "10.0.0.7",
    ])
    .unwrap();

    for allowed in [
        "app.example.com",
        "api.example.com:8443",
        "[2001:db8::1]:443",
        "[2001:db8::2]",
        "10.0.0.7:80",
    ] {
        assert!(allowlist.allows(allowed), "{allowed}");
    }
    assert!(!allowlist.allows("api.example.com:9"));
}

#[test]
fn parse_empty_list_allows_nothing() {
    let allowlist = OriginAllowlist::parse(std::iter::empty::<&str>()).unwrap();

    assert!(allowlist.origins().is_empty());
}

#[test]
fn strip_bearer_scheme_is_case_insensitive_per_rfc() {
    use super::strip_bearer_scheme;
    // RFC 9110 §11.1: the scheme token is case-insensitive.
    assert_eq!(strip_bearer_scheme("Bearer tok"), Some("tok"));
    assert_eq!(strip_bearer_scheme("bearer tok"), Some("tok"));
    assert_eq!(strip_bearer_scheme("BEARER tok"), Some("tok"));
    assert_eq!(strip_bearer_scheme("BeArEr tok"), Some("tok"));
    // Extra space between scheme and credential is tolerated.
    assert_eq!(strip_bearer_scheme("Bearer   tok"), Some("tok"));
    // Non-bearer schemes and empty credentials fail closed.
    assert_eq!(strip_bearer_scheme("Basic tok"), None);
    assert_eq!(strip_bearer_scheme("tok"), None);
    assert_eq!(strip_bearer_scheme("Bearer "), None);
    assert_eq!(strip_bearer_scheme("Bearer    "), None);
}

fn assert_invalid_origin(pattern: &str) {
    let err = OriginAllowlist::parse([pattern]).unwrap_err();
    let McpServerError::InvalidOrigin(message) = err else {
        panic!("expected invalid origin");
    };
    assert!(message.contains(pattern));
}

fn receiver_body(receiver: mpsc::UnboundedReceiver<Result<Bytes, Infallible>>) -> Body {
    Body::from_stream(stream::unfold(receiver, |mut receiver| async {
        receiver.recv().await.map(|item| (item, receiver))
    }))
}

fn collect_revalidated(
    body: Body,
    expires_at: Option<SystemTime>,
    config: RevalidationConfig,
) -> tokio::task::JoinHandle<Bytes> {
    let body = Body::new(RevalidatedBody::new(body, expires_at, config));
    tokio::spawn(async move {
        to_bytes(body, usize::MAX)
            .await
            .expect("body collection succeeds")
    })
}

#[tokio::test(start_paused = true)]
async fn revalidated_body_expiry_ends_active_body() {
    let config = RevalidationConfig {
        max_stream_lifetime: Duration::from_mins(1),
    };
    let (sender, receiver) = mpsc::unbounded_channel();
    sender
        .send(Ok(Bytes::from_static(b"before-expiry")))
        .expect("receiver is alive");
    let task = collect_revalidated(
        receiver_body(receiver),
        Some(SystemTime::now() + Duration::from_secs(2)),
        config,
    );

    tokio::task::yield_now().await;
    time::advance(Duration::from_secs(2)).await;

    assert_eq!(task.await.unwrap(), Bytes::from_static(b"before-expiry"));
}

#[tokio::test(start_paused = true)]
async fn revalidated_body_max_lifetime_ends_active_body() {
    let config = RevalidationConfig {
        max_stream_lifetime: Duration::from_secs(3),
    };
    let (_sender, receiver) = mpsc::unbounded_channel();
    let task = collect_revalidated(receiver_body(receiver), None, config);

    tokio::task::yield_now().await;
    time::advance(Duration::from_secs(3)).await;

    assert!(task.await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn revalidated_body_leaves_short_response_unchanged() {
    let config = RevalidationConfig {
        max_stream_lifetime: Duration::from_mins(1),
    };
    let bytes = to_bytes(
        Body::new(RevalidatedBody::new(Body::from("short"), None, config)),
        usize::MAX,
    )
    .await
    .unwrap();

    assert_eq!(bytes, Bytes::from_static(b"short"));
}
