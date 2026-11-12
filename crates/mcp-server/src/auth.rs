//! MCP edge authentication: typed wire-token dispatch onto the core
//! authz contracts. Bearer credentials are interpreted by the host authenticator.

use std::sync::Arc;

use proxiima_core::{
    AuthPath, Authenticator, AuthzContext, Credentials, GroupId, OwnerAccessPort, OwnerId,
    ServedSurface, authenticate,
};

/// Per-request MCP auth context injected by the edge middleware.
/// `authz` is the core authorization currency; `owner` is the
/// MCP-session specific the tool host needs.
///
/// There is no model-id slot here. Trusted model provenance lives on
/// `authz` ([`AuthzContext::trusted_model_id`]) because that is the value
/// an authenticator produced; a second, edge-local copy could only ever
/// disagree with it.
#[derive(Debug, Clone)]
pub struct McpAuthContext {
    pub owner: OwnerId,
    pub authz: AuthzContext,
}

impl McpAuthContext {
    #[must_use]
    fn bound(authz: AuthzContext, owner: OwnerId) -> Self {
        Self { owner, authz }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedAuth {
    authz: AuthzContext,
}

impl ResolvedAuth {
    /// The authenticated principal that owns transport-session continuity.
    pub(crate) fn principal(&self) -> OwnerId {
        self.authz.principal()
    }

    /// The owner the host named for a request that selects none.
    pub(crate) const fn default_owner(&self) -> Option<OwnerId> {
        self.authz.default_owner()
    }

    /// The mandatory owner carried by a verified forwarding credential.
    pub(crate) const fn verified_forwarded_owner(&self) -> Option<OwnerId> {
        self.authz.verified_forwarded_owner()
    }

    /// Whether the eager map already carries the selected owner.
    fn carries(&self, owner: OwnerId) -> bool {
        self.authz.role_for_owner(&owner).is_some()
    }

    /// Resolve one missing Group role through the host access port and merge it
    /// into the authenticated map; missing answers leave the map unchanged.
    async fn filled_from(self, port: Option<&dyn OwnerAccessPort>, group: GroupId) -> Option<Self> {
        let (Some(port), Some(_subject)) = (port, self.authz.subject()) else {
            return Some(self);
        };
        Some(Self {
            authz: self
                .authz
                .with_authenticated_group_role(port, group)
                .await
                .or_else(|| {
                    tracing::warn!("mcp edge: per-group access resolution failed");
                    None
                })?,
        })
    }
}

/// Edge resolver replacing the probe-by-UUID auth store. Composition
/// decides whether a host authenticator exists.
pub struct McpEdgeAuth {
    host: Option<Arc<dyn Authenticator>>,
    owner_access: Option<Arc<dyn OwnerAccessPort>>,
    served_surface: ServedSurface,
}

impl std::fmt::Debug for McpEdgeAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpEdgeAuth")
            .field("host_path", &self.host.is_some())
            .field("per_owner_resolution", &self.owner_access.is_some())
            .finish_non_exhaustive()
    }
}

impl McpEdgeAuth {
    /// Headless composition. No local bearer tokens are accepted.
    #[must_use]
    pub fn headless() -> Self {
        Self {
            host: None,
            owner_access: None,
            served_surface: ServedSurface::All,
        }
    }

    /// Restrict the deployment surface independently of each caller's exact
    /// scope. Registered reads remain served without granting caller keys.
    #[must_use]
    pub fn with_served_surface(mut self, served_surface: ServedSurface) -> Self {
        self.served_surface = match self.served_surface {
            ServedSurface::All => served_surface,
            current @ ServedSurface::Palette(_) => current.intersect(&served_surface),
        };
        self
    }

    /// Attach the host authenticator for bearer credentials.
    /// Host output must carry `AuthPath::HostBearer`; owner binding is
    /// validated separately against the resolved role set.
    #[must_use]
    pub fn with_host(mut self, authenticator: Arc<dyn Authenticator>) -> Self {
        self.host = Some(authenticator);
        self
    }

    /// Attach host policy for ordinary on-demand Group roles and verified
    /// forwarding claims. A configured forwarder must carry a verified
    /// claim even when its eager role map already contains the selection.
    #[must_use]
    pub fn with_owner_access(mut self, owner_access: Arc<dyn OwnerAccessPort>) -> Self {
        self.owner_access = Some(owner_access);
        self
    }

    pub async fn resolve(&self, raw_bearer: &str, owner: OwnerId) -> Option<McpAuthContext> {
        let resolved = self.resolve_unbound(raw_bearer).await?;
        self.bind_to_owner(resolved, owner).await
    }

    /// Bind an authenticated request to its selected default owner. Verified
    /// forwarding claims use only their configured policy role; neither an
    /// eager role nor an ordinary membership can bypass that binding.
    /// Ordinary subjects retain eager and on-demand Group resolution.
    pub(crate) async fn bind_to_owner(
        &self,
        resolved: ResolvedAuth,
        owner: OwnerId,
    ) -> Option<McpAuthContext> {
        if resolved.verified_forwarded_owner().is_some() {
            let authz = resolved
                .authz
                .with_authenticated_forwarded_owner(self.owner_access.as_deref()?, owner)
                .await?;
            return Some(McpAuthContext::bound(authz, owner));
        }
        if let (Some(port), Some(subject)) =
            (self.owner_access.as_deref(), resolved.authz.subject())
            && port
                .is_forwarder(subject)
                .inspect_err(|error| {
                    tracing::warn!(%error, "mcp edge: forwarding policy resolution failed");
                })
                .ok()?
        {
            return None;
        }
        let resolved = if resolved.carries(owner) {
            resolved
        } else {
            resolved
                .filled_from(self.owner_access.as_deref(), owner.as_group())
                .await?
        };
        if !resolved.carries(owner) {
            return None;
        }
        Some(McpAuthContext::bound(resolved.authz, owner))
    }

    /// Authenticate `raw_bearer` on an accepted path without narrowing to
    /// an owner. The edge middleware retains this result across owner/session
    /// selection so host authentication happens exactly once per request.
    pub(crate) async fn resolve_unbound(&self, raw_bearer: &str) -> Option<ResolvedAuth> {
        self.resolve_host(raw_bearer.to_owned()).await
    }

    async fn resolve_host(&self, material: String) -> Option<ResolvedAuth> {
        let authenticator = self.host.as_ref()?;
        let mut authz = authenticate(authenticator.as_ref(), &Credentials::Bearer(material))
            .await
            .ok()?;
        if authz.auth_path() != AuthPath::HostBearer {
            return None;
        }
        authz = authz.with_served_surface(self.served_surface.clone());
        Some(ResolvedAuth { authz })
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
