use std::sync::{Arc, Mutex};

use super::*;
use crate::protocol::tool as protocol_tool;
use crate::{
    AuthPath, AuthzContext, McpAuthorContext, OwnerId, SpiiceRegistry, SpiiceServices, ToolScope,
    UserId,
};

fn palette(ids: &[&str]) -> ToolScope {
    ToolScope::Palette(
        ids.iter()
            .map(|id| ScopeKey::parse(id).expect("a scope key"))
            .collect(),
    )
}

#[derive(Debug)]
struct RecordingBehavior {
    label: &'static str,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl RequestBehavior for RecordingBehavior {
    async fn handle(&self, call: ToolCall, next: Next<'_>) -> Result<ToolReply, McpToolError> {
        self.calls.lock().expect("recording lock").push(self.label);
        next.run(call).await
    }
}

#[tokio::test]
async fn chain_runs_scope_gate_then_spiice_behavior_then_terminal() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let behaviors: Vec<Arc<dyn RequestBehavior>> = vec![
        Arc::new(ScopeGateBehavior),
        Arc::new(RecordingBehavior {
            label: "spiice",
            calls: calls.clone(),
        }),
    ];
    let terminal_calls = calls.clone();
    let terminal: TerminalDispatch<'_> = Box::new(move |call| {
        Box::pin(async move {
            terminal_calls
                .lock()
                .expect("recording lock")
                .push("terminal");
            Ok(ToolReply::Structured(serde_json::json!({
                "tool": call.name,
                "args": call.args,
            })))
        })
    });

    let ToolReply::Structured(output) = Next::new(&behaviors, terminal)
        .run(ToolCall {
            name: protocol_tool::CORE_SEARCH_MEMORIES.to_string(),
            args: serde_json::json!({ "query": "x" }),
            ctx: test_ctx(ToolScope::All),
        })
        .await
        .expect("chain output")
    else {
        panic!("the terminal answered structured");
    };

    assert_eq!(output["tool"], protocol_tool::CORE_SEARCH_MEMORIES);
    assert_eq!(
        calls.lock().expect("recording lock").as_slice(),
        ["spiice", "terminal"]
    );
}

/// A command outside caller scope is refused before the terminal runs.
#[tokio::test]
async fn scope_gate_denies_out_of_palette_before_terminal() {
    let behaviors: Vec<Arc<dyn RequestBehavior>> = vec![Arc::new(ScopeGateBehavior)];
    let terminal: TerminalDispatch<'_> = Box::new(|_call| {
        Box::pin(async {
            Ok(ToolReply::Structured(
                serde_json::json!({ "unexpected": true }),
            ))
        })
    });

    let err = Next::new(&behaviors, terminal)
        .run(ToolCall {
            name: protocol_tool::CORE_REMEMBER.to_string(),
            args: serde_json::json!({ "title": "t", "body": "b" }),
            ctx: test_ctx(ToolScope::Palette(Vec::new())),
        })
        .await
        .expect_err("scope denial");

    assert!(
        matches!(err, McpToolError::NotAuthorized(ref tool) if tool == protocol_tool::CORE_REMEMBER)
    );
    assert_eq!(err.kind(), super::super::McpToolErrorKind::InvalidRequest);
}

#[tokio::test]
async fn scope_gate_requires_exact_palette_match_for_flat_tools() {
    let action_only = ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_REMEMBER,
        &serde_json::json!({ "title": "t", "body": "b" }),
        &test_ctx(palette(&[&format!("{}:x", protocol_tool::CORE_REMEMBER)])),
    )
    .expect_err("flat tool requires exact palette entry");

    assert!(
        matches!(action_only, McpToolError::NotAuthorized(ref tool) if tool == protocol_tool::CORE_REMEMBER)
    );

    ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_REMEMBER,
        &serde_json::json!({ "title": "t", "body": "b" }),
        &test_ctx(palette(&[protocol_tool::CORE_REMEMBER])),
    )
    .expect("bare palette entry allows flat tool");
}

/// The request name is parsed once, at the gate. Text that is no scope
/// key is in no palette, so a palette refuses it; `All` holds everything
/// and lets it through to the tool, which then answers for itself.
#[test]
fn a_request_name_that_is_no_key_is_refused_by_a_palette_and_passes_all() {
    let args = serde_json::json!({});
    let palette_ctx = test_ctx(palette(&[
        protocol_tool::CORE_REMEMBER,
        "core_goal:set",
        "resource:memory",
    ]));
    let all_ctx = test_ctx(ToolScope::All);

    for name in [
        "",
        ":",
        "a:b:c",
        "core_goal:",
        ":set",
        "resource:",
        "resource:a:b",
        "core remember",
        "core_remember ",
        "a/b",
        "core_rémember",
    ] {
        let err = ScopeGateBehavior::enforce_scope(name, &args, &palette_ctx)
            .expect_err("a palette holds no key for this name");
        assert!(
            matches!(err, McpToolError::NotAuthorized(ref denied) if denied == name),
            "{name:?}: {err:?}"
        );
        ScopeGateBehavior::enforce_scope(name, &args, &all_ctx)
            .unwrap_or_else(|error| panic!("{name:?} under All: {error:?}"));
    }
}

fn judged(name: &str, args: &serde_json::Value, ctx: &McpToolCtx) -> Option<String> {
    let _ = ScopeGateBehavior::enforce_scope(name, args, ctx);
    ctx.judged.get().map(|key| key.to_string())
}

/// The gate is the one place the key is derived, and a host reads it
/// instead of deriving it a second time: a dispatcher's `tool:action`, a
/// flat tool's name, a core resource's key.
#[test]
fn the_gate_records_the_key_it_judged_for_each_kind_of_call() {
    let ctx = test_ctx(ToolScope::All);
    let none = serde_json::json!({});
    assert_eq!(
        judged(
            protocol_tool::CORE_GOAL,
            &serde_json::json!({ "action": "set" }),
            &ctx
        ),
        Some("core_goal:set".into())
    );
    assert_eq!(
        judged(protocol_tool::CORE_REMEMBER, &none, &ctx),
        Some(protocol_tool::CORE_REMEMBER.into())
    );
    assert_eq!(
        judged(crate::protocol::resource::MEMORY, &none, &ctx),
        Some(crate::protocol::resource::MEMORY.into())
    );
}

/// A call the gate refuses for its key carries that key, so a host that
/// records refusals names them as it names the calls it lets through.
#[test]
fn a_call_refused_for_its_key_carries_the_key() {
    let ctx = test_ctx(palette(&["core_remember"]));
    let set = serde_json::json!({ "action": "set" });
    assert!(matches!(
        ScopeGateBehavior::enforce_scope(protocol_tool::CORE_GOAL, &set, &ctx),
        Err(McpToolError::NotAuthorized(_))
    ));
    // The palette holds no leaf of the dispatcher, so it is not even
    // advertised: refused as a whole, on its bare name.
    assert_eq!(
        ctx.judged.get().map(|key| key.to_string()),
        Some("core_goal".into())
    );

    let ctx = test_ctx(palette(&["core_goal:transition"]));
    assert!(matches!(
        ScopeGateBehavior::enforce_scope(protocol_tool::CORE_GOAL, &set, &ctx),
        Err(McpToolError::NotAuthorized(ref key)) if key == "core_goal:set"
    ));
    assert_eq!(
        ctx.judged.get().map(|key| key.to_string()),
        Some("core_goal:set".into())
    );

    let ctx = test_ctx(palette(&["core_goal:set"]));
    assert!(
        ScopeGateBehavior::enforce_scope(
            protocol_tool::CORE_REMEMBER,
            &serde_json::json!({}),
            &ctx
        )
        .is_err()
    );
    assert_eq!(
        ctx.judged.get().map(|key| key.to_string()),
        Some(protocol_tool::CORE_REMEMBER.into())
    );
}

/// No key is derived for a call that names none: a dispatcher call
/// without an action, with an undeclared one, or a name that is no key.
#[test]
fn a_call_with_no_derivable_key_records_none() {
    let ctx = test_ctx(ToolScope::All);
    for args in [
        serde_json::json!({}),
        serde_json::json!({ "action": 7 }),
        serde_json::json!({ "action": "undeclared" }),
    ] {
        assert_eq!(
            judged(protocol_tool::CORE_GOAL, &args, &ctx),
            None,
            "{args}"
        );
    }
    assert_eq!(judged("a:b:c", &serde_json::json!({}), &ctx), None);
}

/// A context serves one call at a time: the gate clears the key on
/// entry, so a call that derives none never shows the previous call's.
#[test]
fn a_context_reused_for_a_second_call_never_shows_the_first_calls_key() {
    let ctx = test_ctx(ToolScope::All);
    assert!(judged(protocol_tool::CORE_REMEMBER, &serde_json::json!({}), &ctx).is_some());
    assert_eq!(judged("a:b:c", &serde_json::json!({}), &ctx), None);
}

#[derive(Debug)]
struct KeyReader {
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

#[async_trait]
impl RequestBehavior for KeyReader {
    async fn handle(&self, call: ToolCall, next: Next<'_>) -> Result<ToolReply, McpToolError> {
        let ctx = call.ctx.clone();
        let reply = next.run(call).await;
        self.seen
            .lock()
            .expect("reader lock")
            .push(ctx.judged.get().map(|key| key.to_string()));
        reply
    }
}

/// A behavior ahead of the gate reads the key from its own clone of the
/// context once the call returns, whether the gate let the call through
/// or refused it.
#[tokio::test]
async fn a_behavior_wrapping_the_gate_reads_the_key_after_the_call() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let behaviors: Vec<Arc<dyn RequestBehavior>> = vec![
        Arc::new(KeyReader { seen: seen.clone() }),
        Arc::new(ScopeGateBehavior),
    ];
    for scope in [ToolScope::All, palette(&["core_goal:transition"])] {
        let terminal: TerminalDispatch<'_> =
            Box::new(|_call| Box::pin(async { Ok(ToolReply::Structured(serde_json::json!({}))) }));
        let _ = Next::new(&behaviors, terminal)
            .run(ToolCall {
                name: protocol_tool::CORE_GOAL.to_string(),
                args: serde_json::json!({ "action": "set" }),
                ctx: test_ctx(scope),
            })
            .await;
    }
    assert_eq!(
        *seen.lock().expect("reader lock"),
        vec![Some("core_goal:set".to_string()); 2]
    );
}

/// A bare dispatcher key cannot grant one of its executable leaves.
#[test]
fn a_bare_dispatcher_entry_does_not_admit_a_call_of_its_action() {
    let args = serde_json::json!({ "action": "set" });

    let err = ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_GOAL,
        &args,
        &test_ctx(palette(&[protocol_tool::CORE_GOAL])),
    )
    .expect_err("the bare name is not the leaf");
    assert!(
        matches!(err, McpToolError::NotAuthorized(ref denied) if denied == protocol_tool::CORE_GOAL),
        "got {err:?}"
    );
}

#[test]
fn an_undisclosed_dispatcher_refuses_before_action_diagnostics() {
    for args in [
        serde_json::json!({}),
        serde_json::json!({"action": 7}),
        serde_json::json!({"action": "undeclared"}),
    ] {
        for keys in [vec![], vec!["core_goal"], vec!["core_goal:undeclared"]] {
            let result = ScopeGateBehavior::enforce_scope(
                protocol_tool::CORE_GOAL,
                &args,
                &test_ctx(palette(&keys)),
            );
            assert!(
                matches!(result, Err(McpToolError::NotAuthorized(_))),
                "{keys:?} {args}: {result:?}"
            );
        }
        let result = ScopeGateBehavior::enforce_scope(
            protocol_tool::CORE_GOAL,
            &args,
            &test_ctx(palette(&["core_goal:set"])),
        );
        assert!(
            matches!(result, Err(McpToolError::InvalidInput(_))),
            "admitted caller keeps useful diagnostics: {result:?}"
        );
    }
}

/// A deployment serves declared reads even when it configures no writes.
#[test]
fn an_empty_served_palette_admits_reads_and_refuses_writes() {
    let mut ctx = test_ctx(ToolScope::All);
    ctx.authz = ctx
        .authz
        .with_served_surface(crate::ServedSurface::Palette(Vec::new()));
    ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_SEARCH_MEMORIES,
        &serde_json::json!({ "query": "x" }),
        &ctx,
    )
    .expect("a read-only flat tool is served under every deployment palette");
    for resource in [
        crate::protocol::resource::MEMORY,
        crate::protocol::resource::HOW_TO,
    ] {
        ScopeGateBehavior::enforce_scope(resource, &serde_json::json!({}), &ctx)
            .expect("core resources and the guide are served under every deployment palette");
    }
    let err = ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_REMEMBER,
        &serde_json::json!({ "title": "t", "body": "b" }),
        &ctx,
    )
    .expect_err("a write outside the palette is refused");
    assert!(matches!(err, McpToolError::NotAuthorized(ref t) if t == protocol_tool::CORE_REMEMBER));
}

/// #462: a dispatcher is judged per action, never by its tool-level
/// join. `core_membership:list_members` reads, `core_membership:add_member` writes.
#[test]
fn a_mixed_dispatcher_admits_only_its_read_action_outside_the_palette() {
    let mut ctx = test_ctx(ToolScope::All);
    ctx.authz = ctx
        .authz
        .with_served_surface(crate::ServedSurface::Palette(Vec::new()));
    ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_MEMBERSHIP,
        &serde_json::json!({ "action": "list_members" }),
        &ctx,
    )
    .expect("the read-only action is admitted");
    let err = ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_MEMBERSHIP,
        &serde_json::json!({ "action": "add_member" }),
        &ctx,
    )
    .expect_err("the additive action is refused");
    assert!(matches!(err, McpToolError::NotAuthorized(_)), "got {err:?}");
}

/// An owner-audience read still needs its exact caller key.
#[test]
fn an_owner_audience_read_needs_the_palette() {
    let args = serde_json::json!({ "action": "list_members" });
    let err = ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_MEMBERSHIP,
        &args,
        &test_ctx(ToolScope::Palette(Vec::new())),
    )
    .expect_err("an owner-audience read omitted by the caller is refused");
    assert!(matches!(err, McpToolError::NotAuthorized(_)), "got {err:?}");
    ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_MEMBERSHIP,
        &args,
        &test_ctx(palette(&["core_membership:list_members"])),
    )
    .expect("the palette holds it");
}

#[test]
fn an_exact_caller_omits_reads_resources_and_the_guide() {
    let mut ctx = test_ctx(palette(&[protocol_tool::CORE_REMEMBER]));
    ctx.authz = ctx
        .authz
        .with_served_surface(crate::ServedSurface::Palette(Vec::new()));
    for (tool, args) in [
        (
            protocol_tool::CORE_SEARCH_MEMORIES,
            serde_json::json!({"query":"x"}),
        ),
        (crate::protocol::resource::MEMORY, serde_json::json!({})),
        (crate::protocol::resource::HOW_TO, serde_json::json!({})),
    ] {
        assert!(
            matches!(
                ScopeGateBehavior::enforce_scope(tool, &args, &ctx),
                Err(McpToolError::NotAuthorized(_))
            ),
            "{tool}"
        );
    }
}

#[test]
fn deployment_serves_an_owner_read_when_the_caller_holds_it() {
    let mut ctx = test_ctx(palette(&["core_membership:list_members"]));
    ctx.authz = ctx
        .authz
        .with_served_surface(crate::ServedSurface::Palette(Vec::new()));
    ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_MEMBERSHIP,
        &serde_json::json!({"action":"list_members"}),
        &ctx,
    )
    .expect("a declared owner read is mandatory deployment surface");
}

/// #462: undeclared is a write. A name nothing declares takes no read
/// path and stays refused by a palette that does not hold it.
#[test]
fn an_undeclared_name_outside_the_palette_is_refused() {
    let err = ScopeGateBehavior::enforce_scope(
        "undeclared_tool",
        &serde_json::json!({}),
        &test_ctx(ToolScope::Palette(Vec::new())),
    )
    .expect_err("silence is a write");
    assert!(matches!(err, McpToolError::NotAuthorized(ref t) if t == "undeclared_tool"));
}

#[test]
fn owner_role_gate_allows_read_tools_and_rejects_write_tools_for_viewer() {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let group = OwnerId::from(crate::GroupId::new(uuid::Uuid::now_v7()));
    let authz = AuthzContext::for_subject_with_role(
        subject,
        [(group, crate::Role::viewer())],
        AuthPath::HostBearer,
    )
    .narrowed_to_owner(group)
    .expect("viewer can bind readable group")
    .with_tool_scope(ToolScope::All);
    let ctx = test_ctx_with_authz(group, authz);

    ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_SEARCH_MEMORIES,
        &serde_json::json!({ "query": "x" }),
        &ctx,
    )
    .expect("viewer can call read-only search");

    ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_SEARCH_MEMORIES,
        &serde_json::json!({ "action": "unexpected", "query": "x" }),
        &ctx,
    )
    .expect("an unexpected field cannot reclassify a flat read as a dispatcher write");

    let err = ScopeGateBehavior::enforce_scope(
        protocol_tool::CORE_REMEMBER,
        &serde_json::json!({ "title": "t", "body": "b" }),
        &ctx,
    )
    .expect_err("viewer cannot call write tool");
    assert!(
        matches!(err, McpToolError::NotAuthorized(ref tool) if tool == protocol_tool::CORE_REMEMBER)
    );
}

/// Setting, modifying and transitioning a Goal are the Goal work
/// `core_goal` declares: a viewer member runs those and no other write
/// of the tool.
#[test]
fn core_goal_declares_set_modify_and_transition_goal_work() {
    let group = OwnerId::from(crate::GroupId::new(uuid::Uuid::now_v7()));
    let authz = AuthzContext::for_subject_with_role(
        UserId::new(uuid::Uuid::now_v7()),
        [(group, crate::Role::viewer())],
        AuthPath::HostBearer,
    );
    let ctx = test_ctx_with_authz(group, authz);
    let goal = ctx
        .registry
        .list_mcp_tools()
        .iter()
        .find(|tool| tool.name == protocol_tool::CORE_GOAL)
        .expect("core_goal is registered");
    let mut admitted = Vec::new();
    for (action, _) in goal.actions() {
        let declared = goal.declaration(Some(action)).owner_access();
        let runs = ScopeGateBehavior::enforce_scope(
            protocol_tool::CORE_GOAL,
            &serde_json::json!({ "action": action }),
            &ctx,
        )
        .is_ok();
        assert_eq!(runs, declared == OwnerAccess::GoalWork, "{action}");
        if runs {
            admitted.push(action);
        }
    }
    assert_eq!(admitted, ["set", "transition", "modify"]);
}

/// A host's Goal work declaration turns its write into Goal work and
/// leaves a read a read.
#[test]
fn a_host_goal_work_declaration_classifies_writes_only() {
    let write = ToolEffect::Destructive(Replay::NonIdempotent);
    assert_eq!(
        McpHostToolCall::new("host", write).owner_access(),
        OwnerAccess::Write
    );
    assert_eq!(
        McpHostToolCall::new("host", write)
            .with_goal_work()
            .owner_access(),
        OwnerAccess::GoalWork
    );
    assert_eq!(
        McpHostToolCall::new("host", ToolEffect::ReadOnly)
            .with_goal_work()
            .owner_access(),
        OwnerAccess::Read
    );
}

fn test_ctx(tool_scope: ToolScope) -> McpToolCtx {
    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let authz =
        AuthzContext::single_owner(&owner, AuthPath::HostBearer).with_tool_scope(tool_scope);
    test_ctx_with_authz(owner, authz)
}

/// A viewer on a group owner: may read, may not write.
fn read_only_ctx() -> McpToolCtx {
    let subject = UserId::new(uuid::Uuid::now_v7());
    let owner = OwnerId::from(crate::GroupId::new(uuid::Uuid::now_v7()));
    let authz = AuthzContext::for_subject_with_role(
        subject,
        [(owner, crate::access::Role::viewer())],
        AuthPath::HostBearer,
    )
    .with_tool_scope(ToolScope::All);
    assert!(
        authz.may_run_tool(&owner, OwnerAccess::Read),
        "viewer runs a read"
    );
    assert!(
        !authz.may_run_tool(&owner, OwnerAccess::Write),
        "viewer does not run a write"
    );
    test_ctx_with_authz(owner, authz)
}

/// `read_resource` runs through this same gate with the resource's
/// scope key as the tool name. Nothing in the tool manifest is keyed
/// by a `resource:` name; without a read exemption, lookup misses and
/// the default demands WRITE.
#[tokio::test]
async fn a_viewer_may_read_resources() {
    for scope_key in [
        crate::protocol::resource::SCHEMAS,
        crate::protocol::resource::MEMORIES,
        crate::protocol::resource::GRAPH,
    ] {
        let behaviors: Vec<Arc<dyn RequestBehavior>> = vec![Arc::new(ScopeGateBehavior)];
        let terminal: TerminalDispatch<'_> = Box::new(|_call| {
            Box::pin(async { Ok(ToolReply::Structured(serde_json::json!({"ok": true}))) })
        });
        let out = Next::new(&behaviors, terminal)
            .run(ToolCall {
                name: scope_key.to_string(),
                args: serde_json::json!({ "uri": "proxiima://schemas" }),
                ctx: read_only_ctx(),
            })
            .await;
        assert!(out.is_ok(), "{scope_key} must be readable by a viewer");
    }
}

/// Deployment read admission never grants a viewer a writing tool.
#[tokio::test]
async fn a_viewer_still_may_not_write() {
    let behaviors: Vec<Arc<dyn RequestBehavior>> = vec![Arc::new(ScopeGateBehavior)];
    let terminal: TerminalDispatch<'_> = Box::new(|_call| {
        Box::pin(async { Ok(ToolReply::Structured(serde_json::json!({"ok": true}))) })
    });
    let out = Next::new(&behaviors, terminal)
        .run(ToolCall {
            name: protocol_tool::CORE_REMEMBER.to_string(),
            args: serde_json::json!({ "text": "x" }),
            ctx: read_only_ctx(),
        })
        .await;
    assert!(out.is_err(), "core_remember must stay denied for a viewer");
}

/// An expired context runs no tool. The same calls reach the terminal
/// with the same context while it is live.
#[tokio::test]
async fn an_expired_context_runs_no_tool() {
    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let context = |expires_at| {
        test_ctx_with_authz(
            owner,
            AuthzContext::single_owner(&owner, AuthPath::HostBearer)
                .with_tool_scope(ToolScope::All)
                .with_expires_at(Some(expires_at)),
        )
    };
    let live = || context(std::time::SystemTime::now() + std::time::Duration::from_mins(5));
    let expired = || context(std::time::SystemTime::UNIX_EPOCH);
    let run = |name: &str, ctx: McpToolCtx| {
        let name = name.to_string();
        async move {
            let behaviors: Vec<Arc<dyn RequestBehavior>> = vec![Arc::new(ScopeGateBehavior)];
            let terminal: TerminalDispatch<'_> = Box::new(|_call| {
                Box::pin(async { Ok(ToolReply::Structured(serde_json::json!({"ok": true}))) })
            });
            Next::new(&behaviors, terminal)
                .run(ToolCall {
                    name,
                    args: serde_json::json!({ "text": "x" }),
                    ctx,
                })
                .await
        }
    };
    for access in [OwnerAccess::Read, OwnerAccess::Write, OwnerAccess::GoalWork] {
        assert!(!expired().authz.may_run_tool(&owner, access), "{access:?}");
    }

    let read = crate::protocol::resource::SCHEMAS;
    let write = protocol_tool::CORE_REMEMBER;
    assert!(
        run(read, live()).await.is_ok(),
        "live read reaches the tool"
    );
    assert!(
        run(write, live()).await.is_ok(),
        "live write reaches the tool"
    );
    // The read is refused by the owner-role gate. The write never gets
    // that far: owner-space resolution, which asks the same context,
    // finds the owner inaccessible first.
    assert!(
        matches!(run(read, expired()).await, Err(McpToolError::NotAuthorized(ref t)) if t == read)
    );
    assert!(
        matches!(run(write, expired()).await, Err(McpToolError::InvalidInput(ref m))
            if m == "current memory space is not accessible")
    );
}

fn test_ctx_with_authz(owner: OwnerId, authz: AuthzContext) -> McpToolCtx {
    McpToolCtx {
        owner,
        authz,
        registry: Arc::new(SpiiceRegistry::new().freeze_or_panic_for_tests()),
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
