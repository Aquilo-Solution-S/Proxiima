//! The mutable core functions, triggers, policies and views pass.

use proxiima_core::{RELEASE_VERSION, Relation, SqlText, StorageError};
use sha2::{Digest, Sha384};
use sqlx::PgConnection;

use crate::VectorStorage;
use crate::error::internal;

pub const FUNCTIONS: &str = include_str!("../definitions/functions.sql");
pub const TRIGGERS: &str = include_str!("../definitions/triggers.sql");
pub const POLICIES: &str = include_str!("../definitions/policies.sql");
/// The read-only views spiice and host SQL may name.
pub const VIEWS: &str = include_str!("../definitions/views.sql");
/// The vector candidate functions: applied only to a store that keeps its
/// vectors in pgvector.
pub const VECTOR: &str = include_str!("../definitions/vector.sql");

/// `proxiima_core.relation_roles()`: the role each membership relation holds,
/// rendered from [`Relation::ALL`] and [`Relation::role`] so the projection
/// fold in SQL and the Rust role cannot disagree. It is part of the
/// definitions, so a change to a relation's role re-applies them.
#[doc(hidden)]
#[must_use]
pub fn relation_roles_sql() -> SqlText {
    let mut sql = SqlText::new(
        "-- Generated from Relation::ALL and Relation::role().
CREATE OR REPLACE FUNCTION proxiima_core.relation_roles()
 RETURNS TABLE(relation proxiima_core.membership_relation,
               read_ceiling proxiima_core.memory_ceiling,
               write_ceiling proxiima_core.memory_ceiling,
               goal_access proxiima_core.goal_access, administers boolean, manage boolean)
 LANGUAGE sql IMMUTABLE
 SET search_path TO 'pg_catalog', 'pg_temp'
AS $function$
    VALUES ",
    );
    for (index, relation) in Relation::ALL.into_iter().enumerate() {
        let role = relation.role();
        if index > 0 {
            sql.push(",\n           ");
        }
        sql.push("('")
            .push(relation.as_str())
            .push("'::proxiima_core.membership_relation, '")
            .push(role.read_ceiling().as_str())
            .push("'::proxiima_core.memory_ceiling, '")
            .push(role.write_ceiling().as_str())
            .push("'::proxiima_core.memory_ceiling, '")
            .push(role.goals().as_str())
            .push("'::proxiima_core.goal_access, ")
            .push(if role.administers() { "true" } else { "false" })
            .push(", ")
            .push(if role.manages() { "true" } else { "false" })
            .push(")");
    }
    sql.push(
        "
$function$;
REVOKE ALL ON FUNCTION proxiima_core.relation_roles() FROM PUBLIC;
",
    );
    sql
}

/// The definitions a store of this kind holds, in application order.
fn files(vectors: VectorStorage) -> Vec<SqlText> {
    let mut files = vec![
        relation_roles_sql(),
        SqlText::new(FUNCTIONS),
        SqlText::new(TRIGGERS),
        SqlText::new(POLICIES),
        SqlText::new(VIEWS),
    ];
    if vectors == VectorStorage::Local {
        files.push(SqlText::new(VECTOR));
    }
    files
}

fn checksum(vectors: VectorStorage) -> Vec<u8> {
    let mut digest = Sha384::new();
    for (index, sql) in files(vectors).iter().enumerate() {
        if index > 0 {
            digest.update([0]);
        }
        digest.update(sql.as_str().as_bytes());
    }
    digest.finalize().to_vec()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Release(u64, u64, u64);

impl Release {
    fn parse(value: &str) -> Result<Self, StorageError> {
        let invalid = || StorageError::Internal(format!("invalid definitions release {value:?}"));
        let mut parts = value.split('.');
        let mut component = || {
            let part = parts.next().ok_or_else(invalid)?;
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid());
            }
            part.parse::<u64>().map_err(|_| invalid())
        };
        let release = Self(component()?, component()?, component()?);
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(release)
    }
}

enum AppliedDefinitions {
    Current { checksum: String },
    Newer,
}

impl AppliedDefinitions {
    async fn read(
        connection: &mut PgConnection,
        policy: crate::MissingMigrationPolicy,
    ) -> Result<Option<Self>, StorageError> {
        let exists: bool = sqlx::query_scalar(
            "SELECT to_regprocedure('proxiima_core.definitions_applied()') IS NOT NULL",
        )
        .fetch_one(&mut *connection)
        .await
        .map_err(internal)?;
        if !exists {
            return Ok(None);
        }
        let record: Option<(String, String)> =
            sqlx::query_as("SELECT checksum, release FROM proxiima_core.definitions_applied()")
                .fetch_optional(&mut *connection)
                .await
                .map_err(internal)?;
        let Some((checksum, release)) = record else {
            return Ok(None);
        };
        let recorded = Release::parse(&release)?;
        let binary = Release::parse(RELEASE_VERSION)?;
        if recorded > binary {
            if policy == crate::MissingMigrationPolicy::Reject {
                return Err(StorageError::Internal(format!(
                    "database definitions were applied by newer release {release}; this binary is {RELEASE_VERSION}"
                )));
            }
            return Ok(Some(AppliedDefinitions::Newer));
        }
        Ok(Some(Self::Current { checksum }))
    }
}

/// The SHA-384 checksum used by the definitions record and test templates.
#[doc(hidden)]
#[must_use]
pub fn checksum_bytes(vectors: VectorStorage) -> Vec<u8> {
    checksum(vectors)
}

/// The definitions checksum as the record function returns it.
#[doc(hidden)]
#[must_use]
pub fn checksum_hex(vectors: VectorStorage) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    checksum(vectors)
        .iter()
        .flat_map(|byte| {
            [
                char::from(HEX[usize::from(byte >> 4)]),
                char::from(HEX[usize::from(byte & 15)]),
            ]
        })
        .collect()
}

/// Every table of the core schema, locked by the server, which quotes each
/// name itself (`%I`): the statement carries no name of ours.
const LOCK_CORE_TABLES_SQL: &str = "DO $$
DECLARE
    core_table record;
BEGIN
    FOR core_table IN
        SELECT n.nspname, c.relname
          FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'proxiima_core' AND c.relkind IN ('r', 'p')
         ORDER BY c.relname
    LOOP
        EXECUTE format('LOCK TABLE %I.%I IN SHARE ROW EXCLUSIVE MODE',
                       core_table.nspname, core_table.relname);
    END LOOP;
END
$$";

/// Replace the definitions in the caller's migration transaction when needed.
///
/// # Errors
///
/// Refuses a newer or malformed recorded release and propagates SQL failures.
pub(crate) async fn apply(
    connection: &mut PgConnection,
    policy: crate::MissingMigrationPolicy,
    vectors: VectorStorage,
) -> Result<(), StorageError> {
    if current_matches_with_policy(connection, policy, vectors).await? {
        return Ok(());
    }
    sqlx::raw_sql(LOCK_CORE_TABLES_SQL)
        .execute(&mut *connection)
        .await
        .map_err(internal)?;
    sqlx::query("SET LOCAL check_function_bodies = off")
        .execute(&mut *connection)
        .await
        .map_err(internal)?;
    for sql in files(vectors) {
        sqlx::raw_sql(sql)
            .execute(&mut *connection)
            .await
            .map_err(internal)?;
    }
    let mut record = SqlText::new(
        "CREATE OR REPLACE FUNCTION proxiima_core.definitions_applied()
         RETURNS TABLE(checksum text, release text) LANGUAGE sql IMMUTABLE
         AS $$ SELECT '",
    );
    record
        .push_hex(&checksum(vectors))
        .push("'::text, '")
        .push(RELEASE_VERSION)
        .push("'::text $$");
    sqlx::raw_sql(record)
        .execute(&mut *connection)
        .await
        .map_err(internal)?;
    Ok(())
}

/// Whether the recorded definitions checksum matches this binary.
///
/// # Errors
///
/// Refuses a newer or malformed recorded release and propagates catalog errors.
pub async fn current_matches(
    connection: &mut PgConnection,
    vectors: VectorStorage,
) -> Result<bool, StorageError> {
    current_matches_with_policy(connection, crate::MissingMigrationPolicy::Reject, vectors).await
}

/// Whether the recorded definitions are current under the missing-version policy.
///
/// A newer recorded release is current under [`crate::MissingMigrationPolicy::Allow`]
/// and is refused under [`crate::MissingMigrationPolicy::Reject`].
///
/// # Errors
///
/// Returns malformed-release, newer-release-under-`Reject`, or catalog errors.
#[doc(hidden)]
pub async fn current_matches_with_policy(
    connection: &mut PgConnection,
    policy: crate::MissingMigrationPolicy,
    vectors: VectorStorage,
) -> Result<bool, StorageError> {
    Ok(match AppliedDefinitions::read(connection, policy).await? {
        Some(AppliedDefinitions::Newer) => true,
        Some(AppliedDefinitions::Current { checksum }) => checksum == checksum_hex(vectors),
        None => false,
    })
}

#[cfg(test)]
mod tests {
    use super::{LOCK_CORE_TABLES_SQL, Release};
    use crate::test_fixtures::fresh_pg;

    /// The one static statement locks every core table, quoted by the server.
    #[tokio::test]
    async fn the_lock_statement_locks_every_core_table() {
        let (pg, _db) = fresh_pg("definitions_lock").await;
        let mut tx = pg.pool_for_tests().begin().await.expect("begin");
        sqlx::raw_sql(LOCK_CORE_TABLES_SQL)
            .execute(&mut *tx)
            .await
            .expect("lock the core tables");
        let (tables, locked): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = 'proxiima_core' AND c.relkind IN ('r', 'p')),
                    (SELECT count(*) FROM pg_locks l JOIN pg_class c ON c.oid = l.relation
                       JOIN pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = 'proxiima_core' AND c.relkind IN ('r', 'p')
                        AND l.pid = pg_backend_pid() AND l.mode = 'ShareRowExclusiveLock')",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("count");
        assert!(tables > 0, "the core schema has tables");
        assert_eq!(locked, tables);
    }

    #[test]
    fn release_parser_refuses_malformed_database_values() {
        for invalid in [
            "",
            "0.0",
            "0.0.36.1",
            "0.0.+36",
            "v0.0.36",
            "0..36",
            "0.0.18446744073709551616",
        ] {
            assert!(Release::parse(invalid).is_err(), "{invalid:?}");
        }
        assert!(Release::parse("0.0.36").unwrap() > Release::parse("0.0.9").unwrap());
    }
}
