use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use proxiima_core::catalog::{ActionArgSpec, OwnerNeed, ToolAudience};
use proxiima_core::{AuthPath, AuthzContext, ScopeKey, SpiiceRegistry, SpiiceServices, ToolScope};
use proxiima_core::{Tool, ToolCtx, ToolError};
use rmcp::service::{RunningService, serve_directly};
use serde_json::{Value, json};

use super::*;

const FLAT: &str = "observed_ping";
const DISPATCH: &str = "observed_dispatch";

#[derive(Clone, Debug, Default)]
struct Seen(Arc<Mutex<Vec<ToolCtx>>>);

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FlatArgs {}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct Output {}

#[derive(Debug)]
struct Flat;

impl Tool for Flat {
    const TITLE: &'static str = "Observed flat tool";
    const NAME: &'static str = FLAT;
    const DESCRIPTION: &'static str = "Retains its context.";
    const EFFECT: Option<ToolEffect> = Some(ToolEffect::ReadOnly);
    type Args = FlatArgs;
    type Output = Output;

    fn call(ctx: ToolCtx, _: FlatArgs) -> BoxFuture<'static, Result<Output, ToolError>> {
        Box::pin(async move {
            ctx.service::<Seen>()
                .expect("fixture service")
                .0
                .lock()
                .expect("seen")
                .push(ctx.clone());
            Ok(Output {})
        })
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
enum DispatchArgs {
    /// Read the fixture.
    Look {},
    /// Change the fixture.
    Touch {},
}

#[derive(Debug)]
struct Dispatch;

impl Tool for Dispatch {
    const TITLE: &'static str = "Observed dispatcher";
    const NAME: &'static str = DISPATCH;
    const DESCRIPTION: &'static str = "Exercises action scope decisions.";
    const ACTION_ARG_SPECS: &'static [ActionArgSpec] = &[
        ActionArgSpec {
            action: "look",
            allowed_fields: &[],
            required_fields: &[],
            effect: ToolEffect::ReadOnly,
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
            title: "Look",
        },
        ActionArgSpec {
            action: "touch",
            allowed_fields: &[],
            required_fields: &[],
            effect: ToolEffect::ReadOnly,
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
            title: "Touch",
        },
    ];
    type Args = DispatchArgs;
    type Output = Output;

    fn call(_: ToolCtx, _: DispatchArgs) -> BoxFuture<'static, Result<Output, ToolError>> {
        Box::pin(async { Ok(Output {}) })
    }
}

struct Fixture {
    handler: DynamicHandler,
    running: RunningService<RoleServer, DynamicHandler>,
    _remote: tokio::io::DuplexStream,
    seen: Seen,
}

impl Fixture {
    fn new() -> Self {
        let mut registry = SpiiceRegistry::new();
        registry.add_tool_or_panic_for_tests::<Flat>("observed");
        registry.add_tool_or_panic_for_tests::<Dispatch>("observed");
        let seen = Seen::default();
        let mut services = SpiiceServices::default();
        services.try_insert(seen.clone()).expect("unique service");
        let handler = DynamicHandler::new(ToolHost::from_parts(
            Arc::new(registry.freeze_or_panic_for_tests()),
            services,
        ));
        let (transport, remote) = tokio::io::duplex(4096);
        let running = serve_directly::<RoleServer, _, _, _, _>(handler.clone(), transport, None);
        Self {
            handler,
            running,
            _remote: remote,
            seen,
        }
    }

    fn context(&self, scope: Option<ToolScope>) -> RequestContext<RoleServer> {
        let mut context = RequestContext::new(
            rmcp::model::NumberOrString::Number(1),
            self.running.peer().clone(),
        );
        if let Some(scope) = scope {
            let owner = proxiima_core::test_fixtures::owner_fixture();
            let (mut parts, ()) = http::Request::new(()).into_parts();
            parts.extensions.insert(AuthContext {
                owner,
                authz: AuthzContext::single_owner(&owner, AuthPath::HostBearer)
                    .with_tool_scope(scope),
            });
            context.extensions.insert(parts);
        }
        context
    }
}

fn request(name: &str, args: &Value) -> CallToolRequestParams {
    serde_json::from_value(json!({"name": name, "arguments": args})).expect("tool request")
}

fn key(value: &str) -> ScopeKey {
    ScopeKey::parse(value).expect("fixture key")
}

#[tokio::test]
async fn observed_flat_and_action_calls_report_the_gate_key() {
    let fixture = Fixture::new();
    for (name, args, expected) in [
        (FLAT, json!({}), FLAT),
        (
            DISPATCH,
            json!({"action": "look"}),
            "observed_dispatch:look",
        ),
    ] {
        let (judged, reply) = fixture
            .handler
            .call_tool_observed(request(name, &args), &fixture.context(Some(ToolScope::All)))
            .await;
        assert!(reply.is_ok());
        assert_eq!(judged, Some(key(expected)));
    }
    assert_eq!(
        fixture.seen.0.lock().expect("seen")[0].judged(),
        Some(key(FLAT))
    );
}

#[tokio::test]
async fn observed_scope_denial_retains_the_denied_action() {
    let fixture = Fixture::new();
    let (judged, reply) = fixture
        .handler
        .call_tool_observed(
            request(DISPATCH, &json!({"action": "touch"})),
            &fixture.context(Some(ToolScope::Palette(vec![key(
                "observed_dispatch:look",
            )]))),
        )
        .await;
    let error = reply.expect_err("touch is outside the palette");
    assert!(error.message.contains("not authorized"), "{error:?}");
    assert_eq!(judged, Some(key("observed_dispatch:touch")));
}

#[tokio::test]
async fn observed_invalid_actions_have_no_judged_key() {
    let fixture = Fixture::new();
    for args in [
        json!({}),
        json!({"action": 7}),
        json!({"action": "unknown"}),
    ] {
        let (judged, reply) = fixture
            .handler
            .call_tool_observed(
                request(DISPATCH, &args),
                &fixture.context(Some(ToolScope::All)),
            )
            .await;
        assert!(reply.is_err());
        assert_eq!(judged, None);
    }
}

#[tokio::test]
async fn observed_early_refusals_have_no_judged_key() {
    let fixture = Fixture::new();
    for (name, args, scope) in [
        ("missing_tool", json!({}), Some(ToolScope::All)),
        (FLAT, json!({}), None),
        (FLAT, json!({"bad": "\u{0}"}), Some(ToolScope::All)),
    ] {
        let (judged, reply) = fixture
            .handler
            .call_tool_observed(request(name, &args), &fixture.context(scope))
            .await;
        assert!(reply.is_err());
        assert_eq!(judged, None);
    }
}

#[tokio::test]
async fn observations_are_isolated_even_when_another_call_is_pending() {
    let fixture = Fixture::new();
    let flat_call = fixture.handler.call_tool_observed(
        request(FLAT, &json!({})),
        &fixture.context(Some(ToolScope::All)),
    );
    let action_call = fixture.handler.call_tool_observed(
        request(DISPATCH, &json!({"action": "look"})),
        &fixture.context(Some(ToolScope::All)),
    );
    let (flat_key, flat_reply) = flat_call.await;
    assert!(flat_reply.is_ok());
    assert_eq!(flat_key, Some(key(FLAT)));
    let retained = fixture.seen.0.lock().expect("seen")[0].clone();
    let (action_key, action_reply) = action_call.await;
    assert!(action_reply.is_ok());
    assert_eq!(action_key, Some(key("observed_dispatch:look")));
    assert_eq!(flat_key, Some(key(FLAT)));
    assert_eq!(retained.judged(), flat_key);
    let (invalid_key, invalid_reply) = fixture
        .handler
        .call_tool_observed(
            request(DISPATCH, &json!({})),
            &fixture.context(Some(ToolScope::All)),
        )
        .await;
    assert!(invalid_reply.is_err());
    assert_eq!(invalid_key, None);
    assert_eq!(retained.judged(), Some(key(FLAT)));
}

impl Fixture {
    fn host_call<'a>(
        &'a self,
        name: &'a str,
        args: Value,
        scope: Option<ToolScope>,
    ) -> impl Future<
        Output = (
            Option<ScopeKey>,
            Result<ToolReply, crate::ToolInvocationError>,
        ),
    > + use<'a> {
        let owner = proxiima_core::test_fixtures::owner_fixture();
        let auth = scope.map(|scope| AuthContext {
            owner,
            authz: AuthzContext::single_owner(&owner, AuthPath::HostBearer).with_tool_scope(scope),
        });
        self.handler.server.call_tool_reply_observed_in_request(
            name,
            args,
            proxiima_core::ToolCaller::new("fixture-model", "fixture", "1"),
            auth,
            SpiiceServices::default(),
        )
    }
}

#[tokio::test]
async fn observed_host_calls_retain_the_flat_and_action_gate_keys() {
    let fixture = Fixture::new();
    for (name, args, expected) in [
        (FLAT, json!({}), FLAT),
        (
            DISPATCH,
            json!({"action": "look"}),
            "observed_dispatch:look",
        ),
    ] {
        let (judged, reply) = fixture.host_call(name, args, Some(ToolScope::All)).await;
        assert_eq!(
            reply.expect("admitted reply"),
            ToolReply::Structured(json!({}))
        );
        assert_eq!(judged, Some(key(expected)));
    }
    assert_eq!(
        fixture.seen.0.lock().expect("seen")[0].judged(),
        Some(key(FLAT))
    );
}

#[tokio::test]
async fn observed_host_scope_refusal_retains_the_denied_action() {
    let fixture = Fixture::new();
    let (judged, reply) = fixture
        .host_call(
            DISPATCH,
            json!({"action": "touch"}),
            Some(ToolScope::Palette(vec![key("observed_dispatch:look")])),
        )
        .await;
    assert!(matches!(
        reply,
        Err(crate::ToolInvocationError::NotAuthorized(_))
    ));
    assert_eq!(judged, Some(key("observed_dispatch:touch")));
}

#[tokio::test]
async fn observed_host_pre_gate_refusals_leave_the_key_empty() {
    let fixture = Fixture::new();
    for (name, args, scope) in [
        (DISPATCH, json!({}), Some(ToolScope::All)),
        (DISPATCH, json!({"action": 7}), Some(ToolScope::All)),
        (DISPATCH, json!({"action": "unknown"}), Some(ToolScope::All)),
        ("missing_tool", json!({}), Some(ToolScope::All)),
        (FLAT, json!({}), None),
        (FLAT, json!({"bad": "\u{0}"}), Some(ToolScope::All)),
    ] {
        let (judged, reply) = fixture.host_call(name, args, scope).await;
        assert!(reply.is_err());
        assert_eq!(judged, None);
    }
}

#[tokio::test]
async fn observed_host_keys_are_isolated_while_another_call_is_pending() {
    let fixture = Fixture::new();
    let first_call = fixture.host_call(FLAT, json!({}), Some(ToolScope::All));
    let second_call = fixture.host_call(DISPATCH, json!({"action": "look"}), Some(ToolScope::All));
    let (first, first_reply) = first_call.await;
    assert!(first_reply.is_ok());
    assert_eq!(first, Some(key(FLAT)));
    let retained = fixture.seen.0.lock().expect("seen")[0].clone();
    let (second, second_reply) = second_call.await;
    assert!(second_reply.is_ok());
    assert_eq!(second, Some(key("observed_dispatch:look")));
    let (invalid, invalid_reply) = fixture
        .host_call(DISPATCH, json!({}), Some(ToolScope::All))
        .await;
    assert!(invalid_reply.is_err());
    assert_eq!(invalid, None);
    assert_eq!(first, Some(key(FLAT)));
    assert_eq!(retained.judged(), Some(key(FLAT)));
}
