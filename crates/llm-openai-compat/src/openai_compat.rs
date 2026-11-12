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
const ERROR_BODY_PREFIX_BYTES: usize = 512;

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

/// Classify the provider status independently of its diagnostic text.
fn embed_http_error(
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
    body: &str,
) -> LlmError {
    match status.as_u16() {
        413 => LlmError::InputRefused(EmbeddingInputRefusal::PayloadTooLarge),
        422 => LlmError::InputRefused(EmbeddingInputRefusal::UnprocessableContent),
        400 => LlmError::Configuration(EmbeddingConfigurationFault::BadRequest),
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
        let body = EmbedRequest {
            model: &self.model_id,
            input: inputs,
            dimensions: self.caps.matryoshka.then_some(self.caps.dim),
        };

        let mut req = self.client.post(self.endpoint.clone()).json(&body);
        if search {
            req = req.header(EMBEDDING_PURPOSE_HEADER, EMBEDDING_SEARCH_PURPOSE);
        }
        if let Some(token) = &self.config.bearer_token {
            req = req.bearer_auth(token);
        }
        let mut resp = req
            .send()
            .await
            .map_err(|error| LlmError::retryable(format!("HTTP send: {}", error.without_url())))?;

        let status = resp.status();
        let retry_after = retry_after_delay(resp.headers(), SystemTime::now());
        if status.is_redirection() {
            return Err(embed_http_error(
                status,
                retry_after,
                "HTTP redirect refused: embedding client does not follow redirects",
            ));
        }
        if !status.is_success() {
            let mut error = embed_http_error(status, retry_after, "");
            if let LlmError::Retryable { message, .. } = &mut error {
                // Diagnostics cannot delay an already known provider refusal.
                match read_error_prefix(&mut resp).now_or_never() {
                    Some(Ok(prefix)) => message.push_str(utf8_prefix(&prefix)),
                    Some(Err(error)) => {
                        write!(message, "HTTP body read: {}", error.without_url())
                            .expect("write to String is infallible");
                    }
                    None => {}
                }
            }
            return Err(error);
        }

        let body_limit = response_body_limit(inputs.len(), self.caps.dim);
        let body = read_body(&mut resp, body_limit).await?;
        let text_body = std::str::from_utf8(&body).map_err(|error| {
            LlmError::retryable(format!(
                "decode OpenAI-compatible envelope as UTF-8: {error}"
            ))
        })?;
        let parsed: OpenAiEmbeddingResponse = serde_json::from_str(text_body).map_err(|e| {
            LlmError::retryable(format!(
                "decode OpenAI-compatible envelope: {:?} at line {} column {}; body: {}",
                e.classify(),
                e.line(),
                e.column(),
                utf8_prefix(&body),
            ))
        })?;
        parsed.into_embeddings(inputs.len(), self.caps)
    }
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
