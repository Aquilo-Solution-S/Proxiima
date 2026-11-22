//! Background workers: the one loop every runtime feature worker runs
//! ([`run`]), the set the runtime owns them in ([`Workers`]), and the
//! spiice-contributed workers that join the same set.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proxiima_core::quiesce::{Checkpoint, GateWorker};
use proxiima_core::write_wake::WriteWake;
use proxiima_spiice::SpiiceWorker;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use crate::metrics::Metrics;

/// The result of a worker pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pass {
    Worked,
    Idle,
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
        // `Proxiima.Operations.commitEndsWait`
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

    // `Ord::clamp` panics when the configured floor exceeds the ceiling.
    fn next(self, current: Duration, ceiling: Duration) -> Duration {
        // `Proxiima.Operations.backoffBound`
        current
            .saturating_mul(proxiima_core::ledger!(
                Proxiima.Operations.backoffMultiplier
            ))
            .min(ceiling)
            .max(self.floor)
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
        // `Proxiima.Operations.passWait`
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

    /// The readiness line, `ended-workers: <label>, <label> x2`: each label
    /// once, in the order its first worker ended, with the count where more
    /// than one worker of that label ended; `None` while every worker runs.
    pub(crate) fn line(&self) -> Option<String> {
        let ended = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `Proxiima.Operations.endedWorkerProjection`
        let mut counts: Vec<(&'static str, usize)> = Vec::new();
        for label in ended.iter() {
            match counts.iter_mut().find(|(seen, _)| seen == label) {
                Some((_, count)) => *count += 1,
                None => counts.push((label, 1)),
            }
        }
        let labels: Vec<String> = counts
            .into_iter()
            .map(|(label, count)| match count {
                1 => label.to_owned(),
                more => format!("{label} x{more}"),
            })
            .collect();
        (!labels.is_empty()).then(|| format!("ended-workers: {}", labels.join(", ")))
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
    pub(crate) fn adopt(
        &mut self,
        worker: SpiiceWorker,
        cancel: CancellationToken,
        grace: Duration,
    ) {
        let SpiiceWorker { name, handle } = worker;
        let mut handle = AbortOnDropHandle::new(handle);
        self.spawn(name, None, async move {
            // `Proxiima.Operations.contributedWorkerShutdown`
            let joined = tokio::select! {
                result = &mut handle => result,
                () = cancel.cancelled() => {
                    let Ok(result) = tokio::time::timeout(grace, &mut handle).await else {
                        tracing::warn!(worker = name, "worker exceeded shutdown grace; aborting");
                        return;
                    };
                    result
                }
            };
            if let Err(error) = joined {
                match error.try_into_panic() {
                    Ok(panic) => std::panic::resume_unwind(panic),
                    Err(error) => tracing::warn!(worker = name, %error, "worker join failed"),
                }
            }
        });
    }

    /// Return the task the runtime awaits to join its supervised workers.
    pub(crate) fn supervise(
        self,
        cancel: CancellationToken,
        ended: EndedWorkers,
        metrics: Metrics,
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
                // Serving and peer workers continue after an exit.
                // `Proxiima.Operations.reportWorkerExit`
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
                        metrics.worker_ended(label, instance.as_deref());
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

#[cfg(test)]
#[path = "workers_tests.rs"]
mod tests;
