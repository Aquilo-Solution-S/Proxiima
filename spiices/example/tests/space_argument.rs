//! A spiice tool that takes a `space` argument resolves it with the SDK's
//! resolver, the one the scope gate judges the call with: the gate judges
//! the caller's role on the owner the tool then acts on.
//!
//! The two tools live here, not in the example's served catalog. Each is
//! called through the host's whole path, request behaviors first, over an
//! engine that refuses storage: neither tool touches it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use proxiima::spiice::{SpiiceRegistry, ToolCaller};
use proxiima::testkit::rejecting_engine;
use proxiima::{AuthContext, AuthPath, AuthzContext, GroupId, OwnerId, Role, UserId};
use proxiima::{SpiiceServices, ToolHost, ToolInvocationError};
use proxiima_spiice::{
    ProjectionDecl, Replay, SpiiceContract, Tool, ToolContract, ToolCtx, ToolEffect, ToolError,
    resolve_space_for_read, resolve_space_for_write,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

const READ_TOOL: &str = "proxiima-example_space_read";
const WRITE_TOOL: &str = "proxiima-example_space_write";

/// The argument both tools take: the gate recognises a top-level `space`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SpaceArgs {
    /// Space key from `core_memory_spaces`; omitted, the current owner.
    #[serde(default)]
    space: Option<String>,
}

/// The owner the tool acts on, and the key it answers for it.
#[derive(Debug, Serialize, JsonSchema)]
struct ActedOn {
    /// The resolved owner's id.
    owner: String,
    /// The resolved space key.
    key: String,
}

type Reply = Pin<Box<dyn Future<Output = Result<ActedOn, ToolError>> + Send>>;

#[derive(Debug)]
struct SpaceReadTool;

impl Tool for SpaceReadTool {
    const NAME: &'static str = READ_TOOL;
    const TITLE: &'static str = "Read a space";
    const DESCRIPTION: &'static str = "Report the owner a space key names, for a read.";
    const EFFECT: Option<ToolEffect> = Some(ToolEffect::ReadOnly);
    type Args = SpaceArgs;
    type Output = ActedOn;

    fn call(ctx: ToolCtx, args: SpaceArgs) -> Reply {
        Box::pin(async move {
            let space = resolve_space_for_read(&ctx, args.space.as_deref())?;
            Ok(ActedOn {
                owner: space.owner.to_string(),
                key: space.key,
            })
        })
    }
}

#[derive(Debug)]
struct SpaceWriteTool;

impl Tool for SpaceWriteTool {
    const NAME: &'static str = WRITE_TOOL;
    const TITLE: &'static str = "Write a space";
    const DESCRIPTION: &'static str = "Report the owner a space key names, for a write.";
    const EFFECT: Option<ToolEffect> = Some(ToolEffect::Additive(Replay::Idempotent));
    type Args = SpaceArgs;
    type Output = ActedOn;

    fn call(ctx: ToolCtx, args: SpaceArgs) -> Reply {
        Box::pin(async move {
            // A write runs under the authority narrowed to the resolved owner.
            let (space, _authority) = resolve_space_for_write(&ctx, args.space.as_deref())?;
            Ok(ActedOn {
                owner: space.owner.to_string(),
                key: space.key,
            })
        })
    }
}

static SPACE_CONTRACT: SpiiceContract = SpiiceContract {
    retired_facts: &[],
    spiice_id: "proxiima-example",
    ordinal: 11,
    schemas: &[],
    state_surfaces: &[],
    scopes: &[],
    kernel_surfaces: &[],
    tools: &[
        ToolContract {
            wire_name: READ_TOOL,
        },
        ToolContract {
            wire_name: WRITE_TOOL,
        },
    ],
    resources: &[],
    bespoke_erase_legs: &[],
    bespoke_transfer_legs: &[],
    evolutions: &[],
    projection: ProjectionDecl::None {
        why: "the space tools register no search surface",
    },
};

proxiima_spiice::proxiima_spiice! {
    name = "proxiima-example",
    mcp_tools = [SpaceReadTool, SpaceWriteTool],
    contract = &SPACE_CONTRACT,
}

/// A caller bound to `bound`, holding `roles`, served by the host.
struct Caller {
    host: ToolHost,
    auth: AuthContext,
}

impl Caller {
    fn new(bound: OwnerId, roles: [(OwnerId, Role); 2]) -> Self {
        let mut registry = SpiiceRegistry::new();
        register(&mut registry).expect("the space tools register");
        let registry = Arc::new(registry.try_freeze().expect("and freeze"));
        let authz = AuthzContext::for_subject_with_role(
            UserId::new(Uuid::now_v7()),
            roles,
            AuthPath::HostBearer,
        );
        Self {
            host: ToolHost::from_engine(rejecting_engine(&registry), SpiiceServices::default()),
            auth: AuthContext {
                owner: bound,
                authz,
            },
        }
    }

    async fn call(
        &self,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ToolInvocationError> {
        self.host
            .call_tool(
                tool,
                args,
                ToolCaller::new("test", "test", "0"),
                Some(self.auth.clone()),
            )
            .await
    }
}

fn group() -> OwnerId {
    OwnerId::from(GroupId::new(Uuid::now_v7()))
}

fn acted_on(reply: &serde_json::Value) -> (String, String) {
    (
        reply["owner"].as_str().expect("owner").to_owned(),
        reply["key"].as_str().expect("key").to_owned(),
    )
}

/// The refusal's text with the key it names replaced, so refusals of
/// different keys compare.
fn refusal(err: &ToolInvocationError, key: &str) -> String {
    err.to_string().replace(key, "<key>")
}

#[tokio::test]
async fn the_gate_judges_the_named_space_and_the_tool_acts_on_it() {
    let (a, b) = (group(), group());
    let caller = Caller::new(a, [(a, Role::editor()), (b, Role::viewer())]);
    let b_key = b.to_string();

    let read = caller
        .call(READ_TOOL, json!({ "space": b_key }))
        .await
        .expect("a viewer on B reads B");
    assert_eq!(acted_on(&read), (b_key.clone(), b_key.clone()));

    let write = caller
        .call(WRITE_TOOL, json!({ "space": b_key }))
        .await
        .expect_err("judged on B, where the caller only views");
    assert!(
        matches!(write, ToolInvocationError::NotAuthorized(_)),
        "{write:?}"
    );

    let caller = Caller::new(a, [(a, Role::viewer()), (b, Role::editor())]);
    let write = caller
        .call(WRITE_TOOL, json!({ "space": b_key }))
        .await
        .expect("judged on B, where the caller edits");
    assert_eq!(acted_on(&write), (b_key.clone(), b_key));
}

#[tokio::test]
async fn without_space_the_tool_acts_on_the_bound_owner() {
    let (a, b) = (group(), group());
    let caller = Caller::new(a, [(a, Role::viewer()), (b, Role::editor())]);

    let read = caller.call(READ_TOOL, json!({})).await.expect("reads A");
    assert_eq!(acted_on(&read), (a.to_string(), "current".to_owned()));
    let write = caller
        .call(WRITE_TOOL, json!({}))
        .await
        .expect_err("judged on A, where the caller only views");
    assert!(
        matches!(write, ToolInvocationError::NotAuthorized(_)),
        "{write:?}"
    );
}

#[tokio::test]
async fn an_owner_the_caller_holds_no_role_on_reads_as_an_unknown_space() {
    let (a, b) = (group(), group());
    let caller = Caller::new(a, [(a, Role::admin()), (b, Role::admin())]);
    let stranger = group().to_string();
    let malformed = "no-such-space";

    for tool in [READ_TOOL, WRITE_TOOL] {
        let hidden = caller
            .call(tool, json!({ "space": stranger }))
            .await
            .expect_err("no role on the named owner");
        let unknown = caller
            .call(tool, json!({ "space": malformed }))
            .await
            .expect_err("no space at all");
        assert_eq!(
            refusal(&hidden, &stranger),
            refusal(&unknown, malformed),
            "{tool}: the refusal tells an owner without a role from no space at all"
        );
        assert!(
            matches!(
                &hidden,
                ToolInvocationError::Tool(ToolError::InvalidInput(_))
            ),
            "{hidden:?}"
        );
    }
}

#[tokio::test]
async fn a_key_that_is_no_space_is_refused_as_a_core_tool_refuses_it() {
    let a = group();
    let caller = Caller::new(a, [(a, Role::admin()), (group(), Role::admin())]);
    for key in [
        "world",
        "personal:not-a-uuid",
        &format!("group:{}", Uuid::now_v7()),
    ] {
        let core = caller
            .call(
                "core_recall",
                json!({ "question": "anything", "space": key }),
            )
            .await
            .expect_err("core refuses the key");
        for tool in [READ_TOOL, WRITE_TOOL] {
            let spiice = caller
                .call(tool, json!({ "space": key }))
                .await
                .expect_err("the spiice tool refuses it too");
            assert_eq!(spiice.to_string(), core.to_string(), "{tool}: {key}");
        }
    }
}

/// A host reaches the same resolver through the facade, as
/// `proxiima::spiice`; the types are the SDK's.
const _: fn(&ToolCtx, Option<&str>) -> ReadResolution = proxiima::spiice::resolve_space_for_read;
const _: fn(&ToolCtx, Option<&str>) -> WriteResolution = proxiima::spiice::resolve_space_for_write;
type ReadResolution = Result<proxiima::spiice::ResolvedMemorySpace, ToolError>;
type WriteResolution = Result<(proxiima_spiice::ResolvedMemorySpace, AuthzContext), ToolError>;
