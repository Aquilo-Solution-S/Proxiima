use sqlx::{PgConnection, Postgres, Transaction};

use super::PgSidecarRegistryFrozen;
use crate::error::map_err;
use proxiima_core::StorageError;

impl PgSidecarRegistryFrozen {
    /// Synchronize sidecars whose owner follows memory in the caller's migration transaction.
    ///
    /// The database helper owns marker replacement, owner RLS and owner indexes;
    /// it touches only the schemas derived from this frozen composition.
    ///
    /// # Errors
    ///
    /// Refuses a transaction outside platform scope and propagates database errors.
    pub async fn sync_owner_follows(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<(), StorageError> {
        sqlx::query("SELECT proxiima_core.sync_owner_follows($1, $2)")
            .bind(self.spiice_schema_names())
            .bind(self.owner_follows_memory_sidecar_tables())
            .execute(&mut **tx)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    /// Reinstall the generated owner policies of every recorded table in the
    /// caller's migration transaction, as the last step of a migration run.
    ///
    /// # Errors
    ///
    /// Refuses a transaction outside platform scope and propagates database
    /// errors, including a recorded table whose shape no longer fits its class.
    pub async fn reinstall_owner_rls(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<(), StorageError> {
        sqlx::query("SELECT proxiima_core.reinstall_owner_rls()")
            .execute(&mut **tx)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    /// Check Follows markers, owner indexes and the owner-policy record without
    /// changing the database.
    ///
    /// The connection belongs to the caller's platform-scoped preflight. Markers
    /// in other compositions' schemas are outside this check. Policy structure
    /// remains the responsibility of [`crate::assert_runtime_rls`].
    ///
    /// # Errors
    ///
    /// Returns `Internal` for a registered sidecar the database does not
    /// have, `MigrationRequired` for a marker mismatch, an existing sidecar
    /// without a valid btree index leading with `owner_id`, or tables of the
    /// composed schemas that carry owner policies but have neither an
    /// `owner_rls_table` record nor a Follows marker, or whose owner policies
    /// differ from the text `owner_rls_apply_policies` recorded, and propagates
    /// database errors.
    pub async fn ensure_owner_rls(&self, conn: &mut PgConnection) -> Result<(), StorageError> {
        let expected = self.owner_follows_memory_sidecar_tables();
        // A registered table the database does not have is not generated
        // state: synchronizing skips it, and no migration run re-creates it.
        let absent: Vec<String> = sqlx::query_scalar(
            "SELECT expected.table_name FROM unnest($1::text[]) AS expected(table_name)
              WHERE NOT EXISTS (
                  SELECT 1 FROM pg_catalog.pg_class AS c
                  JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
                  WHERE n.nspname = split_part(expected.table_name, '.', 1)
                    AND c.relname = split_part(expected.table_name, '.', 2)
                    AND c.relkind IN ('r', 'p')
              ) ORDER BY expected.table_name",
        )
        .bind(&expected)
        .fetch_all(&mut *conn)
        .await
        .map_err(map_err)?;
        if !absent.is_empty() {
            return Err(StorageError::Internal(format!(
                "registered owner_follows sidecars do not exist: {absent:?}"
            )));
        }
        let mut actual: Vec<String> = sqlx::query_scalar(
            "SELECT table_name FROM proxiima_core.owner_follows
              WHERE split_part(table_name, '.', 1) = ANY($1)",
        )
        .bind(self.spiice_schema_names())
        .fetch_all(&mut *conn)
        .await
        .map_err(map_err)?;
        actual.sort_unstable();
        if actual != expected {
            return Err(StorageError::MigrationRequired {
                details: format!(
                    "owner_follows markers differ from the frozen sidecars: expected {expected:?}, \
                 found {actual:?}"
                ),
            });
        }
        let unindexed: Vec<String> = sqlx::query_scalar(
            "SELECT expected.table_name FROM unnest($1::text[]) AS expected(table_name)
              WHERE NOT EXISTS (
                  SELECT 1 FROM pg_catalog.pg_class AS c
                  JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
                  JOIN pg_catalog.pg_attribute AS a ON a.attrelid = c.oid
                  JOIN pg_catalog.pg_index AS i ON i.indrelid = c.oid
                  JOIN pg_catalog.pg_class AS ic ON ic.oid = i.indexrelid
                  JOIN pg_catalog.pg_am AS am ON am.oid = ic.relam
                  WHERE n.nspname = split_part(expected.table_name, '.', 1)
                    AND c.relname = split_part(expected.table_name, '.', 2)
                    AND c.relkind IN ('r', 'p')
                    AND a.attname = 'owner_id' AND NOT a.attisdropped AND a.attnum > 0
                    AND i.indisvalid AND am.amname = 'btree' AND i.indkey[0] = a.attnum
              ) ORDER BY expected.table_name",
        )
        .bind(&expected)
        .fetch_all(&mut *conn)
        .await
        .map_err(map_err)?;
        if !unindexed.is_empty() {
            return Err(StorageError::MigrationRequired {
                details: format!(
                    "owner_follows sidecars lack a valid btree index leading with owner_id: \
                 {unindexed:?}"
                ),
            });
        }
        self.ensure_owner_policies_installed(conn).await
    }

    /// Refuse tables of the composed schemas whose owner policies no installer
    /// recorded, then those whose policies are not the text core created.
    async fn ensure_owner_policies_installed(
        &self,
        conn: &mut PgConnection,
    ) -> Result<(), StorageError> {
        let unrecorded: Vec<String> = sqlx::query_scalar(
            "SELECT format('%I.%I', n.nspname, c.relname)
               FROM pg_catalog.pg_class AS c
               JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
              WHERE n.nspname = ANY($1) AND n.nspname <> 'proxiima_core'
                AND c.relkind IN ('r', 'p')
                AND EXISTS (SELECT 1 FROM pg_catalog.pg_policy AS p
                             WHERE p.polrelid = c.oid
                               AND p.polname IN ('proxiima_owner_read', 'proxiima_owner_write'))
                AND NOT EXISTS (SELECT 1 FROM proxiima_core.owner_rls_table AS r
                                 WHERE r.schema_name = n.nspname AND r.table_name = c.relname)
                AND NOT EXISTS (SELECT 1 FROM proxiima_core.owner_follows AS f
                                 WHERE f.table_name = n.nspname || '.' || c.relname)
              ORDER BY 1",
        )
        .bind(self.spiice_schema_names())
        .fetch_all(&mut *conn)
        .await
        .map_err(map_err)?;
        if !unrecorded.is_empty() {
            return Err(StorageError::MigrationRequired {
                details: format!(
                    "tables carry owner policies no installer recorded: {unrecorded:?}; call \
                     proxiima_core.install_owner_rls_table (or install_owner_rls for the \
                     schema) in a new migration"
                ),
            });
        }
        let altered: Vec<String> = sqlx::query_scalar(
            "SELECT format('%I.%I', n.nspname, c.relname)
               FROM pg_catalog.pg_class AS c
               JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
              WHERE n.nspname = ANY($1) AND n.nspname <> 'proxiima_core'
                AND c.relkind IN ('r', 'p')
                AND EXISTS (SELECT 1 FROM pg_catalog.pg_policy AS p
                             WHERE p.polrelid = c.oid
                               AND p.polname IN ('proxiima_owner_read', 'proxiima_owner_write'))
                AND proxiima_core.owner_rls_policy_text(c.oid) IS DISTINCT FROM
                    (SELECT i.policies FROM proxiima_core.owner_rls_installed AS i
                      WHERE i.schema_name = n.nspname AND i.table_name = c.relname)
              ORDER BY 1",
        )
        .bind(self.spiice_schema_names())
        .fetch_all(&mut *conn)
        .await
        .map_err(map_err)?;
        if !altered.is_empty() {
            return Err(StorageError::MigrationRequired {
                details: format!(
                    "owner policies of {altered:?} are not the ones core installed; \
                     run migrations"
                ),
            });
        }
        Ok(())
    }
}
