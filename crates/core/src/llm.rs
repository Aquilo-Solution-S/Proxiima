//! Embedding-client contracts used by vector retrieval.

use async_trait::async_trait;
use std::time::Duration;

use crate::env::{
    PROXIIMA_EMBED_BATCH_SIZE, PROXIIMA_EMBED_DRAIN_CONCURRENCY,
    PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS, PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS,
    PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS, Setting,
};

mod query;
mod route;
mod space;

pub use query::{
    QUERY_PLACEHOLDER, QueryInstruction, QueryInstructionError, QueryInstructions, QueryTask,
    SemanticWeight, SemanticWeightError,
};
pub use route::{EmbeddingRoute, EmbeddingRouteError, EmbeddingRouter, SingleClientRouter};
pub use space::{
    BoundEmbeddingClient, EmbeddingDim, EmbeddingSpace, SpaceVector, UnsupportedEmbeddingWidth,
    VectorWidthMismatch,
};

/// A submitted input was refused; isolation and chunk rescue may still succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EmbeddingInputRefusal {
    #[error("HTTP 413: embedding input is too large")]
    PayloadTooLarge,
    #[error("HTTP 422: embedding input cannot be processed")]
    UnprocessableContent,
    #[error(
        "embedding input has {actual_chars} characters, exceeding the local cap of {max_chars}"
    )]
    LocalCap {
        actual_chars: usize,
        max_chars: usize,
    },
}

impl EmbeddingInputRefusal {
    /// The provider's refusing status, or no HTTP status for a local cap.
    #[must_use]
    pub const fn status(self) -> Option<u16> {
        match self {
            Self::PayloadTooLarge => Some(413),
            Self::UnprocessableContent => Some(422),
            Self::LocalCap { .. } => None,
        }
    }
}

/// A client configuration fault pauses that original client until rebinding.
///
/// `Display` is the bare reason, safe for health projection; `LlmError`
/// prefixes it with `embedding configuration fault: `.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, thiserror::Error)]
pub enum EmbeddingConfigurationFault {
    #[error("HTTP 400")]
    BadRequest,
    #[error("HTTP 401")]
    Unauthorized,
    #[error("HTTP 403")]
    Forbidden,
    #[error("HTTP 404")]
    NotFound,
    /// The client returned a vector of another width than its embedding space.
    #[error("vector width {received} where the embedding space has {expected}")]
    VectorWidth { expected: usize, received: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("{0}")]
    InputRefused(EmbeddingInputRefusal),
    #[error("embedding configuration fault: {0}")]
    Configuration(EmbeddingConfigurationFault),
    #[error("embedding call failed: {message}")]
    Retryable {
        status: Option<u16>,
        retry_after: Option<Duration>,
        message: String,
    },
    #[error("internal: {0}")]
    Internal(String),
}

impl LlmError {
    /// An ordinary non-HTTP failure with no provider retry timing.
    #[must_use]
    pub fn retryable(message: impl Into<String>) -> Self {
        Self::Retryable {
            status: None,
            retry_after: None,
            message: message.into(),
        }
    }
}

pub const DEFAULT_EMBED_REQUEST_TIMEOUT: Duration = Duration::from_mins(2);
pub const DEFAULT_EMBED_BATCH_SIZE: usize = 32;
pub const DEFAULT_EMBED_DRAIN_CONCURRENCY: usize = 1;
pub const DEFAULT_EMBED_WORKER_INTERVAL: Duration = Duration::from_secs(5);
pub const DEFAULT_EMBED_STALE_CLAIM_TIMEOUT: Duration = Duration::from_mins(15);

pub const MAX_EMBED_REQUEST_TIMEOUT: Duration = Duration::from_hours(1);
pub const MAX_EMBED_BATCH_SIZE: usize = 1_024;
pub const MAX_EMBED_DRAIN_CONCURRENCY: usize = 64;
pub const MAX_EMBED_WORKER_INTERVAL: Duration = Duration::from_hours(1);
pub const MAX_EMBED_STALE_CLAIM_TIMEOUT: Duration = Duration::from_hours(24);

/// Generic host policy for durable embedding work.
///
/// The engine and maintenance boundaries apply one request deadline to each
/// provider call. The shipped OpenAI-compatible adapter also applies its
/// timeout at the HTTP layer. Calls are not queued in the engine: a drain pass
/// sends up to `drain_concurrency` batches at once, a search query goes out
/// when it is asked, and the provider queues or refuses what exceeds its capacity.
/// Claims are renewed on a separate task every third of
/// `stale_claim_timeout`, so poison isolation and chunk rescue may make
/// several bounded provider calls without looking abandoned to a concurrent
/// reconciler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingRuntimePolicy {
    request_timeout: Duration,
    batch_size: usize,
    worker_interval: Duration,
    stale_claim_timeout: Duration,
    drain_concurrency: usize,
}

impl Default for EmbeddingRuntimePolicy {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_EMBED_REQUEST_TIMEOUT,
            batch_size: DEFAULT_EMBED_BATCH_SIZE,
            worker_interval: DEFAULT_EMBED_WORKER_INTERVAL,
            stale_claim_timeout: DEFAULT_EMBED_STALE_CLAIM_TIMEOUT,
            drain_concurrency: DEFAULT_EMBED_DRAIN_CONCURRENCY,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EmbeddingRuntimePolicyError {
    #[error("{key} must be integer seconds, got {value:?}")]
    MalformedSeconds { key: &'static str, value: String },
    #[error("{key} must be a positive integer, got {value:?}")]
    MalformedCount { key: &'static str, value: String },
    #[error("{field} must be in {min}..={max} seconds, got {actual}")]
    DurationOutOfRange {
        field: &'static str,
        min: u64,
        max: u64,
        actual: u64,
    },
    #[error("batch size must be in 1..={max}, got {actual}")]
    BatchSizeOutOfRange { max: usize, actual: usize },
    #[error("drain concurrency must be in 1..={max}, got {actual}")]
    DrainConcurrencyOutOfRange { max: usize, actual: usize },
    #[error("{field} must be an integral number of seconds")]
    NonIntegralSeconds { field: &'static str },
    #[error(
        "PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS ({stale_seconds}s) must be strictly greater than PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS ({request_seconds}s)"
    )]
    StaleClaimNotLongerThanRequest {
        stale_seconds: u64,
        request_seconds: u64,
    },
}

impl EmbeddingRuntimePolicy {
    /// Construct and validate a host embedding policy.
    ///
    /// # Errors
    ///
    /// Rejects zero, out-of-range, and unsafe stale-claim values.
    pub fn new(
        request_timeout: Duration,
        batch_size: usize,
        worker_interval: Duration,
        stale_claim_timeout: Duration,
    ) -> Result<Self, EmbeddingRuntimePolicyError> {
        validate_duration(
            &PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS,
            request_timeout,
            MAX_EMBED_REQUEST_TIMEOUT,
        )?;
        if !(1..=MAX_EMBED_BATCH_SIZE).contains(&batch_size) {
            return Err(EmbeddingRuntimePolicyError::BatchSizeOutOfRange {
                max: MAX_EMBED_BATCH_SIZE,
                actual: batch_size,
            });
        }
        validate_duration(
            &PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS,
            worker_interval,
            MAX_EMBED_WORKER_INTERVAL,
        )?;
        validate_duration(
            &PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS,
            stale_claim_timeout,
            MAX_EMBED_STALE_CLAIM_TIMEOUT,
        )?;
        if stale_claim_timeout <= request_timeout {
            return Err(
                EmbeddingRuntimePolicyError::StaleClaimNotLongerThanRequest {
                    stale_seconds: stale_claim_timeout.as_secs(),
                    request_seconds: request_timeout.as_secs(),
                },
            );
        }
        Ok(Self {
            request_timeout,
            batch_size,
            worker_interval,
            stale_claim_timeout,
            drain_concurrency: DEFAULT_EMBED_DRAIN_CONCURRENCY,
        })
    }

    /// Parse the canonical `PROXIIMA_EMBED_*` policy block through an injected
    /// lookup. Unset fields retain finite defaults; empty values are unset.
    ///
    /// # Errors
    ///
    /// Rejects malformed, zero, out-of-range, and unsafe combinations.
    pub fn from_lookup(
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Self, EmbeddingRuntimePolicyError> {
        let defaults = Self::default();
        let request_timeout = parse_duration_setting(
            lookup,
            &PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS,
            defaults.request_timeout,
        )?;
        let batch_size =
            parse_count_setting(lookup, &PROXIIMA_EMBED_BATCH_SIZE, defaults.batch_size)?;
        let drain_concurrency = parse_count_setting(
            lookup,
            &PROXIIMA_EMBED_DRAIN_CONCURRENCY,
            defaults.drain_concurrency,
        )?;
        let worker_interval = parse_duration_setting(
            lookup,
            &PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS,
            defaults.worker_interval,
        )?;
        let stale_claim_timeout = parse_duration_setting(
            lookup,
            &PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS,
            defaults.stale_claim_timeout,
        )?;
        Self::new(
            request_timeout,
            batch_size,
            worker_interval,
            stale_claim_timeout,
        )?
        .with_drain_concurrency(drain_concurrency)
    }

    /// How many claimed batches one drain pass sends to providers at once.
    /// Default 1: batches go one after another.
    ///
    /// # Errors
    ///
    /// Rejects a concurrency outside `1..=MAX_EMBED_DRAIN_CONCURRENCY`.
    pub const fn with_drain_concurrency(
        mut self,
        concurrency: usize,
    ) -> Result<Self, EmbeddingRuntimePolicyError> {
        if concurrency == 0 || concurrency > MAX_EMBED_DRAIN_CONCURRENCY {
            return Err(EmbeddingRuntimePolicyError::DrainConcurrencyOutOfRange {
                max: MAX_EMBED_DRAIN_CONCURRENCY,
                actual: concurrency,
            });
        }
        self.drain_concurrency = concurrency;
        Ok(self)
    }

    #[must_use]
    pub const fn drain_concurrency(self) -> usize {
        self.drain_concurrency
    }

    /// The jobs one drain pass claims: a batch for each concurrent slot.
    #[must_use]
    pub const fn drain_pass_size(self) -> usize {
        self.batch_size * self.drain_concurrency
    }

    #[must_use]
    pub const fn request_timeout(self) -> Duration {
        self.request_timeout
    }

    #[must_use]
    pub const fn batch_size(self) -> usize {
        self.batch_size
    }

    #[must_use]
    pub const fn worker_interval(self) -> Duration {
        self.worker_interval
    }

    #[must_use]
    pub const fn stale_claim_timeout(self) -> Duration {
        self.stale_claim_timeout
    }

    #[must_use]
    pub fn claim_heartbeat_interval(self) -> Duration {
        self.stale_claim_timeout / 3
    }

    #[must_use]
    pub fn stale_claim_timeout_seconds(self) -> i64 {
        i64::try_from(self.stale_claim_timeout.as_secs()).unwrap_or(i64::MAX)
    }
}

fn validate_duration(
    setting: &Setting,
    value: Duration,
    max: Duration,
) -> Result<(), EmbeddingRuntimePolicyError> {
    let field = setting.name();
    if value.subsec_nanos() != 0 {
        return Err(EmbeddingRuntimePolicyError::NonIntegralSeconds { field });
    }
    if value < Duration::from_secs(1) || value > max {
        return Err(EmbeddingRuntimePolicyError::DurationOutOfRange {
            field,
            min: 1,
            max: max.as_secs(),
            actual: value.as_secs(),
        });
    }
    Ok(())
}

fn parse_count_setting(
    lookup: &impl Fn(&str) -> Option<String>,
    setting: &Setting,
    default: usize,
) -> Result<usize, EmbeddingRuntimePolicyError> {
    let Some(raw) = setting.read(lookup) else {
        return Ok(default);
    };
    raw.parse::<usize>()
        .map_err(|_| EmbeddingRuntimePolicyError::MalformedCount {
            key: setting.name(),
            value: raw,
        })
}

fn parse_duration_setting(
    lookup: &impl Fn(&str) -> Option<String>,
    setting: &Setting,
    default: Duration,
) -> Result<Duration, EmbeddingRuntimePolicyError> {
    let Some(raw) = setting.read(lookup) else {
        return Ok(default);
    };
    let seconds =
        raw.parse::<u64>()
            .map_err(|_| EmbeddingRuntimePolicyError::MalformedSeconds {
                key: setting.name(),
                value: raw,
            })?;
    Ok(Duration::from_secs(seconds))
}

/// Header by which an embedding adapter identifies an explicit query-purpose request.
pub const EMBEDDING_PURPOSE_HEADER: &str = "x-proxiima-embedding-purpose";
/// Query-purpose value of [`EMBEDDING_PURPOSE_HEADER`].
pub const EMBEDDING_SEARCH_PURPOSE: &str = "search";

/// Embedding client surface. Concrete impls live outside core.
#[async_trait]
pub trait EmbeddingClient: Send + Sync + std::fmt::Debug {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError>;

    /// Embed an already-rendered search query. The engine sends every search
    /// query through this operation; providers may use the purpose for
    /// scheduling. Vector semantics remain the same as [`Self::embed`].
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.embed(text).await
    }

    /// Embed several texts in one provider call, one vector per input, in
    /// input order. The default falls back to sequential single embeds so
    /// existing impls stay correct; batching impls should override.
    async fn embed_many(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            out.push(self.embed(text).await?);
        }
        Ok(out)
    }

    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
}

/// Run one provider request under the host's generic request deadline.
///
/// Timeout is a retryable provider failure. This boundary is required even
/// for custom clients; adapter-native timeouts remain useful for cancelling
/// socket work promptly.
///
/// # Errors
///
/// Returns the client's error, or retryable [`LlmError::Retryable`] when the
/// deadline elapses.
pub async fn embed_with_timeout(
    client: &dyn EmbeddingClient,
    text: &str,
    request_timeout: Duration,
) -> Result<Vec<f32>, LlmError> {
    tokio::time::timeout(request_timeout, client.embed(text))
        .await
        .map_err(|_| {
            LlmError::retryable(format!(
                "request timed out after {} seconds",
                request_timeout.as_secs()
            ))
        })?
}

/// Run one batched provider request under the host's generic request
/// deadline. Timeout is retryable.
///
/// # Errors
///
/// Returns the client's error, or retryable [`LlmError::Retryable`] when the
/// deadline elapses.
pub async fn embed_many_with_timeout(
    client: &dyn EmbeddingClient,
    texts: &[String],
    request_timeout: Duration,
) -> Result<Vec<Vec<f32>>, LlmError> {
    tokio::time::timeout(request_timeout, client.embed_many(texts))
        .await
        .map_err(|_| {
            LlmError::retryable(format!(
                "request timed out after {} seconds",
                request_timeout.as_secs()
            ))
        })?
}

/// Smallest piece, in bytes, the chunked-embedding rescue can produce.
/// A rejected segment is split only if both halves reach this floor.
pub const CHUNKED_EMBED_MIN_BYTES: usize = 2048;

/// Lowest [`crate::models::EmbedCaps::max_input_chars`] [`embed_in_chunks`]
/// can satisfy. A cap below this floor can turn a rescuable input terminal.
///
/// Floor = largest piece the split can emit. A segment is cut only when
/// each half is still at least [`CHUNKED_EMBED_MIN_BYTES`], so a piece is at
/// most `2 * CHUNKED_EMBED_MIN_BYTES - 1` bytes. Characters never exceed bytes.
pub const MIN_EMBED_INPUT_CAP_CHARS: usize = 2 * CHUNKED_EMBED_MIN_BYTES - 1;

/// Complete chunk coverage or the final refusal at the minimum split size.
#[derive(Debug, PartialEq)]
pub enum ChunkedEmbedding {
    Embedded(Vec<Vec<f32>>),
    InputRefused(EmbeddingInputRefusal),
}

/// Bisect explicitly refused input into accepted pieces in text order.
/// Only [`LlmError::InputRefused`] enters rescue. Partial coverage is never
/// returned: a later non-input error discards earlier successful pieces.
///
/// # Errors
///
/// Returns the first provider failure that is not an input refusal.
pub async fn embed_in_chunks(
    client: &dyn EmbeddingClient,
    text: &str,
) -> Result<ChunkedEmbedding, LlmError> {
    embed_in_chunks_with(client, text, None, |client, segment| {
        Box::pin(client.embed(segment))
    })
    .await
}

/// [`embed_in_chunks`] with the deadline applied separately to every call.
///
/// # Errors
///
/// Returns the first non-input failure, including a request timeout.
pub async fn embed_in_chunks_with_timeout(
    client: &dyn EmbeddingClient,
    text: &str,
    request_timeout: Duration,
) -> Result<ChunkedEmbedding, LlmError> {
    embed_in_chunks_with(client, text, None, |client, segment| {
        Box::pin(embed_with_timeout(client, segment, request_timeout))
    })
    .await
}

/// Rescue a known input refusal without submitting the full input again.
///
/// A retryable failure cannot authorize input splitting.
///
/// ```compile_fail
/// # use proxiima_core::llm::{EmbeddingClient, LlmError, embed_in_chunks_after_failure};
/// # fn rescue(client: &dyn EmbeddingClient) {
/// let _ = embed_in_chunks_after_failure(client, "text", LlmError::retryable("busy"));
/// # }
/// ```
///
/// # Errors
///
/// Returns the first later provider failure that is not an input refusal.
pub async fn embed_in_chunks_after_failure(
    client: &dyn EmbeddingClient,
    text: &str,
    refusal: EmbeddingInputRefusal,
) -> Result<ChunkedEmbedding, LlmError> {
    embed_in_chunks_with(client, text, Some(refusal), |client, segment| {
        Box::pin(client.embed(segment))
    })
    .await
}

/// Timeout-bounded form of [`embed_in_chunks_after_failure`].
///
/// # Errors
///
/// Returns the first later non-input failure, including a request timeout.
pub async fn embed_in_chunks_after_failure_with_timeout(
    client: &dyn EmbeddingClient,
    text: &str,
    refusal: EmbeddingInputRefusal,
    request_timeout: Duration,
) -> Result<ChunkedEmbedding, LlmError> {
    embed_in_chunks_with(client, text, Some(refusal), |client, segment| {
        Box::pin(embed_with_timeout(client, segment, request_timeout))
    })
    .await
}

/// A LIFO stack takes the left half first, preserving text order.
fn push_left_half_first<'a>(pending: &mut Vec<&'a str>, left: &'a str, right: &'a str) {
    pending.push(right);
    pending.push(left);
}

async fn embed_in_chunks_with<'a, F>(
    client: &'a dyn EmbeddingClient,
    text: &'a str,
    initial_refusal: Option<EmbeddingInputRefusal>,
    mut embed: F,
) -> Result<ChunkedEmbedding, LlmError>
where
    F: for<'b> FnMut(
        &'b dyn EmbeddingClient,
        &'b str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<f32>, LlmError>> + Send + 'b>,
    >,
{
    let mut pending = Vec::new();
    let mut vectors = Vec::new();
    if let Some(refusal) = initial_refusal {
        let Some((left, right)) = split_segment(text) else {
            return Ok(ChunkedEmbedding::InputRefused(refusal));
        };
        push_left_half_first(&mut pending, left, right);
    } else {
        pending.push(text);
    }
    while let Some(segment) = pending.pop() {
        match embed(client, segment).await {
            Ok(vector) => vectors.push(vector),
            Err(LlmError::InputRefused(refusal)) => {
                let Some((left, right)) = split_segment(segment) else {
                    return Ok(ChunkedEmbedding::InputRefused(refusal));
                };
                push_left_half_first(&mut pending, left, right);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(ChunkedEmbedding::Embedded(vectors))
}

fn split_segment(segment: &str) -> Option<(&str, &str)> {
    let mut cut = segment.len() / 2;
    while cut > 0 && !segment.is_char_boundary(cut) {
        cut -= 1;
    }
    if cut < CHUNKED_EMBED_MIN_BYTES {
        return None;
    }
    Some((&segment[..cut], &segment[cut..]))
}

#[cfg(test)]
#[path = "llm_tests.rs"]
mod tests;
