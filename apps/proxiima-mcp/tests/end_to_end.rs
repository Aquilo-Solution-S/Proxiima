mod common;

use std::collections::{BTreeSet, HashMap};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::KeySize;
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use boon::{Compiler, Schemas};
use jsonwebtoken::DecodingKey;
use proxiima::{AuthorizationServers, Proxiima, ResourceServerMetadata};
use proxiima_auth_oidc::{OidcAuthConfig, OidcAuthenticator, OidcSubjectMap, StaticJwksResolver};
use proxiima_core::{OwnerAccessPort, OwnerId, SpiiceRegistry, UserId, provider_safe_tool_name};
use proxiima_mcp::ProxiimaMcpApp;
use serde_json::json;
use uuid::Uuid;

const KID: &str = "end-to-end-key";
const ISSUER: &str = "https://idp.end-to-end.test";
const AUDIENCE: &str = "proxiima-mcp";
const OUTPUT_SCHEMA_URL: &str = "https://proxiima.test/core-fact-output.json";

/// Ceiling on the serialized `tools/list` result for the authorized full
/// catalog. Clients that put every definition into context pay for each
/// char, so growth is a decision: trim prose or schema before raising it.
/// Measured in bytes when set (#375): 71,509 before, 42,601 after; 39,048
/// when proxiima-mcp stopped linking a spiice (#473); 41,175 when restore left
/// `core_forget` for its own `core_restore`, after trimming both tools' prose;
/// 41,267 when an unanswered store became `{store, why}` (#523); 43,586 when
/// retention added `core_retention` and `core_set_retention`, prose trimmed.
/// Goal collaboration adds six actions, typed access/use vocabularies and
/// `core_fork`; their descriptions are compacted before this bounded increase.
const TOOLS_LIST_BUDGET: usize = 47_000;

#[test]
fn an_explicit_object_root_constrains_every_union_branch() {
    for keyword in ["anyOf", "oneOf"] {
        let mut schema = json!({
            "type": "object",
            keyword: [{"type": "object"}, {"type": "string"}],
        });
        let original = schema.clone();
        proxiima_core::catalog::normalize_mcp_output_schema(&mut schema)
            .expect("an explicit object root already constrains the union");
        assert_eq!(schema, original);
        let mut compiler = Compiler::new();
        compiler.add_resource(OUTPUT_SCHEMA_URL, schema).unwrap();
        let mut schemas = Schemas::new();
        let index = compiler.compile(OUTPUT_SCHEMA_URL, &mut schemas).unwrap();
        schemas.validate(&json!({}), index).unwrap();
        for value in [
            json!("scalar"),
            json!(null),
            json!([]),
            json!(42),
            json!(true),
        ] {
            assert!(
                schemas.validate(&value, index).is_err(),
                "{keyword} cannot override the root object constraint: {value}"
            );
        }
    }
}

#[test]
fn composed_tools_publish_object_output_schemas() {
    let mut registry = SpiiceRegistry::new();
    <ProxiimaMcpApp as proxiima::spiice::SpiiceBundle>::register(&mut registry)
        .expect("compiled spiices register");
    let registry = registry.try_freeze().expect("compiled registry freezes");
    for tool in registry.list_mcp_tools() {
        assert_eq!(
            tool.output_schema["type"], "object",
            "{} must publish an object output root: {}",
            tool.name, tool.output_schema
        );
    }
    for name in ["core_goal", "core_fact", "core_membership", "core_upload"] {
        let tool = registry
            .list_mcp_tools()
            .iter()
            .find(|tool| tool.name == name)
            .expect("core union tool registered");
        let branches = tool.output_schema["anyOf"]
            .as_array()
            .expect("output union must remain intact");
        assert!(!branches.is_empty(), "{name} must retain its variants");
        assert!(branches.iter().all(|branch| branch["type"] == "object"));
    }
}

fn keypair() -> (RsaKeyPair, StaticJwksResolver) {
    let signing = RsaKeyPair::generate(KeySize::Rsa2048).expect("generate test RSA key");
    let dec = DecodingKey::from_rsa_der(signing.public_key().as_ref());
    let mut keys = HashMap::new();
    keys.insert(KID.to_string(), Arc::new(dec));
    (signing, StaticJwksResolver::new(keys))
}

fn mint(signing: &RsaKeyPair, sub: &str) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let header = json!({"alg": "RS256", "kid": KID, "typ": "JWT"});
    let claims = json!({"iss": ISSUER, "aud": AUDIENCE, "sub": sub, "exp": exp});
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).expect("serialize header")),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("serialize claims"))
    );
    let mut signature = vec![0; signing.public_modulus_len()];
    signing
        .sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        )
        .expect("sign jwt");
    format!("{}.{}", signing_input, URL_SAFE_NO_PAD.encode(signature))
}

/// What the server printed before its listening line; `None` when its output
/// ended without one.
fn lines_before_listening(output: std::process::ChildStdout) -> Option<Vec<String>> {
    use std::io::BufRead as _;
    let mut before = Vec::new();
    for line in std::io::BufReader::new(output)
        .lines()
        .map_while(Result::ok)
    {
        if line.contains("proxiima-mcp listening") {
            return Some(before);
        }
        before.push(line);
    }
    None
}

/// Answer every request `listener` receives with 200 until one is an OTLP
/// metrics export whose body names `metric`: an instrument name is a plain
/// string in the protobuf payload.
async fn accept_export_naming(
    listener: &tokio::net::TcpListener,
    metric: &str,
) -> std::io::Result<()> {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
    loop {
        let (stream, _) = listener.accept().await?;
        let mut stream = tokio::io::BufReader::new(stream);
        let mut request_line = String::new();
        stream.read_line(&mut request_line).await?;
        let mut length = 0;
        loop {
            let mut header = String::new();
            stream.read_line(&mut header).await?;
            let Some((name, value)) = header.trim_end().split_once(':') else {
                break;
            };
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().map_err(std::io::Error::other)?;
            }
        }
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await?;
        if request_line.starts_with("POST /v1/metrics ")
            && body
                .windows(metric.len())
                .any(|window| window == metric.as_bytes())
        {
            return Ok(());
        }
    }
}

/// `proxiima-mcp migrate`, end to end through the binary: a server refuses a
/// fresh database and names the migrate step, `migrate` brings the database
/// current, and a server then starts on it and pushes its metrics to the
/// OTLP endpoint its environment names. A misspelled `PROXIIMA_*` variable in
/// the environment is named in a warning, its value is not, and neither
/// command is refused for it.
#[tokio::test]
async fn migrate_brings_a_fresh_database_current_for_a_serving_start()
-> Result<(), Box<dyn std::error::Error>> {
    const UNKNOWN: &str = "PROXIIMA_STORE_NUMBR";
    const UNKNOWN_VALUE: &str = "unknown-setting-value";

    let db = proxiima_pg_testkit::SplitRoleDb::create("proxiima_mcp_migrate_cli", &[]).await?;
    let (signing, _) = keypair();
    let public = signing.public_key();
    let jwks = json!({"keys": [{
        "kty": "RSA",
        "kid": KID,
        "alg": "RS256",
        "use": "sig",
        "n": URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),
        "e": URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero()),
    }]})
    .to_string();
    let subject_map =
        json!([{"iss": ISSUER, "sub": "migrate-cli", "user_id": Uuid::now_v7()}]).to_string();
    let command = |subcommand: &str| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_proxiima-mcp"));
        command
            .env_clear()
            .arg(subcommand)
            .args(["--database-url", db.runtime_url()])
            .envs([
                ("PROXIIMA_PLATFORM_DATABASE_URL", db.platform_url()),
                ("PROXIIMA_STORE_NUMBER", "0"),
                ("PROXIIMA_OIDC_ISSUER", ISSUER),
                ("PROXIIMA_OIDC_AUDIENCE", AUDIENCE),
                ("PROXIIMA_OIDC_JWKS_JSON", &jwks),
                ("PROXIIMA_OIDC_SUBJECT_MAP_JSON", &subject_map),
                ("PROXIIMA_PUBLIC_URL", "https://proxiima.test"),
                (UNKNOWN, UNKNOWN_VALUE),
            ]);
        command
    };

    let refused = command("serve").args(["--bind", "127.0.0.1:0"]).output()?;
    let stderr = String::from_utf8(refused.stderr)?;
    assert!(!refused.status.success(), "{stderr}");
    assert!(
        stderr.contains("run the host's `migrate` command"),
        "{stderr}"
    );

    let migrated = command("migrate").output()?;
    let stdout = String::from_utf8(migrated.stdout)?;
    let stderr = String::from_utf8(migrated.stderr)?;
    assert!(migrated.status.success(), "{stderr}");
    assert!(
        stdout.contains("migrate: the database is current"),
        "{stdout}"
    );
    assert!(
        stderr.contains(UNKNOWN),
        "the unknown name is logged: {stderr}"
    );
    assert!(
        !stderr.contains(UNKNOWN_VALUE),
        "its value is not: {stderr}"
    );

    let collector = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let mut server = command("serve")
        .args(["--bind", "127.0.0.1:0"])
        .env(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            format!("http://{}", collector.local_addr()?),
        )
        // Milliseconds. A database gauge appears once its first read is in.
        .env("OTEL_METRIC_EXPORT_INTERVAL", "200")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let output = server.stdout.take().ok_or("server stdout")?;
    let listening = tokio::task::spawn_blocking(move || lines_before_listening(output));
    let listening = tokio::time::timeout(std::time::Duration::from_mins(1), listening).await;
    let pushed = tokio::time::timeout(
        std::time::Duration::from_mins(1),
        accept_export_naming(&collector, "proxiima.embedding.jobs"),
    )
    .await;
    server.kill()?;
    server.wait()?;
    let Ok(Ok(Some(log))) = listening else {
        panic!("the server starts on the migrated database: {listening:?}");
    };
    let log = log.join("\n");
    assert!(log.contains(UNKNOWN), "the unknown name is logged: {log}");
    assert!(!log.contains(UNKNOWN_VALUE), "its value is not: {log}");
    assert!(
        matches!(pushed, Ok(Ok(()))),
        "the server exports a runtime metric to the endpoint: {pushed:?}"
    );
    Ok(())
}

#[tokio::test]
async fn oidc_host_auth_serves_tools_list() -> Result<(), Box<dyn std::error::Error>> {
    if proxiima_pg_testkit::admin_url_or_skip().is_none() {
        return Ok(());
    }
    // All-feature and shipped-default hosts have different projection tables.
    // Each catalog test boots a database for its actual linked spiices.
    let fixture = proxiima_pg_testkit::SplitRoleDb::create("mcp_catalog_e2e", &[]).await?;
    let runtime_url = fixture.runtime_url().to_owned();
    let platform_url = fixture.platform_url().to_owned();
    let subject = UserId::new(Uuid::now_v7());
    let owner_key = OwnerId::from(subject).to_string();
    let (signing, resolver) = keypair();
    let mut subject_map = OidcSubjectMap::new();
    subject_map.insert(ISSUER, "operator-sub", subject)?;
    let owner_access: Arc<dyn OwnerAccessPort> =
        Arc::new(common::owner_access(&runtime_url, &platform_url).await?);
    let authn = OidcAuthenticator::new(
        OidcAuthConfig {
            issuer: ISSUER.to_string(),
            jwks_uri: None,
            audience: AUDIENCE.to_string(),
            allowed_subjects: None,
            leeway_secs: 60,
        },
        Arc::new(resolver),
        subject_map,
        owner_access,
    )?;

    let running = Proxiima::<ProxiimaMcpApp>::app()
        .configure(|b| {
            b.served_surface(proxiima_core::ServedSurface::All)
                .database_url(runtime_url.clone())
                .store_number(0)
                .platform_database_url(platform_url.clone())
                .owner(OwnerId::from(subject))
                .authenticator(Arc::new(authn))
                .resource_metadata(ResourceServerMetadata {
                    public_url: "https://proxiima.end-to-end.test".to_string(),
                    authorization_servers: AuthorizationServers::Issuers(vec![ISSUER.to_string()]),
                    additional_scopes: Vec::new(),
                })
                .mcp_bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
        })
        .run()
        .await?;
    let addr = running.mcp_addr().ok_or("missing MCP listener address")?;
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/mcp");
    let bearer = format!("Bearer {}", mint(&signing, "operator-sub"));
    let session_id = initialize(&client, &url, &bearer, &owner_key).await?;
    initialized(&client, &url, &session_id, &bearer).await?;
    let body = post_rpc(
        &client,
        &url,
        Some(&session_id),
        &bearer,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await?;
    let listed = body["result"]["tools"].as_array().expect("tools");
    let names: Vec<_> = listed
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"core_remember"), "got {names:?}");
    assert!(names.contains(&"core_goal"), "got {names:?}");

    let expected: BTreeSet<_> = running
        .host()
        .registry()
        .list_mcp_tools()
        .iter()
        .map(|tool| provider_safe_tool_name(tool.name))
        .collect();
    let actual: BTreeSet<_> = names.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(
        actual, expected,
        "the full authorized catalog must be listed"
    );
    for tool in listed {
        assert_eq!(
            tool["outputSchema"]["type"], "object",
            "strict MCP clients require object output roots: {tool}"
        );
        assert_plain_wire_schemas(tool);
    }
    let size = serde_json::to_string(&body["result"])?.len();
    assert!(
        size <= TOOLS_LIST_BUDGET,
        "tools/list is {size} bytes, over its {TOOLS_LIST_BUDGET}-byte budget"
    );

    assert_union_reply_matches_schema(&client, &url, &session_id, &bearer, listed).await?;

    running.shutdown().await;
    Ok(())
}

/// One reply against the output schema `tools/list` published for its tool,
/// as a strict client checks it.
fn assert_reply_matches_listed_schema(
    listed: &[serde_json::Value],
    name: &str,
    output: &serde_json::Value,
) {
    assert!(output.is_object(), "{name} replied {output}");
    let schema = &listed
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("{name} listed"))["outputSchema"];
    let url = format!("https://proxiima.test/{name}-output.json");
    let mut compiler = Compiler::new();
    compiler
        .add_resource(&url, schema.clone())
        .expect("published output schema loads");
    let mut schemas = Schemas::new();
    let index = compiler
        .compile(&url, &mut schemas)
        .expect("published output schema compiles");
    if let Err(error) = schemas.validate(output, index) {
        panic!("{name} structuredContent must validate against outputSchema: {error:#}");
    }
}

/// `tools/list` carries plain JSON Schema: no `x-` extension anywhere, no
/// `$schema`, and output schemas without prose (`description`, `title`,
/// `examples`) — models never see an output schema; clients only validate
/// against it.
fn assert_plain_wire_schemas(tool: &serde_json::Value) {
    fn visit(schema: &serde_json::Value, name: &str, output: bool, path: &str) {
        let Some(map) = schema.as_object() else {
            return;
        };
        for key in map.keys() {
            assert!(
                !key.starts_with("x-") && key != "$schema",
                "{name}: `{key}` at {path}"
            );
            assert!(
                !output || !["description", "title", "examples"].contains(&key.as_str()),
                "{name}: output schema keeps `{key}` at {path}"
            );
        }
        for keyword in ["properties", "patternProperties", "$defs"] {
            for (child, subschema) in map
                .get(keyword)
                .and_then(serde_json::Value::as_object)
                .into_iter()
                .flatten()
            {
                visit(
                    subschema,
                    name,
                    output,
                    &format!("{path}/{keyword}/{child}"),
                );
            }
        }
        for keyword in ["anyOf", "oneOf", "allOf", "prefixItems"] {
            for (index, subschema) in map
                .get(keyword)
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                visit(
                    subschema,
                    name,
                    output,
                    &format!("{path}/{keyword}/{index}"),
                );
            }
        }
        for keyword in [
            "items",
            "additionalProperties",
            "not",
            "if",
            "then",
            "else",
            "contains",
        ] {
            if let Some(subschema) = map.get(keyword) {
                visit(subschema, name, output, &format!("{path}/{keyword}"));
            }
        }
    }
    let name = tool["name"].as_str().unwrap_or_default();
    visit(&tool["inputSchema"], name, false, "inputSchema");
    visit(&tool["outputSchema"], name, true, "outputSchema");
}

async fn assert_union_reply_matches_schema(
    client: &reqwest::Client,
    url: &str,
    session_id: &str,
    bearer: &str,
    listed: &[serde_json::Value],
) -> Result<(), Box<dyn std::error::Error>> {
    let cited_object_id = Uuid::now_v7().to_string();
    let called = post_rpc(
        client,
        url,
        Some(session_id),
        bearer,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "core_fact",
                "arguments": {
                    "action": "facts_citing_object",
                    "cited_object_id": cited_object_id,
                },
            },
        }),
    )
    .await?;
    assert!(called.get("error").is_none(), "{called}");
    let output = &called["result"]["structuredContent"];
    assert!(output.is_object(), "{called}");
    assert_eq!(output["cited_object_id"], cited_object_id);
    assert_eq!(output["facts"], json!([]));
    assert_reply_matches_listed_schema(listed, "core_fact", output);

    // A write reply whose type skips a field when unset: the published
    // schema must not require what serde leaves out.
    let call = |id: u64, name: &str, arguments: serde_json::Value| {
        let request = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        });
        async move { post_rpc(client, url, Some(session_id), bearer, request).await }
    };
    let mut facts = Vec::new();
    for (id, title) in [(4, "first observation"), (5, "second observation")] {
        let remembered = call(
            id,
            "core_remember",
            json!({ "title": title, "body": format!("{title} for the schema check") }),
        )
        .await?;
        let output = &remembered["result"]["structuredContent"];
        assert_reply_matches_listed_schema(listed, "core_remember", output);
        facts.push(output["handle"].clone());
    }
    let derived = call(
        6,
        "core_derive",
        json!({
            "kind": "Abstraction",
            "title": "a pattern",
            "body": "a pattern over both observations",
            "source_handles": facts,
        }),
    )
    .await?;
    assert_reply_matches_listed_schema(
        listed,
        "core_derive",
        &derived["result"]["structuredContent"],
    );
    let found = call(
        7,
        "core_search_memories",
        json!({ "query": "observation", "mode": "lexical" }),
    )
    .await?;
    assert!(
        found.get("error").is_none(),
        "core_search_memories raw MCP reply: {found}"
    );
    assert!(
        found["result"]["structuredContent"].is_object(),
        "core_search_memories missing structuredContent: {found}"
    );
    assert_reply_matches_listed_schema(
        listed,
        "core_search_memories",
        &found["result"]["structuredContent"],
    );
    let spaces = call(8, "core_memory_spaces", json!({})).await?;
    assert_reply_matches_listed_schema(
        listed,
        "core_memory_spaces",
        &spaces["result"]["structuredContent"],
    );

    Ok(())
}

async fn initialize(
    client: &reqwest::Client,
    url: &str,
    bearer: &str,
    owner_key: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let response = client
        .post(url)
        .header("Origin", "http://localhost")
        .header("Authorization", bearer)
        .header("X-Proxiima-Owner", owner_key)
        .header("MCP-Protocol-Version", "2025-03-26")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "e2e", "version": "0"}
            }
        }))
        .send()
        .await?;
    assert!(response.status().is_success(), "{}", response.status());
    let session_id = response
        .headers()
        .get("Mcp-Session-Id")
        .ok_or("missing session id")?
        .to_str()?
        .to_string();
    let _ = sse_json(response).await?;
    Ok(session_id)
}

async fn initialized(
    client: &reqwest::Client,
    url: &str,
    session_id: &str,
    bearer: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = client
        .post(url)
        .header("Origin", "http://localhost")
        .header("Authorization", bearer)
        .header("MCP-Protocol-Version", "2025-03-26")
        .header("Mcp-Session-Id", session_id)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .send()
        .await?;
    assert!(response.status().is_success(), "{}", response.status());
    Ok(())
}

async fn post_rpc(
    client: &reqwest::Client,
    url: &str,
    session_id: Option<&str>,
    bearer: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let mut request = client
        .post(url)
        .header("Origin", "http://localhost")
        .header("Authorization", bearer)
        .header("MCP-Protocol-Version", "2025-03-26")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(session_id) = session_id {
        request = request.header("Mcp-Session-Id", session_id);
    }
    let response = request.send().await?;
    assert!(response.status().is_success(), "{}", response.status());
    sse_json(response).await
}

async fn sse_json(
    response: reqwest::Response,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let text = response.text().await?;
    for data in text.lines().filter_map(|line| line.strip_prefix("data:")) {
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str(data) {
            return Ok(value);
        }
    }
    Err(format!("missing JSON SSE data in {text:?}").into())
}

/// #477: a changed-content replay returns `idempotency_conflict` with
/// `error.data.existing` on MCP and `existing` on the REST problem body.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn idempotency_conflict_names_existing_on_mcp_and_rest()
-> Result<(), Box<dyn std::error::Error>> {
    if proxiima_pg_testkit::admin_url_or_skip().is_none() {
        return Ok(());
    }
    let fixture = proxiima_pg_testkit::SplitRoleDb::create("mcp_idempotency_e2e", &[]).await?;
    let runtime_url = fixture.runtime_url().to_owned();
    let platform_url = fixture.platform_url().to_owned();
    let subject = UserId::new(Uuid::now_v7());
    let owner_key = OwnerId::from(subject).to_string();
    let (signing, resolver) = keypair();
    let mut subject_map = OidcSubjectMap::new();
    subject_map.insert(ISSUER, "idem-operator-sub", subject)?;
    let owner_access: Arc<dyn OwnerAccessPort> =
        Arc::new(common::owner_access(&runtime_url, &platform_url).await?);
    let authn = OidcAuthenticator::new(
        OidcAuthConfig {
            issuer: ISSUER.to_string(),
            jwks_uri: None,
            audience: AUDIENCE.to_string(),
            allowed_subjects: None,
            leeway_secs: 60,
        },
        Arc::new(resolver),
        subject_map,
        owner_access,
    )?;

    let app = Proxiima::<ProxiimaMcpApp>::app().configure(|b| {
        b.served_surface(proxiima_core::ServedSurface::All)
            .database_url(runtime_url.clone())
            .store_number(0)
            .platform_database_url(platform_url.clone())
            .owner(OwnerId::from(subject))
            .authenticator(Arc::new(authn))
            .resource_metadata(ResourceServerMetadata {
                public_url: "https://proxiima.end-to-end.test".to_string(),
                authorization_servers: AuthorizationServers::Issuers(vec![ISSUER.to_string()]),
                additional_scopes: Vec::new(),
            })
            .mcp_bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
    });
    #[cfg(feature = "rest")]
    let app = app.configure(|b| b.rest_enabled(true));
    let running = app.run().await?;
    let addr = running.mcp_addr().ok_or("missing MCP listener address")?;
    let client = reqwest::Client::new();
    let mcp_url = format!("http://{addr}/mcp");
    let bearer = format!("Bearer {}", mint(&signing, "idem-operator-sub"));
    let session_id = initialize(&client, &mcp_url, &bearer, &owner_key).await?;
    initialized(&client, &mcp_url, &session_id, &bearer).await?;

    let first = post_rpc(
        &client,
        &mcp_url,
        Some(&session_id),
        &bearer,
        json!({
            "jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": {
                "name": "core_remember",
                "arguments": {
                    "title": "idem e2e",
                    "body": "first body",
                    "tags": [],
                    "idempotency_key": "e2e-idem-key",
                },
            },
        }),
    )
    .await?;
    assert!(first.get("error").is_none(), "{first}");
    let handle = first["result"]["structuredContent"]["handle"]
        .as_str()
        .ok_or("handle")?
        .to_owned();

    let conflict = post_rpc(
        &client,
        &mcp_url,
        Some(&session_id),
        &bearer,
        json!({
            "jsonrpc": "2.0", "id": 11, "method": "tools/call",
            "params": {
                "name": "core_remember",
                "arguments": {
                    "title": "idem e2e",
                    "body": "changed body",
                    "tags": [],
                    "idempotency_key": "e2e-idem-key",
                },
            },
        }),
    )
    .await?;
    let err = conflict
        .get("error")
        .ok_or_else(|| format!("expected error: {conflict}"))?;
    assert_eq!(err["code"], -32600, "{conflict}");
    assert_eq!(err["data"]["code"], "idempotency_conflict", "{conflict}");
    assert_eq!(err["data"]["existing"], handle, "{conflict}");

    #[cfg(feature = "rest")]
    {
        let rest = client
            .post(format!("http://{addr}/v1/tools/core_remember"))
            .header("Origin", "http://localhost")
            .header("Authorization", &bearer)
            .header("X-Proxiima-Owner", &owner_key)
            .header("Content-Type", "application/json")
            .header("Accept", "application/problem+json, application/json")
            .json(&json!({
                "title": "idem e2e",
                "body": "changed again",
                "tags": [],
                "idempotency_key": "e2e-idem-key",
            }))
            .send()
            .await?;
        assert_eq!(
            rest.status(),
            reqwest::StatusCode::CONFLICT,
            "{}",
            rest.status()
        );
        let problem: serde_json::Value = rest.json().await?;
        let ty = problem["type"].as_str().unwrap_or("");
        assert!(
            ty.contains("idempotency-conflict"),
            "type={ty}, body={problem}"
        );
        assert_eq!(problem["existing"], handle, "{problem}");
    }

    running.shutdown().await;
    Ok(())
}
