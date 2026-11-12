use std::num::NonZeroU32;

use proxiima_core::PlaintextHosts;
use proxiima_core::llm::{
    EmbeddingClient, EmbeddingConfigurationFault, EmbeddingInputRefusal, LlmError,
    MIN_EMBED_INPUT_CAP_CHARS,
};
use proxiima_core::models::EmbedCaps;

fn decode_vectors(
    data: serde_json::Value,
    expected_count: usize,
) -> Result<Vec<Vec<f32>>, LlmError> {
    let response = super::OpenAiEmbeddingResponse {
        data: serde_json::from_value(data).expect("valid response data"),
    };
    response.into_embeddings(expected_count, EmbedCaps::new(2, false))
}

#[test]
fn response_rejects_duplicate_or_out_of_range_indices() {
    for indices in [[0, 0], [1, 1], [0, 2], [1, 2]] {
        let result = decode_vectors(
            serde_json::json!([
                { "index": indices[0], "embedding": [1.0, 0.0] },
                { "index": indices[1], "embedding": [0.0, 1.0] }
            ]),
            2,
        );
        assert!(
            matches!(result, Err(LlmError::Retryable { .. })),
            "invalid indices {indices:?} must not assign vectors to inputs: {result:?}",
        );
    }
}

#[test]
fn response_rejects_partially_indexed_batches() {
    for indices in [[Some(0), None], [None, Some(1)]] {
        let result = decode_vectors(
            serde_json::json!([
                { "index": indices[0], "embedding": [1.0, 0.0] },
                { "index": indices[1], "embedding": [0.0, 1.0] }
            ]),
            2,
        );
        assert!(
            matches!(result, Err(LlmError::Retryable { .. })),
            "partial indices must not silently fall back to array order: {result:?}",
        );
    }
}

#[test]
fn response_rejects_nonzero_index_for_single_input() {
    assert!(matches!(
        decode_vectors(
            serde_json::json!([{ "index": 1, "embedding": [1.0, 0.0] }]),
            1,
        ),
        Err(LlmError::Retryable { .. })
    ));
}

#[test]
fn response_rejects_wrong_count_or_dimensions() {
    for data in [
        serde_json::json!([]),
        serde_json::json!([{ "index": 0, "embedding": [1.0, 0.0] }]),
        serde_json::json!([
            { "index": 0, "embedding": [1.0, 0.0] },
            { "index": 1, "embedding": [1.0] }
        ]),
    ] {
        assert!(matches!(
            decode_vectors(data, 2),
            Err(LlmError::Retryable { .. })
        ));
    }
}

fn decode_raw_vectors(body: &str, expected_count: usize) -> Result<Vec<Vec<f32>>, LlmError> {
    let response: super::OpenAiEmbeddingResponse = serde_json::from_str(body)
        .map_err(|err| LlmError::retryable(format!("decode response: {err}")))?;
    response.into_embeddings(expected_count, EmbedCaps::new(2, false))
}

#[test]
fn response_rejects_float32_overflow() {
    // Use wire JSON, not json!: finite f64 values can overflow when
    // deserialized into the response's f32 components.
    for (body, expected) in [
        (
            r#"{"data":[{"index":0,"embedding":[1.0,-0.5]}]}"#,
            [1.0_f32, -0.5],
        ),
        (r#"{"data":[{"embedding":[0.0,0.0]}]}"#, [0.0, 0.0]),
        (
            r#"{"data":[{"embedding":[3.4028234e38,-3.4028234e38]}]}"#,
            [f32::MAX, -f32::MAX],
        ),
        (
            r#"{"data":[{"embedding":[-0.0,1e-45]}]}"#,
            [-0.0, f32::from_bits(1)],
        ),
    ] {
        let vectors = decode_raw_vectors(body, 1).expect("finite response remains valid");
        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors[0].len(), 2);
        assert!(vectors[0].iter().all(|value| value.is_finite()));
        assert_eq!(
            vectors[0]
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
            expected.map(f32::to_bits),
            "accepted values are not normalized, clamped or rewritten",
        );
    }
    for (body, count) in [
        (r#"{"data":[{"embedding":[1.0]}]}"#, 1),
        (r#"{"data":[{"embedding":[1.0,0.0]}]}"#, 2),
    ] {
        assert!(matches!(
            decode_raw_vectors(body, count),
            Err(LlmError::Retryable { .. })
        ));
    }
    let observed: Vec<_> = ["1e39", "-1e39", "1e100", "-1e100"]
        .into_iter()
        .map(|number| {
            let body = format!(r#"{{"data":[{{"index":0,"embedding":[{number},0.0]}}]}}"#);
            (number, decode_raw_vectors(&body, 1))
        })
        .collect();
    for (number, result) in observed {
        assert!(
            matches!(result, Err(LlmError::Retryable { .. })),
            "out-of-range component {number} must be a retryable malformed response: {result:?}",
        );
    }
}

#[test]
fn response_checks_later_vectors_and_components() {
    // The invalid component is last in the second logical vector, even
    // when the provider returns the indexed batch in reverse order.
    for body in [
        r#"{"data":[{"embedding":[1.0,0.0]},{"embedding":[0.0,1e39]}]}"#,
        r#"{"data":[{"index":1,"embedding":[0.0,-1e39]},{"index":0,"embedding":[1.0,0.0]}]}"#,
    ] {
        let result = decode_raw_vectors(body, 2);
        assert!(
            matches!(result, Err(LlmError::Retryable { ref message, .. })
                if message == "embedding 1 has non-finite component at position 1"),
            "later invalid values must reject the whole batch with bounded context: {result:?}",
        );
    }
}

#[test]
fn response_shape_errors_precede_non_finite_components() {
    for (body, count, expected_error) in [
        (
            r#"{"data":[{"index":7,"embedding":[1e39,0.0]}]}"#,
            2,
            "requested 2 embeddings, response carried 1",
        ),
        (
            r#"{"data":[{"index":0,"embedding":[1e39,0.0]},{"index":0,"embedding":[1.0]}]}"#,
            2,
            "invalid embedding response index",
        ),
        (
            r#"{"data":[{"index":0,"embedding":[1e39,0.0]},{"index":1,"embedding":[1.0]}]}"#,
            2,
            "expected dim 2 (matryoshka=false), got 1",
        ),
    ] {
        let result = decode_raw_vectors(body, count);
        assert!(
            matches!(result, Err(LlmError::Retryable { ref message, .. }) if message.starts_with(expected_error)),
            "existing shape error takes precedence over overflow: {result:?}",
        );
    }
}

fn probe_caps() -> EmbedCaps {
    EmbedCaps::new(8, false)
}

/// An endpoint that cannot answer. Any request that actually leaves the
/// process fails here, which is what makes "the input was refused before
/// it was sent" observable: the guard returns a typed local input refusal,
/// while a request returns a retryable transport failure.
const UNREACHABLE: &str = "http://127.0.0.1:9/v1";

fn floor_u32() -> u32 {
    u32::try_from(MIN_EMBED_INPUT_CAP_CHARS).expect("the floor fits u32")
}

fn capped_client(max_chars: u32) -> super::OpenAiCompatEmbeddingClient {
    super::OpenAiCompatEmbeddingClient::new(
        "test-embed",
        probe_caps().with_max_input_chars(NonZeroU32::new(max_chars).expect("positive")),
        super::OpenAiCompatConfig::new(UNREACHABLE, None),
    )
    .expect("a cap at or above the floor is accepted")
}

#[tokio::test]
async fn an_over_cap_input_is_refused_without_being_sent() {
    let client = capped_client(floor_u32());
    let too_long = "a".repeat(MIN_EMBED_INPUT_CAP_CHARS + 1);

    match client.embed(&too_long).await {
        Err(LlmError::InputRefused(EmbeddingInputRefusal::LocalCap {
            actual_chars,
            max_chars,
        })) => {
            assert_eq!(actual_chars, MIN_EMBED_INPUT_CAP_CHARS + 1);
            assert_eq!(max_chars, MIN_EMBED_INPUT_CAP_CHARS);
        }
        other => panic!("expected a local input refusal before sending, got {other:?}"),
    }
}

/// The other half: the guard must not stand in for the provider's own
/// judgement. An input inside the cap is sent, and against an endpoint
/// that cannot answer that surfaces as an ordinary retryable error.
#[tokio::test]
async fn an_input_within_the_cap_is_still_sent() {
    let client = capped_client(floor_u32());
    let err = client
        .embed(&"a".repeat(MIN_EMBED_INPUT_CAP_CHARS))
        .await
        .expect_err("the endpoint is unreachable");
    assert!(
        matches!(
            err,
            LlmError::Retryable {
                status: None,
                retry_after: None,
                ..
            }
        ),
        "an in-bounds input must reach the transport, got {err:?}",
    );
}

/// A batch is judged per input. One over-cap text refuses the call, which
/// is what routes the drain into per-input isolation so the other 31
/// still embed.
#[tokio::test]
async fn one_over_cap_text_refuses_the_batch_it_travels_in() {
    let client = capped_client(floor_u32());
    let batch = vec![
        "short".to_string(),
        "a".repeat(MIN_EMBED_INPUT_CAP_CHARS + 1),
        "also short".to_string(),
    ];
    assert!(
        matches!(
            client.embed_many(&batch).await,
            Err(LlmError::InputRefused(
                EmbeddingInputRefusal::LocalCap { .. }
            ))
        ),
        "an over-cap member must refuse the batch before it is sent",
    );
}

/// No cap is the default and preserves the prior behaviour exactly: the
/// provider judges its own input. A deployment whose provider rejects
/// cleanly does not need this guard.
#[tokio::test]
async fn without_a_cap_every_input_is_offered_to_the_provider() {
    let client = super::OpenAiCompatEmbeddingClient::new(
        "test-embed",
        probe_caps(),
        super::OpenAiCompatConfig::new(UNREACHABLE, None),
    )
    .expect("no cap is a valid configuration");
    assert!(client.caps.max_input_chars.is_none());

    let err = client
        .embed(&"a".repeat(MIN_EMBED_INPUT_CAP_CHARS * 10))
        .await
        .expect_err("the endpoint is unreachable");
    assert!(
        matches!(
            err,
            LlmError::Retryable {
                status: None,
                retry_after: None,
                ..
            }
        ),
        "with no cap the input must still be offered, got {err:?}",
    );
}

/// A cap below the floor is refused while someone is looking at it. The
/// alternative is a configuration that boots, behaves as documented at
/// every layer, and silently never embeds a long memory.
#[test]
fn a_cap_the_chunked_rescue_cannot_satisfy_is_refused_at_construction() {
    let err = super::OpenAiCompatEmbeddingClient::new(
        "test-embed",
        probe_caps().with_max_input_chars(
            NonZeroU32::new(floor_u32() - 1).expect("one under the floor is positive"),
        ),
        super::OpenAiCompatConfig::new(UNREACHABLE, None),
    )
    .expect_err("a cap under the floor must not build a client");
    assert!(
        matches!(err, LlmError::Internal(ref m) if m.contains("terminal")),
        "the refusal must say what goes wrong, not just that it did: {err:?}",
    );
}

#[test]
fn a_zero_timeout_is_refused_at_construction() {
    let err = super::OpenAiCompatEmbeddingClient::new(
        "test-embed",
        probe_caps(),
        super::OpenAiCompatConfig::new(UNREACHABLE, None).with_timeout(std::time::Duration::ZERO),
    )
    .expect_err("a zero timeout must not build a client");
    assert!(
        matches!(err, LlmError::Internal(ref m) if m.contains("timeout")),
        "{err:?}"
    );
}

#[test]
fn a_blank_bearer_token_is_refused_at_construction() {
    for blank in ["", "  "] {
        let err = super::OpenAiCompatEmbeddingClient::new(
            "test-embed",
            probe_caps(),
            super::OpenAiCompatConfig::new(UNREACHABLE, Some(blank.to_owned())),
        )
        .expect_err("a blank token must not build a client");
        assert!(
            matches!(err, LlmError::Internal(ref m) if m.contains("bearer token")),
            "{blank:?}: {err:?}"
        );
    }
}

#[test]
fn config_debug_redacts_bearer_token() {
    let cfg = super::OpenAiCompatConfig::new(
        "https://embeddings.example/v1",
        Some("sk-supersecret".into()),
    );
    let debug = format!("{cfg:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("sk-supersecret"));
    // Non-secret fields stay visible for diagnostics.
    assert!(debug.contains("embeddings.example"));
}

#[test]
fn config_debug_shows_absent_bearer_as_none() {
    let cfg = super::OpenAiCompatConfig::new("http://localhost:11434/v1", None);
    let debug = format!("{cfg:?}");
    assert!(debug.contains("None"));
    assert!(!debug.contains("<redacted>"));
}

#[test]
fn client_debug_inherits_bearer_redaction() {
    let client = super::OpenAiCompatEmbeddingClient::new(
        "test-embed",
        probe_caps(),
        super::OpenAiCompatConfig::new(
            "https://embeddings.example/v1",
            Some("sk-topsecret".into()),
        ),
    )
    .expect("client builds");
    let debug = format!("{client:?}");
    assert!(!debug.contains("sk-topsecret"));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn client_rejects_plaintext_non_loopback_base_url() {
    let cfg = super::OpenAiCompatConfig::new("HTTP://api.example.com/v1", Some("t".into()));
    let err = super::OpenAiCompatEmbeddingClient::new("test-embed", probe_caps(), cfg)
        .expect_err("plaintext remote base must be rejected");
    assert!(matches!(err, LlmError::Internal(_)));
}

#[test]
fn a_named_host_is_accepted_as_the_plaintext_base_url_and_others_are_not() {
    let build = |base: &str, hosts: Option<&str>| {
        let mut cfg = super::OpenAiCompatConfig::new(base, Some("t".into()));
        if let Some(hosts) = hosts {
            cfg = cfg.with_plaintext_hosts(PlaintextHosts::parse(hosts).unwrap());
        }
        super::OpenAiCompatEmbeddingClient::new("test-embed", probe_caps(), cfg)
    };
    build("http://gateway:9090/v1", Some("gateway")).expect("named host accepted");
    for (base, hosts) in [
        ("http://gateway:9090/v1", None),
        ("http://other:9090/v1", Some("gateway")),
        ("http://gateway.evil.example:9090/v1", Some("gateway")),
    ] {
        let err = build(base, hosts).expect_err("unnamed plaintext host refused");
        assert!(matches!(err, LlmError::Internal(_)), "{base} {hosts:?}");
    }
    build("http://127.0.0.1:9/v1", None).expect("loopback unchanged");
    build("https://api.example.com/v1", None).expect("https unchanged");
}

#[test]
fn client_rejects_base_url_fragments_at_construction() {
    for base in ["http://127.0.0.1:9/v1#anchor", "https://api.example/v1#"] {
        let result = super::OpenAiCompatEmbeddingClient::new(
            "test-embed",
            probe_caps(),
            super::OpenAiCompatConfig::new(base, None),
        );
        assert!(
            matches!(result, Err(LlmError::Internal(_))),
            "fragment-bearing base must fail at construction: {base}",
        );
    }
}

pub(super) fn assert_http_failure(
    error: &LlmError,
    status: u16,
    retry_after: Option<std::time::Duration>,
) {
    let input = match status {
        413 => Some(EmbeddingInputRefusal::PayloadTooLarge),
        422 => Some(EmbeddingInputRefusal::UnprocessableContent),
        _ => None,
    };
    if let Some(expected) = input {
        assert!(
            matches!(error, LlmError::InputRefused(actual) if *actual == expected),
            "{error:?}"
        );
        return;
    }
    let configuration = match status {
        400 => Some(EmbeddingConfigurationFault::BadRequest),
        401 => Some(EmbeddingConfigurationFault::Unauthorized),
        403 => Some(EmbeddingConfigurationFault::Forbidden),
        404 => Some(EmbeddingConfigurationFault::NotFound),
        _ => None,
    };
    if let Some(expected) = configuration {
        assert!(
            matches!(error, LlmError::Configuration(actual) if *actual == expected),
            "{error:?}"
        );
        return;
    }
    let LlmError::Retryable {
        status: actual_status,
        retry_after: actual_delay,
        ..
    } = error
    else {
        panic!("status {status} must retain work: {error:?}");
    };
    assert_eq!(*actual_status, Some(status));
    assert_eq!(*actual_delay, retry_after);
}

#[test]
fn status_policy_is_independent_of_error_body() {
    let bodies = [
        "",
        "EOF",
        r#"{"error":{"message":"input exceeds the configured limit"}}"#,
        r#"{"message":"temporary service condition"}"#,
        "arbitrary text with no protocol meaning",
    ];
    let retry_after = Some(std::time::Duration::from_mins(2));
    for code in (100..200).chain(300..1_000) {
        let status = reqwest::StatusCode::from_u16(code).expect("valid HTTP status");
        for body in bodies {
            let error = super::embed_http_error(status, retry_after, body);
            assert_http_failure(&error, code, retry_after);
            if let LlmError::Retryable { message, .. } = error {
                assert!(message.ends_with(body));
            }
        }
    }
}

fn retry_after(value: Option<&str>, now: std::time::SystemTime) -> Option<std::time::Duration> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(value) = value {
        headers.insert(
            reqwest::header::RETRY_AFTER,
            value.parse().expect("test header"),
        );
    }
    super::retry_after_delay(&headers, now)
}

#[test]
fn retry_after_parses_delta_seconds_with_conservative_overflow() {
    use std::time::{Duration, SystemTime};
    for (value, expected) in [
        ("0", Duration::ZERO),
        ("123", Duration::from_secs(123)),
        (" 00120 ", Duration::from_mins(2)),
        ("18446744073709551615", Duration::from_secs(u64::MAX)),
        ("18446744073709551616", Duration::MAX),
        (&"9".repeat(300), Duration::MAX),
    ] {
        assert_eq!(
            retry_after(Some(value), SystemTime::UNIX_EPOCH),
            Some(expected),
            "{value}"
        );
    }
}

#[test]
fn retry_after_parses_http_dates_against_the_supplied_clock() {
    use std::time::{Duration, SystemTime};
    let now = SystemTime::UNIX_EPOCH + Duration::from_mins(13_068_529);
    for value in [
        "Sun, 06 Nov 1994 08:49:37 GMT",
        "Sunday, 06-Nov-94 08:49:37 GMT",
        "Sun Nov  6 08:49:37 1994",
    ] {
        assert_eq!(retry_after(Some(value), now), Some(Duration::from_secs(37)));
        assert_eq!(
            retry_after(Some(value), now + Duration::from_secs(37)),
            Some(Duration::ZERO)
        );
        assert_eq!(
            retry_after(Some(value), now + Duration::from_mins(1)),
            Some(Duration::ZERO)
        );
    }
}

#[test]
fn retry_after_missing_or_invalid_is_absent() {
    use std::time::SystemTime;
    for value in [
        None,
        Some(""),
        Some(" "),
        Some("-1"),
        Some("+1"),
        Some("1.5"),
        Some("tomorrow"),
        Some("123, 456"),
        Some("１２３"),
        Some("Sun, 40 Nov 1994 08:49:37 GMT"),
    ] {
        assert_eq!(
            retry_after(value, SystemTime::UNIX_EPOCH),
            None,
            "{value:?}"
        );
    }
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        reqwest::header::HeaderValue::from_bytes(&[0xff]).expect("opaque header bytes"),
    );
    assert_eq!(
        super::retry_after_delay(&headers, SystemTime::UNIX_EPOCH),
        None
    );
}
