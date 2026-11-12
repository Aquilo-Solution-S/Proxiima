//! rmcp dynamic tool projection.
//!
//! The SDK exposes dynamic tools through direct
//! `ServerHandler::list_tools` / `call_tool` overrides. This adapter
//! projects the frozen build-time `SpiiceRegistry` tool descriptors
//! into MCP tool metadata at request time.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use proxiima_core::catalog::{
    ToolAnnotations, ToolContent, ToolDescriptor, ToolDescriptorView, ToolEffect, ToolErrorKind,
    ToolReply, all_core_resources, mcp_wire_output_schema, normalize_mcp_output_schema,
    provider_safe_tool_name, tool_name_matches,
};
use proxiima_core::{
    MemoryId, SpiiceRegistryFrozen, UNKNOWN_OPERATOR_LABEL, resolve_operator_label,
};
use proxiima_core::{ToolCaller, ToolCtx, ToolError};
use rmcp::ServerHandler;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    DiscoverResult, ErrorData, Implementation, InitializeRequestParams, InitializeResult,
    JsonObject, ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, MetaObject,
    PaginatedRequestParams, ProgressNotificationParam, ProgressToken, ProtocolVersion,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, ServerCapabilities, ServerConfig, SubscriptionFilter, Tool,
};
use rmcp::service::{MaybeSendFuture, Peer, RequestContext, RoleServer, SubscriptionContext};

use crate::selfdoc;

/// Product name reported to MCP clients on `initialize`.
const SERVER_NAME: &str = "proxiima";

/// Newest MCP revision Proxiima implements. rmcp's default admits every
/// revision rmcp knows, so an rmcp upgrade that learns a newer one would
/// start serving it before Proxiima meets it: `2026-07-28` was admitted that
/// way while the SEP-2549 list cache hints it requires were missing, and a
/// client on it rejected `tools/list`. Raise this only with the revision's
/// server obligations met; a test fails when rmcp knows a newer one.
const MAX_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2026_07_28;

/// SEP-2549 freshness of every list result, in milliseconds. Each list is
/// projected from the caller's token scope, so a grant or revocation must
/// show on the next list rather than after a cache expires.
const LIST_TTL_MS: u64 = 0;

/// SEP-2549 cache scope of every list result: per-caller, for the same
/// reason as [`LIST_TTL_MS`], so no shared cache may serve it to another
/// token.
const LIST_CACHE_SCOPE: CacheScope = CacheScope::Private;

use crate::auth::AuthContext;
use crate::host_tools::HostTool;
use crate::server::{ToolHost, ToolInvocationError};
use proxiima_core::{CapabilitySet, KeyDeclaration, OwnerAccess, ResourceKey, ScopeKey, ToolName};

#[derive(Clone, Debug)]
pub struct DynamicHandler {
    pub server: ToolHost,
    /// Interval of the `notifications/progress` heartbeat a running tool
    /// call sends when its request carries a progress token.
    pub progress_heartbeat: Duration,
}

impl DynamicHandler {
    /// A handler over `server` with [`crate::DEFAULT_PROGRESS_HEARTBEAT`].
    #[must_use]
    pub const fn new(server: ToolHost) -> Self {
        Self {
            server,
            progress_heartbeat: crate::DEFAULT_PROGRESS_HEARTBEAT,
        }
    }

    /// Set the progress heartbeat interval; keep it under the session idle
    /// timeout ([`crate::McpTransportConfig::progress_heartbeat`]).
    #[must_use]
    pub const fn with_progress_heartbeat(mut self, interval: Duration) -> Self {
        self.progress_heartbeat = interval;
        self
    }

    /// The ordinary tool call, answering also the key the scope gate judged
    /// it by ([`ToolCtx::judged`](proxiima_core::ToolCtx::judged)), after
    /// success or refusal. `None` when the request failed before its context
    /// existed or the gate derived no key.
    ///
    /// # Errors
    ///
    /// The result holds the same errors as [`ServerHandler::call_tool`].
    pub fn call_tool_observed(
        &self,
        request: CallToolRequestParams,
        context: &RequestContext<RoleServer>,
    ) -> impl Future<Output = (Option<ScopeKey>, Result<CallToolResponse, ErrorData>)>
    + MaybeSendFuture
    + use<> {
        let server = self.server.clone();
        let auth = auth_context(context);
        let (client_name, client_version) = peer_implementation(context);
        let request_services = request_services(&self.server, context);
        let heartbeat = context
            .meta
            .get_progress_token()
            .map(|token| (token, context.peer.clone(), self.progress_heartbeat));
        Box::pin(async move {
            let request_services = match request_services {
                Ok(services) => services,
                Err(err) => return (None, Err(err)),
            };
            let request_name = request.name.to_string();
            let mut args = request
                .arguments
                .map_or_else(|| serde_json::json!({}), serde_json::Value::Object);
            let caller = match author_from_args(&args, auth.as_ref(), &client_name, &client_version)
            {
                Ok(caller) => caller,
                Err(err) => return (None, Err(err)),
            };
            strip_call_context_args(&mut args);
            let error_auth = auth.clone();
            let call = async {
                let Some(auth) = auth else {
                    let name = canonical_tool_name(&server, &request_name)
                        .unwrap_or_else(|| request_name.clone());
                    return (None, Err(ToolInvocationError::NotAuthorized(name)));
                };
                let target = match server.resolve_tool(&auth, &request_name).await {
                    Ok(target) => target,
                    Err(err) => return (None, Err(err)),
                };
                server
                    .call_resolved(target, args, caller, auth, request_services)
                    .await
            };
            let (judged, reply) = with_progress_heartbeat(Box::pin(call), heartbeat).await;
            let outcome = reply
                .map_err(|err| {
                    tool_invocation_error_to_error_data(server.registry(), err, error_auth.as_ref())
                })
                .and_then(|reply| wire_reply(&request_name, reply))
                .map(Into::into);
            (judged, outcome)
        })
    }

    /// [`ServerHandler::get_info`] plus `instructions` generated from the
    /// caller's admitted surface (exact token keys, deployment surface, and
    /// request behaviors' `visible`), the surface `list_tools`
    /// advertises, followed by the host's own contribution
    /// ([`HostTools::instructions`](crate::HostTools::instructions)). A
    /// `memory`-profile deployment thus omits guidance for tools it does not
    /// expose.
    fn info_for<'a>(
        &'a self,
        context: &RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ServerConfig, ErrorData>> + MaybeSendFuture + use<'a> {
        let auth = auth_context(context);
        let listing = listing_ctx(&self.server, auth.as_ref(), context);
        async move {
            let generated = {
                let ctx = listing?;
                let caller = Caller::listing(auth.as_ref(), ctx.as_ref());
                selfdoc::build_instructions(
                    &advertised_tool_ids(self.server.registry(), caller),
                    &advertised_resource_scope_keys(auth.as_ref()),
                )
            };
            let host = match &auth {
                Some(auth) => self.server.host_instructions(auth).await,
                None => None,
            };
            let mut info = self.get_info();
            info.instructions = selfdoc::compose_instructions(&generated, host.as_deref());
            Ok(info)
        }
    }
}

/// Canonical ids of the tools a listing shows `caller`. The filter
/// `list_tools` applies, so self-documentation never references a tool the
/// caller cannot see.
pub(crate) fn advertised_tool_ids(
    registry: &SpiiceRegistryFrozen,
    caller: Caller<'_>,
) -> BTreeSet<&'static str> {
    registry
        .list_mcp_tools()
        .iter()
        .filter(|descriptor| tool_allowed_for_auth(caller, descriptor))
        .map(|descriptor| descriptor.name)
        .collect()
}

/// The tool context request behaviors judge one listing with: the caller's
/// own, built as [`ToolHost::read_resource_in_request`] builds its. `None`
/// without an authenticated caller, which a release build lists nothing to.
fn listing_ctx(
    server: &ToolHost,
    auth: Option<&AuthContext>,
    context: &RequestContext<RoleServer>,
) -> Result<Option<ToolCtx>, ErrorData> {
    let Some(auth) = auth else {
        return Ok(None);
    };
    let (client_name, client_version) = peer_implementation(context);
    let author = author_from_ctx(Some(auth), &client_name, &client_version);
    let request = request_services(server, context)?;
    server
        .ctx_for_request(author, auth, request)
        .map(Some)
        .map_err(|err| mcp_tool_error_to_error_data(&err))
}

impl ServerHandler for DynamicHandler {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default();
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        if self.server.tool_list_notifier().is_some()
            && let Some(tools) = info.capabilities.tools.as_mut()
        {
            tools.list_changed = Some(true);
        }
        // NOT `Implementation::from_build_env()`: those `env!` macros expand
        // against rmcp's own manifest, so every Proxiima deployment introduced
        // itself as `rmcp 2.2.0` and no client or operator could tell which
        // release they were talking to.
        info.server_info = Implementation::new(SERVER_NAME, proxiima_core::RELEASE_VERSION);
        info
    }

    /// Every revision rmcp knows up to `2026-07-28` (`MAX_PROTOCOL_VERSION`).
    /// This bounds what `initialize` may agree to, what a per-request
    /// version may name, and what `server/discover` advertises. A host
    /// wrapping this handler in its own `ServerHandler` delegates here (and
    /// to `discover`, `accepted_subscription_filter` and `listen`), or rmcp's
    /// defaults apply instead.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(&MAX_PROTOCOL_VERSION))
    }

    /// The `initialize` handshake (revisions up to `2025-11-25`), answered
    /// with per-caller instructions (`info_for`). Mirrors the SDK
    /// default's `set_peer_info` bookkeeping, and registers the session
    /// with the tool-list notifier when one is attached.
    fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<InitializeResult, ErrorData>> + MaybeSendFuture + '_ {
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        if let (Some(notifier), Some(auth)) =
            (self.server.tool_list_notifier(), auth_context(&context))
        {
            notifier.register_session(auth.owner, context.peer.clone());
        }
        self.info_for(&context)
    }

    /// `server/discover`, which replaces the handshake from `2026-07-28`:
    /// the same per-caller instructions `initialize` returns, never cached
    /// across callers (rmcp sets `ttlMs: 0`, `cacheScope: private`).
    fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<DiscoverResult, ErrorData>> + MaybeSendFuture + '_ {
        let info = self.info_for(&context);
        async move {
            Ok(DiscoverResult::from_server_info(
                self.supported_protocol_versions().into_owned(),
                info.await?,
            ))
        }
    }

    /// `subscriptions/listen` (`2026-07-28`) carries `toolsListChanged` and
    /// nothing else, and only with a tool-list notifier attached.
    fn accepted_subscription_filter(
        &self,
        _requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        self.server
            .tool_list_notifier()
            .map(|_| SubscriptionFilter::builder().tools_list_changed().build())
    }

    /// Holds one accepted subscription under the caller's owner until the
    /// client ends it.
    fn listen(
        &self,
        context: SubscriptionContext,
    ) -> impl Future<Output = Result<(), ErrorData>> + MaybeSendFuture + '_ {
        let registration = match (
            self.server.tool_list_notifier(),
            auth_context(context.request_context()),
        ) {
            (Some(notifier), Some(auth)) if context.accepted().tools_list_changed == Some(true) => {
                Some(notifier.register_subscription(auth.owner, context.sink().clone()))
            }
            _ => None,
        };
        async move {
            context.cancelled().await;
            drop(registration);
            Ok(())
        }
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, ErrorData>> + MaybeSendFuture + '_ {
        let auth = auth_context(&context);
        let resource = Resource::new(selfdoc::HOW_TO_URI, selfdoc::HOW_TO_NAME)
            .with_title(selfdoc::HOW_TO_TITLE)
            .with_description(selfdoc::HOW_TO_DESCRIPTION)
            .with_mime_type(selfdoc::HOW_TO_MIME);
        let mut resources = Vec::new();
        if how_to_allowed_for_auth(auth.as_ref()) {
            resources.push(resource);
        }
        resources.extend(
            all_core_resources()
                .filter(|resource| {
                    !resource.is_template && resource_allowed_for_auth(auth.as_ref(), resource)
                })
                .map(raw_resource_from_meta),
        );
        std::future::ready(Ok(ListResourcesResult::with_all_items(resources)
            .with_ttl_ms(LIST_TTL_MS)
            .with_cache_scope(LIST_CACHE_SCOPE)))
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, ErrorData>> + MaybeSendFuture + '_
    {
        let auth = auth_context(&context);
        let resource_templates = all_core_resources()
            .filter(|resource| {
                resource.is_template && resource_allowed_for_auth(auth.as_ref(), resource)
            })
            .map(raw_resource_template_from_meta)
            .collect();
        std::future::ready(Ok(ListResourceTemplatesResult::with_all_items(
            resource_templates,
        )
        .with_ttl_ms(LIST_TTL_MS)
        .with_cache_scope(LIST_CACHE_SCOPE)))
    }

    /// Answers are always [`ReadResourceResponse::Complete`]. rmcp 3 widened
    /// this return type for MRTR (SEP-2322), whose other variant lets a server
    /// pause mid-request to ask the *client* for input. Proxiima resolves every
    /// resource from its own store under the caller's already-established
    /// scope, so there is nothing to ask back for — see [`call_tool`] for the
    /// same reasoning on the tool path.
    ///
    /// [`call_tool`]: DynamicHandler::call_tool
    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResponse, ErrorData>> + MaybeSendFuture + '_ {
        let uri = request.uri;
        let auth = auth_context(&context);
        let (client_name, client_version) = peer_implementation(&context);
        let request_services = request_services(&self.server, &context);
        let how_to_ctx = if uri == selfdoc::HOW_TO_URI {
            listing_ctx(&self.server, auth.as_ref(), &context)
        } else {
            Ok(None)
        };
        let server = self.server.clone();
        async move {
            if uri == selfdoc::HOW_TO_URI {
                if !how_to_allowed_for_auth(auth.as_ref()) {
                    return Err(mcp_tool_error_to_error_data(&ToolError::NotAuthorized(
                        proxiima_core::protocol::resource::HOW_TO.into(),
                    )));
                }
                let ctx = how_to_ctx?;
                let caller = Caller::listing(auth.as_ref(), ctx.as_ref());
                let advertised = advertised_tool_ids(server.registry(), caller);
                let advertised_resources = advertised_resource_scope_keys(auth.as_ref());
                let body = selfdoc::how_to_markdown(&advertised, &advertised_resources);
                return Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(body, selfdoc::HOW_TO_URI)
                        .with_mime_type(selfdoc::HOW_TO_MIME),
                ])
                .into());
            }
            if !uri.starts_with("proxiima://") {
                return Err(ErrorData::resource_not_found(
                    format!("unknown resource {uri}"),
                    None,
                ));
            }
            let author = author_from_ctx(auth.as_ref(), &client_name, &client_version);
            let request = request_services?;
            let value = server
                .read_resource_in_request(&uri, author, auth, request)
                .await
                .map_err(resource_invocation_error_to_error_data)?;
            let text = serde_json::to_string(&value).map_err(generic_internal_error)?;
            Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, uri).with_mime_type("application/json"),
            ])
            .into())
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        let auth = auth_context(&context);
        let listing = listing_ctx(&self.server, auth.as_ref(), &context);
        async move {
            let ctx = listing?;
            let caller = Caller::listing(auth.as_ref(), ctx.as_ref());
            // `core_search_memories` names the caller's readable stores when
            // an other-stores port is wired; the port is asked once here
            // because the projection below is synchronous.
            let search_schema = match ctx.as_ref() {
                Some(ctx) => Some(
                    caller_search_input_schema(
                        ctx,
                        self.server
                            .registry()
                            .list_mcp_tools()
                            .iter()
                            .find(|d| d.name == proxiima_core::protocol::tool::CORE_SEARCH_MEMORIES)
                            .map(|d| project_dispatcher_actions_for_auth(caller, d))
                            .unwrap_or_default(),
                    )
                    .await?,
                ),
                None => None,
            };
            let mut tools: Vec<Tool> = self
                .server
                .registry()
                .list_mcp_tools()
                .iter()
                .filter(|descriptor| tool_allowed_for_auth(caller, descriptor))
                .map(|descriptor| {
                    let schema = match &search_schema {
                        Some(schema)
                            if descriptor.name
                                == proxiima_core::protocol::tool::CORE_SEARCH_MEMORIES =>
                        {
                            schema.clone()
                        }
                        _ => project_dispatcher_actions_for_auth(caller, descriptor),
                    };
                    let tool = Tool::new(
                        Cow::Owned(provider_safe_tool_name(descriptor.name)),
                        Cow::Borrowed(descriptor.description),
                        Arc::new(rmcp::model::object(schema)),
                    )
                    .with_raw_output_schema(Arc::new(rmcp::model::object(mcp_wire_output_schema(
                        &descriptor.output_schema,
                    ))));
                    match annotations_for_auth(caller, descriptor) {
                        Some(annotations) => tool.annotate(to_rmcp_annotations(annotations)),
                        None => tool,
                    }
                })
                .collect();
            if let Some(auth) = &auth {
                let host_tools = self
                    .server
                    .host_tools_for(auth)
                    .await
                    .map_err(|err| mcp_tool_error_to_error_data(&err))?;
                tools.extend(
                    host_tools
                        .into_iter()
                        .filter(|tool| host_tool_allowed_for_auth(caller, tool))
                        .filter_map(host_tool_metadata),
                );
            }
            Ok(ListToolsResult::with_all_items(tools)
                .with_ttl_ms(LIST_TTL_MS)
                .with_cache_scope(LIST_CACHE_SCOPE))
        }
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.server
            .registry()
            .list_mcp_tools()
            .iter()
            .find(|descriptor| tool_name_matches(descriptor.name, name))
            .map(|descriptor| {
                let tool = Tool::new(
                    Cow::Owned(provider_safe_tool_name(descriptor.name)),
                    Cow::Borrowed(descriptor.description),
                    Arc::new(rmcp::model::object(descriptor.args_schema.clone())),
                )
                .with_raw_output_schema(Arc::new(rmcp::model::object(
                    mcp_wire_output_schema(&descriptor.output_schema),
                )));
                match annotations_for_auth(Caller::new(None), descriptor) {
                    Some(annotations) => tool.annotate(to_rmcp_annotations(annotations)),
                    None => tool,
                }
            })
    }

    /// Answers are always [`CallToolResponse::Complete`]. rmcp 3 widened this
    /// return type for two draft extensions Proxiima deliberately does not
    /// implement:
    ///
    /// - `InputRequired` (MRTR, SEP-2322) pauses a call to elicit input from
    ///   the client. Every Proxiima tool is a single authorized store
    ///   operation whose arguments arrive complete; a missing or malformed one
    ///   is a caller error, reported as such by
    ///   `tool_invocation_error_to_error_data`, not a prompt to fill in.
    /// - `Task` (SEP-2663) hands back a task id for the client to poll. That
    ///   is for long-running work; Proxiima answers inline.
    ///
    /// Neither is advertised in [`get_info`]'s capabilities, so a spec-abiding
    /// client never expects them.
    ///
    /// [`get_info`]: DynamicHandler::get_info
    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + MaybeSendFuture + '_ {
        let call = self.call_tool_observed(request, &context);
        async move { call.await.1 }
    }
}

/// Await `call`, sending `notifications/progress` for the request's token
/// every interval until it finishes.
///
/// rmcp closes a session that carries no traffic for its idle timeout, and
/// neither SSE pings nor a call still running count: a long tool call lost
/// its session and its result. Each notification is session traffic, and
/// tells the client the call is alive. `progress` counts beats — the spec
/// asks only that it increase — and no total is claimed.
async fn with_progress_heartbeat<F: Future>(
    call: F,
    heartbeat: Option<(ProgressToken, Peer<RoleServer>, Duration)>,
) -> F::Output {
    let Some((token, peer, interval)) = heartbeat else {
        return call.await;
    };
    let mut call = std::pin::pin!(call);
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    let mut beats = 0_u32;
    loop {
        tokio::select! {
            output = &mut call => return output,
            _ = ticks.tick() => {
                beats = beats.saturating_add(1);
                let beat = ProgressNotificationParam::new(token.clone(), f64::from(beats))
                    .with_message("tool call running");
                if let Err(err) = peer.notify_progress(beat).await {
                    tracing::debug!(error = %err, "progress heartbeat not delivered");
                }
            }
        }
    }
}

/// Map a tool-invocation failure to a typed JSON-RPC error so external
/// agents can tell bad input from a server fault, instead of every failure
/// collapsing to `internal_error` (-32603). The mapping `tools/call` uses;
/// a not-authorized message lists the caller's still-allowed actions.
#[must_use]
pub fn tool_invocation_error_to_error_data(
    registry: &SpiiceRegistryFrozen,
    err: ToolInvocationError,
    auth: Option<&AuthContext>,
) -> ErrorData {
    match err {
        ToolInvocationError::NotAuthorized(name) => {
            ErrorData::invalid_request(not_authorized_message(registry, &name, auth), None)
        }
        ToolInvocationError::ToolNotFound(name) => {
            ErrorData::invalid_params(format!("unknown tool: {name}"), None)
        }
        ToolInvocationError::Tool(inner) => mcp_tool_error_to_error_data(&inner),
    }
}

/// Build the not-authorized message, enriched with the caller's still-allowed
/// actions on the denied dispatcher tool so an agent can immediately retry with
/// a permitted action instead of guessing. `name` is either a tool id or a
/// `tool:action` leaf.
///
/// The candidate actions come off the descriptor's `action_arg_specs`, which
/// is the enumeration every other seam reads. Sourced from the substrate
/// `all_core_actions()` table, a denied *spiice* dispatcher listed nothing,
/// so the one caller who most needs the hint — someone holding a partial
/// palette on a tool the substrate has never heard of — got the bare message.
fn not_authorized_message(
    registry: &SpiiceRegistryFrozen,
    name: &str,
    auth: Option<&AuthContext>,
) -> String {
    let tool = name.split_once(':').map_or(name, |(tool, _)| tool);
    let allowed: Vec<&str> = auth.map_or_else(Vec::new, |auth| {
        registry.mcp_tool(tool).map_or_else(Vec::new, |descriptor| {
            descriptor
                .action_arg_specs
                .iter()
                .map(|spec| spec.action)
                .chain(descriptor.argv_action_specs.iter().map(|spec| spec.action))
                .filter(|action| {
                    action_allowed_for_auth(Caller::new(Some(auth)), descriptor, action)
                })
                .collect()
        })
    });
    if allowed.is_empty() {
        format!("tool {name} not authorized for this MCP token")
    } else {
        format!(
            "tool {name} not authorized for this MCP token; allowed {tool} actions: {}",
            allowed.join(", ")
        )
    }
}

fn resource_invocation_error_to_error_data(err: ToolInvocationError) -> ErrorData {
    match err {
        ToolInvocationError::NotAuthorized(name) => ErrorData::invalid_request(
            format!("resource {name} not authorized for this MCP token"),
            None,
        ),
        ToolInvocationError::ToolNotFound(name) => {
            ErrorData::resource_not_found(format!("unknown resource: {name}"), None)
        }
        // A missing entity behind a well-formed resource URI is a
        // `resource_not_found` (-32002), unlike the tool path where the
        // same fault is an argument problem.
        ToolInvocationError::Tool(inner) if inner.kind() == ToolErrorKind::NotFound => {
            ErrorData::resource_not_found(inner.client_message(), None)
        }
        ToolInvocationError::Tool(inner) => mcp_tool_error_to_error_data(&inner),
    }
}

/// JSON-RPC "Server error", the implementation-defined range the spec
/// reserves at -32000..-32099. Declared backpressure has no code of its own
/// in either JSON-RPC or MCP, and the three standard ones all misdescribe
/// it: -32602/-32600 blame a request that was legal, and -32603 is the
/// generic internal fault this exists to stop being confused with.
const SERVER_ERROR: rmcp::model::ErrorCode = rmcp::model::ErrorCode(-32000);

/// The machine-readable discriminator carried in `error.data.code`, so a
/// client switches on a token rather than on the human message. Same slug
/// as the REST surface's problem `type` (`capacity-exhausted`), spelled in
/// the snake case the JSON-RPC payloads use.
const CAPACITY_EXHAUSTED_CODE: &str = "capacity_exhausted";

/// `error.data.code` for another store that did not answer: retryable, and
/// never -32602 (a client would read that as "no such memory").
const STORE_UNAVAILABLE_CODE: &str = "store_unavailable";
const IDEMPOTENCY_CONFLICT_CODE: &str = "idempotency_conflict";

/// Classify an [`ToolError`] by JSON-RPC code: caller-input faults —
/// including references to missing entities — → `invalid_params` (-32602);
/// well-formed-but-illegal requests → `invalid_request` (-32600); declared
/// backpressure → server error (-32000) with `data.code`
/// `capacity_exhausted` and the message verbatim; another store that did
/// not answer → server error (-32000) with `data.code` `store_unavailable`
/// and `data.store`; infrastructure faults →
/// `internal_error` (-32603). Resource reads remap `NotFound` before
/// reaching here (resources map a missing entity to `resource_not_found`).
#[must_use]
pub fn mcp_tool_error_to_error_data(err: &ToolError) -> ErrorData {
    match err.kind() {
        ToolErrorKind::InvalidInput | ToolErrorKind::NotFound => {
            ErrorData::invalid_params(err.client_message(), None)
        }
        ToolErrorKind::InvalidRequest => {
            if let Some(data) = idempotency_conflict_data(err) {
                ErrorData::invalid_request(err.client_message(), Some(data))
            } else {
                ErrorData::invalid_request(err.client_message(), None)
            }
        }
        ToolErrorKind::CapacityExhausted => ErrorData::new(
            SERVER_ERROR,
            err.client_message(),
            Some(serde_json::json!({ "code": CAPACITY_EXHAUSTED_CODE })),
        ),
        ToolErrorKind::StoreUnavailable => {
            let store = match err {
                ToolError::StoreUnavailable { store } => Some(*store),
                _ => None,
            };
            ErrorData::new(
                SERVER_ERROR,
                err.client_message(),
                Some(serde_json::json!({ "code": STORE_UNAVAILABLE_CODE, "store": store })),
            )
        }
        ToolErrorKind::Internal => generic_internal_error(err),
    }
}

fn idempotency_conflict_data(err: &ToolError) -> Option<serde_json::Value> {
    let existing = match err {
        ToolError::Protocol(inner)
            if inner.code == proxiima_core::error::ErrorCode::IdempotencyConflict =>
        {
            inner.existing
        }
        ToolError::Storage(proxiima_core::StorageError::IdempotencyConflict {
            existing, ..
        }) => *existing,
        _ => return None,
    };
    let mut data = serde_json::Map::new();
    data.insert(
        "code".into(),
        serde_json::Value::String(IDEMPOTENCY_CONFLICT_CODE.to_string()),
    );
    if let Some(existing) = existing {
        data.insert(
            "existing".into(),
            serde_json::Value::String(existing.to_string()),
        );
    }
    Some(serde_json::Value::Object(data))
}

fn generic_internal_error(err: impl std::fmt::Display) -> ErrorData {
    tracing::error!(error = %err, "mcp internal error");
    ErrorData::internal_error("internal server error", None)
}

fn structured_tool_output(
    tool_name: &str,
    output: serde_json::Value,
) -> Result<(serde_json::Value, String), ErrorData> {
    if !output.is_object() {
        tracing::error!(tool = %tool_name, "mcp tool output must be a JSON object");
        return Err(ErrorData::internal_error("internal server error", None));
    }
    let text = serde_json::to_string(&output).map_err(generic_internal_error)?;
    Ok((output, text))
}

/// A tool's reply as the wire carries it.
fn wire_reply(tool: &str, reply: ToolReply) -> Result<CallToolResult, ErrorData> {
    match reply {
        ToolReply::Structured(output) => {
            let (output, text) = structured_tool_output(tool, output)?;
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            result.structured_content = Some(output);
            Ok(result)
        }
        ToolReply::Content(content) => {
            let blocks: Vec<ContentBlock> = content.into_iter().map(content_block).collect();
            Ok(CallToolResult::success(blocks))
        }
        ToolReply::Failure(content) => Ok(CallToolResult::error(
            content.into_iter().map(content_block).collect(),
        )),
        ToolReply::StructuredFailure {
            structured,
            content,
        } => {
            let mut result =
                CallToolResult::error(content.into_iter().map(content_block).collect());
            result.structured_content = Some(serde_json::Value::Object(structured));
            Ok(result)
        }
    }
}

/// The MCP content block of the same name. Matched variant by variant, so a
/// new [`ToolContent`] shape cannot ship without its wire form.
fn content_block(content: ToolContent) -> ContentBlock {
    match content {
        ToolContent::Text(text) => ContentBlock::text(text),
        ToolContent::Image { data, mime_type } => ContentBlock::image(data, mime_type),
        ToolContent::Audio { data, mime_type } => ContentBlock::audio(data, mime_type),
        ToolContent::ResourceLink {
            uri,
            name,
            description,
            mime_type,
        } => {
            let mut link = Resource::new(uri, name);
            if let Some(description) = description {
                link = link.with_description(description);
            }
            if let Some(mime_type) = mime_type {
                link = link.with_mime_type(mime_type);
            }
            ContentBlock::resource_link(link)
        }
        ToolContent::TextResource {
            uri,
            mime_type,
            text,
        } => ContentBlock::resource(with_mime_type(ResourceContents::text(text, uri), mime_type)),
        ToolContent::BlobResource {
            uri,
            mime_type,
            blob,
        } => ContentBlock::resource(with_mime_type(ResourceContents::blob(blob, uri), mime_type)),
    }
}

fn with_mime_type(contents: ResourceContents, mime_type: Option<String>) -> ResourceContents {
    match mime_type {
        Some(mime_type) => contents.with_mime_type(mime_type),
        None => contents,
    }
}

/// A dispatcher's `inputSchema` over the actions both policies admit,
/// so `tools/list` never advertises an action the caller cannot invoke.
/// Unrestricted (or absent) capabilities and flat tools get the full schema.
#[cfg(test)]
pub(crate) fn project_dispatcher_actions(
    descriptor: &ToolDescriptor,
    capabilities: Option<&CapabilitySet>,
) -> serde_json::Value {
    descriptor.input_schema(|action| {
        capabilities
            .is_none_or(|capabilities| descriptor.action_advertised_by(capabilities, action))
    })
}

/// A descriptor's `inputSchema` over the actions this caller can both name
/// and authorize. A viewer of a mixed dispatcher therefore sees its read
/// leaves — their fields, their prose — without being shown write leaves it
/// cannot invoke.
pub(crate) fn project_dispatcher_actions_for_auth(
    caller: Caller<'_>,
    descriptor: &ToolDescriptor,
) -> serde_json::Value {
    descriptor.input_schema(|action| {
        caller.auth.is_none() || action_allowed_for_auth(caller, descriptor, action)
    })
}

async fn caller_search_input_schema(
    ctx: &ToolCtx,
    schema: serde_json::Value,
) -> Result<serde_json::Value, ErrorData> {
    proxiima_core::catalog::core_tools::search_memories::caller_input_schema(ctx, schema)
        .await
        .map_err(generic_internal_error)
}

fn to_rmcp_annotations(annotations: ToolAnnotations) -> rmcp::model::ToolAnnotations {
    let mut hints = rmcp::model::ToolAnnotations::new();
    if let Some(read_only) = annotations.read_only {
        hints = hints.read_only(read_only);
    }
    if let Some(destructive) = annotations.destructive {
        hints = hints.destructive(destructive);
    }
    if let Some(idempotent) = annotations.idempotent {
        hints = hints.idempotent(idempotent);
    }
    if let Some(open_world) = annotations.open_world {
        hints = hints.open_world(open_world);
    }
    hints
}

fn raw_resource_from_meta(meta: &proxiima_core::spiice::ResourceContract) -> Resource {
    Resource::new(static_resource_uri(meta.uri_template), meta.name)
        .with_title(meta.title)
        .with_description(meta.description)
        .with_mime_type("application/json")
}

fn raw_resource_template_from_meta(
    meta: &proxiima_core::spiice::ResourceContract,
) -> ResourceTemplate {
    ResourceTemplate::new(meta.uri_template, meta.name)
        .with_title(meta.title)
        .with_description(meta.description)
        .with_mime_type("application/json")
}

fn static_resource_uri(uri_template: &str) -> String {
    uri_template
        .split_once('{')
        .map_or(uri_template, |(uri, _)| uri)
        .to_string()
}

/// Whether `auth` may see one core resource: its exact caller keys and
/// deployment surface admit it, and its owner role allows the declaration.
pub(crate) fn resource_allowed_for_auth(
    auth: Option<&AuthContext>,
    resource: &proxiima_core::spiice::ResourceContract,
) -> bool {
    resource_declaration_allowed_for_auth(auth, resource.key(), resource.declaration())
}

fn resource_declaration_allowed_for_auth(
    auth: Option<&AuthContext>,
    key: ResourceKey,
    declared: KeyDeclaration,
) -> bool {
    let in_scope = auth.map_or(UNAUTHENTICATED_SCOPE_ALLOWS, |ctx| {
        ctx.authz
            .capabilities()
            .admits(&ScopeKey::Resource(key), declared)
    });
    in_scope && owner_role_allows(auth, declared.owner_access())
}

pub(crate) fn how_to_allowed_for_auth(auth: Option<&AuthContext>) -> bool {
    resource_declaration_allowed_for_auth(
        auth,
        proxiima_core::catalog::how_to_resource_key(),
        proxiima_core::catalog::how_to_resource_declaration(),
    )
}

pub(crate) fn advertised_resource_scope_keys(auth: Option<&AuthContext>) -> BTreeSet<&'static str> {
    let mut keys: BTreeSet<_> = all_core_resources()
        .filter(|resource| resource_allowed_for_auth(auth, resource))
        .map(|resource| resource.scope_key)
        .collect();
    if how_to_allowed_for_auth(auth) {
        keys.insert(proxiima_core::protocol::resource::HOW_TO);
    }
    keys
}

fn author_from_ctx(
    auth: Option<&AuthContext>,
    client_name: &str,
    client_version: &str,
) -> ToolCaller {
    ToolCaller::new(
        trusted_model_id(auth).unwrap_or_else(|| UNKNOWN_OPERATOR_LABEL.to_string()),
        client_name,
        client_version,
    )
}

/// The only source of trusted model provenance on this transport: the
/// authenticated context. Never `clientInfo`, never an argument.
fn trusted_model_id(auth: Option<&AuthContext>) -> Option<String> {
    auth.and_then(|ctx| ctx.authz.trusted_model_id())
        .map(ToString::to_string)
}

/// Client `(name, version)`, recorded as operator provenance. A per-request
/// call (its `_meta` names a protocol version, as every `2026-07-28` call
/// does) names its client in that `_meta` or not at all; a session call
/// takes the `initialize` handshake's. Falls back to `("unknown", "0")`.
///
/// Not `RequestContext::client_info`: for a per-request call without
/// `clientInfo` it falls back to the peer rmcp synthesizes for stateless
/// requests, which carries rmcp's own name.
#[must_use]
pub fn peer_implementation(context: &RequestContext<RoleServer>) -> (String, String) {
    let client = if context.meta.protocol_version().is_some() {
        context.meta.client_info()
    } else {
        context
            .peer
            .peer_info()
            .map(|info| info.client_info.clone())
    };
    client.map_or_else(
        || ("unknown".to_string(), "0".to_string()),
        |info| (info.name, info.version),
    )
}

fn canonical_tool_name(server: &ToolHost, request_name: &str) -> Option<String> {
    server
        .registry()
        .list_mcp_tools()
        .iter()
        .find(|descriptor| tool_name_matches(descriptor.name, request_name))
        .map(|descriptor| descriptor.name.to_string())
}

/// The per-request service set (request-extension bag plus allowlisted
/// headers) of one served call. Empty when rmcp injected no HTTP parts
/// (direct handler tests).
fn request_services(
    server: &ToolHost,
    context: &RequestContext<RoleServer>,
) -> Result<proxiima_core::SpiiceServices, ErrorData> {
    context
        .extensions
        .get::<http::request::Parts>()
        .map_or_else(
            || Ok(proxiima_core::SpiiceServices::default()),
            |parts| server.request_services(&parts.headers, &parts.extensions),
        )
        .map_err(|err| mcp_tool_error_to_error_data(&err))
}

/// Resolve the token scope from the request auth context. Returns `None`
/// when no token-bearing layer ran ahead of rmcp (direct handler tests).
/// Host-bearer tokens carry the host-provided scope.
///
/// rmcp's `StreamableHttpService` injects [`http::request::Parts`] into
/// the rmcp request extensions, and our auth layer inserts
/// `AuthContext` into the axum request extensions before nesting the
/// rmcp service. The two extension stores are different — we follow the
/// documented bridge.
#[must_use]
pub fn auth_context(context: &RequestContext<RoleServer>) -> Option<AuthContext> {
    let parts = context.extensions.get::<http::request::Parts>()?;
    let ctx = parts.extensions.get::<AuthContext>()?;
    Some(ctx.clone())
}

fn scope_allows(capabilities: Option<&CapabilitySet>, descriptor: &ToolDescriptor) -> bool {
    match capabilities {
        Some(capabilities) => {
            // Argv-keyed dispatchers advertise through `tool:action` leaves
            // exactly like `action`-tagged ones — the derived key is the
            // same vocabulary the invocation gate judges.
            descriptor.advertised_by(capabilities)
        }
        // No auth context bound to the request. In release builds this
        // means the request bypassed the auth layer (which 401s before
        // dispatch) — fail closed rather than expose the full tool
        // surface. Direct handler tests run without the layer, so the
        // test arm stays permissive; it is compiled out of release.
        None => UNAUTHENTICATED_SCOPE_ALLOWS,
    }
}

/// Who a projection is for, and whether request behaviors may narrow it.
///
/// A listing (`tools/list`, the REST catalog, the `OpenAPI` document, the
/// instructions) is built with [`Self::listing`] and also asks every request
/// behavior's `visible`. The call path is built with [`Self::new`] and never
/// does: there a behavior's `handle` decides, with the call in hand.
#[derive(Clone, Copy)]
pub(crate) struct Caller<'a> {
    auth: Option<&'a AuthContext>,
    behaviors: Option<&'a ToolCtx>,
}

impl<'a> Caller<'a> {
    /// Caller keys, deployment surface and owner role only.
    pub(crate) const fn new(auth: Option<&'a AuthContext>) -> Self {
        Self {
            auth,
            behaviors: None,
        }
    }

    /// Both policies, owner role and every request behavior's `visible`, judged
    /// with `ctx` (see [`ToolCtx::behaviors_show`]). `ctx` is `None` only
    /// without an authenticated caller, which nothing is listed to outside
    /// tests.
    pub(crate) const fn listing(auth: Option<&'a AuthContext>, ctx: Option<&'a ToolCtx>) -> Self {
        Self {
            auth,
            behaviors: ctx,
        }
    }

    #[cfg(feature = "rest")]
    pub(crate) const fn auth(&self) -> Option<&'a AuthContext> {
        self.auth
    }

    fn behaviors_show(&self, tool: &ToolDescriptorView<'_>) -> bool {
        self.behaviors.is_none_or(|ctx| ctx.behaviors_show(tool))
    }
}

/// Whether this caller may see `descriptor` at all.
///
/// A dispatcher is visible when at least one of its actions is in scope,
/// permitted by the caller's owner role, and (in a listing) shown by every
/// request behavior. This keeps a mixed spiice dispatcher useful to a viewer
/// without advertising its writes.
pub(crate) fn tool_allowed_for_auth(caller: Caller<'_>, descriptor: &ToolDescriptor) -> bool {
    let scope = caller.auth.map(|ctx| ctx.authz.capabilities());
    if !scope_allows(scope, descriptor) {
        return false;
    }
    if !descriptor.action_arg_specs.is_empty() {
        descriptor
            .action_arg_specs
            .iter()
            .any(|spec| action_allowed_for_auth(caller, descriptor, spec.action))
    } else if !descriptor.argv_action_specs.is_empty() {
        descriptor
            .argv_action_specs
            .iter()
            .any(|spec| action_allowed_for_auth(caller, descriptor, spec.action))
    } else {
        tool_owner_role_allows(caller.auth, descriptor, None)
            && caller.behaviors_show(&ToolDescriptorView::registry(descriptor, None))
    }
}

/// Whether one dispatcher action is in scope, owner-role authorized and (in
/// a listing) shown by every request behavior.
pub(crate) fn action_allowed_for_auth(
    caller: Caller<'_>,
    descriptor: &ToolDescriptor,
    action: &str,
) -> bool {
    let scope_allowed = caller
        .auth
        .is_none_or(|ctx| descriptor.action_advertised_by(ctx.authz.capabilities(), action));
    // One classification rule for both vocabularies, owned by the
    // descriptor, so this advertisement decision and the owner-role gate
    // `ScopeGateBehavior` runs at call time cannot answer differently.
    scope_allowed
        && tool_owner_role_allows(caller.auth, descriptor, Some(action))
        && caller.behaviors_show(&ToolDescriptorView::registry(descriptor, Some(action)))
}

/// A space-selecting tool can be used in any effective owner; the call's
/// scope gate checks the concrete selection. Other tools use the default.
fn tool_owner_role_allows(
    auth: Option<&AuthContext>,
    descriptor: &ToolDescriptor,
    action: Option<&str>,
) -> bool {
    let access = descriptor.declaration(action).owner_access();
    let schema = action.map_or(Some(&descriptor.args_schema), |action| {
        descriptor.action_argument_schema(action)
    });
    let selects_space = schema
        .and_then(|schema| schema.get("properties"))
        .and_then(|properties| properties.get("space"))
        .is_some();
    if selects_space && let Some(ctx) = auth {
        ctx.authz
            .readable_owners(proxiima_core::MemoryKind::Fact)
            .iter()
            .any(|owner| ctx.authz.may_run_tool(owner, access))
    } else {
        owner_role_allows(auth, access)
    }
}

fn owner_role_allows(auth: Option<&AuthContext>, access: OwnerAccess) -> bool {
    let Some(ctx) = auth else {
        return UNAUTHENTICATED_SCOPE_ALLOWS;
    };
    ctx.authz.may_run_tool(&ctx.owner, access)
}

/// MCP/REST tool-level projection for the actions visible to one caller:
/// the [`ToolEffect::join`](proxiima_core::ToolEffect::join) of those
/// actions, so a caller who sees only a dispatcher's reads is told it reads.
/// `None` when the caller sees no action at all.
pub(crate) fn annotations_for_auth(
    caller: Caller<'_>,
    descriptor: &ToolDescriptor,
) -> Option<ToolAnnotations> {
    // Each action's classification is the descriptor's single rule, never
    // re-derived here.
    descriptor
        .effect_over(|action| {
            caller.auth.is_none() || action_allowed_for_auth(caller, descriptor, action)
        })
        .map(ToolAnnotations::registered)
}

/// Whether a request that carries no bound auth context may see or call
/// a tool. Release: `false` (fail closed — a missing auth layer is
/// a regression, not a no-auth grant). Test: `true` (direct-handler
/// ergonomics). The split makes the permissive arm un-shippable.
#[cfg(not(test))]
const UNAUTHENTICATED_SCOPE_ALLOWS: bool = false;
#[cfg(test)]
const UNAUTHENTICATED_SCOPE_ALLOWS: bool = true;

/// The reserved call-context argument that names the operator label. The REST surface
/// reserves the same names, and [`strip_call_context_args`] removes them.
pub const MODEL_ID_ARG: &str = "model_id";
/// The reserved call-context arguments that name the caller's own Self Perspective, in the
/// order they are read.
pub const SELF_PERSPECTIVE_ARGS: [&str; 3] = [
    "_proxiima_caller_self_perspective",
    "caller_self_perspective",
    "current_root_perspective_memory_id",
];

/// The caller of one `tools/call`.
///
/// The reserved `model_id` argument is a caller *claim*. When the token
/// binds a model identity, that identity wins and a differing claim is
/// refused as invalid params — the same error class this function already
/// returns for malformed reserved metadata. Precedence itself lives in
/// [`resolve_operator_label`], shared with the REST surface so the two
/// transports cannot drift.
///
/// # Errors
///
/// `invalid_params` when `model_id` contradicts the token's bound model, or
/// a caller-self-perspective argument is not a UUID string.
pub fn author_from_args(
    args: &serde_json::Value,
    auth: Option<&AuthContext>,
    client_name: &str,
    client_version: &str,
) -> Result<ToolCaller, ErrorData> {
    let trusted = trusted_model_id(auth);
    let claimed = match args.get(MODEL_ID_ARG) {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(label)) => Some(label.as_str()),
        Some(_) => {
            return Err(ErrorData::invalid_params(
                format!("{MODEL_ID_ARG} must be a string"),
                None,
            ));
        }
    };
    let model_id = resolve_operator_label(trusted.as_deref(), claimed)
        .map_err(|conflict| ErrorData::invalid_params(conflict.detail("model_id"), None))?;
    let caller_self_perspective = caller_self_perspective_from_args(args)?;
    Ok(ToolCaller::new(model_id, client_name, client_version)
        .with_self_perspective(caller_self_perspective))
}

fn caller_self_perspective_from_args(
    args: &serde_json::Value,
) -> Result<Option<MemoryId>, ErrorData> {
    let Some((field, raw)) = SELF_PERSPECTIVE_ARGS
        .into_iter()
        .find_map(|field| args.get(field).map(|raw| (field, raw)))
    else {
        return Ok(None);
    };
    let Some(raw) = raw.as_str() else {
        return Err(ErrorData::invalid_params(
            format!("{field} must be a UUID string"),
            None,
        ));
    };
    let id = proxiima_core::catalog::parse_wire_uuid(raw).map_err(|err| {
        ErrorData::invalid_params(format!("{field} must be a valid UUID: {err}"), None)
    })?;
    Ok(Some(MemoryId::new(id)))
}

/// Remove the reserved call-context arguments [`author_from_args`] read
/// (`model_id` and the caller-self-perspective aliases), so a tool's own
/// argument validation never sees them.
pub fn strip_call_context_args(args: &mut serde_json::Value) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    for field in SELF_PERSPECTIVE_ARGS {
        obj.remove(field);
    }
    // `model_id` is the reserved operator label: `author_from_args` has already
    // captured it into the author context, so strip it as a context field. This
    // lets any dispatcher tool (whose per-action specs do not list `model_id`)
    // accept it without a spurious unexpected-field rejection; flat tools that
    // want it (e.g. core_derive) read it from `ctx.caller().model_id`.
    obj.remove(MODEL_ID_ARG);
}

/// Whether `scope` admits a host tool, which is flat: its key is its name as
/// a [`ToolName`], and its declaration is the host's, for every audience (the
/// host lists its tools per caller). A served name is a key (the host tool
/// list checked it), so a name that is not is held by no palette.
fn host_tool_in_scope(capabilities: &CapabilitySet, name: &str, effect: ToolEffect) -> bool {
    match ToolName::parse(name) {
        Ok(name) => capabilities.admits(
            &ScopeKey::Tool(name),
            proxiima_core::KeyDeclaration::shared(Some(effect)),
        ),
        Err(_) => false,
    }
}

/// Whether this caller may see one host tool: both policies admit it, its
/// declaration permits the owner role, and every listing behavior shows it.
fn host_tool_allowed_for_auth(caller: Caller<'_>, tool: &HostTool) -> bool {
    let in_scope = caller.auth.map_or(UNAUTHENTICATED_SCOPE_ALLOWS, |ctx| {
        host_tool_in_scope(ctx.authz.capabilities(), &tool.name, tool.effect)
    });
    in_scope
        && owner_role_allows(caller.auth, tool.call_marker().owner_access())
        && caller.behaviors_show(&ToolDescriptorView::host(
            &tool.name,
            &tool.description,
            tool.effect,
        ))
}

/// A host tool's `tools/list` entry; `None` (and a warning) when its
/// input schema is not a JSON object or its output schema admits non-objects.
/// Its output schema goes out as registered tools' do: validation only.
/// Its hints are [`ToolAnnotations::host`] of its effect, plus the
/// `openWorldHint` it set; its `_meta` goes out as given.
fn host_tool_metadata(mut tool: HostTool) -> Option<Tool> {
    let output = match host_wire_output_schema(tool.output_schema.as_mut()) {
        Ok(output) => output,
        Err(error) => {
            tracing::warn!(tool = %tool.name, error = %error, "host tool output schema must describe JSON objects; not listed");
            return None;
        }
    };
    let serde_json::Value::Object(args) = tool.args_schema else {
        tracing::warn!(tool = %tool.name, "host tool input schema must be a JSON object; not listed");
        return None;
    };
    let mut listed = Tool::new(
        Cow::Owned(provider_safe_tool_name(&tool.name)),
        Cow::Owned(tool.description),
        Arc::new(args),
    )
    .annotate(to_rmcp_annotations(ToolAnnotations {
        open_world: tool.open_world,
        ..ToolAnnotations::host(tool.effect)
    }));
    if let Some(output) = output {
        listed = listed.with_raw_output_schema(Arc::new(output));
    }
    if let Some(meta) = tool.meta {
        listed = listed.with_meta(MetaObject(meta));
    }
    Some(listed)
}

/// The wire form of a host tool's output schema: `None` for a tool that
/// declares none, the reason for one that does not describe JSON objects.
fn host_wire_output_schema(
    schema: Option<&mut serde_json::Value>,
) -> Result<Option<JsonObject>, String> {
    let Some(schema) = schema else {
        return Ok(None);
    };
    normalize_mcp_output_schema(schema)?;
    match mcp_wire_output_schema(schema) {
        serde_json::Value::Object(wire) => Ok(Some(wire)),
        _ => Err("MCP output schema must be a JSON object".to_owned()),
    }
}

#[cfg(test)]
#[path = "handler/output_contract_tests.rs"]
mod output_contract_tests;

#[cfg(test)]
#[path = "handler_tests.rs"]
mod tests;

#[cfg(test)]
mod observed_tests;
