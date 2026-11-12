//! Query starts at `memory_head`. `HeadsOnly` = current `t` per handle.
//! `IncludeSuperseded` = every hot `t` of those handles.

use std::collections::HashMap;

use proxiima_core::read_models::MemorySchemaSpec;
use proxiima_core::verbs::query::{
    EntityKind, MemoryRow, QueryCursor, QueryRequest, QueryResponse, SupersessionStatus,
};
use proxiima_core::verbs::schema::PayloadKind;
use proxiima_core::{MemoryId, OwnerId, SchemaId, SidecarPayload, SqlText, StorageError};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::map_err;
use crate::pg_enums::{PgMemoryKind, memory_payload_kind};
use crate::sidecars::{PgSidecarKey, PgSidecarReadCtx, PgSidecarRegistryFrozen};
use proxiima_core::PgIdent;

use super::edges::query_edges;
use super::goals::query_goals_on_connection;
use super::rows::{MemoryRowDb, memory_row_from_db};

/// Transaction-backed query path. Every statement, including typed sidecar
/// hydration, uses the caller's one borrowed connection.
pub(crate) async fn query_memories_on_connection(
    connection: &mut PgConnection,
    sidecars: &PgSidecarRegistryFrozen,
    read_owners: &[OwnerId],
    req: &QueryRequest,
    schemas: &[MemorySchemaSpec],
) -> Result<QueryResponse, StorageError> {
    let owner_ids: Vec<Uuid> = read_owners
        .iter()
        .copied()
        .map(proxiima_core::OwnerId::into_inner)
        .collect();
    if matches!(req.entity_kind, Some(EntityKind::Goal)) {
        let (goals, next_cursor) = query_goals_on_connection(connection, req, &owner_ids).await?;
        return Ok(QueryResponse {
            memories: Vec::new(),
            goals,
            edges: Vec::new(),
            next_cursor,
        });
    }
    let single_memory_stream = is_single_memory_stream(req);
    let mut rows =
        fetch_memory_page_on_connection(connection, req, &owner_ids, schemas, sidecars).await?;
    let limit = usize::try_from(req.limit)
        .map_err(|_| StorageError::Internal("query limit does not fit usize".into()))?;
    let next_memory_cursor = if single_memory_stream && rows.len() > limit {
        rows.truncate(limit);
        rows.last().map(|row| QueryCursor::Memory {
            created_at: row.created_at,
            memory_id: MemoryId::new(row.memory_id),
        })
    } else {
        None
    };
    let mut memories =
        project_memory_rows_on_connection(connection, sidecars, req, schemas, rows).await?;
    let (goals, next_goal_cursor) = if req.entity_kind.is_none() {
        query_goals_on_connection(connection, req, &owner_ids).await?
    } else {
        (Vec::new(), None)
    };
    let visible_goal_ids: Vec<Uuid> = goals.iter().map(|row| row.id.into_inner()).collect();
    demote_invisible_goal_refs(&mut memories, &visible_goal_ids);
    let edges = query_edges(req, &memories, &visible_goal_ids);
    Ok(QueryResponse {
        memories,
        goals,
        edges,
        next_cursor: next_memory_cursor.or(next_goal_cursor),
    })
}

/// Read one page of memory rows for the request's filters.
///
/// The filter list is built once and then read twice — once to number the
/// placeholders, once to bind them. That is the whole point: the SQL and the
/// arguments are projections of one value, so "the query has N placeholders"
/// and "N arguments were bound" cannot disagree. They used to be two lists
/// grown from the same four predicates, each evaluated twice.
async fn fetch_memory_page_on_connection(
    connection: &mut PgConnection,
    req: &QueryRequest,
    owner_ids: &[Uuid],
    schemas: &[MemorySchemaSpec],
    sidecars: &PgSidecarRegistryFrozen,
) -> Result<Vec<MemoryRowDb>, StorageError> {
    let filters = memory_filters(req, schemas, sidecars)?;
    let heads_only = matches!(req.supersession, SupersessionStatus::HeadsOnly);
    let owner_sql = memory_page_sql(heads_only, &filters, page_fetch_limit(req));
    let goal_routes: bool = if heads_only {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM proxiima_core.goal_visible_memory_ids())")
            .fetch_one(&mut *connection)
            .await
            .map_err(map_err)?
    } else {
        false
    };
    let sql = if goal_routes {
        merge_goal_heads_sql(&owner_sql, &filters, page_fetch_limit(req))
    } else {
        owner_sql
    };
    let mut q = sqlx::query_as::<_, MemoryRowDb>(sql).bind(owner_ids);
    for filter in filters {
        q = match filter {
            MemoryFilter::Schema(schema_id) => q.bind(schema_id),
            // The kind is a literal in the SQL (see `MemoryFilter::predicate`).
            MemoryFilter::Kind(_) | MemoryFilter::NoMatches => q,
            MemoryFilter::Ids(ids) => q.bind(ids),
            MemoryFilter::ReferencedGoalHandles(handles) => q.bind(handles),
            MemoryFilter::Cursor(t) => q.bind(t),
            MemoryFilter::Tags { specs, tags, .. } => {
                let mut q = q;
                for spec in specs {
                    q = q.bind(spec.schema_id);
                }
                q.bind(tags)
            }
        };
    }
    q.fetch_all(&mut *connection).await.map_err(map_err)
}

/// An optional `WHERE` predicate together with the value it binds.
///
/// Keeping the two halves in one value is what removes the bookkeeping: a
/// filter contributes a predicate and an argument or neither, and its
/// placeholder number is its position among the filters that bind.
/// `Kind` is the one filter that binds nothing: its value is one of three
/// closed `PgMemoryKind` literals written into the SQL, so a generic plan
/// can still prove a per-kind partial index predicate (a `$n` cannot).
enum MemoryFilter {
    NoMatches,
    Schema(String),
    Kind(PgMemoryKind),
    Ids(Vec<Uuid>),
    ReferencedGoalHandles(Vec<Uuid>),
    Cursor(Uuid),
    Tags {
        specs: Vec<TagSpec>,
        tags: Vec<String>,
        tag_match: proxiima_core::verbs::query::TagMatch,
    },
}

/// One schema's tag lookup. The identifiers are validated when the spec is
/// built and are the values the predicate splices.
struct TagSpec {
    schema_id: String,
    table: PgIdent,
    key_column: PgIdent,
    tag_column: PgIdent,
}

impl MemoryFilter {
    /// This filter's predicate at `placeholder`, which is its position in
    /// the list the caller will bind in the same order.
    ///
    /// Head kind/schema are immutable and checked against every inserted
    /// memory version. Filtering the ordered head stream avoids visiting
    /// versions that cannot be in this page.
    fn predicate(&self, placeholder: usize, heads_only: bool) -> SqlText {
        let alias = if heads_only { "h" } else { "m" };
        let at = placeholder as u64;
        let mut sql = SqlText::new("");
        match self {
            Self::NoMatches => {
                sql.push(" AND FALSE");
            }
            Self::Schema(_) => {
                sql.push(" AND ")
                    .push(alias)
                    .push(".schema_id = $")
                    .push_u64(at);
            }
            Self::Kind(kind) => {
                sql.push(" AND ")
                    .push(alias)
                    .push(".kind = '")
                    .push(kind.label())
                    .push("'::proxiima_core.memory_kind");
            }
            Self::Ids(_) => {
                sql.push(" AND ")
                    .push(alias)
                    .push(".t = ANY($")
                    .push_u64(at)
                    .push("::uuid[])");
            }
            Self::ReferencedGoalHandles(_) => {
                sql.push(
                    " AND m.kind = 'fact' \
                 AND m.goal_refs && ARRAY(SELECT g.t FROM proxiima_core.goal g \
                 WHERE g.handle = ANY($",
                )
                .push_u64(at)
                .push(
                    "::uuid[]) \
                   AND g.owner_id = ANY($1::uuid[]))",
                );
            }
            Self::Cursor(_) => {
                sql.push(" AND ").push(alias).push(".t < $").push_u64(at);
            }
            Self::Tags {
                specs, tag_match, ..
            } => {
                let tags_placeholder = (placeholder + specs.len()) as u64;
                let op = match tag_match {
                    proxiima_core::verbs::query::TagMatch::Any => "&&",
                    proxiima_core::verbs::query::TagMatch::All => "@>",
                };
                // Keep each lookup correlated to its memory key. Pulling
                // this up can rescan the sidecar's RLS visibility relation
                // per page; OFFSET 0 preserves the one-key probe.
                sql.push(" AND (");
                for (index, spec) in specs.iter().enumerate() {
                    if index > 0 {
                        sql.push(" OR ");
                    }
                    sql.push("(")
                        .push(alias)
                        .push(".schema_id = $")
                        .push_u64((placeholder + index) as u64)
                        .push(" AND EXISTS (SELECT 1 FROM ")
                        .push_ident(&spec.table)
                        .push(" c WHERE c.")
                        .push_ident(&spec.key_column)
                        .push(" = ")
                        .push(alias)
                        .push(".t AND c.")
                        .push_ident(&spec.tag_column)
                        .push(" ")
                        .push(op)
                        .push(" $")
                        .push_u64(tags_placeholder)
                        .push("::text[] OFFSET 0))");
                }
                sql.push(")");
            }
        }
        sql
    }

    /// Whether this filter takes a bind (and so a placeholder number).
    fn bind_count(&self) -> usize {
        match self {
            Self::Kind(_) | Self::NoMatches => 0,
            Self::Tags { specs, .. } => specs.len() + 1,
            Self::Schema(_) | Self::Ids(_) | Self::ReferencedGoalHandles(_) | Self::Cursor(_) => 1,
        }
    }
}

/// Every filter the request asks for, in bind order.
///
/// The single derivation of "what does this request filter on": the page
/// query and [`memory_page_sql_for_tests`] both read it, so the test cannot
/// assert against a shape production does not build.
fn memory_filters(
    req: &QueryRequest,
    schemas: &[MemorySchemaSpec],
    sidecars: &PgSidecarRegistryFrozen,
) -> Result<Vec<MemoryFilter>, StorageError> {
    let mut filters = Vec::new();
    if let Some(schema_id) = req.schema_id.as_ref() {
        filters.push(MemoryFilter::Schema(schema_id.as_str().to_owned()));
    }
    if let Some(kind) = req
        .entity_kind
        .and_then(proxiima_core::MemoryKind::of)
        .map(PgMemoryKind::from)
    {
        filters.push(MemoryFilter::Kind(kind));
    }
    if !req.referenced_goal_handles.is_empty() {
        filters.push(MemoryFilter::ReferencedGoalHandles(
            req.referenced_goal_handles.clone(),
        ));
    }
    if !req.memory_ids.is_empty() {
        filters.push(MemoryFilter::Ids(
            req.memory_ids.iter().map(|id| id.into_inner()).collect(),
        ));
    }
    if let Some(QueryCursor::Memory { memory_id, .. }) = &req.page.after {
        filters.push(MemoryFilter::Cursor(memory_id.into_inner()));
    }
    if !req.tags.is_empty() {
        let mut specs = Vec::new();
        for spec in schemas {
            if req.entity_kind.is_some_and(|kind| kind != spec.kind)
                || req
                    .schema_id
                    .as_ref()
                    .is_some_and(|schema_id| schema_id != &spec.schema_id)
            {
                continue;
            }
            let Some(tag_column) = spec.tag_field.as_ref() else {
                continue;
            };
            let Some(table) = spec.sidecar_table.as_ref() else {
                continue;
            };
            let Some(key_column) = sidecars.memory_key_column(table) else {
                continue;
            };
            specs.push(TagSpec {
                schema_id: spec.schema_id.as_str().to_owned(),
                table: PgIdent::table(table)?,
                key_column: PgIdent::column(key_column)?,
                tag_column: PgIdent::column(tag_column)?,
            });
        }
        filters.push(if specs.is_empty() {
            MemoryFilter::NoMatches
        } else {
            MemoryFilter::Tags {
                specs,
                tags: req.tags.clone(),
                tag_match: req.tag_match,
            }
        });
    }
    Ok(filters)
}

/// Whether this request reads one memory stream, and so can be told a full
/// page from an exhausted one by fetching a row past the limit.
fn is_single_memory_stream(req: &QueryRequest) -> bool {
    matches!(
        req.entity_kind,
        Some(EntityKind::Fact | EntityKind::Abstraction | EntityKind::Perspective)
    )
}

fn page_fetch_limit(req: &QueryRequest) -> u64 {
    if is_single_memory_stream(req) {
        u64::from(req.limit) + 1
    } else {
        u64::from(req.limit)
    }
}

/// Resolve each row's schema, verify its sidecar stamp, and project the page
/// into read-model rows.
async fn project_memory_rows_on_connection(
    connection: &mut PgConnection,
    sidecars: &PgSidecarRegistryFrozen,
    req: &QueryRequest,
    schemas: &[MemorySchemaSpec],
    rows: Vec<MemoryRowDb>,
) -> Result<Vec<MemoryRow>, StorageError> {
    let mut schema_versions = HashMap::new();
    for row in &rows {
        let kind = EntityKind::from(row.kind);
        let spec = proxiima_core::resolve_memory_schema(
            schemas,
            kind,
            &SchemaId::new(row.schema_id.clone()),
        )?;
        validate_row_stamp(
            sidecars,
            spec,
            &row.sidecar_tables,
            MemoryId::new(row.memory_id),
        )?;
        schema_versions.insert(MemoryId::new(row.memory_id), spec.schema_version);
    }
    let mut payloads =
        load_row_payloads_batch_on_connection(connection, sidecars, schemas, &rows).await?;
    let mut memories = Vec::with_capacity(rows.len());
    for row in rows {
        let id = MemoryId::new(row.memory_id);
        let loaded_payload = payloads.remove(&id);
        let schema_version = schema_versions
            .remove(&id)
            .ok_or_else(|| StorageError::Internal("query schema resolution lost row".into()))?;
        if proxiima_core::resolve_memory_schema(
            schemas,
            EntityKind::from(row.kind),
            &SchemaId::new(row.schema_id.clone()),
        )?
        .sidecar_table
        .as_deref()
        .is_some_and(|table| {
            loaded_payload.is_none() && !sidecars.is_owner_pinned_memory_sidecar_table(table)
        }) {
            return Err(StorageError::ConstraintViolation(format!(
                "required sidecar payload missing for memory {id:?}"
            )));
        }
        let payload = req.include_payloads.then_some(loaded_payload).flatten();
        memories.push(memory_row_from_db(row, payload, schema_version)?);
    }
    Ok(memories)
}

/// A Goal reference the caller may not read is not a Goal edge for this
/// response; it stays a plain reference rather than disappearing.
fn demote_invisible_goal_refs(memories: &mut [MemoryRow], visible_goal_ids: &[Uuid]) {
    let visible_goal_set: std::collections::HashSet<Uuid> =
        visible_goal_ids.iter().copied().collect();
    for memory in memories {
        let mut visible = Vec::with_capacity(memory.goal_refs.len());
        for goal in memory.goal_refs.drain(..) {
            if visible_goal_set.contains(&goal.into_inner()) {
                visible.push(goal);
            } else {
                memory.refs.push(MemoryId::new(goal.into_inner()));
            }
        }
        memory.goal_refs = visible;
        let mut visible_guidance = Vec::with_capacity(memory.guided_by.len());
        for goal in memory.guided_by.drain(..) {
            if visible_goal_set.contains(&goal.into_inner()) {
                visible_guidance.push(goal);
            } else {
                memory.refs.push(MemoryId::new(goal.into_inner()));
            }
        }
        memory.guided_by = visible_guidance;
    }
}

async fn load_row_payloads_batch_on_connection(
    connection: &mut PgConnection,
    sidecars: &PgSidecarRegistryFrozen,
    schemas: &[MemorySchemaSpec],
    rows: &[MemoryRowDb],
) -> Result<HashMap<MemoryId, SidecarPayload>, StorageError> {
    let mut ids_by_key = HashMap::<PgSidecarKey, Vec<MemoryId>>::new();
    for row in rows {
        let entity_kind = EntityKind::from(row.kind);
        let spec = proxiima_core::resolve_memory_schema(
            schemas,
            entity_kind,
            &SchemaId::new(row.schema_id.clone()),
        )?;
        let kind = PayloadKind::from(row.kind);
        validate_row_stamp(
            sidecars,
            spec,
            &row.sidecar_tables,
            MemoryId::new(row.memory_id),
        )?;
        let Some(schema_sidecar) = spec.sidecar_table.as_ref() else {
            continue;
        };
        let _ = schema_sidecar;
        let key = PgSidecarKey::new(kind, spec.schema_id.clone(), spec.schema_version);
        if sidecars.contains(&key) {
            ids_by_key
                .entry(key)
                .or_default()
                .push(MemoryId::new(row.memory_id));
        }
    }
    let mut result = HashMap::new();
    for (key, ids) in ids_by_key {
        let rows = sidecars
            .load_memory_payloads_batch(PgSidecarReadCtx::from(&mut *connection), &key, &ids)
            .await?;
        result.extend(rows);
    }
    Ok(result)
}

fn memory_page_sql(heads_only: bool, filters: &[MemoryFilter], fetch_limit: u64) -> SqlText {
    let reference_filter = filters
        .iter()
        .any(|filter| matches!(filter, MemoryFilter::ReferencedGoalHandles(_)));
    if heads_only && !reference_filter {
        return head_page_sql(filters, fetch_limit);
    }
    // Goal-reference predicates start at memory's GIN index. Joining heads
    // here preserves head membership without visiting unrelated heads first.
    let head_join = if heads_only {
        "JOIN proxiima_core.memory_head h ON h.handle = m.handle AND h.t = m.t "
    } else {
        ""
    };
    let mut sql = SqlText::new(
        "SELECT m.t AS memory_id, m.handle, \
                proxiima_core.memory_created_at(m.t) AS created_at, \
                m.owner_id, m.schema_id, m.sidecar_tables, \
                m.kind AS kind, m.origins, m.refs, m.goal_refs, m.guided_by, m.reads, m.occurred_at, m.\"by\", m.via \
         FROM proxiima_core.live_memory m ",
    );
    sql.push(head_join)
        .push("WHERE m.owner_id = ANY($1::uuid[])");
    append_memory_filters(&mut sql, filters, false);
    sql.push(" ORDER BY m.t DESC LIMIT ").push_u64(fetch_limit);
    sql
}

/// Top K of the union is contained in the union of each owner's top K.
/// Owners' primary key deduplicates the requested set. Both RLS checks and
/// every filter precede the per-owner limit; an invisible or missing memory
/// never consumes a slot. No limit is applied to heads before that lookup.
fn head_page_sql(filters: &[MemoryFilter], fetch_limit: u64) -> SqlText {
    let mut sql = SqlText::new(
        "SELECT page.* \
         FROM proxiima_core.owners o CROSS JOIN LATERAL ( \
           SELECT m.t AS memory_id, m.handle, \
                  proxiima_core.memory_created_at(m.t) AS created_at, \
                  m.owner_id, m.schema_id, m.sidecar_tables, \
                  m.kind AS kind, m.origins, m.refs, m.goal_refs, m.guided_by, m.reads, m.occurred_at, m.\"by\", m.via \
           FROM (SELECT h.handle, h.t FROM proxiima_core.memory_head h \
                 WHERE h.owner_id = o.owner_id",
    );
    append_memory_filters(&mut sql, filters, true);
    // Preserve the ordered head stream and one-row memory lookup as separate
    // planner inputs. Without these barriers a pulled-up join can hydrate all
    // matching versions before sorting. With no ordering index the head sort
    // still happens before memory hydration (the pre-activation schema).
    sql.push(
        " ORDER BY h.t DESC OFFSET 0) h \
           CROSS JOIN LATERAL ( \
             SELECT m.* FROM proxiima_core.live_memory m \
             WHERE m.handle = h.handle AND m.t = h.t AND m.owner_id = o.owner_id \
             OFFSET 0 \
           ) m \
           ORDER BY h.t DESC LIMIT ",
    )
    .push_u64(fetch_limit)
    .push(
        " \
         ) page WHERE o.owner_id = ANY($1::uuid[]) \
         ORDER BY page.memory_id DESC LIMIT ",
    )
    .push_u64(fetch_limit);
    sql
}

/// Merge two bounded sorted streams. Owner heads keep their existing cheap
/// plan; the second stream contains only exact Goal-authorised admissions.
fn merge_goal_heads_sql(
    owner_sql: &SqlText,
    filters: &[MemoryFilter],
    fetch_limit: u64,
) -> SqlText {
    let mut goal_sql = SqlText::new(
        "WITH visible AS MATERIALIZED (SELECT * FROM proxiima_core.goal_visible_memory_ids()), \
         latest AS (SELECT DISTINCT ON (memory_handle) memory_t, memory_handle, owner_id \
                    FROM visible ORDER BY memory_handle, memory_t DESC) \
         SELECT m.t AS memory_id, m.handle, proxiima_core.memory_created_at(m.t) AS created_at, \
                m.owner_id, m.schema_id, m.sidecar_tables, m.kind AS kind, m.origins, m.refs, \
                m.goal_refs, m.guided_by, m.reads, m.occurred_at, m.\"by\", m.via \
         FROM latest v JOIN proxiima_core.live_memory m ON m.t = v.memory_t \
         WHERE m.owner_id = ANY($1::uuid[]) \
           AND NOT EXISTS (SELECT 1 FROM proxiima_core.memory_head own_head \
                           WHERE own_head.handle = m.handle AND own_head.t > m.t)",
    );
    append_memory_filters(&mut goal_sql, filters, false);
    goal_sql
        .push(" ORDER BY m.t DESC LIMIT ")
        .push_u64(fetch_limit);
    let mut sql = SqlText::new("SELECT * FROM ((");
    sql.push_text(owner_sql)
        .push(") UNION (")
        .push_text(&goal_sql)
        .push(")) admitted ORDER BY memory_id DESC LIMIT ")
        .push_u64(fetch_limit);
    sql
}

fn append_memory_filters(sql: &mut SqlText, filters: &[MemoryFilter], heads_only: bool) {
    // `$1` is the owner array; the binding filters take the placeholders
    // after it, densely, in the order the caller binds them. A filter that
    // binds nothing (`Kind`) consumes no number.
    let mut placeholder = 2;
    for filter in filters {
        sql.push_text(&filter.predicate(placeholder, heads_only));
        placeholder += filter.bind_count();
    }
}

fn validate_row_stamp(
    sidecars: &PgSidecarRegistryFrozen,
    spec: &MemorySchemaSpec,
    stamped_tables: &[String],
    memory_id: MemoryId,
) -> Result<(), StorageError> {
    let Some(table) = spec.sidecar_table.as_deref() else {
        return Ok(());
    };
    let kind = memory_payload_kind(spec.kind)?;
    if sidecars.table_for_schema(kind, &spec.schema_id, spec.schema_version) != Some(table)
        || !stamped_tables.iter().any(|stamped| stamped == table)
    {
        return Err(StorageError::ConstraintViolation(format!(
            "memory {memory_id:?} has invalid sidecar stamp for {table}"
        )));
    }
    Ok(())
}

/// The page SQL [`query_memories`] would run for `req`.
///
/// It calls the same [`memory_filters`] and [`page_fetch_limit`] the page
/// read calls, so this cannot describe a query production would not build.
///
/// # Errors
///
/// Never fails on the timeseries path (stateful-NK filters are not used).
#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
pub fn memory_page_sql_for_tests(req: &QueryRequest) -> Result<SqlText, StorageError> {
    Ok(memory_page_sql(
        matches!(req.supersession, SupersessionStatus::HeadsOnly),
        &memory_filters(req, &[], &PgSidecarRegistryFrozen::default())?,
        page_fetch_limit(req),
    ))
}

/// Render tagged query SQL with the frozen schema metadata used by production.
#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
pub fn memory_page_sql_for_tests_with_schemas(
    req: &QueryRequest,
    schemas: &[MemorySchemaSpec],
    sidecars: &PgSidecarRegistryFrozen,
) -> Result<SqlText, StorageError> {
    Ok(memory_page_sql(
        matches!(req.supersession, SupersessionStatus::HeadsOnly),
        &memory_filters(req, schemas, sidecars)?,
        page_fetch_limit(req),
    ))
}

#[cfg(test)]
mod tests {
    use super::{MemoryFilter, MemorySchemaSpec, TagSpec, validate_row_stamp};
    use crate::sidecars::{PgSidecarRegistryFrozen, core_pg_sidecars};
    use proxiima_core::PgIdent;
    use proxiima_core::{AgentNoteV1, EntityKind, FactPayload, MemoryId, SchemaId, SchemaVersion};

    /// The tag lookup splices the checked identifiers and nothing else: its
    /// text is the statement the planner has always seen.
    #[test]
    fn the_tag_join_is_the_correlated_probe_over_the_checked_names() {
        let spec = |schema_id: &str, table: &str| TagSpec {
            schema_id: schema_id.to_owned(),
            table: PgIdent::table(table).unwrap(),
            key_column: PgIdent::column("memory_key").unwrap(),
            tag_column: PgIdent::column("tags").unwrap(),
        };
        let filter = MemoryFilter::Tags {
            specs: vec![
                spec("core/a-v1", "proxiima_fixture.a_v1"),
                spec("core/b-v1", "proxiima_fixture.b_v1"),
            ],
            tags: vec!["x".to_owned()],
            tag_match: proxiima_core::verbs::query::TagMatch::Any,
        };
        assert_eq!(
            filter.predicate(2, true).as_str(),
            " AND ((h.schema_id = $2 AND EXISTS (SELECT 1 FROM proxiima_fixture.a_v1 c WHERE c.memory_key = h.t AND c.tags && $4::text[] OFFSET 0)) OR (h.schema_id = $3 AND EXISTS (SELECT 1 FROM proxiima_fixture.b_v1 c WHERE c.memory_key = h.t AND c.tags && $4::text[] OFFSET 0)))"
        );
    }

    #[test]
    fn required_primary_stamp_is_checked_against_the_frozen_sidecar_registry() {
        let spec = MemorySchemaSpec::new(
            EntityKind::Fact,
            SchemaId::new("test/fact".to_owned()),
            SchemaVersion::new(2),
            Some("test.fact_v2".to_owned()),
        );
        let err = validate_row_stamp(
            &PgSidecarRegistryFrozen::default(),
            &spec,
            &[],
            MemoryId::new(uuid::Uuid::now_v7()),
        )
        .expect_err("unregistered or unstamped primary must fail closed");
        assert!(err.to_string().contains("invalid sidecar stamp"));
    }

    #[test]
    fn primary_stamp_must_be_present_but_declared_extensions_do_not_interfere() {
        let registry = core_pg_sidecars();
        let primary = AgentNoteV1::sidecar_table().expect("AgentNote has a primary sidecar");
        let spec = MemorySchemaSpec::new(
            EntityKind::Fact,
            AgentNoteV1::schema_id(),
            SchemaVersion::new(AgentNoteV1::SCHEMA_VERSION),
            Some(primary.to_owned()),
        );
        let memory_id = MemoryId::new(uuid::Uuid::now_v7());

        validate_row_stamp(&registry, &spec, &[primary.to_owned()], memory_id)
            .expect("the registered primary stamp agrees");
        validate_row_stamp(
            &registry,
            &spec,
            &["proxiima_core.write_act_v1".to_owned(), primary.to_owned()],
            memory_id,
        )
        .expect("an additional declared extension does not select the schema");
        let err = validate_row_stamp(
            &registry,
            &spec,
            &["proxiima_core.write_act_v1".to_owned()],
            memory_id,
        )
        .expect_err("a different declared table cannot stand in for the primary");
        assert!(err.to_string().contains("invalid sidecar stamp"));
    }

    #[test]
    fn kind_is_a_literal_and_placeholders_are_dense_in_bind_order() {
        use super::MemoryFilter;
        use crate::pg_enums::PgMemoryKind;
        let id = uuid::Uuid::now_v7();
        for heads_only in [true, false] {
            let filters = [
                MemoryFilter::Schema("s".to_owned()),
                MemoryFilter::Kind(PgMemoryKind::Perspective),
                MemoryFilter::Ids(vec![id]),
                MemoryFilter::Cursor(id),
            ];
            let sql = super::memory_page_sql(heads_only, &filters, 10);
            let sql = sql.as_str();
            let alias = if heads_only { "h" } else { "m" };
            assert!(
                sql.contains(&format!(
                    "{alias}.kind = 'perspective'::proxiima_core.memory_kind"
                )),
                "kind must be a literal so a generic plan can prove the partial index: {sql}"
            );
            assert!(sql.contains(&format!("{alias}.schema_id = $2")), "{sql}");
            assert!(
                sql.contains(&format!("{alias}.t = ANY($3::uuid[])")),
                "{sql}"
            );
            assert!(sql.contains(&format!("{alias}.t < $4")), "{sql}");
            assert!(!sql.contains("$5"), "no placeholder past the binds: {sql}");
            let binds: usize = filters.iter().map(super::MemoryFilter::bind_count).sum();
            assert_eq!(binds, 3, "Kind binds nothing");
        }
        // Kind first: the next binding filter still takes $2.
        let sql = super::memory_page_sql(
            true,
            &[
                MemoryFilter::Kind(PgMemoryKind::Fact),
                MemoryFilter::Cursor(id),
            ],
            10,
        );
        let sql = sql.as_str();
        assert!(sql.contains("h.t < $2") && !sql.contains("$3"), "{sql}");
    }

    #[test]
    fn heads_only_schema_predicates_use_head_columns() {
        let sql = super::memory_page_sql(true, &[super::MemoryFilter::Schema("s".to_owned())], 10);
        let sql = sql.as_str();
        assert!(
            sql.contains("o.owner_id = ANY($1::uuid[])") && sql.contains("h.owner_id = o.owner_id"),
            "HeadsOnly scans each requested owner once: {sql}"
        );
        assert!(
            sql.contains("h.schema_id = $2"),
            "HeadsOnly schema filter must constrain the ordered head scan: {sql}"
        );
        assert!(
            !sql.contains("m.owner_id = ANY"),
            "HeadsOnly must not predicate m.owner_id: {sql}"
        );
        assert!(
            !sql.contains("AND m.schema_id"),
            "HeadsOnly must not predicate m.schema_id: {sql}"
        );
    }
}
