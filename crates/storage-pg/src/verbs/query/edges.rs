use proxiima_core::SqlText;
use proxiima_core::storage_ports::{InboundPinQuery, PinTargetKind};
use proxiima_core::verbs::query::{QueryRequest, SupersessionStatus};
use proxiima_core::{
    Edge, EdgeKind, GoalId, MemoryId, MemoryPinField, OwnerId, PinNode, StorageError,
    project_window_edges,
};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};

use crate::error::map_err;
use crate::pg_enums::PgMemoryKind;
use crate::pin_fields;

/// Hard upper bound on edges returned by snapshot-edge mode.
/// Decoupled from `QueryRequest::limit`, which sizes the node window.
const MAX_SNAPSHOT_EDGES: usize = 50_000;

/// A pin carrier as the statements of this module select it
/// ([`pin_fields::pin_carrier_columns`]): each pin column decodes into the
/// slot its field names.
struct PinNodeRow(PinNode);

impl<'r> sqlx::FromRow<'r, PgRow> for PinNodeRow {
    fn from_row(row: &'r PgRow) -> Result<Self, sqlx::Error> {
        let kind: PgMemoryKind = row.try_get("kind")?;
        let mut node = PinNode {
            id: MemoryId::new(row.try_get("t")?),
            kind: kind.into(),
            schema_id: proxiima_core::SchemaId::new(row.try_get::<String, _>("schema_id")?),
            origins: Vec::new(),
            refs: Vec::new(),
            goal_refs: Vec::new(),
            guided_by: Vec::new(),
            reads: Vec::new(),
            forked_from: None,
        };
        for field in MemoryPinField::ALL {
            let column = field.spec().column;
            match field {
                MemoryPinField::Origins => node.origins = memory_ids(row, column)?,
                MemoryPinField::Refs => node.refs = memory_ids(row, column)?,
                MemoryPinField::GoalRefs => node.goal_refs = goal_ids(row, column)?,
                MemoryPinField::GuidedBy => node.guided_by = goal_ids(row, column)?,
                MemoryPinField::Reads => node.reads = memory_ids(row, column)?,
                MemoryPinField::ForkedFrom => {
                    node.forked_from = row
                        .try_get::<Option<uuid::Uuid>, _>(column)?
                        .map(MemoryId::new);
                }
            }
        }
        Ok(Self(node))
    }
}

fn memory_ids(row: &PgRow, column: &str) -> Result<Vec<MemoryId>, sqlx::Error> {
    let ids: Vec<uuid::Uuid> = row.try_get(column)?;
    Ok(ids.into_iter().map(MemoryId::new).collect())
}

fn goal_ids(row: &PgRow, column: &str) -> Result<Vec<GoalId>, sqlx::Error> {
    let ids: Vec<uuid::Uuid> = row.try_get(column)?;
    Ok(ids.into_iter().map(GoalId::new).collect())
}

/// Resolve readable Goal ids in a batch. Unreadable or unknown ids are left
/// in the raw reference carrier so the engine can redact them.
pub(crate) async fn load_visible_goal_ids_on_connection(
    connection: &mut PgConnection,
    read_owners: &[OwnerId],
    goal_ids: &[GoalId],
) -> Result<Vec<GoalId>, StorageError> {
    if goal_ids.is_empty() || read_owners.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<uuid::Uuid> = goal_ids.iter().map(|id| id.into_inner()).collect();
    let owner_ids = owner_ids(read_owners);
    let rows: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT t FROM proxiima_core.goal WHERE t = ANY($1::uuid[]) AND owner_id = ANY($2::uuid[])",
    )
    .bind(&ids)
    .bind(&owner_ids)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_err)?;
    Ok(rows.into_iter().map(GoalId::new).collect())
}

/// Owner-scoped PK load of pin carriers.
pub(crate) async fn load_pin_nodes_on_connection(
    connection: &mut PgConnection,
    read_owners: &[OwnerId],
    memory_ids: &[MemoryId],
) -> Result<Vec<PinNode>, StorageError> {
    if memory_ids.is_empty() || read_owners.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<uuid::Uuid> = memory_ids.iter().map(|id| id.into_inner()).collect();
    let owner_ids = owner_ids(read_owners);
    let mut sql = SqlText::new("SELECT ");
    sql.push_text(&pin_fields::pin_carrier_columns("")).push(
        " FROM proxiima_core.live_memory WHERE t = ANY($1::uuid[]) AND owner_id = ANY($2::uuid[])",
    );
    let rows: Vec<PinNodeRow> = sqlx::query_as(sql)
        .bind(&ids)
        .bind(&owner_ids)
        .fetch_all(&mut *connection)
        .await
        .map_err(map_err)?;
    Ok(rows.into_iter().map(|row| row.0).collect())
}

/// Owner-scoped GIN page of rows that list any of `query.targets`.
pub(crate) async fn load_inbound_pin_nodes_on_connection(
    connection: &mut PgConnection,
    read_owners: &[OwnerId],
    query: InboundPinQuery<'_>,
) -> Result<Vec<PinNode>, StorageError> {
    if query.targets.is_empty() || read_owners.is_empty() {
        return Ok(Vec::new());
    }
    if query.limit == 0 {
        return Err(StorageError::ConstraintViolation(
            "inbound pin page limit must be at least 1".into(),
        ));
    }
    let ids: Vec<uuid::Uuid> = query.targets.iter().map(|id| id.into_inner()).collect();
    let owner_ids = owner_ids(read_owners);
    let sql = inbound_pin_sql(query.supersession, query.kind, query.target_kind);
    let rows: Vec<PinNodeRow> = sqlx::query_as(sql)
        .bind(&ids)
        .bind(&owner_ids)
        .bind(query.after.map(MemoryId::into_inner))
        .bind(i64::from(query.limit))
        .fetch_all(&mut *connection)
        .await
        .map_err(map_err)?;
    Ok(rows.into_iter().map(|row| row.0).collect())
}

#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
#[must_use]
pub fn inbound_pin_sql_for_tests(
    supersession: SupersessionStatus,
    kind: Option<EdgeKind>,
    target_kind: PinTargetKind,
) -> SqlText {
    inbound_pin_sql(supersession, kind, target_kind)
}

/// The page of pin carriers that pin any of `$1` through a column holding
/// `target_kind` targets of `kind`: the columns come from the pin-column
/// table, so a Goal target scans only the Goal columns.
fn inbound_pin_sql(
    supersession: SupersessionStatus,
    kind: Option<EdgeKind>,
    target_kind: PinTargetKind,
) -> SqlText {
    let heads_only = match supersession {
        SupersessionStatus::HeadsOnly => {
            "
                    AND (m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads())
                      OR EXISTS (SELECT 1 FROM proxiima_core.memory_head h
                                 WHERE h.handle=m.handle AND h.t=m.t))"
        }
        SupersessionStatus::IncludeSuperseded => "",
    };
    let mut sql = SqlText::new("SELECT ");
    sql.push_text(&pin_fields::pin_carrier_columns("m."))
        .push(
            "
                   FROM proxiima_core.live_memory m
                  WHERE m.owner_id = ANY($2::uuid[])",
        )
        .push(heads_only)
        .push(
            "
                    AND ",
        )
        .push_text(&pin_fields::inbound_overlap("m.", target_kind, kind))
        .push(
            "
                    AND ($3::uuid IS NULL OR m.t < $3)
                  ORDER BY m.t DESC
                  LIMIT $4",
        );
    sql
}

/// Snapshot-mode edges: pins whose two endpoints are both in the Query window.
pub(super) fn query_edges(
    _req: &QueryRequest,
    memories: &[proxiima_core::verbs::query::MemoryRow],
    visible_goal_ids: &[uuid::Uuid],
) -> Vec<Edge> {
    let visible_goal_ids: Vec<GoalId> = visible_goal_ids.iter().copied().map(GoalId::new).collect();
    let mut nodes: Vec<PinNode> = memories.iter().map(PinNode::from).collect();
    for node in &mut nodes {
        node.resolve_visible_goal_refs(&visible_goal_ids);
    }
    project_window_edges(&nodes, MAX_SNAPSHOT_EDGES)
}

fn owner_ids(read_owners: &[OwnerId]) -> Vec<uuid::Uuid> {
    read_owners
        .iter()
        .copied()
        .map(OwnerId::into_inner)
        .collect()
}
