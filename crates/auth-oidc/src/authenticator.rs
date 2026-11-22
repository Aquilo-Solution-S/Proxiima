//! OIDC bearer-JWT validation and authentication.
//!
//! [`OidcTokenValidator`] is the audited, authz-free boundary: JWKS
//! signature verification (the algorithm follows the resolved key's type,
//! never the token header), `iss`/`aud`/`exp` checks, and an optional
//! authorized-party policy per binding. Custom hosts that need more than one
//! identity class (e.g. branching on `aud`) compose several validators sharing
//! one [`KeyResolver`] and shape their own [`AuthzContext`] from
//! [`ValidatedOidcClaims`] — see `tests/custom_host_validation.rs`.
//!
//! [`OidcAuthenticator`] is the default [`Authenticator`] built on top of
//! it: [`OidcAuthenticator::new`] maps the validated `(iss, sub)` through an
//! issuer-aware [`OidcSubjectMap`] to a [`UserId`] and resolves ordinary
//! [`proxiima_core::OwnerRoles`] through an `OwnerAccessPort`. A configured
//! forwarder instead carries its verified owner and writer claims without a
//! role until request owner agreement is checked at the edge.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, AlgorithmFamily, Validation, decode, decode_header};
use proxiima_core::{AuthError, Authenticator, AuthzContext, Credentials, OwnerAccessPort};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::authorized_party::AuthorizedPartyPolicy;
use crate::binding_set::OidcBinding;
use crate::config::{OidcAuthConfig, OidcConfigError};
use crate::keys::KeyResolver;
use crate::subject_map::OidcSubjectMap;

pub(crate) const fn verified_algorithms(family: AlgorithmFamily) -> &'static [Algorithm] {
    // `Proxiima.Authentication.signatureAlgorithms`.
    match family {
        AlgorithmFamily::Rsa => &[Algorithm::RS256, Algorithm::RS384, Algorithm::RS512],
        AlgorithmFamily::Ec => &[Algorithm::ES256],
        AlgorithmFamily::Ed => &[Algorithm::EdDSA],
        AlgorithmFamily::Hmac => &[],
    }
}

fn check_token_type(header: Option<&str>, strict: bool) -> Result<(), OidcRejection> {
    const GENERIC: &str = proxiima_core::ledger!(Proxiima.Authentication.genericTokenType);
    const ACCESS: [&str; 2] = proxiima_core::ledger!(Proxiima.Authentication.accessTokenTypes);
    // `Proxiima.Authentication.admitTokenType`.
    match header {
        Some(typ) if ACCESS.iter().any(|access| typ.eq_ignore_ascii_case(access)) => Ok(()),
        Some(typ) if !strict && typ.eq_ignore_ascii_case(GENERIC) => Ok(()),
        None if !strict => Ok(()),
        _ => Err(OidcRejection::WrongTokenType),
    }
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    exp: u64,
}

/// The `(iss, aud, sub, exp)` an [`OidcTokenValidator`] confirmed for a
/// bearer token. No authz shaping: the caller decides identity mapping and
/// capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedOidcClaims {
    pub issuer: String,
    pub audience: String,
    pub subject: String,
    pub expires_at: SystemTime,
}

/// A validated token: the registered claims every token carries, plus the
/// host's own claims `C` read from the same verified payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedOidcToken<C> {
    pub claims: ValidatedOidcClaims,
    /// The whole verified claim set deserialized as `C`: `serde_json::Map`
    /// keeps every claim, a struct picks the ones the host reads.
    pub custom: C,
}

/// Why [`OidcTokenValidator::validate_with`] refused a token. Every variant
/// becomes [`AuthError::InvalidCredentials`] at the authenticator boundary;
/// the reason is for the host's logs and metrics, never for the caller.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OidcRejection {
    #[error("token is not a well-formed JWT: {0}")]
    Malformed(String),
    #[error("token header names no key id")]
    MissingKeyId,
    #[error("no verification key for kid {kid:?}: {reason}")]
    UnknownKey { kid: String, reason: String },
    #[error("token algorithm is not one the verification key's type accepts")]
    DisallowedAlgorithm,
    #[error("token signature does not verify")]
    BadSignature,
    #[error("token has expired")]
    Expired,
    #[error("token is not valid yet (nbf)")]
    NotYetValid,
    #[error("token issuer is not {expected:?}")]
    WrongIssuer { expected: String },
    #[error("token audience does not include {expected:?}")]
    WrongAudience { expected: String },
    #[error("token lacks required claim {0}")]
    MissingClaim(String),
    #[error("token claim {0} has the wrong type")]
    InvalidClaim(String),
    #[error("token expiry is out of range")]
    ExpiryOutOfRange,
    #[error("token subject is not allowed")]
    UnauthorizedSubject,
    #[error("token claims do not match the host's claim type: {0}")]
    CustomClaims(String),
    /// The `typ` header names something other than an access token, or the
    /// validator requires `at+jwt` and the header does not say it.
    #[error("token typ does not name an access token")]
    WrongTokenType,
    /// The binding's [`AuthorizedPartyPolicy`] is set, `aud` names several
    /// audiences and the token has no `azp`.
    #[error("token has several audiences and no authorized party (azp)")]
    MissingAuthorizedParty,
    /// The binding's [`AuthorizedPartyPolicy`] does not list the token's `azp`.
    #[error("token authorized party {azp:?} is not allowed")]
    UnauthorizedParty { azp: String },
}

impl From<OidcRejection> for AuthError {
    fn from(_: OidcRejection) -> Self {
        // Detailed token refusals stay internal to the host.
        Self::InvalidCredentials
    }
}

impl OidcRejection {
    fn from_jwt(err: &jsonwebtoken::errors::Error, issuer: &str, audience: &str) -> Self {
        use jsonwebtoken::errors::ErrorKind;
        match err.kind() {
            ErrorKind::InvalidSignature => Self::BadSignature,
            ErrorKind::InvalidAlgorithm
            | ErrorKind::InvalidAlgorithmName
            | ErrorKind::MissingAlgorithm
            | ErrorKind::UnsupportedAlgorithm => Self::DisallowedAlgorithm,
            ErrorKind::ExpiredSignature => Self::Expired,
            ErrorKind::ImmatureSignature => Self::NotYetValid,
            ErrorKind::InvalidIssuer => Self::WrongIssuer {
                expected: issuer.to_owned(),
            },
            ErrorKind::InvalidAudience => Self::WrongAudience {
                expected: audience.to_owned(),
            },
            ErrorKind::MissingRequiredClaim(claim) => Self::MissingClaim(claim.clone()),
            ErrorKind::InvalidClaimFormat(claim) => Self::InvalidClaim(claim.clone()),
            _ => Self::Malformed(err.to_string()),
        }
    }
}

/// Audited OIDC bearer-JWT validation with no authz shaping.
pub struct OidcTokenValidator {
    issuer: String,
    audience: String,
    leeway_secs: u64,
    allowed_subjects: Option<HashSet<String>>,
    keys: Arc<dyn KeyResolver>,
    authorized_party: Option<AuthorizedPartyPolicy>,
    require_access_token_typ: bool,
}

impl std::fmt::Debug for OidcTokenValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcTokenValidator")
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field("authorized_party", &self.authorized_party)
            .finish_non_exhaustive()
    }
}

impl OidcTokenValidator {
    /// # Errors
    ///
    /// Returns an error when the issuer contains a query or fragment, or the
    /// issuer/JWKS URL violates the HTTPS/loopback transport policy.
    pub fn new(
        config: OidcAuthConfig,
        keys: Arc<dyn KeyResolver>,
    ) -> Result<Self, OidcConfigError> {
        config.validate()?;
        Ok(Self {
            issuer: config.issuer,
            audience: config.audience,
            leeway_secs: config.leeway_secs,
            allowed_subjects: config.allowed_subjects,
            keys,
            authorized_party: None,
            require_access_token_typ: proxiima_core::ledger!(
                Proxiima.Authentication.requireAccessTokenType
            ),
        })
    }

    /// Refuse tokens whose authorized party (`azp`) this validator's issuer
    /// and audience do not allow. Without the call no policy applies; a second
    /// call replaces the first. See [`AuthorizedPartyPolicy`] for the rule.
    #[must_use]
    pub fn with_authorized_party_policy(mut self, policy: AuthorizedPartyPolicy) -> Self {
        self.authorized_party = Some(policy);
        self
    }

    /// Require the RFC 9068 access-token media type (`at+jwt` or
    /// `application/at+jwt`) in the JWT header. Turn it on whenever the issuer
    /// sets it. Without the call a token with no `typ` or the generic `JWT`
    /// also passes, because many issuers send nothing more specific; a `typ`
    /// naming any other kind of token is refused either way.
    #[must_use]
    pub fn require_access_token_typ(mut self) -> Self {
        self.require_access_token_typ = true;
        self
    }

    /// # Errors
    ///
    /// `AuthError::InvalidCredentials` for malformed or unsigned tokens, a
    /// wrong issuer or audience, expiry, a disallowed subject, or an
    /// unresolvable key id. [`Self::validate_with`] gives the specific reason.
    pub async fn validate(&self, token: &str) -> Result<ValidatedOidcClaims, AuthError> {
        self.validate_with::<serde::de::IgnoredAny>(token)
            .await
            .map(|token| token.claims)
            .map_err(AuthError::from)
    }

    /// [`Self::validate`], reading the host's own claims `C` from the same
    /// verified payload and keeping the reason a token was refused.
    ///
    /// `C` sees every claim, the registered ones included; nothing in it is
    /// trusted until the signature, `iss`, `aud`, `exp` and `nbf` checks
    /// above it, and the authorized-party policy when one is set, have passed.
    ///
    /// # Errors
    ///
    /// An [`OidcRejection`] naming the failed check, or
    /// [`OidcRejection::CustomClaims`] when the verified claims do not
    /// deserialize as `C`.
    pub async fn validate_with<C: DeserializeOwned>(
        &self,
        token: &str,
    ) -> Result<ValidatedOidcToken<C>, OidcRejection> {
        let token = self.verify_with(token).await?;
        self.check_subject(&token.claims)?;
        Ok(token)
    }

    // Binding sets count cryptographically verified routes before applying subject policy.
    pub(crate) async fn verify_with<C: DeserializeOwned>(
        &self,
        token: &str,
    ) -> Result<ValidatedOidcToken<C>, OidcRejection> {
        let header = decode_header(token)
            .map_err(|err| OidcRejection::from_jwt(&err, &self.issuer, &self.audience))?;
        check_token_type(header.typ.as_deref(), self.require_access_token_typ)?;
        let kid = header.kid.ok_or(OidcRejection::MissingKeyId)?;
        let key = self
            .keys
            .key_for(&kid)
            .await
            .map_err(|err| OidcRejection::UnknownKey {
                reason: err.to_string(),
                kid,
            })?;

        // The resolved key family sets the allowed algorithms for checking the token header.
        let algorithms = verified_algorithms(key.family());
        if !algorithms.contains(&header.alg) {
            return Err(OidcRejection::DisallowedAlgorithm);
        }
        let mut validation = Validation {
            algorithms: algorithms.to_vec(),
            ..Validation::default()
        };
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = self.leeway_secs;

        // `Proxiima.Authentication.tokenChecks`.
        let data = decode::<serde_json::Value>(token, &key, &validation)
            .map_err(|err| OidcRejection::from_jwt(&err, &self.issuer, &self.audience))?;
        if let Some(policy) = &self.authorized_party {
            policy.check(&data.claims)?;
        }
        // Decode registered and host-typed claims from the same verified payload.
        let registered = Claims::deserialize(&data.claims)
            .map_err(|err| OidcRejection::Malformed(err.to_string()))?;
        let expires_at = UNIX_EPOCH
            .checked_add(Duration::from_secs(registered.exp))
            .ok_or(OidcRejection::ExpiryOutOfRange)?;
        let custom = C::deserialize(data.claims)
            .map_err(|err| OidcRejection::CustomClaims(err.to_string()))?;

        Ok(ValidatedOidcToken {
            claims: ValidatedOidcClaims {
                issuer: self.issuer.clone(),
                audience: self.audience.clone(),
                subject: registered.sub,
                expires_at,
            },
            custom,
        })
    }

    pub(crate) fn check_subject(&self, claims: &ValidatedOidcClaims) -> Result<(), OidcRejection> {
        // `Proxiima.Authentication.subjectAdmission`.
        if self
            .allowed_subjects
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&claims.subject))
        {
            return Err(OidcRejection::UnauthorizedSubject);
        }
        Ok(())
    }
}

/// Validates Zitadel/OIDC bearer JWTs and shapes an [`AuthzContext`]: one
/// [`OidcBinding`] with server-resolved roles, so the post-validation pipeline
/// is written once, there.
pub struct OidcAuthenticator {
    binding: OidcBinding,
}

impl std::fmt::Debug for OidcAuthenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcAuthenticator")
            .field("route", self.binding.route())
            .finish_non_exhaustive()
    }
}

impl OidcAuthenticator {
    /// Default host-resolved path: the validated `(iss, sub)` maps through
    /// `subject_map` to a `UserId`, whose `OwnerRoles` are resolved through
    /// `owner_access`. Returns
    /// `AuthzContext::server_resolved(owner_roles, AuthPath::HostBearer)`.
    ///
    /// # Errors
    ///
    /// Returns an error when the issuer contains a query or fragment, or the
    /// issuer/JWKS URL violates the HTTPS/loopback transport policy.
    pub fn new(
        config: OidcAuthConfig,
        keys: Arc<dyn KeyResolver>,
        subject_map: OidcSubjectMap,
        owner_access: Arc<dyn OwnerAccessPort>,
    ) -> Result<Self, OidcConfigError> {
        Ok(Self {
            binding: OidcBinding::new(config, keys, subject_map, owner_access)?,
        })
    }

    /// Refuse tokens whose authorized party (`azp`) this authenticator's
    /// issuer and audience do not allow. Without the call no policy applies; a
    /// second call replaces the first. See [`AuthorizedPartyPolicy`] for the
    /// rule.
    #[must_use]
    pub fn with_authorized_party_policy(mut self, policy: AuthorizedPartyPolicy) -> Self {
        self.binding = self.binding.with_authorized_party_policy(policy);
        self
    }

    /// Require the RFC 9068 access-token media type in the JWT header.
    #[must_use]
    pub fn require_access_token_typ(mut self) -> Self {
        self.binding = self.binding.require_access_token_typ();
        self
    }
}

#[async_trait]
impl Authenticator for OidcAuthenticator {
    async fn authenticate(&self, creds: &Credentials) -> Result<AuthzContext, AuthError> {
        let Credentials::Bearer(token) = creds;
        let token = self.binding.validator.verify_with(token).await?;
        self.binding.authz_for_token(token).await
    }
}

#[cfg(test)]
#[path = "authenticator_tests.rs"]
mod tests;
