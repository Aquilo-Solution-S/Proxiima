//! The exporter reads `OTEL_EXPORTER_OTLP_ENDPOINT` from the process
//! environment, which a test cannot edit (`set_var` is unsafe), so the binary
//! runs with it set.

#[test]
fn serve_refuses_an_endpoint_that_is_not_a_uri() {
    let refused = std::process::Command::new(env!("CARGO_BIN_EXE_proxiima-mcp"))
        .env_clear()
        .arg("serve")
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", "not a uri")
        .output()
        .expect("run proxiima-mcp");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{stderr}");
    assert!(stderr.contains("OTEL_EXPORTER_OTLP_ENDPOINT"), "{stderr}");
}
