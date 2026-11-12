use std::sync::Arc;

use futures::future::BoxFuture;

use super::ScopeGateBehavior;
use crate::mcp::{
    McpArgvActionSpec, McpAuthorContext, McpTool, McpToolAudience, McpToolCtx, McpToolError,
    McpToolErrorKind, OwnerNeed, Replay, ToolEffect,
};
use crate::{
    AuthPath, AuthzContext, OwnerId, ScopeKey, SpiiceRegistry, SpiiceServices, ToolScope, UserId,
};

fn palette(ids: &[&str]) -> ToolScope {
    ToolScope::Palette(
        ids.iter()
            .map(|id| ScopeKey::parse(id).expect("a scope key"))
            .collect(),
    )
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ArgvArgs {
    #[schemars(description = "command words followed by flags")]
    _argv: Vec<String>,
}

/// An argv-keyed dispatcher with two commands sharing a first word, so
/// the gate's derivation has to pick by longest prefix.
///
/// Mixed on purpose, and in the shape the field exists for: the tool
/// is writable because one command writes, and the read command says
/// so itself; the sibling declares a write.
struct ArgvTool;

impl McpTool for ArgvTool {
    const TITLE: &'static str = "Test tool";
    const NAME: &'static str = "proxiima-stub_cli";
    const DESCRIPTION: &'static str = "An argv-keyed fixture dispatcher.";
    const ARGV_ACTION_SPECS: &'static [McpArgvActionSpec] = &[
        McpArgvActionSpec {
            action: "approval",
            argv_prefix: &["approval"],
            effect: ToolEffect::ReadOnly,
            audience: McpToolAudience::Shared,
            need: OwnerNeed::Memory,
            title: "Test action",
        },
        McpArgvActionSpec {
            action: "approval-decide",
            argv_prefix: &["approval", "decide"],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: McpToolAudience::Shared,
            need: OwnerNeed::Memory,
            title: "Test action",
        },
        McpArgvActionSpec {
            action: "plan",
            argv_prefix: &["plan"],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: McpToolAudience::Shared,
            need: OwnerNeed::GoalWork,
            title: "Test action",
        },
    ];
    type Args = ArgvArgs;
    type Output = super::StubOutput;
    fn call(
        _: McpToolCtx,
        _: Self::Args,
    ) -> BoxFuture<'static, Result<Self::Output, McpToolError>> {
        Box::pin(async { Ok(super::StubOutput {}) })
    }
}

/// A group viewer under `ToolScope::All`: may read, may not write, so
/// the owner-role half of the gate is the only thing that can deny.
fn argv_viewer_ctx() -> McpToolCtx {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let owner = OwnerId::from(crate::GroupId::new(uuid::Uuid::now_v7()));
    let authz = AuthzContext::for_subject_with_role(
        subject,
        [(owner, crate::access::Role::viewer())],
        AuthPath::HostBearer,
    )
    .with_tool_scope(ToolScope::All);
    argv_ctx_with_authz(owner, authz)
}

fn argv_ctx(tool_scope: ToolScope) -> McpToolCtx {
    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let authz =
        AuthzContext::single_owner(&owner, AuthPath::HostBearer).with_tool_scope(tool_scope);
    argv_ctx_with_authz(owner, authz)
}

fn argv_ctx_with_authz(owner: OwnerId, authz: AuthzContext) -> McpToolCtx {
    let mut registry = SpiiceRegistry::new();
    registry.add_mcp_tool_or_panic_for_tests::<ArgvTool>("proxiima-stub");
    McpToolCtx {
        owner,
        authz,
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

/// The gate judges the DERIVED key, so `tools/list` (which advertises
/// leaves from the same specs) and `tools/call` share one vocabulary: a
/// palette holding the leaf admits the call, and the same palette
/// without it denies the same argv.
#[test]
fn enforce_scope_gates_the_derived_argv_action() {
    let args = serde_json::json!({ "argv": ["approval", "decide", "--id", "7"] });

    // Positive control: the leaf in the palette admits the call.
    ScopeGateBehavior::enforce_scope(
        ArgvTool::NAME,
        &args,
        &argv_ctx(palette(&[&format!("{}:approval-decide", ArgvTool::NAME)])),
    )
    .expect("a palette holding the derived leaf admits the call");

    // A palette holding only the SIBLING command's leaf denies this
    // one — proof the gate derived `approval-decide` by longest prefix
    // rather than settling for the one-word match.
    let err = ScopeGateBehavior::enforce_scope(
        ArgvTool::NAME,
        &args,
        &argv_ctx(palette(&[&format!("{}:approval", ArgvTool::NAME)])),
    )
    .expect_err("the sibling leaf must not admit the longer command");
    assert!(
        matches!(err, McpToolError::NotAuthorized(ref key)
            if key == &format!("{}:approval-decide", ArgvTool::NAME)),
        "got {err:?}",
    );
}

/// An argv dispatcher's commands are titled like a tagged one's.
#[test]
fn the_descriptor_titles_an_argv_dispatchers_commands() {
    let ctx = argv_ctx(ToolScope::All);
    let descriptor = ctx.registry.mcp_tool(ArgvTool::NAME).expect("registered");
    let key = ScopeKey::parse(&format!("{}:approval-decide", ArgvTool::NAME)).expect("a key");
    assert_eq!(descriptor.title_of(&key), Some("Test action"));
}

/// The recorded key is the one the gate derived by longest prefix.
#[test]
fn the_gate_records_the_derived_argv_action() {
    let ctx = argv_ctx(ToolScope::All);
    ScopeGateBehavior::enforce_scope(
        ArgvTool::NAME,
        &serde_json::json!({ "argv": ["approval", "decide", "--id", "7"] }),
        &ctx,
    )
    .expect("admitted");
    assert_eq!(
        ctx.judged.get().map(|key| key.to_string()),
        Some(format!("{}:approval-decide", ArgvTool::NAME))
    );
}

#[test]
fn an_undisclosed_dispatcher_refuses_before_argv_diagnostics() {
    for args in [
        serde_json::json!({}),
        serde_json::json!({"argv": 7}),
        serde_json::json!({"argv": ["undeclared"]}),
    ] {
        for keys in [
            vec![],
            vec![ArgvTool::NAME],
            vec!["proxiima-stub_cli:undeclared"],
        ] {
            let result =
                ScopeGateBehavior::enforce_scope(ArgvTool::NAME, &args, &argv_ctx(palette(&keys)));
            assert!(
                matches!(result, Err(McpToolError::NotAuthorized(_))),
                "{keys:?} {args}: {result:?}"
            );
        }
        let result = ScopeGateBehavior::enforce_scope(
            ArgvTool::NAME,
            &args,
            &argv_ctx(palette(&["proxiima-stub_cli:approval"])),
        );
        assert!(
            matches!(result, Err(McpToolError::InvalidInput(_))),
            "admitted caller keeps useful diagnostics: {result:?}"
        );
    }
}

#[test]
fn argv_delegated_commands_require_registered_leaf_keys() {
    let ctx = argv_ctx(ToolScope::All);
    for leaf in ["approval", "approval-decide"] {
        let key = format!("{}:{leaf}", ArgvTool::NAME);
        let command = crate::DelegatedCommand::parse(&key, &ctx.registry)
            .expect("a registered argv leaf can be delegated");
        assert_eq!(command.action().map(crate::ActionName::as_str), Some(leaf));
    }
    for invalid in [
        ArgvTool::NAME.to_owned(),
        format!("{}:unknown", ArgvTool::NAME),
    ] {
        assert!(crate::DelegatedCommand::parse(invalid, &ctx.registry).is_err());
    }
}

#[test]
fn argv_listing_and_calls_share_exact_caller_and_served_leaf_admission() {
    let read = format!("{}:approval", ArgvTool::NAME);
    let unknown = format!("{}:unknown", ArgvTool::NAME);
    for (scope, allowed) in [
        (ToolScope::All, true),
        (palette(&[&read]), true),
        (palette(&[ArgvTool::NAME]), false),
        (palette(&[&unknown]), false),
        (palette(&[]), false),
    ] {
        let mut ctx = argv_ctx(scope);
        ctx.authz = ctx
            .authz
            .with_served_surface(crate::ServedSurface::Palette(Vec::new()));
        let descriptor = ctx
            .registry
            .mcp_tool(ArgvTool::NAME)
            .expect("registered argv tool");
        assert_eq!(descriptor.advertised_by(ctx.authz.capabilities()), allowed);
        assert_eq!(
            descriptor.action_advertised_by(ctx.authz.capabilities(), "approval"),
            allowed
        );
        assert!(!descriptor.action_advertised_by(ctx.authz.capabilities(), "approval-decide"));
        assert!(!descriptor.action_advertised_by(ctx.authz.capabilities(), "unknown"));
        let result = ScopeGateBehavior::enforce_scope(
            ArgvTool::NAME,
            &serde_json::json!({"argv":["approval"]}),
            &ctx,
        );
        assert_eq!(result.is_ok(), allowed, "{result:?}");
    }
}

/// A read-capable-only owner keeps the read commands of a writable argv
/// dispatcher.
///
/// Each command declares its own effect, so a dispatcher whose commands
/// are mostly reads keeps its read surface for a viewer. The writing
/// sibling is the negative control.
#[test]
fn a_viewer_keeps_the_read_command_of_a_writable_argv_dispatcher() {
    let ctx = argv_viewer_ctx();

    ScopeGateBehavior::enforce_scope(
        ArgvTool::NAME,
        &serde_json::json!({ "argv": ["approval", "--list"] }),
        &ctx,
    )
    .expect("an argv action that declares read_only is callable by a viewer");

    let err = ScopeGateBehavior::enforce_scope(
        ArgvTool::NAME,
        &serde_json::json!({ "argv": ["approval", "decide", "--id", "7"] }),
        &ctx,
    )
    .expect_err("an argv action that declares nothing stays a write");
    assert!(
        matches!(err, McpToolError::NotAuthorized(ref key) if key == ArgvTool::NAME),
        "got {err:?}",
    );

    ScopeGateBehavior::enforce_scope(
        ArgvTool::NAME,
        &serde_json::json!({ "argv": ["plan", "--next"] }),
        &ctx,
    )
    .expect("an argv write declared Goal work is callable by a viewer member");
}

/// The closed set holds at the gate too: argv outside the declared
/// commands is a validation error, never a pass-through to the tool.
#[test]
fn enforce_scope_refuses_argv_outside_the_vocabulary() {
    let err = ScopeGateBehavior::enforce_scope(
        ArgvTool::NAME,
        &serde_json::json!({ "argv": ["unknown", "verb"] }),
        &argv_ctx(ToolScope::All),
    )
    .expect_err("unmatched argv is refused");
    assert_eq!(err.kind(), McpToolErrorKind::InvalidInput);

    // Positive control: the same scope serves a declared command.
    ScopeGateBehavior::enforce_scope(
        ArgvTool::NAME,
        &serde_json::json!({ "argv": ["approval"] }),
        &argv_ctx(ToolScope::All),
    )
    .expect("a declared command passes under All");
}
