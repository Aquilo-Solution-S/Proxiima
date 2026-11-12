use crate::MemoryKind;
use crate::OwnerId;
use crate::access::EntityId;
use crate::authz::{
    AuthPath, AuthzContext, AuthzInput, AuthzOperation, AuthzOutcome, EngineAuthority,
};
use crate::error::ProtocolError;
use crate::storage::StorageError;
use crate::storage_ports::{OwnerWritePermit, WriteAuthority};

use super::Engine;

/// Proof that the resolved owner passed a write gate. Sealed: only this
/// module's authorization gates can mint it.
#[derive(Debug)]
pub struct WritePermit {
    owner_write: OwnerWritePermit,
}

impl WritePermit {
    pub(in crate::engine) fn authorize_transfer_to(&mut self, destination: OwnerId) {
        self.owner_write.authorize_transfer_to(destination);
    }

    #[must_use]
    pub fn owner(&self) -> &OwnerId {
        self.owner_write.owner()
    }

    #[must_use]
    pub const fn owner_write_permit(&self) -> &OwnerWritePermit {
        &self.owner_write
    }

    pub(in crate::engine) fn into_owner_write(self) -> OwnerWritePermit {
        self.owner_write
    }

    #[must_use]
    pub fn owner_scope(&self) -> Option<&crate::OwnerScope> {
        self.owner_write.owner_scope()
    }

    /// Test-only write permit. The gates in this module remain the
    /// production mint; see
    /// [`crate::verbs::fact_ingest::AuthorizedFactWrite::new_for_tests`].
    #[cfg(any(test, feature = "testkit"))]
    #[must_use]
    pub fn for_tests(owner_write: OwnerWritePermit) -> Self {
        Self { owner_write }
    }

    #[cfg(test)]
    pub(crate) fn expire_delegated_write_for_test(&mut self) {
        self.owner_write.expire_delegated_for_test();
    }
}

/// The uniform answer [`Engine::authorize_entry_read`] gives for both a
/// nonexistent entity and one the caller may not see — existence is not
/// disclosed to non-readers. Read verbs that want to present the case as
/// a not-found (rather than a forbidden) match on this constant.
pub(in crate::engine) const ENTRY_NOT_FOUND_MESSAGE: &str = "entry not found";

/// Proof that one entry passed the read-scope predicate. Sealed: only this
/// module's authorization gates can mint it.
#[derive(Debug)]
pub struct EntryReadPermit {
    owner: OwnerId,
    kind: crate::EntityKind,
    schema_id: Option<crate::SchemaId>,
}

impl EntryReadPermit {
    #[must_use]
    pub fn owner(&self) -> &OwnerId {
        &self.owner
    }

    pub(in crate::engine) const fn kind(&self) -> crate::EntityKind {
        self.kind
    }

    pub(in crate::engine) fn schema_id(&self) -> Option<&crate::SchemaId> {
        self.schema_id.as_ref()
    }
}

impl Engine {
    /// Storage-tier owner-write gate for `kind`. A `System` context must
    /// come from [`AuthzContext::for_system`] with this engine's
    /// [`crate::SystemAuthority`].
    ///
    /// # Errors
    ///
    /// Returns `Forbidden` when `authz` cannot write `kind` on `owner`.
    #[allow(
        clippy::unused_async,
        reason = "keeps the owner-write gate on the async Engine authorization seam"
    )]
    pub async fn authorize_owner_write(
        &self,
        authz: &AuthzContext,
        owner: &OwnerId,
        kind: MemoryKind,
    ) -> Result<OwnerWritePermit, ProtocolError> {
        Ok(self
            .write_permit(authz, owner, WriteAuthority::Memory(kind))?
            .owner_write)
    }

    /// Write gate: the rule for `kind` on `owner`.
    #[allow(
        clippy::unused_async,
        reason = "keeps the write gate on the async Engine authorization seam"
    )]
    pub(in crate::engine) async fn authorize_write<A>(
        &self,
        authority: &A,
        owner: &OwnerId,
        kind: MemoryKind,
    ) -> Result<WritePermit, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        self.write_permit(authority, owner, WriteAuthority::Memory(kind))
    }

    /// Goal work gate ([`Role::works_goals`]): set, modify and transition a
    /// Goal of `owner`. The permit's scope names `owner` as its Goal work
    /// and carries none of its memory writes.
    #[allow(
        clippy::unused_async,
        reason = "keeps the Goal work gate on the async Engine authorization seam"
    )]
    pub(in crate::engine) async fn authorize_goal_work<A>(
        &self,
        authority: &A,
        owner: &OwnerId,
    ) -> Result<WritePermit, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        self.write_permit(authority, owner, WriteAuthority::GoalWork)
            .map(|permit| WritePermit {
                owner_write: permit.owner_write.for_goal_work(),
            })
    }

    /// Write gate for rows whose kind only storage reads (forget,
    /// hydration, embedding backfill): the permit carries the caller's write limit on `owner`,
    /// and storage answers `NotFound` for a row above it.
    #[allow(
        clippy::unused_async,
        reason = "keeps the write gate on the async Engine authorization seam"
    )]
    pub(in crate::engine) async fn authorize_write_limit<A>(
        &self,
        authority: &A,
        owner: &OwnerId,
    ) -> Result<WritePermit, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let kind = self
            .operation_authority(authority)?
            .authz()
            .role_for_owner(owner)
            .and_then(|role| role.write_ceiling().top())
            .unwrap_or(MemoryKind::Fact);
        self.write_permit(authority, owner, WriteAuthority::Memory(kind))
    }

    /// The owner-admin gate ([`Role::administers`]): membership admin,
    /// transfer, erase, the graph overview, and achieving, decomposing and
    /// sharing the owner's Goals.
    #[allow(
        clippy::unused_async,
        reason = "keeps the owner-admin gate on the async Engine authorization seam"
    )]
    pub(in crate::engine) async fn authorize_owner_admin<A>(
        &self,
        authority: &A,
        owner: &OwnerId,
    ) -> Result<WritePermit, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        self.write_permit(authority, owner, WriteAuthority::Administration)
    }

    /// One permit per admitted need: the gate judges the operation that
    /// names `need`, and the permit carries that same need to storage.
    fn write_permit<A>(
        &self,
        authority: &A,
        owner: &OwnerId,
        need: WriteAuthority,
    ) -> Result<WritePermit, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let authority = self.operation_authority(authority)?;
        let authz = authority.authz();
        let operation = match need {
            WriteAuthority::Memory(kind) => AuthzOperation::Write { kind },
            WriteAuthority::GoalWork => AuthzOperation::GoalWork,
            WriteAuthority::Administration => AuthzOperation::OwnerAdmin,
        };
        let resolved = self.gate(authz, owner, operation, authority.redeemed_phase(), allows)?;
        let owner_write = if authority.redeemed_phase() {
            let expires_at = authz.expires_at().ok_or_else(|| {
                ProtocolError::forbidden("delegated worker phase has no finite expiry")
            })?;
            OwnerWritePermit::new_delegated(
                resolved,
                need,
                self.delegation_runtime_binding.clone(),
                expires_at,
                authz.owner_scope().cloned(),
                authz.bound_reads(),
            )
        } else {
            OwnerWritePermit::new(
                resolved,
                need,
                authz.owner_scope().cloned(),
                authz.bound_reads(),
            )
        };
        Ok(WritePermit { owner_write })
    }

    /// Read gate for one owner: the rule for `kind`. Returns the resolved
    /// owner.
    #[allow(
        clippy::unused_async,
        reason = "keeps the read gate on the async Engine authorization seam"
    )]
    pub(in crate::engine) async fn authorize_owner_read<A>(
        &self,
        authority: &A,
        owner: &OwnerId,
        kind: MemoryKind,
    ) -> Result<OwnerId, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let authority = self.operation_authority(authority)?;
        self.gate(
            authority.authz(),
            owner,
            AuthzOperation::Read { kind },
            authority.redeemed_phase(),
            allows,
        )
    }

    /// The one owner gate: resolve `owner`, apply the rule for `operation`,
    /// run the vetoes, and report the outcome to the observers.
    fn gate(
        &self,
        authz: &AuthzContext,
        owner: &OwnerId,
        operation: AuthzOperation,
        redeemed_phase: bool,
        admits: Admits,
    ) -> Result<OwnerId, ProtocolError> {
        if authz.auth_path() == AuthPath::Delegated && !redeemed_phase {
            return Err(ProtocolError::forbidden(
                "raw delegated authorization contexts are not Engine authority",
            ));
        }
        let refuse = |resolved: &OwnerId, outcome, err| {
            let input = AuthzInput {
                authz,
                requested: owner,
                resolved,
                operation: operation.clone(),
            };
            self.registry.run_authorization_observers(&input, outcome);
            Err(err)
        };
        let writes = !matches!(operation, AuthzOperation::Read { .. });
        if writes
            && authz.auth_path() == AuthPath::System
            && !authz.carries_system_authority(&self.system_authority_binding)
        {
            return refuse(
                owner,
                AuthzOutcome::DeniedGrant,
                ProtocolError::forbidden(
                    "System write authority requires a context from AuthzContext::for_system",
                ),
            );
        }
        if authz.auth_path() == AuthPath::Denied {
            return refuse(
                owner,
                AuthzOutcome::DeniedResolution,
                ProtocolError::forbidden("denied context authorizes nothing"),
            );
        }
        let resolved = match self.registry.resolve_owner(authz, owner) {
            Ok(resolved) => resolved,
            Err(err) => return refuse(owner, AuthzOutcome::DeniedResolution, err),
        };
        if !admits(authz, &resolved, &operation) {
            return refuse(&resolved, AuthzOutcome::DeniedGrant, denied(&operation));
        }
        let input = AuthzInput {
            authz,
            requested: owner,
            resolved: &resolved,
            operation,
        };
        if let Err(err) = self.registry.run_authorization_vetoes(&input) {
            self.registry
                .run_authorization_observers(&input, AuthzOutcome::DeniedVeto);
            return Err(err);
        }
        self.registry
            .run_authorization_observers(&input, AuthzOutcome::Allowed);
        Ok(resolved)
    }

    /// Every owner this context may read anything of, minus the owners a
    /// host veto denies. Storage returns only the rows whose kind each
    /// owner's read limit covers.
    ///
    /// Each owner in the raw read set is gated once, as `Read { kind: Fact }`
    /// ("any read on this owner"; storage still applies the per-kind limits),
    /// and the observers get one decision per owner, denials included. A
    /// vetoed owner silently drops out: the returned set can be empty, and an
    /// empty set is not an error, because an error would reveal the veto.
    /// Callers turn an empty set into empty results or not-found.
    ///
    /// # Errors
    ///
    /// `Forbidden` only when the context itself authorizes nothing (a denied
    /// context, or no readable owner at all).
    #[allow(
        clippy::unused_async,
        reason = "keeps the shared read gate source-compatible with async Engine callers"
    )]
    pub(in crate::engine) async fn authorize_read<A>(
        &self,
        authority: &A,
    ) -> Result<Vec<OwnerId>, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let operation = self.operation_authority(authority)?;
        let authz = operation.authz();
        let read = self.readable_set(authz)?;
        Ok(read
            .into_iter()
            .filter(|owner| self.veto_read_owner(authz, owner, MemoryKind::Fact).is_ok())
            .collect())
    }

    /// Candidate owners for row reads. Goal candidates only narrow a scan;
    /// the authenticated transaction still checks each exact row. They never
    /// enter the caller's owner roles or the public owner-authority read set.
    pub(in crate::engine) async fn authorize_row_read<A>(
        &self,
        authority: &A,
    ) -> Result<Vec<OwnerId>, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let operation = self.operation_authority(authority)?;
        let authz = operation.authz();
        let candidates = self.row_read_candidates(authz).await?;
        Ok(candidates
            .into_iter()
            .filter(|owner| self.veto_read_owner(authz, owner, MemoryKind::Fact).is_ok())
            .collect())
    }

    async fn row_read_candidates(
        &self,
        authz: &AuthzContext,
    ) -> Result<Vec<OwnerId>, ProtocolError> {
        let mut candidates = self.readable_set(authz)?;
        if let Some(scope) = authz
            .owner_scope()
            .filter(|scope| scope.goal_principal().is_some())
        {
            let goal_candidates = self
                .storage()
                .pipeline
                .goal_collaboration
                .read_owner_candidates(scope)
                .await
                .map_err(|err| storage_error("goal_read_candidates", &err))?;
            candidates.extend(goal_candidates);
            candidates.sort_unstable();
            candidates.dedup();
        }
        Ok(candidates)
    }

    /// The unvetoed read set: every owner this context may read anything of.
    /// A context that reads nothing is refused here, with one
    /// `DeniedResolution` decision for its principal.
    fn readable_set(&self, authz: &AuthzContext) -> Result<Vec<OwnerId>, ProtocolError> {
        let read = if authz.auth_path() == AuthPath::Denied {
            Vec::new()
        } else {
            authz.readable_owners(MemoryKind::Fact)
        };
        if read.is_empty() {
            let principal = authz.principal();
            let input = AuthzInput {
                authz,
                requested: &principal,
                resolved: &principal,
                operation: AuthzOperation::Read {
                    kind: MemoryKind::Fact,
                },
            };
            self.registry
                .run_authorization_observers(&input, AuthzOutcome::DeniedResolution);
            return Err(ProtocolError::forbidden(
                "denied context authorizes nothing",
            ));
        }
        Ok(read)
    }

    /// [`Self::veto_read`] for a memory read of `kind`.
    fn veto_read_owner(
        &self,
        authz: &AuthzContext,
        owner: &OwnerId,
        kind: MemoryKind,
    ) -> Result<(), ProtocolError> {
        self.veto_read(authz, owner, AuthzOperation::Read { kind })
    }

    /// Run the host vetoes for a read `operation` on one stored owner and
    /// report the decision to the observers.
    ///
    /// `owner` is already a concrete, stored owner (from the read set or from
    /// storage), so it is not re-resolved: a resolver could remap the gate
    /// onto a different owner than the one storage filters on.
    fn veto_read(
        &self,
        authz: &AuthzContext,
        owner: &OwnerId,
        operation: AuthzOperation,
    ) -> Result<(), ProtocolError> {
        let input = AuthzInput {
            authz,
            requested: owner,
            resolved: owner,
            operation,
        };
        if let Err(err) = self.registry.run_authorization_vetoes(&input) {
            self.registry
                .run_authorization_observers(&input, AuthzOutcome::DeniedVeto);
            return Err(err);
        }
        self.registry
            .run_authorization_observers(&input, AuthzOutcome::Allowed);
        Ok(())
    }

    /// Single-entry read gate. Existence is not disclosed to non-readers: a
    /// missing entity, one whose kind the caller may not read, and one whose
    /// home owner a host veto denies are all [`ENTRY_NOT_FOUND_MESSAGE`].
    ///
    /// Only the one owner that matters is gated: storage finds the entity's
    /// single home owner within the unvetoed read set, and that owner is then
    /// vetoed with the entity's real kind. One observer decision per entry.
    pub(in crate::engine) async fn authorize_entry_read<A>(
        &self,
        authority: &A,
        entity: EntityId,
    ) -> Result<EntryReadPermit, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        let mut permits = self.authorize_entry_reads(authority, &[entity]).await?;
        Ok(permits.remove(0))
    }

    /// [`Self::authorize_entry_read`] for each of `entities`, in order, with
    /// one storage lookup for all of them. Each entry gets its own decision;
    /// the first refused entry is the error and the entries after it are not
    /// decided.
    pub(in crate::engine) async fn authorize_entry_reads<A>(
        &self,
        authority: &A,
        entities: &[EntityId],
    ) -> Result<Vec<EntryReadPermit>, ProtocolError>
    where
        A: EngineAuthority + ?Sized,
    {
        if entities.is_empty() {
            return Ok(Vec::new());
        }
        let operation = self.operation_authority(authority)?;
        let authz = operation.authz();
        let read = self.row_read_candidates(authz).await?;
        let homes = self
            .storage()
            .pipeline
            .owner_access_read
            .visible_entries(authz.owner_scope(), entities, &read)
            .await
            .map_err(|err| storage_error("visible_home_owner", &err))?;
        homes
            .into_iter()
            .map(|home| {
                let entry = home
                    .filter(|entry| match entry.route {
                        crate::storage_ports::EntryReadRoute::Owner => {
                            authz.may_read_entry(&entry.owner, entry.kind)
                        }
                        crate::storage_ports::EntryReadRoute::Goal => authz
                            .owner_scope()
                            .is_some_and(|scope| scope.goal_principal().is_some()),
                    })
                    .ok_or_else(|| ProtocolError::forbidden(ENTRY_NOT_FOUND_MESSAGE))?;
                let (owner, kind) = (entry.owner, entry.kind);
                let operation = match MemoryKind::of(kind) {
                    Some(kind) => AuthzOperation::Read { kind },
                    None => AuthzOperation::GoalRead,
                };
                self.veto_read(authz, &owner, operation)
                    .map_err(|_| ProtocolError::forbidden(ENTRY_NOT_FOUND_MESSAGE))?;
                Ok(EntryReadPermit {
                    owner,
                    kind,
                    schema_id: entry.schema,
                })
            })
            .collect()
    }
}

/// The rule that admits an operation on a resolved owner.
type Admits = fn(&AuthzContext, &OwnerId, &AuthzOperation) -> bool;

fn allows(authz: &AuthzContext, owner: &OwnerId, operation: &AuthzOperation) -> bool {
    match *operation {
        AuthzOperation::Read { kind } => authz.may_read(owner, kind),
        AuthzOperation::GoalRead => authz.may_read_goals(owner),
        AuthzOperation::Write { kind } => authz.may_write(owner, kind),
        AuthzOperation::GoalWork => authz.may_work_goals(owner),
        AuthzOperation::OwnerAdmin => authz.may_administer(owner),
        AuthzOperation::Membership { .. }
        | AuthzOperation::EntityTransfer { .. }
        | AuthzOperation::GoalCollaboration { .. } => false,
    }
}

fn denied(operation: &AuthzOperation) -> ProtocolError {
    ProtocolError::forbidden(match operation {
        AuthzOperation::Read { kind } => format!("requires {kind:?} read on this owner"),
        AuthzOperation::GoalRead => "requires Goal read on this owner".to_owned(),
        AuthzOperation::Write { kind } => format!("requires {kind:?} write on this owner"),
        AuthzOperation::GoalWork => "requires Goal work on this owner".to_owned(),
        AuthzOperation::OwnerAdmin
        | AuthzOperation::Membership { .. }
        | AuthzOperation::EntityTransfer { .. }
        | AuthzOperation::GoalCollaboration { .. } => "requires admin on this owner".to_owned(),
    })
}

fn storage_error(context: &str, err: &StorageError) -> ProtocolError {
    ProtocolError::internal(format!("{context}: {err}"))
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;
