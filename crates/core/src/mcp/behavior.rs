use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;

use super::core_tools::memory_spaces::{ResolvedMemorySpace, SpaceDefault, resolve_space_owner};
use super::{
    KeyDeclaration, McpToolCtx, McpToolDescriptor, McpToolError, OwnerNeed, Replay, ToolEffect,
    ToolReply,
};
use crate::{ActionName, OwnerAccess, ScopeKey};

#[derive(Debug)]
pub struct ToolCall {
    pub name: String,
    pub args: serde_json::Value,
    pub ctx: McpToolCtx,
}

pub type TerminalDispatch<'a> =
    Box<dyn FnOnce(ToolCall) -> BoxFuture<'a, Result<ToolReply, McpToolError>> + Send + 'a>;

pub struct Next<'a> {
    rest: &'a [Arc<dyn RequestBehavior>],
    terminal: Option<TerminalDispatch<'a>>,
}

impl std::fmt::Debug for Next<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Next")
            .field("rest_len", &self.rest.len())
            .field("has_terminal", &self.terminal.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> Next<'a> {
    #[must_use]
    pub fn new(rest: &'a [Arc<dyn RequestBehavior>], terminal: TerminalDispatch<'a>) -> Self {
        Self {
            rest,
            terminal: Some(terminal),
        }
    }

    /// # Errors
    ///
    /// Returns a behavior or terminal tool error.
    pub async fn run(mut self, call: ToolCall) -> Result<ToolReply, McpToolError> {
        if let Some((head, tail)) = self.rest.split_first() {
            head.handle(
                call,
                Self {
                    rest: tail,
                    terminal: self.terminal.take(),
                },
            )
            .await
        } else {
            let Some(terminal) = self.terminal.take() else {
                return Err(McpToolError::Other("terminal dispatch missing".into()));
            };
            terminal(call).await
        }
    }
}

#[async_trait]
pub trait RequestBehavior: Send + Sync + std::fmt::Debug {
    /// Wrap one call: refuse it, or pass it to `next` and post-process the
    /// reply. A behavior that only reads JSON matches
    /// [`ToolReply::Structured`] and returns the other variants unchanged.
    /// A tool that ran and failed reaches it as `Ok` with
    /// [`ToolReply::Failure`] or [`ToolReply::StructuredFailure`], never as
    /// `Err`.
    ///
    /// # Errors
    ///
    /// The behavior's own refusal, or the error `next` returned.
    async fn handle(&self, call: ToolCall, next: Next<'_>) -> Result<ToolReply, McpToolError>;

    /// Whether `ctx`'s caller may see `tool` in a listing: `tools/list`, the
    /// REST catalog and `OpenAPI` document, the generated instructions and
    /// `core/list_substrate_tools`. Default: yes.
    ///
    /// Only narrows. A tool is listed when the palette, the owner role and
    /// every behavior allow it, so `true` never lists a tool the other
    /// filters hide, and a dispatcher is listed when one of its actions is
    /// visible. Only a listing asks: the call path never does, so a
    /// behavior that hides a tool still refuses its call in [`Self::handle`],
    /// where the arguments are known.
    fn visible(&self, _ctx: &McpToolCtx, _tool: &ToolDescriptorView<'_>) -> bool {
        true
    }
}

/// Which catalog serves a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    /// The frozen spiice registry (substrate and spiice tools).
    Registry,
    /// A host's `McpHostTools`.
    Host,
}

/// What [`RequestBehavior::visible`] judges: one tool, or one action of a
/// registry dispatcher, borrowed from the descriptor that is being listed.
/// Only the transport builds one, from a descriptor it is listing.
///
/// ```compile_fail,E0451
/// let _view = proxiima_core::ToolDescriptorView {
///     name: "forged",
///     description: "",
///     effect: proxiima_core::ToolEffect::ReadOnly,
///     source: proxiima_core::ToolSource::Host,
///     action: None,
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolDescriptorView<'a> {
    name: &'a str,
    description: &'a str,
    effect: ToolEffect,
    source: ToolSource,
    action: Option<&'a str>,
}

impl<'a> ToolDescriptorView<'a> {
    /// A registry tool; `action` is the dispatcher action under test, `None`
    /// for a flat tool. The effect is the action's declaration, the flat
    /// tool's, or (a tool that declared none, which a frozen registry never
    /// holds) the strongest write, as the owner-role gate reads silence.
    #[must_use]
    pub fn registry(descriptor: &'a McpToolDescriptor, action: Option<&'a str>) -> Self {
        let effect = match action {
            Some(action) => descriptor.action_effect(action),
            None => descriptor.effect(),
        };
        Self {
            name: descriptor.name,
            description: descriptor.description,
            effect: effect.unwrap_or(ToolEffect::Destructive(Replay::NonIdempotent)),
            source: ToolSource::Registry,
            action,
        }
    }

    /// A host tool, which is flat.
    #[must_use]
    pub const fn host(name: &'a str, description: &'a str, effect: ToolEffect) -> Self {
        Self {
            name,
            description,
            effect,
            source: ToolSource::Host,
            action: None,
        }
    }

    /// The tool's canonical name.
    #[must_use]
    pub const fn name(&self) -> &'a str {
        self.name
    }

    #[must_use]
    pub const fn description(&self) -> &'a str {
        self.description
    }

    /// What the tool, or the action under test, does.
    #[must_use]
    pub const fn effect(&self) -> ToolEffect {
        self.effect
    }

    #[must_use]
    pub const fn source(&self) -> ToolSource {
        self.source
    }

    /// The dispatcher action under test; `None` for a flat tool.
    #[must_use]
    pub const fn action(&self) -> Option<&'a str> {
        self.action
    }
}

impl McpToolCtx {
    /// Whether every request behavior of this context's registry lets the
    /// caller see `tool` ([`RequestBehavior::visible`]).
    #[must_use]
    pub fn behaviors_show(&self, tool: &ToolDescriptorView<'_>) -> bool {
        self.registry
            .request_behaviors()
            .iter()
            .all(|behavior| behavior.visible(self, tool))
    }
}

/// Marks one call as a host-served tool (not in the spiice registry) and
/// carries what the host declared about it.
///
/// The MCP tool host places it in the call's request services
/// ([`McpToolCtx::services`]) when it dispatches a host tool, so every
/// behavior sees which kind of call it wraps and the scope gate classifies
/// the call from the host's declaration instead of treating the unknown
/// name as a write. It names the tool it describes; the gate reads it only
/// for that name and only when the registry has no tool of that name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHostToolCall {
    name: String,
    effect: ToolEffect,
    need: OwnerNeed,
}

impl McpHostToolCall {
    #[must_use]
    pub fn new(name: impl Into<String>, effect: ToolEffect) -> Self {
        Self {
            name: name.into(),
            effect,
            need: OwnerNeed::Memory,
        }
    }

    /// Declare the tool's writes [Goal work](OwnerNeed::GoalWork).
    #[must_use]
    pub const fn with_goal_work(mut self) -> Self {
        self.need = OwnerNeed::GoalWork;
        self
    }

    /// What the host declared, for every audience: the host lists its tools
    /// per caller.
    #[must_use]
    pub const fn declaration(&self) -> KeyDeclaration {
        KeyDeclaration {
            need: self.need,
            ..KeyDeclaration::shared(Some(self.effect))
        }
    }

    /// What a call of this tool asks of the owner it selects.
    #[must_use]
    pub const fn owner_access(&self) -> OwnerAccess {
        self.declaration().owner_access()
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn effect(&self) -> ToolEffect {
        self.effect
    }
}

#[derive(Debug, Default)]
pub struct ScopeGateBehavior;

impl ScopeGateBehavior {
    /// Admit or refuse one call; an admitted call answers what it asks of
    /// its owner.
    fn enforce_scope(
        tool: &str,
        args: &serde_json::Value,
        ctx: &McpToolCtx,
    ) -> Result<OwnerAccess, McpToolError> {
        ctx.judged.clear();
        let capabilities = ctx.authz.capabilities();
        // The request name is parsed once, here. A name that is no key is in
        // no palette (`All` holds everything, as ever), and every comparison
        // below is between keys.
        let key = ScopeKey::parse(tool);
        // Whether a tool dispatches actions is read off its descriptor, not
        // off the substrate `CoreActionMeta` tables: a spiice dispatcher is
        // absent from those tables, so keying on them would drop it to the
        // whole-tool branch below — a palette holding only
        // `spiice_tool:action` would deny the tool outright, and an unknown
        // action would sail past this gate into the tool.
        let descriptor = ctx.registry.mcp_tool(tool);
        if descriptor.is_some_and(|descriptor| !descriptor.advertised_by(capabilities)) {
            // Refused as a whole, so the key it was judged by is the tool's.
            if let Ok(key) = key {
                ctx.judged.record(key);
            }
            return Err(McpToolError::NotAuthorized(tool.to_string()));
        }
        // Every branch ends with the declaration of exactly the key it judged
        // — a dispatcher's action, a flat tool's, a host tool's, a core
        // resource's — and the owner-role gate reads that same value. Each
        // records that key on the context before it decides on it, so a
        // refusal carries the key too.
        let access = match descriptor {
            Some(descriptor) if !descriptor.action_arg_specs.is_empty() => {
                // A registered tool's name is a `ToolName`: `try_freeze` checked.
                let Ok(ScopeKey::Tool(tool_name)) = &key else {
                    return Err(McpToolError::NotAuthorized(tool.to_string()));
                };
                let requested = args.get("action").and_then(serde_json::Value::as_str);
                let declared = descriptor.declaration(requested);
                let Some(action) = requested else {
                    return Err(McpToolError::InvalidInput(format!(
                        "tool {tool} requires string action"
                    )));
                };
                // A declared action is an `ActionName` (also checked at
                // freeze), so text that is none is no declared action.
                let known = ActionName::parse(action).ok().filter(|name| {
                    descriptor
                        .action_arg_specs
                        .iter()
                        .any(|spec| spec.action == name.as_str())
                });
                let Some(action) = known else {
                    return Err(McpToolError::InvalidInput(format!(
                        "unknown action {action:?} for tool {tool}"
                    )));
                };
                Self::record_action(tool_name, &action, ctx)?;
                if !capabilities.admits_action(tool_name, &action, declared) {
                    return Err(McpToolError::NotAuthorized(format!("{tool}:{action}")));
                }
                declared.owner_access()
            }
            Some(descriptor) if !descriptor.argv_action_specs.is_empty() => {
                let Ok(ScopeKey::Tool(tool_name)) = &key else {
                    return Err(McpToolError::NotAuthorized(tool.to_string()));
                };
                // The same resolution the terminal dispatch runs, once, so
                // the gate and the call share one vocabulary: argv matching
                // no declared prefix is undeclared here and refused below
                // rather than sailing past into the tool.
                let resolved = super::resolve_argv_action(tool, descriptor.argv_action_specs, args);
                let declared = descriptor.declaration(resolved.as_ref().ok().copied());
                let action = resolved?;
                let Ok(action_name) = ActionName::parse(action) else {
                    return Err(McpToolError::NotAuthorized(format!("{tool}:{action}")));
                };
                Self::record_action(tool_name, &action_name, ctx)?;
                if !capabilities.admits_action(tool_name, &action_name, declared) {
                    return Err(McpToolError::NotAuthorized(format!("{tool}:{action}")));
                }
                declared.owner_access()
            }
            _ => {
                let declared = Self::flat_declaration(tool, key.as_ref().ok(), descriptor, ctx);
                if let Ok(key) = &key {
                    ctx.judged.record(key.clone());
                }
                let allowed = match &key {
                    Ok(key) => capabilities.admits(key, declared),
                    Err(_) => {
                        matches!(capabilities.tool_scope, crate::ToolScope::All)
                            && matches!(capabilities.served_surface, crate::ServedSurface::All)
                    }
                };
                if !allowed {
                    return Err(McpToolError::NotAuthorized(tool.to_string()));
                }
                declared.owner_access()
            }
        };
        let owner = Self::resolve_scope_owner(tool, args, ctx)?;
        Self::enforce_owner_role(tool, access, owner.owner, ctx)?;
        Ok(access)
    }

    /// Read the host read set's other-store reads (#677) through the
    /// other-stores port as the caller, the way a cross-store derive reads
    /// its sources, before a call that may write. A memory the caller
    /// cannot read, or any other-store read without a port, refuses the
    /// call; a read-only call reads nothing. Storage refuses a write naming
    /// an other-store read that was not read here.
    async fn read_other_store_reads(
        mut call: ToolCall,
        access: OwnerAccess,
    ) -> Result<ToolCall, McpToolError> {
        let unread = call.ctx.authz.unread_other_store_reads();
        if access == OwnerAccess::Read || unread.is_empty() {
            return Ok(call);
        }
        let read =
            super::core_tools::other_stores::resolve_remote_origins(&call.ctx, &unread).await?;
        call.ctx.authz = call.ctx.authz.with_other_store_reads(read);
        Ok(call)
    }

    /// Record `tool:action` as the key the gate judges this call by.
    fn record_action(
        tool: &crate::ToolName,
        action: &ActionName,
        ctx: &McpToolCtx,
    ) -> Result<(), McpToolError> {
        let key = ScopeKey::action(tool.clone(), action.clone())
            .map_err(|_| McpToolError::NotAuthorized(format!("{tool}:{action}")))?;
        ctx.judged.record(key);
        Ok(())
    }

    /// Resolve the owner selected by a call's optional `space` argument for
    /// the scope gate. The tool's own space resolver remains the source of
    /// truth for the operation; this gate uses the same resolver for the
    /// owner-role decision instead of trusting the bound owner.
    fn resolve_scope_owner(
        tool: &str,
        args: &serde_json::Value,
        ctx: &McpToolCtx,
    ) -> Result<ResolvedMemorySpace, McpToolError> {
        let Some(properties) = Self::space_properties(tool, args, ctx) else {
            return Ok(ResolvedMemorySpace {
                key: "current".into(),
                label: "Current owner".into(),
                owner: ctx.owner,
            });
        };
        let mut normalized = args.clone();
        super::tool::coerce_space_aliases(tool, &mut normalized, &properties)?;
        let raw = match normalized.get("space") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => Some(value.as_str().ok_or_else(|| {
                McpToolError::InvalidInput("space must be a string or null".into())
            })?),
        };
        resolve_space_owner(ctx, raw, SpaceDefault::Current)
    }

    fn space_properties(
        tool: &str,
        args: &serde_json::Value,
        ctx: &McpToolCtx,
    ) -> Option<Vec<String>> {
        let descriptor = ctx.registry.mcp_tool(tool)?;
        let schema = descriptor
            .action_arg_specs
            .is_empty()
            .then_some(&descriptor.args_schema)
            .or_else(|| {
                args.get("action")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|action| descriptor.action_argument_schema(action))
            });
        let properties = schema
            .and_then(|schema| schema.get("properties"))
            .and_then(serde_json::Value::as_object)
            .filter(|properties| properties.contains_key("space"))?;
        Some(properties.keys().cloned().collect())
    }

    /// What a flat key declares: a core resource's contract, a registered
    /// flat tool's declaration, or the host tool's (shared, as the host
    /// lists it per caller). [Undeclared](KeyDeclaration::UNDECLARED) for a
    /// name nothing declares — an unknown `resource:` key is not waved
    /// through as a read on the strength of its prefix.
    fn flat_declaration(
        tool: &str,
        key: Option<&ScopeKey>,
        descriptor: Option<&McpToolDescriptor>,
        ctx: &McpToolCtx,
    ) -> KeyDeclaration {
        match (key, descriptor) {
            (Some(ScopeKey::Resource(resource)), _)
                if *resource == super::how_to_resource_key() =>
            {
                super::how_to_resource_declaration()
            }
            (Some(ScopeKey::Resource(resource)), _) => crate::spiice::SPIICE_0
                .resource_by_scope_key(resource)
                .map_or(KeyDeclaration::UNDECLARED, |resource| {
                    resource.declaration()
                }),
            (_, Some(descriptor)) => descriptor.declaration(None),
            (_, None) => Self::host_call(tool, None, ctx)
                .map_or(KeyDeclaration::UNDECLARED, |host| host.declaration()),
        }
    }

    /// The host's declaration of `tool`: only for a name the registry does
    /// not serve, and only the marker that names it.
    fn host_call(
        tool: &str,
        descriptor: Option<&McpToolDescriptor>,
        ctx: &McpToolCtx,
    ) -> Option<Arc<McpHostToolCall>> {
        if descriptor.is_some() {
            return None;
        }
        ctx.services
            .get::<McpHostToolCall>()
            .filter(|host| host.name == tool)
    }

    /// The owner-role gate: a declared read needs read access to the selected
    /// owner, Goal work [`crate::Role::works_goals`], anything else write
    /// access. Silence is a write: a tool that has not said what it does may
    /// well write, and guessing "read" would hand a viewer a mutation.
    fn enforce_owner_role(
        tool: &str,
        access: OwnerAccess,
        owner: crate::OwnerId,
        ctx: &McpToolCtx,
    ) -> Result<(), McpToolError> {
        if ctx.authz.may_run_tool(&owner, access) {
            Ok(())
        } else {
            Err(McpToolError::NotAuthorized(tool.to_string()))
        }
    }
}

#[async_trait]
impl RequestBehavior for ScopeGateBehavior {
    async fn handle(&self, call: ToolCall, next: Next<'_>) -> Result<ToolReply, McpToolError> {
        let access = Self::enforce_scope(&call.name, &call.args, &call.ctx)?;
        next.run(Self::read_other_store_reads(call, access).await?)
            .await
    }
}

#[cfg(test)]
#[derive(serde::Serialize, schemars::JsonSchema)]
struct StubOutput {}

#[cfg(test)]
#[path = "behavior_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "behavior_argv_scope_tests.rs"]
mod argv_scope_tests;

#[cfg(test)]
mod owner_role_tests {
    use super::ScopeGateBehavior;
    use crate::access::Role;
    use crate::mcp::{McpAuthorContext, McpTool, McpToolCtx, McpToolError, ToolEffect};
    use crate::{AuthPath, AuthzContext, GroupId, OwnerId, SpiiceRegistry, SpiiceServices, UserId};
    use futures::future::BoxFuture;
    use std::sync::Arc;

    #[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
    struct StubArgs {
        #[schemars(description = "anything")]
        _query: String,
    }

    /// A spiice tool that says it only reads.
    #[derive(Debug)]
    struct DeclaredReadTool;

    impl McpTool for DeclaredReadTool {
        const TITLE: &'static str = "Test tool";
        const NAME: &'static str = "proxiima-stub_search";
        const DESCRIPTION: &'static str = "A spiice read tool that declares itself read-only.";
        const EFFECT: Option<ToolEffect> = Some(ToolEffect::ReadOnly);
        type Args = StubArgs;
        type Output = super::StubOutput;
        fn call(
            _: McpToolCtx,
            _: Self::Args,
        ) -> BoxFuture<'static, Result<Self::Output, McpToolError>> {
            Box::pin(async { Ok(super::StubOutput {}) })
        }
    }

    /// A spiice tool that says nothing. `try_freeze` refuses to seal a
    /// registry containing one, so this never reaches a served registry —
    /// see `crate::spiice::freeze`. It stays here to pin that refusal.
    #[derive(Debug)]
    struct SilentTool;

    impl McpTool for SilentTool {
        const TITLE: &'static str = "Test tool";
        const NAME: &'static str = "proxiima-stub_silent";
        const DESCRIPTION: &'static str = "A spiice tool that declares no annotations.";
        type Args = StubArgs;
        type Output = super::StubOutput;
        fn call(
            _: McpToolCtx,
            _: Self::Args,
        ) -> BoxFuture<'static, Result<Self::Output, McpToolError>> {
            Box::pin(async { Ok(super::StubOutput {}) })
        }
    }

    /// A group viewer: read on everything, write on nothing.
    fn viewer_ctx(tool_owner: OwnerId) -> McpToolCtx {
        let subject = UserId::new(uuid::Uuid::now_v7());
        let mut registry = SpiiceRegistry::new();
        registry.add_mcp_tool_or_panic_for_tests::<DeclaredReadTool>("proxiima-stub");
        McpToolCtx {
            owner: tool_owner,
            authz: AuthzContext::for_subject_with_role(
                subject,
                [(tool_owner, Role::viewer())],
                AuthPath::HostBearer,
            ),
            registry: Arc::new(registry.freeze_or_panic_for_tests()),
            author: McpAuthorContext {
                model_id: "t".into(),
                trusted_model_id: None,
                client_name: "t".into(),
                client_version: "0".into(),
                caller_self_perspective: None,
            },
            caller_self_perspective: None,
            services: SpiiceServices::default(),
            engine: None,
            judged: proxiima_core::mcp::JudgedKey::default(),
        }
    }

    /// A read-only role can call a spiice tool that declares itself read-only.
    /// The owner-role gate reads the key's declared effect; a missing
    /// declaration is treated as WRITE.
    #[test]
    fn a_viewer_may_call_a_spiice_tool_that_declares_itself_read_only() {
        let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
        let ctx = viewer_ctx(owner);
        let args = serde_json::json!({"query": "anything"});

        assert!(
            ScopeGateBehavior::enforce_scope("core_search_memories", &args, &ctx,).is_ok(),
            "core's read tool is annotated read-only, so a viewer passes"
        );
        assert!(
            ScopeGateBehavior::enforce_scope(DeclaredReadTool::NAME, &args, &ctx,).is_ok(),
            "a spiice read tool that declares read_only must not be billed as a write"
        );
    }

    /// A registry containing a tool that declares nothing does not seal.
    ///
    /// This is the primary defence, and it fires at boot rather than at the
    /// first refused call. The gate's `unwrap_or(false)` below is the second
    /// one, for a name that never reached the registry at all.
    #[test]
    fn a_spiice_tool_that_declares_nothing_cannot_be_frozen() {
        let mut registry = SpiiceRegistry::new();
        registry.add_mcp_tool_or_panic_for_tests::<DeclaredReadTool>("proxiima-stub");
        registry.add_mcp_tool_or_panic_for_tests::<SilentTool>("proxiima-stub");
        let err = registry.try_freeze().expect_err("silence must not seal");
        assert!(
            matches!(
                err,
                crate::SpiiceRegistryError::UndeclaredToolBehavior {
                    name: "proxiima-stub_silent"
                }
            ),
            "got {err:?}",
        );
        // The message names `EFFECT`; a silent tool is otherwise billed as
        // a write with no stated cause.
        let rendered = err.to_string();
        assert!(rendered.contains("EFFECT"), "{rendered}");
    }

    /// Declaring nothing still means write. The default has to stay
    /// conservative: a tool that has not thought about it may well write,
    /// and guessing "read" would hand a viewer a mutation.
    ///
    /// Freeze now rejects a *registered* silent tool, so the reachable case
    /// is a name the registry never saw — a tool dispatched by a name that
    /// does not match any descriptor.
    #[test]
    fn a_tool_the_registry_never_saw_is_still_a_write() {
        let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
        let ctx = viewer_ctx(owner);
        let args = serde_json::json!({"query": "anything"});
        assert!(
            ScopeGateBehavior::enforce_scope(SilentTool::NAME, &args, &ctx,).is_err(),
            "silence must not be read as a promise to only read"
        );
    }

    /// The owner-role half of the scope gate follows the selected memory
    /// space, rather than the transport-bound owner.
    #[test]
    fn selected_space_owner_role_refuses_when_bound_owner_would_allow() {
        let subject = UserId::new(uuid::Uuid::now_v7());
        let bound_owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
        let selected_owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
        let authz = AuthzContext::for_subject_with_role(
            subject,
            [
                (bound_owner, Role::editor()),
                (selected_owner, Role::viewer()),
            ],
            AuthPath::HostBearer,
        )
        .with_tool_scope(crate::ToolScope::All);
        let ctx = McpToolCtx {
            owner: bound_owner,
            authz,
            registry: Arc::new(SpiiceRegistry::new().freeze_or_panic_for_tests()),
            author: McpAuthorContext {
                model_id: "t".into(),
                trusted_model_id: None,
                client_name: "t".into(),
                client_version: "0".into(),
                caller_self_perspective: None,
            },
            caller_self_perspective: None,
            services: SpiiceServices::default(),
            engine: None,
            judged: proxiima_core::mcp::JudgedKey::default(),
        };

        let err = ScopeGateBehavior::enforce_scope(
            crate::protocol::tool::CORE_REMEMBER,
            &serde_json::json!({
                "title": "t",
                "body": "b",
                "spaces": [selected_owner.to_string()],
            }),
            &ctx,
        )
        .expect_err("viewer on selected space cannot write there");
        assert!(matches!(err, McpToolError::NotAuthorized(_)), "got {err:?}");
    }

    #[test]
    fn selected_space_owner_role_allows_when_bound_owner_would_refuse() {
        let subject = UserId::new(uuid::Uuid::now_v7());
        let bound_owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
        let selected_owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
        let authz = AuthzContext::for_subject_with_role(
            subject,
            [
                (bound_owner, Role::viewer()),
                (selected_owner, Role::editor()),
            ],
            AuthPath::HostBearer,
        )
        .with_tool_scope(crate::ToolScope::All);
        let ctx = McpToolCtx {
            owner: bound_owner,
            authz,
            registry: Arc::new(SpiiceRegistry::new().freeze_or_panic_for_tests()),
            author: McpAuthorContext {
                model_id: "t".into(),
                trusted_model_id: None,
                client_name: "t".into(),
                client_version: "0".into(),
                caller_self_perspective: None,
            },
            caller_self_perspective: None,
            services: SpiiceServices::default(),
            engine: None,
            judged: proxiima_core::mcp::JudgedKey::default(),
        };

        ScopeGateBehavior::enforce_scope(
            crate::protocol::tool::CORE_REMEMBER,
            &serde_json::json!({
                "title": "t",
                "body": "b",
                "space": selected_owner.to_string(),
            }),
            &ctx,
        )
        .expect("editor on selected space can write there");
    }
}

#[cfg(test)]
mod visibility_tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::protocol::tool as protocol_tool;
    use crate::{
        AuthPath, AuthzContext, McpAuthorContext, OwnerId, SpiiceRegistry, SpiiceServices,
        ToolScope, UserId,
    };

    type Asked = Arc<Mutex<Vec<(String, Option<String>, ToolSource, ToolEffect)>>>;

    /// Hides `hidden` (every action of it) and records what it was asked.
    #[derive(Debug)]
    struct Hides {
        hidden: &'static str,
        asked: Asked,
    }

    #[async_trait]
    impl RequestBehavior for Hides {
        async fn handle(&self, call: ToolCall, next: Next<'_>) -> Result<ToolReply, McpToolError> {
            next.run(call).await
        }

        fn visible(&self, _ctx: &McpToolCtx, tool: &ToolDescriptorView<'_>) -> bool {
            self.asked.lock().expect("asked").push((
                tool.name().to_string(),
                tool.action().map(ToString::to_string),
                tool.source(),
                tool.effect(),
            ));
            tool.name() != self.hidden
        }
    }

    /// Passes everything on and keeps the default `visible`.
    #[derive(Debug)]
    struct PassThrough;

    #[async_trait]
    impl RequestBehavior for PassThrough {
        async fn handle(&self, call: ToolCall, next: Next<'_>) -> Result<ToolReply, McpToolError> {
            next.run(call).await
        }
    }

    fn ctx_over(registry: SpiiceRegistry) -> McpToolCtx {
        let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
        McpToolCtx {
            owner,
            authz: AuthzContext::single_owner(&owner, AuthPath::HostBearer)
                .with_tool_scope(ToolScope::All),
            registry: Arc::new(registry.freeze_or_panic_for_tests()),
            author: McpAuthorContext {
                model_id: "test".into(),
                trusted_model_id: None,
                client_name: "test".into(),
                client_version: "0".into(),
                caller_self_perspective: None,
            },
            caller_self_perspective: None,
            services: SpiiceServices::default(),
            engine: None,
            judged: proxiima_core::mcp::JudgedKey::default(),
        }
    }

    #[test]
    fn a_registry_without_a_visibility_opinion_hides_nothing() {
        let mut registry = SpiiceRegistry::new();
        registry.add_request_behavior(PassThrough);
        let ctx = ctx_over(registry);
        for descriptor in ctx.registry.list_mcp_tools() {
            assert!(
                ctx.behaviors_show(&ToolDescriptorView::registry(descriptor, None)),
                "{} hidden by the default",
                descriptor.name
            );
        }
        let host = ToolDescriptorView::host("host_tool", "d", ToolEffect::ReadOnly);
        assert!(ctx.behaviors_show(&host));
    }

    /// One behavior saying no is enough: `visible` is `ANDed`.
    #[test]
    fn any_one_behavior_hiding_a_tool_hides_it() {
        let mut registry = SpiiceRegistry::new();
        registry.add_request_behavior(PassThrough);
        registry.add_request_behavior(Hides {
            hidden: "host_secret",
            asked: Asked::default(),
        });
        let ctx = ctx_over(registry);
        let secret = ToolDescriptorView::host("host_secret", "d", ToolEffect::ReadOnly);
        let other = ToolDescriptorView::host("host_other", "d", ToolEffect::ReadOnly);
        assert!(!ctx.behaviors_show(&secret));
        assert!(ctx.behaviors_show(&other));
    }

    /// A dispatcher is judged per action, with that action's own effect; a
    /// flat tool and a host tool carry no action.
    #[test]
    fn the_view_names_the_action_under_test_and_its_own_effect() {
        let asked = Asked::default();
        let mut registry = SpiiceRegistry::new();
        registry.add_request_behavior(Hides {
            hidden: "nothing_is_called_this",
            asked: Arc::clone(&asked),
        });
        let ctx = ctx_over(registry);
        // A dispatcher whose actions differ in effect: the view must carry the
        // action's own, not the join the tool as a whole declares.
        let (goal, action, effect) = ctx
            .registry
            .list_mcp_tools()
            .iter()
            .find_map(|tool| {
                let joined = tool.effect()?;
                tool.actions()
                    .find(|(_, effect)| *effect != joined)
                    .map(|(action, effect)| (tool, action, effect))
            })
            .expect("a registered dispatcher mixes effects");
        assert!(ctx.behaviors_show(&ToolDescriptorView::registry(goal, Some(action))));
        let search = ctx
            .registry
            .mcp_tool(protocol_tool::CORE_SEARCH_MEMORIES)
            .expect("core_search_memories is registered");
        assert!(ctx.behaviors_show(&ToolDescriptorView::registry(search, None)));
        assert!(ctx.behaviors_show(&ToolDescriptorView::host("h", "d", ToolEffect::ReadOnly)));

        assert_eq!(
            *asked.lock().expect("asked"),
            [
                (
                    goal.name.to_string(),
                    Some(action.to_string()),
                    ToolSource::Registry,
                    effect
                ),
                (
                    protocol_tool::CORE_SEARCH_MEMORIES.to_string(),
                    None,
                    ToolSource::Registry,
                    ToolEffect::ReadOnly
                ),
                (
                    "h".to_string(),
                    None,
                    ToolSource::Host,
                    ToolEffect::ReadOnly
                ),
            ]
        );
    }
}
