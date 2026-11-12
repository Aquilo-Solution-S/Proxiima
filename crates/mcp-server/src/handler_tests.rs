use super::*;
use proxiima_core::mcp::Replay;
use proxiima_core::protocol::{
    action as protocol_action, resource as protocol_resource, tool as protocol_tool,
};
use proxiima_core::{ServedSurface, ToolScope};

fn palette(ids: &[&str]) -> ToolScope {
    ToolScope::Palette(
        ids.iter()
            .map(|id| ScopeKey::parse(id).expect("a scope key"))
            .collect(),
    )
}

// Retain the existing guard and JSON-RPC error assertions through the
// shared host validator and the production transport error mapper.
fn reject_nul_in_args(args: &serde_json::Value) -> Result<(), ErrorData> {
    crate::server::reject_nul_in_args(args).map_err(|error| mcp_tool_error_to_error_data(&error))
}

fn spiice_descriptor(name: &'static str, effect: Option<ToolEffect>) -> McpToolDescriptor {
    McpToolDescriptor {
        name,
        description: "stub",
        origin: proxiima_core::mcp::McpToolOrigin::Spiice("proxiima-stub".to_string()),
        produces_schema_ids: &[],
        args_schema: serde_json::json!({"type": "object"}),
        dispatcher_schema: None,
        output_schema: serde_json::json!({"type": "object"}),
        action_arg_specs: &[],
        argv_action_specs: &[],
        effect,
        audience: proxiima_core::mcp::McpToolAudience::Shared,
        call: &|_, _| Box::pin(async { Ok(serde_json::json!({})) }),
        title: "Test tool",
    }
}

const MIXED_ACTIONS: &[proxiima_core::mcp::McpActionArgSpec] = &[
    proxiima_core::mcp::McpActionArgSpec {
        action: "look",
        allowed_fields: &["id"],
        required_fields: &["id"],
        effect: ToolEffect::ReadOnly,
        audience: proxiima_core::mcp::McpToolAudience::Shared,
        need: proxiima_core::mcp::OwnerNeed::Memory,
        title: "Test action",
    },
    proxiima_core::mcp::McpActionArgSpec {
        action: "touch",
        allowed_fields: &["id"],
        required_fields: &["id"],
        effect: ToolEffect::Additive(Replay::NonIdempotent),
        audience: proxiima_core::mcp::McpToolAudience::Shared,
        need: proxiima_core::mcp::OwnerNeed::Memory,
        title: "Test action",
    },
];

/// The argv twin of [`MIXED_ACTIONS`]: one command that reads, one that
/// writes.
const MIXED_ARGV_ACTIONS: &[proxiima_core::mcp::McpArgvActionSpec] = &[
    proxiima_core::mcp::McpArgvActionSpec {
        action: "approval",
        argv_prefix: &["approval"],
        effect: ToolEffect::ReadOnly,
        audience: proxiima_core::mcp::McpToolAudience::Shared,
        need: proxiima_core::mcp::OwnerNeed::Memory,
        title: "Test action",
    },
    proxiima_core::mcp::McpArgvActionSpec {
        action: "approval-decide",
        argv_prefix: &["approval", "decide"],
        effect: ToolEffect::Additive(Replay::NonIdempotent),
        audience: proxiima_core::mcp::McpToolAudience::Shared,
        need: proxiima_core::mcp::OwnerNeed::Memory,
        title: "Test action",
    },
];

/// Listing uses the call gate's predicate. Under an empty served palette
/// an undeclared flat tool is not listed, a read-only one is, and a
/// mixed dispatcher shows its read action only.
#[test]
fn listing_under_an_empty_served_palette_shows_reads_only() {
    let mut auth = full_auth(ToolScope::All);
    auth.authz = auth
        .authz
        .with_served_surface(ServedSurface::Palette(Vec::new()));
    let caller = Caller::new(Some(&auth));

    assert!(!tool_allowed_for_auth(
        caller,
        &spiice_descriptor("stub_undeclared", None)
    ));
    assert!(!tool_allowed_for_auth(
        caller,
        &spiice_descriptor(
            "stub_write",
            Some(ToolEffect::Additive(Replay::NonIdempotent))
        )
    ));
    assert!(tool_allowed_for_auth(
        caller,
        &spiice_descriptor("stub_read", Some(ToolEffect::ReadOnly))
    ));

    let mut mixed = spiice_descriptor("stub_mixed", None);
    mixed.action_arg_specs = MIXED_ACTIONS;
    assert!(tool_allowed_for_auth(caller, &mixed));
    assert!(action_allowed_for_auth(caller, &mixed, "look"));
    assert!(!action_allowed_for_auth(caller, &mixed, "touch"));

    // Owner audience is still a registered read; owner authorization
    // remains independent of deployment command selection.
    let mut owner_read = spiice_descriptor("stub_owner_read", Some(ToolEffect::ReadOnly));
    owner_read.audience = proxiima_core::mcp::McpToolAudience::Owner;
    assert!(tool_allowed_for_auth(caller, &owner_read));
    let held = full_auth(ToolScope::Palette(vec![
        ScopeKey::parse("stub_owner_read").expect("a scope key"),
    ]));
    assert!(tool_allowed_for_auth(Caller::new(Some(&held)), &owner_read));

    for resource in all_core_resources() {
        assert!(
            resource_allowed_for_auth(Some(&auth), resource),
            "{} is a read and is listed",
            resource.scope_key
        );
    }
}

#[test]
fn an_empty_caller_palette_hides_every_read_resource_and_guide() {
    let auth = full_auth(ToolScope::Palette(Vec::new()));
    let caller = Caller::new(Some(&auth));
    assert!(!tool_allowed_for_auth(
        caller,
        &spiice_descriptor("stub_read", Some(ToolEffect::ReadOnly))
    ));
    let mut owner_read = spiice_descriptor("stub_owner_read", Some(ToolEffect::ReadOnly));
    owner_read.audience = proxiima_core::mcp::McpToolAudience::Owner;
    assert!(!tool_allowed_for_auth(caller, &owner_read));
    let mut mixed = spiice_descriptor("stub_mixed", None);
    mixed.action_arg_specs = MIXED_ACTIONS;
    assert!(!tool_allowed_for_auth(caller, &mixed));
    assert!(!action_allowed_for_auth(caller, &mixed, "look"));
    assert!(!action_allowed_for_auth(caller, &mixed, "touch"));
    assert!(all_core_resources().all(|resource| !resource_allowed_for_auth(Some(&auth), resource)));
    assert!(!how_to_allowed_for_auth(Some(&auth)));
    assert!(advertised_resource_scope_keys(Some(&auth)).is_empty());
    let host_read = McpHostTool::new(
        "host_read",
        "read",
        serde_json::json!({}),
        ToolEffect::ReadOnly,
    );
    assert!(!host_tool_allowed_for_auth(caller, &host_read));
}

/// A resource is listed as it is read: the palette admits it and the
/// owner role allows what it declares. A caller with no role on the owner
/// is shown none, under any palette; a resource that is no read needs
/// the palette.
#[test]
fn a_resource_is_listed_only_where_it_is_admitted() {
    let owner = proxiima_core::OwnerId::from(proxiima_core::UserId::new(uuid::Uuid::now_v7()));
    let stranger = McpAuthContext {
        owner,
        authz: proxiima_core::AuthzContext::for_subject(
            proxiima_core::UserId::new(uuid::Uuid::now_v7()),
            proxiima_core::AuthPath::HostBearer,
        )
        .with_tool_scope(ToolScope::All),
    };
    for resource in all_core_resources() {
        assert!(
            !resource_allowed_for_auth(Some(&stranger), resource),
            "{} is not listed to a caller with no role",
            resource.scope_key
        );
    }

    let memory = all_core_resources()
        .find(|resource| resource.scope_key == protocol_resource::MEMORY)
        .expect("the memory resource");
    let write = proxiima_core::spiice::ResourceContract {
        read_only: false,
        ..*memory
    };
    let mut auth = full_auth(ToolScope::All);
    auth.authz = auth
        .authz
        .with_served_surface(ServedSurface::Palette(Vec::new()));
    assert!(resource_allowed_for_auth(Some(&auth), memory));
    assert!(!resource_allowed_for_auth(Some(&auth), &write));
    let empty_caller = full_auth(ToolScope::Palette(Vec::new()));
    assert!(!resource_allowed_for_auth(Some(&empty_caller), memory));
    assert!(!resource_allowed_for_auth(Some(&empty_caller), &write));
    let held = full_auth(ToolScope::Palette(vec![ScopeKey::Resource(write.key())]));
    assert!(resource_allowed_for_auth(Some(&held), &write));
}

/// A host tool declared Goal work is listed to a viewer member, who
/// runs it; an undeclared host write is not.
#[test]
fn a_declared_goal_work_host_tool_is_listed_to_a_viewer_member() {
    let group = proxiima_core::OwnerId::from(proxiima_core::GroupId::new(uuid::Uuid::now_v7()));
    let viewer = McpAuthContext {
        owner: group,
        authz: proxiima_core::AuthzContext::for_subject_with_role(
            proxiima_core::UserId::new(uuid::Uuid::now_v7()),
            [(group, proxiima_core::Role::viewer())],
            proxiima_core::AuthPath::HostBearer,
        ),
    };
    let write = ToolEffect::Additive(proxiima_core::Replay::NonIdempotent);
    let host_write = McpHostTool::new("host_write", "write", serde_json::json!({}), write);
    let host_goal = host_write.clone().with_goal_work();
    let caller = Caller::new(Some(&viewer));
    assert!(host_tool_allowed_for_auth(caller, &host_goal));
    assert!(!host_tool_allowed_for_auth(caller, &host_write));
}

fn full_auth(scope: ToolScope) -> McpAuthContext {
    let owner = proxiima_core::OwnerId::from(proxiima_core::UserId::new(uuid::Uuid::now_v7()));
    McpAuthContext {
        owner,
        authz: proxiima_core::AuthzContext::single_owner(
            &owner,
            proxiima_core::AuthPath::HostBearer,
        )
        .with_tool_scope(scope),
    }
}

/// An auth context whose token binds a model identity, as the OIDC
/// subject map produces for a configured runner principal.
fn trusted_auth(trusted_model_id: &str) -> McpAuthContext {
    let owner = proxiima_core::OwnerId::from(proxiima_core::UserId::new(uuid::Uuid::now_v7()));
    McpAuthContext {
        owner,
        authz: proxiima_core::AuthzContext::single_owner(
            &owner,
            proxiima_core::AuthPath::HostBearer,
        )
        .with_trusted_model_id(trusted_model_id)
        .expect("a well-formed runner id binds"),
    }
}

/// Fails when an rmcp upgrade knows a revision newer than the one
/// Proxiima serves: implement the new revision's server obligations
/// (list cache hints, lifecycle, headers, errors), then raise
/// `MAX_PROTOCOL_VERSION`. Admitting it by default is how AQS/aquilo#9484
/// happened.
#[test]
fn the_protocol_ceiling_is_rmcps_newest_revision() {
    assert_eq!(
        ProtocolVersion::KNOWN_VERSIONS.last(),
        Some(&MAX_PROTOCOL_VERSION),
        "rmcp knows a revision newer than MAX_PROTOCOL_VERSION"
    );
}

#[test]
fn author_from_args_extracts_caller_self_perspective() {
    let self_id = uuid::Uuid::now_v7();
    let args = serde_json::json!({
        "model_id": "test-model",
        "_proxiima_caller_self_perspective": self_id.to_string(),
    });

    let author = author_from_args(&args, None, "unknown", "0").expect("caller");

    assert_eq!(author.model_id, "test-model");
    assert_eq!(
        author.caller_self_perspective.map(MemoryId::into_inner),
        Some(self_id)
    );
}

#[test]
fn strip_call_context_args_removes_reserved_metadata() {
    let mut args = serde_json::json!({
        "payload": {},
        "_proxiima_caller_self_perspective": uuid::Uuid::now_v7().to_string(),
        "caller_self_perspective": uuid::Uuid::now_v7().to_string(),
        "current_root_perspective_memory_id": uuid::Uuid::now_v7().to_string(),
    });

    strip_call_context_args(&mut args);

    assert!(args.get("payload").is_some());
    assert!(args.get("_proxiima_caller_self_perspective").is_none());
    assert!(args.get("caller_self_perspective").is_none());
    assert!(args.get("current_root_perspective_memory_id").is_none());
}

#[test]
fn strip_call_context_args_removes_reserved_model_id() {
    // `model_id` is captured into the caller before stripping, then
    // removed so a dispatcher tool that does not list it as an action field
    // is not tripped by an unexpected-field rejection.
    let args = serde_json::json!({ "action": "set", "model_id": "example-model" });
    let author = author_from_args(&args, None, "unknown", "0").expect("author reads model_id");
    assert_eq!(author.model_id, "example-model");

    let mut args = args;
    strip_call_context_args(&mut args);
    assert!(args.get("model_id").is_none(), "model_id stripped: {args}");
    assert_eq!(args["action"], "set");
}

#[test]
fn a_model_id_that_is_not_a_string_is_invalid_params() {
    for claimed in [
        serde_json::json!(123),
        serde_json::json!({}),
        serde_json::json!(["a"]),
    ] {
        let err = author_from_args(
            &serde_json::json!({ "model_id": claimed }),
            None,
            "unknown",
            "0",
        )
        .expect_err("a label that is not a string is refused, not read as absent");
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(err.message.contains("model_id"), "{}", err.message);
    }
    let author = author_from_args(
        &serde_json::json!({ "model_id": null }),
        None,
        "unknown",
        "0",
    )
    .expect("null is an absent label");
    assert_eq!(author.model_id, "unknown");
}

#[test]
fn a_bound_model_identity_becomes_the_persisted_label() {
    let auth = trusted_auth("runner/pinned");
    let author = author_from_args(&serde_json::json!({}), Some(&auth), "unknown", "0")
        .expect("no caller claim, no conflict");
    assert_eq!(
        author.model_id, "runner/pinned",
        "the persisted label is the bound identity, not `unknown`"
    );
}

#[test]
fn a_matching_model_id_argument_is_accepted() {
    let auth = trusted_auth("runner/pinned");
    let author = author_from_args(
        &serde_json::json!({ "model_id": "  runner/pinned  " }),
        Some(&auth),
        "unknown",
        "0",
    )
    .expect("an agreeing claim is not a conflict");

    assert_eq!(author.model_id, "runner/pinned");
}

#[test]
fn a_differing_model_id_argument_is_invalid_params() {
    let auth = trusted_auth("runner/pinned");
    let err = author_from_args(
        &serde_json::json!({ "model_id": "claimed/model" }),
        Some(&auth),
        "unknown",
        "0",
    )
    .expect_err("a caller may not relabel an authenticated runner");

    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(err.message.contains("model_id"), "{}", err.message);
    assert!(
        err.message.contains("authenticated token already binds"),
        "{}",
        err.message
    );
}

/// Without a bound identity nothing changes: the caller's label stands,
/// and an unmapped caller that merely *says* a runner name gains no
/// trusted status from having said it.
#[test]
fn an_unmapped_caller_keeps_its_own_label_and_gains_no_trusted_status() {
    let auth = full_auth(ToolScope::All);
    let author = author_from_args(
        &serde_json::json!({ "model_id": "runner/pinned" }),
        Some(&auth),
        "unknown",
        "0",
    )
    .expect("no bound identity, no conflict");

    assert_eq!(author.model_id, "runner/pinned");

    let unattributed =
        author_from_args(&serde_json::json!({}), Some(&auth), "unknown", "0").expect("caller");
    assert_eq!(unattributed.model_id, "unknown");
}

/// A blank argument is no claim, matching REST — where an empty header
/// is dropped before it can mean anything. Without the shared rule the
/// same request would be an error here and `unknown` there.
#[test]
fn a_blank_model_id_argument_is_absent_on_both_paths() {
    for blank in ["", "   "] {
        let unbound = author_from_args(
            &serde_json::json!({ "model_id": blank }),
            Some(&full_auth(ToolScope::All)),
            "unknown",
            "0",
        )
        .expect("a blank argument is absent, not an empty label");
        assert_eq!(unbound.model_id, "unknown", "blank {blank:?}");

        let bound = author_from_args(
            &serde_json::json!({ "model_id": blank }),
            Some(&trusted_auth("runner/pinned")),
            "unknown",
            "0",
        )
        .expect("a blank argument cannot conflict");
        assert_eq!(bound.model_id, "runner/pinned", "blank {blank:?}");
    }
}

/// `clientInfo` is peer-declared and unauthenticated; it names the
/// client, never the trusted model.
#[test]
fn client_info_never_becomes_the_trusted_model_id() {
    let auth = full_auth(ToolScope::All);
    let author = author_from_args(
        &serde_json::json!({}),
        Some(&auth),
        "runner/pinned",
        "1.0.0",
    )
    .expect("caller");

    assert_eq!(author.client_name, "runner/pinned");
    assert_eq!(author.model_id, "unknown");
}

/// Resource reads take the same precedence, with no argument object to
/// claim from.
#[test]
fn resource_author_context_carries_the_bound_identity() {
    let bound = author_from_ctx(Some(&trusted_auth("runner/pinned")), "unknown", "0");
    assert_eq!(bound.model_id, "runner/pinned");

    let unbound = author_from_ctx(Some(&full_auth(ToolScope::All)), "unknown", "0");
    assert_eq!(unbound.model_id, "unknown");
}

/// A bound identity is captured into the caller and the argument
/// is still stripped, so a dispatcher tool never sees it as an
/// unexpected field.
#[test]
fn strip_call_context_args_still_removes_an_agreeing_model_id() {
    let auth = trusted_auth("runner/pinned");
    let mut args = serde_json::json!({ "action": "set", "model_id": "runner/pinned" });
    let author =
        author_from_args(&args, Some(&auth), "unknown", "0").expect("agreeing claim accepted");
    assert_eq!(author.model_id, "runner/pinned");

    strip_call_context_args(&mut args);
    assert!(args.get("model_id").is_none(), "model_id stripped: {args}");
    assert_eq!(args["action"], "set");
}

#[test]
fn author_from_args_carries_peer_implementation() {
    let args = serde_json::json!({ "model_id": "m" });
    let author =
        author_from_args(&args, None, "example-client", "1.2.3").expect("author with peer info");
    assert_eq!(author.client_name, "example-client");
    assert_eq!(author.client_version, "1.2.3");
}

#[test]
fn palette_scope_narrows_advertised_dispatcher_actions() {
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let goal = registry
        .list_mcp_tools()
        .iter()
        .find(|descriptor| descriptor.name == protocol_tool::CORE_GOAL)
        .expect("core_goal descriptor")
        .clone();

    // A palette that permits only the `set` leaf of core_goal.
    let scope = palette(&[protocol_action::CORE_GOAL_SET]);
    let auth = full_auth(scope);
    let projected = project_dispatcher_actions(&goal, Some(auth.authz.capabilities()));

    let enum_values = projected
        .pointer("/properties/action/enum")
        .and_then(serde_json::Value::as_array)
        .expect("action enum");
    assert_eq!(enum_values, &vec![serde_json::json!("set")]);

    // Only `set`'s fields and prose: no field another action alone
    // takes, no guide line for an action the palette withholds.
    let advertised: std::collections::BTreeSet<&str> = projected["properties"]
        .as_object()
        .expect("properties")
        .keys()
        .map(String::as_str)
        .collect();
    let mut expected: std::collections::BTreeSet<&str> = goal
        .action_arg_specs
        .iter()
        .find(|spec| spec.action == "set")
        .expect("set spec")
        .allowed_fields
        .iter()
        .copied()
        .collect();
    expected.insert("action");
    assert_eq!(advertised, expected);
    let guide = projected["properties"]["action"]["description"]
        .as_str()
        .expect("action guide");
    assert!(guide.contains("\n- set: "), "{guide}");
    assert!(!guide.contains("\n- transition:"), "{guide}");
    assert!(
        !projected["properties"]["evidence"]["description"]
            .as_str()
            .expect("evidence prose")
            .contains("mark_achieved"),
        "{projected:#}"
    );
}

#[test]
fn all_scope_leaves_dispatcher_actions_unchanged() {
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let goal = registry
        .list_mcp_tools()
        .iter()
        .find(|descriptor| descriptor.name == protocol_tool::CORE_GOAL)
        .expect("core_goal descriptor")
        .clone();
    let projected = project_dispatcher_actions(&goal, Some(&CapabilitySet::all()));
    assert_eq!(projected, goal.args_schema);
}

#[test]
fn not_authorized_message_lists_allowed_actions() {
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let scope = palette(&[protocol_action::CORE_GOAL_SET]);
    let auth = full_auth(scope);
    let err = tool_invocation_error_to_error_data(
        &registry,
        ToolError::NotAuthorized(protocol_action::CORE_GOAL_TRANSITION.into()).into(),
        Some(&auth),
    );
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_REQUEST);
    assert!(
        err.message.contains(&format!(
            "allowed {} actions: set",
            protocol_tool::CORE_GOAL
        )),
        "message: {}",
        err.message
    );
}

/// The retry hint reaches a SPIICE dispatcher too. Built from
/// `all_core_actions()` — a table over substrate names — this message
/// listed nothing for a spiice tool, so the caller holding a partial
/// palette on a tool the substrate never heard of got no hint at all.
#[test]
fn not_authorized_lists_a_spiice_dispatchers_allowed_actions() {
    use futures_util::future::BoxFuture;
    use proxiima_core::mcp::McpActionArgSpec;
    use proxiima_core::{Tool, ToolCtx, ToolError};

    #[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
    #[serde(tag = "action", rename_all = "snake_case")]
    #[expect(dead_code, reason = "the derived schema is the subject")]
    enum StubArgs {
        Look {
            #[schemars(description = "Which thing to look at.")]
            id: String,
        },
        Touch {
            #[schemars(description = "Which thing to touch.")]
            id: String,
        },
    }

    #[derive(Debug)]
    struct StubDispatchTool;

    #[derive(Debug, serde::Serialize, schemars::JsonSchema)]
    struct StubOutput {}

    impl Tool for StubDispatchTool {
        const TITLE: &'static str = "Test tool";
        const NAME: &'static str = "proxiima-stub_dispatch";
        const DESCRIPTION: &'static str = "A spiice dispatcher.";
        const ACTION_ARG_SPECS: &'static [McpActionArgSpec] = &[
            McpActionArgSpec {
                action: "look",
                allowed_fields: &["id"],
                required_fields: &["id"],
                effect: ToolEffect::Additive(Replay::NonIdempotent),
                audience: proxiima_core::mcp::McpToolAudience::Shared,
                need: proxiima_core::mcp::OwnerNeed::Memory,
                title: "Test action",
            },
            McpActionArgSpec {
                action: "touch",
                allowed_fields: &["id"],
                required_fields: &["id"],
                effect: ToolEffect::Additive(Replay::NonIdempotent),
                audience: proxiima_core::mcp::McpToolAudience::Shared,
                need: proxiima_core::mcp::OwnerNeed::Memory,
                title: "Test action",
            },
        ];
        type Args = StubArgs;
        type Output = StubOutput;
        fn call(_: ToolCtx, _: Self::Args) -> BoxFuture<'static, Result<Self::Output, ToolError>> {
            Box::pin(async { Ok(StubOutput {}) })
        }
    }

    let mut registry = proxiima_core::SpiiceRegistry::new();
    registry.add_tool_or_panic_for_tests::<StubDispatchTool>("proxiima-stub");
    let registry = registry.freeze_or_panic_for_tests();

    let scope = palette(&["proxiima-stub_dispatch:look"]);
    let auth = full_auth(scope);
    let err = tool_invocation_error_to_error_data(
        &registry,
        ToolError::NotAuthorized("proxiima-stub_dispatch:touch".into()).into(),
        Some(&auth),
    );
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_REQUEST);
    assert!(
        err.message
            .contains("allowed proxiima-stub_dispatch actions: look"),
        "message: {}",
        err.message
    );
}

#[test]
fn unavailable_error_reaches_caller_as_invalid_request() {
    let err = mcp_tool_error_to_error_data(&ToolError::Unavailable(
        "semantic search unavailable: no embedding client is configured for this host".into(),
    ));
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_REQUEST);
    assert_eq!(
        err.message,
        "semantic search unavailable: no embedding client is configured for this host"
    );
}

/// #480: an other store that did not answer is a retryable -32000 with
/// `data.code` `store_unavailable` naming the store, never -32602 (which
/// a client reads as "no such memory") or -32600.
#[test]
fn an_unanswered_store_is_a_server_error_naming_the_store() {
    let err = mcp_tool_error_to_error_data(&ToolError::StoreUnavailable { store: 1 });
    assert_eq!(err.code, SERVER_ERROR);
    assert_ne!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert_ne!(err.code, rmcp::model::ErrorCode::INVALID_REQUEST);
    assert_eq!(err.message, "store 1 unavailable");
    assert_eq!(
        err.data,
        Some(serde_json::json!({ "code": "store_unavailable", "store": 1 }))
    );
}

#[test]
fn caller_self_perspective_metadata_errors_are_invalid_params() {
    let args = serde_json::json!({
        "caller_self_perspective": 42,
    });
    let err = author_from_args(&args, None, "unknown", "0").expect_err("non-string metadata fails");
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(
        err.message.contains("caller_self_perspective"),
        "message: {}",
        err.message
    );

    let args = serde_json::json!({
        "current_root_perspective_memory_id": "not-a-uuid",
    });
    let err =
        author_from_args(&args, None, "unknown", "0").expect_err("invalid uuid metadata fails");
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(
        err.message.contains("current_root_perspective_memory_id"),
        "message: {}",
        err.message
    );
}

#[test]
fn tool_output_serialization_errors_are_internal_and_redacted() {
    let err = mcp_tool_error_to_error_data(&ToolError::Other(
        "serialize tool output: fixture secret from output serializer".into(),
    ));
    assert_eq!(err.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    assert_eq!(err.message, "internal server error");
    assert!(!err.message.contains("fixture secret"));
}

/// A missing entity is a `resource_not_found` on the resource path but
/// an argument fault on the tool path — the same `ToolError` maps
/// to different JSON-RPC codes by surface.
#[test]
fn not_found_maps_by_surface() {
    let not_found = || ToolError::NotFound("memory F:018f not found".into());

    let resource = resource_invocation_error_to_error_data(
        crate::server::ToolInvocationError::Tool(not_found()),
    );
    assert_eq!(resource.code, rmcp::model::ErrorCode::RESOURCE_NOT_FOUND);
    assert_eq!(resource.message, "memory F:018f not found");

    let tool = mcp_tool_error_to_error_data(&not_found());
    assert_eq!(tool.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert_eq!(tool.message, "memory F:018f not found");
}

#[test]
fn tool_scope_denials_remain_invalid_request() {
    // No auth threaded (e.g. direct-handler path) → bare message, no
    // allowed-action enrichment.
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let err = tool_invocation_error_to_error_data(
        &registry,
        ToolError::NotAuthorized(protocol_action::CORE_GOAL_SET.into()).into(),
        None,
    );

    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_REQUEST);
    assert_eq!(
        err.message,
        format!(
            "tool {} not authorized for this MCP token",
            protocol_action::CORE_GOAL_SET
        )
    );
}

/// A viewer sees a spiice's read tool in `tools/list`.
/// Read-vs-write comes from the descriptor's own effect, never from its
/// name.
#[test]
fn a_viewer_sees_a_spiice_read_tool_and_not_its_write_tool() {
    use proxiima_core::{AuthPath, AuthzContext, GroupId, OwnerId, UserId, access::Role};

    let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let viewer = McpAuthContext {
        owner,
        authz: AuthzContext::for_subject_with_role(
            UserId::new(uuid::Uuid::now_v7()),
            [(owner, Role::viewer())],
            AuthPath::HostBearer,
        ),
    };

    let read = spiice_descriptor("proxiima-stub_search", Some(ToolEffect::ReadOnly));
    let write = spiice_descriptor(
        "proxiima-stub_write",
        Some(ToolEffect::Additive(Replay::NonIdempotent)),
    );
    // No name-keyed fallback: a substrate descriptor that declares
    // nothing is a write, whatever its name.
    let silent_core = McpToolDescriptor {
        origin: proxiima_core::mcp::McpToolOrigin::Substrate,
        ..spiice_descriptor(protocol_tool::CORE_SEARCH_MEMORIES, None)
    };
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let core_read = registry
        .mcp_tool(protocol_tool::CORE_SEARCH_MEMORIES)
        .expect("core search is registered");

    assert!(
        tool_allowed_for_auth(Caller::new(Some(&viewer)), &read),
        "a spiice tool that declares read_only must be visible to a viewer"
    );
    assert!(
        !tool_allowed_for_auth(Caller::new(Some(&viewer)), &write),
        "a spiice write tool must stay hidden from a viewer"
    );
    assert!(
        !tool_allowed_for_auth(Caller::new(Some(&viewer)), &silent_core),
        "a name is not a declaration"
    );
    assert!(
        tool_allowed_for_auth(Caller::new(Some(&viewer)), core_read),
        "core search declares its own read effect"
    );
}

#[test]
fn a_space_tool_is_visible_when_another_effective_owner_allows_it() {
    use proxiima_core::{AuthPath, AuthzContext, GroupId, OwnerId, Role, UserId};

    let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let subject = UserId::new(uuid::Uuid::now_v7());
    let auth = McpAuthContext {
        owner,
        authz: AuthzContext::for_subject_with_role(
            subject,
            [(owner, Role::viewer())],
            AuthPath::HostBearer,
        ),
    };
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let goal = registry.mcp_tool(protocol_tool::CORE_GOAL).expect("goals");
    assert!(action_allowed_for_auth(
        Caller::new(Some(&auth)),
        goal,
        "mark_achieved"
    ));

    let default_only = McpAuthContext {
        owner,
        authz: auth.authz.narrowed_to_owner(owner).expect("viewer default"),
    };
    assert!(!action_allowed_for_auth(
        Caller::new(Some(&default_only)),
        goal,
        "mark_achieved"
    ));
}

#[test]
fn a_bogus_action_scope_never_advertises_a_flat_tool() {
    use proxiima_core::{AuthPath, AuthzContext, OwnerId, UserId};

    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let descriptor = spiice_descriptor("proxiima-stub_search", Some(ToolEffect::ReadOnly));
    let auth = McpAuthContext {
        owner,
        authz: AuthzContext::for_subject(UserId::new(uuid::Uuid::now_v7()), AuthPath::HostBearer)
            .with_tool_scope(palette(&["proxiima-stub_search:bogus"])),
    };

    assert!(descriptor.action_arg_specs.is_empty());
    assert!(!tool_allowed_for_auth(
        Caller::new(Some(&auth)),
        &descriptor
    ));
}

#[test]
fn a_viewer_sees_only_the_read_leaf_of_a_mixed_dispatcher() {
    use proxiima_core::{AuthPath, AuthzContext, GroupId, OwnerId, UserId, access::Role};

    let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let viewer = McpAuthContext {
        owner,
        authz: AuthzContext::for_subject_with_role(
            UserId::new(uuid::Uuid::now_v7()),
            [(owner, Role::viewer())],
            AuthPath::HostBearer,
        ),
    };
    let no_fields = serde_json::json!({
        "type": "object", "properties": {}, "additionalProperties": false
    });
    let dispatcher = proxiima_core::mcp::McpDispatcherSchema {
        discriminator: "action".to_owned(),
        description: None,
        actions: ["look", "touch"]
            .map(|action| proxiima_core::mcp::McpActionSchema {
                action: action.to_owned(),
                description: None,
                argument_schema: no_fields.clone(),
            })
            .to_vec(),
    };
    let mixed = McpToolDescriptor {
        args_schema: dispatcher.render(|_| true),
        dispatcher_schema: Some(dispatcher),
        action_arg_specs: MIXED_ACTIONS,
        // Parent says read; the write action must not inherit it.
        ..spiice_descriptor("proxiima-stub_dispatch", Some(ToolEffect::ReadOnly))
    };
    assert!(
        tool_allowed_for_auth(Caller::new(Some(&viewer)), &mixed),
        "the read action keeps a mixed dispatcher visible"
    );
    let viewer_schema = project_dispatcher_actions_for_auth(Caller::new(Some(&viewer)), &mixed);
    assert_eq!(
        viewer_schema["properties"]["action"]["enum"],
        serde_json::json!(["look"]),
    );
    assert_eq!(
        annotations_for_auth(Caller::new(Some(&viewer)), &mixed).and_then(|value| value.read_only),
        Some(true),
    );

    let writer = McpAuthContext {
        owner,
        authz: AuthzContext::for_subject_with_role(
            UserId::new(uuid::Uuid::now_v7()),
            [(owner, Role::editor())],
            AuthPath::HostBearer,
        )
        .with_tool_scope(ToolScope::All),
    };
    assert_eq!(
        project_dispatcher_actions_for_auth(Caller::new(Some(&writer)), &mixed)["properties"]["action"]
            ["enum"],
        serde_json::json!(["look", "touch"]),
    );
    assert_eq!(
        annotations_for_auth(Caller::new(Some(&writer)), &mixed).and_then(|value| value.read_only),
        Some(false),
        "a mixed dispatcher is conservatively a write when both actions are visible",
    );
}

#[test]
fn scoped_dispatcher_projection_preserves_allowed_argument_schema_metadata() {
    use proxiima_core::access::Role;
    use proxiima_core::{AuthPath, AuthzContext, GroupId, OwnerId, SpiiceRegistry, UserId};

    let registry = SpiiceRegistry::default().freeze_or_panic_for_tests();
    let descriptor = registry
        .list_mcp_tools()
        .iter()
        .find(|tool| tool.name == protocol_tool::CORE_MEMBERSHIP)
        .expect("core membership dispatcher");
    let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let viewer = McpAuthContext {
        owner,
        authz: AuthzContext::for_subject_with_role(
            UserId::new(uuid::Uuid::now_v7()),
            [(owner, Role::viewer())],
            AuthPath::HostBearer,
        ),
    };
    let projected = project_dispatcher_actions_for_auth(Caller::new(Some(&viewer)), descriptor);
    assert_eq!(
        projected["properties"]["action"]["enum"],
        serde_json::json!(["list_members"])
    );
    // The viewer's schema is `list_members`' own: its field, as declared,
    // and nothing `add_member` / `remove_member` alone take.
    let list_members = descriptor
        .action_argument_schema("list_members")
        .expect("list_members argument schema");
    let fields: Vec<&String> = projected["properties"]
        .as_object()
        .expect("properties")
        .keys()
        .filter(|field| *field != "action")
        .collect();
    assert_eq!(
        fields,
        list_members["properties"]
            .as_object()
            .expect("list_members properties")
            .keys()
            .collect::<Vec<_>>()
    );
    assert_eq!(
        projected["properties"]["group"],
        list_members["properties"]["group"]
    );
}

/// The argv vocabulary classifies per command too, and an unannotated
/// command still falls back to the tool.
///
/// The motivating shape is a dispatcher whose argv-keyed commands are
/// mostly reads under a tool that must call itself writable because
/// some of them write. Classifying the whole tool cost a
/// read-capable-only owner every one of those reads.
#[test]
fn a_viewer_keeps_the_annotated_read_command_of_an_argv_dispatcher() {
    use proxiima_core::{
        AuthPath, AuthzContext, GroupId, OwnerId, ToolScope, UserId, access::Role,
    };

    let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let viewer = McpAuthContext {
        owner,
        authz: AuthzContext::for_subject_with_role(
            UserId::new(uuid::Uuid::now_v7()),
            [(owner, Role::viewer())],
            AuthPath::HostBearer,
        )
        .with_tool_scope(ToolScope::All),
    };
    let cli = McpToolDescriptor {
        argv_action_specs: MIXED_ARGV_ACTIONS,
        // The tool writes; only the annotated command opts out.
        ..spiice_descriptor(
            "proxiima-stub_cli",
            Some(ToolEffect::Additive(Replay::NonIdempotent)),
        )
    };

    assert!(
        action_allowed_for_auth(Caller::new(Some(&viewer)), &cli, "approval"),
        "an argv command that declares read_only must stay callable by a viewer"
    );
    assert!(
        !action_allowed_for_auth(Caller::new(Some(&viewer)), &cli, "approval-decide"),
        "an argv command that declares nothing classifies from the tool, which writes"
    );
    assert!(
        tool_allowed_for_auth(Caller::new(Some(&viewer)), &cli),
        "the read command keeps the dispatcher visible"
    );
    assert_eq!(
        annotations_for_auth(Caller::new(Some(&viewer)), &cli).and_then(|value| value.read_only),
        Some(true),
        "only the read command is visible to a viewer",
    );

    let writer = McpAuthContext {
        owner,
        authz: AuthzContext::for_subject_with_role(
            UserId::new(uuid::Uuid::now_v7()),
            [(owner, Role::editor())],
            AuthPath::HostBearer,
        )
        .with_tool_scope(ToolScope::All),
    };
    assert_eq!(
        annotations_for_auth(Caller::new(Some(&writer)), &cli).and_then(|value| value.read_only),
        Some(false),
        "a mixed argv dispatcher is conservatively a write when both commands are visible",
    );
}

/// An argv command classifies from its own effect, never the tool's:
/// even a hand-built descriptor carrying a read-only tool-level effect
/// (which `try_freeze` refuses on a dispatcher) lends it to no command.
#[test]
fn an_argv_command_classifies_from_its_own_effect_never_the_tools() {
    const WRITE: &[proxiima_core::mcp::McpArgvActionSpec] =
        &[proxiima_core::mcp::McpArgvActionSpec {
            action: "approval",
            argv_prefix: &["approval"],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: proxiima_core::mcp::McpToolAudience::Shared,
            need: proxiima_core::mcp::OwnerNeed::Memory,
            title: "Test action",
        }];

    let tool = McpToolDescriptor {
        argv_action_specs: WRITE,
        ..spiice_descriptor("proxiima-stub_cli", Some(ToolEffect::ReadOnly))
    };
    assert!(!tool.action_is_read_only("approval"));
    assert!(
        !tool.action_is_read_only("not-a-command"),
        "a key the vocabulary does not emit is a write"
    );
    assert_eq!(
        tool.effect(),
        Some(ToolEffect::Additive(Replay::NonIdempotent)),
        "the tool-level effect is its commands' join"
    );
}

#[test]
fn denial_hints_exclude_actions_forbidden_by_the_owner_role() {
    use proxiima_core::{AuthPath, AuthzContext, GroupId, OwnerId, UserId, access::Role};

    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    let viewer = McpAuthContext {
        owner,
        authz: AuthzContext::for_subject_with_role(
            UserId::new(uuid::Uuid::now_v7()),
            [(owner, Role::viewer())],
            AuthPath::HostBearer,
        )
        .with_tool_scope(ToolScope::All),
    };
    let err = tool_invocation_error_to_error_data(
        &registry,
        ToolError::NotAuthorized(protocol_action::CORE_MEMBERSHIP_ADD_MEMBER.into()).into(),
        Some(&viewer),
    );

    assert_eq!(
        err.message,
        "tool core_membership:add_member not authorized for this MCP token; allowed \
             core_membership actions: list_members",
    );
}

// Completeness gate: every substrate tool resolves an effect from its
// descriptor — a flat tool's `EFFECT`, a dispatcher's join over its
// action specs.
#[test]
fn every_core_tool_is_annotated() {
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    for descriptor in registry.list_mcp_tools() {
        if descriptor.name.starts_with("core_") {
            assert!(
                descriptor.annotations().is_some(),
                "core tool {} has no resolvable MCP annotations",
                descriptor.name
            );
        }
    }
}

#[test]
fn core_tool_effects_encode_expected_semantics() {
    let registry = proxiima_core::SpiiceRegistry::new().freeze_or_panic_for_tests();
    let hints = |name: &str| {
        registry
            .mcp_tool(name)
            .and_then(McpToolDescriptor::annotations)
            .unwrap_or_else(|| panic!("{name} resolves hints"))
    };

    // Closed substrate: open_world is always false.
    let read = hints(protocol_tool::CORE_SEARCH_MEMORIES);
    assert_eq!(read.read_only, Some(true));
    assert_eq!(read.open_world, Some(false));

    // Convergent additive write (required idempotency key).
    let derive = hints(protocol_tool::CORE_DERIVE);
    assert_eq!(derive.read_only, Some(false));
    assert_eq!(derive.destructive, Some(false));
    assert_eq!(derive.idempotent, Some(true));

    // Additive write with an OPTIONAL idempotency key: identical args
    // without a key create a new Fact, so it is not replay-safe.
    let remember = hints(protocol_tool::CORE_REMEMBER);
    assert_eq!(remember.read_only, Some(false));
    assert_eq!(remember.destructive, Some(false));
    assert_eq!(remember.idempotent, Some(false));

    // Grouped fact dispatcher contains only citation reads; the answer
    // is the join of its action specs, not duplicated here.
    assert_eq!(hints(protocol_tool::CORE_FACT).read_only, Some(true));

    // Idempotent by content: the interpretation's memory id folds the
    // claim, its confidence and its subjects, so re-asserting the same
    // judgment lands on one memory rather than a pile of duplicates.
    let interpret = hints(protocol_tool::CORE_INTERPRET);
    assert_eq!(interpret.read_only, Some(false));
    assert_eq!(interpret.destructive, Some(false));
    assert_eq!(interpret.idempotent, Some(true));

    // Grouped goal dispatcher joins write actions with mixed
    // idempotence: one non-idempotent action makes the tool one.
    let goal = hints(protocol_tool::CORE_GOAL);
    assert_eq!(goal.read_only, Some(false));
    assert_eq!(goal.destructive, Some(true));
    assert_eq!(goal.idempotent, Some(false));

    // One destructive action (`remove_member`) makes the dispatcher
    // destructive, so a client asks before auto-approving any of it.
    let membership = hints(protocol_tool::CORE_MEMBERSHIP);
    assert_eq!(membership.read_only, Some(false));
    assert_eq!(membership.destructive, Some(true));

    // Spiice-shipped / unknown tools are not in the substrate registry.
    assert!(registry.mcp_tool("company/upsert").is_none());
}

/// A NUL is well-formed JSON and fatal to Postgres, so it has to be
/// caught here rather than surfacing as a server fault.
#[test]
fn nul_in_a_string_argument_is_invalid_params() {
    let args = serde_json::json!({ "query": "chunk\u{0}er" });
    let err = reject_nul_in_args(&args).expect_err("NUL must be rejected");
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(err.message.contains("NUL"), "{}", err.message);
}

#[test]
fn nul_nested_in_an_array_or_object_is_found() {
    for args in [
        serde_json::json!({ "tags": ["fine", "b\u{0}ad"] }),
        serde_json::json!({ "outer": { "inner": { "deep": "b\u{0}ad" } } }),
        serde_json::json!({ "list": [{ "k": ["x", { "y": "b\u{0}ad" }] }] }),
    ] {
        assert!(
            reject_nul_in_args(&args).is_err(),
            "NUL must be found anywhere in the tree: {args}"
        );
    }
}

#[test]
fn nul_in_an_argument_name_is_rejected() {
    let mut map = serde_json::Map::new();
    map.insert("na\u{0}me".to_string(), serde_json::json!("fine"));
    let err = reject_nul_in_args(&serde_json::Value::Object(map))
        .expect_err("NUL in a key must be rejected");
    assert!(err.message.contains("argument names"), "{}", err.message);
}

/// The check must not reject ordinary arguments, including other
/// control characters and non-ASCII text, which Postgres stores fine.
#[test]
fn ordinary_arguments_pass() {
    let args = serde_json::json!({
        "query": "how does the chunker decide\tsize?\nline two",
        "limit": 12,
        "include_calls": true,
        "repo_handle": serde_json::Value::Null,
        "tags": ["münchen", "\u{1F525} emoji", ""],
    });
    assert!(reject_nul_in_args(&args).is_ok());
}

/// Deepest argument tree a request can carry: `serde_json` rejects
/// nesting at depth 128, so 127 is the maximum.
#[test]
fn the_deepest_reachable_argument_tree_is_walked() {
    let deep = format!("{}{}{}", "[".repeat(127), "\"leaf\"", "]".repeat(127));
    let value: serde_json::Value =
        serde_json::from_str(&deep).expect("127 levels is under the parser limit");
    assert!(reject_nul_in_args(&value).is_ok());

    let too_deep = format!("{}{}{}", "[".repeat(128), "\"leaf\"", "]".repeat(128));
    assert!(
        serde_json::from_str::<serde_json::Value>(&too_deep).is_err(),
        "the parser, not this function, is what bounds depth"
    );
}
