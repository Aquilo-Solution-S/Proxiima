use super::{
    CHUNKED_EMBED_MIN_BYTES, ChunkedEmbedding, DEFAULT_EMBED_BATCH_SIZE,
    DEFAULT_EMBED_REQUEST_TIMEOUT, DEFAULT_EMBED_STALE_CLAIM_TIMEOUT,
    DEFAULT_EMBED_WORKER_INTERVAL, EmbeddingClient, EmbeddingConfigurationFault,
    EmbeddingInputRefusal, EmbeddingRuntimePolicy, EmbeddingRuntimePolicyError, LlmError,
    MAX_EMBED_BATCH_SIZE, MAX_EMBED_DRAIN_CONCURRENCY, MAX_EMBED_REQUEST_TIMEOUT,
    MAX_EMBED_STALE_CLAIM_TIMEOUT, MIN_EMBED_INPUT_CAP_CHARS, embed_in_chunks,
    embed_in_chunks_after_failure, embed_in_chunks_after_failure_with_timeout,
    embed_in_chunks_with_timeout, embed_many_with_timeout, embed_with_timeout,
};
use crate::env as settings;
use crate::env_from_pairs as lookup;

const PROXIIMA_EMBED_BATCH_SIZE: &str = settings::PROXIIMA_EMBED_BATCH_SIZE.name();
const PROXIIMA_EMBED_DRAIN_CONCURRENCY: &str = settings::PROXIIMA_EMBED_DRAIN_CONCURRENCY.name();
const PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS: &str =
    settings::PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS.name();
const PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS: &str =
    settings::PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS.name();
const PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS: &str =
    settings::PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS.name();
use async_trait::async_trait;
use std::sync::Mutex;
use std::time::Duration;

#[test]
fn embedding_runtime_policy_has_finite_defaults() {
    let policy = EmbeddingRuntimePolicy::default();
    assert_eq!(policy.request_timeout(), DEFAULT_EMBED_REQUEST_TIMEOUT);
    assert_eq!(policy.batch_size(), DEFAULT_EMBED_BATCH_SIZE);
    assert_eq!(policy.worker_interval(), DEFAULT_EMBED_WORKER_INTERVAL);
    assert_eq!(
        policy.stale_claim_timeout(),
        DEFAULT_EMBED_STALE_CLAIM_TIMEOUT
    );
    assert_eq!(policy.claim_heartbeat_interval(), Duration::from_mins(5));
}

#[test]
fn embedding_runtime_policy_parses_canonical_env_block() {
    let policy = EmbeddingRuntimePolicy::from_lookup(&lookup(&[
        (PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS, "30"),
        (PROXIIMA_EMBED_BATCH_SIZE, "17"),
        (PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS, "9"),
        (PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS, "91"),
    ]))
    .expect("valid policy");
    assert_eq!(policy.request_timeout(), Duration::from_secs(30));
    assert_eq!(policy.batch_size(), 17);
    assert_eq!(policy.worker_interval(), Duration::from_secs(9));
    assert_eq!(policy.stale_claim_timeout(), Duration::from_secs(91));
}

#[test]
fn drain_concurrency_defaults_to_one_batch_and_sizes_the_drain_pass() {
    let policy = EmbeddingRuntimePolicy::default();
    assert_eq!(policy.drain_concurrency(), 1);
    assert_eq!(policy.drain_pass_size(), DEFAULT_EMBED_BATCH_SIZE);
    let policy = EmbeddingRuntimePolicy::from_lookup(&lookup(&[
        (PROXIIMA_EMBED_BATCH_SIZE, "8"),
        (PROXIIMA_EMBED_DRAIN_CONCURRENCY, "4"),
    ]))
    .expect("valid policy");
    assert_eq!(policy.drain_concurrency(), 4);
    assert_eq!(policy.drain_pass_size(), 32);
}

#[test]
fn embedding_runtime_policy_rejects_bad_values_and_unsafe_relation() {
    let duration = |field, max, actual| EmbeddingRuntimePolicyError::DurationOutOfRange {
        field,
        min: 1,
        max,
        actual,
    };
    for (key, value, expected) in [
        (
            PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS,
            "0",
            duration(
                PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS,
                MAX_EMBED_REQUEST_TIMEOUT.as_secs(),
                0,
            ),
        ),
        (
            PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS,
            "3601",
            duration(
                PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS,
                MAX_EMBED_REQUEST_TIMEOUT.as_secs(),
                3_601,
            ),
        ),
        (
            PROXIIMA_EMBED_BATCH_SIZE,
            "0",
            EmbeddingRuntimePolicyError::BatchSizeOutOfRange {
                max: MAX_EMBED_BATCH_SIZE,
                actual: 0,
            },
        ),
        (
            PROXIIMA_EMBED_BATCH_SIZE,
            "1025",
            EmbeddingRuntimePolicyError::BatchSizeOutOfRange {
                max: MAX_EMBED_BATCH_SIZE,
                actual: 1_025,
            },
        ),
        (
            PROXIIMA_EMBED_DRAIN_CONCURRENCY,
            "0",
            EmbeddingRuntimePolicyError::DrainConcurrencyOutOfRange {
                max: MAX_EMBED_DRAIN_CONCURRENCY,
                actual: 0,
            },
        ),
        (
            PROXIIMA_EMBED_DRAIN_CONCURRENCY,
            "65",
            EmbeddingRuntimePolicyError::DrainConcurrencyOutOfRange {
                max: MAX_EMBED_DRAIN_CONCURRENCY,
                actual: 65,
            },
        ),
        (
            PROXIIMA_EMBED_DRAIN_CONCURRENCY,
            "many",
            EmbeddingRuntimePolicyError::MalformedCount {
                key: PROXIIMA_EMBED_DRAIN_CONCURRENCY,
                value: "many".into(),
            },
        ),
        (
            PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS,
            "nope",
            EmbeddingRuntimePolicyError::MalformedSeconds {
                key: PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS,
                value: "nope".into(),
            },
        ),
        (
            PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS,
            "86401",
            duration(
                PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS,
                MAX_EMBED_STALE_CLAIM_TIMEOUT.as_secs(),
                86_401,
            ),
        ),
    ] {
        let err = EmbeddingRuntimePolicy::from_lookup(&lookup(&[(key, value)]))
            .expect_err("invalid setting must fail");
        assert_eq!(err, expected, "{key}={value}");
    }

    let err = EmbeddingRuntimePolicy::from_lookup(&lookup(&[
        (PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS, "120"),
        (PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS, "120"),
    ]))
    .expect_err("stale claim must outlive one request");
    assert_eq!(
        err,
        EmbeddingRuntimePolicyError::StaleClaimNotLongerThanRequest {
            stale_seconds: 120,
            request_seconds: 120,
        },
        "{err:?}"
    );
}

#[test]
fn embedding_runtime_policy_rejects_fractional_programmatic_durations() {
    for (field, result) in [
        (
            PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS,
            EmbeddingRuntimePolicy::new(
                Duration::from_millis(1_500),
                2,
                Duration::from_secs(1),
                Duration::from_secs(3),
            ),
        ),
        (
            PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS,
            EmbeddingRuntimePolicy::new(
                Duration::from_secs(1),
                2,
                Duration::from_millis(1_500),
                Duration::from_secs(3),
            ),
        ),
        (
            PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS,
            EmbeddingRuntimePolicy::new(
                Duration::from_secs(1),
                2,
                Duration::from_secs(1),
                Duration::from_millis(3_500),
            ),
        ),
    ] {
        assert!(
            matches!(
                result,
                Err(EmbeddingRuntimePolicyError::NonIntegralSeconds { field: actual })
                    if actual == field
            ),
            "fractional {field} must be rejected"
        );
    }
}

#[derive(Debug)]
struct HangingEmbedding;

#[async_trait]
impl EmbeddingClient for HangingEmbedding {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
        std::future::pending().await
    }

    async fn embed_many(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
        std::future::pending().await
    }

    fn model_id(&self) -> &'static str {
        "hanging"
    }

    fn dim(&self) -> usize {
        4
    }
}

#[tokio::test(start_paused = true)]
async fn request_timeout_bounds_single_and_batch_custom_client_calls() {
    let timeout = Duration::from_secs(1);
    for result in [
        embed_with_timeout(&HangingEmbedding, "one", timeout)
            .await
            .map(|_| ()),
        embed_many_with_timeout(&HangingEmbedding, &["one".to_owned()], timeout)
            .await
            .map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(LlmError::Retryable { ref message, .. }) if message.contains("timed out")),
            "timeout must be a retryable embedding error: {result:?}"
        );
    }
}

#[derive(Debug)]
struct RescueThenHang {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl EmbeddingClient for RescueThenHang {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Err(LlmError::InputRefused(
                EmbeddingInputRefusal::PayloadTooLarge,
            ));
        }
        std::future::pending().await
    }

    fn model_id(&self) -> &'static str {
        "rescue-then-hang"
    }

    fn dim(&self) -> usize {
        4
    }
}

#[tokio::test(start_paused = true)]
async fn request_timeout_bounds_each_chunk_rescue_provider_call() {
    let client = RescueThenHang {
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let result = embed_in_chunks_with_timeout(
        &client,
        &"a".repeat(CHUNKED_EMBED_MIN_BYTES * 2),
        Duration::from_secs(1),
    )
    .await;
    assert!(
        matches!(result, Err(LlmError::Retryable { ref message, .. }) if message.contains("timed out")),
        "a hung chunk request must be retryable: {result:?}"
    );
}

/// A provider that refuses over-cap input the way a client-side
/// `max_input_chars` does — and records every length it was offered, so a
/// test can assert what was *sent*, not merely what came back.
#[derive(Debug)]
struct CappedEmbedding {
    max_chars: usize,
    offered: Mutex<Vec<usize>>,
}

impl CappedEmbedding {
    fn new(max_chars: usize) -> Self {
        Self {
            max_chars,
            offered: Mutex::new(Vec::new()),
        }
    }

    fn accepted(&self) -> Vec<usize> {
        let offered = self
            .offered
            .lock()
            .expect("no test holds this across a panic");
        offered
            .iter()
            .copied()
            .filter(|chars| *chars <= self.max_chars)
            .collect()
    }
}

#[async_trait]
impl EmbeddingClient for CappedEmbedding {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        let chars = text.chars().count();
        self.offered
            .lock()
            .expect("no test holds this across a panic")
            .push(chars);
        if chars > self.max_chars {
            return Err(LlmError::InputRefused(EmbeddingInputRefusal::LocalCap {
                actual_chars: chars,
                max_chars: self.max_chars,
            }));
        }
        Ok(vec![0.0; 4])
    }

    fn model_id(&self) -> &'static str {
        "capped"
    }

    fn dim(&self) -> usize {
        4
    }
}

/// The cap and the rescue have to compose, because the cap's refusal is
/// what triggers the rescue. Everything over the limit comes back split,
/// and every piece actually embedded is within it.
#[tokio::test]
async fn a_client_side_cap_is_rescued_into_chunks() {
    let client = CappedEmbedding::new(MIN_EMBED_INPUT_CAP_CHARS);
    let text = "a".repeat(MIN_EMBED_INPUT_CAP_CHARS * 5);

    let ChunkedEmbedding::Embedded(vectors) = embed_in_chunks(&client, &text)
        .await
        .expect("a cap refusal is not a transient error")
    else {
        panic!("an over-cap input is splittable, not invalid");
    };

    assert!(vectors.len() > 1, "an over-cap input must come back split");
    assert_eq!(
        vectors.len(),
        client.accepted().len(),
        "one vector per piece the provider accepted",
    );
    for chars in client.accepted() {
        assert!(
            chars <= MIN_EMBED_INPUT_CAP_CHARS,
            "a piece of {chars} chars was embedded above the cap",
        );
    }
}

#[derive(Debug)]
struct ScriptedEmbedding {
    replies: Mutex<std::collections::VecDeque<Result<Vec<f32>, LlmError>>>,
    offered: Mutex<Vec<String>>,
}

impl ScriptedEmbedding {
    fn new(replies: impl IntoIterator<Item = Result<Vec<f32>, LlmError>>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            offered: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl EmbeddingClient for ScriptedEmbedding {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.offered.lock().expect("offered").push(text.into());
        if text == "ok" {
            return Ok(vec![0.0; 4]);
        }
        self.replies
            .lock()
            .expect("replies")
            .pop_front()
            .expect("unexpected provider request")
    }
    fn model_id(&self) -> &'static str {
        "scripted"
    }
    fn dim(&self) -> usize {
        4
    }
}

#[tokio::test]
async fn non_input_refusals_never_probe_or_split_even_when_a_probe_would_succeed() {
    for length in [10, MIN_EMBED_INPUT_CAP_CHARS * 5] {
        let errors = [
            LlmError::retryable("network unavailable"),
            LlmError::Internal("client fault".into()),
            LlmError::Configuration(EmbeddingConfigurationFault::BadRequest),
            LlmError::Configuration(EmbeddingConfigurationFault::Unauthorized),
            LlmError::Configuration(EmbeddingConfigurationFault::Forbidden),
            LlmError::Configuration(EmbeddingConfigurationFault::NotFound),
        ]
        .into_iter()
        .chain(
            [408, 429, 500, 502, 503, 504].map(|status| LlmError::Retryable {
                status: Some(status),
                retry_after: None,
                message: "provider failure".into(),
            }),
        );
        for error in errors {
            let expected = error.to_string();
            let client = ScriptedEmbedding::new([Err(error)]);
            let text = "a".repeat(length);
            let result = embed_in_chunks(&client, &text)
                .await
                .expect_err("non-input failure");
            assert_eq!(
                result.to_string(),
                expected,
                "preserve original non-input failure"
            );
            assert_eq!(
                *client.offered.lock().expect("offered"),
                [text],
                "no probe or rescue"
            );
        }
    }
}

#[tokio::test]
async fn later_non_input_failure_discards_partial_chunk_coverage() {
    for error in [
        LlmError::Retryable {
            status: Some(429),
            retry_after: Some(Duration::from_secs(90)),
            message: "busy".into(),
        },
        LlmError::Configuration(EmbeddingConfigurationFault::Unauthorized),
        LlmError::retryable("network unavailable"),
    ] {
        let expected = error.to_string();
        let client = ScriptedEmbedding::new([Ok(vec![1.0; 4]), Err(error)]);
        let text = "a".repeat(CHUNKED_EMBED_MIN_BYTES * 2);
        let result =
            embed_in_chunks_after_failure(&client, &text, EmbeddingInputRefusal::PayloadTooLarge)
                .await
                .expect_err("partial chunks must not escape");
        assert_eq!(result.to_string(), expected);
        assert_eq!(
            client.offered.lock().expect("offered").len(),
            2,
            "no full-text replay or probe"
        );
    }
}

#[tokio::test]
async fn chunk_rescue_preserves_utf8_order_and_final_refusing_status() {
    let text = format!(
        "{}{}",
        "é".repeat(CHUNKED_EMBED_MIN_BYTES / 2),
        "界".repeat(CHUNKED_EMBED_MIN_BYTES)
    );
    let client = ScriptedEmbedding::new([Ok(vec![1.0; 4]), Ok(vec![2.0; 4])]);
    let coverage =
        embed_in_chunks_after_failure(&client, &text, EmbeddingInputRefusal::PayloadTooLarge)
            .await
            .expect("UTF-8 rescue");
    assert_eq!(
        coverage,
        ChunkedEmbedding::Embedded(vec![vec![1.0; 4], vec![2.0; 4]])
    );
    assert_eq!(
        client.offered.lock().expect("offered").concat(),
        text,
        "left-to-right complete coverage"
    );

    let client = ScriptedEmbedding::new([Err(LlmError::InputRefused(
        EmbeddingInputRefusal::UnprocessableContent,
    ))]);
    let coverage = embed_in_chunks_after_failure(
        &client,
        &"a".repeat(CHUNKED_EMBED_MIN_BYTES * 2),
        EmbeddingInputRefusal::PayloadTooLarge,
    )
    .await
    .expect("input refusal");
    assert_eq!(
        coverage,
        ChunkedEmbedding::InputRefused(EmbeddingInputRefusal::UnprocessableContent)
    );
    assert_eq!(client.offered.lock().expect("offered").len(), 1);

    let client = ScriptedEmbedding::new([]);
    let coverage = embed_in_chunks_after_failure_with_timeout(
        &client,
        "short",
        EmbeddingInputRefusal::PayloadTooLarge,
        Duration::from_secs(1),
    )
    .await
    .expect("known refusal requires no provider call below split floor");
    assert_eq!(
        coverage,
        ChunkedEmbedding::InputRefused(EmbeddingInputRefusal::PayloadTooLarge)
    );
    assert!(client.offered.lock().expect("offered").is_empty());
}

/// Floor is a real bound: rescued at the floor, terminal one char below.
/// Input length must be `2 * MIN_EMBED_INPUT_CAP_CHARS` so halves land
/// on the widest piece the split can emit.
#[tokio::test]
async fn one_char_under_the_floor_is_the_difference_between_split_and_terminal() {
    // Halves to exactly MIN_EMBED_INPUT_CAP_CHARS, the widest piece the
    // bisection can hand the provider.
    let text = "a".repeat(2 * MIN_EMBED_INPUT_CAP_CHARS);

    let at_floor = CappedEmbedding::new(MIN_EMBED_INPUT_CAP_CHARS);
    assert_eq!(
        embed_in_chunks(&at_floor, &text)
            .await
            .expect("not a transient error"),
        ChunkedEmbedding::Embedded(vec![vec![0.0; 4]; 2]),
        "at the floor the widest piece is acceptable, so the rescue finishes",
    );

    let under_floor = CappedEmbedding::new(MIN_EMBED_INPUT_CAP_CHARS - 1);
    assert!(
        matches!(
            embed_in_chunks(&under_floor, &text)
                .await
                .expect("not a transient error"),
            ChunkedEmbedding::InputRefused(_)
        ),
        "one char lower, that piece is refused and is too small to split \
             again, so a rescuable input goes terminal — which is why a cap \
             under MIN_EMBED_INPUT_CAP_CHARS is refused at construction",
    );
}

/// Smallest cap that works for every length: the test above builds its
/// input from the constant, so it would pass a range of wrong values.
#[tokio::test]
async fn the_floor_is_the_smallest_cap_that_works_for_every_input() {
    async fn rescues_every_length(cap: usize) -> bool {
        // Every length whose halves can straddle the split minimum.
        for len in CHUNKED_EMBED_MIN_BYTES..=(4 * CHUNKED_EMBED_MIN_BYTES) {
            let client = CappedEmbedding::new(cap);
            let rescued = embed_in_chunks(&client, &"a".repeat(len))
                .await
                .expect("not a transient error");
            if matches!(rescued, ChunkedEmbedding::InputRefused(_)) {
                return false;
            }
        }
        true
    }

    assert!(
        rescues_every_length(MIN_EMBED_INPUT_CAP_CHARS).await,
        "the floor must leave no input un-rescuable",
    );
    assert!(
        !rescues_every_length(MIN_EMBED_INPUT_CAP_CHARS - 1).await,
        "one char below the floor some input must fail, or the floor is \
             higher than it needs to be and refuses workable configurations",
    );
}

/// A cap is not the only typed input refusal. Invalid input
/// that is invalid rather than long is refused all the way down, and the
/// caller must see that as "no rescue" so the job goes terminal instead
/// of cycling — the behaviour the cap must not change.
#[tokio::test]
async fn input_refused_at_every_length_is_not_rescued() {
    let client = CappedEmbedding::new(0);
    let outcome = embed_in_chunks(&client, &"a".repeat(MIN_EMBED_INPUT_CAP_CHARS * 4))
        .await
        .expect("not a transient error");
    assert!(
        matches!(
            outcome,
            ChunkedEmbedding::InputRefused(EmbeddingInputRefusal::LocalCap { max_chars: 0, .. })
        ),
        "nothing was acceptable at any length"
    );
}
