//! The opt-in grant phase for [`crate::StorageProxiima::migrate`].
//!
//! The advisory lock serializes default-ACL updates from concurrent migrations.

use std::str::FromStr;

use proxiima_core::{PgIdent, SqlText};
use sqlx::PgPool;
use sqlx::postgres::PgConnectOptions;

use crate::ProxiimaError;
use crate::runtime_config::cause_with;
use proxiima_spiice::NamedMigrator;

const RUNTIME_GRANTS_LOCK_KEY: i64 =
    proxiima_core::ledger!(Proxiima.DatabaseAuthority.runtimeGrantLockKey);

/// The schema whose views are read-only for the runtime role.
const CORE_SCHEMA: &str = "proxiima_core";

/// A validated runtime-grant plan: whom to grant, over which candidate
/// schemas and ledgers. Candidates that do not exist are skipped at apply.
#[derive(Debug)]
pub(crate) struct RuntimeGrants {
    runtime_role: String,
    schemas: Vec<String>,
    ledgers: Vec<String>,
}

impl RuntimeGrants {
    /// Validate the role split. Issues no SQL.
    ///
    /// `ledgers` are qualified ledger names ([`ledger_names`]).
    ///
    /// # Errors
    ///
    /// [`ProxiimaError::Config`] when either URL does not parse or both URLs
    /// name the same user.
    pub(crate) fn plan(
        database_url: &str,
        platform_database_url: &str,
        schemas: Vec<String>,
        ledgers: Vec<String>,
    ) -> Result<Self, ProxiimaError> {
        let runtime_role = url_user(database_url, "DATABASE_URL")?;
        let platform_role = url_user(platform_database_url, "PROXIIMA_PLATFORM_DATABASE_URL")?;
        if runtime_role == platform_role {
            return Err(ProxiimaError::Config(format!(
                "runtime grants require split roles: DATABASE_URL and \
                 PROXIIMA_PLATFORM_DATABASE_URL both connect as {runtime_role:?}"
            )));
        }
        Ok(Self {
            runtime_role,
            schemas,
            ledgers,
        })
    }

    /// Issue the grants on `platform` in one transaction.
    ///
    /// # Errors
    ///
    /// [`ProxiimaError::Config`] when the runtime role does not exist;
    /// [`ProxiimaError::Storage`] naming the statement class that failed. The
    /// transaction rolls back: nothing is granted.
    pub(crate) async fn apply(&self, platform: &PgPool) -> Result<(), ProxiimaError> {
        // `Proxiima.DatabaseAuthority.grantPhaseOrder`
        let mut tx = platform
            .begin()
            .await
            .map_err(|error| failed("BEGIN", "platform pool", error))?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(RUNTIME_GRANTS_LOCK_KEY)
            .execute(&mut *tx)
            .await
            .map_err(|error| failed("advisory lock", "runtime grants", error))?;
        let runtime_exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
                .bind(&self.runtime_role)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| failed("role lookup", &self.runtime_role, error))?;
        if !runtime_exists {
            return Err(ProxiimaError::Config(format!(
                "runtime grants: DATABASE_URL role {:?} does not exist",
                self.runtime_role
            )));
        }
        let schemas: Vec<String> = sqlx::query_scalar(
            "SELECT nspname::text FROM pg_namespace WHERE nspname = ANY($1::text[]) ORDER BY 1",
        )
        .bind(&self.schemas)
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| failed("schema lookup", "composed schemas", error))?;
        // `to_regclass` resolves each compiled-in ledger name the way SQLx's
        // pinned-`public` migrator does and yields NULL for a ledger not yet
        // created; the join drops those. Schema `USAGE` is probed so an
        // already-usable `public` (PUBLIC holds it by default) is not
        // re-granted by a non-owner, which Postgres answers with a WARNING.
        let ledgers: Vec<Ledger> = sqlx::query_as::<_, (String, String, bool)>(
            "SELECT DISTINCT n.nspname::text, c.relname::text,
                    has_schema_privilege($2, n.oid, 'USAGE')
               FROM unnest($1::text[]) AS ledger(name)
               JOIN pg_class AS c ON c.oid = to_regclass(ledger.name)
               JOIN pg_namespace AS n ON n.oid = c.relnamespace
              WHERE c.relkind IN ('r', 'p')
              ORDER BY 1, 2",
        )
        .bind(&self.ledgers)
        .bind(&self.runtime_role)
        .fetch_all(&mut *tx)
        .await
        .map_err(|error| failed("ledger lookup", "migration ledgers", error))?
        .into_iter()
        .map(|(schema, table, runtime_schema_usage)| Ledger {
            schema,
            table,
            runtime_schema_usage,
        })
        .collect();
        for statement in grant_statements(&self.runtime_role, &schemas, &ledgers)? {
            sqlx::raw_sql(statement.sql)
                .execute(&mut *tx)
                .await
                .map_err(|error| failed(statement.class, &statement.object, error))?;
        }
        self.grant_read_function_execution(&mut tx, &schemas)
            .await?;
        tx.commit()
            .await
            .map_err(|error| failed("COMMIT", "runtime grants", error))?;
        tracing::info!(
            runtime_role = %self.runtime_role,
            schemas = ?schemas,
            ledgers = ledgers.len(),
            "runtime grants applied"
        );
        Ok(())
    }

    async fn grant_read_function_execution(
        &self,
        connection: &mut sqlx::PgConnection,
        schemas: &[String],
    ) -> Result<(), ProxiimaError> {
        let fail = |error| {
            failed(
                "GRANT EXECUTE ON FUNCTION",
                "isolated read functions",
                error,
            )
        };
        let schemas_json = serde_json::to_string(schemas).map_err(|error| {
            ProxiimaError::Config(format!("runtime grants: schema list: {error}"))
        })?;
        sqlx::query(
            "SELECT set_config('proxiima.grant_schemas', $1, true),
                    set_config('proxiima.grant_role', $2, true)",
        )
        .bind(schemas_json)
        .bind(&self.runtime_role)
        .execute(&mut *connection)
        .await
        .map_err(fail)?;
        // `Proxiima.DatabaseAuthority.scopedReaderGrantOrder`
        sqlx::raw_sql(
            "DO $grant$
             DECLARE
               original_role name := current_user;
               function_row record;
             BEGIN
               FOR function_row IN
                 SELECT owner_role.rolname AS owner_name, n.nspname AS schema_name,
                        p.proname AS function_name,
                        pg_get_function_identity_arguments(p.oid) AS args
                   FROM pg_proc p
                   JOIN pg_namespace n ON n.oid = p.pronamespace
                   JOIN pg_roles owner_role ON owner_role.oid = p.proowner
                  WHERE n.nspname IN (SELECT jsonb_array_elements_text(
                                        current_setting('proxiima.grant_schemas')::jsonb))
                    AND owner_role.rolname IN ('proxiima_search','proxiima_goal_reader')
                    AND p.prosecdef AND p.prokind = 'f'
                  ORDER BY n.nspname, p.proname, p.oid
               LOOP
                 EXECUTE format('SET LOCAL ROLE %I', function_row.owner_name);
                 EXECUTE format('GRANT EXECUTE ON FUNCTION %I.%I(%s) TO %I',
                                function_row.schema_name, function_row.function_name,
                                function_row.args, current_setting('proxiima.grant_role'));
                 EXECUTE format('SET LOCAL ROLE %I', original_role);
               END LOOP;
             END $grant$",
        )
        .execute(&mut *connection)
        .await
        .map_err(fail)?;
        Ok(())
    }
}

/// Qualified names of every ledger this binary migrates into: core's, then
/// each composed migrator's `table_name`. An unqualified name lives in
/// `public`, where the migration facade pins `search_path`.
pub(crate) fn ledger_names(migrators: &[NamedMigrator]) -> Vec<String> {
    let core = proxiima_storage_pg::core_migrator();
    let mut ledgers: Vec<String> = std::iter::once(core.table_name.as_ref())
        .chain(
            migrators
                .iter()
                .map(|named| named.migrator().table_name.as_ref()),
        )
        .map(|table| {
            if table.contains('.') {
                table.to_owned()
            } else {
                format!("public.{table}")
            }
        })
        .collect();
    ledgers.sort();
    ledgers.dedup();
    ledgers
}

fn url_user(url: &str, variable: &str) -> Result<String, ProxiimaError> {
    // The parse error is not echoed: it could carry URL fragments.
    PgConnectOptions::from_str(url)
        .map(|options| options.get_username().to_owned())
        .map_err(|_| ProxiimaError::Config(format!("{variable} is not a valid Postgres URL")))
}

fn failed(class: &str, object: &str, error: sqlx::Error) -> ProxiimaError {
    ProxiimaError::Storage(cause_with(
        format!("runtime grants: {class} on {object} failed: {error}"),
        error,
    ))
}

/// One existing migration ledger, resolved from the catalog.
#[derive(Debug)]
struct Ledger {
    schema: String,
    table: String,
    /// Runtime can already use `schema` (before this step's grants).
    runtime_schema_usage: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct GrantStatement {
    /// Statement class named in a failure.
    class: &'static str,
    /// The schema or table it targets, for the failure message.
    object: String,
    sql: SqlText,
}

impl GrantStatement {
    /// A statement from a program literal; `names` fill its placeholders in
    /// one pass.
    fn new(
        class: &'static str,
        object: &str,
        template: &'static str,
        names: &[(&'static str, &PgIdent)],
    ) -> Self {
        let parts: Vec<(&'static str, SqlText)> = names
            .iter()
            .map(|&(placeholder, name)| (placeholder, SqlText::from(name)))
            .collect();
        let parts: Vec<(&'static str, &SqlText)> = parts
            .iter()
            .map(|(placeholder, text)| (*placeholder, text))
            .collect();
        Self {
            class,
            object: object.to_owned(),
            sql: SqlText::fill(template, &parts),
        }
    }
}

fn quoted(name: &str) -> Result<PgIdent, ProxiimaError> {
    PgIdent::quoted(name).map_err(|error| ProxiimaError::Config(format!("runtime grants: {error}")))
}

/// Apply read-only restrictions after blanket grants so a ledger in a composed schema loses the new DML grants.
fn grant_statements(
    runtime_role: &str,
    schemas: &[String],
    ledgers: &[Ledger],
) -> Result<Vec<GrantStatement>, ProxiimaError> {
    let runtime = quoted(runtime_role)?;
    let mut statements = Vec::new();
    // Grants stay scoped to each composed schema and confer no ownership or role membership.
    for name in schemas {
        let schema = quoted(name)?;
        let schema_statement = |class, template| {
            GrantStatement::new(
                class,
                name,
                template,
                &[("{schema}", &schema), ("{runtime}", &runtime)],
            )
        };
        statements.extend([
            schema_statement(
                "GRANT USAGE ON SCHEMA",
                "GRANT USAGE ON SCHEMA {schema} TO {runtime}",
            ),
            schema_statement(
                "REVOKE CREATE ON SCHEMA FROM PUBLIC",
                "REVOKE CREATE ON SCHEMA {schema} FROM PUBLIC",
            ),
            schema_statement(
                "REVOKE CREATE ON SCHEMA FROM runtime",
                "REVOKE CREATE ON SCHEMA {schema} FROM {runtime}",
            ),
            schema_statement(
                "GRANT DML ON ALL TABLES",
                "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {schema} \
                 TO {runtime}",
            ),
        ]);
        if name == CORE_SCHEMA {
            // The blanket table grant also covers views.
            statements.push(GrantStatement::new(
                "RESTRICT core view grants",
                name,
                "SELECT proxiima_core.restrict_view_grants()",
                &[],
            ));
        }
        statements.extend([
            schema_statement(
                "GRANT ON ALL SEQUENCES",
                "GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA {schema} TO {runtime}",
            ),
            schema_statement(
                "ALTER DEFAULT PRIVILEGES ON TABLES",
                "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
                 GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {runtime}",
            ),
            schema_statement(
                "ALTER DEFAULT PRIVILEGES ON SEQUENCES",
                "ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} \
                 GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO {runtime}",
            ),
        ]);
    }
    // Runtime can read migration ledgers, with schema usage added only where needed.
    let mut ledger_schemas: Vec<&String> = ledgers
        .iter()
        .filter(|ledger| !ledger.runtime_schema_usage && !schemas.contains(&ledger.schema))
        .map(|ledger| &ledger.schema)
        .collect();
    ledger_schemas.sort();
    ledger_schemas.dedup();
    for name in ledger_schemas {
        statements.push(GrantStatement::new(
            "GRANT USAGE ON ledger SCHEMA",
            name,
            "GRANT USAGE ON SCHEMA {schema} TO {runtime}",
            &[("{schema}", &quoted(name)?), ("{runtime}", &runtime)],
        ));
    }
    for Ledger { schema, table, .. } in ledgers {
        let object = format!("{schema}.{table}");
        let ledger_statement = |class, template| {
            Ok::<_, ProxiimaError>(GrantStatement::new(
                class,
                &object,
                template,
                &[
                    ("{ledger_schema}", &quoted(schema)?),
                    ("{ledger_table}", &quoted(table)?),
                    ("{runtime}", &runtime),
                ],
            ))
        };
        statements.extend([
            ledger_statement(
                "REVOKE writes ON ledger",
                "REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON {ledger_schema}.{ledger_table} \
                 FROM {runtime}",
            )?,
            ledger_statement(
                "GRANT SELECT ON ledger",
                "GRANT SELECT ON {ledger_schema}.{ledger_table} TO {runtime}",
            )?,
        ]);
    }
    Ok(statements)
}

#[cfg(test)]
mod tests {
    use super::{Ledger, ProxiimaError, RuntimeGrants, grant_statements, ledger_names};

    fn owned(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn ledger(schema: &str, table: &str) -> Ledger {
        Ledger {
            schema: schema.to_owned(),
            table: table.to_owned(),
            runtime_schema_usage: false,
        }
    }

    fn sql(statements: &[super::GrantStatement]) -> Vec<&str> {
        statements.iter().map(|s| s.sql.as_str()).collect()
    }

    #[test]
    fn identifiers_are_quoted_and_embedded_quotes_doubled() {
        let statements =
            grant_statements("rt\"x", &owned(&["proxiima_core"]), &[]).expect("statements");
        assert!(
            sql(&statements).contains(&"GRANT USAGE ON SCHEMA \"proxiima_core\" TO \"rt\"\"x\"")
        );
        assert!(sql(&statements).iter().all(|s| !s.contains("TO rt\"x")));
    }

    #[test]
    fn composed_schemas_get_dml_and_scoped_defaults_and_never_escalation() {
        let schemas = owned(&["proxiima_core", "proxiima_fixture"]);
        let statements = grant_statements(
            "runtime",
            &schemas,
            &[
                ledger("public", "_sqlx_migrations"),
                ledger("public", "_sqlx_migrations_proxiima_fixture"),
            ],
        )
        .expect("statements");
        let sql = sql(&statements);
        for schema in &schemas {
            for expected in [
                format!("GRANT USAGE ON SCHEMA \"{schema}\" TO \"runtime\""),
                format!("REVOKE CREATE ON SCHEMA \"{schema}\" FROM PUBLIC"),
                format!("REVOKE CREATE ON SCHEMA \"{schema}\" FROM \"runtime\""),
                format!(
                    "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA \"{schema}\" TO \"runtime\""
                ),
                format!(
                    "GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA \"{schema}\" TO \"runtime\""
                ),
                format!(
                    "ALTER DEFAULT PRIVILEGES IN SCHEMA \"{schema}\" GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO \"runtime\""
                ),
                format!(
                    "ALTER DEFAULT PRIVILEGES IN SCHEMA \"{schema}\" GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO \"runtime\""
                ),
            ] {
                assert!(sql.contains(&expected.as_str()), "missing {expected}");
            }
        }
        for statement in &sql {
            if statement.starts_with("REVOKE") {
                continue;
            }
            for forbidden in [
                "TRUNCATE",
                "REFERENCES",
                "TRIGGER",
                "CREATE",
                "OWNER",
                "ALL PRIVILEGES",
            ] {
                assert!(!statement.contains(forbidden), "{statement}");
            }
            // Defaults are schema-scoped, never global.
            if statement.starts_with("ALTER DEFAULT PRIVILEGES") {
                assert!(statement.contains(" IN SCHEMA "), "{statement}");
            }
        }
        // The ledger schema gets USAGE only: no DML on `public`'s host tables.
        assert!(sql.contains(&"GRANT USAGE ON SCHEMA \"public\" TO \"runtime\""));
        assert!(
            sql.iter()
                .all(|s| !s.contains("ALL TABLES IN SCHEMA \"public\""))
        );
        assert!(
            sql.iter()
                .all(|s| !s.contains("IN SCHEMA \"public\" GRANT"))
        );
        assert!(sql.contains(
            &"GRANT SELECT ON \"public\".\"_sqlx_migrations_proxiima_fixture\" TO \"runtime\""
        ));
    }

    #[test]
    fn core_views_are_restricted_right_after_the_core_blanket_grant_and_only_there() {
        let statements = grant_statements("runtime", &owned(&["proxiima_core", "proxiima_x"]), &[])
            .expect("statements");
        let issued = sql(&statements);
        let restrict = "SELECT proxiima_core.restrict_view_grants()";
        assert_eq!(issued.iter().filter(|s| **s == restrict).count(), 1);
        let blanket = issued
            .iter()
            .position(|s| s.contains("ALL TABLES IN SCHEMA \"proxiima_core\""))
            .expect("core blanket grant");
        assert_eq!(issued[blanket + 1], restrict);
        let without_core =
            grant_statements("runtime", &owned(&["proxiima_x"]), &[]).expect("statements");
        assert!(
            sql(&without_core)
                .iter()
                .all(|s| !s.contains("restrict_view_grants"))
        );
    }

    #[test]
    fn usable_ledger_schema_is_not_regranted() {
        let usable = Ledger {
            runtime_schema_usage: true,
            ..ledger("public", "_sqlx_migrations")
        };
        let statements = grant_statements("runtime", &[], &[usable]).expect("statements");
        assert_eq!(
            sql(&statements),
            [
                "REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON \"public\".\"_sqlx_migrations\" FROM \"runtime\"",
                "GRANT SELECT ON \"public\".\"_sqlx_migrations\" TO \"runtime\"",
            ]
        );
    }

    #[test]
    fn ledger_inside_a_composed_schema_is_narrowed_after_its_schema_grant() {
        let statements = grant_statements(
            "runtime",
            &owned(&["proxiima_x"]),
            &[ledger("proxiima_x", "_ledger")],
        )
        .expect("statements");
        let sql = sql(&statements);
        let schema_dml = sql
            .iter()
            .position(|s| s.contains("ALL TABLES IN SCHEMA \"proxiima_x\""))
            .expect("schema DML");
        let narrowed = sql
            .iter()
            .position(|s| {
                *s == "REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON \"proxiima_x\".\"_ledger\" FROM \"runtime\""
            })
            .expect("ledger narrowed");
        assert!(narrowed > schema_dml);
        // A composed ledger schema is not granted USAGE twice.
        assert_eq!(
            sql.iter()
                .filter(|s| s.starts_with("GRANT USAGE ON SCHEMA \"proxiima_x\""))
                .count(),
            1
        );
    }

    #[test]
    fn ledger_names_are_qualified_and_deduplicated() {
        let named = proxiima_spiice::NamedMigrator::new(
            "fixture-spiice",
            fixture_migrator("public._sqlx_migrations_proxiima_fixture"),
        );
        let unqualified = proxiima_spiice::NamedMigrator::new(
            "inline-spiice",
            fixture_migrator("_sqlx_migrations"),
        );
        assert_eq!(
            ledger_names(&[named, unqualified]),
            owned(&[
                "public._sqlx_migrations",
                "public._sqlx_migrations_proxiima_fixture"
            ])
        );
    }

    fn fixture_migrator(table: &'static str) -> sqlx::migrate::Migrator {
        let mut migrator = sqlx::migrate::Migrator::DEFAULT;
        migrator.dangerous_set_table_name(table);
        migrator
    }

    #[test]
    fn plan_refuses_without_split_roles() {
        let same = RuntimeGrants::plan(
            "postgres://same:a@localhost/db",
            "postgres://same:b@localhost/db",
            Vec::new(),
            Vec::new(),
        )
        .expect_err("same user");
        assert!(
            matches!(same, ProxiimaError::Config(ref m) if m.contains("split roles")),
            "{same}"
        );
        let split = RuntimeGrants::plan(
            "postgres://rt:a@localhost/db",
            "postgres://platform:b@localhost/db",
            Vec::new(),
            Vec::new(),
        )
        .expect("split roles plan");
        assert_eq!(split.runtime_role, "rt");
    }
}
