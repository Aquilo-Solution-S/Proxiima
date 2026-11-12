//! Replay immutable SQL under its historical catalog before the rename finalizer.
//!
//! Historical definitions live inside the final migration, alongside the old
//! applied SQL they make replayable. Runtime definitions use only current names.

use crate::{MissingMigrationPolicy, VectorStorage, error::internal};
use proxiima_core::StorageError;
use sqlx::migrate::{Migration, Migrator};
use sqlx::{PgConnection, Postgres, Transaction};

const FINALIZATION: &str = "-- rename-finalization-begin";

/// Historical migrations precede all catalog finalizers in a composed host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationPhase {
    Historical,
    Finalize,
    Current,
}

/// Each source keeps immutable SQL before its finalizer; later SQL uses current names.
#[must_use]
pub fn migration_phase(source: &Migrator, migration: &Migration) -> MigrationPhase {
    let sql = migration.sql.as_str();
    if sql.contains(FINALIZATION) {
        return MigrationPhase::Finalize;
    }
    match source
        .iter()
        .find(|candidate| candidate.sql.as_str().contains(FINALIZATION))
    {
        Some(finalizer) if migration.version < finalizer.version => MigrationPhase::Historical,
        Some(_) | None => MigrationPhase::Current,
    }
}

/// Keep each original migration and checksum, selecting only its execution phase.
#[must_use]
pub fn migrations_for_phase(source: &Migrator, phase: MigrationPhase) -> Migrator {
    let migrations = source
        .iter()
        .filter(|migration| migration_phase(source, migration) == phase)
        .map(|migration| match phase {
            MigrationPhase::Finalize => corrected_finalizer(migration),
            MigrationPhase::Historical | MigrationPhase::Current => migration.clone(),
        })
        .collect::<Vec<_>>();
    Migrator {
        migrations: std::borrow::Cow::Owned(migrations),
        // The caller validates the complete source/ledger before selecting a
        // phase; rows from the other phase remain in that same ledger.
        ignore_missing: true,
        locking: source.locking,
        no_tx: source.no_tx,
        table_name: source.table_name.clone(),
        create_schemas: source.create_schemas.clone(),
    }
}

/// A finalizer as it runs: its shipped version and checksum, so a ledger that
/// already records it is unchanged and a new one records the shipped row, with
/// SQL that keeps policy roles (`keep_policy_roles`). Released files are
/// immutable; their SQL passed every policy's roles through the word mapper.
fn corrected_finalizer(migration: &Migration) -> Migration {
    use sqlx::SqlSafeStr as _;

    let sql = keep_policy_roles(migration.sql.as_str(), "pg_temp.rename_spiice_word");
    Migration {
        // SQL-POLICY: fixed-fragment — the embedded finalizer with its policy capture narrowed.
        sql: sqlx::AssertSqlSafe(sql).into_sql_str(),
        ..migration.clone()
    }
}

fn finalizer() -> Migration {
    crate::core_migrator()
        .iter()
        .find(|migration| migration.sql.as_str().contains(FINALIZATION))
        .expect("embedded catalog finalizer")
        .clone()
}

fn historical_snapshot(kind: &str) -> String {
    let marker = format!("$rename_history_{kind}$");
    finalizer()
        .sql
        .as_str()
        .split(&marker)
        .nth(1)
        .expect("historical definition embedded by the catalog finalizer")
        .to_owned()
}

fn historical_definition(kind: &str) -> String {
    // Definitions are mutable replay inputs, unlike applied migration SQL.
    // Bind their privileged search owner to the superseding infrastructure
    // role without creating or accepting an alias for that role.
    let role = format!("{}search", historical_namespace_prefix());
    rebind_historical_role(&historical_snapshot(kind), &role)
}

fn rebind_historical_role(sql: &str, role: &str) -> String {
    sql.split_inclusive(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .map(|part| {
            let identifier = part.trim_end_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '_'
            });
            if identifier == role {
                format!("proxiima_search{}", &part[identifier.len()..])
            } else {
                part.to_owned()
            }
        })
        .collect()
}

fn historical_scope_key() -> String {
    historical_snapshot("functions")
        .split("current_setting('")
        .skip(1)
        .filter_map(|tail| tail.split_once('\'').map(|(key, _)| key))
        .find(|key| key.starts_with("app.") && key.ends_with("_scope"))
        .expect("historical definitions declare their platform scope")
        .to_owned()
}

fn catalog_finalization_sql() -> String {
    let sql = finalizer()
        .sql
        .as_str()
        .split_once(FINALIZATION)
        .expect("embedded catalog finalizer marker")
        .1
        .replace(
            "    IF to_regclass('proxiima_core.memory')",
            "    IF to_regclass('proxiima_core.owner_follows') IS NOT NULL THEN
        UPDATE proxiima_core.owner_follows SET table_name=pg_temp.rename_spiice_word(table_name)
        WHERE table_name<>pg_temp.rename_spiice_word(table_name);
    END IF;
    IF to_regclass('proxiima_core.memory')",
        );
    keep_policy_roles(&sql, "pg_temp.rename_spiice_word")
}

/// Role identities are operator-owned: a platform role named `proxima_platform`
/// is not a catalog word. Rewrite a policy's name, relation and expressions,
/// and keep the TO list the catalog reports. The shipped finalizers passed the
/// whole statement, roles included, through the word mapper. A finalizer
/// without that mapped policy capture is returned as it is.
fn keep_policy_roles(sql: &str, mapper: &str) -> String {
    let Some((head, policy)) = sql.split_once("SELECT 'policy',") else {
        return sql.to_owned();
    };
    let Some((policy, tail)) = policy.split_once("FROM pg_policy") else {
        return sql.to_owned();
    };
    if !policy.contains(&format!("{mapper}(format(")) {
        return sql.to_owned();
    }
    let policy = policy
        .replace(&format!("{mapper}(format("), "format(")
        .replace(
            "p.polname,n.nspname,c.relname,",
            &format!("{mapper}(p.polname),{mapper}(n.nspname),{mapper}(c.relname),"),
        )
        .replace(
            "pg_get_expr(p.polqual,p.polrelid)",
            &format!("{mapper}(pg_get_expr(p.polqual,p.polrelid))"),
        )
        .replace(
            "pg_get_expr(p.polwithcheck,p.polrelid)",
            &format!("{mapper}(pg_get_expr(p.polwithcheck,p.polrelid))"),
        )
        .replace(" END))", " END)");
    format!("{head}SELECT 'policy',{policy}FROM pg_policy{tail}")
}

/// The historical spelling of an embedded catalog identifier, for migration replay.
#[doc(hidden)]
#[must_use]
pub fn historical_catalog_name(value: &str) -> String {
    let sql = catalog_finalization_sql();
    let pattern = sql
        .split("regexp_instr(rest, '")
        .nth(1)
        .and_then(|tail| tail.split_once('\'').map(|(pattern, _)| pattern))
        .expect("embedded catalog word pattern");
    let extension = pattern
        .split('|')
        .next()
        .expect("extension word pattern")
        .replace("u?", "");
    value
        .replace(
            "proxiima",
            historical_namespace_prefix().trim_end_matches('_'),
        )
        .replace("spiice", &extension)
}

fn owner_follows_finalization_sql() -> String {
    let sql = catalog_finalization_sql();
    let (mapper, _) = sql
        .split_once("END $mapping$;")
        .expect("embedded catalog mapper");
    format!(
        "{mapper}END $mapping$;
DO $follows$ BEGIN
    IF to_regclass('proxiima_core.owner_follows') IS NOT NULL THEN
        UPDATE proxiima_core.owner_follows
           SET table_name=pg_temp.rename_spiice_word(table_name)
         WHERE table_name<>pg_temp.rename_spiice_word(table_name);
    END IF;
END $follows$;
DROP FUNCTION pg_temp.rename_spiice_word(text);"
    )
}

fn restore_historical_catalog_sql() -> String {
    // Invert the shipped catalog algorithm, not a second object-name inventory.
    let sql = catalog_finalization_sql();
    let pattern = sql
        .split("regexp_instr(rest, '")
        .nth(1)
        .and_then(|tail| tail.split_once('\'').map(|(pattern, _)| pattern))
        .expect("embedded catalog word pattern");
    let extension_pattern = pattern.split('|').next().expect("extension word pattern");
    let extension = extension_pattern.replace("u?", "");
    let prefix = historical_namespace_prefix();
    let product = prefix.trim_end_matches('_');
    // The forward policy capture already keeps its TO list (`keep_policy_roles`);
    // the word inversion below carries that shape over.
    sql.replace("proxiima", product).replace("spiice", &extension)
        .replace(pattern, "spiice|proxiima")
        .replace(&format!("^({extension_pattern}|{product})"), "^(spiice|proxiima)")
        .replace(&format!("'{}%'", &extension[..3]), "'spi%'")
        .replace("AND c.relname<>pg_temp.",
            "AND NOT (n.nspname = 'public' AND starts_with(c.relname, '_sqlx_migrations')) AND c.relname<>pg_temp.")
        .replace("n.nspname !~ '^(pg_|information_schema$)'",
            "n.nspname !~ '^(pg_|information_schema$)' AND n.nspowner = current_user::regrole")
        .replace("WHERE nspname !~ '^pg_'", "WHERE nspname !~ '^pg_' AND nspowner = current_user::regrole")
        .replace("WHERE d.objoid>=16384", "WHERE d.objoid>=16384 AND (
            identified.schema IN (SELECT nspname FROM pg_namespace WHERE nspowner = current_user::regrole)
            OR (d.classoid='pg_namespace'::regclass AND d.objoid IN
                (SELECT oid FROM pg_namespace WHERE nspowner = current_user::regrole)))")
}

pub(crate) fn historical_namespace_prefix() -> String {
    let setting = historical_scope_key();
    format!(
        "{}_",
        setting
            .strip_prefix("app.")
            .and_then(|key| key.strip_suffix("_scope"))
            .expect("historical platform scope namespace")
    )
}

async fn finalized(connection: &mut PgConnection) -> Result<bool, StorageError> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *connection)
            .await
            .map_err(internal)?;
    if !exists {
        return Ok(false);
    }
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM public._sqlx_migrations WHERE version = $1 AND success)",
    )
    .bind(finalizer().version)
    .fetch_one(&mut *connection)
    .await
    .map_err(internal)
}

async fn historical_definitions_present(
    connection: &mut PgConnection,
) -> Result<bool, StorageError> {
    sqlx::query_scalar("SELECT to_regprocedure($1) IS NOT NULL")
        .bind(format!(
            "{}core.assert_memory_declares_sidecar()",
            historical_namespace_prefix()
        ))
        .fetch_one(&mut *connection)
        .await
        .map_err(internal)
}

/// Historical RLS scope exists only while immutable migrations are replayed.
pub(crate) async fn configure_history_scope(
    connection: &mut PgConnection,
) -> Result<(), StorageError> {
    if !finalized(connection).await? {
        sqlx::query("SELECT set_config($1, 'platform', true)")
            .bind(historical_scope_key())
            .execute(&mut *connection)
            .await
            .map_err(internal)?;
    }
    Ok(())
}

/// Apply one core phase inside the caller's migration transaction.
///
/// Historical names are replay inputs: the caller finalizes them in this same
/// transaction before committing, including when adding a historical spiice
/// lane to an already-finalized core.
///
/// # Errors
/// Returns a storage error if schema or release validation, migration execution,
/// or definition installation fails.
pub async fn run_core_phase(
    transaction: &mut Transaction<'_, Postgres>,
    policy: MissingMigrationPolicy,
    vectors: VectorStorage,
    phase: MigrationPhase,
) -> Result<(), StorageError> {
    let connection = transaction.as_mut();
    let state = crate::CoreSchemaState::read(connection, policy).await?;
    if phase == MigrationPhase::Historical && state.recorded.contains(&finalizer().version) {
        sqlx::query("SELECT set_config($1, 'platform', true)")
            .bind(historical_scope_key())
            .execute(&mut *connection)
            .await
            .map_err(internal)?;
        execute_replay_sql(connection, restore_historical_catalog_sql()).await?;
        // The released definitions return the four-rung enum, a row type the
        // current function cannot be replaced by in place.
        execute_replay_sql(
            connection,
            format!(
                "DROP FUNCTION IF EXISTS {}core.read_subject_authority(uuid)",
                historical_namespace_prefix()
            ),
        )
        .await?;
        install_historical_definitions(connection, vectors).await?;
    }
    if phase == MigrationPhase::Historical {
        crate::definitions::validate_historical_release(connection, policy).await?;
    }
    if phase == MigrationPhase::Finalize && !state.recorded.contains(&finalizer().version) {
        hand_over_historical_search_role(connection).await?;
    }
    migrations_for_phase(&crate::core_migrator_for(vectors), phase)
        .run_direct(None, &mut *connection, false)
        .await
        .map_err(internal)?;
    match phase {
        MigrationPhase::Historical => {
            // Operator staging replays original definitions before renaming
            // infrastructure roles. Do not reinstall those historical grants
            // after that external phase; validate their recorded release above.
            if historical_definitions_present(connection).await? {
                return Ok(());
            }
            install_historical_definitions(connection, vectors).await
        }
        MigrationPhase::Finalize => {
            if historical_definitions_present(connection).await? {
                execute_replay_sql(connection, catalog_finalization_sql()).await?;
                crate::definitions::invalidate_replayed_definitions(connection, policy).await?;
            }
            execute_replay_sql(connection, owner_follows_finalization_sql()).await?;
            Ok(())
        }
        MigrationPhase::Current => crate::definitions::apply(connection, policy, vectors).await,
    }
}

async fn install_historical_definitions(
    connection: &mut PgConnection,
    vectors: VectorStorage,
) -> Result<(), StorageError> {
    sqlx::query("SET LOCAL check_function_bodies = off")
        .execute(&mut *connection)
        .await
        .map_err(internal)?;
    let kinds: &[&str] = match vectors {
        VectorStorage::Local => &["functions", "triggers", "policies", "vector"],
        VectorStorage::HostIndex => &["functions", "triggers", "policies"],
    };
    for kind in kinds {
        execute_replay_sql(connection, historical_definition(kind)).await?;
    }
    Ok(())
}

/// Move a store built by an earlier release off its historical search role.
///
/// That release created the search and vector candidate functions as the
/// historical search role, which the finalizer would recreate under that
/// role, out of reach of `proxiima_search`. The platform role already holds
/// `SET` on it, so this drops those functions as their owner and resets the
/// role's default privileges; the definitions pass recreates them as
/// `proxiima_search`. It also moves the role's schema access to
/// `proxiima_search`. Anything else the role owns needs a superuser, so it
/// refuses with the statement to run.
async fn hand_over_historical_search_role(
    connection: &mut PgConnection,
) -> Result<(), StorageError> {
    let historical = format!("{}search", historical_namespace_prefix());
    let core = format!("{}core", historical_namespace_prefix());
    // SQL-POLICY: fixed-fragment — role and schema names derive only from embedded historical SQL.
    let sql = format!(
        r"DO $handover$
DECLARE
    historical oid := to_regrole('{historical}');
    original name := current_user;
    object record;
BEGIN
    IF historical IS NULL OR NOT EXISTS (
        SELECT 1 FROM pg_shdepend s JOIN pg_database d ON d.oid = s.dbid
         WHERE d.datname = current_database() AND s.refobjid = historical
    ) THEN
        RETURN;
    END IF;
    IF NOT pg_has_role(current_user, historical, 'SET') OR EXISTS (
        SELECT 1 FROM pg_shdepend s JOIN pg_database d ON d.oid = s.dbid
         WHERE d.datname = current_database() AND s.refobjid = historical AND s.deptype = 'o'
           AND s.classid <> 'pg_default_acl'::regclass
           AND NOT (s.classid = 'pg_proc'::regclass AND EXISTS (
               SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                WHERE p.oid = s.objid AND n.nspowner = current_user::regrole))
    ) THEN
        RAISE EXCEPTION 'role % owns objects in this database that migration 61 cannot hand over', '{historical}'
            USING ERRCODE = '42501',
                  HINT = 'as a superuser in this database, run: REASSIGN OWNED BY {historical} TO proxiima_search; then migrate again';
    END IF;
    EXECUTE format('SET LOCAL ROLE %I', '{historical}');
    FOR object IN SELECT p.oid::regprocedure AS routine
                    FROM pg_proc p WHERE p.proowner = historical ORDER BY p.oid LOOP
        EXECUTE format('DROP ROUTINE %s', object.routine);
    END LOOP;
    FOR object IN SELECT d.defaclnamespace, a.grantee,
                         CASE d.defaclobjtype WHEN 'r' THEN 'TABLES' WHEN 'S' THEN 'SEQUENCES'
                              WHEN 'f' THEN 'ROUTINES' WHEN 'T' THEN 'TYPES' WHEN 'n' THEN 'SCHEMAS' END AS kind
                    FROM pg_default_acl d, aclexplode(d.defaclacl) a
                   WHERE d.defaclrole = historical AND a.grantee NOT IN (0, historical) LOOP
        EXECUTE format('ALTER DEFAULT PRIVILEGES FOR ROLE %I %s REVOKE ALL ON %s FROM %I',
            '{historical}',
            CASE WHEN object.defaclnamespace = 0 THEN ''
                 ELSE format('IN SCHEMA %I', object.defaclnamespace::regnamespace) END,
            object.kind, pg_get_userbyid(object.grantee));
    END LOOP;
    EXECUTE format('SET LOCAL ROLE %I', original);
    FOR object IN SELECT nspname FROM pg_namespace WHERE nspowner = current_user::regrole LOOP
        EXECUTE format('REVOKE ALL ON ALL TABLES IN SCHEMA %I FROM %I', object.nspname, '{historical}');
        EXECUTE format('REVOKE ALL ON SCHEMA %I FROM %I', object.nspname, '{historical}');
    END LOOP;
    IF to_regnamespace('{core}') IS NOT NULL THEN
        GRANT USAGE ON SCHEMA {core} TO proxiima_search;
    END IF;
END
$handover$;"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(connection)
        .await
        .map_err(internal)?;
    Ok(())
}

async fn execute_replay_sql(
    connection: &mut PgConnection,
    sql: String,
) -> Result<(), StorageError> {
    // SQL-POLICY: fixed-fragment — only immutable embedded catalog SQL and its inverse.
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(connection)
        .await
        .map_err(internal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn historical_role_rebinding_keeps_policy_identifiers() {
        let historical = format!("{}search", super::historical_namespace_prefix());
        let sql = format!(
            "TO {historical}; FROM {historical}; SET LOCAL ROLE {historical}; \
            DROP POLICY {historical}_candidates; to_regrole('{historical}')"
        );
        let expected = format!(
            "TO proxiima_search; FROM proxiima_search; SET LOCAL ROLE proxiima_search; \
            DROP POLICY {historical}_candidates; to_regrole('proxiima_search')"
        );
        assert_eq!(super::rebind_historical_role(&sql, &historical), expected);
    }
}
