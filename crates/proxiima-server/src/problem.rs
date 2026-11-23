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
const TYPE_BASE: &str = proxiima_core::ledger!(Proxiima.ToolSurface.problemTypePrefix);

/// Every answer the listener renders is owner-, token- or request-scoped, so
/// none is shared-cacheable — including `QUERY` reads, whose cacheability is
/// deliberately unused. What `QUERY` buys here is retry safety and correct
/// modelling, not caching.
pub(crate) const NO_STORE: &str = proxiima_core::ledger!(Proxiima.ToolSurface.privateCacheControl);

// The status vocabulary used to construct a problem also supplies OpenAPI.
#[derive(Clone, Copy)]
pub(crate) struct ProblemStatus(StatusCode);

macro_rules! problem_statuses {
    ($( $(#[$availability:meta])* $name:ident => $description:literal, )*) => {
        impl ProblemStatus {
            $(
                $(#[$availability])*
                pub(crate) const $name: Self = Self(StatusCode::$name);
            )*

            #[cfg(feature = "rest")]
            pub(crate) const DOCUMENTED: &'static [(StatusCode, &'static str)] =
                &[$((Self::$name.0, $description)),*];
        }
    };
}

problem_statuses! {
    BAD_REQUEST => "Invalid input, a reserved call-context argument in the request body, \
         an unknown schema, a body `action` conflicting with the route, or a \
         missing or malformed Host header.",
    UNAUTHORIZED => "Bearer token missing or invalid.",
    FORBIDDEN => "Not authorized for this tool or action, or a Host, Origin, or owner \
         selection the listener refuses.",
    NOT_FOUND => "No such tool, action, resource, or memory, or an unknown or expired \
         `Mcp-Session-Id`.",
    #[cfg(feature = "rest")]
    CONFLICT => "Idempotency conflict, duplicate ingest, trigger conflict, or a \
         suppressed subject.",
    #[cfg(feature = "rest")]
    UNPROCESSABLE_ENTITY => "The write would violate F/A/P layering, or the tool ran and failed \
         (`tool-failure`, the tool's text as detail).",
    PAYLOAD_TOO_LARGE => "Request body larger than the listener's limit.",
    #[cfg(feature = "rest")]
    INTERNAL_SERVER_ERROR => "Internal error. The detail is deliberately generic.",
    SERVICE_UNAVAILABLE => "A caller-actionable precondition is unavailable; retry is meaningful.",
    #[cfg(feature = "rest")]
    METHOD_NOT_ALLOWED => "This method is unsupported on the route or for the selected action. The Allow header lists the supported methods.",
}

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
        status: ProblemStatus,
        slug: &'static str,
        title: &'static str,
        detail: impl Into<String>,
        instance: &str,
    ) -> Self {
        Self {
            status: status.0,
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
        // `Proxiima.Transport.credentialFailureStatus`.
        Self::new(
            ProblemStatus::UNAUTHORIZED,
            "auth-required",
            "Authentication required",
            "send a valid bearer token in the Authorization header",
            instance,
        )
    }

    #[cfg(feature = "rest")]
    pub(crate) fn invalid_input(detail: impl Into<String>, instance: &str) -> Self {
        Self::new(
            ProblemStatus::BAD_REQUEST,
            "invalid-input",
            "Invalid input",
            detail,
            instance,
        )
    }

    #[cfg(feature = "rest")]
    pub(crate) fn not_found(detail: impl Into<String>, instance: &str) -> Self {
        Self::new(
            ProblemStatus::NOT_FOUND,
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
            ProblemStatus::BAD_REQUEST,
            "invalid-host",
            "Invalid Host header",
            detail,
            instance,
        )
    }

    pub(crate) fn host_not_allowed(instance: &str) -> Self {
        Self::new(
            ProblemStatus::FORBIDDEN,
            "host-not-allowed",
            "Host not allowed",
            "this listener does not serve the requested Host; use a host name it is configured for",
            instance,
        )
    }

    pub(crate) fn origin_not_allowed(instance: &str) -> Self {
        Self::new(
            ProblemStatus::FORBIDDEN,
            "origin-not-allowed",
            "Origin not allowed",
            "the request's Origin is not on this listener's allowlist; send at most one allowed Origin",
            instance,
        )
    }

    pub(crate) fn invalid_cors_preflight(instance: &str) -> Self {
        Self::new(
            ProblemStatus::BAD_REQUEST,
            "invalid-cors-preflight",
            "Invalid CORS preflight",
            "Access-Control-Request-Method must name one method and \
             Access-Control-Request-Headers must list header names",
            instance,
        )
    }

    pub(crate) fn owner_selection_refused(instance: &str) -> Self {
        // `Proxiima.Transport.credentialFailureStatus`.
        Self::new(
            ProblemStatus::FORBIDDEN,
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
        // `Proxiima.Transport.credentialFailureStatus`.
        Self::new(
            ProblemStatus::NOT_FOUND,
            "session-not-found",
            "Unknown or expired MCP session",
            "the Mcp-Session-Id is unknown or expired; start a new session with initialize",
            instance,
        )
    }

    pub(crate) fn session_limit_reached(instance: &str) -> Self {
        Self::new(
            ProblemStatus::SERVICE_UNAVAILABLE,
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
            ProblemStatus::PAYLOAD_TOO_LARGE,
            "body-too-large",
            "Request body too large",
            "the request body exceeds this listener's size limit",
            instance,
        )
    }

    /// A route that does not serve `method`. The caller adds `Allow`: the
    /// route knows which methods it serves.
    #[cfg(feature = "rest")]
    pub(crate) fn method_not_allowed(
        method: &axum::http::Method,
        instance: &str,
        action: Option<&str>,
    ) -> Self {
        let target = action.map_or_else(
            || "this route".to_owned(),
            |action| format!("the selected action `{action}`"),
        );
        Self::method_not_allowed_detail(
            format!(
                "{method} is unsupported for {target}. The Allow header lists the supported methods."
            ),
            instance,
        )
    }

    #[cfg(feature = "rest")]
    pub(crate) fn query_action_required(instance: &str) -> Self {
        Self::method_not_allowed_detail(
            "QUERY requires selecting an available read-only action. The Allow header lists the supported methods.",
            instance,
        )
    }

    #[cfg(feature = "rest")]
    fn method_not_allowed_detail(detail: impl Into<String>, instance: &str) -> Self {
        Self::new(
            ProblemStatus::METHOD_NOT_ALLOWED,
            "method-not-allowed",
            "Method not allowed",
            detail,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_refusals_match_ledger_vectors() {
        let data: serde_json::Value =
            serde_json::from_str(include_str!("../../core/tests/ledger_vectors.json")).unwrap();
        let rows = data["Proxiima.Transport.credentialFailureStatus"]
            .as_array()
            .unwrap();
        for row in rows {
            let problem = match row["in"][0].as_str().unwrap() {
                "invalidBearer" => Problem::auth_required("/mcp"),
                "unavailableSession" => Problem::session_not_found("/mcp"),
                "ownerSelectionRefused" => Problem::owner_selection_refused("/mcp"),
                choice => panic!("unknown credential failure: {choice}"),
            };
            assert_eq!(
                serde_json::json!(problem.status.as_u16()),
                row["out"],
                "{row}"
            );
        }
    }
}
