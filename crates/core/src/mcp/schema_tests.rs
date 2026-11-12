use super::*;
use schemars::JsonSchema;
use serde::Deserialize;

#[test]
fn output_object_unions_gain_a_root_type_without_changing_branches() {
    for keyword in ["anyOf", "oneOf"] {
        let branches = serde_json::json!([
            { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] },
            { "type": "object", "properties": { "count": { "type": "integer" } }, "required": ["count"] }
        ]);
        let mut schema = serde_json::json!({ "title": "Output union", keyword: branches });
        normalize_mcp_output_schema(&mut schema).unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema[keyword], branches);
        assert_eq!(schema["title"], "Output union");
        assert!(schema.get("properties").is_none());
    }
}

#[test]
fn output_normalization_requires_every_present_union_to_be_objects() {
    let object_branch = serde_json::json!([{ "type": "object" }]);
    let mut valid = serde_json::json!({ "anyOf": object_branch, "oneOf": object_branch });
    normalize_mcp_output_schema(&mut valid).unwrap();
    assert_eq!(valid["type"], "object");
    for mut invalid in [
        serde_json::json!(true),
        serde_json::json!(false),
        serde_json::json!({}),
        serde_json::json!({ "properties": { "value": { "type": "string" } } }),
        serde_json::json!({ "type": ["object"] }),
        serde_json::json!({ "type": "null" }),
        serde_json::json!({ "type": "array" }),
        serde_json::json!({ "type": "string", "anyOf": object_branch }),
        serde_json::json!({ "anyOf": [] }),
        serde_json::json!({ "oneOf": true }),
        serde_json::json!({ "anyOf": [{ "type": "object" }, { "type": "string" }] }),
        serde_json::json!({ "oneOf": [{ "type": "object" }, {}] }),
        serde_json::json!({ "anyOf": object_branch, "oneOf": [] }),
    ] {
        let original = invalid.clone();
        assert!(
            normalize_mcp_output_schema(&mut invalid).is_err(),
            "{invalid}"
        );
        assert_eq!(invalid, original, "invalid schemas must remain unchanged");
    }
}

#[test]
fn generated_output_unions_preserve_variant_validation() {
    let untagged = mcp_output_schema::<UntaggedRootUnion>().unwrap();
    assert_eq!(untagged["type"], "object");
    assert_eq!(untagged["anyOf"].as_array().unwrap().len(), 2);
    let tagged = mcp_output_schema::<CollidingDispatcher>().unwrap();
    assert_eq!(tagged["type"], "object");
    assert_eq!(tagged["oneOf"].as_array().unwrap().len(), 2);
    assert!(tagged.get("properties").is_none());
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct Inner {
    label: String,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct Nested {
    inner: Inner,
    count: u32,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct ReservedSchemaPropertyNames {
    #[serde(rename = "$ref")]
    reference: String,
    #[serde(rename = "$defs")]
    defs: String,
    #[serde(rename = "definitions")]
    definitions: String,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct Recursive {
    /// A self-referential field makes this type unrepresentable as a
    /// finite `$ref`-free schema.
    next: Option<Box<Recursive>>,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct Described {
    /// Description authored as a doc-comment.
    documented: String,
    #[schemars(description = "Description authored as a schemars attribute.")]
    attributed: String,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
#[serde(untagged)]
enum UntaggedRootUnion {
    Text { text: String },
    Count { count: u32 },
}

#[derive(JsonSchema)]
#[allow(dead_code)]
#[serde(tag = "action", rename_all = "snake_case")]
enum CollidingDispatcher {
    Text { value: String },
    Count { value: u32 },
}

/// A dispatcher whose discriminator tag is `kind`, not `action` — the
/// out-of-tree spiice shape (e.g. working-hero's query tool). The flattener
/// must honor the actual serde tag rather than assuming `action`.
#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
#[serde(tag = "kind")]
enum Demo {
    /// Inspect one value without changing it.
    #[serde(rename = "a")]
    A { x: Option<String> },
    /// Apply the requested change.
    #[serde(rename = "b")]
    B {},
}

/// A `kind`-tagged dispatcher with a field (`shared`) whose prose differs
/// by variant, so the render inlines each variant's prose. That text must
/// name the actual discriminator (`kind`), not a hardcoded `action`.
#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
#[serde(tag = "kind")]
enum DemoShared {
    #[serde(rename = "left")]
    Left {
        /// Text for the left side.
        shared: Option<String>,
    },
    #[serde(rename = "right")]
    Right {
        /// Text for the right side.
        shared: String,
        /// Only the right side counts.
        count: u32,
    },
    #[serde(rename = "middle")]
    Middle {
        /// Text for the left side.
        shared: Option<String>,
    },
}

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
#[serde(tag = "shape", deny_unknown_fields)]
enum NestedPayload {
    Text { action: String, text: String },
    Number { number: i64 },
}

#[derive(Deserialize, JsonSchema)]
#[allow(dead_code)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum ConditionalDispatcher {
    Submit { payload: NestedPayload },
    Clear {},
}

#[derive(Deserialize)]
#[allow(dead_code)]
#[serde(tag = "action")]
enum RootConditionalDispatcher {
    Choose { mode: String },
    Reset {},
}

impl JsonSchema for RootConditionalDispatcher {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RootConditionalDispatcher".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "choose", "type": "string" },
                        "mode": { "type": "string" }
                    },
                    "required": ["action", "mode"],
                    "additionalProperties": false,
                    "if": {
                        // Presence matters for the condition: without
                        // this requirement, an absent mode would satisfy
                        // the const-only property subschema.
                        "properties": { "mode": { "const": "strict" } },
                        "required": ["mode"]
                    },
                    "then": {
                        "description": "strict-only branch",
                        "properties": { "level": {} },
                        "required": ["level"]
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "reset", "type": "string" }
                    },
                    "required": ["action"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
#[serde(tag = "action")]
enum UnhoistedBranchRequiredDispatcher {
    Choose { mode: String },
    Reset {},
}

impl JsonSchema for UnhoistedBranchRequiredDispatcher {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "UnhoistedBranchRequiredDispatcher".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "choose", "type": "string" },
                        "mode": { "type": "string" }
                    },
                    "required": ["action", "mode"],
                    "additionalProperties": false,
                    "if": {
                        "type": "object",
                        "properties": { "mode": { "const": "strict" } }
                    },
                    "then": {
                        "type": "object",
                        "required": ["x"]
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "reset", "type": "string" }
                    },
                    "required": ["action"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
#[serde(tag = "action")]
enum RefDispatcher {
    Submit { payload: String },
    Clear {},
}

impl JsonSchema for RefDispatcher {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RefDispatcher".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "$defs": {
                "Payload": {
                    "type": "object",
                    "properties": { "value": { "type": "string" } },
                    "required": ["value"],
                    "additionalProperties": false
                },
                "Submit": {
                    "type": "object",
                    "properties": {
                        "action": { "const": "submit", "type": "string" },
                        "payload": {
                            "$ref": "#/$defs/Payload",
                            "description": "payload"
                        }
                    },
                    "required": ["action", "payload"],
                    "description": "target submit"
                }
            },
            "oneOf": [
                {
                    "$ref": "#/$defs/Submit",
                    "description": "submit",
                    "properties": { "extra": { "type": "integer" } }
                },
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "clear", "type": "string" }
                    },
                    "required": ["action"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
#[serde(tag = "action")]
enum ConditionalFieldCollisionDispatcher {
    Conditional { mode: String },
    Typed { x: i64 },
}

impl JsonSchema for ConditionalFieldCollisionDispatcher {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ConditionalFieldCollisionDispatcher".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "conditional", "type": "string" },
                        "mode": { "type": "string" }
                    },
                    "required": ["action"],
                    "additionalProperties": false,
                    "if": {
                        "type": "object",
                        "properties": { "mode": { "const": "strict" } },
                        "required": ["mode"]
                    },
                    "then": {
                        "type": "object",
                        "properties": { "x": { "type": "string" } }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "action": { "const": "typed", "type": "string" },
                        "x": { "type": "integer" }
                    },
                    "required": ["action", "x"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
#[serde(tag = "action")]
enum UntypedVariantDispatcher {
    Run { value: String },
    Clear {},
}

impl JsonSchema for UntypedVariantDispatcher {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "UntypedVariantDispatcher".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "oneOf": [
                {
                    "properties": {
                        "action": { "const": "run", "type": "string" },
                        "value": { "type": "string" }
                    },
                    "required": ["action", "value"]
                },
                {
                    "properties": {
                        "action": { "const": "clear", "type": "string" }
                    },
                    "required": ["action"]
                }
            ]
        })
    }
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct PartiallyDescribed {
    #[schemars(description = "A described field.")]
    described: String,
    bare: String,
}

fn dispatcher(args: &McpArgsSchema) -> &McpDispatcherSchema {
    args.dispatcher
        .as_ref()
        .unwrap_or_else(|| panic!("a tagged enum derives a dispatcher: {:#}", args.schema))
}

#[test]
fn the_action_guide_gives_each_action_its_prose_and_fields() {
    let args = mcp_tool_schema::<Demo>();
    let guide = args
        .schema
        .pointer("/properties/kind/description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("kind discriminator description present: {:#}", args.schema));
    assert!(
        guide.contains("\n- a: Inspect one value without changing it. Optional: x."),
        "{guide}"
    );
    assert!(
        guide.contains("\n- b: Apply the requested change. No fields."),
        "{guide}"
    );
}

#[test]
fn undescribed_property_names_flags_only_bare_fields() {
    let schema = mcp_tool_schema::<PartiallyDescribed>().schema;
    assert_eq!(
        undescribed_property_names(&schema),
        vec!["bare".to_string()]
    );
}

#[test]
fn shared_field_prose_is_inlined_per_action_under_the_actual_discriminator() {
    let schema = mcp_tool_schema::<DemoShared>().schema;
    let shared = &schema["properties"]["shared"];
    assert_eq!(
        shared["description"],
        "Depends on `kind`:\n- left, middle: Text for the left side.\n- right: Text for the right side.",
        "{schema:#}"
    );
    // Nullability widens; the prose of a one-action field is its own.
    assert_eq!(shared["type"], serde_json::json!(["string", "null"]));
    assert_eq!(
        schema["properties"]["count"]["description"],
        "Only the right side counts."
    );
}

#[test]
fn a_narrowed_render_names_only_the_permitted_actions() {
    let args = mcp_tool_schema::<DemoShared>();
    let narrowed = dispatcher(&args).render(|action| action != "right");
    assert_eq!(
        narrowed["properties"]["kind"]["enum"],
        serde_json::json!(["left", "middle"])
    );
    assert!(
        narrowed.pointer("/properties/count").is_none(),
        "{narrowed:#}"
    );
    assert_eq!(
        narrowed["properties"]["shared"]["description"],
        "Text for the left side."
    );
    let guide = narrowed["properties"]["kind"]["description"]
        .as_str()
        .unwrap();
    assert!(!guide.contains("- right"), "{guide}");
    assert_eq!(dispatcher(&args).render(|_| true), args.schema);
}

#[test]
fn invalid_dispatcher_refs_fail_registration() {
    let cases = [
        (
            serde_json::json!("#/$defs/Missing"),
            serde_json::json!({ "Known": { "type": "string" } }),
            "unresolved local reference",
        ),
        (
            serde_json::json!("https://example.test/schema"),
            serde_json::json!({ "Known": { "type": "string" } }),
            "non-local reference",
        ),
        (
            serde_json::json!("#/$defs/A"),
            serde_json::json!({
                "A": { "$ref": "#/$defs/B" },
                "B": { "$ref": "#/$defs/A" }
            }),
            "cyclic local reference",
        ),
    ];
    for (reference, defs, expected) in cases {
        let schema = serde_json::json!({
            "$defs": defs,
            "oneOf": [{
                "type": "object",
                "properties": {
                    "action": { "const": "run", "type": "string" },
                    "value": { "$ref": reference }
                },
                "required": ["action", "value"],
                "additionalProperties": false
            }]
        });
        let error = derive_dispatcher(&schema).expect_err("invalid refs reject");
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn core_goal_modify_nullable_evidence_is_safe_on_the_flat_root() {
    use crate::mcp::core_tools::goal::{CoreGoalArgs, CoreGoalTool};
    use crate::mcp::{McpTool, validate_action_args};

    let registry = crate::SpiiceRegistry::default().freeze_or_panic_for_tests();
    let descriptor = registry
        .list_mcp_tools()
        .iter()
        .find(|tool| tool.name == "core_goal")
        .expect("core_goal registered");
    let argument = descriptor
        .action_argument_schema("modify")
        .expect("modify action schema");
    assert_eq!(
        argument["properties"]["evidence"]["type"],
        serde_json::json!(["array", "null"])
    );

    for evidence in [
        serde_json::Value::Null,
        serde_json::json!(["00000000-0000-8000-8000-000000000001"]),
    ] {
        let value = serde_json::json!({
            "action": "modify",
            "goal": "00000000-0000-8000-8000-000000000002",
            "schema_id": "goal",
            "title": "title",
            "text": "text",
            "evidence": evidence.clone()
        });
        assert!(
            validate_action_args("core_goal", CoreGoalTool::ACTION_ARG_SPECS, &value).is_ok(),
            "flat precheck rejects {value:#}"
        );
        assert!(
            evaluates(&descriptor.args_schema, &value),
            "flat schema rejects {value:#}"
        );
        assert!(
            serde_json::from_value::<CoreGoalArgs>(value).is_ok(),
            "serde rejects {evidence:#}"
        );
    }
}

/// Every registered tool's wire schemas are plain JSON Schema: no `x-`
/// keyword, no `$schema`, no root Rust type name, no non-standard
/// `format`; a dispatcher's is its full render.
#[test]
fn registered_wire_schemas_are_plain_json_schema() {
    fn keywords(value: &serde_json::Value, out: &mut Vec<String>) {
        let Some(map) = value.as_object() else {
            return;
        };
        for (key, child) in map {
            out.push(key.clone());
            if let Some(format) = child.as_str().filter(|_| key == "format") {
                out.push(format!("format:{format}"));
            }
        }
        let mut map = map.clone();
        for_each_subschema_mut(&mut map, &mut |child| keywords(child, out));
    }
    let registry = crate::SpiiceRegistry::default().freeze_or_panic_for_tests();
    for tool in registry.list_mcp_tools() {
        let mut found = Vec::new();
        keywords(&tool.args_schema, &mut found);
        keywords(&mcp_wire_output_schema(&tool.output_schema), &mut found);
        assert!(
            !tool.args_schema.to_string().contains("\"default\":null"),
            "{}: default null",
            tool.name
        );
        for keyword in &found {
            assert!(
                !keyword.starts_with("x-") && keyword != "$schema",
                "{}: {keyword}",
                tool.name
            );
            if let Some(format) = keyword.strip_prefix("format:") {
                assert!(
                    STANDARD_FORMATS.contains(&format),
                    "{}: {format}",
                    tool.name
                );
            }
        }
        assert!(tool.args_schema.get("title").is_none(), "{}", tool.name);
        if let Some(dispatcher) = &tool.dispatcher_schema {
            assert_eq!(
                dispatcher.render(|_| true),
                tool.args_schema,
                "{}",
                tool.name
            );
        }
    }
}

#[test]
fn doc_comment_wraps_are_rejoined_but_paragraphs_and_lists_kept() {
    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Wrapped {
        /// A description a source file
        /// wrapped at its line width.
        field: String,
    }
    assert_eq!(
        unwrap_lines(
            "One sentence wrapped\nat the source width.\n\nA paragraph:\n- one\n- two\n1. three"
        ),
        "One sentence wrapped at the source width.\n\nA paragraph:\n- one\n- two\n1. three"
    );
    assert_eq!(
        mcp_tool_schema::<Wrapped>().schema["properties"]["field"]["description"],
        "A description a source file wrapped at its line width."
    );
}

#[test]
fn the_wire_output_schema_keeps_validation_and_property_names() {
    let documented = serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "Reply",
        "description": "What the tool returns.",
        "type": "object",
        "properties": {
            "description": {
                "type": "string",
                "description": "A field named description.",
                "examples": ["text"]
            },
            "count": { "type": "integer", "format": "uint32", "minimum": 0, "default": 1 },
            "at": { "type": "string", "format": "date-time", "title": "When" }
        },
        "required": ["description"]
    });
    assert_eq!(
        mcp_wire_output_schema(&documented),
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": { "type": "string" },
                "count": { "type": "integer", "minimum": 0 },
                "at": { "type": "string", "format": "date-time" }
            },
            "required": ["description"]
        })
    );
}

/// A reply is what its type *serializes* to: a field serde may skip is
/// optional, else a strict client refuses the reply that omits it.
#[test]
fn output_schemas_follow_the_serialize_contract() {
    #[derive(serde::Serialize, JsonSchema)]
    struct Reply {
        always: u32,
        #[serde(skip_serializing_if = "core::ops::Not::not")]
        deferred: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        maybe: Option<u32>,
    }
    let schema = mcp_output_schema::<Reply>().unwrap();
    assert_eq!(
        schema["required"],
        serde_json::json!(["always"]),
        "{schema:#}"
    );
}

#[test]
#[should_panic(expected = "is recursive")]
fn recursive_type_panics() {
    let _ = mcp_tool_schema::<Recursive>();
}

#[test]
#[should_panic(expected = "root schema combinator")]
fn unflattenable_root_union_panics() {
    let _ = mcp_tool_schema::<UntaggedRootUnion>();
}

#[test]
#[should_panic(expected = "root schema type")]
fn non_object_root_panics() {
    let _ = mcp_tool_schema::<String>();
}

#[test]
#[should_panic(expected = "conflicting property")]
fn tagged_enum_duplicate_incompatible_properties_panic() {
    let _ = mcp_tool_schema::<CollidingDispatcher>();
}

#[test]
fn non_action_discriminator_flattens_under_its_own_tag() {
    let args = mcp_tool_schema::<Demo>();
    let schema = &args.schema;

    assert_eq!(
        schema.get("type").and_then(serde_json::Value::as_str),
        Some("object"),
        "kind-tagged dispatcher must have an object root: {schema:#}",
    );
    assert!(
        schema
            .get("properties")
            .is_some_and(serde_json::Value::is_object),
        "kind-tagged dispatcher must expose a top-level properties object: {schema:#}",
    );
    for combinator in ["oneOf", "anyOf", "allOf"] {
        assert!(
            schema.get(combinator).is_none(),
            "kind-tagged dispatcher must not leave a root {combinator}: {schema:#}",
        );
    }

    // The discriminator lives at `properties.kind`, NOT `properties.action`.
    assert!(
        schema.pointer("/properties/action").is_none(),
        "kind-tagged dispatcher must not invent an `action` discriminator: {schema:#}",
    );
    let mut kind_values = schema
        .pointer("/properties/kind/enum")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("discriminator must live at properties.kind.enum: {schema:#}"))
        .iter()
        .map(|value| value.as_str().expect("kind enum values are strings"))
        .collect::<Vec<_>>();
    kind_values.sort_unstable();
    assert_eq!(
        kind_values,
        ["a", "b"],
        "kind enum must carry the renamed variant values: {schema:#}",
    );

    // `required` names the detected discriminator, not `action`.
    let required = schema
        .pointer("/required")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("flattened schema must declare required: {schema:#}"));
    assert!(
        required.iter().any(|item| item == "kind"),
        "flattened schema must require the `kind` discriminator: {schema:#}",
    );
    assert!(
        !required.iter().any(|item| item == "action"),
        "flattened schema must not require a phantom `action` field: {schema:#}",
    );

    // The typed contract is keyed by the variant values, not by `action`.
    let dispatcher = dispatcher(&args);
    assert_eq!(dispatcher.discriminator, "kind");
    assert_eq!(
        dispatcher
            .actions
            .iter()
            .map(|action| action.action.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"],
    );
    assert_eq!(
        dispatcher
            .action("a")
            .and_then(|a| a.description.as_deref()),
        Some("Inspect one value without changing it.")
    );

    // The optional `x` field of variant `a` survives as a top-level property.
    assert!(
        schema.pointer("/properties/x").is_some(),
        "variant fields must be merged into top-level properties: {schema:#}",
    );
}

#[test]
fn action_argument_schema_preserves_nested_union_and_closes_only_its_root() {
    let args = mcp_tool_schema::<ConditionalDispatcher>();
    let argument = &dispatcher(&args)
        .action("submit")
        .expect("submit action schema present")
        .argument_schema;
    assert_eq!(argument.get("type"), Some(&serde_json::json!("object")));
    assert_eq!(
        argument.get("additionalProperties"),
        Some(&serde_json::Value::Bool(false))
    );
    assert!(argument.pointer("/properties/action").is_none());
    assert!(argument.pointer("/properties/payload/oneOf").is_some());
    assert!(
        argument
            .pointer("/properties/payload/oneOf/0/properties/action")
            .is_some()
    );
    assert!(
        argument
            .pointer("/properties/payload/properties/text")
            .is_none()
    );
    assert!(
        argument
            .pointer("/properties/payload/properties/number")
            .is_none()
    );
    assert!(!schema_contains_ref(argument));
    assert!(argument.get("$defs").is_none());
    let fields = analyze_root_fields(argument).unwrap();
    assert_eq!(fields.allowed, ["payload"]);
    assert_eq!(fields.required, ["payload"]);
}

#[test]
fn root_condition_fields_are_hoisted_but_requirements_stay_conditional() {
    let args = mcp_tool_schema::<RootConditionalDispatcher>();
    let choose = dispatcher(&args)
        .action("choose")
        .expect("choose action schema present");
    let argument = &choose.argument_schema;
    assert_eq!(
        argument["properties"]["mode"],
        serde_json::json!({
            "type": "string"
        })
    );
    assert_eq!(argument["properties"]["level"], serde_json::json!({}));
    assert!(argument.pointer("/if/properties/mode").is_some());
    assert!(argument.pointer("/then/properties/level").is_some());
    assert!(argument.pointer("/properties/action").is_none());
    let fields = analyze_root_fields(argument).unwrap();
    assert_eq!(fields.allowed, ["mode", "level"]);
    assert_eq!(fields.required, ["mode"]);
    assert!(
        choose.description.is_none(),
        "a conditional branch description is not an action description"
    );
    assert!(!evaluates(argument, &serde_json::json!({})));
    assert!(!evaluates(argument, &serde_json::json!({ "level": 1 })));
    assert!(evaluates(argument, &serde_json::json!({ "mode": "loose" })));
    assert!(evaluates(
        argument,
        &serde_json::json!({ "mode": "loose", "level": "text" })
    ));
    assert!(!evaluates(
        argument,
        &serde_json::json!({ "mode": "strict" })
    ));
    assert!(evaluates(
        argument,
        &serde_json::json!({ "mode": "strict", "level": 2 })
    ));
}

#[test]
fn root_field_analysis_accepts_empty_required_and_rejects_bad_names() {
    let empty = serde_json::json!({
        "type": "object",
        "properties": {},
        "required": [],
        "additionalProperties": false
    });
    assert_eq!(
        analyze_root_fields(&empty).unwrap().required,
        Vec::<String>::new()
    );
    for required in [
        serde_json::json!([""]),
        serde_json::json!(["value", "value"]),
    ] {
        let mut malformed = empty.clone();
        malformed["required"] = required;
        assert!(analyze_root_fields(&malformed).is_err(), "{malformed:#}");
    }
}

#[test]
fn object_applicators_may_omit_type_but_reject_an_explicit_non_object_type() {
    let implicit_object = serde_json::json!({
        "type": "object",
        "properties": { "mode": { "type": "string" } },
        "additionalProperties": false,
        "then": {
            "properties": { "mode": { "const": "strict" } },
            "required": ["mode"]
        }
    });
    validate_closed_root_schema(&implicit_object)
        .expect("object applicators inherit the action root instance type");
    assert_eq!(
        analyze_root_fields(&implicit_object)
            .expect("implicit object applicator analyzes")
            .allowed,
        vec!["mode"]
    );

    let explicit_string = serde_json::json!({
        "type": "object",
        "properties": { "mode": { "type": "string" } },
        "additionalProperties": false,
        "then": {
            "type": "string",
            "properties": { "mode": { "const": "strict" } }
        }
    });
    let error = validate_closed_root_schema(&explicit_string)
        .expect_err("an explicit non-object type contradicts root fields");
    assert!(error.contains("explicit non-object type"), "{error}");
}

#[test]
fn validation_shape_never_rewrites_literal_instance_objects() {
    let left = serde_json::json!({
        "const": {
            "description": "left",
            "type": ["string", "null"]
        },
        "description": "annotation"
    });
    let right = serde_json::json!({
        "const": {
            "description": "right",
            "type": "string"
        },
        "description": "other annotation"
    });
    assert_ne!(
        validation_shape(&left),
        validation_shape(&right),
        "literal const objects are validation data, not nested schemas"
    );

    let mut annotation_only = left.clone();
    annotation_only["description"] = serde_json::json!("different annotation");
    assert_eq!(
        validation_shape(&left),
        validation_shape(&annotation_only),
        "schema-node annotations do not change compatibility"
    );
}

#[test]
#[should_panic(expected = "root schema combinator")]
fn an_untyped_action_variant_cannot_be_fabricated_as_an_object() {
    let _ = mcp_tool_schema::<UntypedVariantDispatcher>();
}

#[test]
#[should_panic(expected = "root schema combinator")]
fn an_unhoisted_branch_requirement_cannot_be_fabricated_as_a_flat_field() {
    let _ = mcp_tool_schema::<UnhoistedBranchRequiredDispatcher>();
}

/// Small draft-2020-12 evaluator for this test module. It intentionally
/// implements only the keywords emitted by the dispatcher fixture, so the
/// acceptance matrix below exercises the advertised contract rather than
/// relying on a second production validator.
fn evaluates(schema: &serde_json::Value, value: &serde_json::Value) -> bool {
    if let Some(constant) = schema.get("const")
        && constant != value
    {
        return false;
    }
    if let Some(values) = schema.get("enum").and_then(serde_json::Value::as_array)
        && !values.iter().any(|candidate| candidate == value)
    {
        return false;
    }
    if let Some(schema_type) = schema.get("type") {
        let matches = match schema_type {
            serde_json::Value::String(kind) => matches_type(kind, value),
            serde_json::Value::Array(kinds) => kinds
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|kind| matches_type(kind, value)),
            _ => false,
        };
        if !matches {
            return false;
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object);
        if let Some(required) = schema.get("required").and_then(serde_json::Value::as_array)
            && required
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|field| !object.contains_key(field))
        {
            return false;
        }
        if schema.get("additionalProperties") == Some(&serde_json::Value::Bool(false))
            && object
                .keys()
                .any(|field| !properties.is_some_and(|properties| properties.contains_key(field)))
        {
            return false;
        }
        if properties.is_some_and(|properties| {
            properties.iter().any(|(field, child)| {
                object
                    .get(field)
                    .is_some_and(|present| !evaluates(child, present))
            })
        }) {
            return false;
        }
    }
    for keyword in ["oneOf", "anyOf", "allOf"] {
        if let Some(branches) = schema.get(keyword).and_then(serde_json::Value::as_array) {
            let matches = branches
                .iter()
                .filter(|branch| evaluates(branch, value))
                .count();
            let valid = match keyword {
                "oneOf" => matches == 1,
                "anyOf" => matches > 0,
                _ => matches == branches.len(),
            };
            if !valid {
                return false;
            }
        }
    }
    if let Some(condition) = schema.get("if")
        && evaluates(condition, value)
    {
        if let Some(then) = schema.get("then")
            && !evaluates(then, value)
        {
            return false;
        }
    } else if let Some(otherwise) = schema.get("else")
        && !evaluates(otherwise, value)
    {
        return false;
    }
    true
}

fn matches_type(kind: &str, value: &serde_json::Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true,
    }
}
