//! Direct `OwnerId` column helpers: the role codec, group membership and home owners.

use super::owner_registry::insert_group_row;
use super::transfer::entity_kind;
use crate::error::{map_err, with_bounded_retry};
use proxiima_core::storage_ports::{MembershipEndpointAuthority, MembershipMemberAuthority};
use proxiima_core::{
    AccessCeiling, AccessKind, EntityId, EntityKind, GoalAccess, GroupId, MembershipRow, OwnerId,
    Relation, Role, SchemaId, StorageError, UserId,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "proxiima_core.access_ceiling", rename_all = "lowercase")]
pub(crate) enum PgAccessCeiling {
    None,
    Fact,
    Abstraction,
    Perspective,
    Goal,
}

/// The stored role columns still carry a fourth rung above Perspective.
/// This is the only place that reads or writes it: on a read column it is
/// Goal access, on a write column administration.
impl PgAccessCeiling {
    /// The memory ceiling under the stored rung.
    const fn memory(self) -> AccessCeiling {
        match self {
            Self::None => AccessCeiling::None,
            Self::Fact => AccessCeiling::Fact,
            Self::Abstraction => AccessCeiling::Abstraction,
            Self::Perspective | Self::Goal => AccessCeiling::Perspective,
        }
    }

    const fn of(ceiling: AccessCeiling) -> Self {
        match ceiling {
            AccessCeiling::None => Self::None,
            AccessCeiling::Fact => Self::Fact,
            AccessCeiling::Abstraction => Self::Abstraction,
            AccessCeiling::Perspective => Self::Perspective,
        }
    }
}

fn stored_role(read: PgAccessCeiling, write: PgAccessCeiling) -> Result<Role, StorageError> {
    Role::new(read.memory(), write.memory()).map_err(|error| {
        StorageError::ConstraintViolation(format!("stored role is invalid: {error}"))
    })
}

/// A role from its stored columns: the top read rung works the owner's Goals,
/// the top write rung administers the owner. The group role projection and
/// delegated ceilings share this one reading.
pub(crate) fn member_role(
    read: PgAccessCeiling,
    write: PgAccessCeiling,
    manage: bool,
) -> Result<Role, StorageError> {
    let mut role = stored_role(read, write)?;
    if read == PgAccessCeiling::Goal {
        role = role.with_goals(GoalAccess::Works);
    }
    if write == PgAccessCeiling::Goal {
        role = role.administering();
    }
    Ok(if manage { role.managing() } else { role })
}

/// The stored columns of a delegated ceiling: the inverse of
/// [`member_role`]. The fourth rung sits above Perspective and says "works
/// Goals" or "administers", so a ceiling that reads Goals without working
/// them, works them below a Perspective read limit, or administers without
/// a Perspective write limit and Goal work has no stored form. It is
/// refused, neither widened nor narrowed.
pub(crate) fn delegated_ceiling_columns(
    role: Role,
) -> Result<(PgAccessCeiling, PgAccessCeiling), StorageError> {
    let top = AccessCeiling::Perspective;
    let read = match role.goals() {
        GoalAccess::None => Some(PgAccessCeiling::of(role.read_ceiling())),
        GoalAccess::Works if role.read_ceiling() == top => Some(PgAccessCeiling::Goal),
        GoalAccess::Reads | GoalAccess::Works => None,
    };
    let write = if !role.administers() {
        Some(PgAccessCeiling::of(role.write_ceiling()))
    } else if role.write_ceiling() == top && role.works_goals() {
        Some(PgAccessCeiling::Goal)
    } else {
        None
    };
    read.zip(write).ok_or_else(|| {
        StorageError::ConstraintViolation(
            "a delegated ceiling works Goals only with a Perspective read limit, reads Goals \
             only by working them, and administers only with a Perspective write limit and \
             Goal work"
                .into(),
        )
    })
}

/// The access kind of a memory's entity kind.
pub(crate) fn memory_access_kind(kind: EntityKind) -> Result<AccessKind, StorageError> {
    AccessKind::of(kind)
        .ok_or_else(|| StorageError::ConstraintViolation("a Goal is no memory".into()))
}

/// # Errors
///
/// Returns `Internal` on sqlx failure.
pub(crate) async fn resolve_membership(
    conn: &mut PgConnection,
    owner: &OwnerId,
) -> Result<Vec<MembershipRow>, StorageError> {
    let resolved = resolve_subject_membership(conn, owner.as_user(), None).await?;
    if resolved.subject_is_group {
        return Err(StorageError::ConstraintViolation(
            "membership lookup subject is registered as a group".into(),
        ));
    }
    Ok(resolved
        .memberships
        .into_iter()
        .map(|(group, role)| MembershipRow { group, role })
        .collect())
}

/// The subject kind and the memberships visible to one resolution probe.
/// The kind is read in the same query as the resolved projection so a
/// group-shaped subject is refused without a second round trip.
pub(crate) struct SubjectMemberships {
    pub(crate) subject_is_group: bool,
    pub(crate) memberships: Vec<(GroupId, Role)>,
}

type SubjectMembershipRow = (
    bool,
    Option<uuid::Uuid>,
    Option<PgAccessCeiling>,
    Option<PgAccessCeiling>,
    Option<bool>,
);

/// Resolve a subject's registered kind and projected roles in one query.
/// `group` narrows the projection rows for the point probe used by
/// `group_role`; `None` enumerates every group for `roles_for_subject`.
///
/// # Errors
///
/// Returns `Internal` on sqlx failure.
pub(crate) async fn resolve_subject_membership(
    conn: &mut sqlx::PgConnection,
    subject: UserId,
    group: Option<GroupId>,
) -> Result<SubjectMemberships, StorageError> {
    let query = if let Some(group) = group {
        sqlx::query_as::<_, SubjectMembershipRow>(
            "WITH subject AS (
             SELECT EXISTS (
                 SELECT 1 FROM proxiima_core.owners
                  WHERE owner_id = $1 AND kind = 'group'
             ) AS subject_is_group
         )
         SELECT subject.subject_is_group, memberships.group_id,
                memberships.read_ceiling, memberships.write_ceiling, memberships.manage
           FROM subject
           LEFT JOIN proxiima_core.group_role_projection AS memberships
             ON memberships.member_user_id = $1
            AND memberships.group_id = $2",
        )
        .bind(subject.into_inner())
        .bind(group.into_inner())
    } else {
        sqlx::query_as::<_, SubjectMembershipRow>(
            "WITH subject AS (
             SELECT EXISTS (
                 SELECT 1 FROM proxiima_core.owners
                  WHERE owner_id = $1 AND kind = 'group'
             ) AS subject_is_group
         )
         SELECT subject.subject_is_group, memberships.group_id,
                memberships.read_ceiling, memberships.write_ceiling, memberships.manage
           FROM subject
           LEFT JOIN proxiima_core.group_role_projection AS memberships
             ON memberships.member_user_id = $1
          ORDER BY memberships.group_id, memberships.read_ceiling,
                   memberships.write_ceiling, memberships.manage",
        )
        .bind(subject.into_inner())
    };
    let rows = query.fetch_all(&mut *conn).await.map_err(map_err)?;

    let subject_is_group = rows.first().is_some_and(|(is_group, _, _, _, _)| *is_group);
    let mut memberships = Vec::with_capacity(rows.len());
    for (_, group, read, write, manage) in rows {
        let Some(group) = group else { continue };
        let (Some(read), Some(write), Some(manage)) = (read, write, manage) else {
            continue;
        };
        memberships.push((GroupId::new(group), member_role(read, write, manage)?));
    }
    Ok(SubjectMemberships {
        subject_is_group,
        memberships,
    })
}

/// # Errors
///
/// Returns `Internal` on sqlx failure.
pub(crate) async fn lock_group_membership_tx(
    conn: &mut PgConnection,
    group_id: GroupId,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO proxiima_core.group_lock_request
            (request_id, left_group_id)
         VALUES ($1, $2)",
    )
    .bind(Uuid::now_v7())
    .bind(group_id.into_inner())
    .execute(&mut *conn)
    .await
    .map_err(map_err)?;
    Ok(())
}

/// Acquire the tree lock before reading a member's owner kind. The trigger
/// gateway computes the roots, orders two trees, and rechecks roots after the
/// advisory locks; the row is consumed by its BEFORE trigger.
pub(crate) async fn lock_group_membership_write_tx(
    conn: &mut PgConnection,
    parent: GroupId,
    member: OwnerId,
) -> Result<bool, StorageError> {
    let row: (bool,) = sqlx::query_as(
        "INSERT INTO proxiima_core.group_lock_request
            (request_id, left_group_id, right_group_id)
         VALUES ($1, $2, $3)
         RETURNING member_is_group",
    )
    .bind(Uuid::now_v7())
    .bind(parent.into_inner())
    .bind(member.into_inner())
    .fetch_one(&mut *conn)
    .await
    .map_err(map_err)?;
    Ok(row.0)
}

/// # Errors
///
/// Returns `Conflict` when any Admin row already exists for the group, and
/// `Internal` on sqlx failure.
pub(crate) async fn bootstrap_group_admin(
    _pool: &PgPool,
    platform: &crate::PgPlatformScope,
    group_id: GroupId,
    first_admin_user_id: UserId,
    _granted_by: uuid::Uuid,
) -> Result<(), StorageError> {
    with_bounded_retry(move || async move {
        let mut tx = platform.begin().await?;
        let result =
            bootstrap_group_admin_on_connection(&mut tx, group_id, first_admin_user_id).await;
        crate::owner_scope::finish_transaction(tx, result).await
    })
    .await
}

/// Register a group and seed its first Admin; refuse an existing Admin.
///
/// No authorization runs, the authorization hooks included: the host vouches
/// for this write as for its own rows.
/// The caller must supply a platform-scoped transaction (`&mut *tx`). This
/// function never begins, commits, rolls back, retries or sets a scope; on a
/// retryable error, the caller retries its whole transaction. All effects are
/// database rows in that transaction; no post-commit call is needed.
///
/// # Errors
///
/// Returns [`StorageError::Conflict`] if an Admin exists or the group id is
/// registered as a user (the membership foreign key); other database failures
/// are classified as [`StorageError`].
pub async fn bootstrap_group_admin_on_connection(
    conn: &mut PgConnection,
    group_id: GroupId,
    first_admin_user_id: UserId,
) -> Result<(), StorageError> {
    lock_group_membership_tx(conn, group_id).await?;
    insert_group_row(conn, group_id).await?;
    let inserted = sqlx::query(
        "INSERT INTO proxiima_core.group_memberships
            (group_id, member_user_id, relation)
         SELECT $1, $2, $3
          WHERE NOT EXISTS (
              SELECT 1
                FROM proxiima_core.group_memberships
               WHERE group_id = $1
                 AND relation = $3
          )",
    )
    .bind(group_id.into_inner())
    .bind(first_admin_user_id.into_inner())
    .bind(Relation::Admin)
    .execute(&mut *conn)
    .await
    .map_err(map_err)?;

    if inserted.rows_affected() == 0 {
        return Err(StorageError::Conflict(
            "group already has an Admin membership".into(),
        ));
    }

    Ok(())
}

/// Result of an idempotent membership addition in the caller's transaction.
///
/// The change becomes durable only when that transaction commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipAddOutcome {
    /// The requested person relation or group edge was inserted.
    Changed,
    /// The exact requested relation or group edge already existed.
    AlreadyPresent,
}

/// Adding a member to an id never seen before creates the group, so the
/// id is registered as a group first.
///
/// # Errors
///
/// [`StorageError::Conflict`] when the id is registered as a user: a
/// membership names a group, never a user. `Internal` on sqlx failure.
pub(crate) async fn add_group_member(
    pool: &PgPool,
    owner_scope: Option<&proxiima_core::OwnerScope>,
    group_id: GroupId,
    member: OwnerId,
    relation: Relation,
    member_authority: MembershipMemberAuthority,
    _granted_by: uuid::Uuid,
) -> Result<(), StorageError> {
    with_bounded_retry(move || async move {
        let mut tx = crate::owner_scope::begin_owner_transaction(
            pool,
            crate::owner_scope::require_owner_scope(owner_scope)?,
        )
        .await?;
        let result = add_group_member_on_connection_authorized(
            &mut tx,
            group_id,
            member,
            relation,
            member_authority,
        )
        .await;
        crate::owner_scope::finish_transaction(tx, result)
            .await
            .map(|_| ())
    })
    .await
}

/// Register an unseen group and add a relation; an existing relation stays.
/// The outcome distinguishes an insertion from an exact replay under the
/// same membership lock as the write.
///
/// No authorization runs, the authorization hooks included: the host vouches
/// for this write as for its own rows.
/// The caller must supply a platform-scoped transaction (`&mut *tx`). This
/// function never begins, commits, rolls back, retries or sets a scope; on a
/// retryable error, the caller retries its whole transaction. All effects are
/// database rows in that transaction; no post-commit call is needed.
///
/// # Errors
///
/// Returns [`StorageError::Conflict`] if the group id is registered as a user
/// (the membership foreign key); other database failures are classified as
/// [`StorageError`].
pub async fn add_group_member_on_connection(
    conn: &mut PgConnection,
    group_id: GroupId,
    member: OwnerId,
    relation: Relation,
) -> Result<MembershipAddOutcome, StorageError> {
    add_group_member_on_connection_authorized(
        conn,
        group_id,
        member,
        relation,
        MembershipMemberAuthority::CallerManagesMember,
    )
    .await
}

async fn add_group_member_on_connection_authorized(
    conn: &mut PgConnection,
    group_id: GroupId,
    member: OwnerId,
    relation: Relation,
    member_authority: MembershipMemberAuthority,
) -> Result<MembershipAddOutcome, StorageError> {
    let member_is_group = lock_group_membership_write_tx(conn, group_id, member).await?;
    insert_group_row(conn, group_id).await?;
    if member_is_group {
        if !matches!(
            member_authority,
            MembershipMemberAuthority::CallerManagesMember
        ) {
            return Err(StorageError::Forbidden(
                "attaching a group requires manage authority on the attached group".into(),
            ));
        }
        let existing: Option<(Uuid, Relation)> = sqlx::query_as(
            "SELECT parent_group_id, relation
               FROM proxiima_core.group_edges
              WHERE child_group_id = $1",
        )
        .bind(member.into_inner())
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_err)?;
        if let Some((existing_parent, existing_relation)) = existing {
            if existing_parent == group_id.into_inner() && existing_relation == relation {
                return Ok(MembershipAddOutcome::AlreadyPresent);
            }
            return Err(StorageError::Conflict(
                "group is already attached to a parent".into(),
            ));
        }
        let inserted = sqlx::query(
            "INSERT INTO proxiima_core.group_edges (child_group_id, parent_group_id, relation)
             VALUES ($2, $1, $3)
            ON CONFLICT (child_group_id) DO NOTHING",
        )
        .bind(group_id.into_inner())
        .bind(member.into_inner())
        .bind(relation)
        .execute(&mut *conn)
        .await
        .map_err(map_err)?;
        if inserted.rows_affected() == 0 {
            return Err(StorageError::Conflict(
                "group is already attached to a parent".into(),
            ));
        }
        Ok(MembershipAddOutcome::Changed)
    } else {
        let inserted = sqlx::query(
            "INSERT INTO proxiima_core.group_memberships
                (group_id, member_user_id, relation)
             VALUES ($1, $2, $3)
             ON CONFLICT (group_id, member_user_id, relation) DO NOTHING",
        )
        .bind(group_id.into_inner())
        .bind(member.into_inner())
        .bind(relation)
        .execute(&mut *conn)
        .await
        .map_err(map_err)?;
        Ok(if inserted.rows_affected() == 0 {
            MembershipAddOutcome::AlreadyPresent
        } else {
            MembershipAddOutcome::Changed
        })
    }
}

/// Under the group-tree lock: the caller the engine authorized must still
/// manage `group`. A role revoked between the engine check and this lock
/// refuses the write. A write without a named caller is the host's own.
async fn recheck_caller_manages(
    conn: &mut PgConnection,
    group_id: GroupId,
    caller: Option<UserId>,
) -> Result<(), StorageError> {
    let Some(caller) = caller else {
        return Ok(());
    };
    let manages: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM proxiima_core.group_role_projection
              WHERE group_id = $1 AND member_user_id = $2 AND manage)",
    )
    .bind(group_id.into_inner())
    .bind(caller.into_inner())
    .fetch_one(&mut *conn)
    .await
    .map_err(map_err)?;
    if !manages {
        return Err(StorageError::Forbidden(
            "the caller no longer manages this group".into(),
        ));
    }
    Ok(())
}

/// Whether `group` has a stored manager. Groups whose roles the host resolves
/// on demand have none; storage then has no role to re-read, and the change
/// rests on the engine's check alone. Once a group has a stored manager, a
/// change a caller makes under the group-tree lock must not leave it with
/// none. The host's own writes name no caller and may revoke the last one.
async fn group_has_manager(
    conn: &mut PgConnection,
    group_id: GroupId,
) -> Result<bool, StorageError> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM proxiima_core.group_role_projection
              WHERE group_id = $1 AND manage)",
    )
    .bind(group_id.into_inner())
    .fetch_one(&mut *conn)
    .await
    .map_err(map_err)
}

/// One membership change of a group, made as a caller who manages it: an owner-scoped
/// transaction under the group-membership write lock, retried whole on a transient conflict. A
/// group that has a stored manager must still have one afterwards, and the caller must still be
/// one of them when the lock is held; `change` is the one statement that differs.
async fn manager_preserving_change<C>(
    pool: &PgPool,
    owner_scope: Option<&proxiima_core::OwnerScope>,
    group_id: GroupId,
    member: OwnerId,
    caller: Option<UserId>,
    change: C,
) -> Result<(), StorageError>
where
    C: for<'c> Fn(
        &'c mut PgConnection,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'c>,
    >,
{
    let change = &change;
    with_bounded_retry(move || async move {
        let mut tx = crate::owner_scope::begin_owner_transaction(
            pool,
            crate::owner_scope::require_owner_scope(owner_scope)?,
        )
        .await?;
        let result = async {
            #[cfg(test)]
            crate::pause_point::reach(crate::pause_point::BEFORE_GROUP_LOCK, group_id.into_inner())
                .await;
            lock_group_membership_write_tx(&mut tx, group_id, member).await?;
            let had_manager = group_has_manager(&mut tx, group_id).await?;
            if had_manager {
                recheck_caller_manages(&mut tx, group_id, caller).await?;
            }
            change(&mut tx).await?;
            if had_manager && !group_has_manager(&mut tx, group_id).await? {
                return Err(StorageError::Conflict(
                    "the change would leave the group without a manager".into(),
                ));
            }
            Ok(())
        }
        .await;
        crate::owner_scope::finish_transaction(tx, result).await
    })
    .await
}

/// # Errors
///
/// Returns `Internal` on sqlx failure.
pub(crate) async fn remove_group_member(
    pool: &PgPool,
    owner_scope: Option<&proxiima_core::OwnerScope>,
    group_id: GroupId,
    member: OwnerId,
    member_authority: MembershipMemberAuthority,
    endpoint_authority: MembershipEndpointAuthority,
    caller: Option<UserId>,
) -> Result<(), StorageError> {
    manager_preserving_change(pool, owner_scope, group_id, member, caller, move |conn| {
        Box::pin(remove_group_member_on_connection_authorized(
            conn,
            group_id,
            member,
            member_authority,
            endpoint_authority,
        ))
    })
    .await
}

/// Remove every relation of a member in a group; an absent member is `Ok`.
///
/// No authorization runs, the authorization hooks included: the host vouches
/// for this write as for its own rows.
/// The caller must supply a platform-scoped transaction (`&mut *tx`). This
/// function never begins, commits, rolls back, retries or sets a scope; on a
/// retryable error, the caller retries its whole transaction. All effects are
/// database rows in that transaction; no post-commit call is needed.
///
/// # Errors
///
/// Returns [`StorageError`] for database failures.
pub async fn remove_group_member_on_connection(
    conn: &mut PgConnection,
    group_id: GroupId,
    member: OwnerId,
) -> Result<(), StorageError> {
    remove_group_member_on_connection_authorized(
        conn,
        group_id,
        member,
        MembershipMemberAuthority::CallerManagesMember,
        MembershipEndpointAuthority::Parent,
    )
    .await
}

async fn remove_group_member_on_connection_authorized(
    conn: &mut PgConnection,
    group_id: GroupId,
    member: OwnerId,
    member_authority: MembershipMemberAuthority,
    endpoint_authority: MembershipEndpointAuthority,
) -> Result<(), StorageError> {
    let member_is_group = lock_group_membership_write_tx(conn, group_id, member).await?;
    if member_is_group
        && matches!(endpoint_authority, MembershipEndpointAuthority::Member)
        && !matches!(
            member_authority,
            MembershipMemberAuthority::CallerManagesMember
        )
    {
        return Err(StorageError::Forbidden(
            "detaching a group requires manage authority on the attached group".into(),
        ));
    }
    if !member_is_group && matches!(endpoint_authority, MembershipEndpointAuthority::Member) {
        return Err(StorageError::Forbidden(
            "detaching a person requires parent authority".into(),
        ));
    }
    if member_is_group {
        sqlx::query(
            "DELETE FROM proxiima_core.group_edges
              WHERE parent_group_id = $1 AND child_group_id = $2",
        )
        .bind(group_id.into_inner())
        .bind(member.into_inner())
        .execute(&mut *conn)
        .await
        .map_err(map_err)?;
        return Ok(());
    }
    sqlx::query(
        "DELETE FROM proxiima_core.group_memberships WHERE group_id = $1 AND member_user_id = $2",
    )
    .bind(group_id.into_inner())
    .bind(member.into_inner())
    .execute(&mut *conn)
    .await
    .map_err(map_err)?;
    Ok(())
}

/// Delete the one `relation` row of `member_user_id` in `group_id`, under the
/// group's membership lock. An absent row is `Ok`.
///
/// # Errors
///
/// Returns `Internal` on sqlx failure.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn remove_group_member_relation(
    pool: &PgPool,
    owner_scope: Option<&proxiima_core::OwnerScope>,
    group_id: GroupId,
    member: OwnerId,
    relation: Relation,
    member_authority: MembershipMemberAuthority,
    endpoint_authority: MembershipEndpointAuthority,
    caller: Option<UserId>,
) -> Result<(), StorageError> {
    manager_preserving_change(pool, owner_scope, group_id, member, caller, move |conn| {
        Box::pin(remove_group_member_relation_on_connection_authorized(
            conn,
            group_id,
            member,
            relation,
            member_authority,
            endpoint_authority,
        ))
    })
    .await
}

/// Remove one relation of a member in a group; an absent relation is `Ok`.
///
/// No authorization runs, the authorization hooks included: the host vouches
/// for this write as for its own rows.
/// The caller must supply a platform-scoped transaction (`&mut *tx`). This
/// function never begins, commits, rolls back, retries or sets a scope; on a
/// retryable error, the caller retries its whole transaction. All effects are
/// database rows in that transaction; no post-commit call is needed.
///
/// # Errors
///
/// Returns [`StorageError`] for database failures.
pub async fn remove_group_member_relation_on_connection(
    conn: &mut PgConnection,
    group_id: GroupId,
    member: OwnerId,
    relation: Relation,
) -> Result<(), StorageError> {
    remove_group_member_relation_on_connection_authorized(
        conn,
        group_id,
        member,
        relation,
        MembershipMemberAuthority::CallerManagesMember,
        MembershipEndpointAuthority::Parent,
    )
    .await
}

async fn remove_group_member_relation_on_connection_authorized(
    conn: &mut PgConnection,
    group_id: GroupId,
    member: OwnerId,
    relation: Relation,
    member_authority: MembershipMemberAuthority,
    endpoint_authority: MembershipEndpointAuthority,
) -> Result<(), StorageError> {
    let member_is_group = lock_group_membership_write_tx(conn, group_id, member).await?;
    if member_is_group
        && matches!(endpoint_authority, MembershipEndpointAuthority::Member)
        && !matches!(
            member_authority,
            MembershipMemberAuthority::CallerManagesMember
        )
    {
        return Err(StorageError::Forbidden(
            "detaching a group requires manage authority on the attached group".into(),
        ));
    }
    if !member_is_group && matches!(endpoint_authority, MembershipEndpointAuthority::Member) {
        return Err(StorageError::Forbidden(
            "detaching a person requires parent authority".into(),
        ));
    }
    if member_is_group {
        sqlx::query(
            "DELETE FROM proxiima_core.group_edges
              WHERE parent_group_id = $1 AND child_group_id = $2 AND relation = $3",
        )
        .bind(group_id.into_inner())
        .bind(member.into_inner())
        .bind(relation)
        .execute(&mut *conn)
        .await
        .map_err(map_err)?;
    } else {
        sqlx::query(
            "DELETE FROM proxiima_core.group_memberships
              WHERE group_id = $1 AND member_user_id = $2 AND relation = $3",
        )
        .bind(group_id.into_inner())
        .bind(member.into_inner())
        .bind(relation)
        .execute(&mut *conn)
        .await
        .map_err(map_err)?;
    }
    Ok(())
}

/// Replace the `from` row of `member_user_id` in `group_id` with a `to` row in
/// one transaction under the group's membership lock: delete `from`, insert
/// `to` (an existing `to` row stays), commit. The lock serializes every
/// membership change of the group, so a second replacement of the same row
/// waits, then finds `from` gone.
///
/// # Errors
///
/// Returns `Conflict`, with nothing changed, when the member does not hold
/// `from` (including a row the owner scope cannot see), and `Internal` on sqlx
/// failure.
pub(crate) async fn replace_group_member_relation(
    pool: &PgPool,
    owner_scope: Option<&proxiima_core::OwnerScope>,
    group_id: GroupId,
    member: OwnerId,
    from: Relation,
    to: Relation,
    caller: Option<UserId>,
) -> Result<(), StorageError> {
    manager_preserving_change(pool, owner_scope, group_id, member, caller, move |conn| {
        Box::pin(replace_group_member_relation_on_connection(
            conn, group_id, member, from, to,
        ))
    })
    .await
}

/// Replace a held relation with another; an existing target relation stays.
///
/// No authorization runs, the authorization hooks included: the host vouches
/// for this write as for its own rows.
/// The caller must supply a platform-scoped transaction (`&mut *tx`). This
/// function never begins, commits, rolls back, retries or sets a scope; on a
/// retryable error, the caller retries its whole transaction. All effects are
/// database rows in that transaction; no post-commit call is needed.
///
/// # Errors
///
/// Returns [`StorageError::Conflict`] if the member does not hold `from`, with
/// nothing changed. Other database failures are classified as [`StorageError`];
/// the caller must roll back the transaction on failure.
pub async fn replace_group_member_relation_on_connection(
    conn: &mut PgConnection,
    group_id: GroupId,
    member: OwnerId,
    from: Relation,
    to: Relation,
) -> Result<(), StorageError> {
    let member_is_group = lock_group_membership_write_tx(conn, group_id, member).await?;
    if member_is_group {
        let updated = sqlx::query(
            "UPDATE proxiima_core.group_edges
                SET relation = $4
              WHERE parent_group_id = $1 AND child_group_id = $2 AND relation = $3",
        )
        .bind(group_id.into_inner())
        .bind(member.into_inner())
        .bind(from)
        .bind(to)
        .execute(&mut *conn)
        .await
        .map_err(map_err)?;
        if updated.rows_affected() == 0 {
            return Err(StorageError::Conflict(
                "member does not hold the relation being replaced".into(),
            ));
        }
        return Ok(());
    }
    let deleted = sqlx::query(
        "DELETE FROM proxiima_core.group_memberships
          WHERE group_id = $1
            AND member_user_id = $2
            AND relation = $3",
    )
    .bind(group_id.into_inner())
    .bind(member.into_inner())
    .bind(from)
    .execute(&mut *conn)
    .await
    .map_err(map_err)?;
    if deleted.rows_affected() == 0 {
        return Err(StorageError::Conflict(
            "member does not hold the relation being replaced".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO proxiima_core.group_memberships
            (group_id, member_user_id, relation)
         VALUES ($1, $2, $3)
         ON CONFLICT (group_id, member_user_id, relation) DO NOTHING",
    )
    .bind(group_id.into_inner())
    .bind(member.into_inner())
    .bind(to)
    .execute(&mut *conn)
    .await
    .map_err(map_err)?;
    Ok(())
}

/// # Errors
///
/// Returns `Internal` on sqlx failure.
pub(crate) async fn list_group_members(
    conn: &mut PgConnection,
    group_id: GroupId,
) -> Result<Vec<(OwnerId, Relation)>, StorageError> {
    // People remain the membership rows as written; child groups are the
    // forest edge rows. Both use the same stable owner-id/relation order.
    let rows: Vec<(uuid::Uuid, bool, Relation)> = sqlx::query_as(
        "SELECT member_user_id, false, relation
           FROM proxiima_core.group_memberships
          WHERE group_id = $1
         UNION ALL
         SELECT child_group_id, true, relation
           FROM proxiima_core.group_edges
          WHERE parent_group_id = $1
          ORDER BY 1, 2, 3",
    )
    .bind(group_id.into_inner())
    .fetch_all(&mut *conn)
    .await
    .map_err(map_err)?;

    Ok(rows
        .into_iter()
        .map(|(member, _is_group, relation)| (OwnerId::new(member), relation))
        .collect())
}

/// One page of group members in the keyset total order
/// `(member_user_id, relation)`, starting strictly after `after` when
/// given. Fetches at most `limit` rows; callers over-fetch by one to
/// detect further pages. Relation ties order by the
/// `proxiima_core.membership_relation` enum definition.
///
/// # Errors
///
/// Returns `Internal` on sqlx failure.
pub(crate) async fn list_group_members_page(
    conn: &mut PgConnection,
    group_id: GroupId,
    after: Option<(OwnerId, Relation)>,
    limit: i64,
) -> Result<Vec<(OwnerId, Relation)>, StorageError> {
    let after_member = after.map(|(member, _)| member.into_inner());
    let after_relation = after.map(|(_, relation)| relation);
    let rows: Vec<(uuid::Uuid, bool, Relation)> = sqlx::query_as(
        "SELECT member_id, is_group, relation
           FROM (
             SELECT member_user_id AS member_id, false AS is_group, relation
               FROM proxiima_core.group_memberships
              WHERE group_id = $1
             UNION ALL
             SELECT child_group_id, true, relation
               FROM proxiima_core.group_edges
              WHERE parent_group_id = $1
           ) members
          WHERE ($2::uuid IS NULL
                 OR (member_id, relation)
                    > ($2::uuid, $3::proxiima_core.membership_relation))
          ORDER BY member_id, is_group, relation
          LIMIT $4",
    )
    .bind(group_id.into_inner())
    .bind(after_member)
    .bind(after_relation)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map_err(map_err)?;

    Ok(rows
        .into_iter()
        .map(|(member, _is_group, relation)| (OwnerId::new(member), relation))
        .collect())
}

/// A visible entity's row, by `(is_goal, t)`: owner, stored kind, schema.
type HomeRows = std::collections::HashMap<(bool, uuid::Uuid), (uuid::Uuid, String, Option<String>)>;

/// Home owner, kind and schema of each of `entities`, in order, when the row
/// is in `read_owners`. Absent and foreign are both `None`. A goal row has
/// no schema.
///
/// # Errors
///
/// Returns `Internal` on sqlx failure.
pub(crate) async fn visible_home_owners(
    conn: &mut sqlx::PgConnection,
    entities: &[EntityId],
    read_owners: &[OwnerId],
) -> Result<Vec<Option<(OwnerId, EntityKind, Option<SchemaId>)>>, StorageError> {
    if read_owners.is_empty() {
        return Ok(vec![None; entities.len()]);
    }

    let owner_ids: Vec<uuid::Uuid> = read_owners
        .iter()
        .copied()
        .map(OwnerId::into_inner)
        .collect();
    let (mut memories, mut goals) = (Vec::new(), Vec::new());
    for entity in entities {
        match entity {
            EntityId::Memory(id) => memories.push(id.into_inner()),
            EntityId::Goal(id) => goals.push(id.into_inner()),
        }
    }
    // The entity discriminant is authorization data. Memory visibility is the
    // memory spine, forgotten rows included, while Goal visibility stays on
    // its own spine. The candidate-owner uniqueness guard makes a malformed
    // duplicate `(entity, owner)` state fail closed instead of authorizing the
    // first row returned by PostgreSQL.
    let mut homes: HomeRows = std::collections::HashMap::new();
    if !memories.is_empty() {
        let rows: Vec<(uuid::Uuid, uuid::Uuid, String, String)> = sqlx::query_as(
            "WITH candidates AS (
                 SELECT t, owner_id, kind, schema_id FROM proxiima_core.memory WHERE t = ANY($1::uuid[]) /* FORGET-POLICY: includes-forgotten (transfer moves forgotten rows too) */
             )
             SELECT c.t, c.owner_id, c.kind::text, c.schema_id
               FROM candidates c
              WHERE c.owner_id = ANY($2::uuid[])
                AND NOT EXISTS (
                    SELECT 1
                      FROM candidates other
                     WHERE other.t = c.t
                       AND other.owner_id <> c.owner_id
                )",
        )
        .bind(&memories)
        .bind(&owner_ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(map_err)?;
        homes.extend(
            rows.into_iter()
                .map(|(t, owner, kind, schema)| ((false, t), (owner, kind, Some(schema)))),
        );
    }
    if !goals.is_empty() {
        let rows: Vec<(uuid::Uuid, uuid::Uuid)> = sqlx::query_as(
            "SELECT g.t, g.owner_id
               FROM proxiima_core.goal g
              WHERE g.t = ANY($1::uuid[]) AND g.owner_id = ANY($2::uuid[])",
        )
        .bind(&goals)
        .bind(&owner_ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(map_err)?;
        homes.extend(
            rows.into_iter()
                .map(|(t, owner)| ((true, t), (owner, "goal".to_owned(), None))),
        );
    }

    entities
        .iter()
        .map(|entity| {
            let key = match entity {
                EntityId::Memory(id) => (false, id.into_inner()),
                EntityId::Goal(id) => (true, id.into_inner()),
            };
            homes
                .get(&key)
                .map(|(owner, kind, schema)| {
                    Ok((
                        OwnerId::new(*owner),
                        entity_kind(kind)?,
                        schema.clone().map(SchemaId::new),
                    ))
                })
                .transpose()
        })
        .collect()
}

/// # Errors
///
/// Returns `Internal` on sqlx failure.
pub(crate) async fn home_owner<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    entity: EntityId,
) -> Result<Option<OwnerId>, StorageError> {
    // The entity discriminant is authorization data, exactly as in
    // `visible_home_owner`. A union over both spines on the bare uuid lets a
    // caller relabel a Memory as a Goal (or vice versa) and resolve the owner
    // space off the wrong spine, so each arm asks its own spine only.
    let row: Option<uuid::Uuid> = match entity {
        EntityId::Memory(memory_id) => {
            sqlx::query_scalar(
                "SELECT m.owner_id
               FROM proxiima_core.memory m /* FORGET-POLICY: includes-forgotten (transfer moves forgotten rows too) */
              WHERE m.t = $1",
            )
            .bind(memory_id.into_inner())
            .fetch_optional(executor)
            .await
        }
        EntityId::Goal(goal_id) => {
            sqlx::query_scalar(
                "SELECT g.owner_id
               FROM proxiima_core.goal g
              WHERE g.t = $1",
            )
            .bind(goal_id.into_inner())
            .fetch_optional(executor)
            .await
        }
    }
    .map_err(map_err)?;

    Ok(row.map(OwnerId::new))
}

#[cfg(test)]
#[path = "group_manager_race_pg_tests.rs"]
mod group_manager_race_pg_tests;

#[cfg(test)]
mod tests {
    use super::*;

    const RUNGS: [PgAccessCeiling; 5] = [
        PgAccessCeiling::None,
        PgAccessCeiling::Fact,
        PgAccessCeiling::Abstraction,
        PgAccessCeiling::Perspective,
        PgAccessCeiling::Goal,
    ];

    /// The fourth stored rung is Goal work on a read column and
    /// administration on a write column; below it the columns are the
    /// memory ceilings.
    #[test]
    fn a_stored_role_reads_the_fourth_rung_as_goal_work_and_administration() {
        for (r, read) in RUNGS.into_iter().enumerate() {
            for write in RUNGS.into_iter().take(r + 1) {
                let role = member_role(read, write, true).expect("write within read is a role");
                assert_eq!(role.read_ceiling(), read.memory());
                assert_eq!(role.write_ceiling(), write.memory());
                assert_eq!(role.works_goals(), read == PgAccessCeiling::Goal);
                assert_eq!(role.reads_goals(), read == PgAccessCeiling::Goal);
                assert_eq!(role.administers(), write == PgAccessCeiling::Goal);
                assert!(role.manages());
            }
        }
        assert!(member_role(PgAccessCeiling::Fact, PgAccessCeiling::Abstraction, false).is_err());
    }

    /// A delegated ceiling is stored exactly or not at all: every stored
    /// pair writes back to itself, and a ceiling with no stored form is
    /// refused rather than recorded as a different one.
    #[test]
    fn a_delegated_ceiling_is_stored_exactly_or_refused() {
        for (r, read) in RUNGS.into_iter().enumerate() {
            for write in RUNGS.into_iter().take(r + 1) {
                let role = member_role(read, write, false).expect("write within read is a role");
                assert_eq!(
                    delegated_ceiling_columns(role).expect("a stored ceiling has columns"),
                    (read, write)
                );
            }
        }
        for unstorable in [
            Role::editor().with_goals(GoalAccess::Reads),
            Role::ingest().with_goals(GoalAccess::Works),
            Role::editor().with_goals(GoalAccess::None).administering(),
            Role::viewer().administering(),
        ] {
            assert!(
                matches!(
                    delegated_ceiling_columns(unstorable),
                    Err(StorageError::ConstraintViolation(_))
                ),
                "{unstorable:?} has no stored form"
            );
        }
    }
}
