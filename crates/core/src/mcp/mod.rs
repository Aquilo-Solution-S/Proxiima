//! Build-time MCP tool registration for local development adapters.
//!
//! Composite binaries register tools through spiice crates at startup;
//! there is no runtime registration path.

pub mod behavior;
pub mod core_tools;
pub mod cursor;
pub mod handles;
pub(crate) mod schema;

mod context;
mod error;
mod ids;
mod manifest;
mod reply;
mod tool;
mod types;

#[cfg(test)]
mod tests;

pub use behavior::{
    McpHostToolCall, Next, RequestBehavior, ScopeGateBehavior, TerminalDispatch, ToolCall,
    ToolDescriptorView, ToolSource,
};
// The one enforcement point for the reserved `model_id` operator label.
// Published because a spiice tool that accepts `model_id` must resolve it
// the same way the core authoring tools do — the transport edges only see a
// top-level field, so re-implementing this is how a nested one gets through.
pub use crate::authz::{
    KeyDeclaration, McpToolAudience, OwnerNeed, Replay, ToolEffect, provider_safe_tool_name,
    tool_name_matches,
};
pub use error::ToolErrorKind;
pub use handles::MemoryHandleClass;
pub use ids::parse_wire_uuid;
pub use manifest::{
    CoreActionMeta, McpToolAnnotations, all_core_actions, all_core_resources, canonical_scope_keys,
    canonical_scope_keys_excluding, canonical_served_read_keys, core_action_meta,
    how_to_resource_declaration, how_to_resource_key,
};
pub use reply::{ToolContent, ToolReply};
pub use schema::{
    McpActionSchema, McpDispatcherSchema, mcp_wire_output_schema, normalize_mcp_output_schema,
};
pub use tool::{
    MAX_TITLE_CHARS, McpActionArgSpec, McpArgvActionSpec, McpCallFn, McpToolDescriptor,
    McpToolOrigin, McpUnknownFieldPolicy,
};
pub(crate) use tool::{
    prepare_flat_tool_args, resolve_argv_action, title_problem, validate_action_args,
};
pub(crate) use types::JudgedKey;
pub use types::{
    MAX_OPERATOR_LABEL_CHARS, OperatorLabelConflict, UNKNOWN_OPERATOR_LABEL, resolve_operator_label,
};
