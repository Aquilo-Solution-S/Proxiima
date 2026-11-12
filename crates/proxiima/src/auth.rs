//! Wiring an OIDC-authenticated MCP host from the environment.
//!
//! WHY THIS IS IN THE FACADE and not left to each host. Serving MCP requires
//! an `Authenticator` — `allow_insecure_single_owner` is refused for MCP on
//! purpose, so there is no development shortcut that reaches the transport.
//! The only authenticator that ships lives in `proxiima-auth-oidc`, and an
//! out-of-tree spiice takes exactly ONE dependency on this repository (the
//! facade) so its lockfile cannot drift between two revisions of the same
//! tree. The env parse therefore lives here: `RuntimeBuilder::authenticator`
//! would otherwise take an argument no out-of-tree caller can construct.
//!
//! IT IS THE ENV CONTRACT, NOT A CONVENIENCE. Single implementation of
//! `PROXIIMA_OIDC_*`; `apps/proxiima-mcp` delegates here rather than keeping
//! a second copy that could answer differently for the same variables.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use proxiima_core::{Authenticator, Setting};
use proxiima_server::{AuthorizationServers, ResourceServerMetadata};

use crate::OwnerAccessPort;
use crate::config::parse_bool_value;
use crate::runtime_config::ProxiimaError;
use crate::settings::{
    PROXIIMA_OIDC_ALLOWED_SUBJECTS as ALLOWED_SUBJECTS, PROXIIMA_OIDC_AUDIENCE,
    PROXIIMA_OIDC_HTTP_TIMEOUT_SECONDS as OIDC_HTTP_TIMEOUT_SECONDS, PROXIIMA_OIDC_ISSUER,
    PROXIIMA_OIDC_JWKS_JSON as JWKS_JSON, PROXIIMA_OIDC_JWKS_URI as JWKS_URI,
    PROXIIMA_OIDC_REQUIRE_ACCESS_TOKEN_TYP as OIDC_REQUIRE_ACCESS_TOKEN_TYP,
    PROXIIMA_OIDC_SUBJECT_MAP_JSON as SUBJECT_MAP_JSON, PROXIIMA_PUBLIC_URL,
};

/// How much clock skew a token may carry, in seconds.
const LEEWAY_SECS: u64 = 60;

/// Every variable [`oidc_from_lookup`] reads. The runtime environment layer
/// captures exactly these, so the authenticator it builds at resolve sees
/// the environment the builder was given.
pub(crate) const OIDC_ENV_KEYS: [Setting; 9] = [
    PROXIIMA_OIDC_ISSUER,
    PROXIIMA_OIDC_AUDIENCE,
    PROXIIMA_PUBLIC_URL,
    JWKS_URI,
    JWKS_JSON,
    ALLOWED_SUBJECTS,
    OIDC_HTTP_TIMEOUT_SECONDS,
    SUBJECT_MAP_JSON,
    OIDC_REQUIRE_ACCESS_TOKEN_TYP,
];

/// What a host needs to serve authenticated MCP: who validates a bearer
/// token, and what to advertise at the protected-resource metadata endpoint.
pub type OidcBundle = (Arc<dyn Authenticator>, ResourceServerMetadata);

/// Low-level OIDC primitives a host authenticator composes, and the two
/// stock authenticators (`OidcAuthenticator` one route, `OidcBindingSet`
/// several) with their constructor inputs. The runtime builder reads the
/// one-audience `PROXIIMA_OIDC_*` env path through [`oidc_from_lookup`], which
/// a host with its own owner resolution calls for the same authenticator.
///
/// A host reading its own claims uses [`OidcTokenValidator::validate_with`]
/// (typed [`OidcRejection`] reasons) or an [`OidcRoleShape::Host`]
/// [`OidcRoleShaper`] on a binding; neither needs a JWT library of its own.
/// A host that pins which clients (`azp`) may call a binding attaches an
/// [`AuthorizedPartyPolicy`] to it.
pub use proxiima_auth_oidc::{
    AuthorizedPartyPolicy, AuthorizedPartyPolicyError, DEFAULT_HTTP_REQUEST_TIMEOUT,
    HttpJwksResolver, KeyError, KeyResolver, MAX_HTTP_REQUEST_TIMEOUT, OidcAuthConfig,
    OidcAuthenticator, OidcBinding, OidcBindingRoute, OidcBindingSet, OidcBindingSetError,
    OidcClaimMap, OidcConfigError, OidcRejection, OidcRoleShape, OidcRoleShaper, OidcSubjectMap,
    OidcSubjectMapError, OidcTokenValidator, StaticJwksResolver, SubjectBinding,
    ValidatedOidcClaims, ValidatedOidcToken,
};
pub use proxiima_core::{AccessError, OwnerRoles};

/// Build the OIDC authenticator and resource metadata from an environment
/// lookup (the runtime builder's captured environment, or a test's map).
///
/// Returns `Ok(None)` when `PROXIIMA_OIDC_ISSUER` is unset — a host with no
/// issuer is not misconfigured, it simply is not serving authenticated MCP,
/// and an embedded host using the Host API needs no issuer at all. Once an
/// issuer IS set, every companion variable becomes required and its absence
/// is an error rather than a silent downgrade to something less
/// authenticated than the operator asked for.
///
/// Variables:
/// - `PROXIIMA_OIDC_ISSUER` — the issuer URL; presence switches this on.
/// - `PROXIIMA_OIDC_AUDIENCE` — the audience every token must carry. It
///   names this resource server, never an OAuth client id: an issuer puts a
///   client id into the `aud` of the ID tokens it mints for that client.
/// - `PROXIIMA_PUBLIC_URL` — this server's own URL, advertised to clients.
/// - `PROXIIMA_OIDC_SUBJECT_MAP_JSON` — which subject maps to which owner.
/// - `PROXIIMA_OIDC_JWKS_URI` — optional override; discovered otherwise.
/// - `PROXIIMA_OIDC_JWKS_JSON` — optional JWKS document pinned in config. When
///   set nothing is fetched: the issuer is only matched against `iss`, for a
///   host with no network path to it. Mutually exclusive with `JWKS_URI`.
/// - `PROXIIMA_OIDC_HTTP_TIMEOUT_SECONDS` — optional complete-request timeout;
///   default 10 seconds, maximum 300 seconds.
/// - `PROXIIMA_OIDC_ALLOWED_SUBJECTS` — optional comma-separated allowlist.
/// - `PROXIIMA_OIDC_REQUIRE_ACCESS_TOKEN_TYP` — optional boolean; when
///   `true`, require JWT `typ` to be `at+jwt` or `application/at+jwt`. Set
///   it whenever the issuer sends that type (RFC 9068). Otherwise a token
///   with no `typ` or `JWT` also passes; any other `typ` is always refused.
///
/// # Errors
///
/// Returns [`ProxiimaError::Config`] when an issuer is set without its
/// companions, when both JWKS sources are set,
/// when the subject map or pinned JWKS will not parse, or when the issuer or
/// JWKS URI is not a secure URL.
pub fn oidc_from_lookup(
    lookup: &impl Fn(&str) -> Option<String>,
    owner_access: Arc<dyn OwnerAccessPort>,
) -> Result<Option<OidcBundle>, ProxiimaError> {
    let Some(issuer) = PROXIIMA_OIDC_ISSUER.read(lookup) else {
        return Ok(None);
    };
    let audience = PROXIIMA_OIDC_AUDIENCE.read(lookup).ok_or_else(|| {
        ProxiimaError::Config("PROXIIMA_OIDC_ISSUER set without PROXIIMA_OIDC_AUDIENCE".into())
    })?;
    let public_url = public_url(&PROXIIMA_PUBLIC_URL.read(lookup).ok_or_else(|| {
        ProxiimaError::Config("PROXIIMA_OIDC_ISSUER set without PROXIIMA_PUBLIC_URL".into())
    })?)?;
    let jwks_uri = JWKS_URI.read(lookup);
    let jwks_json = JWKS_JSON.read(lookup);
    // Both is an error rather than a precedence rule: one says fetch keys
    // from the issuer, the other says never contact it.
    if jwks_uri.is_some() && jwks_json.is_some() {
        return Err(ProxiimaError::Config(format!(
            "{JWKS_JSON} and {JWKS_URI} are mutually exclusive"
        )));
    }
    let allowed_subjects = allowed_subjects(lookup)?;

    let config = proxiima_auth_oidc::OidcAuthConfig {
        issuer: issuer.clone(),
        jwks_uri,
        audience,
        allowed_subjects,
        leeway_secs: LEEWAY_SECS,
    };
    // Keep the URL security boundary ahead of non-security companion parsing.
    // `with_request_timeout` validates it again so its standalone low-level
    // contract does not depend on this facade.
    config
        .validate()
        .map_err(|err| ProxiimaError::Config(err.to_string()))?;
    let request_timeout = oidc_http_timeout(lookup)?;
    let require_access_token_typ = oidc_require_access_token_typ(lookup)?;
    // The issuer/JWKS URL boundary is validated BEFORE the subject map and
    // before storage, so an insecure-URL rejection short-circuits rather
    // than being reported after two other things have already been parsed.
    let resolver: Arc<dyn KeyResolver> = match jwks_json {
        Some(json) => Arc::new(
            StaticJwksResolver::from_jwks_json(&json)
                .map_err(|err| ProxiimaError::Config(format!("{JWKS_JSON}: {err}")))?,
        ),
        None => Arc::new(
            HttpJwksResolver::with_request_timeout(
                issuer.clone(),
                config.jwks_uri.clone(),
                request_timeout,
            )
            .map_err(|err| ProxiimaError::Config(err.to_string()))?,
        ),
    };
    let subject_map = subject_map(lookup)?;

    let authenticator =
        proxiima_auth_oidc::OidcAuthenticator::new(config, resolver, subject_map, owner_access)
            .map_err(|err| ProxiimaError::Config(err.to_string()))?;
    let authenticator = if require_access_token_typ {
        authenticator.require_access_token_typ()
    } else {
        authenticator
    };
    Ok(Some((
        Arc::new(authenticator),
        ResourceServerMetadata {
            public_url,
            authorization_servers: AuthorizationServers::Issuers(vec![issuer]),
            additional_scopes: Vec::new(),
        },
    )))
}

/// This server's own base URL, as clients are told it: `http` or `https`,
/// with a host, and no credentials, query or fragment. The value is not
/// echoed: a URL could carry a password.
fn public_url(raw: &str) -> Result<String, ProxiimaError> {
    let refused = |reason: &str| {
        ProxiimaError::Config(format!(
            "PROXIIMA_PUBLIC_URL must be an http(s) URL with a host and no credentials, \
             query or fragment: {reason}"
        ))
    };
    let url = reqwest::Url::parse(raw).map_err(|_| refused("it does not parse"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(refused("the scheme is not http or https"));
    }
    if url.host_str().is_none() {
        return Err(refused("it has no host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(refused("it carries credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(refused("it carries a query or fragment"));
    }
    Ok(raw.to_owned())
}

/// `PROXIIMA_OIDC_ALLOWED_SUBJECTS`: comma-separated subjects, absent when unset. A value that
/// names none (`,`) is refused, since an empty allowlist would refuse every token without a word.
fn allowed_subjects(
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<Option<HashSet<String>>, ProxiimaError> {
    let Some(raw) = ALLOWED_SUBJECTS.read(lookup) else {
        return Ok(None);
    };
    let subjects = raw
        .split(',')
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
        .map(ToOwned::to_owned)
        .collect::<HashSet<String>>();
    if subjects.is_empty() {
        return Err(ProxiimaError::Config(format!(
            "{ALLOWED_SUBJECTS} is set but names no subject"
        )));
    }
    Ok(Some(subjects))
}

pub(crate) fn oidc_require_access_token_typ(
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<bool, ProxiimaError> {
    let Some(raw) = OIDC_REQUIRE_ACCESS_TOKEN_TYP.read(lookup) else {
        return Ok(false);
    };
    parse_bool_value(&OIDC_REQUIRE_ACCESS_TOKEN_TYP, &raw)
}

pub(crate) fn oidc_http_timeout(
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<Duration, ProxiimaError> {
    let Some(raw) = OIDC_HTTP_TIMEOUT_SECONDS.read(lookup) else {
        return Ok(proxiima_auth_oidc::DEFAULT_HTTP_REQUEST_TIMEOUT);
    };
    let timeout = crate::runtime_config::parse_duration_seconds(&OIDC_HTTP_TIMEOUT_SECONDS, &raw)?;
    let seconds = timeout.as_secs();
    if seconds == 0 || seconds > proxiima_auth_oidc::MAX_HTTP_REQUEST_TIMEOUT.as_secs() {
        return Err(ProxiimaError::Config(format!(
            "{OIDC_HTTP_TIMEOUT_SECONDS} must be between 1 and {} seconds, got {raw:?}",
            proxiima_auth_oidc::MAX_HTTP_REQUEST_TIMEOUT.as_secs()
        )));
    }
    Ok(timeout)
}

/// Parse the issuer-aware subject map.
///
fn subject_map(
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<proxiima_auth_oidc::OidcSubjectMap, ProxiimaError> {
    let json = SUBJECT_MAP_JSON.read(lookup).ok_or_else(|| {
        ProxiimaError::Config(format!(
            "PROXIIMA_OIDC_ISSUER set without {SUBJECT_MAP_JSON}"
        ))
    })?;
    proxiima_auth_oidc::OidcSubjectMap::from_json(&json)
        .map_err(|err| ProxiimaError::Config(format!("{SUBJECT_MAP_JSON}: {err}")))
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
