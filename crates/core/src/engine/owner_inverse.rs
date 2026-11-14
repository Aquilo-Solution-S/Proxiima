//! Engine owner erase methods — abandonment-only.

use super::Engine;
use crate::authz::{AuthPath, AuthzContext};
use crate::error::ProtocolError;
use crate::owner_inverse::{
    EraseAuthorization, ExportAuthorization, OwnerEraseContext, OwnerEraseOutcome,
    OwnerEraseRefusal, OwnerEraseTarget, OwnerExportBundle, OwnerExportContext, OwnerExportTarget,
    OwnerSurfaces,
};
use crate::storage_ports::{OperatorMaintenanceProof, OwnerEraseAuthorityHandle};
use crate::{EmbeddingAnnObservability, EmbeddingOrphanSweepOutcome};
use crate::{GroupId, SourceId, UserId};

/// The verdict of [`Engine::admit_erase`].
enum EraseAdmission {
    /// Authorized: the token storage requires before it will delete.
    Admitted(EraseAuthorization),
    /// Turned away, and already recorded against the operation id the caller
    /// is handed back.
    Refused(OwnerEraseOutcome),
}

/// The answer to an authority question, or the port that must give it.
enum Authority<'a> {
    Settled(bool),
    Ask(&'a OwnerEraseAuthorityHandle),
}

impl Engine {
    /// The one place the owner-inverse lanes learn which tables exist.
    ///
    /// Every leg, including `owner_pinned`, comes off the spiice contracts —
    /// via `TransferRule::RetainAtSource` for that one — so the Postgres
    /// adapter's `pg_sidecar!(owner_pinned: true)` flag is a second source
    /// core can see, and it is checked against this one when the sidecar
    /// registry freezes.
    pub(crate) fn owner_surfaces(&self) -> OwnerSurfaces {
        OwnerSurfaces::for_registry(&self.registry)
    }

    fn erase_context(authz: &AuthzContext, target: OwnerEraseTarget) -> OwnerEraseContext {
        OwnerEraseContext::new(
            uuid::Uuid::now_v7(),
            target,
            authz.subject(),
            authz.auth_path(),
            time::OffsetDateTime::now_utc(),
        )
    }

    fn export_context(authz: &AuthzContext, target: OwnerExportTarget) -> OwnerExportContext {
        OwnerExportContext::new(
            uuid::Uuid::now_v7(),
            target,
            authz.subject(),
            authz.auth_path(),
            time::OffsetDateTime::now_utc(),
        )
    }

    /// What an authority question comes to before any port is asked: a delegated caller is
    /// refused, a system caller is granted, and a store with no authority port refuses. Erase,
    /// export and operator maintenance share it, so they cannot order these differently.
    fn authority_for(&self, authz: &AuthzContext) -> Authority<'_> {
        match authz.auth_path() {
            AuthPath::Delegated => Authority::Settled(false),
            AuthPath::System => Authority::Settled(true),
            AuthPath::HostBearer => match &self.storage.owner_inverse.erase_authority {
                Some(port) => Authority::Ask(port),
                None => Authority::Settled(false),
            },
        }
    }

    /// Whether the authority port grants this erase.
    ///
    /// A port FAILURE is not a verdict. Collapsing it into `false` published a
    /// denial the port never issued: the caller got `Unauthorized` — which
    /// reads as permanent and nobody retries — and the audit row asserted an
    /// authorization decision that was never actually made. An unreachable
    /// port is `internal`, which is retryable and true.
    ///
    /// # Errors
    ///
    /// Returns internal when the authority port cannot answer.
    pub(in crate::engine) async fn erase_authority_grants(
        &self,
        authz: &AuthzContext,
        target: &OwnerEraseTarget,
    ) -> Result<bool, ProtocolError> {
        let port = match self.authority_for(authz) {
            Authority::Settled(verdict) => return Ok(verdict),
            Authority::Ask(port) => port,
        };
        port.may_erase_owner(authz, target)
            .await
            .map_err(|e| ProtocolError::internal(format!("may_erase_owner: {e}")))
    }

    /// Whether the authority port grants this export. Same seam, same rule as
    /// [`Self::erase_authority_grants`]: a failure to ask is not a `no`.
    ///
    /// # Errors
    ///
    /// Returns internal when the authority port cannot answer.
    async fn export_authority_grants(
        &self,
        authz: &AuthzContext,
        target: &OwnerExportTarget,
    ) -> Result<bool, ProtocolError> {
        let port = match self.authority_for(authz) {
            Authority::Settled(verdict) => return Ok(verdict),
            Authority::Ask(port) => port,
        };
        port.may_export_owner(authz, target)
            .await
            .map_err(|e| ProtocolError::internal(format!("may_export_owner: {e}")))
    }

    /// Whether the authority port grants operator maintenance. Same seam, same
    /// rule as [`Self::erase_authority_grants`].
    ///
    /// # Errors
    ///
    /// Returns internal when the authority port cannot answer.
    async fn operator_maintenance_authorized(
        &self,
        authz: &AuthzContext,
    ) -> Result<bool, ProtocolError> {
        let port = match self.authority_for(authz) {
            Authority::Settled(verdict) => return Ok(verdict),
            Authority::Ask(port) => port,
        };
        port.may_perform_operator_maintenance(authz)
            .await
            .map_err(|e| ProtocolError::internal(format!("may_perform_operator_maintenance: {e}")))
    }

    /// Owner-agnostic embedding ANN health signals.
    ///
    /// Requires system or owner-erase or operator-maintenance authority.
    ///
    /// # Errors
    ///
    /// Returns forbidden when the caller lacks operator authority, or internal
    /// when the storage read fails.
    pub async fn embedding_ann_observability(
        &self,
        authz: &AuthzContext,
    ) -> Result<EmbeddingAnnObservability, ProtocolError> {
        if !self.operator_maintenance_authorized(authz).await? {
            return Err(ProtocolError::forbidden(
                "embedding ANN observability requires operator maintenance authorization",
            ));
        }
        self.storage
            .owner_inverse
            .embedding_maintenance
            .embedding_ann_observability(
                self.embedding_runtime_policy(),
                OperatorMaintenanceProof::new(),
            )
            .await
            .map_err(|e| ProtocolError::internal(format!("embedding_ann_observability: {e}")))
    }

    /// Sweep orphaned embedding infrastructure rows.
    ///
    /// This is crash-residue maintenance only. Owner erase remains
    /// synchronous and cannot rely on this sweep for lawful wipe semantics.
    ///
    /// # Errors
    ///
    /// Returns forbidden when the caller lacks operator authority, or internal
    /// when the storage sweep fails.
    pub async fn sweep_orphan_embedding_rows(
        &self,
        authz: &AuthzContext,
    ) -> Result<EmbeddingOrphanSweepOutcome, ProtocolError> {
        if !self.operator_maintenance_authorized(authz).await? {
            return Err(ProtocolError::forbidden(
                "embedding orphan sweep requires operator maintenance authorization",
            ));
        }
        self.storage
            .owner_inverse
            .embedding_maintenance
            .sweep_orphan_embedding_rows(OperatorMaintenanceProof::new())
            .await
            .map_err(|e| ProtocolError::internal(format!("sweep_orphan_embedding_rows: {e}")))
    }

    async fn verify_personal_owner_drop(
        &self,
        user_id: UserId,
        drop_event_id: &str,
    ) -> Result<bool, OwnerEraseRefusal> {
        let Some(port) = &self.storage.owner_inverse.owner_drop_proof else {
            return Err(OwnerEraseRefusal::DropProofPortUnavailable);
        };
        match port
            .verify_personal_owner_dropped(user_id, drop_event_id)
            .await
        {
            Ok(true) => Ok(true),
            // "the port says the owner still exists" and "the port could not
            // be asked" are different facts, and the refusal enum already
            // carries both. Reporting an outage as `PersonalDropNotVerified`
            // hands the operator a receipt reading "the user is still there",
            // so a lawful erase is dropped instead of retried.
            Ok(false) => Err(OwnerEraseRefusal::PersonalDropNotVerified),
            Err(error) => {
                tracing::warn!(%error, "owner drop proof could not be verified");
                Err(OwnerEraseRefusal::DropProofPortUnavailable)
            }
        }
    }

    /// The front half of every owner erase: open the audit row, check
    /// owner-erase authority, and — when the target names a drop
    /// event — verify it before storage can receive a deletion token.
    ///
    /// Both refusals carry the operation id the caller is handed back, so an
    /// attempt that deleted nothing is as identifiable as one that deleted
    /// everything. Core writes no trail of either: the receipt goes to the
    /// host, which owns whatever record its own promises require.
    ///
    /// `drop_event` is `Some` exactly for the two personal targets. A group
    /// owner declares no drop event; its abandonment is what storage rechecks
    /// under lock instead.
    async fn admit_erase(
        &self,
        authz: &AuthzContext,
        target: OwnerEraseTarget,
        drop_event: Option<(UserId, &str)>,
    ) -> Result<EraseAdmission, ProtocolError> {
        let audit = Self::erase_context(authz, target.clone());
        let operation_id = audit.operation_id();

        if !self.erase_authority_grants(authz, &target).await? {
            return Ok(EraseAdmission::Refused(OwnerEraseOutcome::Unauthorized {
                operation_id,
            }));
        }

        let drop_proof_verified = drop_event.is_some();
        if let Some((user_id, drop_event_id)) = drop_event
            && let Err(reason) = self
                .verify_personal_owner_drop(user_id, drop_event_id)
                .await
        {
            return Ok(EraseAdmission::Refused(OwnerEraseOutcome::Refused {
                operation_id,
                reason,
            }));
        }

        Ok(EraseAdmission::Admitted(EraseAuthorization::new(
            audit,
            drop_proof_verified,
        )))
    }

    /// Export one owner's owner bundle.
    ///
    /// Requires owner-erase authority. Export is
    /// non-destructive: personal-owner drop proof does not gate it.
    ///
    /// # Errors
    ///
    /// Returns forbidden when the caller lacks owner-erase authority,
    /// or internal when storage export fails.
    pub async fn export_owner_bundle(
        &self,
        authz: &AuthzContext,
        target: OwnerExportTarget,
    ) -> Result<OwnerExportBundle, ProtocolError> {
        if !self.export_authority_grants(authz, &target).await? {
            return Err(ProtocolError::forbidden(
                "owner export requires owner-erase authority",
            ));
        }

        let auth = ExportAuthorization::new(Self::export_context(authz, target));
        let surfaces = self.owner_surfaces();
        self.storage
            .owner_inverse
            .owner_erase
            .export_owner_bundle(&auth, &surfaces)
            .await
            .map_err(|e| ProtocolError::internal(format!("export_owner_bundle: {e}")))
    }

    /// Erase an abandoned group owner and all its owned rows.
    ///
    /// Requires `OwnerEraseAuthorityPort` approval or [`AuthPath::System`].
    /// Storage rechecks abandonment under lock before deleting.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when audit recording or storage execution fails.
    pub async fn erase_group_owner(
        &self,
        authz: &AuthzContext,
        group_id: GroupId,
    ) -> Result<OwnerEraseOutcome, ProtocolError> {
        let auth = match self
            .admit_erase(authz, OwnerEraseTarget::GroupOwner { group_id }, None)
            .await?
        {
            EraseAdmission::Admitted(auth) => auth,
            EraseAdmission::Refused(outcome) => return Ok(outcome),
        };
        let surfaces = self.owner_surfaces();
        self.storage
            .owner_inverse
            .owner_erase
            .erase_group_owner(&auth, group_id, &surfaces)
            .await
            .map_err(|e| ProtocolError::internal(format!("erase_group_owner: {e}")))
    }

    /// Erase a dropped personal owner and all its owned rows.
    ///
    /// Requires `OwnerEraseAuthorityPort`/system authorization and trusted host
    /// drop proof before storage can receive a deletion token.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when audit recording, drop-proof access, or storage execution fails.
    pub async fn erase_personal_owner(
        &self,
        authz: &AuthzContext,
        user_id: UserId,
        drop_event_id: String,
    ) -> Result<OwnerEraseOutcome, ProtocolError> {
        let target = OwnerEraseTarget::PersonalOwner {
            user_id,
            drop_event_id: drop_event_id.clone(),
        };
        let auth = match self
            .admit_erase(authz, target, Some((user_id, &drop_event_id)))
            .await?
        {
            EraseAdmission::Admitted(auth) => auth,
            EraseAdmission::Refused(outcome) => return Ok(outcome),
        };
        let surfaces = self.owner_surfaces();
        self.storage
            .owner_inverse
            .owner_erase
            .erase_personal_owner(&auth, user_id, &surfaces)
            .await
            .map_err(|e| ProtocolError::internal(format!("erase_personal_owner: {e}")))
    }

    /// Erase one source scope for an abandoned group owner.
    /// Storage rechecks abandonment under lock before deleting.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when audit recording or storage execution fails.
    pub async fn erase_group_source_scope(
        &self,
        authz: &AuthzContext,
        group_id: GroupId,
        source_id: SourceId,
    ) -> Result<OwnerEraseOutcome, ProtocolError> {
        let target = OwnerEraseTarget::GroupSourceScope {
            group_id,
            source_id: source_id.clone(),
        };
        let auth = match self.admit_erase(authz, target, None).await? {
            EraseAdmission::Admitted(auth) => auth,
            EraseAdmission::Refused(outcome) => return Ok(outcome),
        };
        let surfaces = self.owner_surfaces();
        // A source scope is a partial owner: the storage verb enqueues only
        // the object keys its own selection orphans, and reports the debt on
        // `cold_object_purge_pending` like every other scope.
        self.storage
            .owner_inverse
            .owner_erase
            .erase_group_source_scope(&auth, group_id, &source_id, &surfaces)
            .await
            .map_err(|e| ProtocolError::internal(format!("erase_group_source_scope: {e}")))
    }

    /// Erase one source scope for a dropped personal owner.
    ///
    /// # Errors
    ///
    /// Returns a protocol error when audit recording, drop-proof access, or storage execution fails.
    pub async fn erase_personal_source_scope(
        &self,
        authz: &AuthzContext,
        user_id: UserId,
        source_id: SourceId,
        drop_event_id: String,
    ) -> Result<OwnerEraseOutcome, ProtocolError> {
        let target = OwnerEraseTarget::PersonalSourceScope {
            user_id,
            source_id: source_id.clone(),
            drop_event_id: drop_event_id.clone(),
        };
        let auth = match self
            .admit_erase(authz, target, Some((user_id, &drop_event_id)))
            .await?
        {
            EraseAdmission::Admitted(auth) => auth,
            EraseAdmission::Refused(outcome) => return Ok(outcome),
        };
        let surfaces = self.owner_surfaces();
        // See `erase_group_source_scope`: the selection decides the debt.
        self.storage
            .owner_inverse
            .owner_erase
            .erase_personal_source_scope(&auth, user_id, &source_id, &surfaces)
            .await
            .map_err(|e| ProtocolError::internal(format!("erase_personal_source_scope: {e}")))
    }
}

#[cfg(test)]
#[path = "owner_inverse_purge_tests.rs"]
mod purge_tests;
