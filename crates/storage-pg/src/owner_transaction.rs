//! Owner-scoped transactions for a spiice's own tables: the service the
//! runtime publishes, and the transaction it begins, which runs the
//! statements a spiice builds with sqlx and nothing else.

use proxiima_core::{
    AuthzContext, OwnerId, OwnerScope, SpiiceServiceError, SpiiceServices, StorageError, ToolCtx,
    ToolError,
};
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgArguments, PgQueryResult, PgRow, PgStatement};
use sqlx::query::{Query, QueryAs, QueryScalar};
use sqlx::{Execute, Executor, FromRow, PgPool, Postgres, SqlStr, Transaction};

use crate::error::map_err;
use crate::owner_scope::begin_owner_transaction;
use crate::sidecars::{refuse_core_schema_sql, sql_identifiers};

/// Opens an extra-table transaction for the served owner under the caller's authority.
#[derive(Clone, Debug)]
pub struct HostOwnerTransactions(OwnerTransactions);

impl HostOwnerTransactions {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self(OwnerTransactions { pool })
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

    /// Bind the caller's authority for the served owner before opening a transaction.
    ///
    /// # Errors
    ///
    /// [`Forbidden`](StorageError::Forbidden) when the context cannot serve the owner or has expired.
    /// Propagates database errors.
    pub async fn begin(
        &self,
        caller: &AuthzContext,
        owner: OwnerId,
    ) -> Result<OwnerTransaction, StorageError> {
        self.0.begin_for_owner(caller, owner).await
    }

    /// Install the tool's owner-transaction service in the composed service set.
    ///
    /// # Errors
    ///
    /// Returns an error if the service is already installed.
    pub fn publish_to_tools(
        &self,
        services: &mut SpiiceServices,
    ) -> Result<(), SpiiceServiceError> {
        services.try_insert(self.0.clone())
    }
}

/// Begin a transaction on the spiice's own tables for the owner served by `ctx`.
///
/// Every statement of the operation runs on the returned
/// [`OwnerTransaction`]; it applies owner RLS as well as the statement's own
/// filter, refuses a statement that names the core schema, and rolls back
/// unless committed. Use the Engine's [`proxiima_core::engine::UnitOfWork`]
/// for Fact and state writes.
///
/// # Errors
///
/// [`ToolError::Unavailable`] when the host serving `ctx` publishes no
/// owner-transaction service. [`ToolError::Storage`] carries a scope refusal
/// or a database error when the transaction cannot begin.
///
/// ```compile_fail,E0432
/// use proxiima_storage_pg::OwnerTransactions;
/// ```
pub async fn owner_transaction(ctx: &ToolCtx) -> Result<OwnerTransaction, ToolError> {
    let transactions = ctx.service::<OwnerTransactions>().ok_or_else(|| {
        ToolError::Unavailable(
            "owner-scoped transactions are not configured: the host serving this tool \
             publishes no OwnerTransactions service"
                .into(),
        )
    })?;
    Ok(transactions
        .begin_for_owner(ctx.authz(), ctx.owner())
        .await?)
}

/// The service installed in tool contexts. Its raw scope entry stays in this crate.
#[derive(Debug, Clone)]
pub(crate) struct OwnerTransactions {
    pool: PgPool,
}

impl OwnerTransactions {
    async fn begin_for_owner(
        &self,
        caller: &AuthzContext,
        owner: OwnerId,
    ) -> Result<OwnerTransaction, StorageError> {
        // `Proxiima.OwnerAuthority.ScopeRestriction.confinement`.
        let caller = caller.clone().narrowed_to_owner(owner);
        let scope = caller
            .as_ref()
            .and_then(AuthzContext::owner_scope)
            .ok_or_else(|| {
                StorageError::Forbidden("authenticated scope for the served owner required".into())
            })?;
        self.begin(scope).await
    }

    /// Begin a transaction under `scope`, as
    /// [`begin_owner_transaction`] does.
    ///
    /// # Errors
    ///
    /// Refuses expired authority and propagates database errors.
    pub(crate) async fn begin(&self, scope: &OwnerScope) -> Result<OwnerTransaction, StorageError> {
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
/// applies. Session settings and PostgreSQL's SQL-to-XML helpers are outside
/// this statement interface. The transaction's boundaries are its own: [`Self::commit`]
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
    /// A statement outside the extra-table SQL boundary, a bind that does
    /// not encode, or a database error.
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

/// A validated data statement with its binds and caching choice. Even an empty
/// bind set makes it run as a prepared statement.
struct Checked {
    sql: SqlStr,
    arguments: Option<PgArguments>,
    persistent: bool,
}

impl Checked {
    fn new(statement: impl sealed::Sealed) -> Result<Self, StorageError> {
        let (arguments, persistent, sql) = statement.into_parts();
        let arguments = arguments.map_err(|error| map_err(sqlx::Error::Encode(error)))?;
        let text = sql.as_str();
        refuse_other_than_data_statement(text)?;
        let normalized = text.to_ascii_lowercase();
        if sql_identifiers(&normalized).any(reserved_owner_statement_identifier)
            || normalized.contains("u&\"")
        {
            return Err(StorageError::Internal(
                "an owner transaction accepts extra-table SQL with literal identifiers".into(),
            ));
        }
        refuse_core_schema_sql(text)?;
        Ok(Self {
            sql,
            arguments: Some(arguments.unwrap_or_default()),
            persistent,
        })
    }
}

fn reserved_owner_statement_identifier(name: &str) -> bool {
    matches!(name, "set_config" | "pg_settings")
        || name.split_once("_to_xml").is_some_and(|(source, suffix)| {
            matches!(source, "query" | "table" | "cursor" | "schema" | "database")
                && matches!(suffix, "" | "_and_xmlschema" | "schema")
        })
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
#[path = "owner_transaction_tests.rs"]
mod tests;
