use crate::ToolCtx;
use crate::catalog::core_tools::wire_ref;
use schemars::JsonSchema;
use serde::Serialize;

/// One edge touching the memory being read: the same four fields every
/// other edge-bearing output carries, minus `created_at`, which a
/// neighbor listing has no use for.
#[derive(Debug, Serialize, serde::Deserialize, JsonSchema)]
pub struct NeighborEdge {
    pub source: String,
    pub target: String,
    pub kind: crate::EdgeKind,
}

pub(crate) fn neighbor_edges_from_rows(ctx: &ToolCtx, rows: Vec<crate::Edge>) -> Vec<NeighborEdge> {
    rows.into_iter()
        .map(|edge| NeighborEdge {
            source: wire_ref::format_endpoint(ctx, edge.source),
            target: wire_ref::format_target_projection(ctx, edge.target),
            kind: edge.kind,
        })
        .collect()
}
