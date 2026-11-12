//! Background workers: the one loop every runtime feature worker runs
//! ([`run`]), the set the runtime owns them in ([`Workers`]), and the
//! spiice-contributed workers that join the same set.
//!
//! A worker that ends before the runtime is cancelled, by a panic or by
//! returning, is logged as an error when it ends and listed on `/readyz`
//! ([`EndedWorkers`]). Serving and every other worker continue; nothing
//! restarts it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proxiima_core::quiesce::{Checkpoint, GateWorker};
use proxiima_core::write_wake::WriteWake;
use proxiima_core::{Engine, SpiiceServices};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

/// What one pass of a worker found, which decides how long the worker waits
/// before the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pass {
    /// It found work, so more may be waiting: the next pass starts at once,
    /// and both waits fall back to the floor.
    Worked,
    /// It found nothing: wait, twice as long after each idle pass in a row,
    /// up to [`Backoff::idle_max`].
    Idle,
    /// It failed: wait, twice as long after each failed pass in a row, up to
    /// [`Backoff::failed_max`].
    Failed,
}

/// Which waits a write committed in this process ends.
#[derive(Debug)]
pub(crate) enum Wake {
    /// None: only cancel, a gate change or the wait itself ends a wait.
    Never,
    /// The wait after an idle pass, never the wait after a failed one: a
    /// write does not hurry a retry against a failing dependency.
    OnIdle(WriteWake),
}

impl Wake {
    /// Resolves when a committed write ends the wait that follows `after`;
    /// never, when this wake does not end that wait.
    async fn changed(&mut self, after: Pass) {
        match (self, after) {
            (Self::OnIdle(writes), Pass::Idle) => writes.changed().await,
            (Self::Never, Pass::Worked | Pass::Idle | Pass::Failed)
            | (Self::OnIdle(_), Pass::Worked | Pass::Failed) => std::future::pending().await,
        }
    }
}

/// The bounds of a worker's waits between passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Backoff {
    /// The first wait after an idle pass, and the least wait after a failed
    /// one.
    pub(crate) floor: Duration,
    /// The longest wait between idle passes.
    pub(crate) idle_max: Duration,
    /// The longest wait between failed passes.
    pub(crate) failed_max: Duration,
}

impl Backoff {
    /// The same wait after every pass that did not find work.
    pub(crate) const fn fixed(interval: Duration) -> Self {
        Self {
            floor: interval,
            idle_max: interval,
            failed_max: interval,
        }
    }

    /// Double `current`, up to `ceiling`.
    ///
    /// `min` then `max`, never `clamp`: `Ord::clamp` PANICS when `min > max`,
    /// and a floor above the ceiling must not end the worker. A floor above
    /// the ceiling wins.
    fn next(self, current: Duration, ceiling: Duration) -> Duration {
        current.saturating_mul(2).min(ceiling).max(self.floor)
    }
}

/// One worker's work, run once per pass by [`run`].
pub(crate) trait WorkerPass: Send {
    /// Do one round of work and say what it found.
    ///
    /// The gate was open when the pass started; a pass that holds a batch
    /// checks [`GateWorker::is_closing`] between records and hands the rest
    /// back. `cancel` is the worker's own: a pass that does not observe it
    /// finishes before the worker ends.
    fn once(
        &mut self,
        gate: &GateWorker,
        cancel: &CancellationToken,
    ) -> impl Future<Output = Pass> + Send;
}

/// The one worker loop: pass, wait, pass, until `cancel`.
///
/// Before every pass the worker stops at `gate`'s checkpoint, where a
/// quiesce parks it holding nothing. After a pass that did not find work it
/// waits as [`Pass`] and `backoff` say; the wait ends early on cancel, on a
/// gate change (a quiesce or a resume), and on a write committed in this
/// process when `wake` says so for that wait. Cancel always ends the
/// worker between passes; whether a pass itself observes it is the pass's
/// business.
pub(crate) async fn run(
    mut gate: GateWorker,
    cancel: CancellationToken,
    mut wake: Wake,
    backoff: Backoff,
    mut pass: impl WorkerPass,
) {
    let mut idle_wait = backoff.floor;
    let mut failed_wait = backoff.floor;
    loop {
        if gate.checkpoint(cancel.cancelled()).await == Checkpoint::Cancelled {
            return;
        }
        let outcome = pass.once(&gate, &cancel).await;
        let wait = match outcome {
            Pass::Worked => {
                idle_wait = backoff.floor;
                failed_wait = backoff.floor;
                continue;
            }
            Pass::Idle => {
                failed_wait = backoff.floor;
                let wait = idle_wait;
                idle_wait = backoff.next(idle_wait, backoff.idle_max);
                wait
            }
            Pass::Failed => {
                idle_wait = backoff.floor;
                failed_wait = backoff.next(failed_wait, backoff.failed_max);
                failed_wait
            }
        };
        tokio::select! {
            () = cancel.cancelled() => return,
            () = gate.changed() => {}
            () = wake.changed(outcome) => idle_wait = backoff.floor,
            () = tokio::time::sleep(wait) => {}
        }
    }
}

/// The labels of the workers that ended before the runtime was cancelled,
/// in the order they ended, one entry per ended worker. Written by the
/// [`Workers`] supervisor, read by the anonymous `/readyz`.
///
/// It holds only labels fixed in the code, never a `String`, so no
/// operator-chosen name (an outbox consumer, say) can reach the probe.
#[derive(Debug, Clone, Default)]
pub(crate) struct EndedWorkers(Arc<Mutex<Vec<&'static str>>>);

impl EndedWorkers {
    fn record(&self, label: &'static str) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(label);
    }

    /// The readiness line, `ended-workers: <label>, <label>`, a label once
    /// per ended worker; `None` while every worker runs.
    pub(crate) fn line(&self) -> Option<String> {
        let ended = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (!ended.is_empty()).then(|| format!("ended-workers: {}", ended.join(", ")))
    }
}

/// How the supervisor names one worker: a label fixed in the code, which
/// `/readyz` may show, and the instance detail, which only the log shows.
#[derive(Debug)]
struct WorkerName {
    label: &'static str,
    instance: Option<String>,
}

/// Every worker the runtime started, each under its name, in one
/// [`JoinSet`].
#[derive(Debug, Default)]
pub(crate) struct Workers {
    running: JoinSet<()>,
    names: HashMap<tokio::task::Id, WorkerName>,
}

impl Workers {
    /// Spawn `work` as a worker of class `label`. `instance` tells two
    /// workers of one class apart in the log (an outbox consumer's name);
    /// it never reaches `/readyz`.
    pub(crate) fn spawn(
        &mut self,
        label: &'static str,
        instance: Option<String>,
        work: impl Future<Output = ()> + Send + 'static,
    ) {
        let id = self.running.spawn(work).id();
        self.names.insert(id, WorkerName { label, instance });
    }

    /// Adopt a worker a spiice already spawned, labelled by the static name
    /// its declaration carries: it ends, and panics, in this set as its own
    /// task does.
    pub(crate) fn adopt(&mut self, worker: SpiiceWorker) {
        let SpiiceWorker { name, handle } = worker;
        self.spawn(name, None, async move {
            if let Err(error) = handle.await
                && let Ok(panic) = error.try_into_panic()
            {
                std::panic::resume_unwind(panic);
            }
        });
    }

    /// Watch every worker until all have ended. One that ends before
    /// `cancel` is logged as an error at once and its label recorded in
    /// `ended`; the others keep running. The returned task finishes once
    /// every worker has ended, so a caller that cancelled `cancel` awaits it
    /// to join them.
    pub(crate) fn supervise(
        self,
        cancel: CancellationToken,
        ended: EndedWorkers,
    ) -> JoinHandle<()> {
        let Self {
            mut running,
            mut names,
        } = self;
        tokio::spawn(async move {
            while let Some(joined) = running.join_next_with_id().await {
                let (id, failure) = match joined {
                    Ok((id, ())) => (id, None),
                    Err(error) => (error.id(), Some(error)),
                };
                let WorkerName { label, instance } = names.remove(&id).unwrap_or(WorkerName {
                    label: "unnamed-worker",
                    instance: Some(id.to_string()),
                });
                match (cancel.is_cancelled(), failure) {
                    (false, failure) => {
                        let reason = failure
                            .map_or_else(|| "returned".to_owned(), |error| error.to_string());
                        tracing::error!(
                            worker = label,
                            instance = instance.as_deref(),
                            reason = %reason,
                            "a background worker ended before shutdown; serving continues \
                             without it, and /readyz lists it until the process restarts"
                        );
                        ended.record(label);
                    }
                    (true, Some(error)) => {
                        tracing::warn!(
                            worker = label,
                            instance = instance.as_deref(),
                            error = %error,
                            "worker join failed"
                        );
                    }
                    (true, None) => {}
                }
            }
        })
    }
}

/// Runtime handles passed to [`SpiiceBundle::spawn_workers`].
///
/// The `cancel` token is an observation of the serving runtime's
/// shutdown, not a control over it: it is a child of the runtime's own
/// token, so it fires when the runtime shuts down, but cancelling it
/// only reaches this context's clones. Every worker spawned from this
/// context MUST observe it and terminate when it is cancelled.
///
/// `service::<T>()` resolves from the same composed service set used by MCP
/// and REST tools. `CitedBlobService` and `CitedBlobReadService` are absent
/// unless the host configured S3, so a worker that needs either must fail its
/// job typed rather than silently no-op. A queued worker stores only a
/// [`DelegationId`](proxiima_core::DelegationId), resolves the shared
/// [`DelegatedAuthorityService`](proxiima_core::DelegatedAuthorityService),
/// and redeems a non-cloneable
/// [`DelegatedPhase`](proxiima_core::DelegatedPhase) at job claim and at each
/// subsequent phase boundary. The service is absent when the host has no
/// authenticator.
/// Delegated-capable Engine methods and the bound `CitedBlobService` /
/// `CitedBlobReadService` accept that phase; raw delegated `AuthzContext`
/// values are rejected. `collect_verified` requires a non-zero
/// byte ceiling and returns bytes only after length, BLAKE3, and SHA-256
/// verification. Owner reconciliation is not delegated-capable.
///
/// [`SpiiceBundle::spawn_workers`]: crate::spiice::SpiiceBundle::spawn_workers
#[derive(Clone)]
pub struct SpiiceWorkerContext {
    pub engine: Arc<Engine>,
    pub cancel: CancellationToken,
    pub(crate) services: SpiiceServices,
}

impl SpiiceWorkerContext {
    #[must_use]
    pub fn service<T>(&self) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        self.services.get::<T>()
    }
}

#[cfg(any(test, feature = "testkit"))]
impl SpiiceWorkerContext {
    /// Test-only context for exercising a bundle's `spawn_workers`
    /// without booting the serving runtime. Services start empty; attach the
    /// exact test set with [`with_services`](Self::with_services).
    #[must_use]
    pub fn new_for_tests(engine: Arc<Engine>, cancel: CancellationToken) -> Self {
        Self {
            engine,
            cancel,
            services: SpiiceServices::default(),
        }
    }

    /// Attach the composed service set to a test context.
    #[must_use]
    pub fn with_services(mut self, services: SpiiceServices) -> Self {
        self.services = services;
        self
    }
}

impl std::fmt::Debug for SpiiceWorkerContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpiiceWorkerContext")
            .field("cancel", &self.cancel)
            .field("services", &self.services)
            .finish_non_exhaustive()
    }
}

/// One spawned spiice worker: a join handle plus the name the runtime logs
/// and lists on `/readyz` when it ends before shutdown.
#[derive(Debug)]
pub struct SpiiceWorker {
    pub name: &'static str,
    pub handle: tokio::task::JoinHandle<()>,
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use proxiima_core::QuiesceGate;
    use tokio::time::Instant;

    use super::*;

    /// When each pass started.
    type Starts = Arc<Mutex<Vec<Instant>>>;

    /// A pass that notes when it starts and answers from its script, then
    /// [`Pass::Idle`] once the script runs out.
    struct Scripted {
        script: std::vec::IntoIter<Pass>,
        starts: Starts,
    }

    impl WorkerPass for Scripted {
        async fn once(&mut self, _gate: &GateWorker, _cancel: &CancellationToken) -> Pass {
            self.starts.lock().unwrap().push(Instant::now());
            self.script.next().unwrap_or(Pass::Idle)
        }
    }

    fn scripted(script: Vec<Pass>, starts: &Starts) -> Scripted {
        Scripted {
            script: script.into_iter(),
            starts: starts.clone(),
        }
    }

    struct Panics;

    impl WorkerPass for Panics {
        async fn once(&mut self, _gate: &GateWorker, _cancel: &CancellationToken) -> Pass {
            panic!("a pass that panics")
        }
    }

    struct Counts(Arc<AtomicUsize>);

    impl WorkerPass for Counts {
        async fn once(&mut self, _gate: &GateWorker, _cancel: &CancellationToken) -> Pass {
            self.0.fetch_add(1, Ordering::SeqCst);
            Pass::Idle
        }
    }

    /// Wait (in virtual time) until `starts` holds `count` passes. Each
    /// pass notes its own start, so the poll's step does not blur a gap.
    async fn passes(starts: &Starts, count: usize) -> Vec<Instant> {
        tokio::time::timeout(Duration::from_hours(24), async {
            loop {
                let seen = starts.lock().unwrap().clone();
                if seen.len() >= count {
                    return seen;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{count} passes"))
    }

    /// The gaps between consecutive pass starts, in whole seconds.
    fn gaps(starts: &[Instant]) -> Vec<u64> {
        starts
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).as_secs())
            .collect()
    }

    const HOUR: Backoff = Backoff::fixed(Duration::from_hours(1));

    #[tokio::test(start_paused = true)]
    async fn cancel_ends_a_waiting_worker() {
        let starts = Starts::default();
        let (_control, gate) = QuiesceGate::new();
        let cancel = CancellationToken::new();
        let started = Instant::now();
        let worker = tokio::spawn(run(
            gate,
            cancel.clone(),
            Wake::Never,
            HOUR,
            scripted(Vec::new(), &starts),
        ));
        passes(&starts, 1).await;
        cancel.cancel();
        worker.await.expect("the worker ends");
        assert!(
            started.elapsed() < HOUR.floor,
            "it did not wait out its sleep"
        );
        assert_eq!(starts.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_gate_change_starts_a_pass() {
        let starts = Starts::default();
        let (control, gate) = QuiesceGate::new();
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(run(
            gate,
            cancel.clone(),
            Wake::Never,
            HOUR,
            scripted(Vec::new(), &starts),
        ));
        let first = passes(&starts, 1).await[0];
        control.quiesce().await.expect("the idle worker parks");
        control.resume().expect("and resumes");
        let second = passes(&starts, 2).await[1];
        assert!(
            second - first < HOUR.floor,
            "the resume, not the sleep, ran it"
        );
        cancel.cancel();
        worker.await.expect("the worker ends");
    }

    #[tokio::test(start_paused = true)]
    async fn a_committed_write_starts_a_pass_after_an_idle_one() {
        let starts = Starts::default();
        let (_control, gate) = QuiesceGate::new();
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(run(
            gate,
            cancel.clone(),
            Wake::OnIdle(WriteWake::subscribe()),
            HOUR,
            scripted(Vec::new(), &starts),
        ));
        let first = passes(&starts, 1).await[0];
        proxiima_core::write_wake::write_committed();
        let second = passes(&starts, 2).await[1];
        assert!(
            second - first < HOUR.floor,
            "the write, not the sleep, ran it"
        );
        cancel.cancel();
        worker.await.expect("the worker ends");
    }

    #[tokio::test(start_paused = true)]
    async fn a_committed_write_does_not_end_the_wait_after_a_failed_pass() {
        let starts = Starts::default();
        let (_control, gate) = QuiesceGate::new();
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(run(
            gate,
            cancel.clone(),
            Wake::OnIdle(WriteWake::subscribe()),
            HOUR,
            scripted(vec![Pass::Failed], &starts),
        ));
        let first = passes(&starts, 1).await[0];
        proxiima_core::write_wake::write_committed();
        let second = passes(&starts, 2).await[1];
        assert_eq!(
            second - first,
            HOUR.floor,
            "the retry waited out its interval"
        );
        cancel.cancel();
        worker.await.expect("the worker ends");
    }

    #[tokio::test(start_paused = true)]
    async fn idle_passes_back_off_to_their_bound_and_work_resets_it() {
        use Pass::{Idle, Worked};
        let starts = Starts::default();
        let (_control, gate) = QuiesceGate::new();
        let cancel = CancellationToken::new();
        let backoff = Backoff {
            floor: Duration::from_secs(1),
            idle_max: Duration::from_secs(4),
            failed_max: Duration::from_hours(1),
        };
        let script = vec![Idle, Idle, Idle, Idle, Idle, Worked, Idle, Idle];
        let worker = tokio::spawn(run(
            gate,
            cancel.clone(),
            Wake::Never,
            backoff,
            scripted(script, &starts),
        ));
        let seen = passes(&starts, 9).await;
        cancel.cancel();
        worker.await.expect("the worker ends");
        assert_eq!(gaps(&seen[..9]), [1, 2, 4, 4, 4, 0, 1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_passes_back_off_to_their_own_bound() {
        use Pass::{Failed, Idle};
        let starts = Starts::default();
        let (_control, gate) = QuiesceGate::new();
        let cancel = CancellationToken::new();
        let backoff = Backoff {
            floor: Duration::from_secs(1),
            idle_max: Duration::from_hours(1),
            failed_max: Duration::from_secs(4),
        };
        let script = vec![Failed, Failed, Failed, Idle, Failed];
        let worker = tokio::spawn(run(
            gate,
            cancel.clone(),
            Wake::Never,
            backoff,
            scripted(script, &starts),
        ));
        let seen = passes(&starts, 6).await;
        cancel.cancel();
        worker.await.expect("the worker ends");
        // A failure waits twice the floor at first; an idle pass between
        // failures starts both waits over.
        assert_eq!(gaps(&seen[..6]), [2, 4, 4, 1, 2]);
    }

    #[test]
    fn the_backoff_doubles_to_the_ceiling_and_a_higher_floor_wins() {
        let poll = Duration::from_millis(500);
        let ceiling = Duration::from_secs(30);
        let backoff = Backoff {
            floor: poll,
            idle_max: ceiling,
            failed_max: ceiling,
        };
        assert_eq!(backoff.next(poll, ceiling), Duration::from_secs(1));
        assert_eq!(
            backoff.next(Duration::from_secs(20), ceiling),
            Duration::from_secs(30)
        );
        let slow = Backoff::fixed(Duration::from_mins(1));
        assert_eq!(
            slow.next(slow.floor, ceiling),
            slow.floor,
            "a floor over the ceiling wins"
        );
    }

    /// A worker that panics or returns before cancel is recorded by name at
    /// once; the others keep running their passes; cancel still joins them.
    #[tokio::test(start_paused = true)]
    async fn a_worker_that_ends_early_is_recorded_and_the_others_keep_running() {
        let ended = EndedWorkers::default();
        let cancel = CancellationToken::new();
        let counted = Arc::new(AtomicUsize::new(0));
        let mut workers = Workers::default();
        let (_panics_control, panics_gate) = QuiesceGate::new();
        workers.spawn(
            "panics",
            None,
            run(panics_gate, cancel.clone(), Wake::Never, HOUR, Panics),
        );
        let (_counts_control, counts_gate) = QuiesceGate::new();
        workers.spawn(
            "counts",
            None,
            run(
                counts_gate,
                cancel.clone(),
                Wake::Never,
                Backoff::fixed(Duration::from_secs(1)),
                Counts(counted.clone()),
            ),
        );
        workers.spawn("returns", None, async {});
        workers.spawn("until-cancelled", None, {
            let cancel = cancel.clone();
            async move { cancel.cancelled().await }
        });
        let supervisor = workers.supervise(cancel.clone(), ended.clone());

        tokio::time::timeout(Duration::from_secs(1), async {
            while ended.0.lock().unwrap().len() < 2 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("both early ends are recorded while the others run");
        let mut names = ended.0.lock().unwrap().clone();
        names.sort_unstable();
        assert_eq!(names, ["panics", "returns"]);

        let before = counted.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            counted.load(Ordering::SeqCst) > before,
            "the counting worker keeps running passes"
        );

        cancel.cancel();
        supervisor.await.expect("the supervisor joins every worker");
        assert_eq!(
            ended.0.lock().unwrap().len(),
            2,
            "workers that end on cancel are not listed"
        );
    }

    #[test]
    fn the_readiness_line_lists_one_label_per_ended_worker_in_order() {
        let ended = EndedWorkers::default();
        assert_eq!(ended.line(), None, "no line while every worker runs");
        ended.record("embedding-drain");
        ended.record("outbox-publisher");
        ended.record("outbox-publisher");
        assert_eq!(
            ended.line().as_deref(),
            Some("ended-workers: embedding-drain, outbox-publisher, outbox-publisher")
        );
    }

    /// Every event of this test binary as `(thread, "field=value ... level=LEVEL")`.
    /// Global, not `set_default`: a thread-scoped dispatcher races the
    /// callsite interest cache against tests on other threads.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<(std::thread::ThreadId, String)>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Captured {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Fields(String);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write as _;
                    write!(&mut self.0, "{}={value:?} ", field.name()).expect("string write");
                }
            }
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            {
                use std::fmt::Write as _;
                write!(&mut fields.0, "level={}", event.metadata().level()).expect("string write");
            }
            self.0
                .lock()
                .unwrap()
                .push((std::thread::current().id(), fields.0));
        }
    }

    fn captured() -> &'static Captured {
        static CAPTURED: std::sync::OnceLock<Captured> = std::sync::OnceLock::new();
        CAPTURED.get_or_init(|| {
            use tracing_subscriber::prelude::*;
            let captured = Captured::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(captured.clone()),
            )
            .expect("only this test installs a subscriber in this binary");
            captured
        })
    }

    /// Two outbox drains end: the log names each consumer, the probe record
    /// holds the class label once per drain and no consumer name.
    #[tokio::test]
    async fn an_ended_drain_is_logged_with_its_consumer_and_listed_by_label() {
        let captured = captured();
        let ended = EndedWorkers::default();
        let cancel = CancellationToken::new();
        let mut workers = Workers::default();
        for consumer in ["billing-export", "search-index"] {
            workers.spawn("outbox-publisher", Some(consumer.to_owned()), async {});
        }
        let supervisor = workers.supervise(cancel.clone(), ended.clone());
        supervisor.await.expect("both drains have ended");

        assert_eq!(
            ended.line().as_deref(),
            Some("ended-workers: outbox-publisher, outbox-publisher")
        );
        let thread = std::thread::current().id();
        let logged: Vec<String> = captured
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(from, line)| *from == thread && line.contains("worker=\"outbox-publisher\""))
            .map(|(_, line)| line.clone())
            .collect();
        assert_eq!(logged.len(), 2, "{logged:?}");
        for consumer in ["billing-export", "search-index"] {
            assert!(
                logged
                    .iter()
                    .any(|line| line.contains(&format!("instance=\"{consumer}\""))
                        && line.contains("reason=returned")
                        && line.ends_with("level=ERROR")),
                "{logged:?}"
            );
        }
    }
}
