use std::collections::{BTreeMap, HashMap, HashSet};

use proxiima_core::verbs::query::{
    EntityKind, MemoryLineageCursor, MemoryLineageDirection, MemoryLineageEdge, MemoryLineageNode,
    MemoryLineageRequest, MemoryLineageResponse, UnreadTargets,
};
use proxiima_core::verbs::schema::MemorySearchProjection;
use proxiima_core::{
    Edge, EdgeEndpoint, EdgeKind, EntityRef, MemoryId, OwnerId, SchemaId, StorageError,
};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::map_err;
use crate::pg_enums::PgMemoryKind;
use crate::pg_ident::PgIdent;
use crate::projection::{projection_key_ident, snippet_sql};

/// Outbound origin and read pins of the frontier, newest `(src, tgt)` first.
/// A target that is both an origin and a read of one source is its origin.
const ANCESTOR_HOP_SQL: &str = "SELECT DISTINCT ON (m.t, pin)
       m.t AS src, m.kind AS src_kind, pin AS tgt,
       NOT (pin = ANY (m.origins)) AS is_read,
       proxiima_core.memory_created_at(m.t) AS created_at
  FROM proxiima_core.live_memory m
  JOIN unnest(m.origins || m.reads) AS pin ON true
 WHERE m.t = ANY($1::uuid[])
   AND m.owner_id = ANY($2::uuid[])
   AND ($3::uuid IS NULL OR (m.t, pin) < ($3::uuid, $4::uuid))
 ORDER BY m.t DESC, pin DESC
 LIMIT $5";

/// Children whose `origins` or `reads` overlap the frontier; `tgt` is only a
/// parent in that frontier (not the child's other pins).
const DESCENDANT_HOP_SQL: &str = "SELECT DISTINCT ON (child.t, pin)
       child.t AS src, child.kind AS src_kind, pin AS tgt,
       NOT (pin = ANY (child.origins)) AS is_read,
       proxiima_core.memory_created_at(child.t) AS created_at
  FROM proxiima_core.live_memory child
  JOIN unnest(child.origins || child.reads) AS pin ON true
 WHERE child.owner_id = ANY($2::uuid[])
   AND (child.origins && $1::uuid[] OR child.reads && $1::uuid[])
   AND pin = ANY($1::uuid[])
   AND ($3::uuid IS NULL OR (child.t, pin) < ($3::uuid, $4::uuid))
 ORDER BY child.t DESC, pin DESC
 LIMIT $5";

const ANCESTOR_FRONTIER_SQL: &str = "SELECT DISTINCT pin
  FROM proxiima_core.live_memory m
  JOIN unnest(m.origins || m.reads) AS pin ON true
 WHERE m.t = ANY($1::uuid[])
   AND m.owner_id = ANY($2::uuid[])";

const DESCENDANT_FRONTIER_SQL: &str = "SELECT DISTINCT child.t
  FROM proxiima_core.live_memory child
 WHERE child.owner_id = ANY($2::uuid[])
   AND (child.origins && $1::uuid[] OR child.reads && $1::uuid[])";

#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
#[must_use]
pub fn ancestor_hop_sql_for_tests() -> &'static str {
    ANCESTOR_HOP_SQL
}

#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
#[must_use]
pub fn descendant_hop_sql_for_tests() -> &'static str {
    DESCENDANT_HOP_SQL
}

pub(crate) async fn walk_memory_lineage_on_connection(
    connection: &mut PgConnection,
    read_owners: &[OwnerId],
    req: &MemoryLineageRequest,
    projections: &[MemorySearchProjection],
) -> Result<MemoryLineageResponse, StorageError> {
    if read_owners.is_empty() {
        return Ok(MemoryLineageResponse {
            nodes: Vec::new(),
            edges: Vec::new(),
            truncated: false,
            next_cursor: None,
            external_edges: Vec::new(),
        });
    }
    let owner_ids: Vec<Uuid> = read_owners
        .iter()
        .copied()
        .map(OwnerId::into_inner)
        .collect();
    let start = req.start_memory_id.into_inner();
    let depth = req.depth.clamp(1, 8);
    let page_len = usize::try_from(req.limit.min(200)).unwrap_or(usize::MAX);
    let mut hops = walk_lineage_hops_on_connection(
        connection,
        req.direction,
        start,
        &owner_ids,
        depth,
        page_len.saturating_add(1),
        req.after,
    )
    .await?;
    let truncated = hops.len() > page_len;
    hops.truncate(page_len);
    assemble_lineage_page_on_connection(
        connection,
        &owner_ids,
        req.direction,
        start,
        hops,
        truncated,
        projections,
    )
    .await
}

async fn assemble_lineage_page_on_connection(
    connection: &mut PgConnection,
    owner_ids: &[Uuid],
    direction: MemoryLineageDirection,
    start: Uuid,
    hops: Vec<WalkHop>,
    truncated: bool,
    projections: &[MemorySearchProjection],
) -> Result<MemoryLineageResponse, StorageError> {
    let mut node_ids = vec![start];
    node_ids.extend(hops.iter().flat_map(|h| [h.src, h.tgt]));
    let (rows, mut snippets) =
        lineage_node_rows_on_connection(connection, &node_ids, owner_ids, projections).await?;
    if !rows.iter().any(|(id, _, _)| *id == start) {
        return Ok(MemoryLineageResponse {
            nodes: Vec::new(),
            edges: Vec::new(),
            truncated: false,
            next_cursor: None,
            external_edges: Vec::new(),
        });
    }
    let live: HashMap<MemoryId, EntityKind> = rows
        .iter()
        .map(|(id, kind, _)| (MemoryId::new(*id), *kind))
        .collect();
    let nodes = rows
        .into_iter()
        .map(|(id, kind, schema)| MemoryLineageNode {
            memory_id: MemoryId::new(id),
            kind,
            schema_id: SchemaId::new(schema),
            snippet: snippets.remove(&id).unwrap_or_default(),
            distance: node_distance(direction, start, &hops, id),
        })
        .collect();
    let unseen: Vec<Uuid> = hops
        .iter()
        .map(|h| h.tgt)
        .filter(|t| !live.contains_key(&MemoryId::new(*t)))
        .collect();
    let unread = unread_targets_on_connection(&mut *connection, &unseen, owner_ids).await?;
    let edges = hops
        .iter()
        .map(|h| project_lineage_edge(h, &live, &unread))
        .collect();
    // Other-store edges (origin or reference) of the page's visible nodes.
    let visible_ids: Vec<Uuid> = live.keys().map(|id| id.into_inner()).collect();
    let external_edges =
        external_edges_on_connection(&mut *connection, &visible_ids, owner_ids).await?;
    let next_cursor = truncated.then(|| {
        let last = hops.last().expect("truncated page is non-empty");
        MemoryLineageCursor {
            distance: last.dist,
            source: EntityRef::Memory(MemoryId::new(last.src)),
            target: EntityRef::Memory(MemoryId::new(last.tgt)),
        }
    });
    Ok(MemoryLineageResponse {
        nodes,
        edges,
        truncated,
        next_cursor,
        external_edges,
    })
}

/// The readable rows `(t, kind, schema_id)` of `node_ids` and their
/// declared snippets: the node half of a lineage page, shared with the
/// core ancestor walk (`lineage_nodes`, #480).
async fn lineage_node_rows_on_connection(
    connection: &mut PgConnection,
    node_ids: &[Uuid],
    owner_ids: &[Uuid],
    projections: &[MemorySearchProjection],
) -> Result<(Vec<(Uuid, EntityKind, String)>, HashMap<Uuid, String>), StorageError> {
    let rows: Vec<(Uuid, PgMemoryKind, String)> = sqlx::query_as(
        "SELECT m.t, m.kind, m.schema_id FROM proxiima_core.live_memory m
          WHERE m.t = ANY($1::uuid[]) AND m.owner_id = ANY($2::uuid[])",
    )
    .bind(node_ids)
    .bind(owner_ids)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_err)?;
    let rows: Vec<(Uuid, EntityKind, String)> = rows
        .into_iter()
        .map(|(id, kind, schema)| (id, kind.into(), schema))
        .collect();
    let keys: Vec<(Uuid, String)> = rows
        .iter()
        .map(|(id, _, schema)| (*id, schema.clone()))
        .collect();
    let snippets = load_lineage_snippets_on_connection(connection, projections, &keys).await?;
    Ok((rows, snippets))
}

/// Lineage nodes of the readable `memory_ids`, `distance` 0.
pub(crate) async fn lineage_nodes_on_connection(
    connection: &mut PgConnection,
    memory_ids: &[Uuid],
    owner_ids: &[Uuid],
    projections: &[MemorySearchProjection],
) -> Result<Vec<MemoryLineageNode>, StorageError> {
    let (rows, mut snippets) =
        lineage_node_rows_on_connection(connection, memory_ids, owner_ids, projections).await?;
    Ok(rows
        .into_iter()
        .map(|(id, kind, schema)| MemoryLineageNode {
            memory_id: MemoryId::new(id),
            kind,
            schema_id: SchemaId::new(schema),
            snippet: snippets.remove(&id).unwrap_or_default(),
            distance: 0,
        })
        .collect())
}

/// What the transaction's reader may know of the `memory_ids` it cannot
/// read live: the one probe every "forgotten target" and "erased target"
/// answer reads (pins, neighbours, lineage).
///
/// Forgotten rows, as lineage nodes (no snippet; distance 0): the probe
/// widens the caller's OWNER transaction to the lifecycle scope for this one
/// statement, so the owner read policy still applies the reader's owner and
/// kind permits — an id the reader could not read in either state is absent
/// and stays redacted.
///
/// Erased ids: the erase witness of an id that a live memory the reader can
/// read still pins (#671). The witness's owner read policy states the same
/// rule; the owner filter repeats it for the fixture lane without policies.
///
/// Never run it in platform scope: that would disclose forgotten rows and
/// erasures the reader has no permit for.
pub(crate) async fn unread_targets_on_connection(
    connection: &mut PgConnection,
    memory_ids: &[Uuid],
    owner_ids: &[Uuid],
) -> Result<UnreadTargets, StorageError> {
    if memory_ids.is_empty() || owner_ids.is_empty() {
        return Ok(UnreadTargets::default());
    }
    let lifecycle = crate::lifecycle_scope::PgLifecycleScope::enter(&mut *connection).await?;
    let rows: Result<Vec<(Uuid, PgMemoryKind, String)>, StorageError> = sqlx::query_as(
        "SELECT m.t, m.kind, m.schema_id
           FROM proxiima_core.memory_including_forgotten m
          WHERE m.t = ANY($1::uuid[]) AND m.owner_id = ANY($2::uuid[])
            AND m.forgotten_at IS NOT NULL",
    )
    .bind(memory_ids)
    .bind(owner_ids)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_err);
    let rows = lifecycle.finish(&mut *connection, rows).await?;
    let erased: Vec<Uuid> = sqlx::query_scalar(
        "SELECT erased.t
           FROM proxiima_core.erased_pin_target erased
          WHERE erased.t = ANY($1::uuid[])
            AND EXISTS (
                SELECT 1
                  FROM proxiima_core.live_memory pinning
                 WHERE (pinning.origins @> ARRAY[erased.t] OR pinning.refs @> ARRAY[erased.t]
                        OR pinning.reads @> ARRAY[erased.t] OR pinning.forked_from = erased.t)
                   AND pinning.owner_id = ANY($2::uuid[])
            )",
    )
    .bind(memory_ids)
    .bind(owner_ids)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_err)?;
    Ok(UnreadTargets {
        forgotten: rows
            .into_iter()
            .map(|(id, kind, schema)| MemoryLineageNode {
                memory_id: MemoryId::new(id),
                kind: kind.into(),
                schema_id: SchemaId::new(schema),
                snippet: String::new(),
                distance: 0,
            })
            .collect(),
        erased: erased.into_iter().map(MemoryId::new).collect(),
    })
}

/// Other-store edges `(memory, other-store memory, kind)` recorded for
/// `memory_ids`, in `(memory, other-store memory)` order. The one statement
/// both a lineage page and the shared ancestor walk read (#480). Rows are
/// bound by the memory's owner RLS; the owner filter repeats it for the
/// fixture lane without policies.
pub(crate) async fn external_edges_on_connection(
    connection: &mut PgConnection,
    memory_ids: &[Uuid],
    owner_ids: &[Uuid],
) -> Result<Vec<(MemoryId, Uuid, EdgeKind)>, StorageError> {
    let rows: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT e.t, e.memory, e.edge_kind
           FROM proxiima_core.memory_external_origin e
           JOIN proxiima_core.live_memory m ON m.t = e.t
          WHERE e.t = ANY($1::uuid[]) AND m.owner_id = ANY($2::uuid[])
          ORDER BY e.t, e.memory",
    )
    .bind(memory_ids)
    .bind(owner_ids)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_err)?;
    rows.into_iter()
        .map(|(t, memory, kind)| {
            let kind = match kind.as_str() {
                "origin" => EdgeKind::Origin,
                "reference" => EdgeKind::Reference,
                "read" => EdgeKind::Read,
                other => {
                    return Err(StorageError::Internal(format!(
                        "memory_external_origin.edge_kind {other:?} is not origin, reference or read"
                    )));
                }
            };
            Ok((MemoryId::new(t), memory, kind))
        })
        .collect()
}

/// Hop distance of a page node: the start is 0; a node is reached at the
/// distance of a hop whose far end it is (`tgt` toward ancestors, `src`
/// toward descendants) and sits one level before a hop whose near end it is
/// (a resumed page's frontier). Levels expand only unseen nodes, so the
/// smallest of these is the node's level.
fn node_distance(direction: MemoryLineageDirection, start: Uuid, hops: &[WalkHop], id: Uuid) -> u8 {
    if id == start {
        return 0;
    }
    hops.iter()
        .filter_map(|hop| {
            let (far, near) = match direction {
                MemoryLineageDirection::Ancestors => (hop.tgt, hop.src),
                MemoryLineageDirection::Descendants => (hop.src, hop.tgt),
            };
            if far == id {
                Some(hop.dist)
            } else if near == id {
                Some(hop.dist.saturating_sub(1))
            } else {
                None
            }
        })
        .min()
        .unwrap_or(1)
}

fn project_lineage_edge(
    hop: &WalkHop,
    live: &HashMap<MemoryId, EntityKind>,
    unread: &UnreadTargets,
) -> MemoryLineageEdge {
    let target = proxiima_core::project_memory_target(MemoryId::new(hop.tgt), live, unread);
    MemoryLineageEdge {
        edge: Edge {
            source: EdgeEndpoint::memory(hop.src_kind, MemoryId::new(hop.src)),
            target,
            kind: hop.kind,
            created_at: hop.created_at,
        },
        distance: hop.dist,
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct HopRow {
    src: Uuid,
    src_kind: PgMemoryKind,
    tgt: Uuid,
    is_read: bool,
    created_at: time::OffsetDateTime,
}

#[derive(Debug, Clone)]
struct WalkHop {
    src: Uuid,
    src_kind: EntityKind,
    tgt: Uuid,
    kind: EdgeKind,
    dist: u8,
    created_at: time::OffsetDateTime,
}

fn resume_point(after: Option<MemoryLineageCursor>) -> (u8, Option<(Uuid, Uuid)>) {
    let Some(after) = after else {
        return (1, None);
    };
    match (after.source, after.target) {
        (EntityRef::Memory(src), EntityRef::Memory(tgt)) => (
            after.distance.max(1),
            Some((src.into_inner(), tgt.into_inner())),
        ),
        _ => (after.distance.saturating_add(1).max(1), None),
    }
}

async fn walk_lineage_hops_on_connection(
    connection: &mut PgConnection,
    direction: MemoryLineageDirection,
    start: Uuid,
    owner_ids: &[Uuid],
    depth: u8,
    page_limit: usize,
    after: Option<MemoryLineageCursor>,
) -> Result<Vec<WalkHop>, StorageError> {
    let (start_d, mut keyset) = resume_point(after);
    if start_d > depth {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::from([start]);
    let mut frontier = vec![start];
    for _ in 1..start_d {
        if frontier.is_empty() {
            return Ok(Vec::new());
        }
        frontier = take_unseen_frontier(
            next_frontier_on_connection(connection, direction, &frontier, owner_ids).await?,
            &mut seen,
        );
    }
    let mut hops = Vec::new();
    let mut dist = start_d;
    while dist <= depth && hops.len() < page_limit && !frontier.is_empty() {
        let remaining = i64::try_from(page_limit - hops.len()).unwrap_or(i64::MAX);
        let rows = hop_edges_on_connection(
            connection,
            direction,
            &frontier,
            owner_ids,
            keyset.take(),
            remaining,
        )
        .await?;
        hops.extend(rows.into_iter().map(|r| WalkHop {
            src: r.src,
            src_kind: r.src_kind.into(),
            tgt: r.tgt,
            kind: if r.is_read {
                EdgeKind::Read
            } else {
                EdgeKind::Origin
            },
            dist,
            created_at: r.created_at,
        }));
        if hops.len() >= page_limit {
            break;
        }
        frontier = take_unseen_frontier(
            next_frontier_on_connection(connection, direction, &frontier, owner_ids).await?,
            &mut seen,
        );
        dist = dist.saturating_add(1);
    }
    Ok(hops)
}

fn take_unseen_frontier(next: Vec<Uuid>, seen: &mut HashSet<Uuid>) -> Vec<Uuid> {
    next.into_iter().filter(|id| seen.insert(*id)).collect()
}

async fn hop_edges_on_connection(
    connection: &mut PgConnection,
    direction: MemoryLineageDirection,
    frontier: &[Uuid],
    owner_ids: &[Uuid],
    after: Option<(Uuid, Uuid)>,
    limit: i64,
) -> Result<Vec<HopRow>, StorageError> {
    if frontier.is_empty() || limit <= 0 {
        return Ok(Vec::new());
    }
    let (a, b) = after.map_or((None, None), |(a, b)| (Some(a), Some(b)));
    let query = match direction {
        MemoryLineageDirection::Ancestors => sqlx::query_as(ANCESTOR_HOP_SQL),
        MemoryLineageDirection::Descendants => sqlx::query_as(DESCENDANT_HOP_SQL),
    };
    query
        .bind(frontier)
        .bind(owner_ids)
        .bind(a)
        .bind(b)
        .bind(limit)
        .fetch_all(&mut *connection)
        .await
        .map_err(map_err)
}

async fn next_frontier_on_connection(
    connection: &mut PgConnection,
    direction: MemoryLineageDirection,
    frontier: &[Uuid],
    owner_ids: &[Uuid],
) -> Result<Vec<Uuid>, StorageError> {
    if frontier.is_empty() {
        return Ok(Vec::new());
    }
    let query = match direction {
        MemoryLineageDirection::Ancestors => sqlx::query_scalar(ANCESTOR_FRONTIER_SQL),
        MemoryLineageDirection::Descendants => sqlx::query_scalar(DESCENDANT_FRONTIER_SQL),
    };
    query
        .bind(frontier)
        .bind(owner_ids)
        .fetch_all(&mut *connection)
        .await
        .map_err(map_err)
}

#[derive(Debug, sqlx::FromRow)]
struct SnippetRow {
    t: Uuid,
    snippet: Option<String>,
}

async fn load_lineage_snippets_on_connection(
    connection: &mut PgConnection,
    projections: &[MemorySearchProjection],
    rows: &[(Uuid, String)],
) -> Result<HashMap<Uuid, String>, StorageError> {
    let mut by_schema = BTreeMap::<&str, Vec<Uuid>>::new();
    for (t, schema) in rows {
        by_schema.entry(schema.as_str()).or_default().push(*t);
    }
    let mut result = HashMap::new();
    for (schema, ts) in by_schema {
        if let Some(p) = projections.iter().find(|p| p.schema_id.as_str() == schema) {
            result.extend(load_one_schema_snippets_on_connection(connection, p, ts).await?);
        }
    }
    Ok(result)
}

/// One primary-key lookup over one sidecar, rendering the declared snippet
/// expression for the ids given.
///
/// Shared with `search`, which hydrates the snippets of the rows that made its
/// page the same way. A second copy of "group by schema, look the projection
/// up, `c.t = ANY($1)`" would be a second place for the snippet expression to
/// drift.
pub(super) async fn load_one_schema_snippets_on_connection(
    connection: &mut PgConnection,
    projection: &MemorySearchProjection,
    ts: Vec<Uuid>,
) -> Result<Vec<(Uuid, String)>, StorageError> {
    if ts.is_empty() {
        return Ok(Vec::new());
    }
    let table = PgIdent::table(&projection.sidecar_table)?;
    let key = projection_key_ident(projection)?;
    let snippet = snippet_sql(projection)?;
    let sql = format!(
        "SELECT c.{key} AS t, {snippet} AS snippet FROM {table} c WHERE c.{key} = ANY($1::uuid[])",
        table = table.as_str(),
        key = key.as_str(),
        snippet = snippet
    );
    // SQL-POLICY: PgIdent
    let rows: Vec<SnippetRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&ts)
        .fetch_all(&mut *connection)
        .await
        .map_err(map_err)?;
    Ok(rows
        .into_iter()
        .map(|row| (row.t, row.snippet.unwrap_or_default()))
        .collect())
}

/// A lineage edge always sources at a memory row, so the endpoint decode
/// below can never see a Goal address; this is the assertion that says so.
#[cfg(test)]
mod tests {

    use super::{ANCESTOR_HOP_SQL, DESCENDANT_HOP_SQL};

    #[test]
    fn each_lineage_hop_pages_in_sql() {
        assert!(
            ANCESTOR_HOP_SQL.contains("LIMIT") && DESCENDANT_HOP_SQL.contains("LIMIT"),
            "each hop query must page in SQL"
        );
    }
}
