//! The outbox drain for one host-provided intake and its consumer lane.

use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
#[cfg(test)]
use proxiima_core::QuiesceGate;
use proxiima_core::publication::intake::{DurableIntake, ReceivedEvent};
use proxiima_core::quiesce::GateWorker;
#[cfg(test)]
use proxiima_core::storage_ports::publication::ClaimToken;
use proxiima_core::storage_ports::publication::{
    AckOutcome, ClaimedPublication, ConsumerName, PublicationOutboxPort, PublisherId,
    ReleaseOutcome,
};
use proxiima_core::write_wake::WriteWake;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::workers::{Backoff, Pass, Wake, WorkerPass};

/// The longest a failing drain waits between passes, and the shipped
/// [`Settings::idle_poll_max`].
const MAX_BACKOFF: Duration = Duration::from_secs(proxiima_core::ledger!(
    Proxiima.Publication.pollCeilingSeconds
));

/// How one drain claims and delivers.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// Names this drain on every claim it holds.
    pub(crate) publisher_id: PublisherId,
    /// Records leased per pass.
    pub(crate) batch: NonZeroU32,
    /// How long a claim holds a record before it becomes deliverable again.
    /// The storage port refuses a lease under one second.
    pub(crate) lease: Duration,
    /// How long a drain waits after its first empty claim; also the floor of
    /// the backoff after a failed pass. Each further empty claim doubles the
    /// wait, up to `idle_poll_max`; a committed write or a claim that finds
    /// work resets it.
    pub(crate) poll_interval: Duration,
    /// The longest an idle drain sleeps between claims when no write commits
    /// in this process: how late it finds work enqueued by another process.
    pub(crate) idle_poll_max: Duration,
    /// How long one `accept` may take before its claim is released.
    pub(crate) accept_timeout: Duration,
}

impl Settings {
    /// The shipped values. The label is `proxiima:<pid>`: an operator's hint
    /// for who holds a claim, not an identity (the claim token fences).
    pub(crate) fn new() -> Self {
        Self {
            publisher_id: PublisherId::new(format!("proxiima:{}", std::process::id()))
                .expect("`proxiima:<pid>` is a valid publisher label"),
            batch: NonZeroU32::new(proxiima_core::ledger!(Proxiima.Publication.drainBatch))
                .expect("positive claim batch"),
            lease: Duration::from_secs(proxiima_core::ledger!(
                Proxiima.Publication.drainLeaseSeconds
            )),
            // Crate tests compile this with `cfg(test)` and must not sit
            // out the shipped 500 ms backoff between failed accepts.
            poll_interval: if cfg!(test) {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(proxiima_core::ledger!(Proxiima.Publication.pollFloorMillis))
            },
            idle_poll_max: MAX_BACKOFF,
            accept_timeout: Duration::from_secs(proxiima_core::ledger!(
                Proxiima.Publication.intakeTimeoutSeconds
            )),
        }
    }
}

/// Seam between "the intake accepted it" and "the outbox knows".
///
/// It exists because that window is the one this design cannot test any other
/// way: a crash after `accept` and before the mark is the failure the whole
/// delivery contract is built to survive, and it lives inside a single
/// `await` a test cannot otherwise interrupt. It is a real, always-compiled
/// dispatch rather than a `cfg(test)` branch: a fault path production never
/// runs is a fault path production never checked.
#[async_trait::async_trait]
pub(crate) trait AfterAccept: Send + Sync + std::fmt::Debug {
    /// Called with the event id once `accept` returned, before the record is
    /// marked published. `Break` behaves as though the process died here: the
    /// pass is abandoned at once, leaving this record and every unattempted
    /// claim leased.
    async fn after_accept(&self, event_id: &str) -> ControlFlow<()>;
}

/// The shipped hook: carry on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Proceed;

#[async_trait::async_trait]
impl AfterAccept for Proceed {
    async fn after_accept(&self, _event_id: &str) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }
}

/// Why one record, or a whole pass, delivered nothing. Only the category is
/// logged: the intake's own words stay in the intake.
#[derive(Debug, Clone, Copy)]
enum Failure {
    /// The outbox could not claim, mark or release.
    Storage,
    /// The captured bytes are not a `CloudEvents` envelope.
    Malformed,
    /// The intake could not make an outcome durable.
    Intake,
    /// `accept` did not return within the timeout.
    TimedOut,
    /// `accept` panicked.
    Panicked,
}

/// What became of one record that did not fail.
enum Flow {
    /// The intake made an outcome durable; the mark is still owed.
    Accepted { flush_at: Instant },
    /// The claim had moved on: another drain owns the record now.
    Stale,
    /// The hook asked to behave as though the process died here.
    Abort,
    /// Shutdown or a quiesce: hand back everything this pass still holds.
    HandBack,
}

/// Durable decisions awaiting a mark, with the oldest claim’s deadline.
#[derive(Default)]
struct Accepted<'a> {
    records: Vec<&'a ClaimedPublication>,
    deadline: Option<Instant>,
}

impl<'a> Accepted<'a> {
    fn push(&mut self, record: &'a ClaimedPublication, flush_at: Instant) {
        self.deadline.get_or_insert(flush_at);
        self.records.push(record);
    }

    fn take(&mut self) -> Vec<&'a ClaimedPublication> {
        self.deadline = None;
        std::mem::take(&mut self.records)
    }
}

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[cfg_attr(test, serde(rename_all = "camelCase"))]
enum ClaimBoundary {
    Opened,
    Closed,
    Cancelled,
}

impl ClaimBoundary {
    fn observe(gate: &GateWorker, cancel: &CancellationToken) -> Self {
        if cancel.is_cancelled() {
            Self::Cancelled
        } else if gate.is_closing() {
            Self::Closed
        } else {
            Self::Opened
        }
    }

    // `Proxiima.Publication.handBack`.
    fn hand_back(self) -> bool {
        match self {
            Self::Cancelled => true,
            Self::Opened | Self::Closed => false,
        }
    }
}

/// The claim → accept → mark loop over one consumer's lanes and its intake.
pub(crate) struct Drain {
    outbox: Arc<dyn PublicationOutboxPort>,
    consumer: ConsumerName,
    intake: Arc<dyn DurableIntake>,
    settings: Settings,
    hook: Arc<dyn AfterAccept>,
}

impl Drain {
    pub(crate) fn new(
        outbox: Arc<dyn PublicationOutboxPort>,
        consumer: ConsumerName,
        intake: Arc<dyn DurableIntake>,
        settings: Settings,
        hook: Arc<dyn AfterAccept>,
    ) -> Self {
        Self {
            outbox,
            consumer,
            intake,
            settings,
            hook,
        }
    }

    /// Drain until cancelled: claim again at once while claims return work
    /// or a write commits in this process, back off while claims come back
    /// empty, and back off while the intake is unhappy.
    ///
    /// Never panics and never returns an error: a drain that stopped on an
    /// intake outage would turn a recoverable delivery pause into a silent
    /// permanent one, and capture keeps running either way.
    pub(crate) async fn run(self, cancel: CancellationToken, gate: GateWorker) {
        self.run_with_wake(cancel, gate, WriteWake::subscribe())
            .await;
    }

    async fn run_with_wake(self, cancel: CancellationToken, gate: GateWorker, writes: WriteWake) {
        // An empty claim waits longer each time nothing turns up, until a
        // write commits here; writes committed by another process are found
        // by the poll, within `idle_poll_max`. A failing intake is not
        // hurried by new writes.
        let backoff = Backoff {
            floor: self.settings.poll_interval,
            idle_max: self.settings.idle_poll_max,
            failed_max: MAX_BACKOFF,
        };
        crate::workers::run(
            gate,
            cancel,
            // Subscribed before the first claim, so a write that commits
            // while a pass runs wakes the drain right after it.
            Wake::OnIdle(writes),
            backoff,
            self,
        )
        .await;
    }

    /// One claim → accept → mark pass; the number of records it claimed.
    ///
    /// A quiesce lets the record in flight finish: it is delivered and
    /// marked, or fails, like any other. Only the records not yet started are
    /// released, so settling costs one `accept`, not a batch of them.
    /// Shutdown also abandons the `accept` in flight.
    ///
    /// Failures are ISOLATED. One record the intake will never take would
    /// otherwise be re-claimed at the head of every pass, and every later Fact
    /// would stall behind it until the outbox filled and listenable writes
    /// started failing. Its claim goes straight back and the pass continues;
    /// the port's `attempts ASC` ordering demotes it behind fresher work on
    /// the next claim, so it costs one slot per pass.
    ///
    /// # Errors
    ///
    /// The first failure, when NO record got through, so an error means the
    /// intake or the outbox, not one envelope, is the problem.
    async fn pass(&self, cancel: &CancellationToken, gate: &GateWorker) -> Result<usize, Failure> {
        let claimed = self
            .outbox
            .claim(
                &self.consumer,
                &self.settings.publisher_id,
                self.settings.batch,
                self.settings.lease,
            )
            .await
            .map_err(|_| Failure::Storage)?;
        let count = claimed.len();
        let mut delivered = 0usize;
        let mut first_failure: Option<Failure> = None;
        let mut accepted = Accepted::default();
        let mut records = claimed.iter();
        while let Some(record) = records.next() {
            // `Proxiima.Operations.proceedWithClaim`.
            let flow = if matches!(ClaimBoundary::observe(gate, cancel), ClaimBoundary::Opened) {
                // Keep polling the same attempt after flushing older marks.
                let attempt = async {
                    let flow = self.deliver(record, gate, cancel).await;
                    if flow.is_err() {
                        self.release(record).await;
                    }
                    flow
                };
                tokio::pin!(attempt);
                // `Proxiima.Publication.flushAccepted`.
                if let Some(deadline) = accepted.deadline {
                    tokio::select! {
                        biased;
                        () = tokio::time::sleep_until(deadline) => {
                            let ((), flow) = tokio::join!(
                                self.mark(&mut accepted, &mut delivered, &mut first_failure),
                                attempt,
                            );
                            flow
                        }
                        flow = &mut attempt => flow,
                    }
                } else {
                    attempt.await
                }
            } else {
                Ok(Flow::HandBack)
            };
            match flow {
                Ok(Flow::Accepted { flush_at }) => {
                    accepted.push(record, flush_at);
                    if accepted
                        .deadline
                        .is_some_and(|deadline| deadline <= Instant::now())
                    {
                        self.mark(&mut accepted, &mut delivered, &mut first_failure)
                            .await;
                    }
                }
                Ok(Flow::Stale) => {}
                // As though the process died here: what is accepted and not
                // yet marked stays leased, as `after_accept` promises.
                Ok(Flow::Abort) => return Ok(count),
                Ok(Flow::HandBack) => {
                    // Not a failure: what the intake already took is marked,
                    // and everything else this pass holds goes back so the
                    // next process does not wait out a lease for work nobody
                    // is doing.
                    self.mark(&mut accepted, &mut delivered, &mut first_failure)
                        .await;
                    self.release(record).await;
                    for rest in records {
                        self.release(rest).await;
                    }
                    return Ok(count);
                }
                Err(failure) => {
                    first_failure.get_or_insert(failure);
                }
            }
        }
        self.mark(&mut accepted, &mut delivered, &mut first_failure)
            .await;
        // `Proxiima.Publication.deliveryPassFailed`.
        match first_failure {
            // Nothing got through at all: report it, so `run` backs off
            // rather than spinning on a dead intake.
            Some(failure) if delivered == 0 => Err(failure),
            _ => Ok(count),
        }
    }

    /// Mark every accepted record published in one transaction and leave
    /// `accepted` empty. A failed mark leaves the claims to expire.
    async fn mark(
        &self,
        accepted: &mut Accepted<'_>,
        delivered: &mut usize,
        first_failure: &mut Option<Failure>,
    ) {
        let records = accepted.take();
        if records.is_empty() {
            return;
        }
        let claims: Vec<_> = records
            .iter()
            .map(|record| (record.id, record.claim))
            .collect();
        match self
            .outbox
            .mark_published_batch(&self.consumer, &claims)
            .await
        {
            Ok(outcomes) => {
                for (record, outcome) in records.iter().zip(outcomes) {
                    match outcome {
                        AckOutcome::Published | AckOutcome::AlreadyPublished => *delivered += 1,
                        AckOutcome::StaleClaim => tracing::info!(
                            consumer = %self.consumer,
                            event_id = %record.event_id,
                            "the mark was fenced out by a newer claim"
                        ),
                    }
                }
            }
            Err(_) => {
                first_failure.get_or_insert(Failure::Storage);
            }
        }
    }

    /// Hand one claimed record to the intake and record what it said.
    ///
    /// Cancellation is observed BETWEEN records and inside the `accept`
    /// itself, so a shutdown costs one storage round trip rather than
    /// `batch × accept timeout`.
    async fn deliver(
        &self,
        record: &ClaimedPublication,
        gate: &GateWorker,
        cancel: &CancellationToken,
    ) -> Result<Flow, Failure> {
        // Parsed here, once. Only the category of the parse error is logged:
        // its text can quote the payload.
        let event = ReceivedEvent::from_record(record).map_err(|error| {
            tracing::error!(
                consumer = %self.consumer,
                event_id = %record.event_id,
                parse_error = ?error.classify(),
                "a captured envelope is not a CloudEvents envelope; the record is released \
                 and stays undelivered"
            );
            Failure::Malformed
        })?;
        // Re-check the claim right before the hand-over: a lease that expired
        // while this pass was busy may belong to another drain now, and the
        // record may have been erased since the claim. Either way it is not
        // ours to deliver; it is neither marked nor released.
        let flush_at = Instant::now()
            + self.settings.lease / proxiima_core::ledger!(Proxiima.Publication.flushLeaseDivisor);
        let still_ours = self
            .outbox
            .renew_claim(&self.consumer, record.id, record.claim, self.settings.lease)
            .await
            .map_err(|_| Failure::Storage)?;
        if !still_ours {
            tracing::debug!(
                consumer = %self.consumer,
                skipped = 1u32,
                "a claimed record is no longer deliverable under this claim; skipped"
            );
            return Ok(Flow::Stale);
        }
        if ClaimBoundary::observe(gate, cancel).hand_back() {
            return Ok(Flow::HandBack);
        }
        let accepted = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(Flow::HandBack),
            accepted = tokio::time::timeout(
                self.settings.accept_timeout,
                AssertUnwindSafe(self.intake.accept(&event)).catch_unwind(),
            ) => accepted,
        };
        // `Proxiima.Publication.owesMark`.
        match accepted {
            // `Accepted` and `Rejected` are both outcomes the intake made
            // durable.
            Ok(Ok(Ok(_decision))) => {}
            Ok(Ok(Err(_))) => return Err(Failure::Intake),
            // Only the host's code is caught: the panic hook has already
            // printed its message, and the payload stays out of the log.
            Ok(Err(_panic)) => {
                tracing::error!(
                    consumer = %self.consumer,
                    "the intake panicked on a record; the record is released and will be \
                     delivered again"
                );
                return Err(Failure::Panicked);
            }
            Err(_) => return Err(Failure::TimedOut),
        }
        if self.hook.after_accept(&record.event_id).await.is_break() {
            return Ok(Flow::Abort);
        }
        Ok(Flow::Accepted { flush_at })
    }

    /// Hand one claim back, best effort. A failed release is not an error:
    /// the lease expires on its own, which is the whole reason releases are an
    /// optimization rather than a correctness requirement.
    async fn release(&self, record: &ClaimedPublication) {
        match self
            .outbox
            .release(&self.consumer, record.id, record.claim)
            .await
        {
            Ok(
                ReleaseOutcome::Released
                | ReleaseOutcome::AlreadyPublished
                | ReleaseOutcome::StaleClaim,
            ) => {}
            Err(_) => {
                tracing::warn!(
                    consumer = %self.consumer,
                    failure_category = ?Failure::Storage,
                    "releasing a claim failed; the lease expiry still returns it"
                );
            }
        }
    }
}

impl WorkerPass for Drain {
    async fn once(&mut self, gate: &GateWorker, cancel: &CancellationToken) -> Pass {
        match self.pass(cancel, gate).await {
            // A pass that claimed anything is a pass with more waiting.
            Ok(claimed) if claimed > 0 => Pass::Worked,
            Ok(_) => Pass::Idle,
            Err(failure) => {
                tracing::warn!(
                    consumer = %self.consumer,
                    failure_category = ?failure,
                    "in-process outbox delivery failed; the records stay pending and are retried"
                );
                Pass::Failed
            }
        }
    }
}

/// Run `drain` on its own task under its own pause gate, outside the
/// runtime: crate tests drive a drain they configured themselves.
#[cfg(test)]
pub(crate) fn spawn_for_tests(
    drain: Drain,
    cancel: CancellationToken,
) -> (QuiesceGate, tokio::task::JoinHandle<()>) {
    let (gate, worker) = QuiesceGate::new();
    let task = tokio::spawn(drain.run_with_wake(cancel, worker, WriteWake::subscribe()));
    (gate, task)
}

/// Counts every `claim` the runtime drain issues, empty ones included.
///
/// Crate tests wait on the counter instead of a wall clock: an idle pass is
/// the event that "nothing more was delivered".
#[cfg(test)]
pub(crate) struct CountingOutbox {
    inner: Arc<dyn PublicationOutboxPort>,
    claims: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl CountingOutbox {
    pub(crate) fn wrap(
        inner: Arc<dyn PublicationOutboxPort>,
    ) -> (
        Arc<dyn PublicationOutboxPort>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let claims = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            Arc::new(Self {
                inner,
                claims: claims.clone(),
            }),
            claims,
        )
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl PublicationOutboxPort for CountingOutbox {
    async fn claim(
        &self,
        consumer: &ConsumerName,
        publisher: &PublisherId,
        limit: NonZeroU32,
        lease: Duration,
    ) -> Result<Vec<ClaimedPublication>, proxiima_core::StorageError> {
        self.claims
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.claim(consumer, publisher, limit, lease).await
    }

    async fn mark_published_batch(
        &self,
        consumer: &ConsumerName,
        claims: &[(uuid::Uuid, ClaimToken)],
    ) -> Result<Vec<AckOutcome>, proxiima_core::StorageError> {
        self.inner.mark_published_batch(consumer, claims).await
    }

    async fn renew_claim(
        &self,
        consumer: &ConsumerName,
        id: uuid::Uuid,
        claim: ClaimToken,
        lease: Duration,
    ) -> Result<bool, proxiima_core::StorageError> {
        self.inner.renew_claim(consumer, id, claim, lease).await
    }

    async fn release(
        &self,
        consumer: &ConsumerName,
        id: uuid::Uuid,
        claim: ClaimToken,
    ) -> Result<ReleaseOutcome, proxiima_core::StorageError> {
        self.inner.release(consumer, id, claim).await
    }

    async fn pending_count(&self) -> Result<u64, proxiima_core::StorageError> {
        self.inner.pending_count().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proxiima_core::publication::intake::{Intake, IntakeError};
    use proxiima_core::storage_ports::publication::ClaimToken;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    #[test]
    fn hand_back_matches_ledger_vectors() {
        #[derive(serde::Deserialize)]
        struct Vector {
            #[serde(rename = "in")]
            input: (ClaimBoundary,),
            out: bool,
        }
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../core/tests/ledger_vectors.json"))
                .expect("ledger vectors");
        let rows: Vec<Vector> =
            serde_json::from_value(vectors["Proxiima.Publication.handBack"].clone())
                .expect("hand-back table");
        assert_eq!(rows.len(), 3);
        for row in rows {
            assert_eq!(row.input.0.hand_back(), row.out);
        }
    }

    #[derive(Debug)]
    struct DeadlineOutbox {
        records: Vec<ClaimedPublication>,
        releasing: tokio::sync::Notify,
        finish_release: tokio::sync::Notify,
        marks: tokio::sync::mpsc::UnboundedSender<(Instant, Vec<Uuid>)>,
    }

    #[async_trait::async_trait]
    impl PublicationOutboxPort for DeadlineOutbox {
        async fn claim(
            &self,
            _: &ConsumerName,
            _: &PublisherId,
            _: NonZeroU32,
            _: Duration,
        ) -> Result<Vec<ClaimedPublication>, proxiima_core::StorageError> {
            Ok(self.records.clone())
        }

        async fn mark_published_batch(
            &self,
            _: &ConsumerName,
            claims: &[(Uuid, ClaimToken)],
        ) -> Result<Vec<AckOutcome>, proxiima_core::StorageError> {
            self.marks
                .send((Instant::now(), claims.iter().map(|(id, _)| *id).collect()))
                .expect("mark observer");
            Ok(vec![AckOutcome::Published; claims.len()])
        }

        async fn renew_claim(
            &self,
            _: &ConsumerName,
            _: Uuid,
            _: ClaimToken,
            _: Duration,
        ) -> Result<bool, proxiima_core::StorageError> {
            Ok(true)
        }

        async fn release(
            &self,
            _: &ConsumerName,
            _: Uuid,
            _: ClaimToken,
        ) -> Result<ReleaseOutcome, proxiima_core::StorageError> {
            self.releasing.notify_one();
            self.finish_release.notified().await;
            Ok(ReleaseOutcome::Released)
        }

        async fn pending_count(&self) -> Result<u64, proxiima_core::StorageError> {
            Ok(2)
        }
    }

    #[derive(Debug)]
    struct RefuseSecond {
        first: Uuid,
    }

    #[async_trait::async_trait]
    impl DurableIntake for RefuseSecond {
        async fn accept(&self, event: &ReceivedEvent) -> Result<Intake, IntakeError> {
            if event.id == self.first.to_string() {
                Ok(Intake::Accepted)
            } else {
                Err(IntakeError::new("later attempt failed"))
            }
        }
    }

    fn claimed(id: Uuid) -> ClaimedPublication {
        let envelope = serde_json::json!({
            "specversion": "1.0",
            "id": id,
            "source": "urn:proxiima:deadline",
            "type": "probe/test",
            "subject": id,
            "sequence": "00000000000000000001",
            "data": {},
        });
        ClaimedPublication {
            id,
            event_id: id.to_string(),
            owner_id: Uuid::nil(),
            schema_id: "probe/test".into(),
            schema_version: 1,
            event_type: "probe/test".into(),
            envelope: serde_json::to_vec(&envelope).expect("fixture envelope"),
            digest: [0; 32],
            attempts: 1,
            claim: ClaimToken::new(id),
            lease_expires_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    // `Proxiima.Publication.flushAccepted`.
    #[tokio::test(start_paused = true)]
    async fn a_failed_later_delivery_cannot_delay_accepted_marks() {
        let first = Uuid::from_u128(1);
        let (marks, mut marked) = tokio::sync::mpsc::unbounded_channel();
        let outbox = Arc::new(DeadlineOutbox {
            records: vec![claimed(first), claimed(Uuid::from_u128(2))],
            releasing: tokio::sync::Notify::new(),
            finish_release: tokio::sync::Notify::new(),
            marks,
        });
        let lease = Duration::from_secs(8);
        let drain = Drain::new(
            outbox.clone(),
            ConsumerName::default(),
            Arc::new(RefuseSecond { first }),
            Settings {
                lease,
                ..Settings::new()
            },
            Arc::new(Proceed),
        );
        let (_gate, worker) = QuiesceGate::new();
        let start = Instant::now();
        let task =
            tokio::spawn(async move { drain.pass(&CancellationToken::new(), &worker).await });
        tokio::time::timeout(lease, outbox.releasing.notified())
            .await
            .expect("the failed attempt starts to release");
        let (at, ids) = tokio::time::timeout(lease, marked.recv())
            .await
            .expect("mark before the lease expires")
            .expect("one mark");
        assert_eq!(at, start + lease / 2);
        assert_eq!(ids, vec![first]);
        outbox.finish_release.notify_one();
        assert_eq!(
            task.await
                .expect("pass joins")
                .expect("one record delivered"),
            2
        );
    }

    /// An outbox that never has anything to deliver; counts its claims.
    #[derive(Debug, Default)]
    struct EmptyOutbox {
        claims: AtomicUsize,
        claimed: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl PublicationOutboxPort for EmptyOutbox {
        async fn claim(
            &self,
            _consumer: &ConsumerName,
            _publisher: &PublisherId,
            _limit: NonZeroU32,
            _lease: Duration,
        ) -> Result<Vec<ClaimedPublication>, proxiima_core::StorageError> {
            self.claims.fetch_add(1, Ordering::SeqCst);
            self.claimed.notify_one();
            Ok(Vec::new())
        }

        async fn mark_published_batch(
            &self,
            _consumer: &ConsumerName,
            _claims: &[(Uuid, ClaimToken)],
        ) -> Result<Vec<AckOutcome>, proxiima_core::StorageError> {
            unreachable!("nothing is claimed")
        }

        async fn renew_claim(
            &self,
            _consumer: &ConsumerName,
            _id: Uuid,
            _claim: ClaimToken,
            _lease: Duration,
        ) -> Result<bool, proxiima_core::StorageError> {
            unreachable!("nothing is claimed")
        }

        async fn release(
            &self,
            _consumer: &ConsumerName,
            _id: Uuid,
            _claim: ClaimToken,
        ) -> Result<ReleaseOutcome, proxiima_core::StorageError> {
            unreachable!("nothing is claimed")
        }

        async fn pending_count(&self) -> Result<u64, proxiima_core::StorageError> {
            Ok(0)
        }
    }

    #[derive(Debug)]
    struct NoIntake;

    #[async_trait::async_trait]
    impl DurableIntake for NoIntake {
        async fn accept(&self, _event: &ReceivedEvent) -> Result<Intake, IntakeError> {
            unreachable!("nothing is claimed")
        }
    }

    fn run_drain(
        outbox: &Arc<EmptyOutbox>,
        settings: Settings,
    ) -> (impl Fn(), CancellationToken, tokio::task::JoinHandle<()>) {
        let drain = Drain::new(
            outbox.clone(),
            ConsumerName::default(),
            Arc::new(NoIntake),
            settings,
            Arc::new(Proceed),
        );
        let cancel = CancellationToken::new();
        let (writes, wake) = WriteWake::channel_for_tests();
        let (_gate, worker) = QuiesceGate::new();
        let task = tokio::spawn(drain.run_with_wake(cancel.clone(), worker, writes));
        (wake, cancel, task)
    }

    /// An idle drain backs off to the ceiling: over ten minutes it claims
    /// about thirty times, not once per poll interval.
    #[tokio::test(start_paused = true)]
    async fn an_idle_drain_backs_off_to_the_ceiling() {
        let outbox = Arc::new(EmptyOutbox::default());
        let settings = Settings::new();
        let (poll, ceiling) = (settings.poll_interval, settings.idle_poll_max);
        let (_wake, cancel, task) = run_drain(&outbox, settings);
        tokio::time::sleep(Duration::from_mins(10)).await;
        cancel.cancel();
        task.await.expect("drain joins");

        // Doubling from the poll floor until `idle_poll_max`, then one claim
        // per ceiling. The first claim is immediate. (`cfg(test)` uses a 50 ms
        // floor; this test is virtual time, so the floor is not a wall wait.)
        let mut doubling = 0usize;
        let mut wait = poll;
        while wait < ceiling {
            doubling += 1;
            wait = wait.saturating_mul(2).min(ceiling);
        }
        let ramp: Duration = (0..doubling)
            .map(|step| poll.saturating_mul(1 << u32::try_from(step).expect("small")))
            .sum();
        let at_ceiling = Duration::from_mins(10)
            .checked_sub(ramp)
            .expect("the ramp is shorter than ten minutes")
            .as_secs()
            / ceiling.as_secs();
        let bound = 1 + doubling + usize::try_from(at_ceiling).expect("small");
        let claims = outbox.claims.load(Ordering::SeqCst);
        assert!(
            (bound - 2..=bound + 1).contains(&claims),
            "{claims} claims in ten idle minutes, expected about {bound}"
        );
    }

    /// A write committed in this process wakes an idle drain at once, however
    /// long its fallback sleep.
    #[tokio::test]
    async fn a_committed_write_wakes_an_idle_drain() {
        let outbox = Arc::new(EmptyOutbox::default());
        let settings = Settings {
            poll_interval: Duration::from_hours(1),
            idle_poll_max: Duration::from_hours(1),
            ..Settings::new()
        };
        let (wake, cancel, task) = run_drain(&outbox, settings);
        let _cancel_on_drop = cancel.clone().drop_guard();
        let claim = || tokio::time::timeout(Duration::from_secs(5), outbox.claimed.notified());
        claim().await.expect("the first claim runs at once");
        assert_eq!(outbox.claims.load(Ordering::SeqCst), 1);
        for expected in 2..=3 {
            wake();
            claim().await.expect("a committed write wakes the drain");
            assert_eq!(outbox.claims.load(Ordering::SeqCst), expected);
        }
        cancel.cancel();
        task.await.expect("drain joins");
    }
}

#[cfg(test)]
#[path = "in_process_outbox_tests.rs"]
mod in_process_outbox_tests;
