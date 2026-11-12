//! The tool catalog: the contract a tool implements ([`Tool`], [`ToolCtx`],
//! [`ToolError`]), the descriptors the registry serves, the request
//! behaviors a call passes through, and core's own tools.
//!
//! Tools are registered at build time through spiice crates; there is no
//! runtime registration path.

pub mod behavior;
pub mod contract;
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

pub use contract::*;

pub use behavior::{
    HostToolCall, Next, RequestBehavior, ScopeGateBehavior, TerminalDispatch, ToolCall,
    ToolDescriptorView, ToolSource,
};
// The one enforcement point for the reserved `model_id` operator label.
// Published because a spiice tool that accepts `model_id` must resolve it
// the same way the core authoring tools do — the transport edges only see a
// top-level field, so re-implementing this is how a nested one gets through.
pub use crate::authz::{
    KeyDeclaration, OwnerNeed, Replay, ToolAudience, ToolEffect, provider_safe_tool_name,
    tool_name_matches,
};
pub use error::ToolErrorKind;
pub use handles::MemoryHandleClass;
pub use ids::parse_wire_uuid;
pub use manifest::{
    CoreActionMeta, ToolAnnotations, all_core_actions, all_core_resources, canonical_scope_keys,
    canonical_scope_keys_excluding, canonical_served_read_keys, core_action_meta,
    how_to_resource_declaration, how_to_resource_key,
};
pub use reply::{ToolContent, ToolReply};
pub use schema::{
    ActionSchema, DispatcherSchema, mcp_wire_output_schema, normalize_mcp_output_schema,
};
pub use tool::{
    ActionArgSpec, ArgvActionSpec, CallFn, MAX_TITLE_CHARS, ToolDescriptor, ToolOrigin,
    UnknownFieldPolicy,
};
pub(crate) use tool::{
    prepare_flat_tool_args, resolve_argv_action, title_problem, validate_action_args,
};
pub(crate) use types::JudgedKey;
pub use types::{
    MAX_OPERATOR_LABEL_CHARS, OperatorLabelConflict, UNKNOWN_OPERATOR_LABEL, resolve_operator_label,
};
