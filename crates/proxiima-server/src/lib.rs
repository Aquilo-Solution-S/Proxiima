//! Proxiima's listener: the tool host and its two transports.
//!
//! [`ToolHost`] runs a tool call or a resource read for an authenticated
//! caller. `mcp` serves it over MCP (Streamable HTTP), `rest` over `/v1`;
//! authentication, the Host and Origin guards and OAuth discovery are
//! shared by both.

mod auth;
mod error;
mod host_tools;
mod mcp;
pub mod oauth;
mod request_scope;
#[cfg(feature = "rest")]
pub mod rest;
pub mod security;
pub mod selfdoc;
mod server;

pub use auth::{AuthContext, EdgeAuth};
pub use error::ServerError;
pub use host_tools::{HostTool, HostTools, MAX_HOST_INSTRUCTIONS_CHARS};
pub use mcp::handler::{
    DynamicHandler, auth_context, author_from_args, mcp_tool_error_to_error_data,
    peer_implementation, strip_call_context_args, tool_invocation_error_to_error_data,
};
pub use mcp::session::{owner_key, parse_owner_key};
pub use mcp::tool_list::ToolListNotifier;
pub use mcp::transport::{
    BodyLimitLayer, DEFAULT_PROGRESS_HEARTBEAT, MAX_REQUEST_BODY_BYTES, McpStreamableService,
    McpTransportConfig, body_limit_layer, enforce_body_limit, guard_listener,
    streamable_http_service, streamable_http_service_with_transport,
};
pub use oauth::{
    AUTHORIZATION_SERVER_METADATA_PATH, AuthorizationServers,
    MCP_AUTHORIZATION_SERVER_METADATA_PATH, MCP_PATH, MCP_PROTECTED_RESOURCE_METADATA_PATH,
    PROTECTED_RESOURCE_METADATA_PATH, ProtectedResource, PublicClientFacade,
    PublicClientFacadeError, REGISTRATION_PATH, ResourceServerMetadata, SCOPES_SUPPORTED,
    ScopeToken, ScopeTokenError, public_discovery_router,
};
pub use request_scope::RequestHeaderAllowlist;
pub use security::{
    AuthLayer, CorsLayer, HostAllowlist, HostGuardLayer, OriginAllowlist, assert_loopback,
    auth_layer_with_config, auth_layer_with_metadata, cors_layer, default_allowlist,
    host_guard_layer,
};
pub use server::{MAX_HOST_TOOL_NAME_CHARS, ToolHost, ToolInvocationError, reject_nul_in_args};
