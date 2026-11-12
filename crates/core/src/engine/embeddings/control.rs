//! Shared client admission for embedding calls, plus observed queue routes.
//!
//! Admission never queues: a provider takes concurrent requests and queues or
//! refuses what exceeds its capacity. Admission only refuses calls a client
//! has been told to stop: a configuration fault stops every call, a transient
//! backoff stops background work while search queries still go out.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use tokio::time::Instant;

use crate::OwnerId;
use crate::llm::{
    EmbeddingClient, EmbeddingConfigurationFault, EmbeddingInputRefusal, EmbeddingRoute,
    EmbeddingRuntimePolicy, EmbeddingSpace, LlmError,
};

const MAX_BACKOFF: Duration = Duration::from_mins(15);
/// The longest a provider's `Retry-After` hint holds a client back. A hint beyond a day (or one
/// that overflows a duration) would otherwise stall the route until the process restarts.
const MAX_RETRY_AFTER: Duration = Duration::from_hours(24);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn next_backoff(previous: Option<Duration>, base: Duration) -> Duration {
    previous
        .map_or(base, |delay| delay.saturating_mul(2))
        .min(MAX_BACKOFF.max(base))
}

#[derive(Debug, Clone, Copy)]
struct Wait {
    since: Instant,
    delay: Duration,
    backoff: Duration,
    status: Option<u16>,
}

impl Wait {
    fn remaining(self, now: Instant) -> Duration {
        self.delay
            .saturating_sub(now.saturating_duration_since(self.since))
    }
}

#[derive(Debug, Default)]
enum ClientState {
    #[default]
    Ready,
    Waiting(Wait),
    Paused(EmbeddingConfigurationFault),
}

impl ClientState {
    fn refusal(&self, admission: Admission, now: Instant) -> Option<LlmError> {
        match (self, admission) {
            (Self::Ready, _) | (Self::Waiting(_), Admission::Query) => None,
            (Self::Paused(fault), _) => Some(LlmError::Configuration(*fault)),
            (Self::Waiting(wait), Admission::Background) => {
                let remaining = wait.remaining(now);
                (!remaining.is_zero()).then(|| LlmError::Retryable {
                    status: wait.status,
                    retry_after: Some(remaining),
                    message: "embedding client is backed off".into(),
                })
            }
        }
    }

    fn settle<T>(&mut self, result: &Result<T, LlmError>, base: Duration, now: Instant) {
        let (status, retry_after) = match result {
            Ok(_)
            | Err(LlmError::InputRefused(
                EmbeddingInputRefusal::PayloadTooLarge
                | EmbeddingInputRefusal::UnprocessableContent,
            )) => {
                *self = Self::Ready;
                return;
            }
            Err(LlmError::InputRefused(EmbeddingInputRefusal::LocalCap { .. })) => return,
            Err(LlmError::Configuration(fault)) => {
                *self = Self::Paused(*fault);
                return;
            }
            Err(LlmError::Retryable {
                status,
                retry_after,
                ..
            }) => (*status, *retry_after),
            Err(LlmError::Internal(_)) => (None, None),
        };
        let previous = match self {
            Self::Waiting(wait) => Some(wait.backoff),
            Self::Ready | Self::Paused(_) => None,
        };
        let backoff = next_backoff(previous, base);
        *self = Self::Waiting(Wait {
            since: now,
            delay: backoff.max(retry_after.unwrap_or_default().min(MAX_RETRY_AFTER)),
            backoff,
            status,
        });
    }
}

/// What a call is, fixed by the operation the engine invokes: a search query
/// through [`EmbeddingClient::embed_query`], anything else is background work.
#[derive(Debug, Clone, Copy)]
enum Admission {
    /// Indexing and maintenance: owns the client's transient backoff.
    Background,
    /// A search query: one small request a caller waits for. It passes a
    /// backoff that background work published, and its own transient failure
    /// goes back to its caller without backing the client off.
    Query,
}

#[derive(Debug, Default)]
struct SharedState {
    value: ClientState,
    refusal_generation: u64,
}

impl SharedState {
    fn refusal(&self, admission: Admission, now: Instant) -> Option<LlmError> {
        self.value.refusal(admission, now)
    }

    /// Whether the client may accept more background work.
    fn blocked(&self, now: Instant) -> bool {
        self.refusal(Admission::Background, now).is_some()
    }

    fn settle<T>(
        &mut self,
        result: &Result<T, LlmError>,
        admission: Admission,
        generation: u64,
        base: Duration,
        now: Instant,
    ) {
        if matches!(admission, Admission::Query)
            && !matches!(result, Err(LlmError::Configuration(_)))
        {
            return;
        }
        // An in-flight success cannot clear a refusal published by a concurrent call.
        // Configuration pauses last until the original client binding is replaced.
        let refusal = matches!(
            result,
            Err(LlmError::Configuration(_) | LlmError::Retryable { .. } | LlmError::Internal(_))
        );
        if matches!(self.value, ClientState::Paused(_))
            || (!refusal && generation != self.refusal_generation)
        {
            return;
        }
        let previous_wait = match self.value {
            ClientState::Waiting(wait) if generation != self.refusal_generation => {
                wait.remaining(now)
            }
            _ => Duration::ZERO,
        };
        self.value.settle(result, base, now);
        if refusal {
            self.refusal_generation = self.refusal_generation.wrapping_add(1);
            if let ClientState::Waiting(wait) = &mut self.value {
                wait.delay = wait.delay.max(previous_wait);
            }
        }
    }
}

#[derive(Debug)]
struct ClientControl {
    trace_id: u64,
    state: Mutex<SharedState>,
}

impl Default for ClientControl {
    fn default() -> Self {
        static NEXT_CLIENT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            trace_id: NEXT_CLIENT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            state: Mutex::default(),
        }
    }
}

impl ClientControl {
    async fn call<T>(
        &self,
        policy: EmbeddingRuntimePolicy,
        admission: Admission,
        call: impl std::future::Future<Output = Result<T, LlmError>>,
    ) -> Result<T, LlmError> {
        let generation = {
            let state = lock(&self.state);
            if let Some(error) = state.refusal(admission, Instant::now()) {
                return Err(error);
            }
            state.refusal_generation
        };
        let result = crate::stage_timing::timed(
            "embedding_provider",
            tokio::time::timeout(policy.request_timeout(), call),
        )
        .await
        .unwrap_or_else(|_| {
            Err(LlmError::retryable(format!(
                "request timed out after {} seconds",
                policy.request_timeout().as_secs()
            )))
        });
        lock(&self.state).settle(
            &result,
            admission,
            generation,
            policy.worker_interval(),
            Instant::now(),
        );
        result
    }
}

#[derive(Debug)]
struct ClientEntry {
    origin: Weak<dyn EmbeddingClient>,
    control: Arc<ClientControl>,
}

#[derive(Debug)]
struct Binding {
    space: EmbeddingSpace,
    control: Arc<ClientControl>,
}

#[derive(Debug, Default)]
struct Registry {
    clients: Vec<ClientEntry>,
    owners: HashMap<OwnerId, Vec<Binding>>,
    route_backoff: HashMap<OwnerId, Wait>,
}

/// Shared by manual drains, workers, queries, maintenance and health.
#[derive(Debug, Default)]
pub(crate) struct EmbeddingControl {
    registry: Mutex<Registry>,
    /// When a drain last took the stale-claim reclaim.
    last_reclaim: Mutex<Option<Instant>>,
}

impl EmbeddingControl {
    /// Whether this drain runs the stale-claim reclaim: the first drain, then
    /// one in every `every`, however many batches are claimed in between.
    pub(super) fn reclaim_due(&self, now: Instant, every: Duration) -> bool {
        let mut last = lock(&self.last_reclaim);
        if last.is_some_and(|at| now.saturating_duration_since(at) < every) {
            return false;
        }
        *last = Some(now);
        true
    }

    pub(super) fn observe(
        &self,
        owner: OwnerId,
        route: EmbeddingRoute,
        policy: EmbeddingRuntimePolicy,
    ) -> EmbeddingRoute {
        let mut registry = lock(&self.registry);
        registry
            .clients
            .retain(|entry| entry.origin.strong_count() > 0);
        registry.route_backoff.remove(&owner);
        let mut bindings = Vec::new();
        let route = route.wrap_clients(|bound, inner| {
            let origin = Arc::downgrade(bound.origin());
            let control = if let Some(entry) = registry
                .clients
                .iter()
                .find(|entry| entry.origin.ptr_eq(&origin))
            {
                Arc::clone(&entry.control)
            } else {
                let control = Arc::new(ClientControl::default());
                registry.clients.push(ClientEntry {
                    origin,
                    control: Arc::clone(&control),
                });
                control
            };
            let space = bound.space().clone();
            bindings.push(Binding {
                space: space.clone(),
                control: Arc::clone(&control),
            });
            Arc::new(ControlledClient {
                inner,
                space,
                control,
                policy,
            })
        });
        if bindings.is_empty() {
            registry.owners.remove(&owner);
        } else {
            registry.owners.insert(owner, bindings);
        }
        route
    }

    pub(super) fn owners_to_refresh(&self, now: Instant) -> Vec<OwnerId> {
        lock(&self.registry)
            .owners
            .iter()
            .filter_map(|(owner, bindings)| {
                bindings
                    .iter()
                    .any(|binding| lock(&binding.control.state).blocked(now))
                    .then_some(*owner)
            })
            .collect()
    }

    pub(super) fn blocked_pairs(&self, now: Instant) -> Vec<(OwnerId, EmbeddingSpace)> {
        lock(&self.registry)
            .owners
            .iter()
            .flat_map(|(owner, bindings)| {
                bindings
                    .iter()
                    .filter(|binding| lock(&binding.control.state).blocked(now))
                    .map(move |binding| (*owner, binding.space.clone()))
            })
            .collect()
    }

    pub(super) fn blocked_owners(&self, now: Instant) -> Vec<OwnerId> {
        lock(&self.registry)
            .route_backoff
            .iter()
            .filter_map(|(owner, wait)| (!wait.remaining(now).is_zero()).then_some(*owner))
            .collect()
    }

    pub(super) fn route_failed(&self, owner: OwnerId, base: Duration, now: Instant) -> Duration {
        let mut registry = lock(&self.registry);
        if let Some(wait) = registry.route_backoff.get(&owner) {
            let remaining = wait.remaining(now);
            if !remaining.is_zero() {
                return remaining;
            }
        }
        let backoff = next_backoff(
            registry.route_backoff.get(&owner).map(|wait| wait.backoff),
            base,
        );
        registry.route_backoff.insert(
            owner,
            Wait {
                since: now,
                delay: backoff,
                backoff,
                status: None,
            },
        );
        backoff
    }

    pub(super) fn configuration_faults(&self) -> Vec<EmbeddingConfigurationFault> {
        lock(&self.registry)
            .owners
            .values()
            .flatten()
            .filter_map(|binding| match lock(&binding.control.state).value {
                ClientState::Paused(fault) => Some(fault),
                ClientState::Ready | ClientState::Waiting(_) => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

#[derive(Debug)]
struct ControlledClient {
    inner: Arc<dyn EmbeddingClient>,
    space: EmbeddingSpace,
    control: Arc<ClientControl>,
    policy: EmbeddingRuntimePolicy,
}

impl ControlledClient {
    fn validate(&self, vector: Vec<f32>) -> Result<Vec<f32>, LlmError> {
        if vector.len() != self.space.dim().width() {
            return Err(LlmError::Configuration(
                EmbeddingConfigurationFault::VectorWidth {
                    expected: self.space.dim().width(),
                    received: vector.len(),
                },
            ));
        }
        if vector.iter().any(|value| !value.is_finite()) {
            return Err(LlmError::retryable(
                "embedding contains a non-finite component",
            ));
        }
        Ok(vector)
    }

    async fn embed_one(
        &self,
        admission: Admission,
        text: &str,
        kind: &'static str,
    ) -> Result<Vec<f32>, LlmError> {
        use tracing::Instrument as _;
        self.control
            .call(self.policy, admission, async {
                let vector = match admission {
                    Admission::Background => self.inner.embed(text).await?,
                    Admission::Query => self.inner.embed_query(text).await?,
                };
                self.validate(vector)
            })
            .instrument(
                tracing::debug_span!(target: "proxiima::stage_timing", "embedding_call",
                call = %uuid::Uuid::now_v7(), client = self.control.trace_id, kind),
            )
            .await
    }
}

#[async_trait::async_trait]
impl EmbeddingClient for ControlledClient {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.embed_one(Admission::Background, text, "single").await
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.embed_one(Admission::Query, text, "query").await
    }

    async fn embed_many(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
        use tracing::Instrument as _;
        self.control.call(self.policy, Admission::Background, async {
            let vectors = self.inner.embed_many(texts).await?;
            if vectors.len() != texts.len() {
                return Err(LlmError::retryable(format!("embedding batch cardinality mismatch: sent {} texts but received {} vectors", texts.len(), vectors.len())));
            }
            vectors.into_iter().map(|vector| self.validate(vector)).collect()
        }).instrument(tracing::debug_span!(target: "proxiima::stage_timing", "embedding_call",
            call = %uuid::Uuid::now_v7(), client = self.control.trace_id, kind = "batch")).await
    }

    fn model_id(&self) -> &str {
        self.space.model_id()
    }

    fn dim(&self) -> usize {
        self.space.dim().width()
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
