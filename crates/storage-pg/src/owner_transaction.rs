//! Owner-scoped transactions for a spiice's own tables: the service the
//! runtime publishes, and the transaction it begins, which runs the
//! statements a spiice builds with sqlx and nothing else.

use proxiima_core::{OwnerScope, StorageError};
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgArguments, PgQueryResult, PgRow, PgStatement};
use sqlx::query::{Query, QueryAs, QueryScalar};
use sqlx::{Execute, Executor, FromRow, PgPool, Postgres, SqlStr, Transaction};

use crate::error::map_err;
use crate::owner_scope::begin_owner_transaction;
use crate::sidecars::refuse_core_schema_sql;

/// Begins [`OwnerTransaction`]s on the serving pool, which it keeps to
/// itself. Made by [`PgStorage::owner_transactions`](crate::PgStorage::owner_transactions).
#[derive(Debug, Clone)]
pub struct OwnerTransactions {
    pool: PgPool,
}

impl OwnerTransactions {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// A service over a pool that never connects, for a composition test
    /// that publishes the service and begins no transaction.
    ///
    /// # Panics
    ///
    /// Only if `SQLx` rejects the fixed test URL.
    #[cfg(feature = "testkit")]
    #[must_use]
    pub fn for_tests() -> Self {
        Self::new(
            PgPool::connect_lazy("postgres://invalid/proxiima")
                .expect("the test pool URL is syntactically valid"),
        )
    }

    /// Begin a transaction under `scope`, as
    /// [`begin_owner_transaction`] does.
    ///
    /// # Errors
    ///
    /// Refuses expired authority and propagates database errors.
    pub async fn begin(&self, scope: &OwnerScope) -> Result<OwnerTransaction, StorageError> {
        Ok(OwnerTransaction {
            transaction: begin_owner_transaction(&self.pool, scope).await?,
        })
    }
}

/// A transaction under one caller's owner scope, for statements on a
/// spiice's own tables.
///
/// It runs `sqlx::query`, `sqlx::query_as` and `sqlx::query_scalar`
/// statements ([`OwnerStatement`]) and gives no access to its connection.
/// Each call runs one data statement: its text starts with `SELECT`, `WITH`,
/// `INSERT`, `UPDATE`, `DELETE` or `MERGE`, and it is sent as a prepared
/// statement, which holds one command. A statement that names the core
/// schema is refused before it runs, by the rule the sidecar read context
/// applies. The transaction's boundaries are its own: [`Self::commit`]
/// commits; dropping it rolls it back.
pub struct OwnerTransaction {
    transaction: Transaction<'static, Postgres>,
}

impl std::fmt::Debug for OwnerTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerTransaction").finish_non_exhaustive()
    }
}

impl OwnerTransaction {
    /// Every row `statement` returns.
    ///
    /// # Errors
    ///
    /// The refusal of a statement that is not one data statement or that
    /// names the core schema, a bind that does not encode, or the database's
    /// error.
    pub async fn fetch_all<Q: OwnerStatement>(
        &mut self,
        statement: Q,
    ) -> Result<Vec<Q::Row>, StorageError> {
        let checked = Checked::new(statement)?;
        let rows = Executor::fetch_all(&mut *self.transaction, checked)
            .await
            .map_err(map_err)?;
        rows.into_iter()
            .map(|row| Q::decode(row).map_err(map_err))
            .collect()
    }

    /// The first row `statement` returns, if any.
    ///
    /// # Errors
    ///
    /// As [`Self::fetch_all`].
    pub async fn fetch_optional<Q: OwnerStatement>(
        &mut self,
        statement: Q,
    ) -> Result<Option<Q::Row>, StorageError> {
        let checked = Checked::new(statement)?;
        let row = Executor::fetch_optional(&mut *self.transaction, checked)
            .await
            .map_err(map_err)?;
        row.map(|row| Q::decode(row).map_err(map_err)).transpose()
    }

    /// The one row `statement` returns.
    ///
    /// # Errors
    ///
    /// As [`Self::fetch_all`]; a statement that returns no row is an error.
    /// Use [`Self::fetch_optional`] where none is an answer.
    pub async fn fetch_one<Q: OwnerStatement>(
        &mut self,
        statement: Q,
    ) -> Result<Q::Row, StorageError> {
        let checked = Checked::new(statement)?;
        let row = Executor::fetch_one(&mut *self.transaction, checked)
            .await
            .map_err(map_err)?;
        Q::decode(row).map_err(map_err)
    }

    /// Run `statement` for its effect.
    ///
    /// # Errors
    ///
    /// As [`Self::fetch_all`].
    pub async fn execute<Q: OwnerStatement>(
        &mut self,
        statement: Q,
    ) -> Result<PgQueryResult, StorageError> {
        let checked = Checked::new(statement)?;
        Executor::execute(&mut *self.transaction, checked)
            .await
            .map_err(map_err)
    }

    /// Commit every statement run on this transaction.
    ///
    /// # Errors
    ///
    /// The database's error; the transaction is then rolled back.
    pub async fn commit(self) -> Result<(), StorageError> {
        self.transaction.commit().await.map_err(map_err)
    }
}

mod sealed {
    use sqlx::error::BoxDynError;
    use sqlx::postgres::PgArguments;
    use sqlx::{Execute, Postgres, SqlStr};

    /// What a statement takes apart into: its binds, whether its prepared
    /// form is cached, and its text.
    pub type Parts = (Result<Option<PgArguments>, BoxDynError>, bool, SqlStr);

    pub trait Sealed {
        fn into_parts(self) -> Parts;
    }

    pub fn parts<'q>(mut statement: impl Execute<'q, Postgres>) -> Parts {
        let arguments = statement.take_arguments();
        let persistent = statement.persistent();
        (arguments, persistent, statement.sql())
    }
}

/// A statement an [`OwnerTransaction`] runs: a `sqlx::query`,
/// `sqlx::query_as` or `sqlx::query_scalar` value with its binds. `Row` is
/// what one returned row decodes to: a `PgRow`, the `query_as` type, or the
/// `query_scalar` value.
pub trait OwnerStatement: sealed::Sealed + Send {
    /// One returned row, decoded.
    type Row: Send;

    #[doc(hidden)]
    fn decode(row: PgRow) -> Result<Self::Row, sqlx::Error>;
}

impl sealed::Sealed for Query<'_, Postgres, PgArguments> {
    fn into_parts(self) -> sealed::Parts {
        sealed::parts(self)
    }
}

impl OwnerStatement for Query<'_, Postgres, PgArguments> {
    type Row = PgRow;

    fn decode(row: PgRow) -> Result<PgRow, sqlx::Error> {
        Ok(row)
    }
}

impl<O: Send> sealed::Sealed for QueryAs<'_, Postgres, O, PgArguments> {
    fn into_parts(self) -> sealed::Parts {
        sealed::parts(self)
    }
}

impl<O> OwnerStatement for QueryAs<'_, Postgres, O, PgArguments>
where
    O: Send + for<'r> FromRow<'r, PgRow>,
{
    type Row = O;

    fn decode(row: PgRow) -> Result<O, sqlx::Error> {
        O::from_row(&row)
    }
}

impl<O: Send> sealed::Sealed for QueryScalar<'_, Postgres, O, PgArguments> {
    fn into_parts(self) -> sealed::Parts {
        sealed::parts(self)
    }
}

impl<O> OwnerStatement for QueryScalar<'_, Postgres, O, PgArguments>
where
    O: Send,
    (O,): for<'r> FromRow<'r, PgRow>,
{
    type Row = O;

    fn decode(row: PgRow) -> Result<O, sqlx::Error> {
        <(O,)>::from_row(&row).map(|(value,)| value)
    }
}

/// The verbs a statement on an [`OwnerTransaction`] starts with. Transaction
/// control, session settings and DDL start with none of them.
const DATA_VERBS: [&str; 6] = ["select", "with", "insert", "update", "delete", "merge"];

/// Refuse a statement whose first word is not one of [`DATA_VERBS`].
fn refuse_other_than_data_statement(sql: &str) -> Result<(), StorageError> {
    let verb = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or_default();
    if DATA_VERBS
        .iter()
        .any(|allowed| verb.eq_ignore_ascii_case(allowed))
    {
        Ok(())
    } else {
        Err(StorageError::Internal(
            "an owner transaction runs SELECT, WITH, INSERT, UPDATE, DELETE or MERGE statements"
                .to_string(),
        ))
    }
}

/// A statement whose text passed [`refuse_other_than_data_statement`] and
/// [`refuse_core_schema_sql`], with the binds and caching choice it was
/// built with. It always carries binds (none is an empty set), so it runs as
/// a prepared statement.
struct Checked {
    sql: SqlStr,
    arguments: Option<PgArguments>,
    persistent: bool,
}

impl Checked {
    fn new(statement: impl sealed::Sealed) -> Result<Self, StorageError> {
        let (arguments, persistent, sql) = statement.into_parts();
        let arguments = arguments.map_err(|error| map_err(sqlx::Error::Encode(error)))?;
        refuse_other_than_data_statement(sql.as_str())?;
        refuse_core_schema_sql(sql.as_str())?;
        Ok(Self {
            sql,
            arguments: Some(arguments.unwrap_or_default()),
            persistent,
        })
    }
}

impl Execute<'_, Postgres> for Checked {
    fn sql(self) -> SqlStr {
        self.sql
    }

    fn statement(&self) -> Option<&PgStatement> {
        None
    }

    fn take_arguments(&mut self) -> Result<Option<PgArguments>, BoxDynError> {
        Ok(self.arguments.take())
    }

    fn persistent(&self) -> bool {
        self.persistent
    }
}

#[cfg(test)]
mod tests {
    use proxiima_core::StorageError;

    use super::Checked;

    #[test]
    fn a_statement_that_names_the_core_schema_is_refused_before_it_runs() {
        let refused = Checked::new(
            sqlx::query_as::<_, (uuid::Uuid,)>("SELECT t FROM PROXIIMA_CORE.memory WHERE t = $1")
                .bind(uuid::Uuid::nil()),
        )
        .err()
        .expect("core-schema SQL is refused");
        assert!(
            matches!(&refused, StorageError::Internal(message) if message.contains("proxiima_core")),
            "{refused:?}"
        );
        assert!(Checked::new(sqlx::query("SELECT 1 FROM proxiima_example.notebooks")).is_ok());
    }

    #[test]
    fn a_statement_that_is_not_a_data_statement_is_refused_before_it_runs() {
        for sql in [
            "COMMIT",
            "  rollback",
            "/* note */ COMMIT",
            "SAVEPOINT a",
            "SET LOCAL role = 'postgres'",
            "DO $$ BEGIN END $$",
            "",
        ] {
            let refused = Checked::new(sqlx::query(sql))
                .err()
                .unwrap_or_else(|| panic!("{sql:?} is refused"));
            assert!(
                matches!(&refused, StorageError::Internal(message) if message.contains("owner transaction")),
                "{sql:?}: {refused:?}"
            );
        }
        for sql in [
            "select 1",
            "\n  WITH a AS (SELECT 1) SELECT * FROM a",
            "INSERT INTO t VALUES (1)",
            "Update t SET a = 1",
            "DELETE FROM t",
            "MERGE INTO t USING s ON true WHEN MATCHED THEN DO NOTHING",
        ] {
            assert!(Checked::new(sqlx::query(sql)).is_ok(), "{sql:?}");
        }
    }
}
