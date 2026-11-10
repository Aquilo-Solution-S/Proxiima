//! Storage is the only writer of the transaction-local owner scope.

use proxiima_core::{AccessKind, OwnerId, OwnerScope, StorageError};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::error::map_err;

/// Begin a transaction guarded by authenticated owner authority.
///
/// The returned transaction must execute every statement of the operation,
/// including sidecar reads. Pool queries do not inherit its scope. Each retry
/// must call this function again; commit and rollback remove the local settings.
///
/// # Errors
///
/// Refuses expired authority and propagates database errors without issuing
/// application queries outside the scoped transaction.
pub async fn begin_owner_transaction(
    pool: &PgPool,
    scope: &OwnerScope,
) -> Result<Transaction<'static, Postgres>, StorageError> {
    let mut transaction = pool.begin().await.map_err(map_err)?;
    bind_owner_scope(&mut transaction, scope).await?;
    Ok(transaction)
}

fn owner_ids(owners: Vec<OwnerId>) -> Vec<Uuid> {
    owners.into_iter().map(OwnerId::into_inner).collect()
}

pub(crate) fn require_owner_scope(scope: Option<&OwnerScope>) -> Result<&OwnerScope, StorageError> {
    scope.ok_or_else(|| {
        StorageError::ConstraintViolation(
            "authenticated OwnerScope is required for owner-scoped storage".into(),
        )
    })
}

pub(crate) async fn finish_transaction<T>(
    transaction: Transaction<'static, Postgres>,
    result: Result<T, StorageError>,
) -> Result<T, StorageError> {
    match result {
        Ok(value) => {
            transaction.commit().await.map_err(map_err)?;
            Ok(value)
        }
        Err(error) => {
            let _ = transaction.rollback().await;
            Err(error)
        }
    }
}

/// The transaction-local owner settings of `scope`, name and text value:
/// the one list [`bind_owner_scope`] writes and [`live_memory_owners_as`]
/// hands to `proxiima_core.live_memory_owners_as`.
fn owner_scope_settings(scope: &OwnerScope) -> [(&'static str, String); 12] {
    // The text form PostgreSQL gives `uuid[]::text`, which the RLS policies
    // cast back with `::uuid[]`.
    fn uuid_array(owners: Vec<OwnerId>) -> String {
        let ids: Vec<String> = owner_ids(owners).iter().map(Uuid::to_string).collect();
        format!("{{{}}}", ids.join(","))
    }
    [
        ("app.proxiima_scope", "owner".to_owned()),
        (
            "app.owner",
            uuid_array(scope.readable_owners(AccessKind::Fact)),
        ),
        (
            "app.read_abstraction",
            uuid_array(scope.readable_owners(AccessKind::Abstraction)),
        ),
        (
            "app.read_perspective",
            uuid_array(scope.readable_owners(AccessKind::Perspective)),
        ),
        (
            "app.read_goal",
            uuid_array(scope.readable_owners(AccessKind::Goal)),
        ),
        (
            "app.write_owner",
            uuid_array(scope.writable_owners(AccessKind::Fact)),
        ),
        (
            "app.write_abstraction",
            uuid_array(scope.writable_owners(AccessKind::Abstraction)),
        ),
        (
            "app.write_perspective",
            uuid_array(scope.writable_owners(AccessKind::Perspective)),
        ),
        (
            "app.write_goal",
            uuid_array(scope.writable_owners(AccessKind::Goal)),
        ),
        ("app.manage_owner", uuid_array(scope.managed_owners())),
        ("app.subject", scope.subject().into_inner().to_string()),
        (
            "app.goal_subject",
            scope
                .goal_principal()
                .map(|principal| principal.subject().into_inner().to_string())
                .unwrap_or_default(),
        ),
    ]
}

fn refuse_expired(scope: &OwnerScope) -> Result<(), StorageError> {
    if scope.is_expired() {
        return Err(StorageError::ConstraintViolation(
            "authenticated owner scope has expired".into(),
        ));
    }
    Ok(())
}

/// `(t, owner_id)` of each live memory of `ids` that `reader` may read,
/// under `reader`'s owner RLS, inside `connection`'s transaction. With no
/// `reader`, the transaction's own scope judges.
///
/// For the host read set (#677): a write is narrowed to its space, but the
/// session may have read anything its caller can read. One statement: the
/// function sets the reader scope in a subtransaction it always rolls back,
/// which puts the transaction's own scope back before the statement ends,
/// so no later statement of the transaction can run under the reader scope,
/// even when this future is dropped mid-probe.
///
/// # Errors
///
/// Expired reader authority, or a database error.
pub(crate) async fn live_memory_owners_as(
    connection: &mut PgConnection,
    reader: Option<&OwnerScope>,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, Uuid)>, StorageError> {
    let (names, values): (Vec<&str>, Vec<String>) = match reader {
        Some(reader) => {
            refuse_expired(reader)?;
            owner_scope_settings(reader).into_iter().unzip()
        }
        None => (Vec::new(), Vec::new()),
    };
    sqlx::query_as(
        "SELECT memory_t, memory_owner
           FROM proxiima_core.live_memory_owners_as($1::text[], $2::text[], $3::uuid[])",
    )
    .bind(names)
    .bind(values)
    .bind(ids)
    .fetch_all(connection)
    .await
    .map_err(map_err)
}

pub(crate) async fn bind_owner_scope(
    connection: &mut PgConnection,
    scope: &OwnerScope,
) -> Result<(), StorageError> {
    refuse_expired(scope)?;
    // No header, owner text, or caller-supplied role can enter this
    // function in place of the witness.
    let (names, values): (Vec<&str>, Vec<String>) = owner_scope_settings(scope).into_iter().unzip();
    sqlx::query(
        "SELECT set_config(s.name, s.value, true)
           FROM unnest($1::text[], $2::text[]) AS s(name, value)",
    )
    .bind(names)
    .bind(values)
    .execute(connection)
    .await
    .map_err(map_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use proxiima_core::storage_ports::OwnerWritePermit;
    use proxiima_core::test_fixtures::owner_scope_for_tests;
    use proxiima_core::{AccessKind, OwnerId, UserId};
    use uuid::Uuid;

    use super::{begin_owner_transaction, live_memory_owners_as};
    use crate::test_fixtures::create_core_db;
    use crate::verbs::forget::forget_pg_tests::{draft, ingest_stamped};

    async fn owner_setting(tx: &mut sqlx::PgConnection) -> String {
        sqlx::query_scalar("SELECT current_setting('app.owner')")
            .fetch_one(tx)
            .await
            .expect("app.owner")
    }

    /// The probe reads under the wider reader scope, and a probe dropped
    /// while it waits inside the database leaves no later statement of the
    /// write's transaction under that scope: the probe's own statement
    /// restores the settings, not a second statement the dropped future
    /// would never send (#677).
    #[tokio::test]
    async fn a_dropped_probe_leaves_the_write_scope_in_place() {
        let database = format!("proxiima_test_{}", Uuid::now_v7().simple());
        create_core_db(&database)
            .await
            .expect("PG required for test");
        let (runtime_url, _) = proxiima_pg_testkit::split_role_urls(&database)
            .await
            .expect("split roles");
        let admin = sqlx::PgPool::connect(&proxiima_pg_testkit::db_url(&database))
            .await
            .expect("admin pool");
        // The runtime role: owner RLS applies to it.
        let pool = &sqlx::PgPool::connect(&runtime_url)
            .await
            .expect("runtime pool");
        let writer = OwnerId::from(UserId::new(Uuid::now_v7()));
        let other = OwnerId::from(UserId::new(Uuid::now_v7()));
        let theirs = ingest_stamped(
            &admin,
            &OwnerWritePermit::new_for_tests(other, AccessKind::Fact),
            &draft(None),
            &[],
            &[],
        )
        .await
        .expect("a Fact of the other owner")
        .memory_id
        .into_inner();
        let reader = owner_scope_for_tests(&[writer, other]);
        let mut tx = begin_owner_transaction(pool, &owner_scope_for_tests(&[writer]))
            .await
            .expect("write transaction");
        let narrow = owner_setting(&mut tx).await;

        // Control: the write scope cannot see the other owner's Fact, the
        // reader scope can, and the write scope is back afterwards.
        assert!(
            live_memory_owners_as(&mut tx, None, &[theirs])
                .await
                .expect("probe")
                .is_empty()
        );
        assert_eq!(
            live_memory_owners_as(&mut tx, Some(&reader), &[theirs])
                .await
                .expect("probe"),
            [(theirs, other.into_inner())]
        );
        assert_eq!(owner_setting(&mut tx).await, narrow);

        // Hold the memory table so the probe blocks inside the function,
        // after it bound the reader scope, then drop it there.
        let mut holder = admin.begin().await.expect("holder");
        sqlx::query("LOCK TABLE proxiima_core.memory IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *holder)
            .await
            .expect("lock");
        let dropped = tokio::time::timeout(
            Duration::from_millis(300),
            live_memory_owners_as(&mut tx, Some(&reader), &[theirs]),
        )
        .await;
        assert!(dropped.is_err(), "the probe waited and was dropped");
        holder.rollback().await.expect("release");

        assert_eq!(
            owner_setting(&mut tx).await,
            narrow,
            "the next statement runs under the write scope"
        );
        assert!(
            live_memory_owners_as(&mut tx, None, &[theirs])
                .await
                .expect("probe")
                .is_empty(),
            "and RLS still hides the other owner's Fact"
        );
        tx.rollback().await.expect("rollback");
        pool.close().await;
        admin.close().await;
        let _ = proxiima_pg_testkit::drop_db(&database).await;
    }
}
