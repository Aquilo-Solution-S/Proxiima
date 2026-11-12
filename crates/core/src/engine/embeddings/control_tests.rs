use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::embedding::BoundEmbeddingClient;

const PATIENCE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
enum Reply {
    Accept,
    Configuration(EmbeddingConfigurationFault),
    Busy(Option<Duration>),
    Input,
    LocalCap,
    WrongWidth,
    WrongCount,
    NonFinite,
}

#[derive(Debug)]
struct RecordingClient {
    model: &'static str,
    reply: Mutex<Reply>,
    calls: AtomicUsize,
}

impl RecordingClient {
    fn new(model: &'static str, reply: Reply) -> Arc<Self> {
        Arc::new(Self {
            model,
            reply: Mutex::new(reply),
            calls: AtomicUsize::new(0),
        })
    }

    fn respond(&self) -> Result<Vec<f32>, LlmError> {
        match *lock(&self.reply) {
            Reply::Accept | Reply::WrongCount => Ok(vec![1.0; 384]),
            Reply::Configuration(fault) => Err(LlmError::Configuration(fault)),
            Reply::Busy(retry_after) => Err(LlmError::Retryable {
                status: Some(429),
                retry_after,
                message: "busy".into(),
            }),
            Reply::Input => Err(LlmError::InputRefused(
                EmbeddingInputRefusal::UnprocessableContent,
            )),
            Reply::LocalCap => Err(LlmError::InputRefused(EmbeddingInputRefusal::LocalCap {
                actual_chars: 2_048,
                max_chars: 1_024,
            })),
            Reply::WrongWidth => Ok(vec![1.0; 383]),
            Reply::NonFinite => Ok(vec![f32::NAN; 384]),
        }
    }
}

#[async_trait::async_trait]
impl EmbeddingClient for RecordingClient {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.respond()
    }
    async fn embed_many(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let vector = self.respond()?;
        let count = if matches!(*lock(&self.reply), Reply::WrongCount) {
            0
        } else {
            texts.len()
        };
        Ok(vec![vector; count])
    }
    fn model_id(&self) -> &'static str {
        self.model
    }
    fn dim(&self) -> usize {
        384
    }
}

fn bound(client: &Arc<RecordingClient>) -> BoundEmbeddingClient {
    BoundEmbeddingClient::bind(Arc::clone(client) as Arc<dyn EmbeddingClient>)
        .expect("supported width")
}

#[test]
fn the_stale_claim_reclaim_runs_once_per_interval() {
    let control = EmbeddingControl::default();
    let every = Duration::from_mins(5);
    let start = Instant::now();
    assert!(
        control.reclaim_due(start, every),
        "the first drain reclaims"
    );
    for batch in 1..=100_u64 {
        let now = start + Duration::from_secs(batch);
        assert!(
            !control.reclaim_due(now, every),
            "batch {batch} of a backlog reclaims nothing"
        );
    }
    assert!(
        control.reclaim_due(start + every, every),
        "the interval is up"
    );
    assert!(!control.reclaim_due(start + every, every), "and taken once");
}

#[tokio::test]
async fn configuration_pause_is_shared_and_new_original_client_retires_only_its_binding() {
    for fault in [
        EmbeddingConfigurationFault::BadRequest,
        EmbeddingConfigurationFault::Unauthorized,
        EmbeddingConfigurationFault::Forbidden,
        EmbeddingConfigurationFault::NotFound,
    ] {
        let control = EmbeddingControl::default();
        let old = RecordingClient::new("shared", Reply::Configuration(fault));
        let owner_a = OwnerId::new(uuid::Uuid::now_v7());
        let owner_b = OwnerId::new(uuid::Uuid::now_v7());
        let policy = EmbeddingRuntimePolicy::default();
        let a = control.observe(owner_a, EmbeddingRoute::current(bound(&old)), policy);
        let b = control.observe(owner_b, EmbeddingRoute::current(bound(&old)), policy);
        assert!(
            matches!(a.current_client().expect("current").embed("one").await, Err(LlmError::Configuration(actual)) if actual == fault)
        );
        assert!(
            matches!(b.current_client().expect("current").embed("two").await, Err(LlmError::Configuration(actual)) if actual == fault)
        );
        assert_eq!(
            old.calls.load(Ordering::SeqCst),
            1,
            "shared original pauses all bindings"
        );
        assert_eq!(control.configuration_faults(), [fault]);

        *lock(&old.reply) = Reply::Accept;
        let same = control.observe(owner_a, EmbeddingRoute::current(bound(&old)), policy);
        assert!(matches!(
            same.current_client()
                .expect("current")
                .embed("same Arc")
                .await,
            Err(LlmError::Configuration(_))
        ));
        assert_eq!(
            old.calls.load(Ordering::SeqCst),
            1,
            "same-Arc mutation cannot reset a pause"
        );

        let new = RecordingClient::new("shared", Reply::Accept);
        let replacement = control.observe(owner_a, EmbeddingRoute::current(bound(&new)), policy);
        replacement
            .current_client()
            .expect("current")
            .embed("new Arc")
            .await
            .expect("same-space new client is ready");
        assert_eq!(new.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            control.configuration_faults(),
            [fault],
            "other owner still references old original"
        );
        control.observe(owner_b, EmbeddingRoute::none(), policy);
        assert!(
            control.configuration_faults().is_empty(),
            "no active observed old binding"
        );
        assert!(control.blocked_pairs(Instant::now()).is_empty());

        let returned = control.observe(owner_b, EmbeddingRoute::current(bound(&old)), policy);
        assert!(matches!(
            returned
                .current_client()
                .expect("current")
                .embed("old returns")
                .await,
            Err(LlmError::Configuration(_))
        ));
        assert_eq!(
            old.calls.load(Ordering::SeqCst),
            1,
            "weak pause memo never probes a surviving original"
        );
        assert_eq!(control.configuration_faults(), [fault]);
    }
}

#[tokio::test]
async fn retired_pause_memo_does_not_keep_the_original_client_alive() {
    let control = EmbeddingControl::default();
    let old = RecordingClient::new(
        "old",
        Reply::Configuration(EmbeddingConfigurationFault::Unauthorized),
    );
    let weak = Arc::downgrade(&old);
    let owner = OwnerId::new(uuid::Uuid::now_v7());
    let route = control.observe(
        owner,
        EmbeddingRoute::current(bound(&old)),
        EmbeddingRuntimePolicy::default(),
    );
    route
        .current_client()
        .expect("current")
        .embed("pause")
        .await
        .expect_err("configuration");
    control.observe(
        owner,
        EmbeddingRoute::none(),
        EmbeddingRuntimePolicy::default(),
    );
    drop(route);
    drop(old);
    assert!(
        weak.upgrade().is_none(),
        "memo holds only weak original identity"
    );
    control.observe(
        owner,
        EmbeddingRoute::current(bound(&RecordingClient::new("new", Reply::Accept))),
        EmbeddingRuntimePolicy::default(),
    );
    assert_eq!(
        lock(&control.registry).clients.len(),
        1,
        "expired original memo is pruned"
    );
}

#[tokio::test(start_paused = true)]
async fn retry_after_outlives_local_cap_without_suppressed_calls_extending_it() {
    let control = EmbeddingControl::default();
    let client = RecordingClient::new("busy", Reply::Busy(Some(Duration::from_hours(1))));
    let owner = OwnerId::new(uuid::Uuid::now_v7());
    let route = control.observe(
        owner,
        EmbeddingRoute::current(bound(&client)),
        EmbeddingRuntimePolicy::default(),
    );
    let client_bound = route.current_client().expect("current");
    assert!(
        matches!(client_bound.embed("first").await, Err(LlmError::Retryable { status: Some(429), retry_after: Some(delay), .. }) if delay == Duration::from_hours(1))
    );
    *lock(&client.reply) = Reply::Accept;
    tokio::time::advance(Duration::from_secs(3599)).await;
    assert!(
        matches!(client_bound.embed("too early").await, Err(LlmError::Retryable { retry_after: Some(delay), .. }) if delay == Duration::from_secs(1))
    );
    assert_eq!(client.calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    client_bound.embed("at bound").await.expect("recovered");
    assert_eq!(
        client.calls.load(Ordering::SeqCst),
        2,
        "suppressed call never moved retry bound"
    );
    assert!(control.blocked_pairs(Instant::now()).is_empty());

    let huge = RecordingClient::new("huge", Reply::Busy(Some(Duration::MAX)));
    let route = control.observe(
        owner,
        EmbeddingRoute::current(bound(&huge)),
        EmbeddingRuntimePolicy::default(),
    );
    route
        .current_client()
        .expect("current")
        .embed("first")
        .await
        .expect_err("huge retry delay");
    tokio::time::advance(MAX_RETRY_AFTER.saturating_sub(Duration::from_secs(1))).await;
    assert!(
        matches!(route.current_client().expect("current").embed("still waiting").await, Err(LlmError::Retryable { retry_after: Some(delay), .. }) if delay == Duration::from_secs(1))
    );
    assert_eq!(
        huge.calls.load(Ordering::SeqCst),
        1,
        "no deadline overflow or early retry"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    route
        .current_client()
        .expect("current")
        .embed("at the cap")
        .await
        .expect_err("the provider still answers busy");
    assert_eq!(
        huge.calls.load(Ordering::SeqCst),
        2,
        "a hint beyond the cap holds the client back for the cap only"
    );
}

#[tokio::test(start_paused = true)]
async fn a_local_input_cap_preserves_accumulated_provider_backoff() {
    let control = EmbeddingControl::default();
    let client = RecordingClient::new("local-cap", Reply::Busy(None));
    let policy = EmbeddingRuntimePolicy::default();
    let route = control.observe(
        OwnerId::new(uuid::Uuid::now_v7()),
        EmbeddingRoute::current(bound(&client)),
        policy,
    );
    let bound = route.current_client().expect("current");
    bound
        .embed("first provider failure")
        .await
        .expect_err("busy");
    tokio::time::advance(policy.worker_interval()).await;

    *lock(&client.reply) = Reply::LocalCap;
    assert!(matches!(
        bound.embed("locally over the cap").await,
        Err(LlmError::InputRefused(
            EmbeddingInputRefusal::LocalCap { .. }
        ))
    ));
    *lock(&client.reply) = Reply::Busy(None);
    bound
        .embed("second provider failure")
        .await
        .expect_err("busy");
    let expected = policy.worker_interval() * 2;
    assert!(
        matches!(bound.embed("backed off").await,
            Err(LlmError::Retryable { retry_after: Some(delay), .. }) if delay == expected),
        "a local refusal did not contact the provider or heal its backoff"
    );
    assert_eq!(client.calls.load(Ordering::SeqCst), 3);
    *lock(&client.reply) = Reply::Accept;
    tokio::time::advance(expected).await;
    bound
        .embed("provider recovered")
        .await
        .expect("retry at bound");
    assert_eq!(client.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn paused_current_does_not_exclude_same_owner_next_or_another_original_in_same_space() {
    let control = EmbeddingControl::default();
    let owner = OwnerId::new(uuid::Uuid::now_v7());
    let other = OwnerId::new(uuid::Uuid::now_v7());
    let current = RecordingClient::new(
        "current",
        Reply::Configuration(EmbeddingConfigurationFault::Forbidden),
    );
    let next = RecordingClient::new("next", Reply::Accept);
    let route = control.observe(
        owner,
        EmbeddingRoute::moving(Some(bound(&current)), bound(&next)).expect("different spaces"),
        EmbeddingRuntimePolicy::default(),
    );
    let other_route = control.observe(
        other,
        EmbeddingRoute::current(bound(&RecordingClient::new("current", Reply::Accept))),
        EmbeddingRuntimePolicy::default(),
    );
    route
        .current_client()
        .expect("current")
        .embed("pause")
        .await
        .expect_err("configuration");
    route
        .next_client()
        .expect("next")
        .embed("healthy next")
        .await
        .expect("next is independent");
    other_route
        .current_client()
        .expect("current")
        .embed("separate original")
        .await
        .expect("same space is independent");
    assert_eq!(
        control.blocked_pairs(Instant::now()),
        [(owner, bound(&current).space().clone())]
    );
    assert!(
        control.blocked_owners(Instant::now()).is_empty(),
        "client faults never skip the whole owner"
    );
    assert_eq!(
        control.configuration_faults(),
        [EmbeddingConfigurationFault::Forbidden],
        "unrelated successes do not clear pause"
    );
}

#[tokio::test]
async fn a_wrong_count_or_non_finite_vector_is_retryable_and_input_refusal_does_not_pause() {
    for reply in [Reply::WrongCount, Reply::NonFinite] {
        let control = EmbeddingControl::default();
        let client = RecordingClient::new("malformed", reply);
        let owner = OwnerId::new(uuid::Uuid::now_v7());
        let route = control.observe(
            owner,
            EmbeddingRoute::current(bound(&client)),
            EmbeddingRuntimePolicy::default(),
        );
        let client_bound = route.current_client().expect("current");
        assert!(matches!(
            client_bound.embed_many(&["a".into(), "b".into()]).await,
            Err(LlmError::Retryable { status: None, .. })
        ));
        assert!(matches!(
            client_bound.embed("suppressed").await,
            Err(LlmError::Retryable { .. })
        ));
        assert_eq!(client.calls.load(Ordering::SeqCst), 1);
        assert!(control.configuration_faults().is_empty());
        assert_eq!(control.blocked_pairs(Instant::now()).len(), 1);
    }
    let control = EmbeddingControl::default();
    let client = RecordingClient::new("input", Reply::Input);
    let route = control.observe(
        OwnerId::new(uuid::Uuid::now_v7()),
        EmbeddingRoute::current(bound(&client)),
        EmbeddingRuntimePolicy::default(),
    );
    assert!(matches!(
        route
            .current_client()
            .expect("current")
            .embed("refused")
            .await,
        Err(LlmError::InputRefused(_))
    ));
    assert!(control.configuration_faults().is_empty());
    assert!(
        control.blocked_pairs(Instant::now()).is_empty(),
        "input rescue can proceed immediately"
    );
}

#[tokio::test]
async fn a_wrong_vector_width_pauses_the_client_for_background_work_and_queries() {
    let fault = EmbeddingConfigurationFault::VectorWidth {
        expected: 384,
        received: 383,
    };
    for background in [true, false] {
        let control = EmbeddingControl::default();
        let client = RecordingClient::new("wrong-width", Reply::WrongWidth);
        let route = control.observe(
            OwnerId::new(uuid::Uuid::now_v7()),
            EmbeddingRoute::current(bound(&client)),
            EmbeddingRuntimePolicy::default(),
        );
        let bound = route.current_client().expect("current");
        let first = if background {
            bound.embed_many(&["a".into(), "b".into()]).await.map(drop)
        } else {
            bound
                .embed_query("q", crate::embedding::QueryTask::DEFAULT)
                .await
                .map(drop)
        };
        assert!(
            matches!(first, Err(LlmError::Configuration(actual)) if actual == fault),
            "background={background}: {first:?}"
        );
        assert!(matches!(bound.embed("suppressed").await,
                Err(LlmError::Configuration(actual)) if actual == fault));
        assert!(
            matches!(
                bound.embed_query("suppressed", crate::embedding::QueryTask::DEFAULT).await,
                Err(LlmError::Configuration(actual)) if actual == fault
            ),
            "a paused client refuses queries too"
        );
        assert_eq!(client.calls.load(Ordering::SeqCst), 1);
        assert_eq!(control.configuration_faults(), [fault]);
    }
}

#[tokio::test]
async fn observing_a_wrapped_binding_replaces_the_engine_layer() {
    let control = EmbeddingControl::default();
    let owner = OwnerId::new(uuid::Uuid::now_v7());
    let client = RecordingClient::new("reobserved", Reply::Accept);
    let route = control.observe(
        owner,
        EmbeddingRoute::current(bound(&client)),
        EmbeddingRuntimePolicy::default(),
    );
    let route = control.observe(owner, route, EmbeddingRuntimePolicy::default());
    tokio::time::timeout(
        PATIENCE,
        route.current_client().expect("current").embed("one"),
    )
    .await
    .expect("re-observation must not nest the same gate")
    .expect("valid response");
    assert_eq!(client.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_refusals_keep_the_longest_wait_and_a_configuration_pause() {
    let start = Instant::now();
    let base = Duration::from_secs(5);
    for late in [
        LlmError::Retryable {
            status: Some(429),
            retry_after: Some(Duration::from_secs(1)),
            message: "busy".into(),
        },
        LlmError::Internal("unavailable".into()),
    ] {
        let mut state = SharedState::default();
        state.settle(
            &Err::<(), _>(LlmError::Retryable {
                status: Some(429),
                retry_after: Some(Duration::from_mins(1)),
                message: "busy".into(),
            }),
            Admission::Background,
            0,
            base,
            start,
        );
        state.settle(
            &Err::<(), _>(late),
            Admission::Background,
            0,
            base,
            start + Duration::from_secs(1),
        );
        assert!(
            matches!(state.refusal(Admission::Background, start + Duration::from_secs(59)), Some(LlmError::Retryable { retry_after: Some(remaining), .. }) if remaining == Duration::from_secs(1))
        );
        assert!(
            state
                .refusal(Admission::Background, start + Duration::from_mins(1))
                .is_none()
        );
    }
    let mut state = SharedState::default();
    state.settle(
        &Err::<(), _>(LlmError::Configuration(
            EmbeddingConfigurationFault::Unauthorized,
        )),
        Admission::Background,
        0,
        base,
        start,
    );
    state.settle(
        &Err::<(), _>(LlmError::retryable("late refusal")),
        Admission::Background,
        0,
        base,
        start,
    );
    assert!(matches!(
        state.refusal(Admission::Background, start + MAX_RETRY_AFTER),
        Some(LlmError::Configuration(
            EmbeddingConfigurationFault::Unauthorized
        ))
    ));
}

fn poll_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    future.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
}

fn busy(retry_after: Duration) -> LlmError {
    LlmError::Retryable {
        status: Some(429),
        retry_after: Some(retry_after),
        message: "busy".into(),
    }
}

#[derive(Debug)]
struct HeldWritesClient {
    entered: AtomicUsize,
    release: tokio::sync::Semaphore,
    queries: AtomicUsize,
}

#[async_trait::async_trait]
impl EmbeddingClient for HeldWritesClient {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.release.acquire().await.unwrap().forget();
        Ok(vec![1.0; 384])
    }
    async fn embed_query(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        Ok(vec![1.0; 384])
    }
    fn model_id(&self) -> &'static str {
        "held-writes"
    }
    fn dim(&self) -> usize {
        384
    }
}

#[tokio::test]
async fn calls_reach_the_provider_concurrently_and_a_query_waits_for_none() {
    const WRITES: usize = 6;
    let client = Arc::new(HeldWritesClient {
        entered: AtomicUsize::new(0),
        release: tokio::sync::Semaphore::new(0),
        queries: AtomicUsize::new(0),
    });
    let control = EmbeddingControl::default();
    let route = control.observe(
        OwnerId::new(uuid::Uuid::now_v7()),
        EmbeddingRoute::current(
            BoundEmbeddingClient::bind(Arc::clone(&client) as Arc<dyn EmbeddingClient>).unwrap(),
        ),
        EmbeddingRuntimePolicy::default(),
    );
    let bound = route.current_client().unwrap();
    let mut writes: Vec<_> = (0..WRITES)
        .map(|_| Box::pin(bound.embed("write")))
        .collect();
    for write in &mut writes {
        assert!(poll_once(write.as_mut()).is_pending());
    }
    assert_eq!(
        client.entered.load(Ordering::SeqCst),
        WRITES,
        "every write is at the provider; none waits in the engine"
    );
    tokio::time::timeout(
        PATIENCE,
        bound.embed_query("query", crate::embedding::QueryTask::DEFAULT),
    )
    .await
    .expect("the query is not queued behind held writes")
    .expect("query vector");
    assert_eq!(
        client.queries.load(Ordering::SeqCst),
        1,
        "a search goes out as a query"
    );
    client.release.add_permits(WRITES);
    for write in writes {
        tokio::time::timeout(PATIENCE, write)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_query_passes_a_background_backoff_without_moving_it_and_stops_at_a_pause() {
    let control = ClientControl::default();
    let policy = EmbeddingRuntimePolicy::default();
    let calls = AtomicUsize::new(0);
    let provider = |reply: Result<(), LlmError>| {
        let calls = &calls;
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            reply
        }
    };
    let backed_off = |result: Result<(), LlmError>, expected: Duration| matches!(result, Err(LlmError::Retryable { retry_after: Some(remaining), .. }) if remaining == expected);
    control
        .call(
            policy,
            Admission::Background,
            provider(Err(busy(Duration::from_mins(1)))),
        )
        .await
        .expect_err("busy");

    control
        .call(policy, Admission::Query, provider(Ok(())))
        .await
        .expect("a query passes the backoff");
    assert!(
        backed_off(
            control
                .call(policy, Admission::Background, provider(Ok(())))
                .await,
            Duration::from_mins(1)
        ),
        "a query's success does not heal the background backoff"
    );
    control
        .call(
            policy,
            Admission::Query,
            provider(Err(busy(Duration::from_hours(1)))),
        )
        .await
        .expect_err("the query's own refusal goes to its caller");
    assert!(
        backed_off(
            control
                .call(policy, Admission::Background, provider(Ok(())))
                .await,
            Duration::from_mins(1)
        ),
        "a query's refusal does not extend the background backoff"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "refused calls never reach the provider"
    );

    control
        .call(
            policy,
            Admission::Query,
            provider(Err(LlmError::Configuration(
                EmbeddingConfigurationFault::Unauthorized,
            ))),
        )
        .await
        .expect_err("configuration");
    for admission in [Admission::Background, Admission::Query] {
        assert!(matches!(
            control.call(policy, admission, provider(Ok(()))).await,
            Err(LlmError::Configuration(
                EmbeddingConfigurationFault::Unauthorized
            ))
        ));
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "a query's configuration fault pauses every call"
    );
}

#[tokio::test(start_paused = true)]
async fn a_late_success_cannot_clear_a_concurrent_refusal() {
    for refusal in [
        busy(Duration::from_mins(1)),
        LlmError::Configuration(EmbeddingConfigurationFault::Unauthorized),
    ] {
        let pause = matches!(refusal, LlmError::Configuration(_));
        let control = ClientControl::default();
        let policy = EmbeddingRuntimePolicy::default();
        let (release, held) = tokio::sync::oneshot::channel();
        let mut in_flight = Box::pin(control.call(policy, Admission::Background, async {
            held.await.unwrap();
            Ok(())
        }));
        assert!(poll_once(in_flight.as_mut()).is_pending());
        control
            .call(policy, Admission::Background, async {
                Err::<(), _>(refusal)
            })
            .await
            .expect_err("refused");
        release.send(()).unwrap();
        in_flight.await.expect("the earlier call succeeds");
        let result = control
            .call(policy, Admission::Background, async { Ok(()) })
            .await;
        if pause {
            assert!(matches!(result, Err(LlmError::Configuration(_))));
        } else {
            assert!(
                matches!(result, Err(LlmError::Retryable { retry_after: Some(remaining), .. }) if remaining == Duration::from_mins(1))
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_provider_call_past_the_request_deadline_backs_the_client_off() {
    let control = ClientControl::default();
    let policy = EmbeddingRuntimePolicy::default();
    let started = Instant::now();
    let error = control
        .call(
            policy,
            Admission::Background,
            std::future::pending::<Result<(), LlmError>>(),
        )
        .await
        .expect_err("deadline");
    assert!(matches!(
        error,
        LlmError::Retryable {
            status: None,
            retry_after: None,
            ..
        }
    ));
    assert_eq!(
        Instant::now().saturating_duration_since(started),
        policy.request_timeout()
    );
    let state = lock(&control.state);
    let ClientState::Waiting(wait) = state.value else {
        panic!("a provider timeout must publish backoff: {state:?}");
    };
    assert_eq!(wait.delay, policy.worker_interval());
}
