//! Edge-auth contracts: WHO (`Identity`) vs WHAT (`CapabilitySet`).
//!
//! Transports authenticate once at the edge via a host-provided
//! [`Authenticator`]; the engine performs only pure checks against the
//! resulting [`AuthzContext`]. Companion to `auth.rs`, which now only
//! carries credential material and auth failures.

mod declaration;
pub mod hooks;
mod scope_key;
mod tool_name;

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures_util::Stream;
use tokio::time::{Instant, Sleep};

use crate::MemoryKind;
use crate::OwnerScope;
use crate::access::{OwnerAccessPort, OwnerRoles, Role};
use crate::auth::{AuthError, Credentials};
use crate::error::ProtocolError;
use crate::owner_scope::ScopeGrant;
use crate::publication::{PublicationExtensions, PublicationExtensionsError};
use crate::{GroupId, OwnerId, UserId};

#[cfg(test)]
pub(crate) use declaration::ALL_EFFECTS;
pub use declaration::{KeyDeclaration, OwnerNeed, Replay, ToolAudience, ToolEffect};
pub use hooks::{
    AuthorizationHook, AuthzInput, AuthzOperation, AuthzOutcome, AuthzVeto,
    GoalCollaborationChange, MembershipChange, OwnerResolver,
};
pub use scope_key::{ActionName, ResourceKey, ScopeKey, ScopeKeyError, ScopeKeyPart, ToolName};
pub use tool_name::{provider_safe_tool_name, tool_name_matches};

/// The principal and optional caller an authenticator attested for a write.
///
/// Separate from the authenticated credential subject and the selected owner.
/// A direct principal call has no `via`; a session call names its session UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WriterAttribution {
    pub by: uuid::Uuid,
    /// Required on serialized reads; `null` denotes a direct principal call.
    #[serde(deserialize_with = "deserialize_writer_via")]
    pub via: Option<uuid::Uuid>,
}

fn deserialize_writer_via<'de, D>(deserializer: D) -> Result<Option<uuid::Uuid>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer)
}

/// WHO the caller is. What the caller may reach, and until when, lives once
/// in the context's owner scope; ask [`AuthzContext`] for either.
#[derive(Debug, Clone, PartialEq)]
struct Identity {
    subject: Option<UserId>,
    writer: Option<WriterAttribution>,
    principal: OwnerId,
    /// Model/runner identity the authenticator bound to this principal.
    ///
    /// Authenticated provenance, on the same footing as `subject`: it
    /// travels with the identity through narrowing and revalidation, and no
    /// transport payload can set it. `None` means the deployment binds no
    /// model identity to this principal.
    trusted_model_id: Option<String>,
}

/// Rejected input to [`AuthzContext::with_trusted_model_id`].
///
/// A configuration fault in the authenticator, never a caller fault: the
/// value it names came from a credential mapping the deployment wrote.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrustedModelIdError {
    #[error("trusted model id must not be blank")]
    Blank,
    #[error("trusted model id must be at most {max} characters, got {chars}")]
    TooLong { chars: usize, max: usize },
}

/// The exact keys one caller may reach, including reads and resources.
///
/// Text becomes a typed key at its boundary ([`ScopeKey::parse`]). A bare
/// dispatcher name grants none of its action leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolScope {
    All,
    Palette(Vec<ScopeKey>),
}

impl ToolScope {
    #[must_use]
    pub fn allows(&self, key: &ScopeKey) -> bool {
        match self {
            Self::All => true,
            Self::Palette(allowed) => allowed.contains(key),
        }
    }

    /// An action requires its exact `tool:action` leaf.
    #[must_use]
    pub fn allows_action(&self, tool: &ToolName, action: &ActionName) -> bool {
        self.holds(|entry| {
            matches!(entry, ScopeKey::Action { tool: held, action: act, .. }
                if held == tool && act == action)
        })
    }

    #[must_use]
    pub fn advertises_action(&self, tool: &ToolName, action: &ActionName) -> bool {
        self.allows_action(tool, action)
    }

    /// Delegation requires the same exact key as invocation.
    #[must_use]
    pub fn covers_command(&self, tool: &ToolName, action: Option<&ActionName>) -> bool {
        match action {
            Some(action) => self.allows_action(tool, action),
            None => self.allows(&ScopeKey::Tool(tool.clone())),
        }
    }

    /// Whether any action leaf of this dispatcher is held. Descriptor
    /// advertisement also checks that the leaf is actually registered.
    #[must_use]
    pub fn allows_group_advertisement(&self, tool: &ToolName) -> bool {
        self.holds(|entry| matches!(entry, ScopeKey::Action { tool: held, .. } if held == tool))
    }

    #[must_use]
    pub fn allows_tool_advertisement(&self, tool: &ToolName, has_actions: bool) -> bool {
        if has_actions {
            self.allows_group_advertisement(tool)
        } else {
            self.allows(&ScopeKey::Tool(tool.clone()))
        }
    }

    /// Restrict one caller scope by another, using exact key equality.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::All, scope) | (scope, Self::All) => scope.clone(),
            (Self::Palette(left), Self::Palette(right)) => {
                Self::Palette(intersect_keys(left, right))
            }
        }
    }

    fn holds(&self, matches: impl Fn(&ScopeKey) -> bool) -> bool {
        match self {
            Self::All => true,
            Self::Palette(allowed) => allowed.iter().any(matches),
        }
    }
}

/// What the deployment serves: configured commands and every declared read.
///
/// This policy never grants a caller a key. [`CapabilitySet`] also checks the
/// caller's exact [`ToolScope`], including for resources and owner reads.
///
/// ```compile_fail,E0308
/// use proxiima_core::{ServedSurface, ToolScope};
/// let _: ServedSurface = ToolScope::All;
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServedSurface {
    All,
    Palette(Vec<ScopeKey>),
}

impl ServedSurface {
    /// Judge the declaration of this exact registered key. An undeclared
    /// name receives no automatic read admission.
    #[must_use]
    pub fn admits(&self, key: &ScopeKey, declared: KeyDeclaration) -> bool {
        declared.is_read()
            || match self {
                Self::All => true,
                Self::Palette(keys) => keys.contains(key),
            }
    }

    #[must_use]
    pub fn admits_action(
        &self,
        tool: &ToolName,
        action: &ActionName,
        declared: KeyDeclaration,
    ) -> bool {
        ScopeKey::action(tool.clone(), action.clone()).is_ok_and(|key| self.admits(&key, declared))
    }

    /// Restrict configured commands, retaining mandatory declared reads.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::All, surface) | (surface, Self::All) => surface.clone(),
            (Self::Palette(left), Self::Palette(right)) => {
                Self::Palette(intersect_keys(left, right))
            }
        }
    }
}

fn intersect_keys(left: &[ScopeKey], right: &[ScopeKey]) -> Vec<ScopeKey> {
    let permitted: HashSet<&ScopeKey> = right.iter().collect();
    left.iter()
        .filter(|key| permitted.contains(key))
        .cloned()
        .collect()
}

/// Caller authority and deployment policy, kept distinct through admission.
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilitySet {
    pub tool_scope: ToolScope,
    pub served_surface: ServedSurface,
}

impl CapabilitySet {
    #[must_use]
    pub fn all() -> Self {
        Self {
            tool_scope: ToolScope::All,
            served_surface: ServedSurface::All,
        }
    }

    #[must_use]
    pub fn admits(&self, key: &ScopeKey, declared: KeyDeclaration) -> bool {
        self.tool_scope.allows(key) && self.served_surface.admits(key, declared)
    }

    #[must_use]
    pub fn admits_action(
        &self,
        tool: &ToolName,
        action: &ActionName,
        declared: KeyDeclaration,
    ) -> bool {
        self.tool_scope.allows_action(tool, action)
            && self.served_surface.admits_action(tool, action, declared)
    }
}

/// Provenance of an authenticated context — recorded for audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPath {
    HostBearer,
    /// A durable, narrowly scoped grant redeemed by a background worker.
    Delegated,
    System,
}

/// Runtime-held witness for issuing owner-write permits from
/// [`AuthPath::System`] contexts.
///
/// Public `AuthzContext` constructors intentionally remain available for
/// tests and trusted host adapters. Permit issuance is the containment line:
/// a caller-shaped `System` context is not enough without this witness.
#[derive(Debug)]
pub struct SystemAuthority {
    binding: SystemAuthorityBinding,
}

impl SystemAuthority {
    #[must_use]
    pub(crate) const fn new(binding: SystemAuthorityBinding) -> Self {
        Self { binding }
    }

    /// Opaque boot-instance binding for backend services assembled with this
    /// engine. The identifier is not authority on its own; only the
    /// uncloneable `SystemAuthority` can validate it.
    #[doc(hidden)]
    #[must_use]
    pub fn binding(&self) -> SystemAuthorityBinding {
        self.binding.clone()
    }

    #[doc(hidden)]
    #[must_use]
    pub fn authorizes(&self, binding: &SystemAuthorityBinding) -> bool {
        self.binding == *binding
    }
}

/// Opaque identity shared by one booted engine and its host-owned backends.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemAuthorityBinding(uuid::Uuid);

impl SystemAuthorityBinding {
    pub(crate) fn fresh() -> Self {
        Self(uuid::Uuid::now_v7())
    }
}

/// Uncloneable boot witness used only while composing the one delegated
/// authority service and the worker-facing backend services for an Engine.
/// It is never published through `SpiiceServices` or `SpiiceWorkerContext`.
#[doc(hidden)]
pub struct DelegationRuntimeAuthority {
    binding: DelegationRuntimeBinding,
}

impl std::fmt::Debug for DelegationRuntimeAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DelegationRuntimeAuthority")
            .finish_non_exhaustive()
    }
}

impl DelegationRuntimeAuthority {
    pub(crate) const fn new(binding: DelegationRuntimeBinding) -> Self {
        Self { binding }
    }

    pub(crate) fn binding(&self) -> DelegationRuntimeBinding {
        self.binding.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DelegationRuntimeBinding(uuid::Uuid);

impl DelegationRuntimeBinding {
    pub(crate) fn fresh() -> Self {
        Self(uuid::Uuid::now_v7())
    }
}

#[derive(Clone, PartialEq)]
struct VerifiedClaims(Arc<serde_json::Map<String, serde_json::Value>>);

impl std::fmt::Debug for VerifiedClaims {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedClaims").finish_non_exhaustive()
    }
}

/// The one holder of a context's roles, expiry and confinement, in whichever
/// form the context has reached.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnerScopeState {
    /// Roles the host resolved, not yet sealed by authentication.
    Unsealed(ScopeGrant),
    Sealed(OwnerScope),
}

impl OwnerScopeState {
    const fn sealed(&self) -> Option<&OwnerScope> {
        match self {
            Self::Unsealed(_) => None,
            Self::Sealed(scope) => Some(scope),
        }
    }

    const fn grant(&self) -> &ScopeGrant {
        match self {
            Self::Unsealed(grant) => grant,
            Self::Sealed(scope) => scope.grant(),
        }
    }

    /// Sealed under `writer`.
    fn sealed_by(self, writer: WriterAttribution) -> Self {
        match self {
            Self::Unsealed(grant) => Self::Sealed(OwnerScope::seal(grant, writer)),
            Self::Sealed(scope) => Self::Sealed(OwnerScope::seal(scope.into_grant(), writer)),
        }
    }

    fn narrowed(self, owner: OwnerId) -> Option<Self> {
        match self {
            Self::Unsealed(grant) => grant.narrow(owner).map(Self::Unsealed),
            Self::Sealed(scope) => scope.narrow(owner).map(Self::Sealed),
        }
    }

    /// The expiry moves earlier or stays ([`ScopeGrant::clamped_to`]).
    fn clamped_to(self, expires_at: Option<SystemTime>) -> Self {
        match self {
            Self::Unsealed(grant) => Self::Unsealed(grant.clamped_to(expires_at)),
            Self::Sealed(scope) => Self::Sealed(scope.clamped_to(expires_at)),
        }
    }
}

/// What a tool call asks of the owner it selects
/// ([`AuthzContext::may_run_tool`]). A key's declaration says which
/// ([`KeyDeclaration::owner_access`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerAccess {
    Read,
    Write,
    /// A write declared [Goal work](OwnerNeed::GoalWork).
    GoalWork,
}

/// A host read set (#677), the owner scope its local reads are judged by
/// (the scope of the context it was bound on, before the write narrowed
/// it), and the other-store reads the caller has read through the port.
/// Built by [`AuthzContext::with_read_set`]; storage reads it from the
/// write permit. The default is the empty set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoundReadSet {
    reads: crate::ReadSet,
    // Boxed: every write permit carries one, and an inline scope would
    // dwarf the permit enums that hold them.
    reader: Option<Box<OwnerScope>>,
    /// What [`crate::storage_ports::other_stores::OtherStoresService`]
    /// returned for the other-store reads it read as the caller.
    other_store: Vec<crate::storage_ports::other_stores::ReadOrigin>,
}

impl BoundReadSet {
    /// The memories the session read.
    #[must_use]
    pub const fn reads(&self) -> &crate::ReadSet {
        &self.reads
    }

    /// The other-store reads, each read through the port as the caller.
    ///
    /// # Errors
    ///
    /// The first other-store read nobody read through the port: a write
    /// that did not come through a tool call, or one whose other-store
    /// read was never resolved.
    pub fn other_store_reads(&self) -> Result<Vec<uuid::Uuid>, crate::MemoryId> {
        self.reads
            .other_store()
            .map(|memory| {
                self.other_store
                    .iter()
                    .any(|read| read.handle() == memory.into_inner())
                    .then_some(memory.into_inner())
                    .ok_or(memory)
            })
            .collect()
    }

    /// The owner scope that must be able to read each of them; `None` when
    /// the set was bound on a context without one, and the write's own
    /// scope judges.
    #[must_use]
    pub fn reader(&self) -> Option<&OwnerScope> {
        self.reader.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthzContext {
    identity: Identity,
    capabilities: CapabilitySet,
    auth_path: AuthPath,
    owner_scope: OwnerScopeState,
    /// Mandatory owner from a verified forwarding credential. It selects no
    /// ordinary membership and survives request narrowing.
    forwarded_owner: Option<OwnerId>,
    /// `CloudEvents` extension attributes the HOST bound to this context.
    ///
    /// Deliberately NOT part of [`Identity`]: it is deployment-shaped
    /// context about the run that produced a write, not a claim about who
    /// the caller is, so it is never presented as authenticated identity
    /// material.
    publication_extensions: PublicationExtensions,
    /// The memories the HOST says this session read before it writes
    /// (#677). Like `publication_extensions`, context about the run, not
    /// identity: never re-presented through revalidation and never taken
    /// from a tool argument.
    read_set: BoundReadSet,
    /// Uninterpreted verified payload for this request, outside the identity
    /// carried into stream revalidation.
    verified_claims: Option<VerifiedClaims>,
    /// The owner the HOST names for a request that selects none. Always an
    /// owner `owner_roles` carries; see [`AuthzContext::with_default_owner`].
    default_owner: Option<OwnerId>,
    /// The registered tool whose handler this context was handed to. Stamped
    /// by the registration's call wrapper, never by a caller, and read by
    /// verbs whose admission depends on the calling tool's declaration
    /// ([`crate::UnitOfWork::erase_own_series`]).
    invoking_tool: Option<InvokingTool>,
    /// The engine binding of the [`SystemAuthority`] [`Self::for_system`]
    /// was given. A System context without it drives no write gate.
    system_binding: Option<SystemAuthorityBinding>,
}

/// The registered tool a context was handed to, and the declared effect of
/// the action it runs: the flat tool's `EFFECT`, or the dispatched action
/// spec's `effect` — never the dispatcher's join, so one destructive action
/// does not lend its authority to the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvokingTool {
    pub name: &'static str,
    /// `None` only for a flat tool that declared nothing, which a frozen
    /// registry refuses.
    pub effect: Option<ToolEffect>,
}

/// Opaque authority for one redeemed durable-worker phase.
///
/// The phase is intentionally neither cloneable nor convertible to
/// [`AuthzContext`]. Linked worker implementations are trusted in-process,
/// but every delegated-capable Engine operation rechecks the finite bearer
/// deadline before extracting the exact-owner, role-ceiling context.
///
/// ```compile_fail
/// # use proxiima_core::DelegatedPhase;
/// fn duplicate(phase: DelegatedPhase) {
///     let _copy = phase.clone();
/// }
/// ```
///
/// ```compile_fail
/// # use proxiima_core::{AuthzContext, DelegatedPhase};
/// fn extract(phase: &DelegatedPhase) -> &AuthzContext {
///     phase.authz()
/// }
/// ```
#[must_use]
pub struct DelegatedPhase {
    authz: AuthzContext,
    runtime_binding: DelegationRuntimeBinding,
}

impl DelegatedPhase {
    pub(crate) fn new(authz: AuthzContext, runtime_binding: DelegationRuntimeBinding) -> Self {
        debug_assert_eq!(authz.auth_path(), AuthPath::Delegated);
        Self {
            authz,
            runtime_binding,
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_for_test(&mut self) {
        self.authz = self
            .authz
            .clone()
            .with_expires_at(Some(SystemTime::UNIX_EPOCH));
    }
}

impl std::fmt::Debug for DelegatedPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DelegatedPhase")
            .field("expires_at", &self.authz.expires_at())
            .finish_non_exhaustive()
    }
}

mod engine_authority_seal {
    use super::{AuthPath, AuthzContext, DelegatedPhase, EngineOperationAuthority, ProtocolError};

    pub(crate) trait Sealed {
        fn context_for_engine_operation(
            &self,
        ) -> Result<EngineOperationAuthority<'_>, ProtocolError>;
    }

    impl Sealed for AuthzContext {
        fn context_for_engine_operation(
            &self,
        ) -> Result<EngineOperationAuthority<'_>, ProtocolError> {
            if self.auth_path() == AuthPath::Delegated {
                return Err(ProtocolError::forbidden(
                    "raw delegated authorization contexts are not Engine authority",
                ));
            }
            Ok(EngineOperationAuthority {
                authz: self,
                redeemed_phase: false,
                runtime_binding: None,
            })
        }
    }

    impl Sealed for DelegatedPhase {
        fn context_for_engine_operation(
            &self,
        ) -> Result<EngineOperationAuthority<'_>, ProtocolError> {
            if self.authz.is_expired() {
                return Err(ProtocolError::forbidden(
                    "delegated worker phase has expired",
                ));
            }
            Ok(EngineOperationAuthority {
                authz: &self.authz,
                redeemed_phase: true,
                runtime_binding: Some(&self.runtime_binding),
            })
        }
    }
}

/// Sealed authority accepted by delegated-capable Engine operations.
///
/// Ordinary callers continue to pass [`AuthzContext`]. Durable workers pass
/// an opaque [`DelegatedPhase`] returned by
/// [`crate::DelegatedAuthorityService::redeem_phase`]. External crates cannot
/// implement this trait or extract its context.
#[allow(private_bounds)]
pub trait EngineAuthority: engine_authority_seal::Sealed {}

impl EngineAuthority for AuthzContext {}
impl EngineAuthority for DelegatedPhase {}

pub(crate) struct EngineOperationAuthority<'a> {
    authz: &'a AuthzContext,
    redeemed_phase: bool,
    runtime_binding: Option<&'a DelegationRuntimeBinding>,
}

impl<'a> EngineOperationAuthority<'a> {
    #[must_use]
    pub(crate) const fn authz(&self) -> &'a AuthzContext {
        self.authz
    }

    #[must_use]
    pub(crate) const fn redeemed_phase(&self) -> bool {
        self.redeemed_phase
    }

    pub(crate) fn validate_runtime_binding(
        &self,
        expected: Option<&DelegationRuntimeBinding>,
    ) -> Result<(), ProtocolError> {
        match (self.runtime_binding, expected) {
            (None, _) => Ok(()),
            (Some(actual), Some(expected)) if actual == expected => Ok(()),
            (Some(_), Some(_) | None) => Err(ProtocolError::forbidden(
                "delegated worker phase belongs to a different runtime",
            )),
        }
    }
}

pub(crate) fn context_for_engine_operation<A>(
    authority: &A,
) -> Result<EngineOperationAuthority<'_>, ProtocolError>
where
    A: EngineAuthority + ?Sized,
{
    engine_authority_seal::Sealed::context_for_engine_operation(authority)
}

impl AuthzContext {
    /// Authenticate credentials through the trusted host and seal its
    /// server-resolved owner roles into an [`OwnerScope`].
    ///
    /// # Errors
    ///
    /// Returns the host authentication error, or `InvalidCredentials` when
    /// the host returned a context without an attested writer.
    pub async fn authenticate(
        authenticator: &dyn Authenticator,
        credentials: &Credentials,
    ) -> Result<Self, AuthError> {
        let context = authenticator.authenticate(credentials).await?;
        context.seal_verified_scope()
    }

    /// A sealed [`AuthPath::System`] context for backend work that has no
    /// bearer to authenticate: scheduled jobs, projectors, host-internal
    /// writes.
    ///
    /// It takes the runtime's uncloneable [`SystemAuthority`] where
    /// [`Self::authenticate`] takes credentials, so no stub authenticator
    /// and empty bearer stand in for the missing caller. `owner_roles` are
    /// the roles the host resolved for the owners this work may touch; they
    /// are sealed into the [`OwnerScope`] exactly as an authenticator's
    /// would be. The witness is not the only way to a System-path context
    /// (`authenticate` seals whatever a trusted authenticator returns); the
    /// Engine's write gates accept only a System context built here, with
    /// that Engine's witness.
    #[must_use]
    pub fn for_system(authority: &SystemAuthority, owner_roles: OwnerRoles) -> Self {
        let writer = WriterAttribution {
            by: owner_roles.subject().into_inner(),
            via: None,
        };
        let mut context = Self::server_resolved_with_writer(owner_roles, AuthPath::System, writer);
        context.owner_scope = context.owner_scope.sealed_by(writer);
        context.system_binding = Some(authority.binding());
        context
    }

    /// Whether [`Self::for_system`] built this context from the witness
    /// bound to `binding`.
    pub(crate) fn carries_system_authority(&self, binding: &SystemAuthorityBinding) -> bool {
        self.system_binding.as_ref() == Some(binding)
    }

    /// A sealed [`AuthPath::HostBearer`] context for an embedded host that
    /// explicitly opts into insecure single-owner access.
    ///
    /// The boot-held witness gates this credential-free path. Both the roles
    /// and the sealed scope grant only `owner`, as the synthetic subject's own
    /// id. This context grants no
    /// System-path authority and must never authenticate a network request.
    ///
    /// # Panics
    ///
    /// Panics only if the exact owner role constructed here cannot be narrowed.
    #[must_use]
    pub fn for_insecure_single_owner(authority: &SystemAuthority, owner: OwnerId) -> Self {
        let _ = authority;
        // The one role rule: the owner is the synthetic subject's own id, so
        // it holds the personal role, which never manages.
        let subject = owner.as_user();
        let roles = OwnerRoles::scoped_to(subject, owner, Role::personal());
        let writer = WriterAttribution {
            by: subject.into_inner(),
            via: None,
        };
        let mut context = Self::server_resolved_with_writer(roles, AuthPath::HostBearer, writer);
        context.owner_scope = context.owner_scope.sealed_by(writer);
        context
            .narrowed_to_owner(owner)
            .expect("single owner's verified role is self-accessible")
    }

    pub(crate) fn seal_verified_scope(mut self) -> Result<Self, AuthError> {
        let writer = self.writer().ok_or(AuthError::InvalidCredentials)?;
        self.owner_scope = self.owner_scope.sealed_by(writer);
        if self.auth_path == AuthPath::HostBearer
            && self.forwarded_owner.is_none()
            && let OwnerScopeState::Sealed(scope) = self.owner_scope
        {
            let subject = scope.subject();
            self.owner_scope =
                OwnerScopeState::Sealed(scope.with_goal_principal(
                    crate::goal_collaboration::GoalPrincipalScope::new(subject),
                ));
        }
        Ok(self)
    }

    #[must_use]
    pub fn owner_scope(&self) -> Option<&OwnerScope> {
        self.owner_scope.sealed()
    }

    #[must_use]
    pub fn subject(&self) -> Option<UserId> {
        self.identity.subject
    }

    #[must_use]
    pub const fn auth_path(&self) -> AuthPath {
        self.auth_path
    }

    #[must_use]
    pub fn principal(&self) -> OwnerId {
        self.identity.principal
    }

    pub(crate) fn is_expired(&self) -> bool {
        self.owner_scope.grant().is_expired()
    }

    #[must_use]
    pub fn expires_at(&self) -> Option<SystemTime> {
        self.owner_scope.grant().expires_at()
    }

    /// Model/runner identity certified by the authenticating token, if the
    /// deployment binds one to this principal.
    ///
    /// This is the only trustworthy answer to "which model is calling".
    /// Spiices may build policy on it; the caller-supplied `model_id` label
    /// is a claim, not a credential.
    #[must_use]
    pub fn trusted_model_id(&self) -> Option<&str> {
        self.identity.trusted_model_id.as_deref()
    }

    /// The complete verified payload this request's authenticator retained,
    /// including registered claims, or `None` when it retained none.
    ///
    /// Access is immutable and survives cloning and owner narrowing. This
    /// exposes JSON claims, never the raw bearer credential. The consumer
    /// interprets its application claims; their presence grants no authority.
    #[must_use]
    pub fn verified_claims(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.verified_claims
            .as_ref()
            .map(|claims| claims.0.as_ref())
    }

    /// Retain this request's already verified payload unchanged.
    ///
    /// For trusted authenticators only: `claims` must come from their verified
    /// payload, never an unverified header or tool argument. This does not
    /// validate or interpret claims, or grant owner or tool authority.
    #[must_use]
    pub fn with_verified_claims(
        mut self,
        claims: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        self.verified_claims = Some(VerifiedClaims(Arc::new(claims)));
        self
    }

    /// `CloudEvents` extension attributes this context contributes to every
    /// Fact it captures. Empty unless the host bound some.
    #[must_use]
    pub fn publication_extensions(&self) -> &PublicationExtensions {
        &self.publication_extensions
    }

    /// The memories the host bound as read before this context's writes.
    /// Empty unless the host bound some.
    #[must_use]
    pub const fn read_set(&self) -> &crate::ReadSet {
        &self.read_set.reads
    }

    /// The read set with the read authority storage checks it against.
    ///
    /// The reader keeps the wider roles it was bound with, but is handed out
    /// with this context's current deadline: the one exit of the read set is
    /// where its expiry is read, so it cannot outlive a shortened context.
    pub(crate) fn bound_reads(&self) -> BoundReadSet {
        let mut bound = self.read_set.clone();
        bound.reader = bound
            .reader
            .map(|reader| Box::new(reader.clamped_to(self.expires_at())));
        bound
    }

    /// The other-store reads of the read set not yet read through the port.
    pub(crate) fn unread_other_store_reads(&self) -> Vec<uuid::Uuid> {
        let read = &self.read_set.other_store;
        self.read_set
            .reads
            .other_store()
            .map(crate::MemoryId::into_inner)
            .filter(|memory| !read.iter().any(|origin| origin.handle() == *memory))
            .collect()
    }

    /// Keep what the other-stores port returned for the read set's
    /// other-store reads, read as this context's caller.
    pub(crate) fn with_other_store_reads(
        mut self,
        read: Vec<crate::storage_ports::other_stores::ReadOrigin>,
    ) -> Self {
        self.read_set.other_store.extend(read);
        self
    }

    /// The owner a request that selects none acts for, if the host named one.
    #[must_use]
    pub const fn default_owner(&self) -> Option<OwnerId> {
        self.default_owner
    }

    /// The writer attested by the authenticator, before or after scope sealing.
    #[must_use]
    pub const fn writer(&self) -> Option<WriterAttribution> {
        self.identity.writer
    }

    /// Bind the writer verified by the host authenticator before authentication seals it.
    ///
    /// The pair must come from authenticated identity, never a tool argument,
    /// request header or publication extension. A sealed scope cannot be rebound.
    #[must_use]
    pub fn with_attested_writer(mut self, writer: WriterAttribution) -> Option<Self> {
        if self.owner_scope().is_some() {
            return None;
        }
        self.identity.writer = Some(writer);
        Some(self)
    }

    #[must_use]
    pub fn tool_scope(&self) -> &ToolScope {
        &self.capabilities.tool_scope
    }

    #[must_use]
    pub const fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    #[must_use]
    pub const fn served_surface(&self) -> &ServedSurface {
        &self.capabilities.served_surface
    }

    #[must_use]
    pub fn can_access_owner(&self, owner: &OwnerId) -> bool {
        self.may_read(owner, MemoryKind::Fact)
    }

    /// The owners whose Facts the context reads, in the order of memory
    /// spaces.
    pub(crate) fn accessible_owners(&self) -> impl Iterator<Item = OwnerId> {
        self.readable_owners(MemoryKind::Fact).into_iter()
    }

    #[must_use]
    pub fn may_read(&self, owner: &OwnerId, kind: MemoryKind) -> bool {
        self.owner_scope.grant().may_read(owner, kind)
    }

    #[must_use]
    pub fn may_write(&self, owner: &OwnerId, kind: MemoryKind) -> bool {
        self.owner_scope.grant().may_write(owner, kind)
    }

    /// The owner-role rule for a tool call: a declared read needs read
    /// access to the selected owner, Goal work [`Role::works_goals`], anything
    /// else write access. The call-time gate and every advertisement of a
    /// tool ask this, so they cannot answer differently.
    ///
    /// [`Role::works_goals`]: crate::Role::works_goals
    #[must_use]
    pub fn may_run_tool(&self, owner: &OwnerId, access: OwnerAccess) -> bool {
        match access {
            OwnerAccess::Read => self.may_read(owner, MemoryKind::Fact),
            OwnerAccess::Write => self.may_write(owner, MemoryKind::Fact),
            OwnerAccess::GoalWork => self.may_work_goals(owner),
        }
    }

    /// Whether the subject sets, modifies and transitions `owner`'s Goals:
    /// its role [works Goals](crate::Role::works_goals)
    /// (`request_may_work_goals` in `docs/lean/Causa/Authorization.lean`).
    /// A forwarded role is capped below Goal work. A delegated or system
    /// request is no membership request: it works Goals only where it also
    /// [administers](crate::Role::administers).
    #[must_use]
    pub fn may_work_goals(&self, owner: &OwnerId) -> bool {
        self.role_for_owner(owner).is_some_and(|role| {
            role.works_goals() && (self.auth_path == AuthPath::HostBearer || role.administers())
        })
    }

    /// Whether the subject reads `owner`'s Goals
    /// ([`Role::reads_goals`](crate::Role::reads_goals)).
    #[must_use]
    pub fn may_read_goals(&self, owner: &OwnerId) -> bool {
        self.role_for_owner(owner)
            .is_some_and(crate::access::Role::reads_goals)
    }

    /// Whether the subject reads an entry of `kind` owned by `owner`: a
    /// memory by the read limit, a Goal by reading the owner's Goals.
    #[must_use]
    pub fn may_read_entry(&self, owner: &OwnerId, kind: crate::EntityKind) -> bool {
        self.role_for_owner(owner)
            .is_some_and(|role| role.may_read_entry(kind))
    }

    /// Whether the subject administers `owner`
    /// ([`Role::administers`](crate::Role::administers)).
    #[must_use]
    pub fn may_administer(&self, owner: &OwnerId) -> bool {
        self.role_for_owner(owner)
            .is_some_and(crate::access::Role::administers)
    }

    #[must_use]
    pub fn may_manage(&self, owner: &OwnerId) -> bool {
        self.owner_scope.grant().may_manage(owner)
    }

    /// Server-resolved role for one owner, if the authenticated subject has
    /// one. Durable delegation records this role only as an upper bound and
    /// resolves it again at every redemption.
    #[must_use]
    pub fn role_for_owner(&self, owner: &OwnerId) -> Option<crate::access::Role> {
        self.owner_scope.grant().role_for(owner)
    }

    #[must_use]
    pub fn readable_owners(&self, kind: MemoryKind) -> Vec<OwnerId> {
        self.owner_scope.grant().readable_owners(kind)
    }

    #[must_use]
    pub fn writable_owners(&self, kind: MemoryKind) -> Vec<OwnerId> {
        self.owner_scope.grant().writable_owners(kind)
    }

    #[must_use]
    pub fn server_resolved(owner_roles: OwnerRoles, auth_path: AuthPath) -> Self {
        let writer = WriterAttribution {
            by: owner_roles.subject().into_inner(),
            via: None,
        };
        Self::server_resolved_with_writer(owner_roles, auth_path, writer)
    }

    pub(crate) fn server_resolved_with_writer(
        owner_roles: OwnerRoles,
        auth_path: AuthPath,
        writer: WriterAttribution,
    ) -> Self {
        let subject = owner_roles.subject();
        Self {
            identity: Identity {
                subject: Some(subject),
                writer: Some(writer),
                principal: OwnerId::from(subject),
                trusted_model_id: None,
            },
            capabilities: CapabilitySet::all(),
            auth_path,
            owner_scope: OwnerScopeState::Unsealed(ScopeGrant::new(owner_roles, None)),
            forwarded_owner: None,
            publication_extensions: PublicationExtensions::new(),
            read_set: BoundReadSet::default(),
            verified_claims: None,
            default_owner: None,
            invoking_tool: None,
            system_binding: None,
        }
    }

    /// Carry a verified forwarding credential without granting owner access.
    ///
    /// For trusted authenticators only: `subject` is the authenticated
    /// forwarder and `owner` comes from its verified credential, never a
    /// request header or tool argument. `writer` is attested by the same
    /// credential. The edge must check selection agreement and resolve the
    /// registered owner before any role is granted.
    #[must_use]
    pub fn from_verified_forwarded_owner(
        subject: UserId,
        owner: OwnerId,
        writer: WriterAttribution,
    ) -> Self {
        let mut context = Self::server_resolved_with_writer(
            OwnerRoles::without_access(subject),
            AuthPath::HostBearer,
            writer,
        );
        context.forwarded_owner = Some(owner);
        context
    }

    #[must_use]
    pub const fn verified_forwarded_owner(&self) -> Option<OwnerId> {
        self.forwarded_owner
    }

    /// Resolve a verified forwarding claim after owner selection. The result
    /// is sealed to that one owner with the non-managing policy ceiling.
    /// A missing/expired credential, mismatched selection, unknown owner or
    /// failed policy/registry lookup returns no authority.
    pub async fn with_authenticated_forwarded_owner(
        &self,
        port: &dyn OwnerAccessPort,
        owner: OwnerId,
    ) -> Option<Self> {
        let scope = self.owner_scope()?;
        if self.forwarded_owner != Some(owner) || scope.is_expired() {
            return None;
        }
        let subject = self.subject()?;
        if !port
            .is_forwarder(subject)
            .inspect_err(|error| {
                tracing::warn!(%error, "forwarded owner: policy resolution failed");
            })
            .ok()?
        {
            return None;
        }
        let role = port
            .resolve_forwarded_owner_role(subject, owner)
            .await
            .inspect_err(|error| {
                tracing::warn!(%error, "forwarded owner: registry resolution failed");
            })
            .ok()??;
        let roles = OwnerRoles::scoped_to(subject, owner, role.role());
        let mut next = self.clone();
        next.owner_scope = OwnerScopeState::Sealed(OwnerScope::from_verified_roles(
            roles,
            scope.expires_at(),
            scope.writer(),
        ));
        next.narrowed_to_owner(owner)
    }

    #[must_use]
    pub fn with_tool_scope(mut self, tool_scope: ToolScope) -> Self {
        self.capabilities.tool_scope = tool_scope;
        self
    }

    /// Restrict deployment policy without widening an existing restriction.
    #[must_use]
    pub fn with_served_surface(mut self, served_surface: ServedSurface) -> Self {
        self.capabilities.served_surface = match self.capabilities.served_surface {
            ServedSurface::All => served_surface,
            current @ ServedSurface::Palette(_) => current.intersect(&served_surface),
        };
        self
    }

    /// Bind the model/runner identity this credential certifies.
    ///
    /// **For authenticators only.** The value must come from the credential
    /// itself — an OIDC subject binding, or the equivalent in a custom host
    /// — never from a tool argument, request header, MCP `clientInfo`, or
    /// any other caller-controlled payload. A transport that lets a caller
    /// reach this builder has published a forgeable provenance field.
    ///
    /// Takes a `String`, deliberately not an `Option`: there is no "clear
    /// it" call, so no later step in a builder chain can quietly drop
    /// provenance an authenticator already established. The value is
    /// trimmed here, once, so the stored label, the conflict comparison and
    /// any idempotency key derived from it are the same string.
    ///
    /// Bounded by [`MAX_OPERATOR_LABEL_CHARS`](crate::MAX_OPERATOR_LABEL_CHARS) at the point of binding
    /// rather than at the point of use: an id a tool would later refuse as
    /// over-long is a deployment that authenticates and cannot write, and
    /// the refusal would surface on a write instead of at the boundary that
    /// produced it.
    ///
    /// # Errors
    ///
    /// [`TrustedModelIdError`] when the value is blank after trimming or
    /// longer than [`MAX_OPERATOR_LABEL_CHARS`](crate::MAX_OPERATOR_LABEL_CHARS).
    pub fn with_trusted_model_id(
        mut self,
        trusted_model_id: impl Into<String>,
    ) -> Result<Self, TrustedModelIdError> {
        self.identity.trusted_model_id =
            Some(crate::catalog::validate_trusted_model_id(trusted_model_id)?);
        Ok(self)
    }

    /// Bind `CloudEvents` extension attributes onto every Fact this context
    /// goes on to capture — the orchestration context a run happens under
    /// (a workflow run, a step, a causal root), recorded without the code
    /// producing the payload assembling anything.
    ///
    /// **For hosts only.** The values must come from the host's own
    /// knowledge of the run it is executing — never from a payload, a tool
    /// argument, a request header, or MCP `clientInfo`. A transport that
    /// lets a caller reach this builder has published a forgeable
    /// provenance field, exactly as it would by exposing
    /// [`Self::with_trusted_model_id`].
    ///
    /// Additive, deliberately: a second call MERGES, and a name either side
    /// already bound is an error. There is no "clear it" and no "overwrite
    /// it" call, so no later step in a builder chain can quietly drop or
    /// restate context an earlier one established.
    ///
    /// Validated here, at the point of binding, rather than at the point of
    /// capture: an attribute a consumer would refuse is a deployment that
    /// authenticates and cannot write, and the refusal belongs at the
    /// boundary that produced it.
    ///
    /// # Errors
    ///
    /// [`PublicationExtensionsError`] for an illegal, reserved, over-long
    /// or already-bound name, an illegal value, or a union over
    /// [`MAX_PUBLICATION_EXTENSIONS`](crate::publication::MAX_PUBLICATION_EXTENSIONS).
    pub fn with_publication_extensions(
        mut self,
        extensions: PublicationExtensions,
    ) -> Result<Self, PublicationExtensionsError> {
        self.publication_extensions =
            std::mem::take(&mut self.publication_extensions).merged(extensions)?;
        Ok(self)
    }

    /// Bind the memories this session read before it writes (#677): every
    /// memory this context writes keeps them as `read` pins.
    ///
    /// **For hosts only**, from the host's own call context — never from a
    /// tool argument, request body or header a caller controls. Readability
    /// is not checked here. Storage checks each local read in its named
    /// space inside the write's own transaction. A write tool call reads
    /// each other-store read through the other-stores port as the caller
    /// before the tool runs, and storage refuses one nobody read that way
    /// (a direct engine write). Either refusal refuses the whole write.
    ///
    /// Readability is judged by this context's owner scope as it stands at
    /// the first bind: a later narrowing to the write space (a write always
    /// narrows) does not shrink what the session could read. Bind the set on
    /// the context the host authenticated.
    ///
    /// Additive like [`Self::with_publication_extensions`]: a second call
    /// merges, and there is no call that clears the set.
    ///
    /// # Errors
    ///
    /// [`crate::ReadSetError::ConflictingSpace`] when the merged set names
    /// one memory in two spaces; [`crate::ReadSetError::TooMany`] when it
    /// passes [`crate::ReadSet::MAX_PINS`].
    pub fn with_read_set(mut self, reads: crate::ReadSet) -> Result<Self, crate::ReadSetError> {
        let reader = self
            .read_set
            .reader
            .take()
            .or_else(|| self.owner_scope().cloned().map(Box::new));
        let reads = std::mem::take(&mut self.read_set.reads).merged(reads)?;
        let other_store = std::mem::take(&mut self.read_set.other_store);
        self.read_set = BoundReadSet {
            reads,
            reader,
            other_store,
        };
        Ok(self)
    }

    /// Name the owner a request that selects none acts for — the MCP edge
    /// uses it when a client sends neither `Mcp-Session-Id` nor
    /// `X-Proxiima-Owner`, as a native OAuth client does.
    ///
    /// **For authenticators only**, and only for an owner this context
    /// already carries a role for: the default selects, it never grants.
    /// Selection preserves this context's effective roles. Use
    /// [`Self::narrowed_to_owner`] to confine authority explicitly. A Group
    /// only an [`OwnerAccessPort`] would answer is not carried and cannot be
    /// the default.
    ///
    /// `None` when the context carries no role for `owner`.
    #[must_use]
    pub fn with_default_owner(mut self, owner: OwnerId) -> Option<Self> {
        self.role_for_owner(&owner)?;
        self.default_owner = Some(owner);
        Some(self)
    }

    /// The registered tool whose handler received this context, or `None`
    /// outside a tool call. Set only by the tool registration's call
    /// wrapper; no argument, header or host builder reaches it.
    #[must_use]
    pub const fn invoking_tool(&self) -> Option<InvokingTool> {
        self.invoking_tool
    }

    /// Stamp the tool whose handler this context is handed to.
    #[must_use]
    pub(crate) const fn invoked_by_tool(mut self, tool: InvokingTool) -> Self {
        self.invoking_tool = Some(tool);
        self
    }

    /// Narrow to `owner`. The result is bound to that one owner, so it
    /// names no default.
    #[must_use]
    pub fn narrowed_to_owner(mut self, owner: OwnerId) -> Option<Self> {
        self.owner_scope = self.owner_scope.narrowed(owner)?;
        self.default_owner = None;
        Some(self)
    }

    /// The same context with one more host-resolved Group role in its map —
    /// the entry an [`OwnerAccessPort`]
    /// answered on demand instead of in the eager enumeration. There is
    /// still exactly one way to narrow: [`Self::narrowed_to_owner`] reads
    /// this map, so a role folded in here is narrowed on under the same
    /// rules as every eagerly resolved one. Typed on [`GroupId`] because a
    /// Personal role is a kernel rule, never a resolver's answer.
    ///
    /// `None` once authentication sealed the context, and for a confined or
    /// forwarded one.
    /// The accessible set is recomputed from the map the same way
    /// [`Self::server_resolved`] computes it.
    ///
    /// `role` MUST come from an `OwnerAccessPort` resolution. A role taken
    /// from a request header, a tool argument, or any other
    /// caller-controlled payload would make the caller its own authorizer.
    #[must_use]
    pub fn with_host_resolved_role(mut self, group: GroupId, role: Role) -> Option<Self> {
        let OwnerScopeState::Unsealed(grant) = self.owner_scope else {
            return None;
        };
        if grant.confined_to().is_some() || self.forwarded_owner.is_some() {
            return None;
        }
        self.owner_scope = OwnerScopeState::Unsealed(grant.with_group_role(group, role));
        Some(self)
    }

    /// Resolve one missing group through the trusted host port while keeping
    /// the authenticated subject private to this operation.
    pub async fn with_authenticated_group_role(
        &self,
        port: &dyn OwnerAccessPort,
        group: GroupId,
    ) -> Option<Self> {
        if self.forwarded_owner.is_some() {
            return None;
        }
        let scope = self.owner_scope()?;
        if scope.is_expired() {
            return None;
        }
        let subject = self.subject()?;
        let role = port.resolve_group_role(subject, group).await.ok()??;
        let scope = scope.add_group_role(group, role)?;
        let mut next = self.clone();
        next.owner_scope = OwnerScopeState::Sealed(scope);
        Some(next)
    }

    #[must_use]
    pub fn with_expires_at(mut self, expires_at: Option<SystemTime>) -> Self {
        self.owner_scope = self.owner_scope.clamped_to(expires_at);
        self
    }

    /// # Panics
    ///
    /// Panics only if constructing the empty subject role set fails.
    #[must_use]
    pub fn for_subject(subject: UserId, auth_path: AuthPath) -> Self {
        Self::server_resolved(OwnerRoles::for_subject(subject, []).unwrap(), auth_path)
    }

    /// # Panics
    ///
    /// Panics if `roles` contains invalid owner-role overrides.
    #[must_use]
    pub fn for_subject_with_role<I>(subject: UserId, roles: I, auth_path: AuthPath) -> Self
    where
        I: IntoIterator<Item = (OwnerId, crate::access::Role)>,
    {
        Self::server_resolved(OwnerRoles::for_subject(subject, roles).unwrap(), auth_path)
    }

    /// Compatibility helper for already-owner-scoped trusted surfaces: the
    /// owner is read as the subject's own id, which holds the personal role.
    /// Access to any other owner must be represented by an explicit subject
    /// role.
    #[must_use]
    pub fn single_owner(owner: &OwnerId, auth_path: AuthPath) -> Self {
        Self::for_subject(owner.as_user(), auth_path)
    }
}

/// The host contract: validate credential material, return an
/// authenticated context.
#[async_trait]
pub trait Authenticator: Send + Sync {
    /// # Errors
    ///
    /// `AuthError::AuthRequired` when credentials are missing,
    /// `AuthError::InvalidCredentials` when they are malformed,
    /// expired, or revoked.
    async fn authenticate(&self, creds: &Credentials) -> Result<AuthzContext, AuthError>;
}

/// Authenticate through the host-owned verifier and mint the sole production
/// [`OwnerScope`] witness from its server-resolved result.
///
/// # Errors
///
/// Returns the host authentication error, or `InvalidCredentials` when the
/// host returned a context without an attested writer.
pub async fn authenticate(
    authenticator: &dyn Authenticator,
    credentials: &Credentials,
) -> Result<AuthzContext, AuthError> {
    AuthzContext::authenticate(authenticator, credentials).await
}

/// Revalidation cadence for long-lived authenticated streams.
///
/// Streams terminate silently at the context's expiry when present, and
/// after `max_stream_lifetime` regardless of identity state. Nothing ends a
/// stream earlier: there is no revocation before expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevalidationConfig {
    pub max_stream_lifetime: Duration,
}

impl Default for RevalidationConfig {
    fn default() -> Self {
        Self {
            max_stream_lifetime: Duration::from_hours(1),
        }
    }
}

struct RevalidatedStream<I> {
    inner: Pin<Box<dyn Stream<Item = I> + Send>>,
    expires_at: Option<Pin<Box<Sleep>>>,
    max_lifetime: Pin<Box<Sleep>>,
    closing: bool,
    closed: bool,
}

impl<I> RevalidatedStream<I> {
    fn observe_termination(&mut self, cx: &mut Context<'_>) -> bool {
        if self.max_lifetime.as_mut().poll(cx).is_ready() {
            return true;
        }

        self.expires_at
            .as_mut()
            .is_some_and(|expires_at| expires_at.as_mut().poll(cx).is_ready())
    }

    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<Option<I>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl<I> Stream for RevalidatedStream<I> {
    type Item = I;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.closed {
            return Poll::Ready(None);
        }

        if self.closing {
            return match self.poll_inner(cx) {
                Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
                Poll::Ready(None) | Poll::Pending => {
                    self.closed = true;
                    Poll::Ready(None)
                }
            };
        }

        match self.poll_inner(cx) {
            Poll::Ready(Some(item)) => {
                self.closing = self.observe_termination(cx);
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => {
                self.closed = true;
                Poll::Ready(None)
            }
            Poll::Pending => {
                if self.observe_termination(cx) {
                    self.closed = true;
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

/// Wrap a long-lived stream with expiry and max-lifetime termination.
/// Termination is a silent stream end.
pub fn revalidate_stream<S>(
    stream: S,
    expires_at: Option<SystemTime>,
    config: RevalidationConfig,
) -> Pin<Box<dyn Stream<Item = S::Item> + Send>>
where
    S: Stream + Send + 'static,
    S::Item: Send + 'static,
{
    let expires_at = expires_at.map(system_time_sleep);
    let max_lifetime = Box::pin(tokio::time::sleep(config.max_stream_lifetime));

    Box::pin(RevalidatedStream {
        inner: Box::pin(stream),
        expires_at,
        max_lifetime,
        closing: false,
        closed: false,
    })
}

fn system_time_sleep(deadline: SystemTime) -> Pin<Box<Sleep>> {
    let until_deadline = deadline
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO);
    Box::pin(tokio::time::sleep_until(Instant::now() + until_deadline))
}

#[cfg(test)]
#[path = "authz_tests.rs"]
mod tests;
