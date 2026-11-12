//! Fresh authority for an edit to one existing Goal, without owner-role widening.

use proxiima_core::goal_collaboration::GoalEditPermit;
use proxiima_core::verbs::goal_write::GoalEvidenceRef;
use proxiima_core::{
    AccessKind, EdgeEndpoint, EntityKind, GoalCommandPermit, GoalId, MemoryId, OwnerId, OwnerRoles,
    OwnerScope, Role, StorageError,
};
use sqlx::{Postgres, Transaction};

use crate::ports::goal_collaboration::source_reads::SourceReads;
use crate::ports::goal_collaboration::{
    current_goal_role, current_role, fenced_roles, membership_roles,
};
use crate::{PgStorage, error::map_err, pg_enums::PgMemoryKind};

pub(crate) struct GoalEditFence<'a> {
    command: &'a GoalCommandPermit,
    prior: GoalId,
    roles: OwnerRoles,
    sources: SourceReads,
    endpoints: Vec<EdgeEndpoint>,
}

impl GoalEditFence<'_> {
    pub(crate) async fn revalidate(
        &self,
        storage: &PgStorage,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<(), StorageError> {
        let generation: Option<i64> = sqlx::query_scalar(
            "SELECT h.authority_generation FROM proxiima_core.goal_head h
             JOIN proxiima_core.goal g ON g.handle=h.handle AND g.owner_id=h.owner_id
             WHERE h.handle=$1 AND h.owner_id=$2 AND g.t=$3",
        )
        .bind(self.command.goal().into_inner())
        .bind(self.command.controller().into_inner())
        .bind(self.prior.into_inner())
        .fetch_optional(&mut **tx)
        .await
        .map_err(map_err)?;
        let generation = generation.ok_or(StorageError::NotFound)?;
        let role = current_goal_role(tx, self.command, &self.roles, generation)
            .await?
            .ok_or(StorageError::NotFound)?;
        if !role.works_goals() {
            return Err(StorageError::Forbidden("Goal work required".into()));
        }
        self.sources
            .revalidate(
                storage,
                tx,
                self.command.scope(),
                &self.roles,
                &self.endpoints,
            )
            .await
    }
}

/// What a Goal work write names besides its owner: its prior Goal and
/// dependencies, its new evidence, motivations and author Perspective, and
/// the prior whose evidence it carries.
pub(crate) struct GoalWorkSources<'a> {
    pub(crate) goals: Vec<GoalId>,
    pub(crate) evidence: &'a [GoalEvidenceRef],
    pub(crate) memories: Vec<MemoryId>,
    pub(crate) carried_from: Option<GoalId>,
}

/// Begin one Goal work write on `owner` (set, modify, transition). A scope
/// that administers `owner` keeps its owner transaction. Goal work
/// by membership alone ([`Role::works_goals`]) writes under platform scope,
/// as a Goal route does: every named source is first read under the
/// caller's scope, then the owner fences of `owner` and every source owner
/// are held, the subject's membership is resolved again in this
/// transaction, and the sources are read again against it. A member
/// removed or demoted since authentication works no Goal, and no source the
/// caller cannot read enters the write.
pub(crate) async fn begin_goal_work(
    storage: &PgStorage,
    scope: &OwnerScope,
    owner: OwnerId,
    named: &GoalWorkSources<'_>,
) -> Result<Transaction<'static, Postgres>, StorageError> {
    // An owner transaction registers the owner row, which takes a Fact write
    // limit; an administrator without one writes under platform scope too.
    if scope.goal_work() != Some(owner)
        || (scope.administers(&owner) && scope.may_write(&owner, AccessKind::Fact))
    {
        return crate::owner_scope::begin_owner_transaction(&storage.pool, scope).await;
    }
    let mut tx = storage.platform_transaction().await?;
    crate::owner_scope::bind_owner_scope(&mut tx, scope).await?;
    let mut memories = named.memories.clone();
    if let Some(prior) = named.carried_from {
        memories.extend(carried_local_evidence(&mut tx, owner, prior, storage.store_number).await?);
    }
    let mut endpoints = memory_endpoints(&mut tx, named.evidence, &memories).await?;
    endpoints.extend(named.goals.iter().map(|goal| EdgeEndpoint::goal(*goal)));
    let sources = SourceReads::discover(storage, &mut tx, scope, &endpoints).await?;
    let owners: Vec<_> = sources.owners().chain([owner]).collect();
    let roles = membership_roles(storage, &mut tx, scope, &owners, &[]).await?;
    if !current_role(scope, &roles, owner).is_some_and(Role::works_goals) {
        return Err(StorageError::Forbidden(
            "Goal work requires membership".into(),
        ));
    }
    sources
        .revalidate(storage, &mut tx, scope, &roles, &endpoints)
        .await?;
    Ok(tx)
}

/// The local evidence a modify carries from `prior`, read under the
/// caller's bound scope.
async fn carried_local_evidence(
    tx: &mut Transaction<'_, Postgres>,
    owner: OwnerId,
    prior: GoalId,
    store_number: u16,
) -> Result<Vec<MemoryId>, StorageError> {
    let ids: Option<Vec<uuid::Uuid>> = sqlx::query_scalar(
        "SELECT evidence_t FROM proxiima_core.goal WHERE t = $1 AND owner_id = $2",
    )
    .bind(prior.into_inner())
    .bind(owner.into_inner())
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_err)?
    .ok_or(StorageError::NotFound)?;
    Ok(ids
        .unwrap_or_default()
        .into_iter()
        .filter(|id| proxiima_core::memory_store_number(*id) == store_number)
        .map(MemoryId::new)
        .collect())
}

/// The named local memories with their kinds, read under the caller's
/// bound scope: a memory it cannot read is not found.
async fn memory_endpoints(
    tx: &mut Transaction<'_, Postgres>,
    evidence: &[GoalEvidenceRef],
    memories: &[MemoryId],
) -> Result<Vec<EdgeEndpoint>, StorageError> {
    let local: Vec<_> = evidence
        .iter()
        .filter(|item| item.remote_origin().is_none())
        .map(|item| item.memory_id().into_inner())
        .chain(memories.iter().map(|memory| memory.into_inner()))
        .collect();
    let rows: Vec<(uuid::Uuid, PgMemoryKind)> = sqlx::query_as(
        "SELECT t,kind FROM proxiima_core.live_memory
         WHERE t=ANY($1::uuid[])",
    )
    .bind(&local)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_err)?;
    if local
        .iter()
        .any(|id| !rows.iter().any(|(found, _)| found == id))
    {
        return Err(StorageError::NotFound);
    }
    Ok(rows
        .into_iter()
        .map(|(id, kind)| EdgeEndpoint::memory(EntityKind::from(kind), MemoryId::new(id)))
        .collect())
}

/// Select the existing owner transaction or fence a single Goal route. New
/// source statements are admitted under the original caller RLS before any
/// platform materialization; carried prior statements are not new reads.
pub(crate) async fn begin_goal_edit<'a>(
    storage: &PgStorage,
    permit: GoalEditPermit<'a>,
    prior: GoalId,
    evidence: &[GoalEvidenceRef],
    motivated_by: &[MemoryId],
    carried_from: Option<GoalId>,
    author_self: Option<MemoryId>,
) -> Result<(Transaction<'static, Postgres>, Option<GoalEditFence<'a>>), StorageError> {
    let GoalEditPermit::Goal(command) = permit else {
        let named = GoalWorkSources {
            goals: vec![prior],
            evidence,
            memories: motivated_by.iter().copied().chain(author_self).collect(),
            carried_from,
        };
        return Ok((
            begin_goal_work(
                storage,
                crate::owner_scope::require_owner_scope(permit.scope())?,
                permit.owner(),
                &named,
            )
            .await?,
            None,
        ));
    };
    let mut tx = storage.platform_transaction().await?;
    crate::owner_scope::bind_owner_scope(&mut tx, command.scope()).await?;
    let local: Vec<_> = evidence
        .iter()
        .filter(|item| item.remote_origin().is_none())
        .map(|item| item.memory_id().into_inner())
        .collect();
    let rows: Vec<(uuid::Uuid, PgMemoryKind)> = sqlx::query_as(
        "SELECT t,kind FROM proxiima_core.live_memory
         WHERE t=ANY($1::uuid[])",
    )
    .bind(&local)
    .fetch_all(&mut *tx)
    .await
    .map_err(map_err)?;
    if local
        .iter()
        .any(|id| !rows.iter().any(|(found, _)| found == id))
    {
        return Err(StorageError::NotFound);
    }
    let mut endpoints: Vec<_> = rows
        .into_iter()
        .map(|(id, kind)| EdgeEndpoint::memory(EntityKind::from(kind), MemoryId::new(id)))
        .collect();
    endpoints.push(EdgeEndpoint::goal(prior));
    if let Some(id) = author_self {
        endpoints.push(EdgeEndpoint::memory(EntityKind::Perspective, id));
    }
    let sources = SourceReads::discover(storage, &mut tx, command.scope(), &endpoints).await?;
    let owners: Vec<_> = sources.owners().chain([command.controller()]).collect();
    let roles = fenced_roles(storage, &mut tx, command.scope(), &owners, &[]).await?;
    sources
        .lock_goals(&mut tx, (command.goal(), command.controller()))
        .await?;
    let fence = GoalEditFence {
        command,
        prior,
        roles,
        sources,
        endpoints,
    };
    fence.revalidate(storage, &mut tx).await?;
    Ok((tx, Some(fence)))
}

pub(crate) async fn revalidate_goal_edit(
    storage: &PgStorage,
    tx: &mut Transaction<'_, Postgres>,
    fence: Option<&GoalEditFence<'_>>,
) -> Result<(), StorageError> {
    if let Some(fence) = fence {
        fence.revalidate(storage, tx).await?;
    }
    Ok(())
}
