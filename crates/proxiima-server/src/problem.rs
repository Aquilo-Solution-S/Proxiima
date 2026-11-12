//! RFC 9457 `application/problem+json`: the one rendering of every refusal
//! the listener answers itself.
//!
//! The Host, Origin and body-size guards, the bearer layer and the `/v1`
//! routes all answer through [`Problem`], so one refusal reads the same on
//! `/mcp`, on `/v1` and on a host-mounted route. `detail` says what the
//! caller can act on; it never echoes a credential, a header value or an
//! internal error text.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::security::PROXIIMA_OWNER_HEADER;

/// Base of every `type` URI. A stable, dereferenceable namespace so a client
/// can switch on `type` instead of on the human `title`.
const TYPE_BASE: &str = "https://proxiima.dev/errors/";

/// Every answer the listener renders is owner-, token- or request-scoped, so
/// none is shared-cacheable — including `QUERY` reads, whose cacheability is
/// deliberately unused. What `QUERY` buys here is retry safety and correct
/// modelling, not caching.
pub(crate) const NO_STORE: &str = "private, no-store";

/// One RFC 9457 problem document plus the response metadata that travels
/// with it.
///
/// Only this crate builds one, through a named refusal, so a document's
/// status and `type` always belong together; a host renders it with
/// [`IntoResponse`].
///
/// ```compile_fail
/// let _ = proxiima_server::Problem::new(axum::http::StatusCode::OK, "ok", "Ok", "", "/");
/// ```
#[derive(Debug, Clone)]
pub struct Problem {
    status: StatusCode,
    /// Slug appended to [`TYPE_BASE`]; the machine-readable discriminator.
    slug: &'static str,
    title: &'static str,
    detail: String,
    instance: String,
    /// `Allow` for a `405`. Emitted by hand because the generated routes use
    /// `any()`, which calls `skip_allow_header()` and suppresses axum's own.
    allow: Option<&'static str>,
    /// RFC 9457 extension: existing handle for an idempotency conflict.
    existing: Option<uuid::Uuid>,
}

impl Problem {
    pub(crate) fn new(
        status: StatusCode,
        slug: &'static str,
        title: &'static str,
        detail: impl Into<String>,
        instance: &str,
    ) -> Self {
        Self {
            status,
            slug,
            title,
            detail: detail.into(),
            instance: instance.to_string(),
            allow: None,
            existing: None,
        }
    }

    #[cfg(feature = "rest")]
    pub(crate) fn with_allow(mut self, allow: &'static str) -> Self {
        self.allow = Some(allow);
        self
    }

    #[cfg(feature = "rest")]
    pub(crate) fn with_existing(mut self, existing: uuid::Uuid) -> Self {
        self.existing = Some(existing);
        self
    }

    /// No accepted bearer credential: missing, unknown, or not bindable to
    /// the selected owner. The bearer layer adds its `WWW-Authenticate`
    /// challenge to this answer.
    pub(crate) fn auth_required(instance: &str) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "auth-required",
            "Authentication required",
            "send a valid bearer token in the Authorization header",
            instance,
        )
    }

    #[cfg(feature = "rest")]
    pub(crate) fn invalid_input(detail: impl Into<String>, instance: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid-input",
            "Invalid input",
            detail,
            instance,
        )
    }

    #[cfg(feature = "rest")]
    pub(crate) fn not_found(detail: impl Into<String>, instance: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not-found",
            "Not found",
            detail,
            instance,
        )
    }

    /// A `Host` (or HTTP/2 `:authority`) that is missing, malformed, or not
    /// UTF-8. `detail` names which, never the value.
    pub(crate) fn invalid_host(detail: &'static str, instance: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid-host",
            "Invalid Host header",
            detail,
            instance,
        )
    }

    pub(crate) fn host_not_allowed(instance: &str) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "host-not-allowed",
            "Host not allowed",
            "this listener does not serve the requested Host; use a host name it is configured for",
            instance,
        )
    }

    pub(crate) fn origin_not_allowed(instance: &str) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "origin-not-allowed",
            "Origin not allowed",
            "the request's Origin is not on this listener's allowlist; send at most one allowed Origin",
            instance,
        )
    }

    pub(crate) fn invalid_cors_preflight(instance: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid-cors-preflight",
            "Invalid CORS preflight",
            "Access-Control-Request-Method must name one method and \
             Access-Control-Request-Headers must list header names",
            instance,
        )
    }

    pub(crate) fn owner_selection_refused(instance: &str) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "owner-selection-refused",
            "Owner selection required or invalid",
            format!(
                "name one owner in the {PROXIIMA_OWNER_HEADER} header; it must agree with the \
                 session and with the credential's forwarded owner"
            ),
            instance,
        )
    }

    /// A present `Mcp-Session-Id` with no live binding for this caller.
    /// Streamable-HTTP clients re-initialize on the `404` status.
    pub(crate) fn session_not_found(instance: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "session-not-found",
            "Unknown or expired MCP session",
            "the Mcp-Session-Id is unknown or expired; start a new session with initialize",
            instance,
        )
    }

    pub(crate) fn session_limit_reached(instance: &str) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "session-limit-reached",
            "Session limit reached",
            "the listener holds its maximum number of MCP sessions; retry later",
            instance,
        )
    }

    /// One document whether the body declared its length or was cut off
    /// while it streamed past the cap.
    pub(crate) fn body_too_large(instance: &str) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body-too-large",
            "Request body too large",
            "the request body exceeds this listener's size limit",
            instance,
        )
    }

    /// A route that does not serve `method`. The caller adds `Allow`: the
    /// route knows which methods it serves.
    #[cfg(feature = "rest")]
    pub(crate) fn method_not_allowed(method: &axum::http::Method, instance: &str) -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method-not-allowed",
            "Method not allowed",
            format!(
                "{method} is not allowed here; the Allow header names the methods this route serves"
            ),
            instance,
        )
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "type": format!("{TYPE_BASE}{}", self.slug),
            "title": self.title,
            "status": self.status.as_u16(),
            "detail": self.detail,
            "instance": self.instance,
        });
        if let Some(existing) = self.existing {
            value["existing"] = serde_json::json!(existing.to_string());
        }
        value
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let body = self.to_json().to_string();
        let mut response = (self.status, body).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
        if let Some(allow) = self.allow {
            headers.insert(header::ALLOW, HeaderValue::from_static(allow));
        }
        response
    }
}
