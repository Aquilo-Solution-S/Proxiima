//! Durable, bearer-bounded authority for registered background commands.
//!
//! Queue rows persist only [`DelegationId`]. Redemption checks the exact queued
//! command, current deployment profile, grant revocation, and current
//! membership. It returns a non-cloneable [`DelegatedPhase`](crate::DelegatedPhase), never a
//! reusable serialized [`AuthzContext`]. Engine/blob calls then enforce the
//! same-runtime binding, exact owner, recorded role ceiling, and finite expiry.
//!
//! Registered linked workers remain trusted in-process to choose among those
//! allowed operations: command scope binds queue routing/redemption, not an
//! in-process operation sandbox. Revocation or membership changes
//! deny the next redemption; they do not cancel an already-redeemed phase.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use thiserror::Error;
use uuid::Uuid;

use crate::authz::DelegationRuntimeBinding;
use crate::text_bounds::check_trimmed_len;

pub(crate) const MAX_TOOL_ID_CHARS: usize = 200;
use crate::{
    AccessError, ActionName, AuthPath, AuthzContext, CapabilitySet, DelegatedPhase,
    DelegationRuntimeAuthority, OwnerAccessPort, OwnerId, OwnerRoles, ProtocolError, Role,
    ScopeKey, ScopeKeyError, ServedSurface, SpiiceRegistryFrozen, StorageError, ToolName,
    ToolScope, UserId, WriterAttribution,
};

/// The registered tool or dispatcher leaf `value` names: a [`ScopeKey::Tool`]
/// for a flat tool, a [`ScopeKey::Action`] for a declared action.
///
/// # Errors
///
/// `InvalidArgument` on `tool_id` when `value` is no scope key, names a
/// resource, is not registered, is a dispatcher without its action, or names
/// an action the tool does not declare.
fn registered_tool_key(
    value: &str,
    registry: &SpiiceRegistryFrozen,
) -> Result<ScopeKey, ProtocolError> {
    const NOT_CANONICAL: &str = "tool id must be provider-safe canonical id";
    const LEAF_REGISTERED: &str = "leaf action scope required and must be registered";
    let invalid = |reason| ProtocolError::invalid_argument("tool_id", reason);
    if value.contains('/') {
        return Err(invalid(NOT_CANONICAL));
    }
    let Ok(key) = ScopeKey::parse(value) else {
        // Text that is no key: the colon says which message the caller gets.
        return Err(invalid(if value.contains(':') {
            LEAF_REGISTERED
        } else {
            NOT_CANONICAL
        }));
    };
    match &key {
        // Both halves resolve through the descriptor's own `action_arg_specs`
        // rather than the substrate `CoreActionMeta` tables, so a spiice
        // dispatcher's leaf is nameable in a delegated command.
        ScopeKey::Action { tool, action } => {
            let declared = registry.mcp_tool(tool.as_str()).is_some_and(|descriptor| {
                descriptor
                    .actions()
                    .any(|(known, _)| known == action.as_str())
            });
            if declared {
                Ok(key)
            } else {
                Err(invalid(LEAF_REGISTERED))
            }
        }
        ScopeKey::Tool(tool) => match registry.mcp_tool(tool.as_str()) {
            Some(descriptor)
                if !descriptor.action_arg_specs.is_empty()
                    || !descriptor.argv_action_specs.is_empty() =>
            {
                Err(invalid("leaf action scope required for grouped tools"))
            }
            Some(_) => Ok(key),
            None => Err(invalid("tool id is not registered")),
        },
        // `resource:name` is a read, not a tool or a leaf.
        ScopeKey::Resource(_) => Err(invalid(LEAF_REGISTERED)),
    }
}

/// Opaque `UUIDv7` handle persisted by a queued job.
#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct DelegationId(Uuid);

impl DelegationId {
    /// Restore a persisted delegation handle; redemption checks its authority.
    #[must_use]
    pub const fn from_uuid(inner: Uuid) -> Self {
        Self(inner)
    }

    #[must_use]
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for DelegationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("delegation:<redacted>")
    }
}

impl std::fmt::Debug for DelegationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DelegationId(<redacted>)")
    }
}

/// One canonical registered flat tool or exact dispatcher action.
///
/// Holds the typed names of a [`ScopeKey::Tool`] or [`ScopeKey::Action`], never
/// a resource key, so the delegated scope and the stored columns come from one
/// parse. Its text ([`Display`](std::fmt::Display), `Serialize`) is `tool` or
/// `tool:action`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DelegatedCommand {
    tool: ToolName,
    action: Option<ActionName>,
}

impl DelegatedCommand {
    /// Parse through the shared registry-aware tool resolution.
    ///
    /// # Errors
    ///
    /// Returns `InvalidArgument` for a non-canonical, unregistered, grouped
    /// tool without an action, or action not present in `registry`.
    pub fn parse(
        raw: impl Into<String>,
        registry: &SpiiceRegistryFrozen,
    ) -> Result<Self, ProtocolError> {
        let raw = raw.into();
        let value = check_trimmed_len(&raw, MAX_TOOL_ID_CHARS).map_err(|violation| {
            ProtocolError::invalid_argument("tool_id", violation.reason("tool id"))
        })?;
        Self::from_key(registered_tool_key(value, registry)?).ok_or_else(|| {
            ProtocolError::invalid_argument("tool_id", "tool id must name a tool or a leaf action")
        })
    }

    /// The tool the command runs, or the dispatcher it is an action of.
    #[must_use]
    pub fn tool(&self) -> &ToolName {
        &self.tool
    }

    /// The dispatcher action; `None` for a flat tool.
    #[must_use]
    pub fn action(&self) -> Option<&ActionName> {
        self.action.as_ref()
    }

    /// `None` for a resource key, which names no command.
    fn from_key(key: ScopeKey) -> Option<Self> {
        match key {
            ScopeKey::Tool(tool) => Some(Self { tool, action: None }),
            ScopeKey::Action { tool, action } => Some(Self {
                tool,
                action: Some(action),
            }),
            ScopeKey::Resource(_) => None,
        }
    }

    /// [`Self::from_storage_parts`], for a test of the stored form.
    #[cfg(test)]
    pub(crate) fn from_storage_parts_for_tests(
        tool: &str,
        action: Option<&str>,
    ) -> Result<Self, StorageError> {
        Self::from_storage_parts(tool, action)
    }

    /// Rebuild a command from the two stored columns.
    fn from_storage_parts(tool: &str, action: Option<&str>) -> Result<Self, StorageError> {
        let not_canonical = || {
            StorageError::ConstraintViolation("stored delegation command is not canonical".into())
        };
        let tool = ToolName::parse(tool).map_err(|_| not_canonical())?;
        let key = match action {
            None => ScopeKey::Tool(tool),
            Some(action) => {
                let action = ActionName::parse(action).map_err(|_| not_canonical())?;
                ScopeKey::action(tool, action).map_err(|_| not_canonical())?
            }
        };
        let command = Self::from_key(key).ok_or_else(not_canonical)?;
        if command.to_string().chars().count() > MAX_TOOL_ID_CHARS {
            return Err(not_canonical());
        }
        Ok(command)
    }

    /// The scope a redeemed phase runs under: exactly this command's key.
    fn scope_key(&self) -> Result<ScopeKey, ScopeKeyError> {
        match &self.action {
            None => Ok(ScopeKey::Tool(self.tool.clone())),
            Some(action) => ScopeKey::action(self.tool.clone(), action.clone()),
        }
    }

    fn tool_scope(&self) -> Result<ToolScope, ScopeKeyError> {
        Ok(ToolScope::Palette(vec![self.scope_key()?]))
    }
}

impl std::fmt::Display for DelegatedCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.action {
            None => write!(formatter, "{}", self.tool),
            Some(action) => write!(formatter, "{}:{action}", self.tool),
        }
    }
}

impl serde::Serialize for DelegatedCommand {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Supported issue response. The queue persists `id`; `expires_at` lets the
/// scheduler avoid claiming work that cannot be redeemed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelegationIssued {
    pub id: DelegationId,
    pub expires_at: SystemTime,
}

/// Result of an authorized, idempotent revocation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationRevocation {
    Revoked,
    AlreadyRevoked,
}

/// Typed delegated-authority failure.
#[derive(Debug, Error)]
pub enum DelegatedAuthorityError {
    #[error("only a host bearer may issue or revoke delegated authority")]
    HostBearerRequired,
    #[error("authenticated subject is missing")]
    MissingSubject,
    #[error("the source bearer has no finite expiry")]
    MissingBearerExpiry,
    #[error("the source bearer or delegated grant has expired")]
    Expired,
    #[error("delegated role ceilings cannot include owner management")]
    ManagementNotDelegable,
    #[error("requested role ceiling exceeds current owner membership")]
    RoleCeilingExceeded,
    #[error("delegated command `{0}` is outside the current caller or deployment tool scope")]
    ToolScopeDenied(String),
    #[error("delegated command is not registered in the current runtime: {0}")]
    CommandUnavailable(String),
    #[error("delegation grant was not found for the expected owner")]
    NotFound,
    #[error("delegation grant is revoked")]
    Revoked,
    #[error("delegation command does not match the queued job")]
    CommandMismatch,
    #[error("the subject no longer has membership on the delegated owner")]
    MembershipRevoked,
    #[error("the subject's current owner role no longer holds the delegated role ceiling")]
    RoleCeilingNoLongerHeld,
    #[error("only the issuer or a current owner manager may revoke this grant")]
    RevocationDenied,
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Nonconstructible witness sealing delegation-table mutations to the
/// validated service path.
#[doc(hidden)]
#[derive(Debug)]
pub struct DelegationMutationPermit {
    _private: (),
}

impl DelegationMutationPermit {
    const fn new() -> Self {
        Self { _private: () }
    }
}

/// Persistence DTO used only by a delegation store adapter.
#[doc(hidden)]
#[derive(Debug)]
pub struct DelegationGrantStorage {
    pub delegation_id: DelegationId,
    pub subject: UserId,
    pub writer: WriterAttribution,
    pub owner: OwnerId,
    pub tool: String,
    pub action: Option<String>,
    pub role_ceiling: Role,
    pub expires_at: SystemTime,
    pub issued_at: SystemTime,
    pub revoked_at: Option<SystemTime>,
    pub revoked_by: Option<UserId>,
}

/// Validated durable row. Unsupported persistence detail; workers retain only
/// [`DelegationId`].
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationGrant {
    delegation_id: DelegationId,
    subject: UserId,
    writer: WriterAttribution,
    owner: OwnerId,
    command: DelegatedCommand,
    role_ceiling: Role,
    expires_at: SystemTime,
    issued_at: SystemTime,
    revoked_at: Option<SystemTime>,
    revoked_by: Option<UserId>,
}

impl DelegationGrant {
    fn issued(
        source: &crate::OwnerScope,
        owner: OwnerId,
        command: DelegatedCommand,
        role_ceiling: Role,
        expires_at: SystemTime,
        issued_at: SystemTime,
    ) -> Self {
        Self {
            delegation_id: DelegationId::from_uuid(Uuid::now_v7()),
            subject: source.subject(),
            writer: source.writer(),
            owner,
            command,
            role_ceiling,
            expires_at,
            issued_at,
            revoked_at: None,
            revoked_by: None,
        }
    }

    /// Validate one row decoded by the backend adapter.
    #[doc(hidden)]
    pub fn from_storage(
        _permit: &DelegationMutationPermit,
        stored: &DelegationGrantStorage,
    ) -> Result<Self, StorageError> {
        if stored.role_ceiling.manages() {
            return Err(StorageError::ConstraintViolation(
                "stored delegation role ceiling includes management".into(),
            ));
        }
        if stored.expires_at <= stored.issued_at {
            return Err(StorageError::ConstraintViolation(
                "stored delegation expiry is not after issue time".into(),
            ));
        }
        if stored.revoked_at.is_some() != stored.revoked_by.is_some() {
            return Err(StorageError::ConstraintViolation(
                "stored delegation revocation shape is invalid".into(),
            ));
        }
        let command = DelegatedCommand::from_storage_parts(&stored.tool, stored.action.as_deref())?;
        Ok(Self {
            delegation_id: stored.delegation_id,
            subject: stored.subject,
            writer: stored.writer,
            owner: stored.owner,
            command,
            role_ceiling: stored.role_ceiling,
            expires_at: stored.expires_at,
            issued_at: stored.issued_at,
            revoked_at: stored.revoked_at,
            revoked_by: stored.revoked_by,
        })
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn delegation_id(&self) -> DelegationId {
        self.delegation_id
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn subject(&self) -> UserId {
        self.subject
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn writer(&self) -> WriterAttribution {
        self.writer
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn owner(&self) -> OwnerId {
        self.owner
    }

    #[doc(hidden)]
    #[must_use]
    pub fn command(&self) -> &DelegatedCommand {
        &self.command
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn role_ceiling(&self) -> Role {
        self.role_ceiling
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn expires_at(&self) -> SystemTime {
        self.expires_at
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn issued_at(&self) -> SystemTime {
        self.issued_at
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn revoked_at(&self) -> Option<SystemTime> {
        self.revoked_at
    }

    #[doc(hidden)]
    #[must_use]
    pub const fn revoked_by(&self) -> Option<UserId> {
        self.revoked_by
    }
}

/// Unsupported persistence port sealed by [`DelegationMutationPermit`].
#[doc(hidden)]
#[async_trait]
pub trait DelegationStorePort: Send + Sync {
    async fn insert(
        &self,
        permit: &DelegationMutationPermit,
        grant: &DelegationGrant,
    ) -> Result<(), StorageError>;

    async fn load(
        &self,
        permit: &DelegationMutationPermit,
        delegation_id: DelegationId,
        expected_owner: OwnerId,
    ) -> Result<Option<DelegationGrant>, StorageError>;

    async fn revoke(
        &self,
        permit: &DelegationMutationPermit,
        delegation_id: DelegationId,
        expected_owner: OwnerId,
        revoked_at: SystemTime,
        revoked_by: UserId,
    ) -> Result<bool, StorageError>;
}

/// Runtime-composed issue/redeem/revoke service shared by tools and workers.
///
/// Exact command registration and the current deployment profile are checked
/// both at issue and redemption. The opaque phase bounds what Engine accepts;
/// the registered linked worker implementation itself is trusted in-process.
#[derive(Clone)]
pub struct DelegatedAuthorityService {
    store: Arc<dyn DelegationStorePort>,
    mutation_permit: Arc<DelegationMutationPermit>,
    owner_access: Arc<dyn OwnerAccessPort>,
    registry: Arc<SpiiceRegistryFrozen>,
    served_surface: ServedSurface,
    runtime_binding: DelegationRuntimeBinding,
}

impl std::fmt::Debug for DelegatedAuthorityService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DelegatedAuthorityService")
            .finish_non_exhaustive()
    }
}

impl DelegatedAuthorityService {
    /// The witness, for a test that calls the store as the service does.
    #[cfg(test)]
    pub(crate) fn mutation_permit_for_tests(&self) -> &DelegationMutationPermit {
        &self.mutation_permit
    }

    /// Runtime-only composition seam. Applications obtain the shared service
    /// from `SpiiceServices` rather than constructing a second instance.
    #[doc(hidden)]
    #[must_use]
    pub fn new(
        store: Arc<dyn DelegationStorePort>,
        owner_access: Arc<dyn OwnerAccessPort>,
        registry: Arc<SpiiceRegistryFrozen>,
        served_surface: ServedSurface,
        runtime_authority: &DelegationRuntimeAuthority,
    ) -> Self {
        Self {
            store,
            mutation_permit: Arc::new(DelegationMutationPermit::new()),
            owner_access,
            registry,
            served_surface,
            runtime_binding: runtime_authority.binding(),
        }
    }

    /// Issue one finite bearer-bounded grant after current membership,
    /// registry, caller-scope, and deployment-profile checks.
    ///
    /// # Errors
    ///
    /// No row is inserted unless all checks succeed.
    pub async fn issue(
        &self,
        source: &AuthzContext,
        owner: OwnerId,
        command: DelegatedCommand,
        role_ceiling: Role,
    ) -> Result<DelegationIssued, DelegatedAuthorityError> {
        let (subject, expires_at) = Self::validate_host_bearer(source)?;
        let scope = source
            .owner_scope()
            .ok_or(DelegatedAuthorityError::HostBearerRequired)?;
        if !scope
            .role_for_owner(&owner)
            .is_some_and(|role| role.dominates(role_ceiling))
        {
            return Err(DelegatedAuthorityError::RoleCeilingExceeded);
        }
        let issued_at = SystemTime::now();
        if expires_at <= issued_at {
            return Err(DelegatedAuthorityError::Expired);
        }
        if role_ceiling.manages() {
            return Err(DelegatedAuthorityError::ManagementNotDelegable);
        }
        let current_roles = self.current_roles(subject).await?;
        let current_role = current_roles
            .role_for(&owner)
            .ok_or(DelegatedAuthorityError::RoleCeilingExceeded)?;
        if !current_role.dominates(role_ceiling) {
            return Err(DelegatedAuthorityError::RoleCeilingExceeded);
        }
        self.validate_current_command(&command)?;
        self.validate_scopes(source.capabilities(), &command)?;

        let grant =
            DelegationGrant::issued(scope, owner, command, role_ceiling, expires_at, issued_at);
        let issued = DelegationIssued {
            id: grant.delegation_id(),
            expires_at,
        };
        self.store.insert(&self.mutation_permit, &grant).await?;
        Ok(issued)
    }

    /// Redeem one queued phase against current owner, command, registry,
    /// deployment profile, membership, and expiry.
    ///
    /// # Errors
    ///
    /// Returns no reusable [`AuthzContext`]; Engine expiry is checked again at
    /// the start of every delegated-capable operation.
    pub async fn redeem_phase(
        &self,
        delegation_id: DelegationId,
        expected_owner: OwnerId,
        expected_command: &DelegatedCommand,
    ) -> Result<DelegatedPhase, DelegatedAuthorityError> {
        let grant = self
            .store
            .load(&self.mutation_permit, delegation_id, expected_owner)
            .await?
            .ok_or(DelegatedAuthorityError::NotFound)?;
        if grant.delegation_id() != delegation_id || grant.owner() != expected_owner {
            return Err(DelegatedAuthorityError::NotFound);
        }
        if grant.command() != expected_command {
            return Err(DelegatedAuthorityError::CommandMismatch);
        }
        if grant.revoked_at().is_some() {
            return Err(DelegatedAuthorityError::Revoked);
        }
        if grant.expires_at() <= SystemTime::now() {
            return Err(DelegatedAuthorityError::Expired);
        }
        self.validate_current_command(grant.command())?;
        self.validate_scopes(&CapabilitySet::all(), grant.command())?;
        let current_roles = self.current_roles(grant.subject()).await?;
        let current_role = current_roles
            .role_for(&expected_owner)
            .ok_or(DelegatedAuthorityError::MembershipRevoked)?;
        if !current_role.dominates(grant.role_ceiling()) {
            return Err(DelegatedAuthorityError::RoleCeilingNoLongerHeld);
        }
        let effective_role = current_role.meet(grant.role_ceiling());
        let tool_scope = grant.command().tool_scope().map_err(|_| {
            DelegatedAuthorityError::CommandUnavailable(grant.command().to_string())
        })?;

        let authz = AuthzContext::server_resolved_with_writer(
            OwnerRoles::scoped_to(grant.subject(), expected_owner, effective_role),
            AuthPath::Delegated,
            grant.writer(),
        )
        .with_expires_at(Some(grant.expires_at()))
        .with_tool_scope(tool_scope)
        .with_served_surface(self.served_surface.clone())
        // The durable grant was issued from a verified bearer. Redemption
        // rechecks expiry, current membership and ceiling above.
        // Seal only after those checks; never reconstruct from a queued owner.
        .seal_verified_scope()
        .map_err(|_| DelegatedAuthorityError::MembershipRevoked)?;
        Ok(DelegatedPhase::new(authz, self.runtime_binding.clone()))
    }

    /// Revoke as the issuer or a current owner manager. Both paths require a
    /// host bearer and current membership on `expected_owner`.
    ///
    /// # Errors
    ///
    /// The owner is part of both load and update predicates. Revocation is
    /// idempotent under concurrent callers.
    pub async fn revoke(
        &self,
        caller: &AuthzContext,
        delegation_id: DelegationId,
        expected_owner: OwnerId,
    ) -> Result<DelegationRevocation, DelegatedAuthorityError> {
        let (caller_subject, _) = Self::validate_host_bearer(caller)?;
        let grant = self
            .store
            .load(&self.mutation_permit, delegation_id, expected_owner)
            .await?
            .ok_or(DelegatedAuthorityError::NotFound)?;
        if grant.delegation_id() != delegation_id || grant.owner() != expected_owner {
            return Err(DelegatedAuthorityError::NotFound);
        }
        let current_roles = self.current_roles(caller_subject).await?;
        let current_role = current_roles
            .role_for(&expected_owner)
            .ok_or(DelegatedAuthorityError::RevocationDenied)?;
        if caller_subject != grant.subject() && !current_role.manages() {
            return Err(DelegatedAuthorityError::RevocationDenied);
        }
        if grant.revoked_at().is_some() {
            return Ok(DelegationRevocation::AlreadyRevoked);
        }
        if self
            .store
            .revoke(
                &self.mutation_permit,
                delegation_id,
                expected_owner,
                SystemTime::now(),
                caller_subject,
            )
            .await?
        {
            Ok(DelegationRevocation::Revoked)
        } else {
            Ok(DelegationRevocation::AlreadyRevoked)
        }
    }

    fn validate_host_bearer(
        source: &AuthzContext,
    ) -> Result<(UserId, SystemTime), DelegatedAuthorityError> {
        if source.auth_path() != AuthPath::HostBearer || source.verified_forwarded_owner().is_some()
        {
            return Err(DelegatedAuthorityError::HostBearerRequired);
        }
        let subject = source
            .subject()
            .ok_or(DelegatedAuthorityError::MissingSubject)?;
        let scope = source
            .owner_scope()
            .ok_or(DelegatedAuthorityError::HostBearerRequired)?;
        let expires_at = source
            .expires_at()
            .ok_or(DelegatedAuthorityError::MissingBearerExpiry)?
            .min(
                scope
                    .expires_at()
                    .ok_or(DelegatedAuthorityError::MissingBearerExpiry)?,
            );
        if expires_at <= SystemTime::now() {
            return Err(DelegatedAuthorityError::Expired);
        }
        Ok((subject, expires_at))
    }

    async fn current_roles(&self, subject: UserId) -> Result<OwnerRoles, DelegatedAuthorityError> {
        let roles = self.owner_access.resolve_roles_for_subject(subject).await?;
        if roles.subject() != subject {
            return Err(AccessError::Resolution(
                "owner access resolver returned roles for a different subject".into(),
            )
            .into());
        }
        Ok(roles)
    }

    fn validate_current_command(
        &self,
        command: &DelegatedCommand,
    ) -> Result<(), DelegatedAuthorityError> {
        let current = DelegatedCommand::parse(command.to_string(), &self.registry)
            .map_err(|error| DelegatedAuthorityError::CommandUnavailable(error.message))?;
        if current != *command {
            return Err(DelegatedAuthorityError::CommandUnavailable(
                command.to_string(),
            ));
        }
        Ok(())
    }

    fn validate_scopes(
        &self,
        capabilities: &CapabilitySet,
        command: &DelegatedCommand,
    ) -> Result<(), DelegatedAuthorityError> {
        let descriptor = self
            .registry
            .mcp_tool(command.tool().as_str())
            .ok_or_else(|| DelegatedAuthorityError::CommandUnavailable(command.to_string()))?;
        let declared = descriptor.declaration(command.action().map(ActionName::as_str));
        let key = command
            .scope_key()
            .map_err(|_| DelegatedAuthorityError::CommandUnavailable(command.to_string()))?;
        if !capabilities.admits(&key, declared) || !self.served_surface.admits(&key, declared) {
            return Err(DelegatedAuthorityError::ToolScopeDenied(
                command.to_string(),
            ));
        }
        Ok(())
    }
}
