//! Multi-binding OIDC authenticator composition.
//!
//! Hosts with several OIDC audiences can register one binding per
//! `(issuer, audience)` route. Authentication succeeds only when exactly one
//! binding validates the bearer token.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use proxiima_core::{
    AuthError, AuthPath, Authenticator, AuthzContext, Credentials, OwnerAccessPort, OwnerId,
    ToolScope, UserId, WriterAttribution,
};

use crate::{
    AuthorizedPartyPolicy, KeyResolver, OidcAuthConfig, OidcConfigError, OidcSubjectMap,
    OidcTokenValidator, ValidatedOidcToken,
};

/// Every verified claim of a token, as [`OidcRoleShaper`] sees them.
pub type OidcClaimMap = serde_json::Map<String, serde_json::Value>;

/// Host authz shaping for one binding: runs after the binding validated the
/// token and resolved ordinary roles or retained a forwarder's owner claim.
///
/// It receives the server-resolved context and every verified claim, and
/// returns the context to use. It may narrow (a tool palette, a default
/// owner, publication extensions, fewer owners or rights) or refuse. A
/// returned context with another subject, attested writer, auth path,
/// trusted model id or mandatory forwarded owner, or any owner right the resolved roles lack,
/// refuses the token; its tool scope is intersected with the given one and
/// its expiry clamped to the token's.
pub trait OidcRoleShaper: Send + Sync + std::fmt::Debug {
    /// # Errors
    ///
    /// An [`AuthError`] refuses the token; the binding set fails closed.
    fn shape(
        &self,
        context: AuthzContext,
        token: &ValidatedOidcToken<OidcClaimMap>,
    ) -> Result<AuthzContext, AuthError>;
}

/// Static route owned by one OIDC binding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OidcBindingRoute {
    pub issuer: String,
    pub audience: String,
}

/// Authz shaping applied after a binding validates `(iss, aud, sub)`.
#[derive(Debug, Clone)]
pub enum OidcRoleShape {
    /// Resolve ordinary roles through `OwnerAccessPort`, or retain a
    /// forwarder's verified owner claim pending request owner agreement.
    ServerResolved,
    /// Same as [`Self::ServerResolved`], then attach a tool palette/scope.
    ServerResolvedWithToolScope(ToolScope),
    /// Same as [`Self::ServerResolved`], then the host's [`OidcRoleShaper`]
    /// with every verified claim.
    Host(Arc<dyn OidcRoleShaper>),
}

/// `Host` shapes are equal when they are the same shaper instance.
impl PartialEq for OidcRoleShape {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::ServerResolved, Self::ServerResolved) => true,
            (Self::ServerResolvedWithToolScope(a), Self::ServerResolvedWithToolScope(b)) => a == b,
            (Self::Host(a), Self::Host(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Eq for OidcRoleShape {}

impl OidcRoleShape {
    fn apply(
        &self,
        ctx: AuthzContext,
        token: &ValidatedOidcToken<OidcClaimMap>,
    ) -> Result<AuthzContext, AuthError> {
        match self {
            Self::ServerResolved => Ok(ctx),
            Self::ServerResolvedWithToolScope(scope) => Ok(ctx.with_tool_scope(scope.clone())),
            Self::Host(shaper) => {
                let host_context = shaper.shape(ctx.clone(), token)?;
                narrowed_by_host(&ctx, host_context, token.claims.expires_at)
            }
        }
    }
}

/// Accept a host shaper's context only when it narrows the one it was given:
/// the same `HostBearer` subject, attested writer, trusted model id and
/// mandatory forwarded owner, no owner, kind or manage right the resolved
/// roles lack. The tool scope is intersected with the given one and the expiry
/// clamped to the token's, so neither can widen either.
fn narrowed_by_host(
    resolved: &AuthzContext,
    shaped: AuthzContext,
    token_expires_at: std::time::SystemTime,
) -> Result<AuthzContext, AuthError> {
    let rights_widened = proxiima_core::MemoryKind::ALL.into_iter().any(|kind| {
        shaped.readable_owners(kind).iter().any(|owner| {
            !resolved.may_read(owner, kind)
                || (shaped.may_manage(owner) && !resolved.may_manage(owner))
        }) || shaped
            .writable_owners(kind)
            .iter()
            .any(|owner| !resolved.may_write(owner, kind))
    });
    if shaped.auth_path() != AuthPath::HostBearer
        || shaped.subject() != resolved.subject()
        || shaped.writer() != resolved.writer()
        || shaped.trusted_model_id() != resolved.trusted_model_id()
        || shaped.verified_forwarded_owner() != resolved.verified_forwarded_owner()
        || rights_widened
    {
        tracing::warn!("oidc binding set: host role shape widened the resolved context; refused");
        return Err(AuthError::InvalidCredentials);
    }
    let expires_at = shaped
        .expires_at()
        .map_or(token_expires_at, |at| at.min(token_expires_at));
    let tool_scope = shaped.tool_scope().intersect(resolved.tool_scope());
    Ok(shaped
        .with_expires_at(Some(expires_at))
        .with_tool_scope(tool_scope)
        .with_served_surface(resolved.served_surface().clone()))
}

/// Attach the subject map's trusted model id, if it declares one.
///
/// The map validated the value at parse time, so a rejection here means the
/// two bounds disagree — a deployment fault, not a caller fault. Failing the
/// authentication is the fail-closed answer: a context that silently lost
/// its provenance would write under the caller's own label.
fn bind_trusted_model_id(
    ctx: AuthzContext,
    trusted_model_id: Option<String>,
) -> Result<AuthzContext, AuthError> {
    let Some(trusted_model_id) = trusted_model_id else {
        return Ok(ctx);
    };
    ctx.with_trusted_model_id(trusted_model_id).map_err(|err| {
        tracing::error!(error = %err, "oidc auth: configured trusted_model_id is unusable");
        AuthError::InvalidCredentials
    })
}

fn signed_uuid_claim(value: &serde_json::Value) -> Result<uuid::Uuid, AuthError> {
    value
        .as_str()
        .and_then(|raw| raw.parse::<OwnerId>().ok())
        .map(OwnerId::into_inner)
        .ok_or(AuthError::InvalidCredentials)
}

/// Resolve ordinary memberships, or carry a forwarder's verified owner to
/// the edge without resolving that owner before header agreement.
async fn context_for_verified_token(
    owner_access: &dyn OwnerAccessPort,
    subject: UserId,
    token: &ValidatedOidcToken<OidcClaimMap>,
) -> Result<AuthzContext, AuthError> {
    let is_forwarder = owner_access.is_forwarder(subject).map_err(|err| {
        tracing::warn!(error = %err, "oidc auth: forwarding policy resolution failed");
        AuthError::InvalidCredentials
    })?;
    let context = if is_forwarder {
        let owner = OwnerId::new(signed_uuid_claim(
            token
                .custom
                .get("owner")
                .ok_or(AuthError::InvalidCredentials)?,
        )?);
        let by = signed_uuid_claim(
            token
                .custom
                .get("by")
                .ok_or(AuthError::InvalidCredentials)?,
        )?;
        let via = match token
            .custom
            .get("via")
            .ok_or(AuthError::InvalidCredentials)?
        {
            serde_json::Value::Null => None,
            value => Some(signed_uuid_claim(value)?),
        };
        AuthzContext::from_verified_forwarded_owner(subject, owner, WriterAttribution { by, via })
    } else {
        let roles = owner_access
            .resolve_roles_for_subject(subject)
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "oidc auth: owner-access resolution failed");
                AuthError::InvalidCredentials
            })?;
        AuthzContext::server_resolved(roles, AuthPath::HostBearer)
    };
    Ok(context.with_expires_at(Some(token.claims.expires_at)))
}

/// One OIDC route: validator, identity map, owner-role resolver, authz shape.
pub struct OidcBinding {
    route: OidcBindingRoute,
    pub(crate) validator: OidcTokenValidator,
    allowed_subjects: Option<HashSet<String>>,
    subject_map: OidcSubjectMap,
    owner_access: Arc<dyn OwnerAccessPort>,
    role_shape: OidcRoleShape,
}

impl std::fmt::Debug for OidcBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcBinding")
            .field("route", &self.route)
            .field("role_shape", &self.role_shape)
            .finish_non_exhaustive()
    }
}

impl OidcBinding {
    /// Build a default host-resolved binding.
    ///
    /// # Errors
    ///
    /// Returns an OIDC config error when the issuer/JWKS URL is invalid.
    pub fn new(
        config: OidcAuthConfig,
        keys: Arc<dyn KeyResolver>,
        subject_map: OidcSubjectMap,
        owner_access: Arc<dyn OwnerAccessPort>,
    ) -> Result<Self, OidcConfigError> {
        Self::with_role_shape(
            config,
            keys,
            subject_map,
            owner_access,
            OidcRoleShape::ServerResolved,
        )
    }

    /// Build a binding with an explicit authz shape.
    ///
    /// # Errors
    ///
    /// Returns an OIDC config error when the issuer/JWKS URL is invalid.
    pub fn with_role_shape(
        mut config: OidcAuthConfig,
        keys: Arc<dyn KeyResolver>,
        subject_map: OidcSubjectMap,
        owner_access: Arc<dyn OwnerAccessPort>,
        role_shape: OidcRoleShape,
    ) -> Result<Self, OidcConfigError> {
        let route = OidcBindingRoute {
            issuer: config.issuer.clone(),
            audience: config.audience.clone(),
        };
        let allowed_subjects = std::mem::take(&mut config.allowed_subjects);
        let validator = OidcTokenValidator::new(config, keys)?;
        Ok(Self {
            route,
            validator,
            allowed_subjects,
            subject_map,
            owner_access,
            role_shape,
        })
    }

    #[must_use]
    pub fn route(&self) -> &OidcBindingRoute {
        &self.route
    }

    /// Require the RFC 9068 access-token media type in the JWT header.
    #[must_use]
    pub(crate) fn require_access_token_typ(mut self) -> Self {
        self.validator = self.validator.require_access_token_typ();
        self
    }

    /// Refuse tokens whose authorized party (`azp`) this binding's route does
    /// not allow. The policy belongs to this one `(issuer, audience)` route:
    /// two bindings on one issuer each enforce their own. Without the call no
    /// policy applies; a second call replaces the first. See
    /// [`AuthorizedPartyPolicy`] for the rule.
    #[must_use]
    pub fn with_authorized_party_policy(mut self, policy: AuthorizedPartyPolicy) -> Self {
        self.validator = self.validator.with_authorized_party_policy(policy);
        self
    }

    pub(crate) async fn authz_for_token(
        &self,
        token: ValidatedOidcToken<OidcClaimMap>,
    ) -> Result<AuthzContext, AuthError> {
        let claims = &token.claims;
        if let Some(allow) = &self.allowed_subjects
            && !allow.contains(&claims.subject)
        {
            return Err(AuthError::InvalidCredentials);
        }

        let Some(binding) = self
            .subject_map
            .resolve_binding(&claims.issuer, &claims.subject)
        else {
            tracing::debug!(
                sub = %claims.subject,
                iss = %claims.issuer,
                aud = %claims.audience,
                "oidc auth: token subject not in subject map"
            );
            return Err(AuthError::InvalidCredentials);
        };
        let ctx = bind_trusted_model_id(
            context_for_verified_token(self.owner_access.as_ref(), binding.user_id, &token).await?,
            binding.trusted_model_id,
        )?;
        let ctx = self.role_shape.apply(ctx, &token)?;
        tracing::debug!(sub = %claims.subject, "oidc token accepted (host-resolved)");
        Ok(ctx.with_verified_claims(token.custom))
    }
}

/// The `iss` a token claims before any signature check.
///
/// It only selects which bindings are worth trying and never grants identity:
/// every binding that remains validates the token in full, and one skipped for
/// another issuer could not have accepted it, since the signed `iss` must equal
/// that binding's issuer.
struct ClaimedIssuer(String);

impl ClaimedIssuer {
    /// `None` when the payload or its `iss` cannot be read as a plain string
    /// (a duplicate `iss` member included); every binding is then tried.
    fn read(token: &str) -> Option<Self> {
        #[derive(serde::Deserialize)]
        struct Payload {
            iss: String,
        }
        let payload = URL_SAFE_NO_PAD.decode(token.split('.').nth(1)?).ok()?;
        let Payload { iss } = serde_json::from_slice(&payload).ok()?;
        Some(Self(iss))
    }

    fn selects(&self, binding: &OidcBinding) -> bool {
        binding.route.issuer == self.0
    }
}

/// Construction errors for [`OidcBindingSet`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OidcBindingSetError {
    #[error("OIDC binding set must contain at least one binding")]
    Empty,
    #[error("duplicate OIDC binding route for issuer {issuer:?} audience {audience:?}")]
    DuplicateRoute { issuer: String, audience: String },
}

/// Multi-route host authenticator.
pub struct OidcBindingSet {
    bindings: Vec<OidcBinding>,
}

impl std::fmt::Debug for OidcBindingSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcBindingSet")
            .field("bindings", &self.bindings.len())
            .finish_non_exhaustive()
    }
}

impl OidcBindingSet {
    /// # Errors
    ///
    /// Returns [`OidcBindingSetError::Empty`] for no bindings and
    /// [`OidcBindingSetError::DuplicateRoute`] for duplicate
    /// `(issuer, audience)` routes.
    pub fn new(
        bindings: impl IntoIterator<Item = OidcBinding>,
    ) -> Result<Self, OidcBindingSetError> {
        let bindings = bindings.into_iter().collect::<Vec<_>>();
        if bindings.is_empty() {
            return Err(OidcBindingSetError::Empty);
        }
        let mut seen = HashSet::new();
        for binding in &bindings {
            if !seen.insert(binding.route.clone()) {
                return Err(OidcBindingSetError::DuplicateRoute {
                    issuer: binding.route.issuer.clone(),
                    audience: binding.route.audience.clone(),
                });
            }
        }
        Ok(Self { bindings })
    }

    #[must_use]
    pub fn bindings(&self) -> &[OidcBinding] {
        &self.bindings
    }
}

#[async_trait]
impl Authenticator for OidcBindingSet {
    async fn authenticate(&self, creds: &Credentials) -> Result<AuthzContext, AuthError> {
        let Credentials::Bearer(token) = creds;
        let mut matches = Vec::new();
        // The unverified `iss` skips bindings for other issuers, so their key
        // resolvers never look up (or fetch) keys for a token they cannot
        // accept. It is never an identity: the bindings left validate in full.
        let claimed = ClaimedIssuer::read(token);
        let candidates = self.bindings.iter().filter(|binding| {
            claimed
                .as_ref()
                .is_none_or(|claimed| claimed.selects(binding))
        });
        for binding in candidates {
            match binding.validator.validate_with::<OidcClaimMap>(token).await {
                Ok(validated) => matches.push((binding, validated)),
                Err(reason) => tracing::debug!(
                    iss = %binding.route.issuer,
                    aud = %binding.route.audience,
                    %reason,
                    "oidc binding set: binding refused the token"
                ),
            }
        }

        match matches.len() {
            0 => Err(AuthError::InvalidCredentials),
            1 => {
                let (binding, validated) = matches.pop().expect("one match");
                binding.authz_for_token(validated).await
            }
            _ => {
                tracing::warn!(
                    matches = matches.len(),
                    "oidc binding set: token matched multiple bindings"
                );
                Err(AuthError::InvalidCredentials)
            }
        }
    }
}

#[cfg(test)]
#[path = "binding_set_tests.rs"]
mod tests;
