//! Source-owned pins incident on one typed endpoint.

use proxiima_core::verbs::query::SurroundingPin;
use proxiima_core::{
    EntityRef, GoalId, MemoryId, MemoryPinField, OwnerId, PinShape, PinTargetKind, SqlText,
    StorageError,
};
use sqlx::PgConnection;

use crate::error::map_err;
use crate::pin_fields;

pub(crate) async fn surrounding_pins_on_connection(
    connection: &mut PgConnection,
    read_owners: &[OwnerId],
    endpoint: EntityRef,
    offset: u32,
    limit: u32,
) -> Result<Vec<SurroundingPin>, StorageError> {
    if read_owners.is_empty() {
        return Ok(Vec::new());
    }
    let (id, endpoint_kind) = match endpoint {
        EntityRef::Memory(id) => (id.into_inner(), PinTargetKind::Memory),
        EntityRef::Goal(id) => (id.into_inner(), PinTargetKind::Goal),
    };
    let owners: Vec<uuid::Uuid> = read_owners.iter().map(|id| id.into_inner()).collect();
    let rows: Vec<(uuid::Uuid, uuid::Uuid, bool, bool, String)> =
        sqlx::query_as(surrounding_pins_sql(endpoint_kind))
            .bind(id)
            .bind(owners)
            .bind(i64::from(offset))
            .bind(i64::from(limit))
            .fetch_all(&mut *connection)
            .await
            .map_err(map_err)?;
    Ok(rows
        .into_iter()
        .map(
            |(source, target, source_goal, target_goal, field)| SurroundingPin {
                source: endpoint_of(source, source_goal),
                target: endpoint_of(target, target_goal),
                field,
            },
        )
        .collect())
}

fn endpoint_of(id: uuid::Uuid, goal: bool) -> EntityRef {
    if goal {
        EntityRef::Goal(GoalId::new(id))
    } else {
        EntityRef::Memory(MemoryId::new(id))
    }
}

fn surrounding_pins_sql(endpoint_kind: PinTargetKind) -> SqlText {
    let mut branches = Vec::new();
    for field in MemoryPinField::ALL {
        let outgoing = endpoint_kind == PinTargetKind::Memory;
        let incoming = endpoint_kind == field.target();
        if !outgoing && !incoming {
            continue;
        }
        let column = field.spec().column;
        let mut branch = SqlText::new("SELECT m.t AS source, p.target, false AS source_goal, ");
        branch
            .push(if field.target() == PinTargetKind::Goal {
                "true"
            } else {
                "false"
            })
            .push(" AS target_goal, '")
            .push(column)
            .push("'::text AS field FROM proxiima_core.live_memory m CROSS JOIN LATERAL unnest(")
            .push_text(&pin_fields::pin_ids("m.", field))
            .push(") AS p(target) WHERE m.owner_id = ANY($2::uuid[]) AND (");
        append_candidates(
            &mut branch,
            "m.",
            column,
            field.spec().shape,
            outgoing,
            incoming,
        );
        branch.push(") AND (");
        append_incidence(&mut branch, PinTargetKind::Memory, outgoing, incoming);
        branch.push(")");
        branches.push(branch);
    }

    if endpoint_kind == PinTargetKind::Memory {
        let mut branch = SqlText::new(
            "SELECT m.t AS source, p.memory AS target, false AS source_goal, false AS target_goal, \
             p.edge_kind AS field FROM proxiima_core.live_memory m \
             JOIN proxiima_core.memory_external_origin p ON p.t = m.t \
             WHERE m.owner_id = ANY($2::uuid[]) AND (",
        );
        // The external-origin table's target column is named `memory`.
        branch.push("m.t = $1 OR (p.memory = $1 AND ");
        append_heads(&mut branch, PinTargetKind::Memory);
        branch.push("))");
        branches.push(branch);
    }

    for (column, target_kind) in [
        ("dependency_t", PinTargetKind::Goal),
        ("evidence_t", PinTargetKind::Memory),
        ("motivated_by", PinTargetKind::Memory),
    ] {
        let outgoing = endpoint_kind == PinTargetKind::Goal;
        let incoming = endpoint_kind == target_kind;
        if !outgoing && !incoming {
            continue;
        }
        let mut branch = SqlText::new("SELECT g.t AS source, p.target, true AS source_goal, ");
        branch
            .push(if target_kind == PinTargetKind::Goal {
                "true"
            } else {
                "false"
            })
            .push(" AS target_goal, '")
            .push(column)
            .push("'::text AS field FROM proxiima_core.goal g CROSS JOIN LATERAL unnest(g.")
            .push(column)
            .push(") AS p(target) WHERE g.owner_id = ANY($2::uuid[]) AND (");
        append_candidates(
            &mut branch,
            "g.",
            column,
            PinShape::Array,
            outgoing,
            incoming,
        );
        branch.push(") AND (");
        append_incidence(&mut branch, PinTargetKind::Goal, outgoing, incoming);
        branch.push(")");
        branches.push(branch);
    }
    let mut sql = SqlText::new("SELECT source, target, source_goal, target_goal, field FROM (");
    for (index, branch) in branches.iter().enumerate() {
        if index > 0 {
            sql.push(" UNION ALL ");
        }
        sql.push_text(branch);
    }
    sql.push(") AS pins ORDER BY source, target, field OFFSET $3::bigint LIMIT $4::bigint");
    sql
}

fn append_candidates(
    sql: &mut SqlText,
    row: &'static str,
    column: &'static str,
    shape: PinShape,
    outgoing: bool,
    incoming: bool,
) {
    if outgoing {
        sql.push(row).push("t = $1");
    }
    if incoming {
        if outgoing {
            sql.push(" OR ");
        }
        sql.push(row).push(column).push(match shape {
            PinShape::Array => " @> ARRAY[$1::uuid]",
            PinShape::Scalar => " = $1",
        });
    }
}

fn append_incidence(sql: &mut SqlText, source: PinTargetKind, outgoing: bool, incoming: bool) {
    if outgoing {
        sql.push(match source {
            PinTargetKind::Memory => "m.t = $1",
            PinTargetKind::Goal => "g.t = $1",
        });
    }
    if incoming {
        if outgoing {
            sql.push(" OR ");
        }
        sql.push("(p.target = $1 AND ");
        append_heads(sql, source);
        sql.push(")");
    }
}

fn append_heads(sql: &mut SqlText, source: PinTargetKind) {
    match source {
        PinTargetKind::Memory => {
            sql.push(
                "EXISTS (SELECT 1 FROM proxiima_core.memory_head h \
                 WHERE h.handle = m.handle AND h.t = m.t)",
            );
        }
        PinTargetKind::Goal => {
            sql.push(
                "EXISTS (SELECT 1 FROM proxiima_core.goal_head h \
                 WHERE h.handle = g.handle AND h.t = g.t)",
            );
        }
    }
}
