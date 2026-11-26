use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures::FutureExt;
use proxiima_core::PlaintextHosts;
use proxiima_core::embedding::{
    EMBEDDING_PURPOSE_HEADER, EMBEDDING_SEARCH_PURPOSE, EmbeddingClient,
    EmbeddingConfigurationFault, EmbeddingInputRefusal, LlmError, MIN_EMBED_INPUT_CAP_CHARS,
};
use proxiima_core::models::EmbedCaps;
use serde::{Deserialize, Serialize};

use crate::{build_client, embedding_endpoint};

// =====================================================================
// OpenAI-compatible embedding client — /embeddings
// =====================================================================

/// Default per-request timeout for `/embeddings` calls. Deliberately far
/// shorter than a text-generation timeout: a single embedding is a small,
/// fast request, and the in-process serial drainer must not stall for
/// minutes on one wedged call. Override with
/// [`OpenAiCompatConfig::with_timeout`] for unusually slow local models.
pub const DEFAULT_EMBED_TIMEOUT: Duration = proxiima_core::embedding::DEFAULT_EMBED_REQUEST_TIMEOUT;

const RESPONSE_ENVELOPE_BYTES: usize = 64 * 1024;
const BYTES_PER_FLOAT_HEADROOM: usize = 32;
const ERROR_BODY_PREFIX_BYTES: usize =
    proxiima_core::ledger!(Proxiima.Embeddings.errorBodyPrefixBytes);

#[derive(Clone)]
pub struct OpenAiCompatConfig {
    /// Provider base URL. `/embeddings` is appended to its path; query
    /// parameters are preserved. Fragments are rejected at construction.
    pub base_url: String,
    pub timeout: Duration,
    pub bearer_token: Option<String>,
    /// Hosts, besides loopback, that `base_url` may name over plaintext
    /// `http://`. Redirects are refused. Empty by default; a host reads it from
    /// `PROXIIMA_PLAINTEXT_HOSTS` with [`PlaintextHosts::from_lookup`].
    pub plaintext_hosts: PlaintextHosts,
}

// Manual Debug: the bearer token is a secret and must never surface in logs
// or panic messages. Renders `Some("<redacted>")` / `None`; `base_url` and
// `timeout` stay visible for diagnostics. `OpenAiCompatEmbeddingClient` derives
// Debug and holds this config, so it inherits the redaction.
impl std::fmt::Debug for OpenAiCompatConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatConfig")
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .field("plaintext_hosts", &self.plaintext_hosts)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl OpenAiCompatConfig {
    #[must_use]
    pub fn new(base_url: impl Into<String>, bearer_token: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            timeout: DEFAULT_EMBED_TIMEOUT,
            bearer_token,
            plaintext_hosts: PlaintextHosts::none(),
        }
    }

    /// Name the hosts, besides loopback, that may use plaintext `http://`.
    /// Defaults to none.
    #[must_use]
    pub fn with_plaintext_hosts(mut self, plaintext_hosts: PlaintextHosts) -> Self {
        self.plaintext_hosts = plaintext_hosts;
        self
    }

    /// Override the per-request timeout (e.g. bump it for an unusually slow
    /// local embedding model). Defaults to [`DEFAULT_EMBED_TIMEOUT`].
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatEmbeddingClient {
    config: OpenAiCompatConfig,
    client: reqwest::Client,
    endpoint: reqwest::Url,
    model_id: String,
    caps: EmbedCaps,
}

impl OpenAiCompatEmbeddingClient {
    /// Construct an OpenAI-compatible embedding client. Matryoshka caps
    /// drive a `dimensions` parameter on the request so nested-prefix
    /// models (qwen3-embedding, text-embedding-3-*) return vectors at
    /// `caps.dim` rather than the model's native size.
    ///
    /// # Errors
    /// Returns `LlmError::Internal` if the HTTP client cannot be built, if
    /// `config.base_url` is invalid, contains a fragment, or is a plaintext
    /// `http://` endpoint on a host that is neither loopback nor named in
    /// `config.plaintext_hosts` (which would leak the bearer token in
    /// transit), if `config.timeout` is zero, if `config.bearer_token` is
    /// blank, or if
    /// [`EmbedCaps::max_input_chars`] is set below
    /// [`proxiima_core::embedding::MIN_EMBED_INPUT_CAP_CHARS`].
    pub fn new(
        model_id: impl Into<String>,
        caps: EmbedCaps,
        config: OpenAiCompatConfig,
    ) -> Result<Self, LlmError> {
        let endpoint = embedding_endpoint(&config.base_url, &config.plaintext_hosts)?;
        if config
            .bearer_token
            .as_deref()
            .is_some_and(|token| token.trim().is_empty())
        {
            return Err(LlmError::Internal(
                "embedding bearer token must not be blank; pass None for no token".into(),
            ));
        }
        if config.timeout.is_zero() {
            return Err(LlmError::Internal(
                "embedding request timeout must be greater than zero".into(),
            ));
        }
        // Rejected at construction rather than tolerated at call time: a cap
        // under the floor makes over-long input terminal instead of chunked,
        // and it does so invisibly — every component behaves as documented
        // and the memory is simply never embedded. Refusing to boot names the
        // misconfiguration while someone is still looking at it.
        if let Some(max) = caps.max_input_chars {
            let floor = MIN_EMBED_INPUT_CAP_CHARS;
            if (max.get() as usize) < floor {
                return Err(LlmError::Internal(format!(
                    "max_input_chars is {max}, below the {floor}-char floor the chunked-embedding \
                     rescue can satisfy; a longer input would go terminal instead of being split",
                )));
            }
        }
        let client = build_client(config.timeout)?;
        Ok(Self {
            config,
            client,
            endpoint,
            model_id: model_id.into(),
            caps,
        })
    }
}

#[derive(Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingDatum>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingDatum {
    #[serde(default)]
    index: Option<usize>,
    embedding: Vec<f32>,
}

impl OpenAiEmbeddingResponse {
    fn into_embeddings(
        self,
        expected_count: usize,
        caps: EmbedCaps,
    ) -> Result<Vec<Vec<f32>>, LlmError> {
        if self.data.len() != expected_count {
            return Err(LlmError::retryable(format!(
                "requested {} embeddings, response carried {}",
                expected_count,
                self.data.len()
            )));
        }
        let mut data = self.data;
        // Index-free compatible providers retain array order. Once any
        // index is supplied, require a complete permutation of the inputs:
        // sorting alone would silently accept duplicates or missing indices.
        if data.iter().any(|d| d.index.is_some()) {
            data.sort_by_key(|d| d.index);
            for (expected_index, datum) in data.iter().enumerate() {
                if datum.index != Some(expected_index) {
                    return Err(LlmError::retryable(format!(
                        "invalid embedding response index {:?}; expected {expected_index} after \
                         sorting a complete set of {expected_count} input indices",
                        datum.index,
                    )));
                }
            }
        }

        let expected = caps.dim as usize;
        // Preserve shape-error precedence across the entire batch before
        // examining components. Valid JSON numbers can overflow f32 while
        // deserializing, even though NaN/Infinity are not JSON number tokens.
        for datum in &data {
            if datum.embedding.len() != expected {
                return Err(LlmError::retryable(format!(
                    "expected dim {} (matryoshka={}), got {}",
                    expected,
                    caps.matryoshka,
                    datum.embedding.len()
                )));
            }
        }
        data.into_iter()
            .enumerate()
            .map(|(embedding_index, datum)| {
                if let Some(component_index) = datum.embedding.iter().position(|value| !value.is_finite()) {
                    Err(LlmError::retryable(format!(
                        "embedding {embedding_index} has non-finite component at position {component_index}"
                    )))
                } else {
                    Ok(datum.embedding)
                }
            })
            .collect()
    }
}

/// How a call reads a refusing status, independently of its body.
enum StatusReading {
    /// Refusal whose retryable form carries a bounded response-body diagnostic.
    Diagnose(LlmError),
    /// Refusal whose response body must not be echoed (for example, a search
    /// query HTTP 400 whose body may repeat the rejected query text).
    Failed(LlmError),
    /// An indexing call's HTTP 400. OpenAI-compatible providers answer an
    /// input over the model's context with it as well as a request the
    /// provider cannot serve, so one sentinel probe tells them apart
    /// ([`read_bad_request_probe`]).
    Probe,
}

// `Proxiima.Embeddings.providerFailure`.
fn read_status(
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
    search: bool,
) -> StatusReading {
    if status == reqwest::StatusCode::BAD_REQUEST {
        if !search {
            return StatusReading::Probe;
        }
        return StatusReading::Failed(LlmError::Retryable {
            status: Some(status.as_u16()),
            retry_after,
            message: format!("openai-compatible /embeddings returned {status}"),
        });
    }
    StatusReading::Diagnose(embed_http_error(status, retry_after, ""))
}

/// What a status means alone. A search query keeps an HTTP 400 transient:
/// it has no input to rescue and a second request would delay the search.
fn embed_http_error(
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
    body: &str,
) -> LlmError {
    match status.as_u16() {
        413 => LlmError::InputRefused(EmbeddingInputRefusal::PayloadTooLarge),
        422 => LlmError::InputRefused(EmbeddingInputRefusal::UnprocessableContent),
        401 => LlmError::Configuration(EmbeddingConfigurationFault::Unauthorized),
        403 => LlmError::Configuration(EmbeddingConfigurationFault::Forbidden),
        404 => LlmError::Configuration(EmbeddingConfigurationFault::NotFound),
        _ => LlmError::Retryable {
            status: Some(status.as_u16()),
            retry_after,
            message: format!("openai-compatible /embeddings returned {status}: {body}"),
        },
    }
}

// `Proxiima.Embeddings.badRequestProbe`.
fn read_bad_request_probe(probe: Result<Vec<Vec<f32>>, SendFailure>) -> LlmError {
    let refusal = match probe {
        Ok(_) => return LlmError::InputRefused(EmbeddingInputRefusal::BadRequest),
        Err(SendFailure::Unusable(error)) => return error,
        Err(SendFailure::Refused(refusal)) => refusal,
    };
    let fault = match refusal.status.as_u16() {
        400 => EmbeddingConfigurationFault::BadRequest,
        401 => EmbeddingConfigurationFault::Unauthorized,
        403 => EmbeddingConfigurationFault::Forbidden,
        404 => EmbeddingConfigurationFault::NotFound,
        _ => {
            let error = LlmError::Retryable {
                status: Some(refusal.status.as_u16()),
                retry_after: refusal.retry_after,
                message: format!(
                    "openai-compatible /embeddings refused the input with 400 Bad Request, \
                     then returned {} for a sentinel input: ",
                    refusal.status
                ),
            };
            return refusal.diagnose(error);
        }
    };
    LlmError::Configuration(fault)
}

/// A one-character input any embedding model accepts, other than the
/// refused input.
fn sentinel(inputs: &[&str]) -> &'static str {
    if inputs == ["a"] { "b" } else { "a" }
}

/// Why one request returned no vectors.
enum SendFailure {
    /// The provider answered with a status other than success. Boxed: the
    /// response it holds would size every result.
    Refused(Box<Refusal>),
    /// Transport, timeout or a malformed success: retryable.
    Unusable(LlmError),
}

/// A refusing answer. Its error body is read only as a diagnostic.
struct Refusal {
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
    response: reqwest::Response,
}

impl Refusal {
    /// Append a bounded diagnostic to a retryable failure. Diagnostics cannot
    /// delay an already known provider refusal, and a redirect's body is
    /// never read.
    fn diagnose(mut self, mut error: LlmError) -> LlmError {
        let LlmError::Retryable { message, .. } = &mut error else {
            return error;
        };
        if self.status.is_redirection() {
            message.push_str("HTTP redirect refused: embedding client does not follow redirects");
            return error;
        }
        match read_error_prefix(&mut self.response).now_or_never() {
            Some(Ok(prefix)) => message.push_str(utf8_prefix(&prefix)),
            Some(Err(read_error)) => {
                write!(message, "HTTP body read: {}", read_error.without_url())
                    .expect("write to String is infallible");
            }
            None => {}
        }
        error
    }
}

fn retry_after_delay(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if value.is_empty() {
        return None;
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(
            value
                .parse::<u64>()
                .map_or(Duration::MAX, Duration::from_secs),
        );
    }
    let deadline = httpdate::parse_http_date(value).ok()?;
    Some(deadline.duration_since(now).unwrap_or(Duration::ZERO))
}

fn response_body_limit(batch_size: usize, dimensions: u32) -> usize {
    RESPONSE_ENVELOPE_BYTES.saturating_add(
        batch_size
            .saturating_mul(dimensions as usize)
            .saturating_mul(BYTES_PER_FLOAT_HEADROOM),
    )
}

async fn read_body(response: &mut reqwest::Response, limit: usize) -> Result<Vec<u8>, LlmError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| LlmError::retryable(format!("HTTP body read: {}", error.without_url())))?
    {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(LlmError::retryable(format!(
                "HTTP response body exceeded the {limit}-byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn read_error_prefix(response: &mut reqwest::Response) -> Result<Vec<u8>, reqwest::Error> {
    let mut prefix = Vec::new();
    while prefix.len() < ERROR_BODY_PREFIX_BYTES {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let remaining = ERROR_BODY_PREFIX_BYTES - prefix.len();
        prefix.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(prefix)
}

fn utf8_prefix(bytes: &[u8]) -> &str {
    let bytes = &bytes[..bytes.len().min(ERROR_BODY_PREFIX_BYTES)];
    match std::str::from_utf8(bytes) {
        Ok(prefix) => prefix,
        Err(error) => std::str::from_utf8(&bytes[..error.valid_up_to()])
            .expect("valid_up_to identifies a valid UTF-8 prefix"),
    }
}

impl OpenAiCompatEmbeddingClient {
    /// The capabilities this client was built with.
    ///
    /// Public so a host can assert what it configured — an input cap that
    /// silently failed to be read looks identical, at runtime, to a provider
    /// that never sees a long input.
    #[must_use]
    pub fn caps(&self) -> EmbedCaps {
        self.caps
    }

    /// Complete-request timeout applied to the underlying HTTP client.
    #[must_use]
    pub const fn request_timeout(&self) -> Duration {
        self.config.timeout
    }

    /// Refuse over-cap input before sending it. The typed local refusal
    /// carries the actual length and configured bound into chunk rescue.
    fn refuse_over_cap(&self, inputs: &[&str]) -> Result<(), LlmError> {
        let Some(max) = self.caps.max_input_chars else {
            return Ok(());
        };
        let max = max.get() as usize;
        for input in inputs {
            let chars = input.chars().count();
            if chars > max {
                return Err(LlmError::InputRefused(EmbeddingInputRefusal::LocalCap {
                    actual_chars: chars,
                    max_chars: max,
                }));
            }
        }
        Ok(())
    }

    async fn embed_call(&self, inputs: &[&str], search: bool) -> Result<Vec<Vec<f32>>, LlmError> {
        self.refuse_over_cap(inputs)?;
        let started = tokio::time::Instant::now();
        let refusal = match self.send_once(inputs, search, started).await {
            Ok(vectors) => return Ok(vectors),
            Err(SendFailure::Unusable(error)) => return Err(error),
            Err(SendFailure::Refused(refusal)) => refusal,
        };
        match read_status(refusal.status, refusal.retry_after, search) {
            StatusReading::Diagnose(error) => Err(refusal.diagnose(error)),
            StatusReading::Failed(error) => Err(error),
            StatusReading::Probe => {
                // The probe shares the call's timeout, so a direct caller
                // waits no longer than for one request.
                let probe = self.send_once(&[sentinel(inputs)], false, started).await;
                Err(read_bad_request_probe(probe))
            }
        }
    }

    /// One `/embeddings` request, its success validated into one finite
    /// vector of the configured width per input.
    async fn send_once(
        &self,
        inputs: &[&str],
        search: bool,
        started: tokio::time::Instant,
    ) -> Result<Vec<Vec<f32>>, SendFailure> {
        let remaining = self.config.timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(SendFailure::Unusable(LlmError::retryable(format!(
                "request timed out after {} seconds",
                self.config.timeout.as_secs()
            ))));
        }
        let body = EmbedRequest {
            model: &self.model_id,
            input: inputs,
            dimensions: self.caps.matryoshka.then_some(self.caps.dim),
        };

        let mut req = self
            .client
            .post(self.endpoint.clone())
            .json(&body)
            .timeout(remaining);
        if search {
            req = req.header(EMBEDDING_PURPOSE_HEADER, EMBEDDING_SEARCH_PURPOSE);
        }
        if let Some(token) = &self.config.bearer_token {
            req = req.bearer_auth(token);
        }
        let mut resp = req.send().await.map_err(|error| {
            SendFailure::Unusable(LlmError::retryable(format!(
                "HTTP send: {}",
                error.without_url()
            )))
        })?;

        let status = resp.status();
        let retry_after = retry_after_delay(resp.headers(), SystemTime::now());
        if !status.is_success() {
            return Err(SendFailure::Refused(Box::new(Refusal {
                status,
                retry_after,
                response: resp,
            })));
        }

        let body_limit = response_body_limit(inputs.len(), self.caps.dim);
        // Leave headroom before the call deadline so an equal-timeout outer
        // wrapper (such as Engine's ClientControl) receives the metadata-bearing
        // error instead of canceling the future before with_http_metadata runs.
        let remaining_for_body = self.config.timeout.saturating_sub(started.elapsed());
        let headroom = (remaining_for_body / 4).min(Duration::from_millis(200));
        let body_timeout = remaining_for_body.saturating_sub(headroom);
        let body = tokio::time::timeout(body_timeout, read_body(&mut resp, body_limit))
            .await
            .unwrap_or_else(|_| {
                Err(LlmError::retryable(format!(
                    "HTTP response body timed out after {} seconds",
                    self.config.timeout.as_secs()
                )))
            })
            .map_err(|error| with_http_metadata(error, status, retry_after))?;
        let text_body = std::str::from_utf8(&body).map_err(|error| {
            with_http_metadata(
                LlmError::retryable(format!(
                    "decode OpenAI-compatible envelope as UTF-8: {error}"
                )),
                status,
                retry_after,
            )
        })?;
        let parsed: OpenAiEmbeddingResponse = serde_json::from_str(text_body).map_err(|e| {
            with_http_metadata(
                LlmError::retryable(format!(
                    "decode OpenAI-compatible envelope: {:?} at line {} column {}; body: {}",
                    e.classify(),
                    e.line(),
                    e.column(),
                    utf8_prefix(&body),
                )),
                status,
                retry_after,
            )
        })?;
        parsed
            .into_embeddings(inputs.len(), self.caps)
            .map_err(|error| with_http_metadata(error, status, retry_after))
    }
}

fn with_http_metadata(
    mut error: LlmError,
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
) -> SendFailure {
    if let LlmError::Retryable {
        status: slot_status,
        retry_after: slot_retry_after,
        ..
    } = &mut error
    {
        *slot_status = Some(status.as_u16());
        *slot_retry_after = retry_after;
    }
    SendFailure::Unusable(error)
}

#[async_trait]
impl EmbeddingClient for OpenAiCompatEmbeddingClient {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        let mut vecs = self.embed_call(&[text], false).await?;
        vecs.pop()
            .ok_or_else(|| LlmError::retryable("OpenAI-compatible response had no embeddings"))
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        let mut vecs = self.embed_call(&[text], true).await?;
        vecs.pop()
            .ok_or_else(|| LlmError::retryable("OpenAI-compatible response had no embeddings"))
    }

    async fn embed_many(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let inputs: Vec<&str> = texts.iter().map(String::as_str).collect();
        self.embed_call(&inputs, false).await
    }

    fn batches_requests(&self) -> bool {
        true
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.caps.dim as usize
    }
}

#[cfg(test)]
#[path = "openai_compat_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "openai_compat_redirect_tests.rs"]
mod redirect_tests;
