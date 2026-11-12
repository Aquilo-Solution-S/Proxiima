//! Transport-neutral spiice tool SDK.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::access::AccessKind;
use crate::authz::TrustedModelIdError;
use crate::storage_ports::OwnerWritePermit;
use crate::text_bounds::check_trimmed_len;
use crate::{AuthzContext, Engine, MemoryId, OwnerId, SpiiceRegistryFrozen};

#[derive(Clone)]
struct ServiceEntry {
    type_name: &'static str,
    value: Arc<dyn Any + Send + Sync>,
}

type ServiceValues = Arc<HashMap<TypeId, ServiceEntry>>;

/// One immutable-at-runtime service set composed by the linked spiices.
///
/// Values are keyed by their concrete Rust type. Composition is fallible:
/// two linked spiices cannot silently replace one another's service, and a
/// spiice cannot replace a substrate service such as `CitedBlobService`.
#[derive(Clone, Default)]
pub struct SpiiceServices {
    values: ServiceValues,
}

impl std::fmt::Debug for SpiiceServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpiiceServices")
            .field("len", &self.values.len())
            .finish_non_exhaustive()
    }
}

/// Failure to compose the runtime services of linked spiices.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpiiceServiceError {
    #[error("duplicate spiice service type `{type_name}`")]
    DuplicateService { type_name: &'static str },
}

impl SpiiceServices {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with<T>(value: T) -> Self
    where
        T: Send + Sync + 'static,
    {
        Self {
            values: Arc::new(HashMap::from([(
                TypeId::of::<T>(),
                ServiceEntry {
                    type_name: std::any::type_name::<T>(),
                    value: Arc::new(value),
                },
            )])),
        }
    }

    /// Insert one service without replacing an existing service of the same
    /// concrete type.
    ///
    /// # Errors
    ///
    /// Returns [`SpiiceServiceError::DuplicateService`] when the type is
    /// already present. The set is unchanged on failure.
    pub fn try_insert<T>(&mut self, value: T) -> Result<(), SpiiceServiceError>
    where
        T: Send + Sync + 'static,
    {
        let type_id = TypeId::of::<T>();
        if self.values.contains_key(&type_id) {
            return Err(SpiiceServiceError::DuplicateService {
                type_name: std::any::type_name::<T>(),
            });
        }
        Arc::make_mut(&mut self.values).insert(
            type_id,
            ServiceEntry {
                type_name: std::any::type_name::<T>(),
                value: Arc::new(value),
            },
        );
        Ok(())
    }

    /// Merge another composed set without overriding either side.
    ///
    /// # Errors
    ///
    /// Returns the lexically first duplicate concrete type. The receiver is
    /// unchanged on failure.
    pub fn try_extend(&mut self, other: Self) -> Result<(), SpiiceServiceError> {
        let duplicate = other
            .values
            .iter()
            .filter(|(type_id, _)| self.values.contains_key(type_id))
            .map(|(_, entry)| entry.type_name)
            .min();
        if let Some(type_name) = duplicate {
            return Err(SpiiceServiceError::DuplicateService { type_name });
        }
        let other_values = Arc::unwrap_or_clone(other.values);
        Arc::make_mut(&mut self.values).extend(other_values);
        Ok(())
    }

    #[must_use]
    pub fn get<T>(&self) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        service(&self.values)
    }
}

fn service<T>(values: &ServiceValues) -> Option<Arc<T>>
where
    T: Send + Sync + 'static,
{
    values
        .get(&TypeId::of::<T>())
        .map(|entry| entry.value.clone())
        .and_then(|value| value.downcast::<T>().ok())
}

/// Allowlisted inbound request headers of one served call, as opaque values.
///
/// The serving host copies exactly the headers its allowlist names
/// (`PROXIIMA_REQUEST_HEADERS`) off the inbound HTTP request; nothing else
/// crosses. Proxiima neither parses nor authenticates a value, so a tool may
/// forward one (a ticket to an egress gateway) but must never authorize on
/// one: the credential decides access, a header never does.
///
/// Published per call on the service set — `ctx.service::<RequestHeaders>()`
/// — and absent when the host allowlists no header. Names are lowercase;
/// lookup is case-insensitive. `Debug` prints names only: values are
/// typically credentials for a downstream service.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RequestHeaders {
    values: std::collections::BTreeMap<String, String>,
}

impl std::fmt::Debug for RequestHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.values.keys()).finish()
    }
}

impl RequestHeaders {
    /// Build from `(name, value)` pairs. Names are lowercased; a repeated
    /// name keeps its last value. Transport adapters build this from an
    /// allowlist that already refuses repeats.
    #[must_use]
    pub fn from_pairs<I, N, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (N, V)>,
        N: AsRef<str>,
        V: Into<String>,
    {
        Self {
            values: pairs
                .into_iter()
                .map(|(name, value)| (name.as_ref().to_ascii_lowercase(), value.into()))
                .collect(),
        }
    }

    /// The value of header `name`, case-insensitively.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Every `(name, value)` pair, ordered by lowercase name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Transport-neutral identity of the client invoking a tool.
///
/// Transport adapters populate this from their call context. `model_id` is
/// the operator *label* that reaches append-only provenance; when the
/// authenticated credential binds a model identity, [`ToolCtx::new`] makes
/// `model_id` that identity. Build policy on
/// [`AuthzContext::trusted_model_id`], the one source of the authenticated
/// model identity, never on this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCaller {
    pub model_id: String,
    pub client_name: String,
    pub client_version: String,
    /// The caller's own Self Perspective, when it named one.
    pub caller_self_perspective: Option<MemoryId>,
}

impl ToolCaller {
    /// Caller provenance with no Self Perspective.
    #[must_use]
    pub fn new(
        model_id: impl Into<String>,
        client_name: impl Into<String>,
        client_version: impl Into<String>,
    ) -> Self {
        Self {
            model_id: model_id.into(),
            client_name: client_name.into(),
            client_version: client_version.into(),
            caller_self_perspective: None,
        }
    }

    /// Name the caller's own Self Perspective.
    #[must_use]
    pub fn with_self_perspective(mut self, memory_id: Option<MemoryId>) -> Self {
        self.caller_self_perspective = memory_id;
        self
    }
}

/// Everything one tool call runs with: the owner it acts for, the caller's
/// authorization, the frozen registry, the caller, the composed services
/// and the engine.
///
/// [`Self::new`] is the only way in; a field changes only through a
/// `with_*` method, so the invariants `new` establishes hold for the whole
/// call.
///
/// ```compile_fail,E0616
/// fn relabel(mut ctx: proxiima_core::ToolCtx, other: proxiima_core::AuthzContext) {
///     ctx.authz = other;
/// }
/// ```
#[derive(Clone)]
pub struct ToolCtx {
    owner: OwnerId,
    authz: AuthzContext,
    registry: Arc<SpiiceRegistryFrozen>,
    caller: ToolCaller,
    services: SpiiceServices,
    engine: Option<Arc<Engine>>,
    judged: crate::catalog::JudgedKey,
}

impl std::fmt::Debug for ToolCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCtx")
            .field("owner", &self.owner)
            .field("caller", &self.caller)
            .field("services", &self.services)
            .field("has_engine", &self.engine.is_some())
            .finish_non_exhaustive()
    }
}

impl ToolCtx {
    /// A context with no engine and an empty judged key.
    ///
    /// When `authz` binds a model identity, the caller's `model_id` becomes
    /// that identity: a host assembles `caller` by hand, and `model_id` is
    /// the label a tool records.
    #[must_use]
    pub fn new(
        owner: OwnerId,
        authz: AuthzContext,
        registry: Arc<SpiiceRegistryFrozen>,
        services: SpiiceServices,
        caller: ToolCaller,
    ) -> Self {
        Self {
            owner,
            authz,
            registry,
            caller,
            services,
            engine: None,
            judged: crate::catalog::JudgedKey::default(),
        }
        .with_bound_model_id()
    }

    fn with_bound_model_id(mut self) -> Self {
        if let Some(trusted) = self.authz.trusted_model_id() {
            trusted.clone_into(&mut self.caller.model_id);
        }
        self
    }

    /// The same call, acting for `owner`.
    #[must_use]
    pub fn with_owner(mut self, owner: OwnerId) -> Self {
        self.owner = owner;
        self
    }

    /// The same call under `authz`; a model identity it binds becomes the
    /// caller's `model_id`, as in [`Self::new`].
    #[must_use]
    pub fn with_authz(mut self, authz: AuthzContext) -> Self {
        self.authz = authz;
        self.with_bound_model_id()
    }

    #[must_use]
    pub fn with_engine(mut self, engine: Option<Arc<Engine>>) -> Self {
        self.engine = engine;
        self
    }

    /// Add one service to this call's set without replacing one of the same
    /// concrete type.
    ///
    /// # Errors
    ///
    /// [`SpiiceServiceError::DuplicateService`] when the type is already
    /// present; the set is unchanged.
    pub fn try_insert_service<T>(&mut self, value: T) -> Result<(), SpiiceServiceError>
    where
        T: Send + Sync + 'static,
    {
        self.services.try_insert(value)
    }

    #[must_use]
    pub fn owner(&self) -> OwnerId {
        self.owner
    }

    /// This request's authenticated authority and immutable verified claims.
    /// Tools pass this to engine verbs, never a substituted identity.
    ///
    /// [`AuthzContext::verified_claims`] exposes application claims when the
    /// authenticator retained them, without exposing the bearer credential.
    #[must_use]
    pub fn authz(&self) -> &AuthzContext {
        &self.authz
    }

    #[must_use]
    pub fn registry(&self) -> &Arc<SpiiceRegistryFrozen> {
        &self.registry
    }

    #[must_use]
    pub fn caller(&self) -> &ToolCaller {
        &self.caller
    }

    /// The caller's own Self Perspective ([`ToolCaller::caller_self_perspective`]).
    #[must_use]
    pub fn caller_self_perspective(&self) -> Option<MemoryId> {
        self.caller.caller_self_perspective
    }

    /// `None` when the host wired no engine.
    #[must_use]
    pub fn engine(&self) -> Option<&Arc<Engine>> {
        self.engine.as_ref()
    }

    /// The wired engine, or the one canonical "engine unavailable" error.
    /// Every tool that needs storage goes through this so a missing
    /// engine reads identically everywhere.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Other`] when no engine is wired.
    pub fn require_engine(&self) -> Result<&Engine, ToolError> {
        self.engine
            .as_deref()
            .ok_or_else(|| ToolError::Other("engine unavailable".into()))
    }

    /// The key the scope gate judged this call by, if it derived one. Clones
    /// of a context share it, so a behavior that wraps the gate reads it from
    /// its own clone once the call returns.
    #[must_use]
    pub fn judged(&self) -> Option<crate::ScopeKey> {
        self.judged.get()
    }

    pub(crate) fn judged_cell(&self) -> &crate::catalog::JudgedKey {
        &self.judged
    }

    /// Authorize this tool context for a storage-tier owner write.
    ///
    /// The permit is minted by the engine from this context's real transport
    /// authorization and scoped owner; spiice code cannot construct it.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Other`] when the tool was not wired with an engine
    /// and [`ToolError::Protocol`] when authorization fails.
    pub async fn owner_write_permit(
        &self,
        kind: AccessKind,
    ) -> Result<OwnerWritePermit, ToolError> {
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| ToolError::Other("tool context has no engine".into()))?;
        engine
            .authorize_owner_write(&self.authz, &self.owner, kind)
            .await
            .map_err(ToolError::Protocol)
    }

    #[must_use]
    pub fn service<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.services.get::<T>()
    }

    /// Resolve the operator label this call's writes are recorded under.
    ///
    /// The one entry point for a tool that accepts its own `model_id`
    /// argument, instead of reading [`ToolCaller::model_id`]. A transport
    /// edge only ever inspects a *top-level* `model_id`, so a nested or
    /// per-item one (`core_episode_commit`'s per-item `derive.model_id` /
    /// `stance[].model_id`), and every argument arriving through the
    /// embedded host API, reaches the tool untouched. This is where a bound
    /// model identity wins over a caller's claim.
    ///
    /// Precedence: a bound identity wins, a differing `explicit` is refused,
    /// a blank one is no claim at all, and an absent one falls back to
    /// [`ToolCaller::model_id`], the label the edge resolved for this call.
    /// The bound identity is read from the authorization context, the carrier
    /// the authenticator hardened.
    ///
    /// The label is trimmed before it is *used*, not just before it is
    /// checked: the stored label and any idempotency key derived from it
    /// must be the same string.
    ///
    /// # Errors
    ///
    /// [`ToolError::InvalidInput`] when `explicit` names a model other
    /// than the one the authenticated token binds, or when the resolved
    /// label is longer than
    /// [`MAX_OPERATOR_LABEL_CHARS`](crate::MAX_OPERATOR_LABEL_CHARS).
    pub fn operator_label(&self, explicit: Option<&str>) -> Result<String, ToolError> {
        resolve_recorded_operator_label(
            self.authz.trusted_model_id(),
            Some(self.caller.model_id.as_str()),
            explicit,
        )
    }
}

/// Reserved argument name for the operator label, on every surface.
pub(crate) const OPERATOR_LABEL_FIELD: &str = "model_id";

/// Trim and bound a model identity an authenticated edge wants to bind.
///
/// Used by [`AuthzContext::with_trusted_model_id`](crate::AuthzContext::with_trusted_model_id),
/// the one carrier of the authenticated model identity.
///
/// # Errors
///
/// [`TrustedModelIdError`] when blank after trimming or over the bound.
pub(crate) fn validate_trusted_model_id(
    value: impl Into<String>,
) -> Result<String, TrustedModelIdError> {
    let raw = value.into();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(TrustedModelIdError::Blank);
    }
    let chars = trimmed.chars().count();
    if chars > crate::catalog::MAX_OPERATOR_LABEL_CHARS {
        return Err(TrustedModelIdError::TooLong {
            chars,
            max: crate::catalog::MAX_OPERATOR_LABEL_CHARS,
        });
    }
    Ok(trimmed.to_string())
}

/// The one implementation of "which operator label does this write carry".
///
/// Reached through [`ToolCtx::operator_label`].
///
/// `explicit` is normalised before anything else looks at it: blank is no
/// claim, so it can neither conflict with a bound identity nor be stored
/// as an operator. REST already dropped an empty header at the edge, and
/// without the same rule here `{"derive": {"model_id": ""}}` would fail a
/// request the identical REST call accepts.
///
/// `context_label` is the fallback, never a claim: it is the label the
/// edge already resolved for this call, so it is not re-checked against
/// the binding.
pub(crate) fn resolve_recorded_operator_label(
    trusted: Option<&str>,
    context_label: Option<&str>,
    explicit: Option<&str>,
) -> Result<String, ToolError> {
    fn claimed(value: Option<&str>) -> Option<&str> {
        value.map(str::trim).filter(|value| !value.is_empty())
    }

    let explicit = claimed(explicit);
    let bound = crate::catalog::resolve_operator_label(trusted, explicit)
        .map_err(|conflict| ToolError::InvalidInput(conflict.detail(OPERATOR_LABEL_FIELD)))?;
    // `bound` is already the answer whenever anything named one: the
    // trusted identity when the token binds one — whatever the caller sent,
    // because a differing claim was refused above — and otherwise the
    // caller's own claim. Only a call that named nothing falls back.
    let raw = match (trusted, explicit) {
        (Some(_), _) | (None, Some(_)) => bound,
        (None, None) => claimed(context_label).map_or(bound, ToString::to_string),
    };
    Ok(validate_trimmed_len(
        OPERATOR_LABEL_FIELD,
        &raw,
        crate::catalog::MAX_OPERATOR_LABEL_CHARS,
    )?
    .to_string())
}

/// Reject `limit: 0` on any paged read. `None` means the caller omitted
/// the bound and takes the tool's default, which is always fine.
///
/// The two ends of a page bound are not symmetric. A limit *above* the
/// maximum can be clamped, because "as many as you will give me" is still
/// the caller's intent and the page they get answers it. Zero answers
/// nothing: it yields a well-formed empty page that no client can tell
/// apart from "nothing matched", or — worse — a clamped page of one, which
/// answers a question that was not asked.
///
/// It is `pub` because every spiice with a paged read needs the same rule,
/// and a rule an out-of-tree spiice cannot reach is a rule it will
/// reimplement differently.
///
/// The engine applies the same rule (`engine::query`,
/// `engine::read_verbs`), so the tool layer refuses what the engine would
/// refuse instead of clamping first.
///
/// # Errors
///
/// [`ToolError::InvalidInput`] when `limit` is `Some(0)`.
pub fn reject_zero_limit(limit: Option<u32>) -> Result<(), ToolError> {
    if limit == Some(0) {
        return Err(ToolError::InvalidInput("limit must be at least 1".into()));
    }
    Ok(())
}

/// Longest search query any tool accepts, in characters.
///
/// One number because the arguments for it are not tool-specific: it
/// bounds the tsquery the lexical arm builds and the token count the
/// embedding arm pays for, and a caller who has to remember a different
/// cap per tool will get it wrong.
pub const MAX_QUERY_CHARS: usize = 512;

/// Longest text cap a caller may ask a search result to carry, in
/// characters — `core_search_memories`' `body_max_chars` and whatever a
/// spiice names its own snippet cap.
///
/// Shared rather than copied so a spiice's cap cannot drift apart from
/// core's unnoticed. Defaults are deliberately NOT shared: a short snippet
/// and an 8,000-character memory body differ because the objects do.
pub const MAX_TEXT_CAP_CHARS: usize = 8_000;

/// Trim `value` and check its length against `1..=max` characters,
/// returning the trimmed text and naming the bound that was actually
/// broken.
///
/// Blank and oversized are two different mistakes with two different
/// fixes, and a caller can only act on the one they made. A single
/// `body must be 1..=20000 chars` names a range a blank value satisfies,
/// which reads as a server fault rather than an instruction to send
/// content.
///
/// Counts characters, not bytes, for the reason
/// [`validate_search_query`] does: a cap that bound bytes would reject a
/// shorter text written in a language that does not fit in ASCII.
///
/// `field` is the wire name of the parameter, so the message points at
/// something the caller can see in the schema.
///
/// This is the tool-SDK spelling of [`check_trimmed_len`]; the rule and its
/// wording live there because `verbs` enforces the same contract on
/// `IdempotencyKey` and goal display fields and cannot reach this module.
///
/// # Errors
///
/// [`ToolError::InvalidInput`] when `value` is empty after trimming, or
/// longer than `max` characters.
pub fn validate_trimmed_len<'a>(
    field: &str,
    value: &'a str,
    max: usize,
) -> Result<&'a str, ToolError> {
    check_trimmed_len(value, max)
        .map_err(|violation| ToolError::InvalidInput(violation.reason(field)))
}

/// Trim `query` and check it against [`MAX_QUERY_CHARS`].
///
/// Three tools carried a byte-identical copy of this with `512` inlined
/// as a literal in each — see [`reject_zero_limit`] for why a rule with no
/// shared home is a rule that eventually disagrees with itself.
///
/// # Errors
///
/// [`ToolError::InvalidInput`] when the trimmed query is empty or longer
/// than [`MAX_QUERY_CHARS`].
pub fn validate_search_query(query: &str) -> Result<&str, ToolError> {
    validate_trimmed_len("query", query, MAX_QUERY_CHARS)
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// A well-formed reference to an entity that does not exist or is not
    /// visible to the caller (deliberately indistinguishable). Resource
    /// reads surface this as JSON-RPC `resource_not_found`; tool calls as
    /// `invalid_params`; REST as 404. The message names the wire handle,
    /// e.g. `memory <uuid> not found`.
    #[error("{0}")]
    NotFound(String),
    #[error("tool not authorized: {0}")]
    NotAuthorized(String),
    #[error("{0}")]
    Protocol(#[from] crate::error::ProtocolError),
    #[error("layering violation: {0}")]
    LayeringViolation(String),
    #[error("storage: {0}")]
    Storage(#[from] crate::StorageError),
    /// A required capability (embedding client, blob lane, engine) is not
    /// configured for this host. Unlike [`Self::Other`], its message is a
    /// caller-actionable precondition and is passed through verbatim rather
    /// than redacted to a generic internal-server-error (REST 503).
    #[error("{0}")]
    Unavailable(String),
    /// Another store did not answer (timed out, failed, or answered for a
    /// different memory). Says nothing about whether the memory exists, and
    /// is retryable. The message names the store number only: the port's
    /// own error text may carry the remote store's internals.
    #[error("store {store} unavailable")]
    StoreUnavailable { store: u16 },
    /// The tool ran and failed, and says why in its own words. A tool call
    /// answers it as [`ToolReply::Failure`](crate::ToolReply::Failure),
    /// `isError: true` with this text as its one content block; it is a
    /// protocol error only on a path that is not a tool call, and its text
    /// stays verbatim there too. Proxiima neither redacts nor rewords it.
    #[error("{0}")]
    Failure(String),
    #[error("{0}")]
    Other(String),
}

/// A tool written against typed arguments and a typed answer, with no wire
/// concepts in its signature.
///
/// There is deliberately no transport-neutral descriptor beside this trait.
/// Registration mints exactly one
/// [`ToolDescriptor`](crate::catalog::ToolDescriptor) per tool — which is
/// what the scope gate, the tool catalog, the REST action routes and the
/// `OpenAPI` document all read. A second descriptor type would be a second
/// answer to "what is registered", and the one no seam consults is the one
/// that drifts.
pub trait Tool: Send + Sync + 'static {
    const NAME: &'static str;
    /// The short name a person reads for this tool ("Goals", "Upload a
    /// file"): what a host shows beside a call it records, and what
    /// `tools/list` serves as the tool's title. A dispatcher's actions carry
    /// their own ([`crate::catalog::ActionArgSpec::title`]); this names the
    /// tool as a whole. One line of at most
    /// [`crate::catalog::MAX_TITLE_CHARS`] characters, checked by
    /// `SpiiceRegistry::try_freeze`.
    const TITLE: &'static str;
    const DESCRIPTION: &'static str;
    const PRODUCES_SCHEMA_IDS: &'static [&'static str] = &[];
    /// What this flat tool does — the one behaviour declaration every gate,
    /// hint and route derives from (see [`crate::catalog::ToolEffect`]).
    ///
    /// Required on a flat tool and refused on a dispatcher by
    /// `SpiiceRegistry::try_freeze`: a dispatcher declares one effect per
    /// action ([`Self::ACTION_ARG_SPECS`], [`Self::ARGV_ACTION_SPECS`]) and
    /// its tool-level effect is their join, so a tool-level value would be a
    /// second answer nothing reads.
    const EFFECT: Option<crate::catalog::ToolEffect> = None;
    /// The actions this tool dispatches, or `&[]` for a flat tool.
    ///
    /// This is THE enumeration of a dispatcher's action set — the scope
    /// gate, the tool catalog, the REST action routes, and the `OpenAPI`
    /// document all read it off `ToolDescriptor::action_arg_specs`.
    /// Declaring it turns a tool into a dispatcher: its `Args` must be an
    /// internally tagged enum tagged on `action`, its arguments are
    /// validated per action before decode, and its scope keys become
    /// `tool:action` leaves rather than the bare tool name.
    /// `SpiiceRegistry::try_freeze` refuses a registry where these and the
    /// schemars-derived dispatcher schema disagree. Each spec's `effect`
    /// is the sole behaviour authority for that action.
    const ACTION_ARG_SPECS: &'static [crate::catalog::ActionArgSpec] = &[];
    /// The actions of an argv-keyed dispatcher, or `&[]`.
    ///
    /// For a tool whose arguments are a CLI grammar (`{argv, flags}`), this
    /// is THE enumeration of its per-command scope keys. The action is
    /// derived at dispatch by longest-prefix match of `args["argv"]` against
    /// each spec's `argv_prefix`, the set is closed (unmatched argv is a
    /// validation error), and the derived `tool:action` key is what the
    /// scope gate and `tools/list` both consult — one vocabulary at both
    /// doors. Flag validation past the action key stays with the tool's own
    /// dispatch.
    ///
    /// Mutually exclusive with [`Self::ACTION_ARG_SPECS`]: registration
    /// refuses a tool declaring both, so no code path has to decide which
    /// vocabulary names an action.
    const ARGV_ACTION_SPECS: &'static [crate::catalog::ArgvActionSpec] = &[];
    /// Tool-level audience: [`crate::catalog::ToolAudience::Owner`] declares
    /// every key of this tool the owner's alone. Descriptor data for hosts
    /// that compute separate tool surfaces per audience — Proxiima's own
    /// gating does not read it. See
    /// [`crate::catalog::ToolDescriptor::audience`].
    const AUDIENCE: crate::catalog::ToolAudience = crate::catalog::ToolAudience::Shared;
    /// What this tool does with a top-level argument key its `Args` schema
    /// does not declare. Default
    /// [`UnknownFieldPolicy::Refuse`](crate::catalog::UnknownFieldPolicy::Refuse):
    /// declaring nothing keeps the strict guard, and the call is refused
    /// naming the offending keys.
    ///
    /// Declare
    /// [`IgnoreAndReport`](crate::catalog::UnknownFieldPolicy::IgnoreAndReport)
    /// only on a read-only tool whose callers are models — the undeclared
    /// keys are dropped before decode and named back in the result's
    /// `ignored_fields` array. Never on a tool that writes, and never where a
    /// silently dropped key could change what the call targets: the report is
    /// after the fact. Dispatchers ([`Self::ACTION_ARG_SPECS`],
    /// [`Self::ARGV_ACTION_SPECS`]) validate per action and ignore this.
    const UNKNOWN_FIELD_POLICY: crate::catalog::UnknownFieldPolicy =
        crate::catalog::UnknownFieldPolicy::Refuse;
    /// Top-level argument keys a *flat* tool accepts beyond its static
    /// schema, because the schema renders them per caller (and leaves them
    /// out where nothing can be rendered). The unknown-field guard admits
    /// them; every other undeclared key is still refused.
    const CALLER_RENDERED_FIELDS: &'static [&'static str] = &[];

    type Args: serde::de::DeserializeOwned + schemars::JsonSchema + Send + 'static;
    /// What the tool answers with. `JsonSchema` is required for the same
    /// reason it is on `Args`: the manifest describes both ends of the call,
    /// and the derived schema is what MCP clients validate
    /// `structuredContent` against. The output must serialize to a JSON object
    /// and its schema must declare an object root. Use an empty braced struct
    /// for an empty reply; unit, scalar and sequence roots cannot register.
    type Output: serde::Serialize + schemars::JsonSchema + Send + 'static;

    fn call(ctx: ToolCtx, args: Self::Args) -> BoxFuture<'static, Result<Self::Output, ToolError>>;
}

#[cfg(test)]
mod spiice_service_tests {
    use std::sync::Arc;

    use super::{SpiiceServiceError, SpiiceServices};

    #[derive(Debug)]
    struct Alpha(&'static str);

    #[derive(Debug)]
    struct Beta;

    #[derive(Debug)]
    struct Gamma;

    #[test]
    fn duplicate_insert_is_typed_and_keeps_the_original() {
        let mut services = SpiiceServices::with(Alpha("first"));
        let original = services.get::<Alpha>().expect("alpha present");

        let err = services.try_insert(Alpha("second")).unwrap_err();

        assert_eq!(
            err,
            SpiiceServiceError::DuplicateService {
                type_name: std::any::type_name::<Alpha>(),
            }
        );
        let retained = services.get::<Alpha>().expect("original retained");
        assert!(Arc::ptr_eq(&original, &retained));
        assert_eq!(retained.0, "first");
    }

    #[test]
    fn duplicate_extend_is_atomic() {
        let mut services = SpiiceServices::with(Alpha("first"));
        let mut incoming = SpiiceServices::with(Beta);
        incoming.try_insert(Alpha("second")).unwrap();
        incoming.try_insert(Gamma).unwrap();

        let err = services.try_extend(incoming).unwrap_err();

        assert!(matches!(
            err,
            SpiiceServiceError::DuplicateService { type_name }
                if type_name == std::any::type_name::<Alpha>()
        ));
        assert!(services.get::<Beta>().is_none());
        assert!(services.get::<Gamma>().is_none());
        assert_eq!(services.get::<Alpha>().expect("alpha retained").0, "first");
    }
}

#[cfg(test)]
mod shared_arg_rule_tests {
    use super::{MAX_QUERY_CHARS, ToolError, validate_search_query};

    #[test]
    fn a_query_is_trimmed_and_bounded() {
        assert_eq!(validate_search_query("  hello  ").unwrap(), "hello");
        assert_eq!(
            validate_search_query(&"a".repeat(MAX_QUERY_CHARS))
                .unwrap()
                .len(),
            MAX_QUERY_CHARS
        );
        assert!(validate_search_query("").is_err());
        assert!(validate_search_query("   ").is_err());
        assert!(validate_search_query(&"a".repeat(MAX_QUERY_CHARS + 1)).is_err());
    }

    /// Characters, not bytes. A byte cap would reject a shorter question
    /// for being written in a language that does not fit in ASCII.
    #[test]
    fn the_cap_counts_characters_not_bytes() {
        let cyrillic = "я".repeat(MAX_QUERY_CHARS);
        assert_eq!(cyrillic.len(), MAX_QUERY_CHARS * 2, "two bytes per char");
        assert!(
            validate_search_query(&cyrillic).is_ok(),
            "a 512-character Russian query is as legal as a 512-character English one"
        );
        assert!(validate_search_query(&"я".repeat(MAX_QUERY_CHARS + 1)).is_err());
    }

    #[test]
    fn the_rejection_is_invalid_input_and_names_the_bound() {
        let ToolError::InvalidInput(message) =
            validate_search_query(&"a".repeat(MAX_QUERY_CHARS + 1)).unwrap_err()
        else {
            panic!("a bad query must be invalid input, not any other error kind");
        };
        assert!(
            message.contains(&MAX_QUERY_CHARS.to_string()),
            "the message must carry the bound: {message}"
        );
    }

    /// Blank and oversized are different mistakes, so they get different
    /// messages. Answering a blank query with `must be 1..=512 chars` names
    /// a range the input satisfies, which reads as a server fault rather
    /// than an instruction to send content.
    #[test]
    fn blank_and_oversized_do_not_share_one_message() {
        let ToolError::InvalidInput(blank) = validate_search_query("   ").unwrap_err() else {
            panic!("blank must be invalid input");
        };
        let ToolError::InvalidInput(over) =
            validate_search_query(&"a".repeat(MAX_QUERY_CHARS + 1)).unwrap_err()
        else {
            panic!("oversized must be invalid input");
        };
        assert_ne!(blank, over, "one message for two mistakes tells neither");
        assert!(
            blank.contains("blank"),
            "a blank query must be told it is blank: {blank}"
        );
        assert!(
            !blank.contains(&MAX_QUERY_CHARS.to_string()),
            "a blank query must not be quoted a length bound it satisfies: {blank}"
        );
    }

    /// The message reports what the caller actually sent, so an agent one
    /// character over does not have to guess how far over it is.
    #[test]
    fn the_oversize_message_reports_the_length_that_was_sent() {
        let ToolError::InvalidInput(message) =
            validate_search_query(&"a".repeat(MAX_QUERY_CHARS + 7)).unwrap_err()
        else {
            panic!("oversized must be invalid input");
        };
        assert!(
            message.contains(&(MAX_QUERY_CHARS + 7).to_string()),
            "the message must carry the length sent: {message}"
        );
    }

    /// The length reported is the trimmed one, matching the length that
    /// was actually measured.
    #[test]
    fn the_reported_length_is_the_trimmed_length() {
        let padded = format!("   {}   ", "a".repeat(MAX_QUERY_CHARS + 1));
        let ToolError::InvalidInput(message) = validate_search_query(&padded).unwrap_err() else {
            panic!("oversized must be invalid input");
        };
        assert!(
            message.contains(&(MAX_QUERY_CHARS + 1).to_string())
                && !message.contains(&padded.chars().count().to_string()),
            "the reported length must be the trimmed one: {message}"
        );
    }
}

#[cfg(test)]
mod operator_label_tests {
    use std::sync::Arc;

    use super::{ToolCaller, ToolCtx, ToolError};
    use crate::test_fixtures::tool_ctx_with;
    use crate::{
        AuthPath, AuthzContext, MAX_OPERATOR_LABEL_CHARS, OwnerId, SpiiceRegistry, SpiiceServices,
        UserId,
    };

    fn owner() -> OwnerId {
        OwnerId::from(UserId::new(uuid::Uuid::now_v7()))
    }

    /// The caller carries the label the edge resolved, the binding lives on
    /// the authorization context.
    fn ctx(trusted: Option<&str>, caller_label: &str) -> ToolCtx {
        let owner = owner();
        let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);
        let authz = match trusted {
            None => authz,
            Some(trusted) => authz
                .with_trusted_model_id(trusted)
                .expect("a well-formed runner id binds"),
        };
        tool_ctx_with(
            owner,
            authz,
            Arc::new(SpiiceRegistry::new().freeze_or_panic_for_tests()),
            SpiiceServices::default(),
            ToolCaller::new(caller_label, "test-client", "0"),
        )
    }

    #[test]
    fn a_spiice_tool_cannot_relabel_a_bound_identity() {
        let err = ctx(Some("acme/runner-v3"), "acme/runner-v3")
            .operator_label(Some("openai/gpt-9"))
            .expect_err("a spiice argument may not relabel an authenticated runner");

        let ToolError::InvalidInput(message) = err else {
            panic!("a bad reserved argument is invalid input");
        };
        assert!(message.contains("model_id"), "{message}");
        assert!(
            message.contains("authenticated token already binds"),
            "{message}"
        );
    }

    /// Reads the binding from the authorization context, not from the
    /// caller struct: a host assembles `ToolCaller` by hand, and only the
    /// credential decides provenance.
    #[test]
    fn the_binding_is_read_from_the_credential_not_the_caller_copy() {
        let ctx = ctx(Some("acme/runner-v3"), "stale/label");

        assert_eq!(
            ctx.operator_label(None).expect("bound identity wins"),
            "acme/runner-v3"
        );
    }

    #[test]
    fn an_over_long_spiice_argument_is_refused() {
        assert!(
            ctx(None, "caller/model")
                .operator_label(Some(&"m".repeat(MAX_OPERATOR_LABEL_CHARS + 1)))
                .is_err()
        );
    }
}
