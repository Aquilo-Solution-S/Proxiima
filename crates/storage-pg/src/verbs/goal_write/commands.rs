use super::authority::{begin_goal_edit, revalidate_goal_edit};
use super::{
    AchieveGoalAtomicRequest, CreateGoalAtomicRequest, DecomposeGoalAtomicRequest,
    DecomposeGoalOutcome, DecomposedGoalOutcome, DraftFromPayload, GoalAuthorship, GoalChange,
    GoalId, GoalState, GoalWriteOutcome, LifecycleWrite, MemoryId, ModifyGoalAtomicRequest,
    OwnerWritePermit, PgPool, PgSidecarRegistryFrozen, PostLockGuard, PreparedGoalInsert,
    ReplayDeclaration, RowColumns, StorageError, SystemOrigin, TransitionGoalAtomicRequest,
    achieve_replay_declaration, carried_evidence, child_draft, create_replay_declaration,
    decompose_replay_declarations, draft_from_payload, draft_from_stored, insert_or_replay_goal,
    internal, lifecycle_outcome, load_prior_goal, lock_prepared_goal_writes,
    modify_replay_declaration, persist_prepared_goal_insert, prepare_goal_insert,
    record_goal_replay_declaration, require_goal_replay, resolve_decompose_replay_set,
    resolve_goal_replay, transition_replay_declaration, validate_active_head,
    validate_evidence_in_owner, validate_goal_achievement, validate_goal_transition,
};
use crate::PgStorage;
use crate::error::with_bounded_retry;
use proxiima_core::goal_collaboration::GoalEditPermit;

// The goal `*_atomic` verbs are pool-scoped write transactions. Each wraps
// its `_in_pool` body in `with_bounded_retry` so a transient deadlock /
// serialization failure (SQLSTATE 40P01/40001) re-runs the whole idempotent
// transaction instead of surfacing to the host as a bare `Internal`.

pub(crate) async fn create_goal_atomic(
    storage: &PgStorage,
    req: &CreateGoalAtomicRequest<'_>,
    permit: &OwnerWritePermit,
    store_number: u16,
) -> Result<GoalWriteOutcome, StorageError> {
    with_bounded_retry(move || async move {
        create_goal_atomic_in_pool(storage, req, permit, store_number).await
    })
    .await
}

pub(crate) async fn transition_goal_atomic(
    storage: &PgStorage,
    req: &TransitionGoalAtomicRequest<'_>,
    permit: &GoalEditPermit<'_>,
    store_number: u16,
) -> Result<GoalWriteOutcome, StorageError> {
    with_bounded_retry(move || async move {
        transition_goal_atomic_in_pool(storage, req, permit, store_number).await
    })
    .await
}

pub(crate) async fn achieve_goal_atomic(
    storage: &PgStorage,
    req: &AchieveGoalAtomicRequest<'_>,
    permit: &GoalEditPermit<'_>,
    store_number: u16,
) -> Result<GoalWriteOutcome, StorageError> {
    with_bounded_retry(move || async move {
        achieve_goal_atomic_in_pool(storage, req, permit, store_number).await
    })
    .await
}

pub(crate) async fn modify_goal_atomic(
    storage: &PgStorage,
    req: &ModifyGoalAtomicRequest<'_>,
    permit: &GoalEditPermit<'_>,
    store_number: u16,
) -> Result<GoalWriteOutcome, StorageError> {
    with_bounded_retry(move || async move {
        modify_goal_atomic_in_pool(storage, req, permit, store_number).await
    })
    .await
}

pub(crate) async fn decompose_goal_atomic(
    pool: &PgPool,
    sidecars: &PgSidecarRegistryFrozen,
    req: &DecomposeGoalAtomicRequest<'_>,
    permit: &OwnerWritePermit,
    store_number: u16,
) -> Result<DecomposeGoalOutcome, StorageError> {
    with_bounded_retry(move || async move {
        decompose_goal_atomic_in_pool(pool, sidecars, req, permit, store_number).await
    })
    .await
}

pub(crate) async fn create_goal_atomic_in_pool(
    storage: &PgStorage,
    req: &CreateGoalAtomicRequest<'_>,
    permit: &OwnerWritePermit,
    store_number: u16,
) -> Result<GoalWriteOutcome, StorageError> {
    let named = super::authority::GoalWorkSources {
        goals: draft_dependency_ids(&req.draft),
        evidence: req.draft.topology.evidence(),
        memories: draft_motivation_ids(&req.draft)
            .into_iter()
            .chain(req.context.author_self_perspective_id)
            .collect(),
        carried_from: None,
    };
    let mut tx = super::authority::begin_goal_work(
        storage,
        crate::owner_scope::require_owner_scope(permit.owner_scope())?,
        *permit.owner(),
        &named,
    )
    .await?;
    let outcome = create_goal_in_tx(&mut tx, &storage.sidecars, req, permit, store_number).await?;
    crate::write_wake::commit(tx).await?;
    Ok(outcome)
}

pub(crate) async fn create_goal_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sidecars: &PgSidecarRegistryFrozen,
    req: &CreateGoalAtomicRequest<'_>,
    permit: &OwnerWritePermit,
    store_number: u16,
) -> Result<GoalWriteOutcome, StorageError> {
    let writer = crate::owner_scope::require_owner_scope(permit.owner_scope())?.writer();
    let owner = req.draft.owner();
    let declaration = create_replay_declaration(req)?;
    if let Some(replay) =
        resolve_goal_replay(tx.as_mut(), &owner, &req.draft.request_id, &declaration).await?
    {
        return Ok(replay);
    }
    let evidence = validate_evidence_in_owner(tx, &owner, req.draft.topology.evidence()).await?;
    let inserted = insert_or_replay_goal(
        tx,
        sidecars,
        &req.draft,
        None,
        GoalChange::Set,
        req.context,
        RowColumns {
            store_number,
            writer,
            text: Some(&req.draft.text),
            write_act_t: req.write_act_t.map(proxiima_core::MemoryId::into_inner),
        },
        PostLockGuard::None,
    )
    .await?;
    if inserted.idempotent_replay {
        return require_goal_replay(tx.as_mut(), &owner, &req.draft.request_id, &declaration).await;
    }
    let dependencies = draft_dependency_ids(&req.draft);
    let motivated_by = draft_motivation_ids(&req.draft);
    let outcome = lifecycle_outcome(
        tx,
        LifecycleWrite {
            owner: &owner,
            inserted,
            motivated_by: &motivated_by,
            dependencies: &dependencies,
            evidence: &evidence,
        },
    )
    .await?;
    record_goal_replay_declaration(tx.as_mut(), &declaration, &outcome).await?;
    Ok(outcome)
}

pub(crate) async fn transition_goal_atomic_in_pool(
    storage: &PgStorage,
    req: &TransitionGoalAtomicRequest<'_>,
    permit: &GoalEditPermit<'_>,
    store_number: u16,
) -> Result<GoalWriteOutcome, StorageError> {
    // Constant-time command validation stays ahead of the replay probe: this
    // rejection ran before every prior write, so no stored declaration can
    // ever carry a shape it refuses, and probing for one is pure cost.
    // (`Achieved` is not a `GoalTransitionKind`, so a transition to it is
    // unrepresentable and needs no screen.)
    if req.authorship == GoalAuthorship::System(SystemOrigin::Operator) {
        return Err(StorageError::ConstraintViolation(
            "operator-authored Goal transition is not supported".into(),
        ));
    }
    let scope = crate::owner_scope::require_owner_scope(permit.scope())?;
    let writer = scope.writer();
    let sidecars = &storage.sidecars;
    let (mut tx, fence) = begin_goal_edit(
        storage,
        *permit,
        req.prior_goal_id,
        &[],
        &[],
        None,
        req.context.author_self_perspective_id,
    )
    .await?;
    let declaration = transition_replay_declaration(req);
    if let Some(replay) = resolve_goal_replay(
        tx.as_mut(),
        &req.owner,
        req.request_id.as_str(),
        &declaration,
    )
    .await?
    {
        revalidate_goal_edit(storage, &mut tx, fence.as_ref()).await?;
        crate::write_wake::commit(tx).await?;
        return Ok(replay);
    }
    let prior =
        load_prior_goal(&mut tx, &req.owner, req.prior_goal_id, req.context.registry).await?;
    let next_state = req.transition.next_state();
    validate_goal_transition(prior.state, next_state)?;
    let draft = draft_from_stored(
        &req.owner,
        &prior,
        next_state,
        Some(req.prior_goal_id),
        req.authorship.clone(),
        req.request_id.as_str(),
        // A plain transition rests on nothing it names: no evidence column,
        // and so no evidence reference rows.
        &[],
    );
    let inserted = insert_or_replay_goal(
        &mut tx,
        sidecars,
        &draft,
        Some(req.prior_goal_id),
        GoalChange::from(req.transition),
        req.context,
        // A lifecycle write declares no text: the version keeps the prior
        // one's, as stored, unknown included.
        RowColumns {
            store_number,
            writer,
            text: prior.text.as_deref(),
            write_act_t: None,
        },
        // Abandon refuses while a decompose child is open, checked after the
        // lifecycle lock on the parent head t, which a decompose child also
        // takes through its dependency_t, whatever the prior (Active or Paused).
        // Pause and resume keep the goal open.
        if req.transition == proxiima_core::verbs::goal_write::GoalTransitionKind::Abandon {
            PostLockGuard::NoOpenChildren
        } else {
            PostLockGuard::None
        },
    )
    .await?;
    if inserted.idempotent_replay {
        let replay = require_goal_replay(
            tx.as_mut(),
            &req.owner,
            req.request_id.as_str(),
            &declaration,
        )
        .await?;
        revalidate_goal_edit(storage, &mut tx, fence.as_ref()).await?;
        crate::write_wake::commit(tx).await?;
        return Ok(replay);
    }
    let outcome = lifecycle_outcome(
        &mut tx,
        LifecycleWrite {
            owner: &req.owner,
            inserted,
            motivated_by: &prior.motivated_by,
            dependencies: &prior.dependencies,
            evidence: &[],
        },
    )
    .await?;
    record_goal_replay_declaration(tx.as_mut(), &declaration, &outcome).await?;
    revalidate_goal_edit(storage, &mut tx, fence.as_ref()).await?;
    crate::write_wake::commit(tx).await?;
    Ok(outcome)
}

pub(crate) async fn achieve_goal_atomic_in_pool(
    storage: &PgStorage,
    req: &AchieveGoalAtomicRequest<'_>,
    permit: &GoalEditPermit<'_>,
    store_number: u16,
) -> Result<GoalWriteOutcome, StorageError> {
    let scope = crate::owner_scope::require_owner_scope(permit.scope())?;
    let writer = scope.writer();
    let sidecars = &storage.sidecars;
    let (mut tx, fence) = begin_goal_edit(
        storage,
        *permit,
        req.prior_goal_id,
        &req.evidence,
        &[],
        None,
        req.context.author_self_perspective_id,
    )
    .await?;
    let declaration = achieve_replay_declaration(req);
    if let Some(replay) = resolve_goal_replay(
        tx.as_mut(),
        &req.owner,
        req.request_id.as_str(),
        &declaration,
    )
    .await?
    {
        revalidate_goal_edit(storage, &mut tx, fence.as_ref()).await?;
        crate::write_wake::commit(tx).await?;
        return Ok(replay);
    }
    let evidence = validate_evidence_in_owner(&mut tx, &req.owner, &req.evidence).await?;
    let prior =
        load_prior_goal(&mut tx, &req.owner, req.prior_goal_id, req.context.registry).await?;
    validate_goal_achievement(prior.state)?;
    let draft = draft_from_stored(
        &req.owner,
        &prior,
        GoalState::Achieved,
        Some(req.prior_goal_id),
        req.authorship.clone(),
        req.request_id.as_str(),
        &evidence,
    );
    let inserted = insert_or_replay_goal(
        &mut tx,
        sidecars,
        &draft,
        Some(req.prior_goal_id),
        GoalChange::Achieve,
        req.context,
        // A lifecycle write declares no text: the version keeps the prior
        // one's, as stored, unknown included.
        RowColumns {
            store_number,
            writer,
            text: prior.text.as_deref(),
            write_act_t: None,
        },
        PostLockGuard::ChildrenClosed,
    )
    .await?;
    if inserted.idempotent_replay {
        let replay = require_goal_replay(
            tx.as_mut(),
            &req.owner,
            req.request_id.as_str(),
            &declaration,
        )
        .await?;
        revalidate_goal_edit(storage, &mut tx, fence.as_ref()).await?;
        crate::write_wake::commit(tx).await?;
        return Ok(replay);
    }
    let outcome = lifecycle_outcome(
        &mut tx,
        LifecycleWrite {
            owner: &req.owner,
            inserted,
            motivated_by: &prior.motivated_by,
            dependencies: &prior.dependencies,
            evidence: &evidence,
        },
    )
    .await?;
    record_goal_replay_declaration(tx.as_mut(), &declaration, &outcome).await?;
    revalidate_goal_edit(storage, &mut tx, fence.as_ref()).await?;
    crate::write_wake::commit(tx).await?;
    Ok(outcome)
}

/// Dependency ids a draft declares, in the shape the topology writer wants.
fn draft_dependency_ids(draft: &proxiima_core::verbs::goal_write::GoalDraft) -> Vec<GoalId> {
    draft
        .topology
        .dependencies()
        .iter()
        .map(|dependency| dependency.goal_id())
        .collect()
}

fn draft_motivation_ids(draft: &proxiima_core::verbs::goal_write::GoalDraft) -> Vec<MemoryId> {
    draft
        .topology
        .motivated_by()
        .iter()
        .map(|motivation| motivation.perspective_id())
        .collect()
}

async fn prepare_modified_goal(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    req: &ModifyGoalAtomicRequest<'_>,
    store_number: u16,
) -> Result<
    (
        proxiima_core::verbs::goal_write::GoalDraft,
        Vec<super::EvidenceTarget>,
    ),
    StorageError,
> {
    let evidence = match &req.evidence {
        Some(evidence) => validate_evidence_in_owner(tx, &req.owner, evidence).await?,
        None => carried_evidence(tx, &req.owner, req.prior_goal_id, store_number).await?,
    };
    let prior = load_prior_goal(tx, &req.owner, req.prior_goal_id, req.context.registry).await?;
    if prior.state != GoalState::Active {
        return Err(StorageError::ConstraintViolation(
            "goal_modify requires an Active prior head".into(),
        ));
    }
    let motivated_by = req.motivated_by.as_ref().map_or_else(
        || prior.motivated_by.clone(),
        |refs| {
            refs.iter()
                .map(|reference| reference.perspective_id())
                .collect()
        },
    );
    let draft = draft_from_payload(DraftFromPayload {
        owner: &req.owner,
        payload: &req.replacement,
        state: GoalState::Active,
        motivated_by,
        dependencies: prior.dependencies,
        supersedes: Some(req.prior_goal_id),
        authorship: req.authorship.clone(),
        request_id: req.request_id.as_str(),
        evidence: &evidence,
    });
    Ok((draft, evidence))
}

pub(crate) async fn modify_goal_atomic_in_pool(
    storage: &PgStorage,
    req: &ModifyGoalAtomicRequest<'_>,
    permit: &GoalEditPermit<'_>,
    store_number: u16,
) -> Result<GoalWriteOutcome, StorageError> {
    if matches!(permit, GoalEditPermit::Goal(_)) && req.motivated_by.is_some() {
        return Err(StorageError::ConstraintViolation(
            "changing Goal motivations requires owner authority".into(),
        ));
    }
    let scope = crate::owner_scope::require_owner_scope(permit.scope())?;
    let writer = scope.writer();
    let sidecars = &storage.sidecars;
    let motivated_by: Vec<_> = req
        .motivated_by
        .iter()
        .flatten()
        .map(|motivation| motivation.perspective_id())
        .collect();
    let (mut tx, fence) = begin_goal_edit(
        storage,
        *permit,
        req.prior_goal_id,
        req.evidence.as_deref().unwrap_or_default(),
        &motivated_by,
        req.evidence.is_none().then_some(req.prior_goal_id),
        req.context.author_self_perspective_id,
    )
    .await?;
    let declaration = modify_replay_declaration(req)?;
    if let Some(replay) = resolve_goal_replay(
        tx.as_mut(),
        &req.owner,
        req.request_id.as_str(),
        &declaration,
    )
    .await?
    {
        revalidate_goal_edit(storage, &mut tx, fence.as_ref()).await?;
        crate::write_wake::commit(tx).await?;
        return Ok(replay);
    }
    let (draft, evidence) = prepare_modified_goal(&mut tx, req, store_number).await?;
    let inserted = insert_or_replay_goal(
        &mut tx,
        sidecars,
        &draft,
        Some(req.prior_goal_id),
        GoalChange::Modify,
        req.context,
        RowColumns {
            store_number,
            writer,
            text: Some(&draft.text),
            write_act_t: None,
        },
        PostLockGuard::None,
    )
    .await?;
    if inserted.idempotent_replay {
        let replay = require_goal_replay(
            tx.as_mut(),
            &req.owner,
            req.request_id.as_str(),
            &declaration,
        )
        .await?;
        revalidate_goal_edit(storage, &mut tx, fence.as_ref()).await?;
        crate::write_wake::commit(tx).await?;
        return Ok(replay);
    }
    let dependencies = draft_dependency_ids(&draft);
    let motivated_by = draft_motivation_ids(&draft);
    let outcome = lifecycle_outcome(
        &mut tx,
        LifecycleWrite {
            owner: &req.owner,
            inserted,
            motivated_by: &motivated_by,
            dependencies: &dependencies,
            evidence: &evidence,
        },
    )
    .await?;
    record_goal_replay_declaration(tx.as_mut(), &declaration, &outcome).await?;
    revalidate_goal_edit(storage, &mut tx, fence.as_ref()).await?;
    crate::write_wake::commit(tx).await?;
    Ok(outcome)
}

pub(crate) async fn decompose_goal_atomic_in_pool(
    pool: &PgPool,
    sidecars: &PgSidecarRegistryFrozen,
    req: &DecomposeGoalAtomicRequest<'_>,
    permit: &OwnerWritePermit,
    store_number: u16,
) -> Result<DecomposeGoalOutcome, StorageError> {
    let scope = crate::owner_scope::require_owner_scope(permit.owner_scope())?;
    let writer = scope.writer();
    let mut tx = crate::owner_scope::begin_owner_transaction(pool, scope).await?;
    let declarations = decompose_replay_declarations(req)?;
    if let Some(replay) = resolve_decompose_replay_set(tx.as_mut(), req, &declarations).await? {
        crate::write_wake::commit(tx).await?;
        return Ok(replay);
    }
    validate_active_head(&mut tx, &req.owner, req.parent_goal_id).await?;

    let mut prepared_children: Vec<(
        PreparedGoalInsert<'_>,
        Vec<super::EvidenceTarget>,
        &ReplayDeclaration,
    )> = Vec::with_capacity(req.children.len());
    for (child, declaration) in req.children.iter().zip(&declarations) {
        let evidence = validate_evidence_in_owner(&mut tx, &req.owner, &child.evidence).await?;
        let draft = child_draft(
            &req.owner,
            req.parent_goal_id,
            &req.topology,
            &req.authorship,
            child,
        )?;
        let prepared = prepare_goal_insert(
            &mut tx,
            &draft,
            None,
            GoalChange::DecomposeChild {
                parent: req.parent_goal_id,
            },
            req.context,
            RowColumns {
                store_number,
                writer,
                text: Some(&draft.text),
                write_act_t: None,
            },
        )
        .await?;
        prepared_children.push((prepared, evidence, declaration));
    }

    if prepared_children.iter().any(|(prepared, _, _)| {
        matches!(
            &prepared.preparation,
            super::GoalWritePreparation::Replay(_)
        )
    }) {
        let replay = resolve_decompose_replay_set(tx.as_mut(), req, &declarations)
            .await?
            .ok_or_else(|| internal("prepared Goal replay disappeared before declaration proof"))?;
        crate::write_wake::commit(tx).await?;
        return Ok(replay);
    }

    // Every child is prepared before any child gets a Goal head or
    // Goal row. One union lock makes crossed child target sets wait in the
    // same sorted order rather than acquiring a child footprint incrementally.
    let lock_items: Vec<&super::GoalWritePreparation> = prepared_children
        .iter()
        .map(|(child, _, _)| &child.preparation)
        .collect();
    lock_prepared_goal_writes(&mut tx, &lock_items).await?;
    // The parent is part of every child's dependency footprint. Re-check its
    // active-head status after that union lock, so a parent transition that
    // committed while children were being prepared aborts before the first
    // child Goal, sidecar, sketch or lifecycle Fact is written.
    validate_active_head(&mut tx, &req.owner, req.parent_goal_id).await?;

    let mut children = Vec::with_capacity(prepared_children.len());
    for (prepared, evidence, declaration) in prepared_children {
        // A replayed child was already returned above; `lifecycle_outcome`
        // holds the single guard for the state that cannot happen here.
        let inserted = persist_prepared_goal_insert(&mut tx, sidecars, &prepared).await?;
        let dependencies = draft_dependency_ids(&prepared.draft);
        let motivated_by = draft_motivation_ids(&prepared.draft);
        let outcome = lifecycle_outcome(
            &mut tx,
            LifecycleWrite {
                owner: &req.owner,
                inserted,
                motivated_by: &motivated_by,
                dependencies: &dependencies,
                evidence: &evidence,
            },
        )
        .await?;
        record_goal_replay_declaration(tx.as_mut(), declaration, &outcome).await?;
        children.push(DecomposedGoalOutcome { outcome });
    }

    crate::write_wake::commit(tx).await?;
    Ok(DecomposeGoalOutcome {
        children,
        idempotent_replay: false,
    })
}

#[cfg(test)]
#[path = "commands_lifecycle_lock_tests.rs"]
mod lifecycle_lock_tests;
