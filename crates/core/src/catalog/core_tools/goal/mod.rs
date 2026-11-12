use crate::catalog::validate_trimmed_len;
use crate::catalog::{ActionArgSpec, CoreActionMeta, OwnerNeed, ToolAudience};
use crate::catalog::{Replay, ToolEffect};
use crate::engine::{
    GoalCreatePayloadWriteRequest, GoalDecomposeRequest, GoalMarkAchievedRequest,
    GoalModifyRequest, GoalTransitionRequest,
};
use crate::protocol::{action as protocol_action, tool as protocol_tool};
use crate::verbs::goal_write::{
    ChildGoalDraft, GoalAuthorship, GoalEvidenceRef, GoalMotivationRef, GoalPayloadWrite,
    GoalTopologyWrite, GoalTransitionKind, GoalWriteOutcome, IdempotencyKey, OperatorKind,
    SystemOrigin,
};
use crate::verbs::schema::PayloadKind;
use crate::{
    GoalHandle, InputContractId, ModelId, OperatorId, PendingWork, PromptVersion, SchemaId,
    SchemaVersion, ToolId,
};
use crate::{Tool, ToolCtx, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const MAX_CHILD_GOALS: usize = 50;
/// Prefix of every decompose child key, `"goal_decompose:{root}:{index}"`.
const DECOMPOSE_CHILD_PREFIX: &str = "goal_decompose";
/// The longest decompose root key whose every child key still fits
/// [`IdempotencyKey::MAX_CHARS`]: the prefix, two separators and the widest
/// child index come off the common bound (180 - (14 + 2 + 2) = 162).
const DECOMPOSE_ROOT_KEY_MAX: usize = IdempotencyKey::MAX_CHARS
    - (DECOMPOSE_CHILD_PREFIX.len() + 2 + decimal_digits(MAX_CHILD_GOALS - 1));

/// How many decimal digits `n` prints with.
const fn decimal_digits(mut n: usize) -> usize {
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}
const MCP_OPERATOR_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x3f, 0x61, 0xde, 0x85, 0x4e, 0x09, 0x45, 0x62, 0x97, 0xc4, 0x8a, 0x74, 0xaa, 0xf9, 0x4a, 0x2c,
]);
const MCP_GOAL_INPUT_CONTRACT_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0xb2, 0x3f, 0x07, 0xef, 0xeb, 0x6f, 0x4f, 0xe7, 0xa9, 0xd3, 0x67, 0xa9, 0x53, 0xf3, 0x4a, 0x6d,
]);
const CORE_GOAL_PRODUCES_SCHEMA_IDS: &[&str] = &[];
pub const CORE_GOAL_ACTIONS: &[CoreActionMeta] = &[
    CoreActionMeta {
        tool: CoreGoalTool::NAME,
        action: "set",
        scope_key: protocol_action::CORE_GOAL_SET,
        produces_schema_ids: &[],
    },
    CoreActionMeta {
        tool: CoreGoalTool::NAME,
        action: "transition",
        scope_key: protocol_action::CORE_GOAL_TRANSITION,
        produces_schema_ids: &[],
    },
    CoreActionMeta {
        tool: CoreGoalTool::NAME,
        action: "modify",
        scope_key: protocol_action::CORE_GOAL_MODIFY,
        produces_schema_ids: &[],
    },
    CoreActionMeta {
        tool: CoreGoalTool::NAME,
        action: "mark_achieved",
        scope_key: protocol_action::CORE_GOAL_MARK_ACHIEVED,
        produces_schema_ids: &[],
    },
    CoreActionMeta {
        tool: CoreGoalTool::NAME,
        action: "decompose",
        scope_key: protocol_action::CORE_GOAL_DECOMPOSE,
        produces_schema_ids: &[],
    },
    CoreActionMeta {
        tool: CoreGoalTool::NAME,
        action: "grant_access",
        scope_key: protocol_action::CORE_GOAL_GRANT_ACCESS,
        produces_schema_ids: &[],
    },
    CoreActionMeta {
        tool: CoreGoalTool::NAME,
        action: "revoke_access",
        scope_key: protocol_action::CORE_GOAL_REVOKE_ACCESS,
        produces_schema_ids: &[],
    },
    CoreActionMeta {
        tool: CoreGoalTool::NAME,
        action: "publish",
        scope_key: protocol_action::CORE_GOAL_PUBLISH,
        produces_schema_ids: &[],
    },
    CoreActionMeta {
        tool: CoreGoalTool::NAME,
        action: "unpublish",
        scope_key: protocol_action::CORE_GOAL_UNPUBLISH,
        produces_schema_ids: &[],
    },
    CoreActionMeta {
        tool: CoreGoalTool::NAME,
        action: "bind_work",
        scope_key: protocol_action::CORE_GOAL_BIND_WORK,
        produces_schema_ids: &[],
    },
    CoreActionMeta {
        tool: CoreGoalTool::NAME,
        action: "unbind_work",
        scope_key: protocol_action::CORE_GOAL_UNBIND_WORK,
        produces_schema_ids: &[],
    },
];

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalAccessArgs {
    /// Stable Goal series UUID.
    pub goal_handle: String,
    /// Person or group UUID. A group's members hold the grant as their own
    /// role allows, and a group grant carries no `manage`.
    pub subject: String,
    /// The published memory the guest reads, by kind. The guest reads the
    /// Goal itself whatever this is.
    pub read_ceiling: crate::AccessCeiling,
    /// The Goal work memory the guest writes, by kind.
    pub write_ceiling: crate::AccessCeiling,
    /// The guest edits this Goal: its title, text and progress.
    #[serde(default)]
    pub works_goals: bool,
    #[serde(default)]
    pub manage: bool,
    pub expires_at: Option<std::time::SystemTime>,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalSubjectArgs {
    pub goal_handle: String,
    pub subject: String,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalPublicationArgs {
    /// Stable Goal series UUID.
    pub goal_handle: String,
    /// Exact memory version UUID.
    pub memory_id: String,
    pub permitted_use: crate::PublicationUse,
}
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalWorkArgs {
    pub goal_handle: String,
    pub series: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GoalCollaborationOutput {
    pub action: String,
    pub goal_handle: String,
    pub subject: Option<String>,
    pub memory_id: Option<String>,
    pub series: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GoalPayloadArgs {
    #[schemars(
        description = "Goal schema id, e.g. `core/simple-text-v1`; see `proxiima://schemas?kind=Goal`."
    )]
    pub schema_id: String,
    #[schemars(description = "Version of `schema_id`. Omit for 1.")]
    pub schema_version: Option<u32>,
    #[schemars(length(max = 240), description = "Goal title, 1-240 trimmed chars.")]
    pub title: String,
    #[schemars(
        length(max = 20000),
        description = "Goal prose, 1-20000 trimmed chars."
    )]
    pub text: String,
    #[serde(default = "default_empty_object")]
    #[schemars(
        with = "std::collections::BTreeMap<String, serde_json::Value>",
        description = "Schema-defined payload; omit for `{}`."
    )]
    pub body: serde_json::Value,
}

fn default_empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalSetArgs {
    #[serde(flatten)]
    pub payload: GoalPayloadArgs,
    #[serde(default)]
    #[schemars(description = "Optional supporting Fact or Abstraction UUIDs.")]
    pub evidence: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Exact motivating Perspective version UUIDs.")]
    pub motivated_by: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Optional memory space key; omit for the session default.")]
    pub space: Option<String>,
    #[schemars(description = "Optional idempotency key (1-180 chars); changed content conflicts.")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GoalWriteOutput {
    pub handle: String,
    pub goal_handle: String,
    /// Index rows the write asserted. Not handles: an edge has no id.
    pub edge_count: usize,
    pub idempotent_replay: bool,
}

#[derive(Debug)]
pub struct CoreGoalTool;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CoreGoalArgs {
    /// Create an Active goal with optional Perspective motivations and evidence.
    Set(GoalSetArgs),
    /// Pause, resume, or abandon a goal; abandon is refused while a child goal is Active or Paused.
    Transition(GoalTransitionArgs),
    /// Replace an Active goal's title, text and payload; omitted evidence carries over.
    Modify(GoalModifyArgs),
    /// Mark a goal Achieved, optionally citing memories that show it; a goal with child goals needs every
    /// child Achieved or Abandoned and at least one Achieved.
    MarkAchieved(GoalMarkAchievedArgs),
    /// Create Active child Goals under a parent goal.
    Decompose(GoalDecomposeArgs),
    /// Grant a subject bounded read/write access to a Goal.
    GrantAccess(GoalAccessArgs),
    /// Revoke a subject's Goal access.
    RevokeAccess(GoalSubjectArgs),
    /// Publish one exact memory version for the Goal.
    Publish(GoalPublicationArgs),
    /// Withdraw one exact Goal publication.
    Unpublish(GoalUnpublishArgs),
    /// Bind a working series to the Goal.
    BindWork(GoalWorkArgs),
    /// Remove a working-series binding from the Goal.
    UnbindWork(GoalWorkArgs),
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum CoreGoalOutput {
    Write(GoalWriteOutput),
    Decompose(GoalDecomposeOutput),
    Collaboration(GoalCollaborationOutput),
}

impl Tool for CoreGoalTool {
    const NAME: &'static str = protocol_tool::CORE_GOAL;
    const TITLE: &'static str = "Goals";
    const DESCRIPTION: &'static str = "Write Goals: create, decompose, transition, edit, achieve, share, publish, or bind work. Use the latest head handle. Motivations and evidence are optional; pause needs pending=keep or discard.";
    const PRODUCES_SCHEMA_IDS: &'static [&'static str] = CORE_GOAL_PRODUCES_SCHEMA_IDS;
    const ACTION_ARG_SPECS: &'static [ActionArgSpec] = &[
        ActionArgSpec {
            action: "set",
            title: "Create a goal",
            allowed_fields: &[
                "schema_id",
                "schema_version",
                "title",
                "text",
                "body",
                "evidence",
                "motivated_by",
                "space",
                "idempotency_key",
            ],
            required_fields: &["schema_id", "title", "text"],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::GoalWork,
        },
        ActionArgSpec {
            action: "grant_access",
            title: "Grant Goal access",
            allowed_fields: &[
                "goal_handle",
                "subject",
                "read_ceiling",
                "write_ceiling",
                "works_goals",
                "manage",
                "expires_at",
            ],
            required_fields: &["goal_handle", "subject", "read_ceiling", "write_ceiling"],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
        },
        ActionArgSpec {
            action: "revoke_access",
            title: "Revoke Goal access",
            allowed_fields: &["goal_handle", "subject"],
            required_fields: &["goal_handle", "subject"],
            effect: ToolEffect::Destructive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
        },
        ActionArgSpec {
            action: "publish",
            title: "Publish an exact memory version",
            allowed_fields: &["goal_handle", "memory_id", "permitted_use"],
            required_fields: &["goal_handle", "memory_id", "permitted_use"],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
        },
        ActionArgSpec {
            action: "unpublish",
            title: "Withdraw an exact publication",
            allowed_fields: &["goal_handle", "memory_id"],
            required_fields: &["goal_handle", "memory_id"],
            effect: ToolEffect::Destructive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
        },
        ActionArgSpec {
            action: "bind_work",
            title: "Bind a working series",
            allowed_fields: &["goal_handle", "series"],
            required_fields: &["goal_handle", "series"],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
        },
        ActionArgSpec {
            action: "unbind_work",
            title: "Remove a working binding",
            allowed_fields: &["goal_handle", "series"],
            required_fields: &["goal_handle", "series"],
            effect: ToolEffect::Destructive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
        },
        ActionArgSpec {
            action: "transition",
            title: "Change a goal's state",
            allowed_fields: &["goal", "transition", "pending", "space", "idempotency_key"],
            // `pending` is required for pause and refused for the other two:
            // `GoalTransitionArgs::transition_kind` decides, once.
            required_fields: &["goal", "transition"],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::GoalWork,
        },
        ActionArgSpec {
            action: "modify",
            title: "Edit a goal",
            allowed_fields: &[
                "goal",
                "schema_id",
                "schema_version",
                "title",
                "text",
                "body",
                "evidence",
                "motivated_by",
                "space",
                "idempotency_key",
            ],
            required_fields: &["goal", "schema_id", "title", "text"],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::GoalWork,
        },
        ActionArgSpec {
            action: "mark_achieved",
            title: "Mark a goal achieved",
            allowed_fields: &["goal", "evidence", "space", "idempotency_key"],
            required_fields: &["goal"],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
        },
        ActionArgSpec {
            action: "decompose",
            title: "Break a goal into subgoals",
            allowed_fields: &[
                "parent_goal",
                "children",
                "motivated_by",
                "space",
                "idempotency_key",
            ],
            required_fields: &["parent_goal", "children", "idempotency_key"],
            effect: ToolEffect::Additive(Replay::Idempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
        },
    ];
    type Args = CoreGoalArgs;
    type Output = CoreGoalOutput;

    fn call(
        ctx: ToolCtx,
        args: CoreGoalArgs,
    ) -> futures::future::BoxFuture<'static, Result<CoreGoalOutput, ToolError>> {
        Box::pin(async move {
            match args {
                CoreGoalArgs::Set(args) => goal_set(ctx, args).await.map(CoreGoalOutput::Write),
                CoreGoalArgs::Transition(args) => {
                    goal_transition(ctx, args).await.map(CoreGoalOutput::Write)
                }
                CoreGoalArgs::Modify(args) => {
                    goal_modify(ctx, args).await.map(CoreGoalOutput::Write)
                }
                CoreGoalArgs::MarkAchieved(args) => goal_mark_achieved(ctx, args)
                    .await
                    .map(CoreGoalOutput::Write),
                CoreGoalArgs::Decompose(args) => goal_decompose(ctx, args)
                    .await
                    .map(CoreGoalOutput::Decompose),
                CoreGoalArgs::GrantAccess(a) => goal_grant_access(ctx, a)
                    .await
                    .map(CoreGoalOutput::Collaboration),
                CoreGoalArgs::RevokeAccess(a) => goal_revoke_access(ctx, a)
                    .await
                    .map(CoreGoalOutput::Collaboration),
                CoreGoalArgs::Publish(a) => goal_publish(ctx, a)
                    .await
                    .map(CoreGoalOutput::Collaboration),
                CoreGoalArgs::Unpublish(a) => goal_unpublish(ctx, a)
                    .await
                    .map(CoreGoalOutput::Collaboration),
                CoreGoalArgs::BindWork(a) => goal_bind_work(ctx, a)
                    .await
                    .map(CoreGoalOutput::Collaboration),
                CoreGoalArgs::UnbindWork(a) => goal_unbind_work(ctx, a)
                    .await
                    .map(CoreGoalOutput::Collaboration),
            }
        })
    }
}

async fn goal_set(ctx: ToolCtx, args: GoalSetArgs) -> Result<GoalWriteOutput, ToolError> {
    let space = super::memory_spaces::resolve_space_owner(
        &ctx,
        args.space.as_deref(),
        super::memory_spaces::SpaceDefault::Current,
    )?;
    let payload = encode_goal_payload(&ctx, args.payload)?;
    let evidence = resolve_goal_evidence_handles(&ctx, &args.evidence).await?;
    let motivated_by = resolve_goal_motivations(&ctx, &args.motivated_by)?;
    let topology =
        GoalTopologyWrite::new(motivated_by, Vec::new(), evidence).map_err(ToolError::Protocol)?;
    let request_id = IdempotencyKey::optional_or_generated("goal_set", args.idempotency_key)
        .map_err(ToolError::InvalidInput)?;
    let authorship = system_operator_authorship(&ctx, "goal_set");
    let engine = ctx.engine();
    let outcome = engine
        .create_goal_from_payload_write(
            ctx.authz(),
            &GoalCreatePayloadWriteRequest {
                owner: space.owner,
                topology,
                payload,
                request_id,
                authorship,
                author_self_perspective_id: ctx.caller_self_perspective(),
            },
        )
        .await
        .map_err(ToolError::Protocol)?;
    Ok(format_goal_write_output(&ctx, &outcome))
}

fn collaboration_goal(raw: &str) -> Result<GoalHandle, ToolError> {
    Ok(GoalHandle::new(crate::catalog::ids::parse_wire_uuid(raw)?))
}

fn collaboration_subject(raw: &str) -> Result<crate::OwnerId, ToolError> {
    Ok(crate::OwnerId::new(crate::catalog::ids::parse_wire_uuid(
        raw,
    )?))
}

fn collaboration_role(a: &GoalAccessArgs) -> Result<crate::Role, ToolError> {
    let role = crate::Role::new(a.read_ceiling, a.write_ceiling)
        .map_err(|e| ToolError::InvalidInput(e.to_string()))?
        .with_goals(if a.works_goals {
            crate::GoalAccess::Works
        } else {
            crate::GoalAccess::Reads
        });
    Ok(if a.manage { role.managing() } else { role })
}

async fn goal_grant_access(
    ctx: ToolCtx,
    a: GoalAccessArgs,
) -> Result<GoalCollaborationOutput, ToolError> {
    let role = collaboration_role(&a)?;
    ctx.engine()
        .grant_goal_access(
            ctx.authz(),
            &crate::GoalAudienceGrantInput {
                goal: collaboration_goal(&a.goal_handle)?,
                subject: collaboration_subject(&a.subject)?,
                role,
                expires_at: a.expires_at,
            },
        )
        .await
        .map(|()| GoalCollaborationOutput {
            action: "grant_access".into(),
            goal_handle: a.goal_handle,
            subject: Some(a.subject),
            memory_id: None,
            series: None,
        })
        .map_err(ToolError::Protocol)
}

async fn goal_revoke_access(
    ctx: ToolCtx,
    a: GoalSubjectArgs,
) -> Result<GoalCollaborationOutput, ToolError> {
    ctx.engine()
        .revoke_goal_access(
            ctx.authz(),
            collaboration_goal(&a.goal_handle)?,
            collaboration_subject(&a.subject)?,
        )
        .await
        .map(|()| GoalCollaborationOutput {
            action: "revoke_access".into(),
            goal_handle: a.goal_handle,
            subject: Some(a.subject),
            memory_id: None,
            series: None,
        })
        .map_err(ToolError::Protocol)
}

async fn goal_publish(
    ctx: ToolCtx,
    a: GoalPublicationArgs,
) -> Result<GoalCollaborationOutput, ToolError> {
    let memory = ctx.resolve_memory(&a.memory_id)?;
    ctx.engine()
        .publish_to_goal(
            ctx.authz(),
            &crate::GoalPublicationInput {
                goal: collaboration_goal(&a.goal_handle)?,
                memory,
                permitted_use: a.permitted_use,
            },
        )
        .await
        .map(|()| GoalCollaborationOutput {
            action: "publish".into(),
            goal_handle: a.goal_handle,
            subject: None,
            memory_id: Some(a.memory_id),
            series: None,
        })
        .map_err(ToolError::Protocol)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalUnpublishArgs {
    pub goal_handle: String,
    pub memory_id: String,
}

async fn goal_unpublish(
    ctx: ToolCtx,
    a: GoalUnpublishArgs,
) -> Result<GoalCollaborationOutput, ToolError> {
    let memory = ctx.resolve_memory(&a.memory_id)?;
    ctx.engine()
        .unpublish_from_goal(ctx.authz(), collaboration_goal(&a.goal_handle)?, memory)
        .await
        .map(|()| GoalCollaborationOutput {
            action: "unpublish".into(),
            goal_handle: a.goal_handle,
            subject: None,
            memory_id: Some(a.memory_id),
            series: None,
        })
        .map_err(ToolError::Protocol)
}

async fn goal_bind_work(
    ctx: ToolCtx,
    a: GoalWorkArgs,
) -> Result<GoalCollaborationOutput, ToolError> {
    let series = crate::engine::SeriesHandle::new(crate::catalog::ids::parse_wire_uuid(&a.series)?);
    ctx.engine()
        .bind_goal_work(
            ctx.authz(),
            &crate::GoalWorkBindingInput {
                goal: collaboration_goal(&a.goal_handle)?,
                series,
            },
        )
        .await
        .map(|()| GoalCollaborationOutput {
            action: "bind_work".into(),
            goal_handle: a.goal_handle,
            subject: None,
            memory_id: None,
            series: Some(a.series),
        })
        .map_err(ToolError::Protocol)
}

async fn goal_unbind_work(
    ctx: ToolCtx,
    a: GoalWorkArgs,
) -> Result<GoalCollaborationOutput, ToolError> {
    let series = crate::engine::SeriesHandle::new(crate::catalog::ids::parse_wire_uuid(&a.series)?);
    ctx.engine()
        .unbind_goal_work(
            ctx.authz(),
            &crate::GoalWorkBindingInput {
                goal: collaboration_goal(&a.goal_handle)?,
                series,
            },
        )
        .await
        .map(|()| GoalCollaborationOutput {
            action: "unbind_work".into(),
            goal_handle: a.goal_handle,
            subject: None,
            memory_id: None,
            series: Some(a.series),
        })
        .map_err(ToolError::Protocol)
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoalTransition {
    #[serde(alias = "Pause", alias = "PAUSE")]
    Pause,
    #[serde(alias = "Resume", alias = "RESUME")]
    Resume,
    #[serde(alias = "Abandon", alias = "ABANDON")]
    Abandon,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalTransitionArgs {
    #[schemars(description = "Latest Goal head handle `<uuid>`; stale handles are refused.")]
    pub goal: String,
    #[schemars(
        description = "pause Active, resume Paused, abandon Active or Paused; use mark_achieved to achieve. Abandon refuses open children."
    )]
    pub transition: GoalTransition,
    #[schemars(
        description = "Required for pause; refused otherwise. `keep` holds queued work for resume; `discard` drops it. Running work finishes either way."
    )]
    pub pending: Option<PendingWork>,
    #[serde(default)]
    #[schemars(description = "Optional space key; omit to use the Goal's owner.")]
    pub space: Option<String>,
    #[schemars(description = "Optional idempotency key (1-180 chars); changed content conflicts.")]
    pub idempotency_key: Option<String>,
}

impl GoalTransitionArgs {
    /// The one place `transition` and `pending` meet: what the caller wrote
    /// becomes a [`GoalTransitionKind`], in which `pending` exists only on a
    /// pause.
    fn transition_kind(&self) -> Result<GoalTransitionKind, ToolError> {
        match (&self.transition, self.pending) {
            (GoalTransition::Pause, Some(pending)) => Ok(GoalTransitionKind::Pause { pending }),
            (GoalTransition::Pause, None) => Err(ToolError::InvalidInput(
                "pending is required when transition is pause: \"keep\" (the scheduler holds the \
                 queued work and runs it after a resume) or \"discard\" (it drops the queued work)"
                    .into(),
            )),
            (GoalTransition::Resume, None) => Ok(GoalTransitionKind::Resume),
            (GoalTransition::Abandon, None) => Ok(GoalTransitionKind::Abandon),
            (GoalTransition::Resume | GoalTransition::Abandon, Some(_)) => Err(
                ToolError::InvalidInput("pending applies only when transition is pause".into()),
            ),
        }
    }
}

/// The named Goal selects its existing owner. An explicit space remains a
/// narrowing selector; it cannot route an edit to another owner.
async fn existing_goal_owner(
    ctx: &ToolCtx,
    prior: crate::GoalId,
    space: Option<&str>,
) -> Result<crate::OwnerId, ToolError> {
    let selected = space
        .map(|raw| {
            super::memory_spaces::resolve_space_owner(
                ctx,
                Some(raw),
                super::memory_spaces::SpaceDefault::Current,
            )
        })
        .transpose()?;
    let mut request = crate::verbs::query::QueryRequest::readable();
    request.entity_kind = Some(crate::EntityKind::Goal);
    request.goal_ids = vec![prior];
    request.supersession = crate::verbs::query::SupersessionStatus::IncludeSuperseded;
    request.include_payloads = false;
    request.limit = 1;
    let row = ctx
        .engine()
        .query(ctx.authz(), &request)
        .await?
        .goals
        .into_iter()
        .next()
        .ok_or_else(|| ToolError::NotFound("Goal not found".into()))?;
    if selected.is_some_and(|space| space.owner != row.owner) {
        return Err(ToolError::NotFound("Goal not found".into()));
    }
    Ok(row.owner)
}

async fn goal_transition(
    ctx: ToolCtx,
    args: GoalTransitionArgs,
) -> Result<GoalWriteOutput, ToolError> {
    let transition = args.transition_kind()?;
    let prior = ctx.resolve_goal(&args.goal)?;
    let owner = existing_goal_owner(&ctx, prior, args.space.as_deref()).await?;
    let request_id = IdempotencyKey::optional_or_generated("goal_transition", args.idempotency_key)
        .map_err(ToolError::InvalidInput)?;
    let engine = ctx.engine();
    let outcome = engine
        .transition_goal(
            ctx.authz(),
            &GoalTransitionRequest {
                owner,
                prior_goal_id: prior,
                transition,
                authorship: GoalAuthorship::User,
                request_id,
                author_self_perspective_id: ctx.caller_self_perspective(),
            },
        )
        .await
        .map_err(ToolError::Protocol)?;
    Ok(format_goal_write_output(&ctx, &outcome))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalMarkAchievedArgs {
    #[schemars(
        description = "Latest Goal head handle `<uuid>`; stale handles and open children are refused."
    )]
    pub goal: String,
    #[serde(default)]
    #[schemars(description = "Optional completion-evidence Fact or Abstraction UUIDs.")]
    pub evidence: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Optional space key; omit to use the Goal's owner.")]
    pub space: Option<String>,
    #[schemars(
        description = "Optional idempotency key (1-180 chars); same content replays, changed content conflicts."
    )]
    pub idempotency_key: Option<String>,
}

async fn goal_mark_achieved(
    ctx: ToolCtx,
    args: GoalMarkAchievedArgs,
) -> Result<GoalWriteOutput, ToolError> {
    let prior = ctx.resolve_goal(&args.goal)?;
    let owner = existing_goal_owner(&ctx, prior, args.space.as_deref()).await?;
    let evidence = resolve_goal_evidence_handles(&ctx, &args.evidence).await?;
    let request_id =
        IdempotencyKey::optional_or_generated("goal_mark_achieved", args.idempotency_key)
            .map_err(ToolError::InvalidInput)?;
    let engine = ctx.engine();
    let outcome = engine
        .mark_goal_achieved(
            ctx.authz(),
            &GoalMarkAchievedRequest {
                owner,
                prior_goal_id: prior,
                authorship: GoalAuthorship::System(SystemOrigin::Tool {
                    tool_id: ToolId::new(protocol_action::CORE_GOAL_MARK_ACHIEVED),
                }),
                request_id,
                evidence,
                author_self_perspective_id: ctx.caller_self_perspective(),
            },
        )
        .await
        .map_err(ToolError::Protocol)?;
    Ok(format_goal_write_output(&ctx, &outcome))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalModifyArgs {
    #[schemars(description = "Latest Goal head handle `<uuid>`; stale handles are refused.")]
    pub goal: String,
    #[serde(flatten)]
    pub payload: GoalPayloadArgs,
    #[schemars(description = "Replacement evidence UUIDs; omit to keep; an empty list clears it.")]
    pub evidence: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(
        description = "Motivation UUIDs; omit to keep, empty list to clear, exact Perspectives to replace."
    )]
    pub motivated_by: Option<Vec<String>>,
    #[serde(default)]
    #[schemars(description = "Optional space key; omit to use the Goal's owner.")]
    pub space: Option<String>,
    #[schemars(
        description = "Optional idempotency key (1-180 chars); same content replays, changed content conflicts."
    )]
    pub idempotency_key: Option<String>,
}

async fn goal_modify(ctx: ToolCtx, args: GoalModifyArgs) -> Result<GoalWriteOutput, ToolError> {
    let prior = ctx.resolve_goal(&args.goal)?;
    let owner = existing_goal_owner(&ctx, prior, args.space.as_deref()).await?;
    let payload = encode_goal_payload(&ctx, args.payload)?;
    let evidence = match args.evidence.as_ref() {
        Some(evidence) => Some(resolve_goal_evidence_handles(&ctx, evidence).await?),
        None => None,
    };
    let motivated_by = match args.motivated_by.as_ref() {
        Some(handles) => Some(resolve_goal_motivations(&ctx, handles)?),
        None => None,
    };
    let request_id = IdempotencyKey::optional_or_generated("goal_modify", args.idempotency_key)
        .map_err(ToolError::InvalidInput)?;
    let engine = ctx.engine();
    let outcome = engine
        .modify_goal(
            ctx.authz(),
            &GoalModifyRequest {
                owner,
                prior_goal_id: prior,
                replacement: payload,
                authorship: system_operator_authorship(&ctx, "goal_modify"),
                request_id,
                evidence,
                motivated_by,
                author_self_perspective_id: ctx.caller_self_perspective(),
            },
        )
        .await
        .map_err(ToolError::Protocol)?;
    Ok(format_goal_write_output(&ctx, &outcome))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GoalDecomposeArgs {
    #[schemars(description = "Parent Goal's current head handle.")]
    pub parent_goal: String,
    #[schemars(
        length(min = 1, max = 50),
        description = "1-50 child Goals; each starts Active. Parents close only after children close; achieving also needs one Achieved child."
    )]
    pub children: Vec<ChildGoalInput>,
    #[serde(default)]
    #[schemars(description = "Exact motivating Perspective version UUIDs for each child.")]
    pub motivated_by: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Optional space key; omit for the session default.")]
    pub space: Option<String>,
    #[schemars(
        description = "Idempotency key, at most 162 chars; each child key derives from it."
    )]
    pub idempotency_key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChildGoalInput {
    #[serde(flatten)]
    pub payload: GoalPayloadArgs,
    #[serde(default)]
    #[schemars(description = "Optional supporting Fact or Abstraction UUIDs.")]
    pub evidence: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GoalDecomposeOutput {
    pub parent_goal: String,
    pub children: Vec<GoalWriteOutput>,
    pub idempotent_replay: bool,
}

async fn goal_decompose(
    ctx: ToolCtx,
    args: GoalDecomposeArgs,
) -> Result<GoalDecomposeOutput, ToolError> {
    // The root key is checked first, against the bound its child keys need,
    // so an over-long key is a root-key error before anything is resolved
    // or any child is built.
    let root_key = IdempotencyKey::new_bounded(
        args.idempotency_key,
        DECOMPOSE_ROOT_KEY_MAX,
        "(child keys append \":<index>\")",
    )
    .map_err(ToolError::InvalidInput)?;
    if args.children.is_empty() {
        return Err(ToolError::InvalidInput(
            "children must contain at least one child goal".into(),
        ));
    }
    if args.children.len() > MAX_CHILD_GOALS {
        return Err(ToolError::InvalidInput(format!(
            "children must contain at most {MAX_CHILD_GOALS} child goals"
        )));
    }
    let space = super::memory_spaces::resolve_space_owner(
        &ctx,
        args.space.as_deref(),
        super::memory_spaces::SpaceDefault::Current,
    )?;
    let parent = ctx.resolve_goal(&args.parent_goal)?;
    let motivated_by = resolve_goal_motivations(&ctx, &args.motivated_by)?;
    let topology = GoalTopologyWrite::new(motivated_by, Vec::new(), Vec::new())
        .map_err(ToolError::Protocol)?;
    let mut children = Vec::with_capacity(args.children.len());
    for (index, child) in args.children.into_iter().enumerate() {
        children.push(ChildGoalDraft {
            payload: encode_goal_payload(&ctx, child.payload)?,
            evidence: resolve_goal_evidence_handles(&ctx, &child.evidence).await?,
            // Cannot fail: the root bound leaves room for every child key.
            request_id: root_key
                .child(DECOMPOSE_CHILD_PREFIX, index)
                .map_err(|reason| {
                    ToolError::Other(format!("decompose child key invariant: {reason}"))
                })?,
        });
    }
    let engine = ctx.engine();
    let outcome = engine
        .decompose_goal(
            ctx.authz(),
            &GoalDecomposeRequest {
                owner: space.owner,
                parent_goal_id: parent,
                authorship: system_operator_authorship(&ctx, "goal_decompose"),
                topology,
                children,
                author_self_perspective_id: ctx.caller_self_perspective(),
            },
        )
        .await
        .map_err(ToolError::Protocol)?;
    Ok(GoalDecomposeOutput {
        parent_goal: parent.into_inner().to_string(),
        children: outcome
            .children
            .into_iter()
            .map(|child| format_goal_write_output(&ctx, &child.outcome))
            .collect(),
        idempotent_replay: outcome.idempotent_replay,
    })
}

pub(crate) fn encode_goal_payload(
    ctx: &ToolCtx,
    args: GoalPayloadArgs,
) -> Result<GoalPayloadWrite, ToolError> {
    let title = validate_trimmed_len("goal title", &args.title, 240)?;
    let text = validate_trimmed_len("goal text", &args.text, 20_000)?;
    let schema_id = SchemaId::new(args.schema_id);
    let schema_version = SchemaVersion::new(args.schema_version.unwrap_or(1));
    let schema = ctx
        .registry()
        .lookup_payload(&schema_id, schema_version, PayloadKind::Goal)
        .ok_or_else(|| {
            ToolError::InvalidInput(format!(
                "unregistered GoalPayload schema {} v{}",
                schema_id.as_str(),
                schema_version.into_inner(),
            ))
        })?;
    if !args.body.is_object() {
        return Err(ToolError::InvalidInput(format!(
            "body for GoalPayload schema {} v{} must be a JSON object",
            schema_id.as_str(),
            schema_version.into_inner(),
        )));
    }
    let protocol_payload = ctx
        .registry()
        .ingest_protocol_payload(&schema_id, schema_version, PayloadKind::Goal, &args.body)
        .map_err(ToolError::InvalidInput)?;
    let sidecar_payload = schema
        .sidecar_table
        .is_some()
        .then_some(protocol_payload.sidecar_payload);
    let payload_bytes = protocol_payload.key_bytes.ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "GoalPayload schema {} v{} did not produce key bytes",
            schema_id.as_str(),
            schema_version.into_inner(),
        ))
    })?;
    Ok(GoalPayloadWrite {
        schema_id,
        schema_version,
        title: title.to_string(),
        text: text.to_string(),
        payload: payload_bytes,
        sidecar_payload,
    })
}

/// Parse supplied Goal evidence handles (bare UUIDs). Storage checks each of
/// this store is a live owned Fact or Abstraction in the write transaction; one
/// issued by another store is read there as the caller, refused when the store
/// is unavailable or the caller may not read it, and named by the read.
pub(crate) async fn resolve_goal_evidence_handles(
    ctx: &ToolCtx,
    evidence: &[String],
) -> Result<Vec<GoalEvidenceRef>, ToolError> {
    let ids = evidence
        .iter()
        .map(|handle| ctx.resolve_memory(handle))
        .collect::<Result<Vec<_>, _>>()?;
    let store_number = ctx.engine().store_number();
    // The remote reads run together under one call-wide deadline, so a goal
    // naming several memories of other stores takes the slowest read.
    let remote_ids: Vec<uuid::Uuid> = ids
        .iter()
        .map(|id| id.into_inner())
        .filter(|id| crate::memory_store_number(*id) != store_number)
        .collect();
    let mut remote = super::other_stores::resolve_remote_origins(ctx, &remote_ids)
        .await?
        .into_iter();
    Ok(ids
        .into_iter()
        .map(|id| {
            if crate::memory_store_number(id.into_inner()) == store_number {
                GoalEvidenceRef::new(id)
            } else {
                GoalEvidenceRef::remote(remote.next().expect("one origin per remote evidence"))
            }
        })
        .collect())
}

fn resolve_goal_motivations(
    ctx: &ToolCtx,
    handles: &[String],
) -> Result<Vec<GoalMotivationRef>, ToolError> {
    handles
        .iter()
        .map(|raw| ctx.resolve_memory(raw).map(GoalMotivationRef::perspective))
        .collect::<Result<Vec<_>, _>>()
}

/// Operator identity for a Goal write.
///
/// Deliberately *not* routed through
/// [`operator_label`](super::memory::util::operator_label), unlike every
/// authoring path that records a label. Nothing here is persisted as a
/// label: `SystemOrigin::Operator` has no goal-authorship column, and
/// replay collapses it to `system_operator`, so the only thing the label
/// reaches is the v5 hash below. Validating it would buy no provenance and
/// would turn a blank or over-long request-context `model_id` into a
/// refused `goal_set` — a caller told its intent was rejected over a field
/// the goal never keeps.
///
/// The bound identity already reaches this: `ctx.caller().model_id` is the
/// trusted id when the token binds one.
///
/// The input-contract label (`fact-or-abstraction-evidence-v1`, #513) feeds
/// only that hash: no stored column carries it, so renaming it changes no
/// replay.
pub(crate) fn system_operator_authorship(ctx: &ToolCtx, prompt_version: &str) -> GoalAuthorship {
    let model_id = &ctx.caller().model_id;
    let operator_key = format!(
        "{}\0{}\0{}\0{}",
        ctx.caller().client_name,
        ctx.caller().client_version,
        model_id,
        prompt_version
    );
    let operator_id = uuid::Uuid::new_v5(&MCP_OPERATOR_NAMESPACE, operator_key.as_bytes());
    let input_contract_id = uuid::Uuid::new_v5(
        &MCP_GOAL_INPUT_CONTRACT_NAMESPACE,
        format!(
            "{}:{prompt_version}:fact-or-abstraction-evidence-v1",
            protocol_tool::CORE_GOAL
        )
        .as_bytes(),
    );
    GoalAuthorship::System(SystemOrigin::Operator {
        operator_id: OperatorId::new(operator_id),
        operator_kind: OperatorKind::AtoGoal,
        input_contract_id: InputContractId::new(input_contract_id),
        model_id: ModelId::new(model_id.clone()),
        prompt_version: PromptVersion::new(prompt_version),
    })
}

fn format_goal_write_output(_ctx: &ToolCtx, outcome: &GoalWriteOutcome) -> GoalWriteOutput {
    GoalWriteOutput {
        handle: outcome.goal_id.into_inner().to_string(),
        goal_handle: outcome.goal_handle.into_inner().to_string(),
        edge_count: outcome.edge_count,
        idempotent_replay: outcome.idempotent_replay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthPath, AuthzContext, MemoryId, OwnerId, SpiiceRegistry, UserId};
    use std::sync::Arc;

    fn test_ctx() -> ToolCtx {
        let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
        crate::test_fixtures::tool_ctx_with(
            owner,
            AuthzContext::single_owner(&owner, AuthPath::HostBearer),
            Arc::new(SpiiceRegistry::new().freeze_or_panic_for_tests()),
            crate::SpiiceServices::default(),
            crate::ToolCaller::new("m", "c", "0"),
        )
    }

    fn valid_goal_payload_args() -> GoalPayloadArgs {
        GoalPayloadArgs {
            schema_id: "core/simple-text-v1".into(),
            schema_version: None,
            title: "test goal".into(),
            text: "test goal body".into(),
            body: serde_json::json!({}),
        }
    }

    /// `children` valid children under a fresh parent, keyed by `root`;
    /// child 0 gets an unresolvable evidence handle when `bad_first`.
    fn decompose_args(
        root: String,
        children: usize,
        bad_first: bool,
        target: MemoryId,
    ) -> GoalDecomposeArgs {
        GoalDecomposeArgs {
            parent_goal: uuid::Uuid::now_v7().to_string(),
            children: (0..children)
                .map(|index| ChildGoalInput {
                    payload: valid_goal_payload_args(),
                    evidence: vec![if bad_first && index == 0 {
                        format!("F:{}", uuid::Uuid::now_v7())
                    } else {
                        crate::new_memory_uuid(0).to_string()
                    }],
                })
                .collect(),
            motivated_by: vec![target.into_inner().to_string()],
            space: None,
            idempotency_key: root,
        }
    }

    /// #479 item 5: a root key one char over the documented bound is refused
    /// up front with a root-key error, before any child is built: not on a
    /// child's evidence, not on a child key at index 10, and not silently
    /// accepted when few children keep every child key short enough.
    #[tokio::test]
    async fn decompose_root_key_one_over_bound_is_refused_before_children() {
        let ctx = test_ctx();
        let target = MemoryId::new(uuid::Uuid::now_v7());
        for (children, bad_first) in [(50, true), (50, false), (1, true), (1, false)] {
            let err = goal_decompose(
                ctx.clone(),
                decompose_args("k".repeat(163), children, bad_first, target),
            )
            .await
            .expect_err("an over-long root key is refused");
            assert!(
                matches!(err, ToolError::InvalidInput(ref message)
                    if message.starts_with("idempotency_key must be at most 162 chars")),
                "{children} children, bad first evidence {bad_first}: {err:?}"
            );
        }
    }

    #[test]
    fn decompose_root_bound_is_derived() {
        assert_eq!(DECOMPOSE_ROOT_KEY_MAX, 162);
        assert_eq!(decimal_digits(0), 1);
        assert_eq!(decimal_digits(49), 2);
        assert_eq!(decimal_digits(100), 3);
        let schema = serde_json::to_value(schemars::schema_for!(GoalDecomposeArgs))
            .expect("schema serializes");
        let description = schema["properties"]["idempotency_key"]["description"]
            .as_str()
            .expect("the root key is described");
        assert!(
            description.contains(&format!("at most {DECOMPOSE_ROOT_KEY_MAX} chars")),
            "the advertised bound is the enforced one: {description}"
        );
    }

    #[tokio::test]
    async fn goal_evidence_rejects_prefixed_ids_before_storage() {
        let ctx = test_ctx();
        for prefix in ['F', 'A', 'P', 'G'] {
            let raw = format!("{prefix}:{}", uuid::Uuid::now_v7());
            let err = resolve_goal_evidence_handles(&ctx, &[raw])
                .await
                .expect_err("bare UUID required");
            assert!(matches!(err, ToolError::InvalidInput(_)), "{err}");
        }
    }

    /// The authorship every direct Goal action hands storage: a system
    /// operator that carries the request's model and the action's prompt
    /// version, and a different operator per prompt version.
    #[test]
    fn operator_authorship_carries_the_model_and_the_prompt_version() {
        let ctx = test_ctx();
        let mut operators = std::collections::HashSet::new();
        for prompt in ["goal_set", "goal_modify", "goal_decompose"] {
            let GoalAuthorship::System(SystemOrigin::Operator {
                operator_id,
                prompt_version,
                model_id,
                ..
            }) = system_operator_authorship(&ctx, prompt)
            else {
                panic!("{prompt}: expected operator authorship");
            };
            assert_eq!(prompt_version.as_str(), prompt);
            assert_eq!(model_id.as_str(), ctx.caller().model_id);
            assert!(operators.insert(operator_id), "{prompt}: operator reused");
        }
    }
}
