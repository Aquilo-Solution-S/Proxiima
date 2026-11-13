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
    fn once(
        &mut self,
        _gate: &GateWorker,
        _cancel: &CancellationToken,
    ) -> impl Future<Output = Pass> {
        self.starts.lock().unwrap().push(Instant::now());
        std::future::ready(self.script.next().unwrap_or(Pass::Idle))
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
    fn once(
        &mut self,
        _gate: &GateWorker,
        _cancel: &CancellationToken,
    ) -> impl Future<Output = Pass> {
        std::future::poll_fn(|_| panic!("a pass that panics"))
    }
}

struct Counts(Arc<AtomicUsize>);

impl WorkerPass for Counts {
    fn once(
        &mut self,
        _gate: &GateWorker,
        _cancel: &CancellationToken,
    ) -> impl Future<Output = Pass> {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Pass::Idle)
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
    let supervisor = workers.supervise(cancel.clone(), ended.clone(), Metrics::unobserved());

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
fn the_readiness_line_lists_each_label_once_in_order_with_its_count() {
    let ended = EndedWorkers::default();
    assert_eq!(ended.line(), None, "no line while every worker runs");
    ended.record("embedding-drain");
    ended.record("outbox-publisher");
    ended.record("outbox-publisher");
    assert_eq!(
        ended.line().as_deref(),
        Some("ended-workers: embedding-drain, outbox-publisher x2")
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
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
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

/// Two outbox drains end: the log names each consumer, the probe line
/// holds the class label with their count and no consumer name.
#[tokio::test]
async fn an_ended_drain_is_logged_with_its_consumer_and_listed_by_label() {
    let captured = captured();
    let ended = EndedWorkers::default();
    let cancel = CancellationToken::new();
    let mut workers = Workers::default();
    for consumer in ["billing-export", "search-index"] {
        workers.spawn("outbox-publisher", Some(consumer.to_owned()), async {});
    }
    let supervisor = workers.supervise(cancel.clone(), ended.clone(), Metrics::unobserved());
    supervisor.await.expect("both drains have ended");

    assert_eq!(
        ended.line().as_deref(),
        Some("ended-workers: outbox-publisher x2")
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
