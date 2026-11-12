use std::sync::Arc;

use super::*;
use crate::auth::McpAuthContext;
use proxiima_core::catalog::core_tools::get_memory::VersionSelector;
use proxiima_core::catalog::core_tools::walk_memory_lineage::WalkMemoryLineageDirectionArg;
use proxiima_core::catalog::{ActionArgSpec, HostToolCall, OwnerNeed, ToolAudience, ToolContent};
use proxiima_core::{
    AuthzContext, OwnerId, ScopeKey, ServedSurface, SpiiceRegistry, ToolScope, UserId,
};
use proxiima_core::{Tool, ToolCtx};

fn fake_owner() -> OwnerId {
    OwnerId::from(UserId::new(uuid::Uuid::now_v7()))
}

#[derive(Debug)]
struct RecordingBehavior(Arc<std::sync::Mutex<Vec<String>>>);

#[async_trait::async_trait]
impl proxiima_core::RequestBehavior for RecordingBehavior {
    async fn handle(&self, call: ToolCall, next: Next<'_>) -> Result<ToolReply, ToolError> {
        self.0.lock().expect("lock").push(call.name.clone());
        next.run(call).await
    }
}

#[derive(Debug)]
struct EchoHostTools;

#[derive(Debug)]
struct OutputHostTools(serde_json::Value);

#[async_trait::async_trait]
impl McpHostTools for OutputHostTools {
    async fn list(&self, _: &McpAuthContext) -> Result<Vec<McpHostTool>, ToolError> {
        Ok(vec![host_tool("host_output", READ)])
    }

    async fn call(&self, _: ToolCall) -> Result<ToolReply, ToolError> {
        Ok(ToolReply::Structured(self.0.clone()))
    }
}

#[tokio::test]
async fn direct_host_tool_calls_require_object_outputs() {
    let owner = fake_owner();
    let auth = McpAuthContext {
        owner,
        authz: AuthzContext::single_owner(&owner, AuthPath::HostBearer),
    };
    for output in [
        serde_json::json!(null),
        serde_json::json!("private-output"),
        serde_json::json!(42),
        serde_json::json!(true),
        serde_json::json!(["private-output"]),
        serde_json::json!({"answer": [42], "nested": {"value": null}}),
    ] {
        let server = make_server().with_host_tools(Arc::new(OutputHostTools(output.clone())));
        let result = server
            .call_tool(
                "host_output",
                serde_json::json!({}),
                author(),
                Some(auth.clone()),
            )
            .await;
        if output.is_object() {
            assert_eq!(result.unwrap(), output);
        } else {
            let Err(ToolInvocationError::Tool(error)) = result else {
                panic!("non-object output must return a tool error: {result:?}");
            };
            assert!(matches!(&error, ToolError::Other(_)));
            assert_eq!(error.kind(), ToolErrorKind::Internal);
            assert_eq!(error.client_message(), "internal server error");
            assert!(!error.to_string().contains("private-output"));
        }
    }
}

const READ: proxiima_core::ToolEffect = proxiima_core::ToolEffect::ReadOnly;
const WRITE: proxiima_core::ToolEffect =
    proxiima_core::ToolEffect::Additive(proxiima_core::Replay::NonIdempotent);

fn host_tool(name: &str, effect: proxiima_core::ToolEffect) -> McpHostTool {
    McpHostTool::new(
        name,
        format!("{name} fixture"),
        serde_json::json!({"type": "object"}),
        effect,
    )
    .with_output_schema(serde_json::json!({"type": "object"}))
}

#[async_trait::async_trait]
impl McpHostTools for EchoHostTools {
    async fn list(&self, _auth: &McpAuthContext) -> Result<Vec<McpHostTool>, ToolError> {
        Ok(vec![
            host_tool("host_echo", READ),
            host_tool("host_write", WRITE),
            // Shadowed by the registry's own tool of this name.
            host_tool("core_memory_spaces", READ),
            // A registry tool's name in another spelling, a scope-key
            // shape, an action-leaf shape, a repeat, and an overlong name.
            host_tool("core:memory_spaces", READ),
            host_tool("resource:graph", WRITE),
            host_tool("core_goal:set", WRITE),
            host_tool("host_echo", WRITE),
            host_tool(&"h".repeat(MAX_HOST_TOOL_NAME_CHARS + 1), READ),
        ])
    }

    async fn call(&self, call: ToolCall) -> Result<ToolReply, ToolError> {
        let marker = call
            .ctx
            .service::<HostToolCall>()
            .ok_or_else(|| ToolError::Other("no host-call marker".into()))?;
        Ok(ToolReply::Structured(serde_json::json!({
            "tool": call.name,
            "marker": marker.name(),
            "args": call.args,
        })))
    }
}

fn author() -> ToolCaller {
    ToolCaller::new("test", "test", "0")
}

/// A host tool is flat, so its palette key is `ScopeKey::Tool(name)`. The
/// name grammar is what keeps it so: of every short string over an
/// alphabet that holds the delimiter, a space and a slash, the ones the
/// host accepts all parse to a `Tool` key printing as the name, and every
/// one that would parse to a leaf or a resource key is refused.
#[test]
fn no_served_host_tool_name_can_spell_a_leaf_or_a_resource_key() {
    let registry = SpiiceRegistry::new().freeze_or_panic_for_tests();
    let alphabet = ['a', 'b', ':', '.', '-', ' ', '/'];
    let mut names = vec![String::new()];
    let mut layer = vec![String::new()];
    for _ in 0..4 {
        layer = layer
            .iter()
            .flat_map(|prefix| alphabet.iter().map(move |c| format!("{prefix}{c}")))
            .collect();
        names.extend(layer.iter().cloned());
    }
    names.extend(["resource", "resource:memory", "core_goal:set"].map(String::from));

    let (mut served, mut spelled_otherwise) = (0, 0);
    for name in &names {
        let refusal = host_tool_name_refusal(&registry, name, &[]);
        match ScopeKey::parse(name) {
            Ok(ScopeKey::Tool(tool)) if refusal.is_none() => {
                assert_eq!(tool.to_string(), *name);
                served += 1;
            }
            Ok(ScopeKey::Action { .. } | ScopeKey::Resource(_)) => {
                assert!(refusal.is_some(), "{name:?} spells a leaf or a resource");
                spelled_otherwise += 1;
            }
            Ok(ScopeKey::Tool(_)) | Err(_) => {
                assert!(refusal.is_some(), "{name:?} is no flat tool name");
            }
        }
    }
    assert!(
        served > 100 && spelled_otherwise > 100,
        "{served} {spelled_otherwise}"
    );
    // A flat tool named like the resource prefix is still a flat tool.
    assert_eq!(host_tool_name_refusal(&registry, "resource", &[]), None);
}

/// A host tool runs through the registry's behaviors, scope gate first,
/// and the gate classifies it from the host's declaration.
#[tokio::test]
async fn a_host_tool_dispatches_through_the_request_behaviors() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registry = SpiiceRegistry::new();
    registry.add_request_behavior(RecordingBehavior(Arc::clone(&seen)));
    let server = McpToolHost::from_parts(
        Arc::new(registry.freeze_or_panic_for_tests()),
        SpiiceServices::default(),
    )
    .with_host_tools(Arc::new(EchoHostTools));
    let owner = fake_owner();
    let auth = McpAuthContext {
        owner,
        authz: AuthzContext::single_owner(&owner, AuthPath::HostBearer),
    };
    let listed: Vec<String> = server
        .host_tools_for(&auth)
        .await
        .expect("the source lists")
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(
        listed,
        ["host_echo", "host_write"],
        "only well-formed, unshadowed, first-listed names are served"
    );

    let output = server
        .call_tool(
            "host_echo",
            serde_json::json!({"x": 1}),
            author(),
            Some(auth.clone()),
        )
        .await
        .expect("owner calls a host tool");
    assert_eq!(
        output,
        serde_json::json!({"tool": "host_echo", "marker": "host_echo", "args": {"x": 1}})
    );
    assert_eq!(*seen.lock().expect("lock"), ["host_echo"]);

    // Exact caller keys gate both reads and writes by their own names.
    let narrowed = McpAuthContext {
        owner,
        authz: auth.authz.clone().with_tool_scope(ToolScope::Palette(vec![
            ScopeKey::parse("host_echo").expect("a scope key"),
        ])),
    };
    assert!(matches!(
        server
            .call_tool("host_write", serde_json::json!({}), author(), Some(narrowed))
            .await,
        Err(ToolInvocationError::NotAuthorized(name)) if name == "host_write"
    ));

    // A viewer reads through a read-only host tool and cannot write.
    let group = OwnerId::from(proxiima_core::GroupId::new(uuid::Uuid::now_v7()));
    let viewer = McpAuthContext {
        owner: group,
        authz: AuthzContext::for_subject_with_role(
            UserId::new(uuid::Uuid::now_v7()),
            [(group, proxiima_core::Role::viewer())],
            AuthPath::HostBearer,
        )
        .narrowed_to_owner(group)
        .expect("viewer narrows"),
    };
    server
        .call_tool(
            "host_echo",
            serde_json::json!({}),
            author(),
            Some(viewer.clone()),
        )
        .await
        .expect("a read-only host tool admits a viewer");
    assert!(matches!(
        server
            .call_tool("host_write", serde_json::json!({}), author(), Some(viewer))
            .await,
        Err(ToolInvocationError::NotAuthorized(name)) if name == "host_write"
    ));
    assert!(matches!(
        server
            .call_tool(
                "host_echo",
                serde_json::json!({"a": "\0"}),
                author(),
                Some(auth)
            )
            .await,
        Err(ToolInvocationError::Tool(ToolError::InvalidInput(_)))
    ));
}

fn make_server() -> McpToolHost {
    McpToolHost::from_parts(
        Arc::new(SpiiceRegistry::new().freeze_or_panic_for_tests()),
        SpiiceServices::default(),
    )
}

#[tokio::test]
async fn nul_validation_preserves_host_auth_and_unknown_tool_priority() {
    let server = make_server();
    let owner = fake_owner();
    let auth = McpAuthContext {
        owner,
        authz: AuthzContext::single_owner(&owner, AuthPath::HostBearer),
    };
    let author = ToolCaller::new("test", "test", "0");
    let args = serde_json::json!({"value": "bad\0input"});
    assert!(matches!(
        server
            .call_tool("core_memory_spaces", args.clone(), author.clone(), None)
            .await,
        Err(ToolInvocationError::NotAuthorized(_))
    ));
    assert!(matches!(
        server
            .call_tool(
                "unknown_tool",
                args.clone(),
                author.clone(),
                Some(auth.clone())
            )
            .await,
        Err(ToolInvocationError::ToolNotFound(_))
    ));
    assert!(matches!(
        server.call_tool("core_memory_spaces", args, author, Some(auth)).await,
        Err(ToolInvocationError::Tool(ToolError::InvalidInput(message)))
            if message == "arguments must not contain NUL (U+0000)"
    ));
}

/// The caller is a per-call struct an out-of-tree host builds by hand, and
/// `ToolCaller::model_id` is the label a spiice tool records: under a
/// binding it is the bound identity, and a differing one is refused, also
/// for callers that did not go through a transport edge.
#[test]
fn ctx_for_reconciles_the_model_label_against_the_credential() {
    let server = make_server();
    let owner = fake_owner();
    let auth = McpAuthContext {
        owner,
        authz: AuthzContext::single_owner(&owner, AuthPath::HostBearer)
            .with_trusted_model_id("acme/runner-v3")
            .expect("a well-formed runner id binds"),
    };
    let author = |model_id: &str| ToolCaller::new(model_id, "test", "0");

    let ctx = server
        .ctx_for(author("acme/runner-v3"), &auth)
        .expect("an agreeing author is accepted");
    assert_eq!(ctx.caller().model_id, "acme/runner-v3");

    let err = server
        .ctx_for(author("openai/gpt-9"), &auth)
        .expect_err("a stale label may not reach a spiice as the one to record");
    assert_eq!(err.kind(), ToolErrorKind::InvalidInput);
}

/// Without a binding the caller's own label stands.
#[test]
fn ctx_for_keeps_the_callers_label_without_a_binding() {
    let server = make_server();
    let owner = fake_owner();
    let auth = McpAuthContext {
        owner,
        authz: AuthzContext::single_owner(&owner, AuthPath::HostBearer),
    };

    let ctx = server
        .ctx_for(ToolCaller::new("caller/model", "test", "0"), &auth)
        .expect("no binding, no conflict");

    assert_eq!(ctx.caller().model_id, "caller/model");
}

#[test]
fn parse_resource_uri_projects_known_resources() {
    let memory = parse_resource_uri(
        "proxiima://memory/018f0000-0000-7000-8000-000000000001?expand_neighbors=true",
    )
    .expect("memory resource");
    assert!(matches!(
        memory,
        ParsedResource::Memory(GetMemoryArgs {
            expand_neighbors: true,
            ..
        })
    ));

    let head =
        parse_resource_uri("proxiima://memory/018f0000-0000-7000-8000-000000000001?version=head")
            .expect("head resource");
    assert!(matches!(
        head,
        ParsedResource::Memory(GetMemoryArgs {
            version: VersionSelector::Head,
            ..
        })
    ));
    assert!(
        parse_resource_uri("proxiima://memory/018f0000-0000-7000-8000-000000000001?version=newest")
            .is_err()
    );

    let lineage =
        parse_resource_uri("proxiima://memory/018f0000-0000-7000-8000-000000000001/lineage?direction=descendants&depth=2&limit=7")
            .expect("lineage resource");
    assert!(matches!(
        lineage,
        ParsedResource::MemoryLineage(WalkMemoryLineageArgs {
            direction: WalkMemoryLineageDirectionArg::Descendants,
            depth: 2,
            limit: 7,
            ..
        })
    ));

    assert_eq!(
        parse_resource_uri("proxiima://memory//lineage").unwrap_err(),
        ResourceUriError::UnknownPath
    );
    assert_eq!(
        parse_resource_uri("proxiima://memory/one/two/lineage").unwrap_err(),
        ResourceUriError::UnknownPath
    );
}

/// `proxiima://memories/counts` shares the `memories` path key with the
/// batch read and is the only tail under it; the batch read keeps its
/// exact path, its required `ids` and its own scope key.
#[test]
fn parse_resource_uri_serves_memories_batch_and_counts() {
    let counts = parse_resource_uri("proxiima://memories/counts").expect("counts resource");
    assert!(matches!(counts, ParsedResource::MemoryCounts));
    assert_eq!(counts.scope_key(), protocol_resource::MEMORY_COUNTS);

    // Parameterless like `proxiima://tools`: a query is not read, and a
    // malformed one is still refused by the shared query parser.
    assert!(matches!(
        parse_resource_uri("proxiima://memories/counts?ids=unused"),
        Ok(ParsedResource::MemoryCounts)
    ));
    assert_eq!(
        parse_resource_uri("proxiima://memories/counts?ids").unwrap_err(),
        ResourceUriError::MalformedQueryPair { pair: "ids".into() }
    );

    for other in [
        "proxiima://memories/other",
        "proxiima://memories/",
        "proxiima://memories/counts/",
        "proxiima://memories/counts/more",
        "proxiima://memories/Counts",
    ] {
        assert_eq!(
            parse_resource_uri(other).unwrap_err(),
            ResourceUriError::UnknownPath,
            "{other}"
        );
    }

    let batch = parse_resource_uri("proxiima://memories?ids=00000000-0000-8000-8000-000000000001,00000000-0000-8000-8000-000000000002").expect("batch read");
    assert_eq!(batch.scope_key(), protocol_resource::MEMORIES);
    assert!(matches!(
        batch,
        ParsedResource::Memories(GetMemoriesArgs { memories }) if memories == ["00000000-0000-8000-8000-000000000001", "00000000-0000-8000-8000-000000000002"]
    ));
    assert_eq!(
        parse_resource_uri("proxiima://memories").unwrap_err(),
        ResourceUriError::MissingParam { param: "ids" }
    );
}

#[test]
fn memory_counts_tail_is_the_contract_template() {
    assert_eq!(
        core_resource(protocol_resource::MEMORY_COUNTS).uri_template,
        format!(
            "proxiima://{}/{}",
            resource_path::MEMORIES,
            resource_path::MEMORY_COUNTS_TAIL
        )
    );
}

const MEMORY: &str = "018f0000-0000-7000-8000-000000000001";

/// A resource read with no query is the tool call with no optional
/// arguments: core's defaults apply to both, none is restated here.
#[test]
fn a_memory_resource_without_a_query_takes_the_tools_defaults() {
    let lineage = parse_resource_uri(&format!("proxiima://memory/{MEMORY}/lineage"))
        .expect("lineage resource");
    let ParsedResource::MemoryLineage(args) = lineage else {
        panic!("expected a lineage resource");
    };
    let tool: WalkMemoryLineageArgs =
        serde_json::from_value(serde_json::json!({ "memory": MEMORY })).expect("tool args");
    assert_eq!(format!("{args:?}"), format!("{tool:?}"));

    let memory =
        parse_resource_uri(&format!("proxiima://memory/{MEMORY}")).expect("memory resource");
    let ParsedResource::Memory(args) = memory else {
        panic!("expected a memory resource");
    };
    let tool: GetMemoryArgs =
        serde_json::from_value(serde_json::json!({ "memory": MEMORY })).expect("tool args");
    assert_eq!(format!("{args:?}"), format!("{tool:?}"));
}

#[test]
fn expand_neighbors_is_true_or_false_and_nothing_else() {
    for (value, expected) in [("true", true), ("false", false)] {
        let parsed = parse_resource_uri(&format!(
            "proxiima://memory/{MEMORY}?expand_neighbors={value}"
        ))
        .expect("memory resource");
        assert!(
            matches!(&parsed, ParsedResource::Memory(args) if args.expand_neighbors == expected),
            "{value}: {parsed:?}"
        );
    }
    for value in ["1", "True", "yes", ""] {
        assert!(
            matches!(
                parse_resource_uri(&format!(
                    "proxiima://memory/{MEMORY}?expand_neighbors={value}"
                )),
                Err(ResourceUriError::InvalidParam { param, .. }) if param == "expand_neighbors"
            ),
            "{value:?} must name the parameter"
        );
    }
}

#[test]
fn a_bad_lineage_number_names_its_parameter_and_the_path_id_cannot_be_overridden() {
    assert_eq!(
        parse_resource_uri(&format!("proxiima://memory/{MEMORY}/lineage?depth=deep")).unwrap_err(),
        ResourceUriError::InvalidParam {
            param: "depth".into(),
            value: "deep".into(),
            expected: "a non-negative integer".into(),
        }
    );
    assert!(matches!(
        parse_resource_uri(&format!("proxiima://memory/{MEMORY}?memory=other")),
        Err(ResourceUriError::InvalidQuery(_))
    ));
}

/// Each parse-failure class carries its own wire shape: unknown paths
/// surface as resource-not-found, while bad or missing parameters on a
/// known template name the parameter (backed by `invalid_params` at
/// the rmcp layer) — no failure may collapse into a generic
/// "unknown resource".
#[test]
fn parse_resource_uri_distinguishes_error_classes() {
    assert_eq!(
        parse_resource_uri("proxiima://no-such-resource").unwrap_err(),
        ResourceUriError::UnknownPath
    );
    assert_eq!(
        parse_resource_uri("nothing://schemas").unwrap_err(),
        ResourceUriError::UnknownPath
    );
    // depth=300 does not collapse into "unknown resource": it parses
    // as a wide integer and the tool clamps it to the documented 1..=8.
    let lineage = parse_resource_uri(
        "proxiima://memory/018f0000-0000-7000-8000-000000000001/lineage?depth=300",
    )
    .expect("oversized depth parses; the tool clamps");
    assert!(matches!(
        lineage,
        ParsedResource::MemoryLineage(WalkMemoryLineageArgs { depth: 300, .. })
    ));
    assert_eq!(
        parse_resource_uri(
            "proxiima://memory/018f0000-0000-7000-8000-000000000001/lineage?direction=sideways"
        )
        .unwrap_err(),
        ResourceUriError::InvalidParam {
            param: "direction".into(),
            value: "sideways".into(),
            expected: "one of ancestors, descendants".into(),
        }
    );
    assert_eq!(
        parse_resource_uri("proxiima://goals?limit").unwrap_err(),
        ResourceUriError::MalformedQueryPair {
            pair: "limit".into()
        }
    );

    let unknown = ResourceUriError::UnknownPath.into_invocation_error("proxiima://nope");
    assert!(matches!(unknown, ToolInvocationError::ToolNotFound(uri) if uri == "proxiima://nope"));
}

/// `read_resource` traverses the same `RequestBehavior` onion
/// (`ScopeGateBehavior`) as `call_tool`. An empty deployment palette
/// retains a core resource read for a caller that holds its key.
#[tokio::test]
async fn read_resource_admitted_under_empty_served_palette() {
    let server = make_server();
    let owner = fake_owner();
    let author = ToolCaller::new("test-model", "test-client", "0.1.0");
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer)
        .with_served_surface(ServedSurface::Palette(Vec::new()));
    let auth = McpAuthContext { owner, authz };

    let result = server
        .read_resource("proxiima://schemas", author, Some(auth))
        .await;

    assert!(
        !matches!(result, Err(ToolInvocationError::NotAuthorized(_))),
        "an empty served palette must not deny a resource read: {result:?}"
    );
    result.expect("the owner reads proxiima://schemas under an empty served palette");
}

#[tokio::test]
async fn ctx_for_speaks_bare_uuids() {
    let server = make_server();
    let author = ToolCaller::new("test-model", "test-client", "0.1.0");
    let owner = fake_owner();
    let auth = McpAuthContext {
        owner,
        authz: AuthzContext::single_owner(&owner, AuthPath::HostBearer)
            .narrowed_to_owner(owner)
            .expect("personal owner narrows"),
    };

    let ctx = server
        .ctx_for(author, &auth)
        .expect("no binding, no conflict");
    let id = proxiima_core::MemoryId::new(uuid::Uuid::now_v7());
    let wire = id.into_inner().to_string();
    assert_eq!(wire, format!("{}", id.into_inner()));
    assert_eq!(ctx.resolve_memory(&wire).expect("round trip"), id);
    assert!(
        ctx.resolve_memory(&format!("F:{}", id.into_inner()))
            .is_err(),
        "prefixed ids must be refused on the wire"
    );
}

/// A host source that counts its catalog reads and answers every call
/// with one scripted reply.
#[derive(Debug)]
struct ScriptedHostTools {
    lists: Arc<std::sync::atomic::AtomicUsize>,
    catalog: Result<Vec<McpHostTool>, &'static str>,
    reply: ToolReply,
}

#[async_trait::async_trait]
impl McpHostTools for ScriptedHostTools {
    async fn list(&self, _: &McpAuthContext) -> Result<Vec<McpHostTool>, ToolError> {
        self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.catalog
            .clone()
            .map_err(|message| ToolError::Unavailable(message.into()))
    }

    async fn call(&self, _: ToolCall) -> Result<ToolReply, ToolError> {
        Ok(self.reply.clone())
    }
}

fn scripted(
    catalog: Result<Vec<McpHostTool>, &'static str>,
    reply: ToolReply,
) -> (McpToolHost, Arc<std::sync::atomic::AtomicUsize>) {
    let lists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server = make_server().with_host_tools(Arc::new(ScriptedHostTools {
        lists: Arc::clone(&lists),
        catalog,
        reply,
    }));
    (server, lists)
}

fn owner_auth() -> McpAuthContext {
    let owner = fake_owner();
    McpAuthContext {
        owner,
        authz: AuthzContext::single_owner(&owner, AuthPath::HostBearer),
    }
}

fn lists(counter: &std::sync::atomic::AtomicUsize) -> usize {
    counter.load(std::sync::atomic::Ordering::SeqCst)
}

/// One request, one catalog: a host call reads the source once to find
/// its tool, and a registry tool is found without reading it at all.
#[tokio::test]
async fn a_host_call_lists_the_catalog_once_and_a_registry_call_never() {
    let (server, counter) = scripted(
        Ok(vec![host_tool("host_echo", READ)]),
        ToolReply::Structured(serde_json::json!({"ok": true})),
    );
    let auth = owner_auth();

    server
        .call_tool(
            "host_echo",
            serde_json::json!({}),
            author(),
            Some(auth.clone()),
        )
        .await
        .expect("the host tool answers");
    assert_eq!(lists(&counter), 1, "one catalog read for the call");

    // The registry's own tool is found first; its outcome is not the
    // subject, the source staying unread is.
    let _ = server
        .call_tool(
            "core_memory_spaces",
            serde_json::json!({}),
            author(),
            Some(auth),
        )
        .await;
    assert_eq!(lists(&counter), 1, "a registry call reads no catalog");
}

/// A host source that answers one name: it counts its listings and
/// resolves, and resolves every name to the entry `answer` names.
#[derive(Debug)]
struct ResolvingHostTools {
    lists: std::sync::atomic::AtomicUsize,
    resolves: std::sync::atomic::AtomicUsize,
    answer: &'static str,
}

#[async_trait::async_trait]
impl McpHostTools for ResolvingHostTools {
    async fn list(&self, _: &McpAuthContext) -> Result<Vec<McpHostTool>, ToolError> {
        self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(vec![host_tool(self.answer, READ)])
    }

    async fn resolve(&self, _: &McpAuthContext, _: &str) -> Result<Option<McpHostTool>, ToolError> {
        self.resolves
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Some(host_tool(self.answer, READ)))
    }

    async fn call(&self, _: ToolCall) -> Result<ToolReply, ToolError> {
        Ok(ToolReply::Structured(serde_json::json!({"ok": true})))
    }
}

/// A call costs what it calls: a host that resolves one name serves the
/// call without listing its catalog. An entry of another name is not
/// served, and a name no host tool may carry is never asked for.
#[tokio::test]
async fn a_resolving_host_serves_a_call_without_listing() {
    let source = Arc::new(ResolvingHostTools {
        lists: std::sync::atomic::AtomicUsize::new(0),
        resolves: std::sync::atomic::AtomicUsize::new(0),
        answer: "host_echo",
    });
    let server = make_server().with_host_tools(source.clone());
    let auth = owner_auth();
    let call = |name: &'static str| {
        server.call_tool(name, serde_json::json!({}), author(), Some(auth.clone()))
    };

    call("host_echo").await.expect("the host tool answers");
    assert!(matches!(
        call("host_other").await,
        Err(ToolInvocationError::ToolNotFound(name)) if name == "host_other"
    ));
    assert!(matches!(
        call("host:echo").await,
        Err(ToolInvocationError::ToolNotFound(_))
    ));
    assert_eq!(lists(&source.resolves), 2, "one resolve per valid name");
    assert_eq!(lists(&source.lists), 0, "no call lists the catalog");
}

/// A catalog that cannot be read is an error, never an empty list: the
/// call that needed it fails with the host's error, and so does asking
/// for the catalog.
#[tokio::test]
async fn a_failing_catalog_fails_the_call_that_needed_it() {
    let (server, counter) = scripted(
        Err("catalog down"),
        ToolReply::Structured(serde_json::json!({})),
    );
    let auth = owner_auth();

    for name in ["host_echo", "no_such_tool"] {
        let result = server
            .call_tool(name, serde_json::json!({}), author(), Some(auth.clone()))
            .await;
        assert!(
            matches!(
                &result,
                Err(ToolInvocationError::Tool(ToolError::Unavailable(message)))
                    if message == "catalog down"
            ),
            "{name}: {result:?}"
        );
    }
    assert!(matches!(
        server.host_tools_for(&auth).await,
        Err(ToolError::Unavailable(message)) if message == "catalog down"
    ));
    assert_eq!(lists(&counter), 3);
}

/// What a catalog entry attached when it was listed.
#[derive(Debug)]
struct Revision(u64);

/// A catalog that moves on right after it is listed: every list stamps
/// its entry with the revision it read, then bumps it.
#[derive(Debug)]
struct MovingHostTools(std::sync::atomic::AtomicU64);

#[async_trait::async_trait]
impl McpHostTools for MovingHostTools {
    async fn list(&self, _: &McpAuthContext) -> Result<Vec<McpHostTool>, ToolError> {
        let listed = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(vec![
            host_tool("host_rev", READ).with_services(SpiiceServices::with(Revision(listed))),
        ])
    }

    async fn call(&self, call: ToolCall) -> Result<ToolReply, ToolError> {
        let resolved = call
            .ctx
            .service::<Revision>()
            .ok_or_else(|| ToolError::Other("no revision".into()))?;
        Ok(ToolReply::Structured(serde_json::json!({
            "resolved": resolved.0,
            "catalog": self.0.load(std::sync::atomic::Ordering::SeqCst),
        })))
    }
}

#[derive(Debug)]
struct RevisionBehavior(Arc<std::sync::Mutex<Vec<u64>>>);

#[async_trait::async_trait]
impl proxiima_core::RequestBehavior for RevisionBehavior {
    async fn handle(&self, call: ToolCall, next: Next<'_>) -> Result<ToolReply, ToolError> {
        if let Some(revision) = call.ctx.service::<Revision>() {
            self.0.lock().expect("lock").push(revision.0);
        }
        next.run(call).await
    }
}

/// A call carries the entry it was resolved to, not the catalog as it
/// stands when the behaviors and the terminal run.
#[tokio::test]
async fn a_host_call_carries_the_entry_it_resolved_to_although_the_catalog_moved() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registry = SpiiceRegistry::new();
    registry.add_request_behavior(RevisionBehavior(Arc::clone(&seen)));
    let server = McpToolHost::from_parts(
        Arc::new(registry.freeze_or_panic_for_tests()),
        SpiiceServices::default(),
    )
    .with_host_tools(Arc::new(MovingHostTools(
        std::sync::atomic::AtomicU64::new(1),
    )));

    let output = server
        .call_tool(
            "host_rev",
            serde_json::json!({}),
            author(),
            Some(owner_auth()),
        )
        .await
        .expect("the host tool answers");
    assert_eq!(output, serde_json::json!({"resolved": 1, "catalog": 2}));
    assert_eq!(*seen.lock().expect("lock"), [1]);
}

fn member_of(group: OwnerId, role: Option<proxiima_core::Role>) -> McpAuthContext {
    let user = UserId::new(uuid::Uuid::now_v7());
    let authz = match role {
        Some(role) => {
            AuthzContext::for_subject_with_role(user, [(group, role)], AuthPath::HostBearer)
        }
        None => AuthzContext::single_owner(&OwnerId::from(user), AuthPath::HostBearer),
    };
    McpAuthContext {
        owner: group,
        authz,
    }
}

fn refused(result: Result<serde_json::Value, ToolInvocationError>, tool: &str) -> bool {
    matches!(result, Err(ToolInvocationError::NotAuthorized(name)) if name == tool)
}

/// A host tool declared Goal work runs for every member who works the
/// group's Goals; an undeclared host write still asks for Fact write.
#[tokio::test]
async fn a_declared_goal_work_host_tool_runs_for_every_member_who_works_goals() {
    let (server, _) = scripted(
        Ok(vec![
            host_tool("host_goal", WRITE).with_goal_work(),
            host_tool("host_write", WRITE),
        ]),
        ToolReply::Structured(serde_json::json!({"ran": true})),
    );
    let group = OwnerId::from(proxiima_core::GroupId::new(uuid::Uuid::now_v7()));
    let call = |tool: &'static str, auth: McpAuthContext| {
        server.call_tool(tool, serde_json::json!({}), author(), Some(auth))
    };

    let viewer = member_of(group, Some(proxiima_core::Role::viewer()));
    call("host_goal", viewer.clone())
        .await
        .expect("a viewer member runs the group's declared Goal work");
    assert!(
        refused(call("host_write", viewer).await, "host_write"),
        "a viewer member runs no undeclared host write"
    );

    let admin = member_of(group, Some(proxiima_core::Role::admin()));
    call("host_goal", admin.clone())
        .await
        .expect("an admin runs the declared Goal work");
    call("host_write", admin)
        .await
        .expect("an admin runs a host write");

    let ingest = member_of(group, Some(proxiima_core::Role::ingest()));
    assert!(
        refused(call("host_goal", ingest).await, "host_goal"),
        "a member whose role reads no Goals works none"
    );
    assert!(
        refused(call("host_goal", member_of(group, None)).await, "host_goal"),
        "a non-member works none of the group's Goals"
    );
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
enum BoardArgs {
    /// Move a Goal's card.
    Advance {},
    /// Pin a note.
    Note {},
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct BoardOutput {}

/// A spiice dispatcher with two writes: one declared Goal work, one not.
#[derive(Debug)]
struct Board;

impl Tool for Board {
    const TITLE: &'static str = "Board";
    const NAME: &'static str = "plan_board";
    const DESCRIPTION: &'static str = "Two writes, one of them Goal work.";
    const ACTION_ARG_SPECS: &'static [ActionArgSpec] = &[
        ActionArgSpec {
            action: "advance",
            title: "Advance a card",
            allowed_fields: &[],
            required_fields: &[],
            effect: WRITE,
            audience: ToolAudience::Shared,
            need: OwnerNeed::GoalWork,
        },
        ActionArgSpec {
            action: "note",
            title: "Pin a note",
            allowed_fields: &[],
            required_fields: &[],
            effect: WRITE,
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
        },
    ];
    type Args = BoardArgs;
    type Output = BoardOutput;

    fn call(
        _: ToolCtx,
        _: BoardArgs,
    ) -> futures_util::future::BoxFuture<'static, Result<BoardOutput, ToolError>> {
        Box::pin(async { Ok(BoardOutput {}) })
    }
}

/// A registered action declared Goal work runs for every member who
/// works the group's Goals, and `tools/list` and the catalog show it to
/// exactly them. The tool's other write and core's memory writes keep
/// asking for Fact write.
#[tokio::test]
async fn a_declared_goal_work_action_runs_for_every_member_who_works_goals() {
    use proxiima_core::Role;
    let mut registry = SpiiceRegistry::new();
    registry.add_tool_or_panic_for_tests::<Board>("plan");
    let server = McpToolHost::from_parts(
        Arc::new(registry.freeze_or_panic_for_tests()),
        SpiiceServices::default(),
    );
    let board = server
        .registry()
        .list_mcp_tools()
        .iter()
        .find(|tool| tool.name == Board::NAME)
        .expect("the board is registered");
    let group = OwnerId::from(proxiima_core::GroupId::new(uuid::Uuid::now_v7()));
    let call = |tool: &'static str, args: serde_json::Value, auth: &McpAuthContext| {
        server.call_tool(tool, args, author(), Some(auth.clone()))
    };

    for (who, role, advance, note) in [
        ("a viewer", Some(Role::viewer()), true, false),
        ("an admin", Some(Role::admin()), true, true),
        ("an ingest member", Some(Role::ingest()), false, true),
        ("a non-member", None, false, false),
    ] {
        let auth = member_of(group, role);
        let catalog = server
            .read_resource("proxiima://tools", author(), Some(auth.clone()))
            .await
            .unwrap_or_default();
        let in_catalog: Vec<&str> = catalog["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|tool| tool["tool_id"] == Board::NAME)
            .flat_map(|tool| tool["actions"].as_array().expect("actions"))
            .map(|action| action["action"].as_str().expect("action"))
            .collect();
        for (action, admitted) in [("advance", advance), ("note", note)] {
            let result = call(Board::NAME, serde_json::json!({"action": action}), &auth).await;
            assert_eq!(result.is_ok(), admitted, "{who} runs {action}: {result:?}");
            assert!(admitted || refused(result, Board::NAME), "{who}, {action}");
            assert_eq!(
                crate::handler::action_allowed_for_auth(
                    crate::handler::Caller::new(Some(&auth)),
                    board,
                    action
                ),
                admitted,
                "{who} is listed {action}"
            );
            assert_eq!(
                in_catalog.contains(&action),
                admitted,
                "{who} finds {action} in the catalog"
            );
        }
    }

    let viewer = member_of(group, Some(Role::viewer()));
    for (tool, args) in [
        (
            "core_remember",
            serde_json::json!({"title": "t", "text": "x"}),
        ),
        (
            "core_interpret",
            serde_json::json!({"subjects": [], "text": "x"}),
        ),
    ] {
        assert!(
            refused(call(tool, args, &viewer).await, tool),
            "a viewer member writes no memory through {tool}"
        );
    }
}

/// A listed entry cannot replace the effect declaration the gate reads:
/// the call fails before any behavior or the source's `call` runs.
#[tokio::test]
async fn a_listed_entry_cannot_replace_a_call_service() {
    let forged = host_tool("host_forged", WRITE)
        .with_services(SpiiceServices::with(HostToolCall::new("host_forged", READ)));
    let (server, _) = scripted(
        Ok(vec![forged]),
        ToolReply::Structured(serde_json::json!({"ran": true})),
    );

    let result = server
        .call_tool(
            "host_forged",
            serde_json::json!({}),
            author(),
            Some(owner_auth()),
        )
        .await;
    let Err(ToolInvocationError::Tool(error @ ToolError::Other(_))) = result else {
        panic!("a colliding entry service must be an internal error: {result:?}");
    };
    assert_eq!(error.kind(), ToolErrorKind::Internal);
    assert_eq!(error.client_message(), "internal server error");
}

fn without_output_schema() -> McpHostTool {
    McpHostTool::new(
        "host_out",
        "no output schema",
        serde_json::json!({"type": "object"}),
        READ,
    )
}

/// The final reply is checked once, where it leaves the behaviors.
#[tokio::test]
async fn replies_are_checked_where_they_leave_the_behaviors() {
    let auth = owner_auth();
    let content = ToolReply::Content(vec![ToolContent::text("read me")]);
    let failure = ToolReply::Failure(vec![ToolContent::text("token sk-live-1234 rejected")]);

    // A tool without an output schema may answer content.
    let (server, _) = scripted(Ok(vec![without_output_schema()]), content.clone());
    let reply = server
        .call_tool_reply_in_request(
            "host_out",
            serde_json::json!({}),
            author(),
            Some(auth.clone()),
            SpiiceServices::default(),
        )
        .await
        .expect("content from a tool that declared no schema");
    assert_eq!(reply, content);
    // A JSON-only surface cannot carry it.
    let json = server
        .call_tool(
            "host_out",
            serde_json::json!({}),
            author(),
            Some(auth.clone()),
        )
        .await;
    assert!(
        matches!(&json, Err(ToolInvocationError::Tool(ToolError::Other(_)))),
        "{json:?}"
    );

    // A tool that declared an output schema may not.
    let (server, _) = scripted(Ok(vec![host_tool("host_out", READ)]), content);
    let error = server
        .call_tool_reply_in_request(
            "host_out",
            serde_json::json!({}),
            author(),
            Some(auth.clone()),
            SpiiceServices::default(),
        )
        .await
        .expect_err("content against a declared schema is refused");
    let ToolInvocationError::Tool(error) = error else {
        panic!("expected an internal tool error, got {error:?}");
    };
    assert_eq!(error.kind(), ToolErrorKind::Internal);
    assert_eq!(error.client_message(), "internal server error");

    // A failure is the host's own content: delivered as built, schema or no.
    let (server, _) = scripted(Ok(vec![host_tool("host_out", READ)]), failure.clone());
    let reply = server
        .call_tool_reply_in_request(
            "host_out",
            serde_json::json!({}),
            author(),
            Some(auth),
            SpiiceServices::default(),
        )
        .await
        .expect("a failure passes");
    assert_eq!(reply, failure);
}

/// A structured failure follows the content rule: delivered from a tool
/// without an output schema, refused from one that declared it.
#[tokio::test]
async fn a_structured_failure_is_refused_from_a_tool_with_an_output_schema() {
    let auth = owner_auth();
    let structured_failure = ToolReply::StructuredFailure {
        structured: serde_json::Map::from_iter([(
            "code".to_owned(),
            serde_json::json!("quota_exhausted"),
        )]),
        content: vec![ToolContent::text("quota exhausted")],
    };
    let (server, _) = scripted(
        Ok(vec![without_output_schema()]),
        structured_failure.clone(),
    );
    let reply = server
        .call_tool_reply_in_request(
            "host_out",
            serde_json::json!({}),
            author(),
            Some(auth.clone()),
            SpiiceServices::default(),
        )
        .await
        .expect("a structured failure from a tool that declared no schema");
    assert_eq!(reply, structured_failure);
    // A JSON-only surface cannot carry it.
    let json = server
        .call_tool(
            "host_out",
            serde_json::json!({}),
            author(),
            Some(auth.clone()),
        )
        .await;
    assert!(
        matches!(&json, Err(ToolInvocationError::Tool(ToolError::Other(_)))),
        "{json:?}"
    );
    let (server, _) = scripted(Ok(vec![host_tool("host_out", READ)]), structured_failure);
    let error = server
        .call_tool_reply_in_request(
            "host_out",
            serde_json::json!({}),
            author(),
            Some(auth),
            SpiiceServices::default(),
        )
        .await
        .expect_err("a structured failure against a declared schema is refused");
    let ToolInvocationError::Tool(error) = error else {
        panic!("expected an internal tool error, got {error:?}");
    };
    assert_eq!(error.kind(), ToolErrorKind::Internal);
    assert_eq!(error.client_message(), "internal server error");
}

/// A behavior that answers content for every call.
#[derive(Debug)]
struct ContentBehavior;

#[async_trait::async_trait]
impl proxiima_core::RequestBehavior for ContentBehavior {
    async fn handle(&self, _: ToolCall, _: Next<'_>) -> Result<ToolReply, ToolError> {
        Ok(ToolReply::Content(vec![ToolContent::text("replaced")]))
    }
}

/// Every registry tool declares an output schema, and a resource is JSON:
/// a behavior that replaces either with content makes an internal error.
#[tokio::test]
async fn a_behavior_cannot_turn_a_registry_tool_or_a_resource_into_content() {
    let mut registry = SpiiceRegistry::new();
    registry.add_request_behavior(ContentBehavior);
    let server = McpToolHost::from_parts(
        Arc::new(registry.freeze_or_panic_for_tests()),
        SpiiceServices::default(),
    );
    let auth = owner_auth();

    let tool = server
        .call_tool_reply_in_request(
            "core_memory_spaces",
            serde_json::json!({}),
            author(),
            Some(auth.clone()),
            SpiiceServices::default(),
        )
        .await;
    assert!(
        matches!(&tool, Err(ToolInvocationError::Tool(ToolError::Other(_)))),
        "{tool:?}"
    );
    let resource = server
        .read_resource("proxiima://schemas", author(), Some(auth))
        .await;
    assert!(
        matches!(
            &resource,
            Err(ToolInvocationError::Tool(ToolError::Other(_)))
        ),
        "{resource:?}"
    );
}

#[tokio::test]
async fn call_tool_without_bound_auth_is_denied() {
    let server = make_server();
    let author = ToolCaller::new("test-model", "test-client", "0.1.0");

    let err = server
        .call_tool("core_search_memories", serde_json::json!({}), author, None)
        .await
        .expect_err("missing bound owner/auth must deny");

    assert!(matches!(err, ToolInvocationError::NotAuthorized(_)));
}
