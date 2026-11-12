use futures::future::BoxFuture;

use crate::authz::{KeyDeclaration, OwnerNeed, ToolAudience};
use crate::{ActionName, CapabilitySet, ScopeKey, ScopeKeyError, ToolCtx, ToolError, ToolName};

/// The longest title ([`ActionArgSpec::title`], [`ToolDescriptor::title`]),
/// in characters: a label beside a recorded call, not a description.
pub const MAX_TITLE_CHARS: usize = 60;

/// Why `title` is no title, or `None` when it is one: a single line of
/// 1 to [`MAX_TITLE_CHARS`] characters with no edge whitespace.
pub(crate) fn title_problem(title: &str) -> Option<&'static str> {
    if title.is_empty() {
        Some("is empty")
    } else if title.trim() != title {
        Some("has leading or trailing whitespace")
    } else if title.chars().any(char::is_control) {
        Some("spans lines or holds a control character")
    } else if title.chars().count() > MAX_TITLE_CHARS {
        Some("is longer than MAX_TITLE_CHARS")
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOrigin {
    Substrate,
    Spiice(String),
}

/// What a *flat* tool does with a top-level argument key its schema does not
/// declare.
///
/// The default, [`Self::Refuse`], is the strict answer and stays the answer
/// for writes and for anything where a mistyped key changes what the call
/// targets — an owner, a space, a filter. The opt-in exists for read-only
/// tools whose callers are models: a small model routinely copies fields out
/// of one result into the next call (a paging flag, a provenance block, a
/// list it was just handed), and refusing that call spends a round trip to
/// teach it nothing it acts on.
///
/// Never declare [`Self::IgnoreAndReport`] on a tool that writes, or on any
/// tool where a silently dropped key could change what the call targets: the
/// report reaches the caller only after the tool has run, so it documents the
/// drop rather than preventing it. An enum rather than a bool so the
/// declaration site names its meaning and a future policy is a new variant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UnknownFieldPolicy {
    /// Refuse the call, naming every undeclared key. The default: declaring
    /// nothing keeps the strict guard.
    #[default]
    Refuse,
    /// Drop the undeclared keys before decode and name them back in the
    /// result's top-level `ignored_fields` array, so the drop is observable
    /// instead of silent. Nothing is added when the tool's result is not a
    /// JSON object, or when nothing was dropped.
    IgnoreAndReport,
}

#[derive(Clone)]
pub struct ToolDescriptor {
    pub name: &'static str,
    /// The short name a person reads for this tool ("Goals"); see
    /// [`crate::Tool::TITLE`]. [`Self::title_of`] names one key of it.
    pub title: &'static str,
    pub description: &'static str,
    pub origin: ToolOrigin,
    pub produces_schema_ids: &'static [&'static str],
    /// The complete wire `inputSchema`: plain JSON Schema, every action of a
    /// dispatcher rendered. [`Self::input_schema`] narrows it per caller.
    pub args_schema: serde_json::Value,
    /// A tagged-enum dispatcher's per-action contract, which `args_schema`
    /// is rendered from; `None` for a flat or argv-keyed tool.
    pub dispatcher_schema: Option<crate::catalog::DispatcherSchema>,
    /// JSON Schema for the tool's reply envelope, documented. `tools/list`
    /// sends [`mcp_wire_output_schema`](crate::catalog::mcp_wire_output_schema)
    /// of it. `produces_schema_ids` names the registry payloads it writes —
    /// a different thing.
    pub output_schema: serde_json::Value,
    pub action_arg_specs: &'static [ActionArgSpec],
    /// The actions of an argv-keyed dispatcher, or `&[]`. Mutually exclusive
    /// with `action_arg_specs`: registration refuses a tool declaring both,
    /// so nothing downstream has to decide which vocabulary wins.
    pub argv_action_specs: &'static [ArgvActionSpec],
    /// What a flat tool declared it does ([`crate::Tool::EFFECT`]), or
    /// `None` for a dispatcher — whose effect is its actions' — and for a
    /// flat tool that declared nothing, which `try_freeze` refuses.
    pub effect: Option<crate::catalog::ToolEffect>,
    /// Tool-level audience: [`ToolAudience::Owner`] means every key of
    /// this tool — the bare name and each `tool:action` leaf — belongs to
    /// the owner alone. See [`ToolAudience`] for what the declaration is
    /// and is not.
    pub audience: ToolAudience,
    pub call: CallFn,
}

impl ToolDescriptor {
    /// What this tool does as a whole: a flat tool's declaration, a
    /// dispatcher's [`ToolEffect::join`](crate::catalog::ToolEffect::join) over
    /// every action.
    ///
    /// `SpiiceRegistry::try_freeze` guarantees `Some` for every registered
    /// tool.
    #[must_use]
    pub fn effect(&self) -> Option<crate::catalog::ToolEffect> {
        if self.action_arg_specs.is_empty() && self.argv_action_specs.is_empty() {
            return self.effect;
        }
        crate::catalog::ToolEffect::strongest(self.actions().map(|(_, effect)| effect))
    }

    /// The MCP hints this tool's [`Self::effect`] projects to.
    #[must_use]
    pub fn annotations(&self) -> Option<crate::catalog::ToolAnnotations> {
        self.effect()
            .map(crate::catalog::ToolAnnotations::registered)
    }

    /// Every action of either dispatcher vocabulary with its declared
    /// effect, in declaration order; empty for a flat tool.
    pub fn actions(&self) -> impl Iterator<Item = (&'static str, crate::catalog::ToolEffect)> + '_ {
        self.action_arg_specs
            .iter()
            .map(|spec| (spec.action, spec.effect))
            .chain(
                self.argv_action_specs
                    .iter()
                    .map(|spec| (spec.action, spec.effect)),
            )
    }

    /// The title a person reads for one key of this tool: the tool's own for
    /// its bare name, an action's for `tool:action`. `None` for a key of
    /// another tool, an action the tool does not declare, and a resource.
    #[must_use]
    pub fn title_of(&self, key: &crate::ScopeKey) -> Option<&'static str> {
        match key {
            crate::ScopeKey::Tool(tool) if tool.as_str() == self.name => Some(self.title),
            crate::ScopeKey::Action { tool, action } if tool.as_str() == self.name => self
                .action_arg_specs
                .iter()
                .map(|spec| (spec.action, spec.title))
                .chain(
                    self.argv_action_specs
                        .iter()
                        .map(|spec| (spec.action, spec.title)),
                )
                .find_map(|(declared, title)| (declared == action.as_str()).then_some(title)),
            _ => None,
        }
    }

    /// The descriptor-owned contract for one dispatcher action.
    #[must_use]
    pub fn action_arg_spec(&self, action: &str) -> Option<&ActionArgSpec> {
        self.action_arg_specs
            .iter()
            .find(|spec| spec.action == action)
    }

    /// The descriptor-owned contract for one argv-keyed action.
    #[must_use]
    pub fn argv_action_spec(&self, action: &str) -> Option<&ArgvActionSpec> {
        self.argv_action_specs
            .iter()
            .find(|spec| spec.action == action)
    }

    /// The action key one `{argv, …}` payload resolves to, or `None` when
    /// this tool is not argv-keyed or the argv matches no declared command.
    ///
    /// The same longest-prefix resolution the scope gate and the terminal
    /// dispatch run, exposed so a caller outside this crate classifies the
    /// action it is actually about to invoke. `None` is the unclassifiable
    /// case and every caller must read it as a write.
    #[must_use]
    pub fn argv_action(&self, args: &serde_json::Value) -> Option<&'static str> {
        if self.argv_action_specs.is_empty() {
            return None;
        }
        resolve_argv_action(self.name, self.argv_action_specs, args).ok()
    }

    /// What one action of this tool does. **The single classification
    /// rule** — every read/write decision about an action goes through here.
    ///
    /// - dispatcher (either vocabulary): the action's own declaration, never
    ///   the tool's; a key the vocabulary does not emit is `None`.
    /// - flat tool: the tool's declaration, which is what the whole-tool
    ///   gate reads.
    #[must_use]
    pub fn action_effect(&self, action: &str) -> Option<crate::catalog::ToolEffect> {
        self.declaration(Some(action)).effect
    }

    /// What one key of this tool declares: its effect as
    /// [`Self::action_effect`] reads it, and its audience — the tool's when
    /// the tool is [`ToolAudience::Owner`], else the action's.
    ///
    /// - flat tool: the tool's own declaration, whatever `action` says.
    /// - dispatcher: the declaration of `action`; `None`, or an action the
    ///   vocabulary does not emit, is [undeclared](KeyDeclaration::UNDECLARED)
    ///   for the tool's audience.
    #[must_use]
    pub fn declaration(&self, action: Option<&str>) -> KeyDeclaration {
        if self.action_arg_specs.is_empty() && self.argv_action_specs.is_empty() {
            return KeyDeclaration {
                effect: self.effect,
                audience: self.audience,
                need: OwnerNeed::Memory,
            };
        }
        let declared = action.and_then(|action| {
            self.action_arg_specs
                .iter()
                .map(|spec| (spec.action, spec.effect, spec.audience, spec.need))
                .chain(
                    self.argv_action_specs
                        .iter()
                        .map(|spec| (spec.action, spec.effect, spec.audience, spec.need)),
                )
                .find(|(declared, ..)| *declared == action)
        });
        match declared {
            Some((_, effect, audience, need)) => KeyDeclaration {
                effect: Some(effect),
                audience: match self.audience {
                    ToolAudience::Owner => ToolAudience::Owner,
                    ToolAudience::Shared => audience,
                },
                need,
            },
            None => KeyDeclaration {
                effect: None,
                audience: self.audience,
                need: OwnerNeed::Memory,
            },
        }
    }

    /// The [`ToolEffect::join`](crate::catalog::ToolEffect::join) over the
    /// actions `permitted` admits: what this tool does for a caller who may
    /// run only those. A flat tool's own declaration; `None` when no action
    /// is permitted.
    #[must_use]
    pub fn effect_over(
        &self,
        permitted: impl Fn(&str) -> bool,
    ) -> Option<crate::catalog::ToolEffect> {
        if self.action_arg_specs.is_empty() && self.argv_action_specs.is_empty() {
            return self.effect;
        }
        crate::catalog::ToolEffect::strongest(
            self.actions()
                .filter(|(action, _)| permitted(action))
                .map(|(_, effect)| effect),
        )
    }

    /// Whether the owner-role gate should treat one action as a read.
    /// Silence — an action [`Self::action_effect`] does not know — is a
    /// write.
    #[must_use]
    pub fn action_is_read_only(&self, action: &str) -> bool {
        self.action_effect(action)
            .is_some_and(crate::catalog::ToolEffect::is_read_only)
    }

    /// What one dispatcher action does: its enum variant's doc comment.
    #[must_use]
    pub fn action_description(&self, action: &str) -> Option<&str> {
        self.dispatcher_schema
            .as_ref()?
            .action(action)?
            .description
            .as_deref()
    }

    /// The closed argument schema of one dispatcher action alone.
    #[must_use]
    pub fn action_argument_schema(&self, action: &str) -> Option<&serde_json::Value> {
        Some(
            &self
                .dispatcher_schema
                .as_ref()?
                .action(action)?
                .argument_schema,
        )
    }

    /// The `inputSchema` for a caller who may run only the actions
    /// `permitted` admits: a dispatcher re-rendered over them, so its
    /// action enum, field set and prose name nothing the caller cannot run.
    /// A flat tool, or a caller permitted every action, gets `args_schema`.
    #[must_use]
    pub fn input_schema(&self, permitted: impl Fn(&str) -> bool) -> serde_json::Value {
        match &self.dispatcher_schema {
            Some(dispatcher)
                if !dispatcher
                    .actions
                    .iter()
                    .all(|action| permitted(&action.action)) =>
            {
                dispatcher.render(permitted)
            }
            _ => self.args_schema.clone(),
        }
    }

    /// Whether the owner-role gate should treat this tool as a read.
    ///
    /// Silence means write. A tool that has not said what it does may well
    /// write, and guessing "read" would hand a viewer a mutation.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.effect()
            .is_some_and(crate::catalog::ToolEffect::is_read_only)
    }

    /// Every [`ToolScope`](crate::ToolScope) key the scope gate judges a
    /// call to this tool by: the bare name for a flat tool, one
    /// `tool:action` leaf per action for either dispatcher vocabulary
    /// (`action_arg_specs` or `argv_action_specs`). The one definition a
    /// palette is built from.
    ///
    /// # Panics
    ///
    /// When the tool or an action name is outside the scope-key grammar, or
    /// a dispatcher is called `resource`. Registration refuses the first and
    /// `try_freeze` refuses both, so a descriptor of a frozen registry never
    /// does.
    #[must_use]
    pub fn palette_keys(&self) -> Vec<ScopeKey> {
        self.keyed_audiences().map(|(key, _)| key).collect()
    }

    /// The subset of [`Self::palette_keys`] that belongs to the owner alone:
    /// every key of an [`ToolAudience::Owner`] tool, else each action
    /// declared [`ToolAudience::Owner`].
    ///
    /// # Panics
    ///
    /// As [`Self::palette_keys`].
    #[must_use]
    pub fn owner_only_keys(&self) -> Vec<ScopeKey> {
        self.keyed_audiences()
            .filter(|(_, audience)| *audience == ToolAudience::Owner)
            .map(|(key, _)| key)
            .collect()
    }

    /// The subset of [`Self::palette_keys`] every deployment serves: declared
    /// reads, judged per action and including owner-audience reads.
    ///
    /// # Panics
    ///
    /// As [`Self::palette_keys`].
    #[must_use]
    pub fn served_read_keys(&self) -> Vec<ScopeKey> {
        self.palette_keys()
            .into_iter()
            .filter(|key| {
                match key {
                    ScopeKey::Tool(_) => self.declaration(None),
                    ScopeKey::Action { action, .. } => self.declaration(Some(action.as_str())),
                    ScopeKey::Resource(_) => KeyDeclaration::UNDECLARED,
                }
                .is_read()
            })
            .collect()
    }

    /// [`Self::palette_keys`] with the audience each key is for, or the first
    /// name that is no scope key. The check `try_freeze` runs over every
    /// tool; `palette_keys` is this, trusting it ran.
    ///
    /// # Errors
    ///
    /// [`ScopeKeyError`] for a tool or action name outside the grammar, or
    /// an action under the tool `resource`.
    pub(crate) fn scope_keys(&self) -> Result<Vec<(ScopeKey, ToolAudience)>, ScopeKeyError> {
        let tool = ToolName::parse(self.name)?;
        let tool_audience = self.audience;
        let effective = move |action: ToolAudience| {
            if tool_audience == ToolAudience::Owner {
                ToolAudience::Owner
            } else {
                action
            }
        };
        if self.action_arg_specs.is_empty() && self.argv_action_specs.is_empty() {
            return Ok(vec![(ScopeKey::Tool(tool), tool_audience)]);
        }
        let tagged = self
            .action_arg_specs
            .iter()
            .map(|spec| (spec.action, effective(spec.audience)));
        let argv = self
            .argv_action_specs
            .iter()
            .map(|spec| (spec.action, effective(spec.audience)));
        tagged
            .chain(argv)
            .map(|(action, audience)| {
                ScopeKey::action(tool.clone(), ActionName::parse(action)?)
                    .map(|key| (key, audience))
            })
            .collect()
    }

    fn keyed_audiences(&self) -> impl Iterator<Item = (ScopeKey, ToolAudience)> {
        self.scope_keys()
            .expect("try_freeze refuses a name that is no scope key")
            .into_iter()
    }

    /// Whether any actual command of this descriptor is admitted. Dispatchers
    /// are listed only when at least one registered leaf can be invoked.
    #[must_use]
    pub fn advertised_by(&self, capabilities: &CapabilitySet) -> bool {
        let dispatches = !self.action_arg_specs.is_empty() || !self.argv_action_specs.is_empty();
        if dispatches {
            self.actions()
                .any(|(action, _)| self.action_advertised_by(capabilities, action))
        } else {
            ToolName::parse(self.name).is_ok_and(|tool| {
                capabilities.admits(&ScopeKey::Tool(tool), self.declaration(None))
            })
        }
    }

    /// Listing and invocation judge the same registered leaf declaration.
    #[must_use]
    pub fn action_advertised_by(&self, capabilities: &CapabilitySet, action: &str) -> bool {
        self.actions().any(|(known, _)| known == action)
            && ToolName::parse(self.name)
                .and_then(|tool| ActionName::parse(action).map(|action| (tool, action)))
                .is_ok_and(|(tool, action)| {
                    capabilities.admits_action(
                        &tool,
                        &action,
                        self.declaration(Some(action.as_str())),
                    )
                })
    }
}

impl std::fmt::Debug for ToolDescriptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolDescriptor")
            .field("name", &self.name)
            .field("title", &self.title)
            .field("description", &self.description)
            .field("origin", &self.origin)
            .field("produces_schema_ids", &self.produces_schema_ids)
            .field("args_schema", &self.args_schema)
            .field("dispatcher_schema", &self.dispatcher_schema)
            .field("output_schema", &self.output_schema)
            .field("action_arg_specs", &self.action_arg_specs)
            .field("argv_action_specs", &self.argv_action_specs)
            .field("effect", &self.effect)
            .field("audience", &self.audience)
            .field("call", &"<callable>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionArgSpec {
    pub action: &'static str,
    /// The short name a person reads for this action ("Create a goal"). A
    /// sentence-case phrase of at most [`MAX_TITLE_CHARS`] characters on one
    /// line; `SpiiceRegistry::try_freeze` refuses anything else.
    pub title: &'static str,
    pub allowed_fields: &'static [&'static str],
    pub required_fields: &'static [&'static str],
    /// What this action does. See [`crate::catalog::ToolEffect`].
    pub effect: crate::catalog::ToolEffect,
    /// Who this action is for. See [`ToolAudience`].
    pub audience: ToolAudience,
    /// What this action asks of the owner. See [`OwnerNeed`].
    pub need: OwnerNeed,
}

/// One action of an argv-keyed dispatcher: a tool whose arguments are a CLI
/// grammar (`{argv, flags}`) rather than an internally tagged enum.
///
/// The action key is derived at dispatch by longest-prefix match of
/// `args["argv"]` against `argv_prefix`, and that derived key is what
/// [`crate::ToolScope::allows_action`] gates — the same `tool:action`
/// vocabulary an `action`-tagged dispatcher uses. The set is closed: argv
/// that matches no declared prefix is a validation error, never a
/// pass-through.
///
/// There is deliberately no field list here. Flag validation for an argv
/// grammar belongs to the tool's own dispatch, which owns the grammar; this
/// spec only names the scope key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgvActionSpec {
    pub action: &'static str,
    /// The short name a person reads for this action. See
    /// [`ActionArgSpec::title`].
    pub title: &'static str,
    pub argv_prefix: &'static [&'static str],
    /// What this action does. See [`crate::catalog::ToolEffect`]. Declared per
    /// command, so a dispatcher whose commands are mostly reads keeps those
    /// reads authorized for a read-capable-only owner — and retry-safe on
    /// `QUERY` — while its writes stay writes.
    pub effect: crate::catalog::ToolEffect,
    /// Who this action is for. See [`ToolAudience`].
    pub audience: ToolAudience,
    /// What this action asks of the owner. See [`OwnerNeed`].
    pub need: OwnerNeed,
}

/// Derive the action key of an argv-keyed dispatcher from `args["argv"]` by
/// longest-prefix match against the declared specs.
///
/// The set is closed: argv matching no declared prefix is a validation
/// error, mirroring how an unknown `action` is refused on a tagged
/// dispatcher — the gate never serves a key the vocabulary does not emit.
/// Longest prefix wins so `["approval"]` and `["approval", "decide"]` can
/// coexist as distinct actions, with the more specific spelling taking the
/// call.
///
/// No flag validation happens here; the tool's own dispatch owns the
/// grammar past the action key.
pub(crate) fn resolve_argv_action(
    tool_name: &str,
    specs: &[ArgvActionSpec],
    args: &serde_json::Value,
) -> Result<&'static str, ToolError> {
    let words = args
        .get("argv")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            ToolError::InvalidInput(format!(
                "{tool_name} arguments must include array field `argv`"
            ))
        })?;
    let words: Vec<&str> = words
        .iter()
        .map(|value| {
            value.as_str().ok_or_else(|| {
                ToolError::InvalidInput(format!("{tool_name} `argv` must be an array of strings"))
            })
        })
        .collect::<Result<_, _>>()?;
    specs
        .iter()
        .filter(|spec| {
            words.len() >= spec.argv_prefix.len()
                && spec
                    .argv_prefix
                    .iter()
                    .zip(&words)
                    .all(|(expected, actual)| expected == actual)
        })
        .max_by_key(|spec| spec.argv_prefix.len())
        .map(|spec| spec.action)
        .ok_or_else(|| {
            let supported = specs
                .iter()
                .map(|spec| spec.argv_prefix.join(" "))
                .collect::<Vec<_>>()
                .join(", ");
            ToolError::InvalidInput(format!(
                "{tool_name} argv {words:?} matches no declared command; expected one of: \
                 {supported}"
            ))
        })
}

pub(crate) fn validate_action_args(
    tool_name: &str,
    specs: &[ActionArgSpec],
    args: &serde_json::Value,
) -> Result<(), ToolError> {
    if specs.is_empty() {
        return Ok(());
    }
    let object = args.as_object().ok_or_else(|| {
        ToolError::InvalidInput(format!("{tool_name} arguments must be a JSON object"))
    })?;
    let action = object
        .get("action")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            ToolError::InvalidInput(format!(
                "{tool_name} arguments must include string field `action`"
            ))
        })?;
    let spec = specs
        .iter()
        .find(|spec| spec.action == action)
        .ok_or_else(|| {
            let supported = specs
                .iter()
                .map(|spec| spec.action)
                .collect::<Vec<_>>()
                .join(", ");
            ToolError::InvalidInput(format!(
                "{tool_name} action `{action}` is not supported; expected one of: {supported}"
            ))
        })?;

    let allowed = spec
        .allowed_fields
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let mut unexpected = object
        .keys()
        .filter(|field| field.as_str() != "action" && !allowed.contains(field.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unexpected.is_empty() {
        unexpected.sort();
        return Err(ToolError::InvalidInput(format!(
            "{tool_name} action `{action}` does not accept field(s): {}",
            unexpected.join(", ")
        )));
    }

    let missing = spec
        .required_fields
        .iter()
        .copied()
        .filter(|field| !object.contains_key(*field))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(ToolError::InvalidInput(format!(
            "{tool_name} action `{action}` requires field(s): {}",
            missing.join(", ")
        )));
    }
    Ok(())
}

/// Validate a *flat* (non-dispatcher) MCP tool's arguments: coerce the
/// `space`/`spaces` arity aliases against the tool's schema, then apply the
/// tool's [`UnknownFieldPolicy`] to every top-level key not declared as a
/// schema property. Dispatcher tools run [`validate_action_args`] instead.
///
/// Under the default [`UnknownFieldPolicy::Refuse`] an undeclared key is a
/// validation error: a mistyped `space` on `core_search_memories` would
/// otherwise search the wrong owner. Under
/// [`UnknownFieldPolicy::IgnoreAndReport`] the undeclared keys are removed
/// from `args` before decode and returned, sorted, for the caller to report.
/// The alias coercion runs first either way, so an alias is never counted as
/// unknown.
///
/// Returns the names of the keys that were dropped — always empty under
/// `Refuse`, and empty under either policy when every key was declared.
pub(crate) fn prepare_flat_tool_args(
    tool_name: &str,
    properties: &[String],
    args: &mut serde_json::Value,
    policy: UnknownFieldPolicy,
) -> Result<Vec<String>, ToolError> {
    coerce_space_aliases(tool_name, args, properties)?;
    let object = args.as_object_mut().ok_or_else(|| {
        ToolError::InvalidInput(format!("{tool_name} arguments must be a JSON object"))
    })?;
    let mut unexpected = object
        .keys()
        .filter(|field| !properties.iter().any(|property| property == field.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if unexpected.is_empty() {
        return Ok(Vec::new());
    }
    unexpected.sort();
    match policy {
        UnknownFieldPolicy::Refuse => Err(ToolError::InvalidInput(format!(
            "{tool_name} does not accept field(s): {}",
            unexpected.join(", ")
        ))),
        UnknownFieldPolicy::IgnoreAndReport => {
            for field in &unexpected {
                object.remove(field);
            }
            Ok(unexpected)
        }
    }
}

/// Reconcile the `space` (scalar) vs `spaces` (array) argument names so a
/// mismatched name is coerced, not silently ignored. Driven by the tool's own
/// schema: a tool declaring `spaces` accepts a scalar `space`; a tool declaring
/// `space` accepts a scalar `spaces` or a single-element `spaces` array.
pub(crate) fn coerce_space_aliases(
    tool_name: &str,
    args: &mut serde_json::Value,
    properties: &[String],
) -> Result<(), ToolError> {
    let Some(object) = args.as_object_mut() else {
        return Ok(());
    };
    let has_spaces = properties.iter().any(|property| property == "spaces");
    let has_space = properties.iter().any(|property| property == "space");
    if has_space
        && !has_spaces
        && let Some(count) = object
            .get("spaces")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len)
        && count > 1
    {
        return Err(ToolError::InvalidInput(format!(
            "{tool_name} accepts a single `space`; received `spaces` array with {count} elements"
        )));
    }
    if has_spaces
        && !has_space
        && !object.contains_key("spaces")
        && let Some(value) = object.remove("space")
    {
        object.insert("spaces".to_string(), serde_json::Value::Array(vec![value]));
    } else if has_space
        && !has_spaces
        && !object.contains_key("space")
        && let Some(value) = object.remove("spaces")
    {
        let scalar = match value {
            serde_json::Value::Array(items) => items.into_iter().next(),
            other => Some(other),
        };
        if let Some(scalar) = scalar {
            object.insert("space".to_string(), scalar);
        }
    }
    Ok(())
}

type Call = dyn Fn(ToolCtx, serde_json::Value) -> BoxFuture<'static, Result<serde_json::Value, ToolError>>
    + Send
    + Sync;

/// Copyable handle to a capture-capable, process-lifetime registered tool call.
pub type CallFn = &'static Call;

#[cfg(test)]
mod argv_action_tests {
    use super::{ArgvActionSpec, OwnerNeed, ToolAudience, resolve_argv_action};
    use crate::catalog::{Replay, ToolEffect};
    use crate::{ToolError, ToolErrorKind};

    /// Two commands sharing a first word, so only longest-prefix matching
    /// can tell them apart.
    const SPECS: &[ArgvActionSpec] = &[
        ArgvActionSpec {
            action: "approval",
            argv_prefix: &["approval"],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
            title: "Test action",
        },
        ArgvActionSpec {
            action: "approval-decide",
            argv_prefix: &["approval", "decide"],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
            title: "Test action",
        },
    ];

    /// The set is closed: argv the vocabulary does not emit is refused, not
    /// dispatched under some nearest key.
    #[test]
    fn unmatched_argv_is_rejected() {
        let args = serde_json::json!({ "argv": ["unknown", "verb"] });
        let err = resolve_argv_action("stub_cli", SPECS, &args)
            .expect_err("argv outside the declared commands is refused");
        assert_eq!(err.kind(), ToolErrorKind::InvalidInput);
        assert!(
            matches!(err, ToolError::InvalidInput(ref message)
                if message.contains("matches no declared command")),
            "got {err:?}",
        );
    }

    #[test]
    fn missing_or_malformed_argv_is_rejected() {
        for args in [
            serde_json::json!({}),
            serde_json::json!({ "argv": "approval" }),
            serde_json::json!({ "argv": ["approval", 7] }),
        ] {
            let err = resolve_argv_action("stub_cli", SPECS, &args)
                .expect_err("argv must be an array of strings");
            assert_eq!(err.kind(), ToolErrorKind::InvalidInput, "for {args}");
        }
    }
}

#[cfg(test)]
mod flat_tool_tests {
    use futures::future::BoxFuture;

    use super::{UnknownFieldPolicy, prepare_flat_tool_args};
    use crate::{Tool, ToolCtx, ToolError, ToolErrorKind};

    #[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
    struct CallerArgs {}

    #[derive(Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
    struct CallerOutput {
        model_id: String,
        client_name: String,
        client_version: String,
        caller_self_perspective: Option<String>,
    }

    struct CallerTool;

    impl Tool for CallerTool {
        const TITLE: &'static str = "Test tool";
        const NAME: &'static str = "proxiima-test_caller";
        const DESCRIPTION: &'static str = "Echo generic caller context.";

        type Args = CallerArgs;
        type Output = CallerOutput;

        fn call(
            ctx: ToolCtx,
            _args: Self::Args,
        ) -> BoxFuture<'static, Result<Self::Output, ToolError>> {
            Box::pin(async move {
                let caller = ctx.caller();
                Ok(CallerOutput {
                    model_id: caller.model_id.clone(),
                    client_name: caller.client_name.clone(),
                    client_version: caller.client_version.clone(),
                    caller_self_perspective: ctx
                        .caller_self_perspective()
                        .map(|id| id.into_inner().to_string()),
                })
            })
        }
    }

    #[test]
    fn flat_tool_refuses_unknown_field_by_default() {
        let mut args = serde_json::json!({ "query": "x", "spaces": [], "bogus": 1 });
        let err = prepare_flat_tool_args(
            "core_search_memories",
            &["query".to_string(), "spaces".to_string()],
            &mut args,
            UnknownFieldPolicy::Refuse,
        )
        .expect_err("unknown field rejected");
        assert!(
            matches!(err, ToolError::InvalidInput(ref m) if m.contains("does not accept field(s): bogus")),
            "got {err:?}",
        );
    }

    #[test]
    fn scalar_space_alias_coerces_to_spaces_array() {
        let mut args = serde_json::json!({ "query": "x", "space": "team" });
        prepare_flat_tool_args(
            "core_search_memories",
            &["query".to_string(), "spaces".to_string()],
            &mut args,
            UnknownFieldPolicy::Refuse,
        )
        .expect("space alias accepted");
        assert_eq!(args["spaces"], serde_json::json!(["team"]));
        assert!(args.get("space").is_none(), "alias key removed: {args}");
    }

    #[test]
    fn multi_element_spaces_alias_is_rejected_for_scalar_space() {
        let mut args = serde_json::json!({ "body": "b", "spaces": ["team", "other"] });
        let err = prepare_flat_tool_args(
            "core_remember",
            &["body".to_string(), "space".to_string()],
            &mut args,
            UnknownFieldPolicy::Refuse,
        )
        .expect_err("multiple spaces rejected for a scalar-space tool");
        assert_eq!(err.kind(), ToolErrorKind::InvalidInput);
        assert!(
            matches!(err, ToolError::InvalidInput(ref message)
                if message == "core_remember accepts a single `space`; received `spaces` array with 2 elements"),
            "got {err:?}",
        );
    }

    #[test]
    fn single_element_spaces_alias_coerces_to_scalar_space() {
        let mut args = serde_json::json!({ "body": "b", "spaces": ["team"] });
        prepare_flat_tool_args(
            "core_remember",
            &["body".to_string(), "space".to_string()],
            &mut args,
            UnknownFieldPolicy::Refuse,
        )
        .expect("single spaces alias accepted");
        assert_eq!(args["space"], serde_json::json!("team"));
        assert!(args.get("spaces").is_none(), "alias key removed: {args}");
    }

    #[test]
    fn scalar_spaces_alias_coerces_to_scalar_space() {
        let mut args = serde_json::json!({ "body": "b", "spaces": "team" });
        prepare_flat_tool_args(
            "core_remember",
            &["body".to_string(), "space".to_string()],
            &mut args,
            UnknownFieldPolicy::Refuse,
        )
        .expect("scalar spaces alias accepted");
        assert_eq!(args["space"], serde_json::json!("team"));
    }

    /// The alias coercion runs before the strip under either policy, so a
    /// scalar `space` is still coerced rather than counted as unknown and
    /// thrown away.
    #[test]
    fn tolerating_still_coerces_the_space_alias() {
        let mut args = serde_json::json!({ "query": "x", "space": "team", "has_more": true });
        let ignored = prepare_flat_tool_args(
            "core_search_memories",
            &["query".to_string(), "spaces".to_string()],
            &mut args,
            UnknownFieldPolicy::IgnoreAndReport,
        )
        .expect("alias coerced, unknown field tolerated");
        assert_eq!(ignored, vec!["has_more"]);
        assert_eq!(args["spaces"], serde_json::json!(["team"]));
    }

    /// The alias arity check reports a contradiction inside the declared
    /// vocabulary, not an undeclared key, so the opt-in does not relax it.
    #[test]
    fn tolerating_does_not_relax_the_alias_arity_check() {
        let mut args = serde_json::json!({ "body": "b", "spaces": ["team", "other"] });
        let err = prepare_flat_tool_args(
            "core_remember",
            &["body".to_string(), "space".to_string()],
            &mut args,
            UnknownFieldPolicy::IgnoreAndReport,
        )
        .expect_err("an ambiguous space alias is still refused");
        assert_eq!(err.kind(), ToolErrorKind::InvalidInput);
    }

    /// Refusing is what a tool gets for free: the enum default and the
    /// declaration a tool that says nothing carries.
    #[test]
    fn refuse_is_the_default_policy() {
        assert_eq!(UnknownFieldPolicy::default(), UnknownFieldPolicy::Refuse);
        assert_eq!(
            <CallerTool as Tool>::UNKNOWN_FIELD_POLICY,
            UnknownFieldPolicy::Refuse,
            "a tool that declares nothing keeps the strict guard",
        );
    }
}
