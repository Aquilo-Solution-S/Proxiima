//! JWKS key resolution.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, AlgorithmFamily, DecodingKey};
use serde::Deserialize as _;
use tokio::sync::{Mutex, RwLock};

use crate::authenticator::verified_algorithms;
use crate::config::{OidcConfigError, validate_issuer_url, validate_jwks_url};

/// Minimum spacing between JWKS refetches. Bounds the outbound-fetch rate so a
/// flood of tokens carrying random unknown `kid`s cannot amplify into one
/// upstream JWKS request per inbound request.
const JWKS_REFRESH_COOLDOWN: Duration = Duration::from_mins(1);

/// Maximum age of a cached JWKS before a hit opportunistically refreshes. Picks
/// up key rotation even when every presented `kid` is already cached (so the
/// unknown-kid miss path never fires).
const JWKS_MAX_AGE: Duration = Duration::from_hours(1);

/// How long after its last successful fetch a key set is trusted. Between
/// [`JWKS_MAX_AGE`] and this bound a failed or throttled refresh still serves
/// the cached key, so a short issuer outage does not fail valid requests.
/// Past it the set resolves nothing until a fetch succeeds.
const JWKS_TRUST_BOUND: Duration = Duration::from_hours(4);

/// Complete-request timeout used by [`HttpJwksResolver::new`]. It covers DNS,
/// connection establishment, response headers, and response-body reads for
/// both discovery and JWKS requests.
pub const DEFAULT_HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest discovery or JWKS response body read. A real key set is a few
/// kilobytes; the cap bounds what an issuer can make the host buffer.
const MAX_RESPONSE_BYTES: usize = 1 << 20;

/// Largest complete-request timeout accepted by [`HttpJwksResolver`].
pub const MAX_HTTP_REQUEST_TIMEOUT: Duration = Duration::from_mins(5);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("unknown key id")]
    UnknownKid,
    #[error("jwks fetch failed: {0}")]
    Fetch(String),
    #[error("jwks parse failed: {0}")]
    Parse(String),
    #[error("jwks config invalid: {0}")]
    Config(String),
}

/// Resolves a signing key by `kid`.
#[async_trait]
pub trait KeyResolver: Send + Sync {
    /// # Errors
    ///
    /// Returns [`KeyError::UnknownKid`] when `kid` is unavailable, or a
    /// fetch/parse error when remote JWKS loading fails.
    async fn key_for(&self, kid: &str) -> Result<Arc<DecodingKey>, KeyError>;
}

/// In-memory resolver for pre-shared keys: tests, and a host verifying
/// against a JWKS pinned in its configuration ([`Self::from_jwks_json`]).
pub struct StaticJwksResolver {
    keys: HashMap<String, Arc<DecodingKey>>,
}

impl std::fmt::Debug for StaticJwksResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticJwksResolver")
            .field("kids", &self.keys.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl StaticJwksResolver {
    #[must_use]
    pub fn new(keys: HashMap<String, Arc<DecodingKey>>) -> Self {
        Self { keys }
    }

    /// Build a resolver from a JWKS document shipped in configuration, so a
    /// host with no network path to its issuer still verifies its tokens.
    ///
    /// Strict where [`HttpJwksResolver`] is tolerant. A fetched set is the
    /// issuer's to publish, and one unsupported key in it must not take down
    /// the others beside it. A pinned set is the operator's, and an entry this
    /// resolver skipped would be a key the operator believes is trusted and
    /// is not, so every entry must be a named verification key of one of
    /// three types: `kty: RSA` (RS256/RS384/RS512), `kty: EC` with
    /// `crv: P-256` (ES256), or `kty: OKP` with `crv: Ed25519` (`EdDSA`).
    ///
    /// # Errors
    ///
    /// [`KeyError::Parse`] when the document is not a JWKS.
    /// [`KeyError::Config`] when the set is empty, or an
    /// entry has no `kid`, repeats one, has another `kty`, lacks `n`/`e` (RSA)
    /// or has ones that do not decode,
    /// `crv`/`x`/`y` (EC) or `crv`/`x` (OKP), names another curve, has a
    /// coordinate that is not 32 bytes of base64url, names an `alg` that does
    /// not belong to its `kty` or a `use` other than `sig`, or carries the
    /// private part `d`.
    pub fn from_jwks_json(raw: &str) -> Result<Self, KeyError> {
        let set: JwkSet =
            serde_json::from_str(raw).map_err(|err| KeyError::Parse(err.to_string()))?;
        if set.keys.is_empty() {
            return Err(KeyError::Config("jwks contains no keys".into()));
        }
        let mut keys = HashMap::with_capacity(set.keys.len());
        for (index, entry) in set.keys.into_iter().enumerate() {
            let jwk = serde_json::from_value::<Jwk>(entry).map_err(|err| {
                KeyError::Config(format!("key {index} is not a well-formed JWK: {err}"))
            })?;
            let Some(kid) = jwk.kid.clone().filter(|kid| !kid.trim().is_empty()) else {
                return Err(KeyError::Config(format!("key {index} has no kid")));
            };
            let key = match jwk.verification_key() {
                Ok(key) => key,
                Err(reason) => {
                    return Err(KeyError::Config(format!("key {kid:?} {reason}")));
                }
            };
            if keys.contains_key(&kid) {
                return Err(KeyError::Config(format!("key {kid:?} appears twice")));
            }
            keys.insert(kid, Arc::new(key));
        }
        Ok(Self::new(keys))
    }
}

#[async_trait]
impl KeyResolver for StaticJwksResolver {
    async fn key_for(&self, kid: &str) -> Result<Arc<DecodingKey>, KeyError> {
        self.keys.get(kid).cloned().ok_or(KeyError::UnknownKid)
    }
}

#[derive(serde::Deserialize)]
struct OpenIdConfig {
    issuer: String,
    jwks_uri: String,
}

/// One JWKS entry, as published by an issuer or shipped in configuration.
/// Every member is optional so a refusal names what is missing instead of a
/// serde position.
#[derive(serde::Deserialize)]
struct Jwk {
    /// Optional in JWK sets; an unnamed key cannot satisfy a kid lookup.
    kid: Option<String>,
    /// Key type: `RSA`, `EC` or `OKP`. Anything else, or a missing one, is an
    /// unsupported entry (skipped when fetched, refused when pinned), so one
    /// such key in the set can't fail the whole JWKS parse.
    #[serde(default)]
    kty: String,
    /// RSA modulus / exponent. `Option` so an EC/OKP entry (which omits them)
    /// deserializes instead of failing the set.
    n: Option<String>,
    e: Option<String>,
    /// EC/OKP curve and public coordinates (base64url): `x` and `y` for EC,
    /// `x` alone for OKP.
    crv: Option<String>,
    x: Option<String>,
    y: Option<String>,
    alg: Option<String>,
    #[serde(rename = "use")]
    key_use: Option<String>,
    /// Private part. Never read, only noticed.
    d: Option<serde::de::IgnoredAny>,
}

/// Length of a P-256 coordinate and of an Ed25519 public key.
const COORDINATE_LEN: usize = 32;

impl Jwk {
    /// The members every verification key shares, whatever its type: an `alg`
    /// that belongs to the key's family, `use` of `sig` if named, and no
    /// private part. A key is never widened by its `alg`; this only refuses a
    /// contradiction.
    fn check_public_signing_key(&self, family: AlgorithmFamily) -> Result<(), String> {
        if let Some(alg) = &self.alg
            && !alg
                .parse::<Algorithm>()
                .is_ok_and(|alg| verified_algorithms(family).contains(&alg))
        {
            return Err(format!(
                "has alg {alg:?}; kty {:?} verifies only {:?}",
                self.kty,
                verified_algorithms(family)
            ));
        }
        if let Some(key_use) = &self.key_use
            && key_use != "sig"
        {
            return Err(format!("has use {key_use:?}; only sig"));
        }
        // Only the public half belongs in a JWKS. A private key in a pinned
        // one would let anyone who can read the config mint tokens.
        if self.d.is_some() {
            return Err("carries private key material (d)".into());
        }
        Ok(())
    }

    /// The verification key this entry is, or the reason it is none: RSA
    /// (`n`, `e`), P-256 (`kty: EC`) or Ed25519 (`kty: OKP`). The key's family
    /// comes from `kty` alone. Both resolvers build keys here, so what counts
    /// as a usable entry is written once; each decides what a refusal costs.
    fn verification_key(&self) -> Result<DecodingKey, String> {
        let key = match self.kty.as_str() {
            "RSA" => {
                self.check_public_signing_key(AlgorithmFamily::Rsa)?;
                let (Some(n), Some(e)) = (&self.n, &self.e) else {
                    return Err("lacks n or e".into());
                };
                DecodingKey::from_rsa_components(n, e)
            }
            "EC" => {
                self.check_public_signing_key(AlgorithmFamily::Ec)?;
                self.check_curve("P-256")?;
                let x = coordinate("x", self.x.as_deref())?;
                let y = coordinate("y", self.y.as_deref())?;
                DecodingKey::from_ec_components(x, y)
            }
            "OKP" => {
                self.check_public_signing_key(AlgorithmFamily::Ed)?;
                self.check_curve("Ed25519")?;
                DecodingKey::from_ed_components(coordinate("x", self.x.as_deref())?)
            }
            other => {
                return Err(format!(
                    "has kty {other:?}; only RSA, EC and OKP are verified"
                ));
            }
        };
        key.map_err(|err| format!("has an unusable key: {err}"))
    }

    /// An entry of a fetched set, read as tolerantly as the HTTP resolver
    /// always has: `kty` in any case, and RSA when `kty` is missing but `n`
    /// and `e` are published. A pinned set is the operator's and stays exact.
    fn tolerant(mut self) -> Self {
        if self.kty.eq_ignore_ascii_case("RSA")
            || (self.kty.is_empty() && self.n.is_some() && self.e.is_some())
        {
            self.kty = "RSA".into();
        }
        self
    }

    fn check_curve(&self, supported: &str) -> Result<(), String> {
        match self.crv.as_deref() {
            Some(crv) if crv == supported => Ok(()),
            Some(crv) => Err(format!(
                "has crv {crv:?}; kty {:?} verifies only {supported}",
                self.kty
            )),
            None => Err("lacks crv".into()),
        }
    }
}

/// A public coordinate: present, base64url, and exactly [`COORDINATE_LEN`]
/// bytes once decoded (RFC 7518 section 6.2.1.2 keeps leading zeros).
fn coordinate<'a>(name: &str, value: Option<&'a str>) -> Result<&'a str, String> {
    let value = value.ok_or_else(|| format!("lacks {name}"))?;
    match URL_SAFE_NO_PAD.decode(value) {
        Ok(bytes) if bytes.len() == COORDINATE_LEN => Ok(value),
        Ok(bytes) => Err(format!(
            "has an {name} of {} bytes; {COORDINATE_LEN} required",
            bytes.len()
        )),
        Err(_) => Err(format!("has an {name} that is not base64url")),
    }
}

/// A JWKS document. Entries stay raw JSON and are read into a [`Jwk`] one by
/// one, so an entry with a wrong-typed member fails only itself, never the
/// keys beside it: the HTTP resolver skips it, a pinned set refuses it.
#[derive(serde::Deserialize)]
struct JwkSet {
    keys: Vec<serde_json::Value>,
}

/// Production resolver: discovers the JWKS endpoint and caches keys by kid,
/// refreshing once on an unknown kid (key rotation) and hourly on a hit.
/// While the issuer cannot be reached, a cached set stays trusted for four
/// hours after its last successful fetch and resolves nothing after that.
/// Discovery metadata must name the configured issuer exactly before its
/// JWKS endpoint can be used.
pub struct HttpJwksResolver {
    issuer: String,
    jwks_uri: Option<String>,
    http: reqwest::Client,
    request_timeout: Duration,
    state: RwLock<JwksState>,
    /// Held by the one request that fetches. A cache miss waits for it; a hit
    /// on an aging set never does.
    refresh_gate: Mutex<()>,
}

/// The fetched key set and the two clocks that govern it, read and written
/// together.
#[derive(Default)]
struct JwksState {
    set: Option<FetchedKeys>,
    /// Last fetch attempt, successful or not; drives the refresh cooldown.
    attempted_at: Option<Instant>,
}

struct FetchedKeys {
    keys: HashMap<String, Arc<DecodingKey>>,
    fetched_at: Instant,
}

/// What the cached set says about one `kid` at one moment.
enum Cached {
    /// The set is younger than [`JWKS_MAX_AGE`] and holds the key.
    Fresh(Arc<DecodingKey>),
    /// The set is due for a refresh and still inside [`JWKS_TRUST_BOUND`].
    Aging(Arc<DecodingKey>),
    /// No set, a set past its trust bound, or a set without this `kid`.
    Absent,
}

impl JwksState {
    fn lookup(&self, kid: &str, now: Instant) -> Cached {
        let Some(set) = &self.set else {
            return Cached::Absent;
        };
        let age = now.saturating_duration_since(set.fetched_at);
        if age >= JWKS_TRUST_BOUND {
            return Cached::Absent;
        }
        match set.keys.get(kid) {
            Some(key) if age >= JWKS_MAX_AGE => Cached::Aging(Arc::clone(key)),
            Some(key) => Cached::Fresh(Arc::clone(key)),
            None => Cached::Absent,
        }
    }

    /// Whether the cooldown since the last fetch attempt has passed.
    fn attempt_due(&self, now: Instant) -> bool {
        self.attempted_at
            .is_none_or(|at| now.saturating_duration_since(at) >= JWKS_REFRESH_COOLDOWN)
    }
}

impl std::fmt::Debug for HttpJwksResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpJwksResolver")
            .field("issuer", &self.issuer)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

impl HttpJwksResolver {
    /// Build a resolver whose discovery and JWKS redirects obey the same
    /// issuer-aware URL policy as the initial endpoints.
    ///
    /// # Errors
    ///
    /// Returns an error when the issuer is not HTTPS or loopback HTTP, or
    /// when the JWKS endpoint is plaintext HTTP that this issuer is not
    /// entitled to name — only a loopback issuer may point at a loopback
    /// JWKS, so a remote provider cannot move key resolution onto the host.
    /// Issuer query and fragment components are rejected.
    pub fn new(issuer: String, jwks_uri: Option<String>) -> Result<Self, OidcConfigError> {
        Self::with_request_timeout(issuer, jwks_uri, DEFAULT_HTTP_REQUEST_TIMEOUT)
    }

    /// Construct a resolver with an explicit complete-request timeout.
    ///
    /// # Errors
    ///
    /// Returns the same URL errors as [`Self::new`], or an error when
    /// `request_timeout` is zero or exceeds [`MAX_HTTP_REQUEST_TIMEOUT`].
    pub fn with_request_timeout(
        issuer: String,
        jwks_uri: Option<String>,
        request_timeout: Duration,
    ) -> Result<Self, OidcConfigError> {
        let http = build_http_client(&issuer, reqwest::Client::builder());
        Self::with_http_client(issuer, jwks_uri, http, request_timeout)
    }

    /// Construct a resolver with an injected HTTP client and an explicit
    /// complete-request timeout. The timeout is applied to each request, so a
    /// client with a weaker or absent default cannot make discovery or JWKS
    /// body reads unbounded.
    ///
    /// The host owns the injected client's redirect policy. It must disable
    /// redirects or validate every target against the issuer's HTTPS/loopback
    /// policy before following it. Unlike the default constructors, this
    /// method cannot replace the policy on an already-built client.
    ///
    /// # Errors
    ///
    /// Returns the same URL and timeout errors as [`Self::with_request_timeout`].
    pub fn with_http_client(
        issuer: String,
        jwks_uri: Option<String>,
        http: reqwest::Client,
        request_timeout: Duration,
    ) -> Result<Self, OidcConfigError> {
        validate_issuer_url(&issuer)?;
        if let Some(uri) = &jwks_uri {
            validate_jwks_url("jwks_uri", uri, &issuer)?;
        }
        validate_request_timeout(request_timeout)?;
        Ok(Self {
            issuer,
            jwks_uri,
            http,
            request_timeout,
            state: RwLock::default(),
            refresh_gate: Mutex::new(()),
        })
    }

    /// Complete-request timeout applied to discovery and JWKS requests.
    #[must_use]
    pub const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    async fn jwks_endpoint(&self) -> Result<String, KeyError> {
        if let Some(uri) = &self.jwks_uri {
            return Ok(uri.clone());
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        );
        let cfg: OpenIdConfig = self.fetch_json(url).await?;
        if cfg.issuer != self.issuer {
            return Err(KeyError::Config(
                "discovered issuer does not match configured issuer".into(),
            ));
        }
        validate_jwks_url("discovered jwks_uri", &cfg.jwks_uri, &self.issuer)
            .map_err(|err| KeyError::Config(err.to_string()))?;
        Ok(cfg.jwks_uri)
    }

    /// Fetch the issuer's current key set. The caller holds `refresh_gate`.
    async fn fetch_keys(&self) -> Result<HashMap<String, Arc<DecodingKey>>, KeyError> {
        let endpoint = self.jwks_endpoint().await?;
        let set: JwkSet = self.fetch_json(endpoint).await?;
        let mut next = HashMap::new();
        for (index, entry) in set.keys.into_iter().enumerate() {
            // Tolerant parse: an unnamed, unsupported or malformed entry (a
            // wrong-typed member, an undecodable modulus, another curve, a
            // contradicting `alg`, a `use` other than `sig`, private material
            // included) is skipped, never half-trusted, so one such key in the
            // issuer's set doesn't fail the others.
            let jwk = match Jwk::deserialize(&entry) {
                Ok(jwk) => jwk.tolerant(),
                Err(err) => {
                    tracing::debug!(index, %err, "jwks entry is not a well-formed JWK; skipped");
                    continue;
                }
            };
            let Some(kid) = jwk.kid.clone() else {
                continue;
            };
            match jwk.verification_key() {
                Ok(key) => {
                    next.insert(kid, Arc::new(key));
                }
                Err(reason) => {
                    tracing::debug!(%kid, %reason, "jwks entry is not a usable verification key; skipped");
                }
            }
        }
        if next.is_empty() {
            return Err(KeyError::Parse(
                "jwks contained no named RSA, P-256 or Ed25519 keys".into(),
            ));
        }
        Ok(next)
    }

    /// One fetch attempt at `now`, spaced by the cooldown. The caller holds
    /// `refresh_gate`. Returns whether a set was fetched (`Ok(false)`: the
    /// cooldown refused the attempt) and the moment the attempt ended, which
    /// is when a fetched set starts to age: `now` advanced by the time since
    /// `began`, when the lookup read `now`.
    async fn refresh(&self, now: Instant, began: Instant) -> (Result<bool, KeyError>, Instant) {
        {
            let mut state = self.state.write().await;
            if !state.attempt_due(now) {
                return (Ok(false), now);
            }
            state.attempted_at = Some(now);
        }
        let fetched = self.fetch_keys().await;
        let now = now + began.elapsed();
        match fetched {
            Ok(keys) => {
                self.state.write().await.set = Some(FetchedKeys {
                    keys,
                    fetched_at: now,
                });
                (Ok(true), now)
            }
            Err(error) => (Err(error), now),
        }
    }

    /// [`KeyResolver::key_for`] at `now`. The lookup's moment is given, and
    /// advances only by the time the lookup itself waits and fetches.
    async fn key_at(&self, kid: &str, now: Instant) -> Result<Arc<DecodingKey>, KeyError> {
        let began = Instant::now();
        let cached = self.state.read().await.lookup(kid, now);
        let _refresh_guard = match cached {
            Cached::Fresh(key) => return Ok(key),
            // Another request is already fetching: an aging key does not wait
            // for it.
            Cached::Aging(key) => match self.refresh_gate.try_lock() {
                Ok(guard) => guard,
                Err(_) => return Ok(key),
            },
            Cached::Absent => self.refresh_gate.lock().await,
        };
        // Another request may have fetched before this one took the gate.
        let cached = self.state.read().await.lookup(kid, now);
        if let Cached::Fresh(key) = cached {
            return Ok(key);
        }
        // The cooldown spaces attempts, so a stream of unknown-kid tokens
        // can't drive one upstream JWKS fetch per request.
        let (refreshed, now) = self.refresh(now, began).await;
        // Resolve against the set as it is when the attempt ends: a fetched
        // set no longer holds a kid the issuer rotated out, and a set that
        // crossed its trust bound during a failed fetch holds nothing.
        let cached = self.state.read().await.lookup(kid, now);
        match (cached, refreshed) {
            (Cached::Fresh(key) | Cached::Aging(key), Ok(true)) => Ok(key),
            (Cached::Fresh(key) | Cached::Aging(key), Ok(false)) => {
                tracing::warn!("jwks cache past max age but refresh throttled");
                Ok(key)
            }
            (Cached::Fresh(key) | Cached::Aging(key), Err(error)) => {
                tracing::warn!(%error, "jwks refresh failed; using the cached key inside its trust bound");
                Ok(key)
            }
            (Cached::Absent, Ok(_)) => Err(KeyError::UnknownKid),
            (Cached::Absent, Err(error)) => Err(error),
        }
    }

    /// GET `url` and parse the body as JSON. Discovery and key-set requests
    /// share the timeout and the [`MAX_RESPONSE_BYTES`] cap, so an issuer
    /// cannot make either buffer an unbounded body.
    async fn fetch_json<T: serde::de::DeserializeOwned>(&self, url: String) -> Result<T, KeyError> {
        let mut response = self
            .http
            .get(url)
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(|e| KeyError::Fetch(e.to_string()))?
            .error_for_status()
            .map_err(|e| KeyError::Fetch(e.to_string()))?;
        let too_large = || KeyError::Fetch(format!("response exceeds {MAX_RESPONSE_BYTES} bytes"));
        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(too_large());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| KeyError::Parse(e.to_string()))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(too_large());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|e| KeyError::Parse(e.to_string()))
    }
}

fn build_http_client(issuer: &str, builder: reqwest::ClientBuilder) -> reqwest::Client {
    let issuer = issuer.to_owned();
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if let Err(error) = validate_jwks_url("redirect target", attempt.url().as_str(), &issuer) {
            return attempt.error(error);
        }
        reqwest::redirect::Policy::default().redirect(attempt)
    });
    builder
        .redirect(policy)
        .build()
        .expect("initialize OIDC HTTP client")
}

fn validate_request_timeout(request_timeout: Duration) -> Result<(), OidcConfigError> {
    if request_timeout.is_zero() || request_timeout > MAX_HTTP_REQUEST_TIMEOUT {
        return Err(OidcConfigError::InvalidTimeout {
            field: "OIDC HTTP request timeout",
            max_seconds: MAX_HTTP_REQUEST_TIMEOUT.as_secs(),
        });
    }
    Ok(())
}

#[async_trait]
impl KeyResolver for HttpJwksResolver {
    async fn key_for(&self, kid: &str) -> Result<Arc<DecodingKey>, KeyError> {
        self.key_at(kid, Instant::now()).await
    }
}

#[cfg(test)]
mod fixtures {
    //! JWK entries shared by both resolvers' tests, so "the pinned set
    //! refuses it" and "the HTTP resolver skips it" are about the same
    //! entries.

    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::{Value, json};

    // Public example keys from RFC 7517 appendix A.1 (P-256) and RFC 8037
    // appendix A.2 (Ed25519). No private half is shipped here.
    pub(super) const EC_X: &str = "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU";
    pub(super) const EC_Y: &str = "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0";
    pub(super) const OKP_X: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";

    pub(super) fn ec_key(kid: &str) -> Value {
        json!({
            "kty": "EC", "kid": kid, "crv": "P-256", "alg": "ES256", "use": "sig",
            "x": EC_X, "y": EC_Y
        })
    }

    pub(super) fn okp_key(kid: &str) -> Value {
        json!({
            "kty": "OKP", "kid": kid, "crv": "Ed25519", "alg": "EdDSA", "use": "sig",
            "x": OKP_X
        })
    }

    pub(super) fn with(mut key: Value, member: &str, value: Value) -> Value {
        key[member] = value;
        key
    }

    pub(super) fn without(mut key: Value, member: &str) -> Value {
        key.as_object_mut().expect("jwk object").remove(member);
        key
    }

    /// `len` bytes as base64url.
    fn coordinate(len: usize) -> Value {
        json!(URL_SAFE_NO_PAD.encode(vec![1_u8; len]))
    }

    /// Entries whose members have the wrong JSON type, one member each: not
    /// JWKs at all, so neither resolver can read them into a key.
    pub(super) fn mistyped_entries() -> Vec<Value> {
        let mistyped = |entry: Value, member: &str| with(entry, member, json!(7));
        vec![
            mistyped(ec_key("ec-crv-7"), "crv"),
            mistyped(ec_key("ec-x-7"), "x"),
            mistyped(ec_key("ec-y-7"), "y"),
            mistyped(ec_key("ec-alg-7"), "alg"),
            mistyped(ec_key("ec-use-7"), "use"),
            mistyped(ec_key("ec-kty-7"), "kty"),
            mistyped(ec_key("ec-kid-7"), "kid"),
            mistyped(okp_key("okp-x-7"), "x"),
            mistyped(json!({ "kid": "rsa-n-7", "kty": "RSA", "e": "AQAB" }), "n"),
            with(ec_key("ec-all-mistyped"), "crv", json!(["P-256"])),
            json!("not an object"),
            json!(7),
            json!(null),
        ]
    }

    /// EC and OKP entries with exactly one fault each, named by their `kid`:
    /// everything a key-type-specific check must refuse.
    pub(super) fn faulty_ec_and_okp_entries() -> Vec<Value> {
        let ec = |fault: &str| ec_key(fault);
        let okp = |fault: &str| okp_key(fault);
        vec![
            without(ec("ec-no-crv"), "crv"),
            without(ec("ec-no-x"), "x"),
            without(ec("ec-no-y"), "y"),
            with(ec("ec-p384"), "crv", json!("P-384")),
            with(ec("ec-ed25519-curve"), "crv", json!("Ed25519")),
            with(ec("ec-lowercase-curve"), "crv", json!("p-256")),
            with(ec("ec-short-x"), "x", coordinate(31)),
            with(ec("ec-long-x"), "x", coordinate(33)),
            with(ec("ec-short-y"), "y", coordinate(31)),
            with(ec("ec-long-y"), "y", coordinate(33)),
            with(with(ec("ec-truncated"), "x", json!("AQ")), "y", json!("AQ")),
            with(ec("ec-not-base64"), "x", json!("!!not base64url!!")),
            with(ec("ec-alg-rs256"), "alg", json!("RS256")),
            with(ec("ec-alg-eddsa"), "alg", json!("EdDSA")),
            with(ec("ec-alg-es384"), "alg", json!("ES384")),
            with(ec("ec-alg-hs256"), "alg", json!("HS256")),
            with(ec("ec-alg-none"), "alg", json!("none")),
            with(ec("ec-use-enc"), "use", json!("enc")),
            with(ec("ec-private"), "d", json!("AQAB")),
            without(okp("okp-no-crv"), "crv"),
            without(okp("okp-no-x"), "x"),
            with(
                with(okp("okp-ed448"), "crv", json!("Ed448")),
                "x",
                coordinate(57),
            ),
            with(okp("okp-x25519"), "crv", json!("X25519")),
            with(okp("okp-p256-curve"), "crv", json!("P-256")),
            with(okp("okp-short-x"), "x", coordinate(31)),
            with(okp("okp-long-x"), "x", coordinate(33)),
            with(okp("okp-truncated"), "x", json!("AQ")),
            with(okp("okp-not-base64"), "x", json!("!!not base64url!!")),
            with(okp("okp-alg-es256"), "alg", json!("ES256")),
            with(okp("okp-alg-rs256"), "alg", json!("RS256")),
            with(okp("okp-alg-hs256"), "alg", json!("HS256")),
            with(okp("okp-use-enc"), "use", json!("enc")),
            with(okp("okp-private"), "d", json!("AQAB")),
        ]
    }
}

#[cfg(test)]
#[path = "keys_http_tests.rs"]
mod http_tests;

#[cfg(test)]
mod pinned_jwks_tests {
    use serde_json::{Value, json};

    use super::fixtures::{
        ec_key, faulty_ec_and_okp_entries, mistyped_entries, okp_key, with, without,
    };
    use super::http_tests::{TEST_JWK_E, TEST_JWK_N};
    use super::{KeyError, KeyResolver, StaticJwksResolver};

    fn rsa_key(kid: &str) -> Value {
        json!({ "kty": "RSA", "kid": kid, "alg": "RS256", "use": "sig", "n": TEST_JWK_N, "e": TEST_JWK_E })
    }

    fn parse(keys: &[Value]) -> Result<StaticJwksResolver, KeyError> {
        StaticJwksResolver::from_jwks_json(&json!({ "keys": keys }).to_string())
    }

    #[tokio::test]
    async fn a_pinned_set_resolves_each_key_by_kid_and_nothing_else() {
        let resolver = parse(&[rsa_key("k1"), without(without(rsa_key("k2"), "alg"), "use")])
            .expect("named RSA keys with and without alg/use");

        assert!(resolver.key_for("k1").await.is_ok());
        assert!(resolver.key_for("k2").await.is_ok());
        assert_eq!(
            resolver.key_for("k3").await.err(),
            Some(KeyError::UnknownKid)
        );
    }

    /// An entry the HTTP resolver would skip is refused here: a pinned key
    /// silently dropped is one the operator believes is trusted and is not.
    #[test]
    fn every_entry_must_be_a_named_public_verification_key() {
        let refused = [
            ("empty set", vec![]),
            ("no kid", vec![without(rsa_key("k1"), "kid")]),
            ("blank kid", vec![rsa_key(" ")]),
            (
                "EC key without crv",
                vec![with(rsa_key("k1"), "kty", json!("EC"))],
            ),
            ("oct key", vec![with(rsa_key("k1"), "kty", json!("oct"))]),
            ("no kty", vec![without(rsa_key("k1"), "kty")]),
            ("no n", vec![without(rsa_key("k1"), "n")]),
            ("no e", vec![without(rsa_key("k1"), "e")]),
            ("HS256", vec![with(rsa_key("k1"), "alg", json!("HS256"))]),
            ("none", vec![with(rsa_key("k1"), "alg", json!("none"))]),
            ("PS256", vec![with(rsa_key("k1"), "alg", json!("PS256"))]),
            (
                "EdDSA on RSA",
                vec![with(rsa_key("k1"), "alg", json!("EdDSA"))],
            ),
            (
                "ES256 on RSA",
                vec![with(rsa_key("k1"), "alg", json!("ES256"))],
            ),
            ("enc use", vec![with(rsa_key("k1"), "use", json!("enc"))]),
            ("private d", vec![with(rsa_key("k1"), "d", json!("AQAB"))]),
            ("duplicate kid", vec![rsa_key("k1"), rsa_key("k1")]),
            (
                "one bad key among good",
                vec![rsa_key("k1"), with(rsa_key("k2"), "kty", json!("OKP"))],
            ),
        ];
        for (case, keys) in refused {
            assert!(
                matches!(parse(&keys), Err(KeyError::Config(_))),
                "{case} must be a config error"
            );
        }
    }

    #[tokio::test]
    async fn p256_and_ed25519_keys_are_pinned_beside_rsa_keys() {
        use jsonwebtoken::AlgorithmFamily;

        let resolver = parse(&[
            rsa_key("rsa"),
            ec_key("ec"),
            okp_key("okp"),
            without(without(ec_key("ec-bare"), "alg"), "use"),
            without(without(okp_key("okp-bare"), "alg"), "use"),
        ])
        .expect("RSA, P-256 and Ed25519 keys");

        for (kid, family) in [
            ("rsa", AlgorithmFamily::Rsa),
            ("ec", AlgorithmFamily::Ec),
            ("okp", AlgorithmFamily::Ed),
            ("ec-bare", AlgorithmFamily::Ec),
            ("okp-bare", AlgorithmFamily::Ed),
        ] {
            let key = resolver.key_for(kid).await.expect("resolves");
            assert_eq!(key.family(), family, "{kid}");
        }
    }

    /// Every EC/OKP fault the HTTP resolver skips is a load error here, and a
    /// bad entry among good ones fails the whole set.
    #[test]
    fn a_faulty_ec_or_okp_entry_is_a_config_error() {
        let faulty = faulty_ec_and_okp_entries();
        assert!(!faulty.is_empty());
        for entry in faulty {
            let kid = entry["kid"].as_str().expect("kid").to_owned();
            assert!(
                matches!(
                    parse(std::slice::from_ref(&entry)),
                    Err(KeyError::Config(_))
                ),
                "{kid} must be a config error"
            );
            assert!(
                matches!(
                    parse(&[rsa_key("k1"), ec_key("ec"), entry]),
                    Err(KeyError::Config(_))
                ),
                "{kid} among good keys must fail the set"
            );
        }
    }

    /// An entry with a wrong-typed member is not a JWK: the pinned set is
    /// refused at load as a config error, alone or beside a good key.
    #[test]
    fn a_mistyped_entry_is_a_config_error() {
        let entries = mistyped_entries();
        assert!(!entries.is_empty());
        for entry in entries {
            assert!(
                matches!(
                    parse(std::slice::from_ref(&entry)),
                    Err(KeyError::Config(_))
                ),
                "{entry} must be a config error"
            );
            assert!(
                matches!(
                    parse(&[rsa_key("k1"), entry.clone()]),
                    Err(KeyError::Config(_))
                ),
                "{entry} beside a good key must fail the set"
            );
        }
    }

    #[test]
    fn a_duplicate_kid_across_key_types_is_refused() {
        assert!(matches!(
            parse(&[ec_key("k1"), okp_key("k1")]),
            Err(KeyError::Config(_))
        ));
    }

    #[test]
    fn a_document_that_is_not_a_jwks_is_a_parse_error() {
        for raw in ["", "not json", "{}", r#"{"keys":{}}"#, r#"[{"kid":"k1"}]"#] {
            assert!(
                matches!(
                    StaticJwksResolver::from_jwks_json(raw),
                    Err(KeyError::Parse(_))
                ),
                "{raw:?} must be a parse error"
            );
        }
    }

    #[test]
    fn an_undecodable_modulus_is_a_config_error() {
        assert!(matches!(
            parse(&[with(rsa_key("k1"), "n", json!("!!not base64url!!"))]),
            Err(KeyError::Config(_))
        ));
    }
}
