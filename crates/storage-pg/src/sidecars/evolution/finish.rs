use proxiima_core::{PgIdent, SqlText, StorageError};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::refused;
use crate::error::map_err;
use crate::sidecars::PgSidecarRegistryFrozen;

/// Finish only after all participating spiices have produced their target
/// records. An incoming pin owned by an omitted spiice is a refusal, never an
/// implicit rewrite of that spiice's payload.
pub(super) async fn finish(
    registry: &PgSidecarRegistryFrozen,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(), StorageError> {
    let memory_ids:Vec<Uuid>=sqlx::query_scalar("SELECT DISTINCT m.t FROM proxiima_core.memory_including_forgotten m
        JOIN proxiima_core.schema_evolution_authority a ON a.source_id=m.t AND a.source_schema=m.schema_id
         AND a.record_kind='memory' AND a.xact=pg_current_xact_id()")
        .fetch_all(&mut **tx).await.map_err(map_err)?;
    let goal_ids:Vec<Uuid>=sqlx::query_scalar("SELECT DISTINCT g.t FROM proxiima_core.goal g
        JOIN proxiima_core.schema_evolution_authority a ON a.source_id=g.t AND a.source_schema=g.schema_id
         AND a.record_kind='goal' AND a.xact=pg_current_xact_id()")
        .fetch_all(&mut **tx).await.map_err(map_err)?;
    let blob_ids:Vec<Uuid>=sqlx::query_scalar("SELECT DISTINCT b.blob_id FROM proxiima_core.blob b
        JOIN proxiima_core.schema_evolution_authority a ON a.source_id=b.blob_id AND a.source_schema=b.schema_id
         AND a.record_kind='blob' AND a.xact=pg_current_xact_id()")
        .fetch_all(&mut **tx).await.map_err(map_err)?;
    incoming_pins(tx, &memory_ids, &goal_ids, &blob_ids).await?;
    super::collaboration::preserve(tx).await?;
    remap_uploads(tx).await?;
    remove_source_sidecars(registry, tx, &memory_ids).await?;
    remove_goal_sidecars(registry, tx, &goal_ids).await?;
    sqlx::query("DELETE FROM proxiima_core.sketch WHERE t=ANY($1::uuid[]) OR t=ANY($2::uuid[])")
        .bind(&memory_ids)
        .bind(&goal_ids)
        .execute(&mut **tx)
        .await
        .map_err(map_err)?;
    let source_content: Vec<Option<Uuid>> = sqlx::query_scalar(
        "DELETE FROM proxiima_core.memory WHERE t=ANY($1::uuid[]) RETURNING content_id",
    )
    .bind(&memory_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_err)?;
    sqlx::query("DELETE FROM proxiima_core.goal WHERE t=ANY($1::uuid[])")
        .bind(&goal_ids)
        .execute(&mut **tx)
        .await
        .map_err(map_err)?;
    sqlx::query("DELETE FROM proxiima_core.blob WHERE blob_id=ANY($1::uuid[])")
        .bind(&blob_ids)
        .execute(&mut **tx)
        .await
        .map_err(map_err)?;
    remove_unused_content(
        tx,
        &source_content.into_iter().flatten().collect::<Vec<_>>(),
    )
    .await?;
    sqlx::query("DELETE FROM proxiima_core.memory_head h WHERE EXISTS (SELECT 1 FROM proxiima_core.schema_evolution_authority a
        WHERE a.xact=pg_current_xact_id() AND a.record_kind='memory' AND a.source_handle=h.handle)
        AND NOT EXISTS (SELECT 1 FROM proxiima_core.memory_including_forgotten m WHERE m.handle=h.handle)")
        .execute(&mut **tx).await.map_err(map_err)?;
    sqlx::query("DELETE FROM proxiima_core.goal_head h WHERE EXISTS (SELECT 1 FROM proxiima_core.schema_evolution_authority a
        WHERE a.xact=pg_current_xact_id() AND a.record_kind='goal' AND a.source_handle=h.handle)
        AND NOT EXISTS (SELECT 1 FROM proxiima_core.goal g WHERE g.handle=h.handle)")
        .execute(&mut **tx).await.map_err(map_err)?;
    validate_targets(tx).await?;
    super::graph::validate(tx).await?;
    // All deferred sidecar constraints must see the still-live narrow permits.
    // The authority-drain constraint itself is checked after removing permits.
    let constraints = data_constraints(tx, registry).await?;
    enforce_data_constraints(tx, &constraints).await?;
    restore_data_constraints(tx, &constraints).await?;
    sqlx::query(
        "DELETE FROM proxiima_core.schema_evolution_authority WHERE xact=pg_current_xact_id()",
    )
    .execute(&mut **tx)
    .await
    .map_err(map_err)?;
    sqlx::query("SET CONSTRAINTS proxiima_core.schema_evolution_authority_drained IMMEDIATE")
        .execute(&mut **tx)
        .await
        .map_err(map_err)?;
    sqlx::query("SET CONSTRAINTS proxiima_core.schema_evolution_authority_drained DEFERRED")
        .execute(&mut **tx)
        .await
        .map_err(map_err)?;
    Ok(())
}

/// Reclaim only content consumed by this plan or by its retired source rows.
/// Retained admissions may already point to new content; replacement admissions
/// keep the old content reachable until the source rows are removed at finish.
pub(super) async fn remove_unused_content(
    tx: &mut Transaction<'_, Postgres>,
    content_ids: &[Uuid],
) -> Result<(), StorageError> {
    sqlx::query("DELETE FROM proxiima_core.content c WHERE c.content_id=ANY($1::uuid[])
        AND NOT EXISTS (SELECT 1 FROM proxiima_core.memory_including_forgotten m WHERE m.content_id=c.content_id)")
        .bind(content_ids).execute(&mut **tx).await.map_err(map_err)?;
    Ok(())
}

pub(super) async fn checkpoint_data_constraints(
    registry: &PgSidecarRegistryFrozen,
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(), StorageError> {
    let constraints = data_constraints(tx, registry).await?;
    enforce_data_constraints(tx, &constraints).await?;
    restore_data_constraints(tx, &constraints).await
}

/// A deferrable catalog constraint, its names parsed where they leave the
/// catalog.
struct DataConstraint {
    schema: PgIdent,
    name: PgIdent,
    initially_deferred: bool,
}

impl DataConstraint {
    fn new(schema: &str, name: &str, initially_deferred: bool) -> Result<Self, StorageError> {
        Ok(Self {
            schema: PgIdent::quoted(schema)?,
            name: PgIdent::quoted(name)?,
            initially_deferred,
        })
    }

    /// `public.finish_probe_a`, the constraint the finish tests create.
    #[cfg(test)]
    fn probe(initially_deferred: bool) -> Self {
        Self::new("public", "finish_probe_a", initially_deferred).expect("plain names")
    }

    async fn set_immediate(&self, tx: &mut Transaction<'_, Postgres>) -> Result<(), StorageError> {
        self.set(tx, " IMMEDIATE").await
    }

    /// Put back the timing the constraint was declared with.
    async fn restore(&self, tx: &mut Transaction<'_, Postgres>) -> Result<(), StorageError> {
        self.set(
            tx,
            if self.initially_deferred {
                " DEFERRED"
            } else {
                " IMMEDIATE"
            },
        )
        .await
    }

    async fn set(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        mode: &'static str,
    ) -> Result<(), StorageError> {
        let mut sql = SqlText::new("SET CONSTRAINTS ");
        sql.push_ident(&self.schema)
            .push(".")
            .push_ident(&self.name)
            .push(mode);
        sqlx::query(sql).execute(&mut **tx).await.map_err(|err| {
            refused(format!(
                "migration constraint {}.{}: {err}",
                self.schema.as_str(),
                self.name.as_str()
            ))
        })?;
        Ok(())
    }
}

async fn data_constraints(
    tx: &mut Transaction<'_, Postgres>,
    registry: &PgSidecarRegistryFrozen,
) -> Result<Vec<DataConstraint>, StorageError> {
    let mut schemas = registry.spiice_schema_names().to_vec();
    schemas.push("proxiima_core".to_owned());
    let rows: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT DISTINCT n.nspname, c.conname, c.condeferred
           FROM pg_constraint c
           JOIN pg_namespace n ON n.oid=c.connamespace
          WHERE c.condeferrable AND n.nspname=ANY($1::text[])
            AND c.conrelid<>'proxiima_core.schema_evolution_authority'::regclass",
    )
    .bind(schemas)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_err)?;
    rows.iter()
        .map(|(schema, name, deferred)| DataConstraint::new(schema, name, *deferred))
        .collect()
}

async fn enforce_data_constraints(
    tx: &mut Transaction<'_, Postgres>,
    constraints: &[DataConstraint],
) -> Result<(), StorageError> {
    for constraint in constraints {
        constraint.set_immediate(tx).await?;
    }
    Ok(())
}

async fn restore_data_constraints(
    tx: &mut Transaction<'_, Postgres>,
    constraints: &[DataConstraint],
) -> Result<(), StorageError> {
    for constraint in constraints {
        constraint.restore(tx).await?;
    }
    Ok(())
}

async fn incoming_pins(
    tx: &mut Transaction<'_, Postgres>,
    memories: &[Uuid],
    goals: &[Uuid],
    blobs: &[Uuid],
) -> Result<(), StorageError> {
    let dangling:bool=sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM proxiima_core.memory_including_forgotten m
        WHERE m.t<>ALL($1::uuid[]) AND (m.origins&&$1::uuid[] OR m.refs&&$1::uuid[] OR m.reads&&$1::uuid[] OR m.forked_from=ANY($1::uuid[])
           OR m.goal_refs&&$2::uuid[] OR m.guided_by&&$2::uuid[] OR m.blob_id=ANY($3::uuid[])))
        OR EXISTS (SELECT 1 FROM proxiima_core.goal g WHERE g.t<>ALL($2::uuid[])
           AND (g.dependency_t&&$2::uuid[] OR g.evidence_t&&$1::uuid[] OR g.motivated_by&&$1::uuid[]
                OR g.close_fact_t=ANY($1::uuid[]) OR g.assignment_t=ANY($1::uuid[]) OR g.write_act_t=ANY($1::uuid[])))")
        .bind(memories).bind(goals).bind(blobs).fetch_one(&mut **tx).await.map_err(map_err)?;
    if dangling {
        return Err(refused(
            "schema migration leaves a source-owned reference unresolved; the owning spiice must supply its reference migration",
        ));
    }
    Ok(())
}

async fn remap_uploads(tx: &mut Transaction<'_, Postgres>) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE proxiima_core.blob_uploads upload SET blob_id=mapping.target_id
        FROM proxiima_core.schema_evolution_terminal_mappings(NULL, ARRAY(
            SELECT source_id FROM proxiima_core.schema_evolution_authority
             WHERE xact=pg_current_xact_id() AND record_kind='blob'
        )) mapping
        JOIN proxiima_core.blob target
          ON target.blob_id=mapping.target_id AND target.schema_id=mapping.target_schema
         AND target.owner_id=mapping.record_owner
        WHERE mapping.record_kind='blob' AND mapping.primary_target
          AND upload.blob_id=mapping.source_id AND upload.owner_id=mapping.record_owner
          AND upload.content_hash=target.content_hash",
    )
    .execute(&mut **tx)
    .await
    .map_err(map_err)?;
    Ok(())
}

async fn remove_source_sidecars(
    registry: &PgSidecarRegistryFrozen,
    tx: &mut Transaction<'_, Postgres>,
    memories: &[Uuid],
) -> Result<(), StorageError> {
    let tables:Vec<String>=sqlx::query_scalar("SELECT DISTINCT unnest(sidecar_tables) FROM proxiima_core.memory_including_forgotten WHERE t=ANY($1::uuid[])")
        .bind(memories).fetch_all(&mut **tx).await.map_err(map_err)?;
    for table in tables {
        let surface = registry
            .evolutions
            .iter()
            .flat_map(|step| step.sources)
            .flat_map(|schema| schema.surfaces)
            .find(|surface| surface.table == table)
            .ok_or_else(|| {
                refused(format!(
                    "source sidecar {table} lacks an explicit migration surface"
                ))
            })?;
        let proxiima_core::spiice::KeyShape::MemoryT { column } = surface.key else {
            return Err(refused("source sidecar lacks a declared Memory key"));
        };
        let table = PgIdent::table(&table)?;
        let column = PgIdent::column(column)?;
        sqlx::query(delete_by_key_sql(&table, &column))
            .bind(memories)
            .execute(&mut **tx)
            .await
            .map_err(map_err)?;
    }
    Ok(())
}

async fn validate_targets(tx: &mut Transaction<'_, Postgres>) -> Result<(), StorageError> {
    let missing: bool = sqlx::query_scalar("SELECT EXISTS (
        SELECT 1 FROM proxiima_core.schema_evolution_authority a WHERE a.xact=pg_current_xact_id()
          AND NOT EXISTS (SELECT 1 FROM proxiima_core.schema_evolution_authority next
              WHERE next.xact=a.xact AND next.record_kind=a.record_kind AND next.source_id=a.target_id AND next.source_schema=a.target_schema)
          AND NOT (CASE a.record_kind
            WHEN 'memory' THEN EXISTS (SELECT 1 FROM proxiima_core.memory_including_forgotten m WHERE m.t=a.target_id AND m.schema_id=a.target_schema AND m.owner_id=a.record_owner AND m.handle=a.target_handle)
            WHEN 'goal' THEN EXISTS (SELECT 1 FROM proxiima_core.goal g WHERE g.t=a.target_id AND g.schema_id=a.target_schema AND g.owner_id=a.record_owner AND g.handle=a.target_handle)
            WHEN 'blob' THEN EXISTS (SELECT 1 FROM proxiima_core.blob b WHERE b.blob_id=a.target_id AND b.schema_id=a.target_schema AND b.owner_id=a.record_owner)
            ELSE false END))")
        .fetch_one(&mut **tx).await.map_err(map_err)?;
    if missing {
        return Err(refused(
            "schema migration is missing a declared terminal target",
        ));
    }
    let invalid:bool=sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM proxiima_core.memory_including_forgotten m
        JOIN proxiima_core.schema_evolution_authority a ON a.target_id=m.t AND a.record_kind='memory' AND a.xact=pg_current_xact_id()
        LEFT JOIN proxiima_core.memory_head h ON h.handle=m.handle
        WHERE h.handle IS NULL OR m.owner_id<>h.owner_id OR m.kind<>h.kind OR m.schema_id<>h.schema_id
           OR (m.kind<>'fact' AND NOT EXISTS (SELECT 1 FROM proxiima_core.content c WHERE c.content_id=m.content_id AND c.owner_id=m.owner_id AND c.schema_id=m.schema_id)))
        OR EXISTS (SELECT 1 FROM proxiima_core.goal g JOIN proxiima_core.schema_evolution_authority a
          ON a.target_id=g.t AND a.record_kind='goal' AND a.xact=pg_current_xact_id()
          LEFT JOIN proxiima_core.goal_head h ON h.handle=g.handle WHERE h.handle IS NULL OR h.owner_id<>g.owner_id OR h.schema_id<>g.schema_id)")
        .fetch_one(&mut **tx).await.map_err(map_err)?;
    if invalid {
        return Err(refused(
            "migration target violates head, owner, or content alignment",
        ));
    }
    let old_rows:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM proxiima_core.memory_including_forgotten m JOIN proxiima_core.schema_evolution e ON e.source_schema_id=m.schema_id AND e.source_kind=m.kind::text)
        OR EXISTS(SELECT 1 FROM proxiima_core.goal g JOIN proxiima_core.schema_evolution e ON e.source_schema_id=g.schema_id AND e.source_kind='goal')
        OR EXISTS(SELECT 1 FROM proxiima_core.blob b JOIN proxiima_core.schema_evolution e ON e.source_schema_id=b.schema_id AND e.source_kind='cited_object')")
        .fetch_one(&mut **tx).await.map_err(map_err)?;
    if old_rows {
        return Err(refused(
            "schema migration did not drain all source representations",
        ));
    }
    Ok(())
}

async fn remove_goal_sidecars(
    registry: &PgSidecarRegistryFrozen,
    tx: &mut Transaction<'_, Postgres>,
    goals: &[Uuid],
) -> Result<(), StorageError> {
    let sources: Vec<(String,Vec<Uuid>)> = sqlx::query_as("SELECT schema_id,array_agg(t) FROM proxiima_core.goal WHERE t=ANY($1::uuid[]) GROUP BY schema_id")
        .bind(goals).fetch_all(&mut **tx).await.map_err(map_err)?;
    for (schema_id, ids) in sources {
        let schema = registry
            .evolutions
            .iter()
            .flat_map(|step| step.sources)
            .find(|s| {
                s.kind == proxiima_core::verbs::schema::PayloadKind::Goal
                    && s.id.render() == schema_id
            })
            .ok_or_else(|| refused("Goal source has no declared migration schema"))?;
        for surface in schema.surfaces {
            let proxiima_core::spiice::KeyShape::GoalT { column } = surface.key else {
                continue;
            };
            let table = PgIdent::table(surface.table)?;
            let column = PgIdent::column(column)?;
            sqlx::query(delete_by_key_sql(&table, &column))
                .bind(&ids)
                .execute(&mut **tx)
                .await
                .map_err(map_err)?;
        }
    }
    Ok(())
}

fn delete_by_key_sql(table: &PgIdent, column: &PgIdent) -> SqlText {
    let mut sql = SqlText::new("DELETE FROM ");
    sql.push_ident(table)
        .push(" WHERE ")
        .push_ident(column)
        .push("=ANY($1::uuid[])");
    sql
}

#[cfg(test)]
mod tests {
    use super::{DataConstraint, enforce_data_constraints, restore_data_constraints};
    use crate::test_fixtures::fresh_pg;

    async fn probe_tx(pool: &sqlx::PgPool) -> sqlx::Transaction<'static, sqlx::Postgres> {
        let mut tx = pool.begin().await.expect("begin");
        sqlx::query(
            "CREATE TABLE public.finish_probe (a int, \
             CONSTRAINT finish_probe_a UNIQUE (a) DEFERRABLE INITIALLY DEFERRED)",
        )
        .execute(&mut *tx)
        .await
        .expect("create the probe table");
        tx
    }

    /// `SET CONSTRAINTS` follows the catalog name it is given: enforce checks
    /// at once, restore puts back the timing the constraint was declared with.
    #[tokio::test]
    async fn constraint_timing_follows_the_named_constraint() {
        let (pg, _db) = fresh_pg("finish_constraints").await;
        let pool = pg.pool_for_tests();

        let mut tx = probe_tx(pool).await;
        sqlx::query("INSERT INTO public.finish_probe VALUES (1), (1)")
            .execute(&mut *tx)
            .await
            .expect("a deferred constraint lets the duplicate in");
        let err = enforce_data_constraints(&mut tx, &[DataConstraint::probe(true)])
            .await
            .expect_err("enforce checks the duplicate now");
        assert!(err.to_string().contains("finish_probe_a"), "{err}");
        drop(tx);

        let mut tx = probe_tx(pool).await;
        enforce_data_constraints(&mut tx, &[DataConstraint::probe(true)])
            .await
            .expect("nothing to violate yet");
        restore_data_constraints(&mut tx, &[DataConstraint::probe(true)])
            .await
            .expect("restore the declared deferral");
        sqlx::query("INSERT INTO public.finish_probe VALUES (1), (1)")
            .execute(&mut *tx)
            .await
            .expect("deferred again, so the duplicate waits for commit");
        drop(tx);

        let mut tx = probe_tx(pool).await;
        restore_data_constraints(&mut tx, &[DataConstraint::probe(false)])
            .await
            .expect("restore an immediate constraint");
        sqlx::query("INSERT INTO public.finish_probe VALUES (1), (1)")
            .execute(&mut *tx)
            .await
            .expect_err("immediate means the duplicate fails in its statement");
    }
}
