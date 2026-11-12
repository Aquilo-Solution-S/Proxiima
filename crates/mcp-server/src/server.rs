use std::sync::Arc;

#[cfg(test)]
use proxiima_core::AuthPath;
use proxiima_core::mcp::core_tools::{
    get_graph::{GetGraphArgs, get_graph},
    get_memories::{GetMemoriesArgs, get_memories},
    get_memory::{GetMemoryArgs, get_memory},
    goal_reads::{ListGoalsArgs, get_goal, list_goals},
    list_schemas::{ListSchemasArgs, get_schema, list_schemas},
    list_substrate_tools::{ListSubstrateToolsArgs, list_substrate_tools},
    memory_counts::memory_counts,
    walk_memory_lineage::{WalkMemoryLineageArgs, walk_memory_lineage},
};
use proxiima_core::mcp::{
    McpToolDescriptor, Next, TerminalDispatch, ToolCall, ToolContent, ToolErrorKind, ToolReply,
    resolve_operator_label, tool_name_matches,
};
use proxiima_core::protocol::resource as protocol_resource;
use proxiima_core::spiice::spiice0::resource as core_resource;
use proxiima_core::{
    Engine, ScopeKey, SpiiceRegistryFrozen, SpiiceServices, ToolCaller, ToolCtx, ToolError,
};
use serde::Serialize;

use crate::auth::McpAuthContext;
use crate::host_tools::{McpHostTool, McpHostTools};
use crate::request_scope::RequestHeaderAllowlist;
use crate::tool_list::ToolListNotifier;

#[derive(Clone)]
pub struct McpToolHost {
    registry: Arc<SpiiceRegistryFrozen>,
    services: SpiiceServices,
    engine: Option<Arc<Engine>>,
    request_headers: RequestHeaderAllowlist,
    host_tools: Option<Arc<dyn McpHostTools>>,
    tool_list: Option<ToolListNotifier>,
}

/// Longest host tool name served.
pub const MAX_HOST_TOOL_NAME_CHARS: usize = 128;

impl std::fmt::Debug for McpToolHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolHost")
            .field("has_engine", &self.engine.is_some())
            .field("host_tools", &self.host_tools)
            .field("tool_list_notifications", &self.tool_list.is_some())
            .finish_non_exhaustive()
    }
}

impl McpToolHost {
    #[must_use]
    pub fn from_parts(registry: Arc<SpiiceRegistryFrozen>, services: SpiiceServices) -> Self {
        Self {
            registry,
            services,
            engine: None,
            request_headers: RequestHeaderAllowlist::default(),
            host_tools: None,
            tool_list: None,
        }
    }

    #[must_use]
    pub fn from_engine(engine: Arc<Engine>, services: SpiiceServices) -> Self {
        Self::from_parts(Arc::new(engine.registry().clone()), services).with_engine(engine)
    }

    #[must_use]
    pub fn with_engine(mut self, engine: Arc<Engine>) -> Self {
        self.engine = Some(engine);
        self
    }

    /// Serve `tools` beside the registry's (see [`McpHostTools`]).
    #[must_use]
    pub fn with_host_tools(mut self, tools: Arc<dyn McpHostTools>) -> Self {
        self.host_tools = Some(tools);
        self
    }

    /// Advertise `tools.listChanged` and register every caller with
    /// `notifier` under its owner, so [`ToolListNotifier::notify`] reaches
    /// that owner's connected clients. A host attaches one only if it calls
    /// `notify` whenever an owner's tool list changes: the capability is a
    /// promise to. Default: none.
    #[must_use]
    pub fn with_tool_list_notifier(mut self, notifier: ToolListNotifier) -> Self {
        self.tool_list = Some(notifier);
        self
    }

    pub(crate) const fn tool_list_notifier(&self) -> Option<&ToolListNotifier> {
        self.tool_list.as_ref()
    }

    /// The engine tools run against, when one was attached.
    #[must_use]
    pub fn engine(&self) -> Option<&Arc<Engine>> {
        self.engine.as_ref()
    }

    /// The host tools `auth` may be shown, before palette, owner-role and
    /// behavior filtering: [`McpHostTools::list`] minus every name that is not
    /// 1..=[`MAX_HOST_TOOL_NAME_CHARS`] characters of `[A-Za-z0-9_.-]`
    /// (so its wire name is itself, and it can be neither a `tool:action`
    /// leaf nor a `resource:` key), that a registry tool serves under its
    /// canonical or wire name, or that the list already named. One call
    /// reads the source once: a request that needs the catalog holds it.
    ///
    /// # Errors
    ///
    /// The source's [`ToolError`], unchanged; no host source means an
    /// empty catalog, a failing one never does.
    pub async fn host_tools_for(
        &self,
        auth: &McpAuthContext,
    ) -> Result<Vec<McpHostTool>, ToolError> {
        let Some(source) = &self.host_tools else {
            return Ok(Vec::new());
        };
        let mut served: Vec<McpHostTool> = Vec::new();
        for tool in source.list(auth).await? {
            match host_tool_name_refusal(&self.registry, &tool.name, &served) {
                Some(reason) => tracing::warn!(tool = %tool.name, reason, "host tool not served"),
                None => served.push(tool),
            }
        }
        Ok(served)
    }

    /// The host's contribution to the `instructions` served to `auth`
    /// ([`McpHostTools::instructions`]); `None` without a host source.
    pub(crate) async fn host_instructions(&self, auth: &McpAuthContext) -> Option<String> {
        self.host_tools.as_ref()?.instructions(auth).await
    }

    /// Copy the allowlisted inbound headers of each served call into its
    /// [`RequestHeaders`](proxiima_core::RequestHeaders) service. Default:
    /// none.
    #[must_use]
    pub fn with_request_headers(mut self, allowlist: RequestHeaderAllowlist) -> Self {
        self.request_headers = allowlist;
        self
    }

    /// The per-request service set of one served call: the
    /// [`SpiiceServices`] a host middleware placed in the request's
    /// extensions, plus the allowlisted headers. Merged onto the boot set by
    /// [`Self::call_tool_in_request`] / [`Self::read_resource_in_request`].
    ///
    /// # Errors
    ///
    /// [`ToolError::InvalidInput`] for a repeated or non-ASCII allowlisted
    /// header; [`ToolError::Other`] when the extension bag carries a
    /// `RequestHeaders` of its own.
    pub fn request_services(
        &self,
        headers: &http::HeaderMap,
        extensions: &http::Extensions,
    ) -> Result<SpiiceServices, ToolError> {
        crate::request_scope::request_services(&self.request_headers, headers, extensions)
    }

    #[must_use]
    pub fn registry(&self) -> &SpiiceRegistryFrozen {
        &self.registry
    }

    /// Build a per-call [`ToolCtx`] derived from the auth regime.
    ///
    /// All entity references cross the wire as bare UUIDs.
    ///
    /// The caller's `model_id` is re-reconciled against the authenticated
    /// identity rather than trusted as given, because a host builds the
    /// [`ToolCaller`] by hand and `model_id` is the label a tool records:
    /// [`ToolCtx::new`] makes it the bound identity when the credential binds
    /// one, and a differing claim is refused here.
    ///
    /// # Errors
    ///
    /// [`ToolError::InvalidInput`] when the caller names a model other
    /// than the one the authenticated token binds. In-tree transports
    /// resolved that at the edge already, so reaching this is a host
    /// assembling a caller the credential does not support.
    pub fn ctx_for(&self, caller: ToolCaller, auth: &McpAuthContext) -> Result<ToolCtx, ToolError> {
        self.ctx_for_request(caller, auth, SpiiceServices::default())
    }

    /// [`Self::ctx_for`] with a per-request service set merged onto the boot
    /// set ([`SpiiceServices::try_extend`]).
    ///
    /// # Errors
    ///
    /// As [`Self::ctx_for`], and [`ToolError::Other`] when `request`
    /// repeats a type the boot set already holds — a host wiring fault, never
    /// a caller one, so a request can never replace a boot service.
    pub fn ctx_for_request(
        &self,
        mut caller: ToolCaller,
        auth: &McpAuthContext,
        request: SpiiceServices,
    ) -> Result<ToolCtx, ToolError> {
        let mut services = self.services.clone();
        services
            .try_extend(request)
            .map_err(|err| ToolError::Other(format!("request services: {err}")))?;
        caller.model_id =
            resolve_operator_label(auth.authz.trusted_model_id(), Some(&caller.model_id))
                .map_err(|conflict| ToolError::InvalidInput(conflict.detail("model_id")))?;
        Ok(ToolCtx::new(
            auth.owner,
            auth.authz.clone(),
            self.registry.clone(),
            services,
            caller,
        )
        .with_engine(self.engine.clone()))
    }

    /// # Errors
    ///
    /// Returns `ToolNotFound`, the called tool error, a failure of one text
    /// block as [`ToolError::Failure`] with that text, or an internal
    /// error when the final reply after request behaviors is anything else
    /// but a JSON object.
    pub async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
        caller: ToolCaller,
        auth: Option<McpAuthContext>,
    ) -> Result<serde_json::Value, ToolInvocationError> {
        self.call_tool_in_request(name, args, caller, auth, SpiiceServices::default())
            .await
    }

    /// [`Self::call_tool`] with a per-request service set (see
    /// [`Self::request_services`]) merged onto the boot set.
    ///
    /// # Errors
    ///
    /// As [`Self::call_tool`] and [`Self::ctx_for_request`].
    pub async fn call_tool_in_request(
        &self,
        name: &str,
        args: serde_json::Value,
        caller: ToolCaller,
        auth: Option<McpAuthContext>,
        request: SpiiceServices,
    ) -> Result<serde_json::Value, ToolInvocationError> {
        let reply = self
            .call_tool_reply_in_request(name, args, caller, auth, request)
            .await?;
        Ok(json_reply(name, reply)?)
    }

    /// [`Self::call_tool_in_request`] with the tool's whole [`ToolReply`]: a
    /// host tool may answer content blocks or a failure, which a JSON-only
    /// surface cannot carry.
    ///
    /// Resolves `name` once: a registry tool without asking the host
    /// source, any other name with one [`McpHostTools::resolve`].
    ///
    /// # Errors
    ///
    /// As [`Self::call_tool_in_request`], and the host source's resolve
    /// error for a name the registry does not serve. A [`ToolReply::Structured`]
    /// that is not an object, or a [`ToolReply::Content`] or
    /// [`ToolReply::StructuredFailure`] from a tool that declared an output
    /// schema, is an internal error.
    pub async fn call_tool_reply_in_request(
        &self,
        name: &str,
        args: serde_json::Value,
        caller: ToolCaller,
        auth: Option<McpAuthContext>,
        request: SpiiceServices,
    ) -> Result<ToolReply, ToolInvocationError> {
        self.call_tool_reply_observed_in_request(name, args, caller, auth, request)
            .await
            .1
    }

    /// [`Self::call_tool_reply_in_request`], answering also the key the scope
    /// gate judged the call by ([`ToolCtx::judged`]), after success or
    /// refusal. `None` when the call failed before its context existed or the
    /// gate derived no key.
    ///
    /// # Errors
    ///
    /// The result holds the same errors as [`Self::call_tool_reply_in_request`].
    pub fn call_tool_reply_observed_in_request<'a>(
        &'a self,
        name: &'a str,
        args: serde_json::Value,
        caller: ToolCaller,
        auth: Option<McpAuthContext>,
        request: SpiiceServices,
    ) -> impl Future<Output = (Option<ScopeKey>, Result<ToolReply, ToolInvocationError>)> + Send + use<'a>
    {
        Box::pin(async move {
            let Some(auth) = auth else {
                return (
                    None,
                    Err(ToolInvocationError::NotAuthorized(name.to_string())),
                );
            };
            match self.resolve_tool(&auth, name).await {
                Ok(target) => {
                    self.call_resolved(target, args, caller, auth, request)
                        .await
                }
                Err(err) => (None, Err(err)),
            }
        })
    }

    /// The tool `name` calls for `auth`: the registry's, else the host
    /// source's, resolving that one name. A name [`Self::host_tools_for`]
    /// would not serve is not asked for, and an entry of another name is
    /// not served.
    pub(crate) async fn resolve_tool(
        &self,
        auth: &McpAuthContext,
        name: &str,
    ) -> Result<ToolTarget<'_>, ToolInvocationError> {
        if let Some(descriptor) = self
            .registry
            .list_mcp_tools()
            .iter()
            .find(|d| tool_name_matches(d.name, name))
        {
            return Ok(ToolTarget::Registry(descriptor));
        }
        let not_found = || ToolInvocationError::ToolNotFound(name.to_string());
        let Some(source) = &self.host_tools else {
            return Err(not_found());
        };
        if host_tool_name_refusal(&self.registry, name, &[]).is_some() {
            return Err(not_found());
        }
        match source.resolve(auth, name).await? {
            Some(tool) if tool.name == name => Ok(ToolTarget::Host(Box::new(tool))),
            Some(tool) => {
                tracing::warn!(tool = %tool.name, asked = %name, "host resolved another name");
                Err(not_found())
            }
            None => Err(not_found()),
        }
    }

    /// Run a [`Self::resolve_tool`] result through the request behaviors,
    /// answering also the key the scope gate judged it by (see
    /// [`Self::call_tool_reply_observed_in_request`]). A host tool's call
    /// services are the request's, the call's [`McpHostToolCall`] and the
    /// `services` of the entry it resolved to; a type any of them already
    /// holds is an internal error.
    pub(crate) async fn call_resolved(
        &self,
        target: ToolTarget<'_>,
        args: serde_json::Value,
        caller: ToolCaller,
        auth: McpAuthContext,
        mut request: SpiiceServices,
    ) -> (Option<ScopeKey>, Result<ToolReply, ToolInvocationError>) {
        let declares_output_schema = target.declares_output_schema();
        let (name, terminal): (String, TerminalDispatch<'_>) = match target {
            ToolTarget::Registry(descriptor) => {
                let call_fn = descriptor.call;
                (
                    descriptor.name.to_string(),
                    Box::new(move |call| {
                        let ToolCall { args, ctx, .. } = call;
                        Box::pin(async move {
                            tool_failure_as_reply(
                                call_fn(ctx, args).await.map(ToolReply::Structured),
                            )
                        })
                    }),
                )
            }
            ToolTarget::Host(tool) => {
                let marker = tool.call_marker();
                let McpHostTool { name, services, .. } = *tool;
                let Some(source) = &self.host_tools else {
                    // A host tool only exists in a source's list.
                    return (None, Err(ToolInvocationError::ToolNotFound(name)));
                };
                if let Err(err) = request.try_insert(marker) {
                    return (
                        None,
                        Err(ToolError::Other(format!("request services: {err}")).into()),
                    );
                }
                // After the call marker, so an entry cannot replace it.
                if let Err(err) = request.try_extend(services) {
                    return (
                        None,
                        Err(ToolError::Other(format!("host tool services: {err}")).into()),
                    );
                }
                let source = Arc::clone(source);
                (
                    name,
                    Box::new(move |call| {
                        Box::pin(async move { tool_failure_as_reply(source.call(call).await) })
                    }),
                )
            }
        };
        let ctx = match self.ctx_for_request(caller, &auth, request) {
            Ok(ctx) => ctx,
            Err(err) => return (None, Err(err.into())),
        };
        // Clones share the judged key, so this one reads what the gate
        // recorded on the context the behaviors receive.
        let observer = ctx.clone();
        let reply = async {
            // Validate once for every transport before a behavior can log
            // arguments or a tool can pass them to storage.
            reject_nul_in_args(&args)?;
            let reply = self
                .dispatch_through_behaviors(name.clone(), args, ctx, terminal)
                .await?;
            Ok(checked_reply(&name, declares_output_schema, reply)?)
        }
        .await;
        (observer.judged(), reply)
    }

    /// # Errors
    ///
    /// Returns `NotAuthorized` or the resource body error.
    pub async fn read_resource(
        &self,
        uri: &str,
        caller: ToolCaller,
        auth: Option<McpAuthContext>,
    ) -> Result<serde_json::Value, ToolInvocationError> {
        self.read_resource_in_request(uri, caller, auth, SpiiceServices::default())
            .await
    }

    /// [`Self::read_resource`] with a per-request service set merged onto
    /// the boot set, so request behaviors see the same values on both paths.
    ///
    /// # Errors
    ///
    /// As [`Self::read_resource`] and [`Self::ctx_for_request`]; a resource
    /// is JSON, so a request behavior that answers anything but
    /// [`ToolReply::Structured`] makes it an internal error.
    pub async fn read_resource_in_request(
        &self,
        uri: &str,
        caller: ToolCaller,
        auth: Option<McpAuthContext>,
        request: SpiiceServices,
    ) -> Result<serde_json::Value, ToolInvocationError> {
        let parsed = parse_resource_uri(uri).map_err(|err| err.into_invocation_error(uri))?;
        let auth =
            auth.ok_or_else(|| ToolInvocationError::NotAuthorized(parsed.scope_key().to_string()))?;
        let ctx = self.ctx_for_request(caller, &auth, request)?;
        let scope_key = parsed.scope_key();

        let terminal: TerminalDispatch<'_> = Box::new(move |call| {
            Box::pin(async move {
                dispatch_resource(parsed, call.ctx)
                    .await
                    .map(ToolReply::Structured)
            })
        });
        let reply = self
            .dispatch_through_behaviors(
                scope_key.to_string(),
                serde_json::json!({ "uri": uri }),
                ctx,
                terminal,
            )
            .await?;
        Ok(json_reply(scope_key, reply)?)
    }

    /// Run `terminal` inside the registry's `RequestBehavior` onion (scope
    /// gate first), as `call_tool` and `read_resource` do. `name` is the
    /// scope key the gate judges; `ctx` comes from [`Self::ctx_for_request`].
    ///
    /// # Errors
    ///
    /// A behavior's refusal or the terminal's error.
    pub async fn dispatch_through_behaviors<'a>(
        &'a self,
        name: String,
        args: serde_json::Value,
        ctx: ToolCtx,
        terminal: TerminalDispatch<'a>,
    ) -> Result<ToolReply, ToolInvocationError> {
        Next::new(self.registry.request_behaviors(), terminal)
            .run(ToolCall { name, args, ctx })
            .await
            .map_err(Into::into)
    }
}

/// What a `tools/call` name resolved to.
pub(crate) enum ToolTarget<'a> {
    Registry(&'a McpToolDescriptor),
    Host(Box<McpHostTool>),
}

impl ToolTarget<'_> {
    /// Every registry tool declares an output schema; a host tool may not.
    const fn declares_output_schema(&self) -> bool {
        match self {
            Self::Registry(_) => true,
            Self::Host(tool) => tool.output_schema.is_some(),
        }
    }
}

/// A tool's own failure ([`ToolError::Failure`]) is a result the caller
/// reads, `isError: true` with the tool's text unchanged; every other error
/// stays a protocol error. Applied at the terminal, so the behaviors see the
/// failure as the reply it is.
fn tool_failure_as_reply(result: Result<ToolReply, ToolError>) -> Result<ToolReply, ToolError> {
    match result {
        Err(ToolError::Failure(text)) => Ok(ToolReply::Failure(vec![ToolContent::Text(text)])),
        other => other,
    }
}

/// The final reply of a tool call, checked where it leaves the behaviors: a
/// structured answer is a JSON object, and only a tool that declared no
/// output schema answers content or a structured failure, which a
/// schema-validating client could not check. A plain failure carries content
/// by definition and is not held to either.
fn checked_reply(
    tool: &str,
    declares_output_schema: bool,
    reply: ToolReply,
) -> Result<ToolReply, ToolError> {
    match &reply {
        ToolReply::Structured(output) if !output.is_object() => {
            tracing::error!(tool = %tool, "tool output after request behaviors must be a JSON object");
            Err(ToolError::Other("tool output must be a JSON object".into()))
        }
        ToolReply::Content(_) if declares_output_schema => {
            tracing::error!(tool = %tool, "a tool that declares an output schema must answer structured content");
            Err(ToolError::Other(
                "tool output must be structured content".into(),
            ))
        }
        ToolReply::StructuredFailure { .. } if declares_output_schema => {
            tracing::error!(tool = %tool, "a tool that declares an output schema must not answer a structured failure");
            Err(ToolError::Other(
                "a structured failure needs a tool without an output schema".into(),
            ))
        }
        ToolReply::Structured(_)
        | ToolReply::Content(_)
        | ToolReply::Failure(_)
        | ToolReply::StructuredFailure { .. } => Ok(reply),
    }
}

/// The JSON of a reply for a surface that carries nothing else (a resource
/// read, the REST surface, `CoreMcpTools`). A failure of one text block, the
/// shape [`tool_failure_as_reply`] builds, is that tool failure again, its
/// text unchanged.
fn json_reply(name: &str, reply: ToolReply) -> Result<serde_json::Value, ToolError> {
    let not_json = || {
        tracing::error!(tool = %name, "this surface carries JSON; the tool answered content");
        ToolError::Other("tool reply must be a JSON object".into())
    };
    match reply.into_structured() {
        Ok(value) => Ok(value),
        Err(ToolReply::Failure(content)) => match <[ToolContent; 1]>::try_from(content) {
            Ok([ToolContent::Text(text)]) => Err(ToolError::Failure(text)),
            _ => Err(not_json()),
        },
        Err(_) => Err(not_json()),
    }
}

fn host_tool_name_refusal(
    registry: &SpiiceRegistryFrozen,
    name: &str,
    served: &[McpHostTool],
) -> Option<&'static str> {
    // A host tool is flat: its palette key is `ScopeKey::Tool(name)`, and a
    // `ToolName` is what makes that so. The grammar has no `:`, so the name
    // is never the spelling of a `tool:action` leaf or a `resource:` key.
    if name.chars().count() > MAX_HOST_TOOL_NAME_CHARS
        || proxiima_core::ToolName::parse(name).is_err()
    {
        return Some("a host tool name is 1..=128 characters of [A-Za-z0-9_.-]");
    }
    if registry.list_mcp_tools().iter().any(|descriptor| {
        descriptor.name == name || proxiima_core::provider_safe_tool_name(descriptor.name) == name
    }) {
        return Some("a registry tool serves this name");
    }
    if served.iter().any(|tool| tool.name == name) {
        return Some("the host listed this name twice");
    }
    None
}

/// Reject NUL in the entire argument tree before behaviors or tool code.
/// `PostgreSQL` text cannot store U+0000, though JSON can encode it. Treat it
/// as the existing caller-input error instead of a later database fault.
/// Rejection preserves the request; silently stripping the character would
/// execute a different query. The same rule applies to object keys.
///
/// # Errors
///
/// [`ToolError::InvalidInput`] naming whether a value or a key held NUL.
pub fn reject_nul_in_args(args: &serde_json::Value) -> Result<(), ToolError> {
    // An explicit worklist keeps the validator stack-safe independently of
    // serde_json's parser depth limit, including direct host callers.
    let mut stack = vec![args];
    while let Some(value) = stack.pop() {
        match value {
            serde_json::Value::String(text) => {
                if text.contains('\0') {
                    return Err(ToolError::InvalidInput(
                        "arguments must not contain NUL (U+0000)".to_string(),
                    ));
                }
            }
            serde_json::Value::Array(items) => stack.extend(items.iter()),
            serde_json::Value::Object(map) => {
                for (key, item) in map {
                    if key.contains('\0') {
                        return Err(ToolError::InvalidInput(
                            "argument names must not contain NUL (U+0000)".to_string(),
                        ));
                    }
                    stack.push(item);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }
    Ok(())
}

async fn dispatch_resource(
    parsed: ParsedResource,
    ctx: ToolCtx,
) -> Result<serde_json::Value, ToolError> {
    match parsed {
        ParsedResource::Schemas(args) => resource_output_value(list_schemas(ctx, args).await?),
        ParsedResource::Schema {
            schema_id,
            schema_version,
        } => resource_output_value(get_schema(ctx, &schema_id, schema_version).await?),
        ParsedResource::Tools(args) => {
            resource_output_value(list_substrate_tools(ctx, args).await?)
        }
        ParsedResource::Graph(args) => resource_output_value(get_graph(ctx, args).await?),
        ParsedResource::Memory(args) => resource_output_value(get_memory(ctx, args).await?),
        ParsedResource::Memories(args) => resource_output_value(get_memories(ctx, args).await?),
        ParsedResource::MemoryCounts => resource_output_value(memory_counts(ctx).await?),
        ParsedResource::MemoryLineage(args) => {
            resource_output_value(walk_memory_lineage(ctx, args).await?)
        }
        ParsedResource::Goals(args) => resource_output_value(list_goals(ctx, args).await?),
        ParsedResource::Goal(reference) => resource_output_value(get_goal(ctx, &reference).await?),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolInvocationError {
    #[error("tool not authorized: {0}")]
    NotAuthorized(String),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("tool error: {0}")]
    Tool(ToolError),
}

impl From<ToolError> for ToolInvocationError {
    fn from(err: ToolError) -> Self {
        match err {
            ToolError::NotAuthorized(tool) => Self::NotAuthorized(tool),
            err => Self::Tool(err),
        }
    }
}

impl ToolInvocationError {
    #[must_use]
    pub fn kind(&self) -> ToolErrorKind {
        match self {
            Self::NotAuthorized(_) => ToolErrorKind::InvalidRequest,
            Self::ToolNotFound(_) => ToolErrorKind::InvalidInput,
            Self::Tool(inner) => inner.kind(),
        }
    }
}

/// Why a `proxiima://` URI failed to parse into a resource read. Each
/// failure keeps its shape instead of collapsing into a generic "unknown
/// resource": an unmatched path is a JSON-RPC `resource_not_found`, while
/// a bad or missing query parameter on a known template is an
/// `invalid_params` naming the parameter.
#[derive(Debug, PartialEq, Eq)]
enum ResourceUriError {
    /// The URI matches no resource template.
    UnknownPath,
    /// A query pair without a `=` separator.
    MalformedQueryPair { pair: String },
    /// A required query parameter is absent or empty.
    MissingParam { param: &'static str },
    /// A query parameter failed to parse.
    InvalidParam {
        param: String,
        value: String,
        expected: String,
    },
    /// The query as a whole does not fit the resource's arguments (a
    /// repeated parameter, or one the path already supplies).
    InvalidQuery(String),
}

impl std::fmt::Display for ResourceUriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ResourceUriError {}

/// Lets core's argument types report their own deserialization failures
/// (a repeated field) through the resource error.
impl serde::de::Error for ResourceUriError {
    fn custom<T: std::fmt::Display>(message: T) -> Self {
        Self::InvalidQuery(message.to_string())
    }
}

impl ResourceUriError {
    fn into_invocation_error(self, uri: &str) -> ToolInvocationError {
        let invalid = |message: String| ToolInvocationError::Tool(ToolError::InvalidInput(message));
        match self {
            Self::UnknownPath => ToolInvocationError::ToolNotFound(uri.to_string()),
            Self::MalformedQueryPair { pair } => invalid(format!(
                "resource {uri}: malformed query parameter '{pair}': expected key=value"
            )),
            Self::MissingParam { param } => invalid(format!(
                "resource {uri}: missing required parameter `{param}`"
            )),
            Self::InvalidParam {
                param,
                value,
                expected,
            } => invalid(format!(
                "resource {uri}: invalid parameter `{param}`: expected {expected}, got '{value}'"
            )),
            Self::InvalidQuery(reason) => {
                invalid(format!("resource {uri}: invalid query: {reason}"))
            }
        }
    }
}

#[derive(Debug)]
enum ParsedResource {
    Schemas(ListSchemasArgs),
    Schema {
        schema_id: String,
        schema_version: u32,
    },
    Tools(ListSubstrateToolsArgs),
    Graph(GetGraphArgs),
    Memory(GetMemoryArgs),
    Memories(GetMemoriesArgs),
    MemoryCounts,
    MemoryLineage(WalkMemoryLineageArgs),
    Goals(ListGoalsArgs),
    Goal(String),
}

impl ParsedResource {
    const fn scope_key(&self) -> &'static str {
        match self {
            Self::Schemas(_) => protocol_resource::SCHEMAS,
            Self::Schema { .. } => protocol_resource::SCHEMA,
            Self::Tools(_) => protocol_resource::TOOLS,
            Self::Graph(_) => protocol_resource::GRAPH,
            Self::Memory(_) => protocol_resource::MEMORY,
            Self::Memories(_) => protocol_resource::MEMORIES,
            Self::MemoryCounts => protocol_resource::MEMORY_COUNTS,
            Self::MemoryLineage(_) => protocol_resource::MEMORY_LINEAGE,
            Self::Goals(_) => protocol_resource::GOALS,
            Self::Goal(_) => protocol_resource::GOAL,
        }
    }
}

/// The dispatch paths, read out of spiice #0's declaration at compile time.
///
/// A `match` arm needs a constant, so the paths are named here — but each
/// name is a projection of the same `ResourceContract` that supplies the
/// advertised URI template and the palette key, not a second table that has
/// to be kept in step with them.
mod resource_path {
    use super::{core_resource, protocol_resource};

    pub const SCHEMAS: &str = core_resource(protocol_resource::SCHEMAS).path;
    pub const SCHEMA: &str = core_resource(protocol_resource::SCHEMA).path;
    pub const TOOLS: &str = core_resource(protocol_resource::TOOLS).path;
    pub const GRAPH: &str = core_resource(protocol_resource::GRAPH).path;
    pub const MEMORIES: &str = core_resource(protocol_resource::MEMORIES).path;
    /// The segment after [`MEMORIES`] that names the counts resource, which
    /// shares its path key; pinned to the contract's template by
    /// `memory_counts_tail_is_the_contract_template`.
    pub const MEMORY_COUNTS_TAIL: &str = "counts";
    pub const MEMORY: &str = core_resource(protocol_resource::MEMORY).path;
    pub const GOALS: &str = core_resource(protocol_resource::GOALS).path;
    pub const GOAL: &str = core_resource(protocol_resource::GOAL).path;

    /// Everything after `<declared path>/`, or `None` when `path` is not
    /// that resource. The id-bearing resources match by prefix rather than
    /// by equality, and a declared `path` is a path — the separator belongs
    /// to the parser, not to the declaration.
    pub fn tail<'a>(path: &'a str, resource: &str) -> Option<&'a str> {
        path.strip_prefix(resource)
            .and_then(|rest| rest.strip_prefix('/'))
    }
}

fn parse_resource_uri(uri: &str) -> Result<ParsedResource, ResourceUriError> {
    let rest = uri
        .strip_prefix("proxiima://")
        .ok_or(ResourceUriError::UnknownPath)?;
    let (path, query) = rest
        .split_once('?')
        .map_or((rest, None), |(path, query)| (path, Some(query)));
    let query = parse_query(query)?;

    match path {
        resource_path::SCHEMAS => Ok(ParsedResource::Schemas(ListSchemasArgs {
            kind: query_parse(
                &query,
                "kind",
                "Fact, Abstraction, Perspective, Goal, or CitedObject",
            )?,
        })),
        resource_path::TOOLS => Ok(ParsedResource::Tools(ListSubstrateToolsArgs {})),
        resource_path::GRAPH => Ok(ParsedResource::Graph(GetGraphArgs {})),
        resource_path::MEMORIES => {
            let ids = query_value(&query, "ids")
                .filter(|ids| !ids.is_empty())
                .ok_or(ResourceUriError::MissingParam { param: "ids" })?;
            Ok(ParsedResource::Memories(GetMemoriesArgs {
                memories: ids.split(',').map(ToOwned::to_owned).collect(),
            }))
        }
        path if resource_path::tail(path, resource_path::MEMORIES)
            == Some(resource_path::MEMORY_COUNTS_TAIL) =>
        {
            Ok(ParsedResource::MemoryCounts)
        }
        resource_path::GOALS => Ok(ParsedResource::Goals(ListGoalsArgs {
            state: query_value(&query, "state").map(ToOwned::to_owned),
            limit: query_parse(&query, "limit", "a non-negative integer")?,
            cursor: query_value(&query, "cursor").map(ToOwned::to_owned),
        })),
        path => parse_id_resource(path, &query),
    }
}

/// The resources whose path carries an id after `<declared path>/`.
fn parse_id_resource(
    path: &str,
    query: &[(&str, &str)],
) -> Result<ParsedResource, ResourceUriError> {
    // A schema id carries its own `/` separators (`core/note`), so the
    // version is split off the END of the tail, not the start.
    if let Some(tail) = resource_path::tail(path, resource_path::SCHEMA) {
        let (schema_id, version) = tail
            .rsplit_once('/')
            .ok_or(ResourceUriError::MissingParam {
                param: "schema_version",
            })?;
        if schema_id.is_empty() {
            return Err(ResourceUriError::UnknownPath);
        }
        let schema_version =
            version
                .parse::<u32>()
                .map_err(|_| ResourceUriError::InvalidParam {
                    param: "schema_version".into(),
                    value: version.to_string(),
                    expected: "a non-negative integer".into(),
                })?;
        return Ok(ParsedResource::Schema {
            schema_id: schema_id.to_string(),
            schema_version,
        });
    }
    if let Some(rest) = resource_path::tail(path, resource_path::MEMORY) {
        return parse_memory_resource_path(rest, query);
    }
    if let Some(id) = resource_path::tail(path, resource_path::GOAL) {
        if id.is_empty() || id.contains('/') {
            return Err(ResourceUriError::UnknownPath);
        }
        return Ok(ParsedResource::Goal(id.to_string()));
    }
    Err(ResourceUriError::UnknownPath)
}

/// `rest` is the path after `<memory path>/`.
fn parse_memory_resource_path(
    rest: &str,
    query: &[(&str, &str)],
) -> Result<ParsedResource, ResourceUriError> {
    if let Some(id) = rest.strip_suffix("/lineage") {
        if id.is_empty() || id.contains('/') {
            return Err(ResourceUriError::UnknownPath);
        }
        return query_args(id, query).map(ParsedResource::MemoryLineage);
    }
    if rest.is_empty() || rest.contains('/') {
        return Err(ResourceUriError::UnknownPath);
    }
    query_args(rest, query).map(ParsedResource::Memory)
}

fn parse_query(query: Option<&str>) -> Result<Vec<(&str, &str)>, ResourceUriError> {
    let Some(query) = query else {
        return Ok(Vec::new());
    };
    if query.is_empty() {
        return Ok(Vec::new());
    }
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            pair.split_once('=')
                .ok_or_else(|| ResourceUriError::MalformedQueryPair {
                    pair: pair.to_string(),
                })
        })
        .collect()
}

fn query_value<'a>(query: &'a [(&str, &str)], key: &str) -> Option<&'a str> {
    query
        .iter()
        .find_map(|(candidate, value)| (*candidate == key).then_some(*value))
}

fn query_parse<T>(
    query: &[(&str, &str)],
    key: &'static str,
    expected: &'static str,
) -> Result<Option<T>, ResourceUriError>
where
    T: std::str::FromStr,
{
    query_value(query, key).map_or(Ok(None), |value| {
        value
            .parse::<T>()
            .map(Some)
            .map_err(|_| ResourceUriError::InvalidParam {
                param: key.into(),
                value: value.to_string(),
                expected: expected.into(),
            })
    })
}

/// The arguments of a memory resource: its id from the path, then the query
/// pairs, deserialized into core's own argument type. Core owns the fields,
/// their defaults and enum spellings, so `resources/read` and `tools/call`
/// cannot disagree about any of them.
fn query_args<T: serde::de::DeserializeOwned>(
    memory: &str,
    query: &[(&str, &str)],
) -> Result<T, ResourceUriError> {
    let pairs = std::iter::once(("memory", memory))
        .chain(query.iter().copied())
        .map(|(key, value)| (key, QueryValue { key, value }));
    T::deserialize(serde::de::value::MapDeserializer::new(pairs))
}

/// One query value, read as the type the field asks for: a number is parsed,
/// a boolean is `true` or `false`, an enum names one of its variants. A value
/// of the wrong shape is an [`ResourceUriError::InvalidParam`] naming the
/// parameter, as every other bad parameter is.
struct QueryValue<'a> {
    key: &'a str,
    value: &'a str,
}

impl QueryValue<'_> {
    fn invalid(&self, expected: impl Into<String>) -> ResourceUriError {
        ResourceUriError::InvalidParam {
            param: self.key.to_owned(),
            value: self.value.to_owned(),
            expected: expected.into(),
        }
    }
}

impl serde::de::IntoDeserializer<'_, ResourceUriError> for QueryValue<'_> {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

impl<'de> serde::de::Deserializer<'de> for QueryValue<'_> {
    type Error = ResourceUriError;

    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_str(self.value)
    }

    fn deserialize_u32<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        let number = self
            .value
            .parse()
            .map_err(|_| self.invalid("a non-negative integer"))?;
        visitor.visit_u32(number)
    }

    fn deserialize_bool<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.value {
            "true" => visitor.visit_bool(true),
            "false" => visitor.visit_bool(false),
            _ => Err(self.invalid("`true` or `false`")),
        }
    }

    fn deserialize_option<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_some(self)
    }

    fn deserialize_enum<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if !variants.contains(&self.value) {
            return Err(self.invalid(format!("one of {}", variants.join(", "))));
        }
        visitor.visit_enum(serde::de::IntoDeserializer::into_deserializer(self.value))
    }

    serde::forward_to_deserialize_any! {
        i8 i16 i32 i64 i128 u8 u16 u64 u128 f32 f64 char str string bytes
        byte_buf unit unit_struct newtype_struct seq tuple tuple_struct map
        struct identifier ignored_any
    }
}

fn resource_output_value<T>(output: T) -> Result<serde_json::Value, ToolError>
where
    T: Serialize,
{
    serde_json::to_value(output)
        .map_err(|err| ToolError::Other(format!("serialize resource output: {err}")))
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
