//! Authenticated, immutable owner scope.
//!
//! `OwnerScope` is deliberately opaque. Production instances are sealed from
//! verified roles by [`crate::authenticate`] or the boot-authority-gated
//! [`crate::AuthzContext::for_system`] and
//! [`crate::AuthzContext::for_insecure_single_owner`] paths. Callers can only
//! inspect or narrow the resulting witness.

use std::time::SystemTime;

use crate::{GroupId, MemoryKind, OwnerId, OwnerRoles, Role, UserId, WriterAttribution};

/// The roles a request holds, until when, and the one owner it is confined
/// to. The single owner of all three: a context carries one grant, sealed or
/// not, so no site can update one copy and forget another.
///
/// Every question about roles is answered here and none is answered once
/// [`Self::is_expired`]; that is the only comparison with the clock for
/// request authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeGrant {
    roles: OwnerRoles,
    expires_at: Option<SystemTime>,
    selected: Option<OwnerId>,
}

impl ScopeGrant {
    pub(crate) const fn new(roles: OwnerRoles, expires_at: Option<SystemTime>) -> Self {
        Self {
            roles,
            expires_at,
            selected: None,
        }
    }

    pub(crate) const fn expires_at(&self) -> Option<SystemTime> {
        self.expires_at
    }

    pub(crate) fn is_expired(&self) -> bool {
        self.expires_at
            .is_some_and(|deadline| deadline <= SystemTime::now())
    }

    /// The deadline moves earlier or stays: an expiry never extends, and
    /// `None` keeps the one already set.
    pub(crate) fn clamped_to(mut self, expires_at: Option<SystemTime>) -> Self {
        self.expires_at = self.expires_at.into_iter().chain(expires_at).min();
        self
    }

    pub(crate) const fn confined_to(&self) -> Option<OwnerId> {
        self.selected
    }

    pub(crate) const fn subject(&self) -> UserId {
        self.roles.subject()
    }

    pub(crate) fn with_group_role(mut self, group: GroupId, role: Role) -> Self {
        self.roles = self.roles.with_group_role(group, role);
        self
    }

    pub(crate) fn narrow(&self, owner: OwnerId) -> Option<Self> {
        let role = self.role_for(&owner)?;
        Some(Self {
            roles: OwnerRoles::scoped_to(self.roles.subject(), owner, role),
            expires_at: self.expires_at,
            selected: Some(owner),
        })
    }

    /// The roles while the grant lives, `None` once it expired: the only way
    /// a question about access reaches them.
    fn live(&self) -> Option<&OwnerRoles> {
        (!self.is_expired()).then_some(&self.roles)
    }

    pub(crate) fn role_for(&self, owner: &OwnerId) -> Option<Role> {
        self.live()?.role_for(owner)
    }

    pub(crate) fn may_read(&self, owner: &OwnerId, kind: MemoryKind) -> bool {
        self.live().is_some_and(|roles| roles.may_read(owner, kind))
    }

    pub(crate) fn may_write(&self, owner: &OwnerId, kind: MemoryKind) -> bool {
        self.live()
            .is_some_and(|roles| roles.may_write(owner, kind))
    }

    pub(crate) fn may_manage(&self, owner: &OwnerId) -> bool {
        self.live().is_some_and(|roles| roles.may_manage(owner))
    }

    pub(crate) fn administers(&self, owner: &OwnerId) -> bool {
        self.live().is_some_and(|roles| roles.administers(owner))
    }

    pub(crate) fn readable_owners(&self, kind: MemoryKind) -> Vec<OwnerId> {
        self.live()
            .map_or_else(Vec::new, |roles| roles.readable_owners(kind))
    }

    pub(crate) fn writable_owners(&self, kind: MemoryKind) -> Vec<OwnerId> {
        self.live()
            .map_or_else(Vec::new, |roles| roles.writable_owners(kind))
    }

    pub(crate) fn goal_reading_owners(&self) -> Vec<OwnerId> {
        self.live()
            .map_or_else(Vec::new, OwnerRoles::goal_reading_owners)
    }

    pub(crate) fn goal_working_owners(&self) -> Vec<OwnerId> {
        self.live()
            .map_or_else(Vec::new, OwnerRoles::goal_working_owners)
    }
}

/// Server-authenticated owner authority carried alongside an [`AuthzContext`](crate::AuthzContext).
///
/// The fields are private and there is no public constructor.  In particular,
/// owner headers and ordinary `AuthzContext` compatibility constructors cannot
/// produce this witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerScope {
    grant: ScopeGrant,
    writer: WriterAttribution,
    goal_principal: Option<crate::goal_collaboration::GoalPrincipalScope>,
    /// The owner a Goal work permit works ([`Self::goal_work`]).
    goal_work: Option<OwnerId>,
}

impl OwnerScope {
    pub(crate) fn from_verified_roles(
        roles: OwnerRoles,
        expires_at: Option<SystemTime>,
        writer: WriterAttribution,
    ) -> Self {
        Self::seal(ScopeGrant::new(roles, expires_at), writer)
    }

    pub(crate) const fn seal(grant: ScopeGrant, writer: WriterAttribution) -> Self {
        Self {
            grant,
            writer,
            goal_principal: None,
            goal_work: None,
        }
    }

    pub(crate) const fn grant(&self) -> &ScopeGrant {
        &self.grant
    }

    pub(crate) fn into_grant(self) -> ScopeGrant {
        self.grant
    }

    /// The same scope with its deadline moved earlier or kept
    /// ([`ScopeGrant::clamped_to`]).
    pub(crate) fn clamped_to(self, expires_at: Option<SystemTime>) -> Self {
        let grant = self.grant.clone().clamped_to(expires_at);
        self.with_grant(grant)
    }

    pub(crate) fn with_grant(self, grant: ScopeGrant) -> Self {
        Self { grant, ..self }
    }

    pub(crate) fn add_group_role(&self, group: GroupId, role: Role) -> Option<Self> {
        // On-demand host resolution belongs before request narrowing. A
        // narrowed witness can never grow its authority again.
        if self.grant.confined_to().is_some() || self.is_expired() {
            return None;
        }
        Some(Self {
            grant: self.grant.clone().with_group_role(group, role),
            writer: self.writer,
            goal_principal: self.goal_principal,
            goal_work: None,
        })
    }

    pub(crate) fn narrow(&self, owner: OwnerId) -> Option<Self> {
        Some(Self {
            grant: self.grant.narrow(owner)?,
            writer: self.writer,
            goal_principal: None,
            goal_work: None,
        })
    }

    /// The same scope, working `owner`'s Goals when its role
    /// [works Goals](Role::works_goals). Only the Goal work gate mints it,
    /// into the permit of one set, modify or transition.
    pub(crate) fn for_goal_work(&self, owner: OwnerId) -> Self {
        Self {
            goal_work: self
                .grant
                .role_for(&owner)
                .is_some_and(Role::works_goals)
                .then_some(owner),
            ..self.clone()
        }
    }

    /// Add a verified Goal principal. Only the authentication engine may mint
    /// this lane; it is never derived from owner roles or a forwarded scope.
    pub(crate) fn with_goal_principal(
        mut self,
        principal: crate::goal_collaboration::GoalPrincipalScope,
    ) -> Self {
        if self.grant.confined_to().is_none() {
            self.goal_principal = Some(principal);
        }
        self
    }

    #[must_use]
    pub fn goal_principal(&self) -> Option<&crate::goal_collaboration::GoalPrincipalScope> {
        if self.is_expired() {
            None
        } else {
            self.goal_principal.as_ref()
        }
    }

    /// The immutable writer captured when authentication sealed this scope.
    ///
    /// ```
    /// # use proxiima_core::{OwnerScope, WriterAttribution};
    /// fn inspect(scope: &OwnerScope) -> WriterAttribution {
    ///     scope.writer()
    /// }
    /// ```
    ///
    /// ```compile_fail
    /// # use proxiima_core::{OwnerScope, WriterAttribution};
    /// # fn replace(scope: &mut OwnerScope, writer: WriterAttribution) {
    /// scope.writer = writer;
    /// # }
    /// ```
    #[must_use]
    pub const fn writer(&self) -> WriterAttribution {
        self.writer
    }

    #[must_use]
    pub const fn subject(&self) -> UserId {
        self.grant.subject()
    }

    #[must_use]
    pub const fn expires_at(&self) -> Option<SystemTime> {
        self.grant.expires_at()
    }

    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.grant.is_expired()
    }

    #[must_use]
    pub fn role_for_owner(&self, owner: &OwnerId) -> Option<Role> {
        self.grant.role_for(owner)
    }

    #[must_use]
    pub fn may_read(&self, owner: &OwnerId, kind: MemoryKind) -> bool {
        self.grant.may_read(owner, kind)
    }

    #[must_use]
    pub fn may_write(&self, owner: &OwnerId, kind: MemoryKind) -> bool {
        self.grant.may_write(owner, kind)
    }

    /// Whether the scope reads an entry of `kind` owned by `owner`
    /// ([`Role::may_read_entry`]).
    #[must_use]
    pub fn may_read_entry(&self, owner: &OwnerId, kind: crate::EntityKind) -> bool {
        self.grant
            .role_for(owner)
            .is_some_and(|role| role.may_read_entry(kind))
    }

    /// Whether the scope administers `owner` ([`Role::administers`]).
    #[must_use]
    pub fn administers(&self, owner: &OwnerId) -> bool {
        self.grant.administers(owner)
    }

    /// Owners whose Goals the scope reads ([`Role::reads_goals`]).
    #[must_use]
    pub fn goal_reading_owners(&self) -> Vec<OwnerId> {
        self.grant.goal_reading_owners()
    }

    /// Owners whose Goals the scope works ([`Role::works_goals`]).
    #[must_use]
    pub fn goal_working_owners(&self) -> Vec<OwnerId> {
        self.grant.goal_working_owners()
    }

    #[must_use]
    pub fn readable_owners(&self, kind: MemoryKind) -> Vec<OwnerId> {
        self.grant.readable_owners(kind)
    }

    #[must_use]
    pub fn managed_owners(&self) -> Vec<OwnerId> {
        self.readable_owners(MemoryKind::Fact)
            .into_iter()
            .filter(|owner| self.role_for_owner(owner).is_some_and(Role::manages))
            .collect()
    }

    #[must_use]
    pub fn writable_owners(&self, kind: MemoryKind) -> Vec<OwnerId> {
        self.grant.writable_owners(kind)
    }

    /// The owner whose Goals a Goal work permit's scope works
    /// ([`Role::works_goals`]); storage writes them fenced on that role,
    /// never through the roles' memory write.
    #[must_use]
    pub fn goal_work(&self) -> Option<OwnerId> {
        self.goal_work.filter(|_| !self.is_expired())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrowing_cannot_restore_personal_or_add_another_group() {
        let subject = UserId::new(uuid::Uuid::now_v7());
        let group = GroupId::new(uuid::Uuid::now_v7());
        let other = GroupId::new(uuid::Uuid::now_v7());
        let roles =
            OwnerRoles::for_subject(subject, [(OwnerId::from(group), Role::editor())]).unwrap();
        let writer = WriterAttribution {
            by: subject.into_inner(),
            via: Some(uuid::Uuid::now_v7()),
        };
        let original = OwnerScope::from_verified_roles(roles, None, writer);
        let narrowed = original.narrow(OwnerId::from(group)).unwrap();
        assert!(narrowed.narrow(OwnerId::from(subject)).is_none());
        assert!(narrowed.add_group_role(other, Role::admin()).is_none());
        assert!(narrowed.narrow(OwnerId::from(group)).is_some());
        assert!(original.narrow(OwnerId::from(subject)).is_some());
        assert_eq!(narrowed.writer(), writer);
        assert_eq!(
            original
                .add_group_role(other, Role::viewer())
                .unwrap()
                .writer(),
            writer
        );
    }
}
