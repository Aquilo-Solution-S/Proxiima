use super::{
    GoalAtomicContext, GoalDraft, GoalId, GoalState, GoalWritePreparation, InsertedGoal,
    PayloadKind, PgSidecarKey, PgSidecarRegistryFrozen, Postgres, RowColumns, StorageError,
    Transaction, lock_prepared_goal_write, persist_prepared_goal_write, prepare_goal_write,
};
use crate::verbs::goal_timeseries::{
    GoalChangeFact, GoalWriteCommand, attach_goal_change_fact_reservation,
    attach_goal_close_fact_reservation, load_goal_by_request_id, persist_goal_change_fact,
    reserve_fact_identity,
};
use proxiima_core::publication::PublicationStamp;
use proxiima_core::{FactPayload, GoalChange, GoalChangeV1, SchemaVersion};
use sqlx::Acquire;

pub(super) struct PreparedGoalInsert<'a> {
    pub(super) draft: GoalDraft,
    pub(super) context: GoalAtomicContext<'a>,
    pub(super) expected_prior: Option<GoalId>,
    pub(super) preparation: GoalWritePreparation,
    /// The change Fact for a fresh write whose registry makes it listenable.
    pub(super) change_fact: Option<GoalChangeFact<'a>>,
}

/// A check that must see the database as it is once the write holds its
/// lifecycle locks, and before anything is persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PostLockGuard {
    None,
    /// Both closing actions recheck the children under the parent lifecycle lock.
    NoOpenChildren,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn insert_or_replay_goal(
    tx: &mut Transaction<'_, Postgres>,
    sidecars: &PgSidecarRegistryFrozen,
    draft: &GoalDraft,
    expected_prior: Option<GoalId>,
    change: GoalChange,
    context: GoalAtomicContext<'_>,
    columns: RowColumns<'_>,
    guard: PostLockGuard,
) -> Result<InsertedGoal, StorageError> {
    let mut retried_collision = false;
    loop {
        let mut attempt = tx.begin().await.map_err(crate::error::map_err)?;
        let result = async {
            let prepared = prepare_goal_insert(
                &mut attempt,
                draft,
                expected_prior,
                change,
                context,
                columns,
            )
            .await?;
            if let GoalWritePreparation::New(prepared_write) = &prepared.preparation {
                lock_prepared_goal_write(&mut attempt, prepared_write).await?;
                // A replay bypasses the guard on the current children.
                match guard {
                    PostLockGuard::None => {}
                    PostLockGuard::NoOpenChildren => {
                        super::ensure_goal_children_closed(
                            &mut attempt,
                            &draft.owner(),
                            prepared_write.handle(),
                        )
                        .await?;
                    }
                }
            }
            persist_prepared_goal_insert(&mut attempt, sidecars, &prepared).await
        }
        .await;
        match result {
            Ok(outcome) => {
                attempt.commit().await.map_err(crate::error::map_err)?;
                return Ok(outcome);
            }
            Err(StorageError::MemoryIdCollision) if !retried_collision => {
                attempt.rollback().await.map_err(crate::error::map_err)?;
                retried_collision = true;
            }
            Err(StorageError::MemoryIdCollision) => return Err(crate::error::collision_exhausted()),
            Err(error) => return Err(error),
        }
    }
}

/// Serialize goal writes that carry the same owner and request id for the
/// rest of the transaction. The `proxiima-goal-request:` prefix keeps the key
/// apart from the owner fence, series handles and lifecycle keys; the key is
/// hashed in the server and neither id is logged.
async fn lock_goal_request(
    tx: &mut Transaction<'_, Postgres>,
    owner: &proxiima_core::OwnerId,
    request_id: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(
                    hashtextextended('proxiima-goal-request:' || $1::text || ':' || $2, 0))",
    )
    .bind(owner.into_inner())
    .bind(request_id)
    .execute(&mut **tx)
    .await
    .map_err(crate::error::map_err)?;
    Ok(())
}

/// `Proxiima.Goals.parentRevision`.
async fn resolve_series_edge(
    tx: &mut Transaction<'_, Postgres>,
    owner: &proxiima_core::OwnerId,
    expected_prior: Option<GoalId>,
    change: GoalChange,
) -> Result<(Option<uuid::Uuid>, Option<uuid::Uuid>), StorageError> {
    if let Some(prior) = expected_prior {
        let row: Option<(uuid::Uuid, Option<uuid::Uuid>)> = sqlx::query_as(
            "SELECT handle, parent_handle FROM proxiima_core.goal WHERE t = $1 AND owner_id = $2",
        )
        .bind(prior.into_inner())
        .bind(owner.into_inner())
        .fetch_optional(&mut **tx)
        .await
        .map_err(crate::error::map_err)?;
        let (handle, parent_handle) = row.ok_or(StorageError::NotFound)?;
        Ok((Some(handle), parent_handle))
    } else if let GoalChange::DecomposeChild { parent } = change {
        let parent_handle: Option<uuid::Uuid> = sqlx::query_scalar(
            "SELECT handle FROM proxiima_core.goal WHERE t = $1 AND owner_id = $2",
        )
        .bind(parent.into_inner())
        .bind(owner.into_inner())
        .fetch_optional(&mut **tx)
        .await
        .map_err(crate::error::map_err)?;
        Ok((None, Some(parent_handle.ok_or(StorageError::NotFound)?)))
    } else {
        Ok((None, None))
    }
}

pub(super) async fn prepare_goal_insert<'a>(
    tx: &mut Transaction<'_, Postgres>,
    draft: &GoalDraft,
    expected_prior: Option<GoalId>,
    change: GoalChange,
    context: GoalAtomicContext<'a>,
    columns: RowColumns<'_>,
) -> Result<PreparedGoalInsert<'a>, StorageError> {
    let store_number = columns.store_number;
    let owner = draft.owner();

    // Serialize on the request key so a concurrent retry sees the first write's
    // replay record before it looks up mutable targets.
    lock_goal_request(tx, &owner, &draft.request_id).await?;
    if let Some(existing) = load_goal_by_request_id(tx, &owner, &draft.request_id).await? {
        return Ok(PreparedGoalInsert {
            draft: draft.clone(),
            context,
            expected_prior,
            preparation: GoalWritePreparation::Replay(
                crate::verbs::goal_timeseries::GoalWriteOutcome {
                    handle: existing.handle,
                    t: existing.t,
                    write_act_t: existing.write_act_t,
                    replay: true,
                },
            ),
            change_fact: None,
        });
    }
    // Fence the owner immediately after replay detection.  The lower-level
    // preparation repeats this re-entrantly, which keeps this decomposition
    // entry point safe when its prior/head preparation spans a bulk
    // owner erase.
    crate::access::fences::lock_owner_fence_shared_tx(tx, &owner).await?;
    crate::access::owner_registry::ensure_owner_row(tx.as_mut(), &owner).await?;
    validate_goal_schema(context, draft)?;

    let (handle, parent_handle) = resolve_series_edge(tx, &owner, expected_prior, change).await?;

    let command = GoalWriteCommand {
        handle,
        schema_id: draft.schema_id.as_str().to_string(),
        title: draft.title.clone(),
        text: columns.text.map(str::to_owned),
        state: draft.state,
        request_id: draft.request_id.clone(),
        close_fact_t: None,

        motivated_by: draft
            .topology
            .motivated_by()
            .iter()
            .map(|target| target.perspective_id().into_inner())
            .collect(),
        dependency_t: draft
            .topology
            .dependencies()
            .iter()
            .map(|dependency| dependency.goal_id().into_inner())
            .collect(),
        evidence_t: draft
            .topology
            .evidence()
            .iter()
            .map(|item| item.memory_id().into_inner())
            .collect(),
        mint_write_act: false,
        write_act_t: columns.write_act_t,
        parent_handle,
    };
    let preparation = prepare_goal_write(
        tx,
        &owner,
        columns.writer,
        &command,
        expected_prior.map(GoalId::into_inner),
        store_number,
    )
    .await?;
    // `Proxiima.Goals.closeFactPresence`.
    let close_fact = match &preparation {
        GoalWritePreparation::New(_)
            if matches!(draft.state, GoalState::Achieved | GoalState::Abandoned) =>
        {
            Some(reserve_fact_identity(store_number))
        }
        GoalWritePreparation::Replay(_) | GoalWritePreparation::New(_) => None,
    };
    let preparation = attach_goal_close_fact_reservation(preparation, close_fact);
    let change_fact = match (&preparation, goal_change_stamp(context)?) {
        (GoalWritePreparation::New(_), Some(stamp)) => Some(GoalChangeFact::new(
            reserve_fact_identity(store_number),
            change,
            stamp,
        )),
        (GoalWritePreparation::Replay(_) | GoalWritePreparation::New(_), _) => None,
    };
    let preparation = attach_goal_change_fact_reservation(preparation, change_fact.as_ref());
    Ok(PreparedGoalInsert {
        draft: draft.clone(),
        context,
        expected_prior,
        preparation,
        change_fact,
    })
}

/// `Proxiima.Goals.publishChangeFact`.
fn goal_change_stamp(
    context: GoalAtomicContext<'_>,
) -> Result<Option<PublicationStamp<'_>>, StorageError> {
    let listenable = context
        .registry
        .lookup_payload(
            &GoalChangeV1::schema_id(),
            SchemaVersion::new(GoalChangeV1::SCHEMA_VERSION),
            PayloadKind::Fact,
        )
        .is_some_and(|schema| schema.listenable);
    match (listenable, context.publication) {
        (false, _) => Ok(None),
        (true, Some(stamp)) => Ok(Some(stamp)),
        (true, None) => Err(StorageError::ConstraintViolation(format!(
            "{} is listenable but the goal context carries no publication stamp",
            GoalChangeV1::SCHEMA_ID
        ))),
    }
}

pub(super) async fn persist_prepared_goal_insert(
    tx: &mut Transaction<'_, Postgres>,
    sidecars: &PgSidecarRegistryFrozen,
    prepared: &PreparedGoalInsert<'_>,
) -> Result<InsertedGoal, StorageError> {
    let out = match &prepared.preparation {
        GoalWritePreparation::Replay(outcome) => outcome.clone(),
        GoalWritePreparation::New(prepared) => persist_prepared_goal_write(tx, prepared).await?,
    };
    if !out.replay {
        insert_goal_sidecar(
            tx,
            sidecars,
            prepared.context,
            &prepared.draft,
            GoalId::new(out.t),
            prepared.expected_prior,
        )
        .await?;
        // `Proxiima.Goals.publishChangeFact`.
        if let (GoalWritePreparation::New(write), Some(fact)) =
            (&prepared.preparation, &prepared.change_fact)
        {
            persist_goal_change_fact(tx, sidecars, write, fact).await?;
        }
    }

    Ok(InsertedGoal {
        goal_id: GoalId::new(out.t),
        handle: out.handle,
        idempotent_replay: out.replay,
    })
}

fn validate_goal_schema(
    context: GoalAtomicContext<'_>,
    draft: &GoalDraft,
) -> Result<(), StorageError> {
    context
        .registry
        .lookup_payload(&draft.schema_id, draft.schema_version, PayloadKind::Goal)
        .ok_or_else(|| {
            StorageError::ConstraintViolation(format!(
                "unregistered GoalPayload schema {} v{}",
                draft.schema_id.as_str(),
                draft.schema_version.into_inner(),
            ))
        })?;
    Ok(())
}

async fn insert_goal_sidecar(
    tx: &mut Transaction<'_, Postgres>,
    sidecars: &PgSidecarRegistryFrozen,
    context: GoalAtomicContext<'_>,
    draft: &GoalDraft,
    goal_id: GoalId,
    source_goal_id: Option<GoalId>,
) -> Result<(), StorageError> {
    let Some(sidecar_table) = context
        .registry
        .lookup_payload(&draft.schema_id, draft.schema_version, PayloadKind::Goal)
        .and_then(|schema| schema.sidecar_table.as_deref())
    else {
        return Ok(());
    };
    if let Some(payload) = &draft.sidecar_payload {
        if payload.kind != PayloadKind::Goal
            || payload.schema_id != draft.schema_id
            || payload.schema_version != draft.schema_version
        {
            return Err(StorageError::ConstraintViolation(format!(
                "Goal sidecar payload drift for {} v{} table {sidecar_table}",
                draft.schema_id.as_str(),
                draft.schema_version.into_inner(),
            )));
        }
        sidecars.insert_goal_sidecar(tx, goal_id, payload).await?;
        return Ok(());
    }

    if let Some(source_goal_id) = source_goal_id {
        let key = PgSidecarKey::new(
            PayloadKind::Goal,
            draft.schema_id.clone(),
            draft.schema_version,
        );
        sidecars
            .copy_goal_sidecar(tx, key, goal_id, source_goal_id)
            .await?;
        return Ok(());
    }

    Err(StorageError::ConstraintViolation(format!(
        "missing typed Goal sidecar payload for {} v{} table {sidecar_table}",
        draft.schema_id.as_str(),
        draft.schema_version.into_inner(),
    )))
}
