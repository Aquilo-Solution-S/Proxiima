//! The runtime's metrics, reported through the OpenTelemetry API.
//!
//! This crate only records. The host installs the meter provider
//! (`opentelemetry::global::set_meter_provider`) before it calls
//! `Proxiima::build` or `run`; without one every instrument is a no-op and
//! nothing here costs a database query. The instruments are listed in
//! `docs/operate.md`.
//!
//! An observable gauge's callback runs when a reader collects, and it is
//! synchronous, so it cannot ask the database. The two database gauges
//! therefore report the [`Sample`] the [`Metrics::sampler`] worker last
//! stored, and each asks that worker for a fresh one unless a read is
//! outstanding. With no reader there is no collection and so no read.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Meter};
use proxiima_core::storage_ports::publication::ConsumerName;
use proxiima_core::{EmbeddingJobBacklog, StorageError};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::object_maintenance::ObjectPurgeHealth;

/// The label the sampler runs under in the worker set.
pub(crate) const SAMPLER_LABEL: &str = "metrics-sampler";

/// What the database gauges report: the last successful read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sample {
    /// Deliveries each registered consumer is still owed.
    pub(crate) outbox: Vec<(ConsumerName, u64)>,
    pub(crate) embedding: EmbeddingJobBacklog,
}

/// The sample the gauges report and the request for the next one.
#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    wanted: Notify,
}

/// One lock holds both, so a collection either sees the read still
/// outstanding or sees its outcome and asks for the next.
#[derive(Debug, Default)]
struct State {
    /// `None` is unknown: the gauges observe nothing.
    sample: Option<Sample>,
    /// A read was asked for and its outcome is not stored yet.
    outstanding: bool,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Report the sample to `observe`, if there is one, and ask for the
    /// next unless a read is outstanding.
    fn report(&self, observe: impl FnOnce(&Sample)) {
        let mut state = self.state();
        if let Some(sample) = &state.sample {
            observe(sample);
        }
        if !std::mem::replace(&mut state.outstanding, true) {
            self.wanted.notify_one();
        }
    }

    /// Store the outcome of the outstanding read.
    fn store(&self, sample: Option<Sample>) {
        *self.state() = State {
            sample,
            outstanding: false,
        };
    }
}

/// The runtime's instruments. Cheap to clone.
#[derive(Debug, Clone)]
pub(crate) struct Metrics {
    worker_ended: Counter<u64>,
    shared: Arc<Shared>,
}

impl Metrics {
    /// Register every instrument on `meter`. The SDK keeps the callbacks
    /// registered for the life of its provider, so the gauge handles are not
    /// held.
    pub(crate) fn new(meter: &Meter, purge: ObjectPurgeHealth) -> Self {
        let shared = Arc::new(Shared::default());
        let worker_ended = meter
            .u64_counter("proxiima.worker.ended")
            .with_unit("{worker}")
            .with_description("Background workers that ended before shutdown")
            .build();

        meter
            .u64_observable_gauge("proxiima.object_purge.debt")
            .with_unit("{object}")
            .with_description("Objects owed to the purge queue after the last pass, by state")
            .with_callback(move |gauge| {
                if let Some(snapshot) = purge.snapshot() {
                    for (state, count) in [
                        ("due", snapshot.due),
                        ("deferred", snapshot.deferred),
                        ("stuck", snapshot.stuck),
                    ] {
                        gauge.observe(count, &[KeyValue::new("state", state)]);
                    }
                }
            })
            .build();

        let sampled = shared.clone();
        meter
            .u64_observable_gauge("proxiima.outbox.pending")
            .with_unit("{record}")
            .with_description("Outbox deliveries not yet published, by consumer")
            .with_callback(move |gauge| {
                sampled.report(|sample| {
                    for (consumer, pending) in &sample.outbox {
                        gauge.observe(
                            *pending,
                            &[KeyValue::new("consumer", consumer.as_str().to_owned())],
                        );
                    }
                });
            })
            .build();

        let sampled = shared.clone();
        meter
            .u64_observable_gauge("proxiima.embedding.jobs")
            .with_unit("{job}")
            .with_description("Embedding jobs by status")
            .with_callback(move |gauge| {
                sampled.report(|sample| {
                    let jobs = sample.embedding;
                    for (status, count) in [
                        ("pending", jobs.pending),
                        ("processing", jobs.processing),
                        ("failed", jobs.failed),
                    ] {
                        gauge.observe(count, &[KeyValue::new("status", status)]);
                    }
                });
            })
            .build();

        Self {
            worker_ended,
            shared,
        }
    }

    /// Instruments of a provider that records nothing, for tests that do not
    /// look at metrics.
    #[cfg(test)]
    pub(crate) fn unobserved() -> Self {
        use opentelemetry::metrics::MeterProvider;
        Self::new(
            &opentelemetry::metrics::NoopMeterProvider::new().meter("proxiima-test"),
            ObjectPurgeHealth::default(),
        )
    }

    /// Count one worker that ended before shutdown. `worker` and `instance`
    /// are the fields of the supervisor's error line.
    pub(crate) fn worker_ended(&self, label: &'static str, instance: Option<&str>) {
        let mut attributes = vec![KeyValue::new("worker", label)];
        if let Some(instance) = instance {
            attributes.push(KeyValue::new("instance", instance.to_owned()));
        }
        self.worker_ended.add(1, &attributes);
    }

    /// The worker that keeps the sample: until `cancel`, each time a
    /// collection asks, it runs `read` and stores the result. A failed read
    /// is logged and stores nothing, since a stale number is worse than none.
    pub(crate) fn sampler<Read, Reading>(
        &self,
        cancel: CancellationToken,
        read: Read,
    ) -> impl Future<Output = ()> + Send + 'static
    where
        Read: Fn() -> Reading + Send + 'static,
        Reading: Future<Output = Result<Sample, StorageError>> + Send,
    {
        let shared = self.shared.clone();
        async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = shared.wanted.notified() => {}
                }
                // A read in flight does not hold shutdown up.
                let read = tokio::select! {
                    () = cancel.cancelled() => return,
                    read = read() => read,
                };
                match read {
                    Ok(sample) => shared.store(Some(sample)),
                    Err(error) => {
                        tracing::warn!(%error, "metrics sample failed; the gauges report nothing");
                        shared.store(None);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
    use tokio::sync::{Mutex as AsyncMutex, mpsc};

    use super::*;
    use crate::object_maintenance::ObjectPurgeSnapshot;
    use crate::workers::{EndedWorkers, Workers};

    /// A meter provider local to one test, collected on demand.
    struct Collector {
        provider: SdkMeterProvider,
        exporter: InMemoryMetricExporter,
    }

    /// One data point: the instrument, its attributes in name order, its value.
    type Point = (String, Vec<(String, String)>, u64);

    impl Collector {
        fn new() -> Self {
            let exporter = InMemoryMetricExporter::default();
            let provider = SdkMeterProvider::builder()
                // Collected on demand only: the interval never elapses.
                .with_reader(
                    PeriodicReader::builder(exporter.clone())
                        .with_interval(Duration::from_hours(24))
                        .build(),
                )
                .build();
            Self { provider, exporter }
        }

        fn meter(&self) -> Meter {
            self.provider.meter("proxiima-test")
        }

        /// Run one collection and return what every instrument reported.
        fn collect(&self) -> Vec<Point> {
            self.exporter.reset();
            self.provider.force_flush().expect("collection");
            let mut points = Vec::new();
            for resource in self.exporter.get_finished_metrics().expect("finished") {
                for scope in resource.scope_metrics() {
                    for metric in scope.metrics() {
                        let AggregatedMetrics::U64(data) = metric.data() else {
                            panic!("{} is not a u64 instrument", metric.name());
                        };
                        let mut add = |attributes: Vec<(String, String)>, value| {
                            let mut attributes = attributes;
                            attributes.sort();
                            points.push((metric.name().to_owned(), attributes, value));
                        };
                        let pairs = |attributes: &mut dyn Iterator<Item = &KeyValue>| {
                            attributes
                                .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                                .collect::<Vec<_>>()
                        };
                        match data {
                            MetricData::Gauge(gauge) => {
                                for point in gauge.data_points() {
                                    add(pairs(&mut point.attributes()), point.value());
                                }
                            }
                            MetricData::Sum(sum) => {
                                for point in sum.data_points() {
                                    add(pairs(&mut point.attributes()), point.value());
                                }
                            }
                            other => panic!("unexpected aggregation {other:?}"),
                        }
                    }
                }
            }
            points.sort();
            points
        }
    }

    fn point(name: &str, attributes: &[(&str, &str)], value: u64) -> Point {
        (
            name.to_owned(),
            attributes
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            value,
        )
    }

    fn named(points: &[Point], prefix: &str) -> Vec<Point> {
        points
            .iter()
            .filter(|(name, ..)| name.starts_with(prefix))
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn a_worker_that_ends_before_shutdown_is_counted_with_its_instance() {
        let collector = Collector::new();
        let metrics = Metrics::new(&collector.meter(), ObjectPurgeHealth::default());
        let cancel = CancellationToken::new();
        let mut workers = Workers::default();
        workers.spawn("outbox-publisher", Some("search-index".to_owned()), async {
        });
        workers.spawn("embedding-drain", None, async {});
        workers.spawn("until-cancelled", None, {
            let cancel = cancel.clone();
            async move { cancel.cancelled().await }
        });
        let ended = EndedWorkers::default();
        let supervisor = workers.supervise(cancel.clone(), ended.clone(), metrics);
        tokio::time::timeout(Duration::from_secs(5), async {
            let both = "ended-workers: outbox-publisher, embedding-drain";
            let either = "ended-workers: embedding-drain, outbox-publisher";
            while !ended
                .line()
                .is_some_and(|line| line == both || line == either)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both workers that returned are recorded");
        cancel.cancel();
        supervisor.await.expect("the supervisor joins every worker");

        assert_eq!(
            named(&collector.collect(), "proxiima.worker."),
            [
                point(
                    "proxiima.worker.ended",
                    &[("instance", "search-index"), ("worker", "outbox-publisher")],
                    1
                ),
                point("proxiima.worker.ended", &[("worker", "embedding-drain")], 1),
            ],
            "the worker that ended on cancel is not counted"
        );
    }

    /// Wait for the sampler to settle on `expected`.
    async fn until_sampled(metrics: &Metrics, expected: Option<Sample>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while metrics.shared.state().sample != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the sampler stores the outcome of its read");
    }

    fn sample(search: u64, pending_jobs: u64) -> Sample {
        Sample {
            outbox: vec![
                (ConsumerName::new("billing").unwrap(), 0),
                (ConsumerName::new("search").unwrap(), search),
            ],
            embedding: EmbeddingJobBacklog {
                pending: pending_jobs,
                processing: 1,
                failed: 2,
            },
        }
    }

    #[tokio::test]
    async fn a_collection_wakes_the_sampler_and_a_failed_read_removes_the_numbers() {
        let collector = Collector::new();
        let metrics = Metrics::new(&collector.meter(), ObjectPurgeHealth::default());
        let cancel = CancellationToken::new();
        // Each read waits for the test to hand it an outcome.
        let (outcomes, inbox) = mpsc::unbounded_channel();
        let inbox = Arc::new(AsyncMutex::new(inbox));
        let sampler = tokio::spawn(metrics.sampler(cancel.clone(), move || {
            let inbox = inbox.clone();
            async move { inbox.lock().await.recv().await.expect("an outcome") }
        }));

        let before = collector.collect();
        assert_eq!(named(&before, "proxiima.outbox."), []);
        assert_eq!(named(&before, "proxiima.embedding."), []);

        outcomes.send(Ok(sample(7, 3))).unwrap();
        until_sampled(&metrics, Some(sample(7, 3))).await;
        let after = collector.collect();
        assert_eq!(
            named(&after, "proxiima.outbox."),
            [
                point("proxiima.outbox.pending", &[("consumer", "billing")], 0),
                point("proxiima.outbox.pending", &[("consumer", "search")], 7),
            ]
        );
        assert_eq!(
            named(&after, "proxiima.embedding."),
            [
                point("proxiima.embedding.jobs", &[("status", "failed")], 2),
                point("proxiima.embedding.jobs", &[("status", "pending")], 3),
                point("proxiima.embedding.jobs", &[("status", "processing")], 1),
            ]
        );

        // That collection asked again: the answer is a failed read.
        outcomes
            .send(Err(StorageError::ConstraintViolation("down".into())))
            .unwrap();
        until_sampled(&metrics, None).await;
        let failed = collector.collect();
        assert_eq!(named(&failed, "proxiima.outbox."), []);
        assert_eq!(named(&failed, "proxiima.embedding."), []);

        cancel.cancel();
        sampler.await.expect("the sampler stops on cancel");
    }

    #[tokio::test]
    async fn requests_coalesce_while_a_read_is_outstanding() {
        let shared = Shared::default();
        shared.report(|_| {});
        shared.report(|_| {});
        shared.wanted.notified().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), shared.wanted.notified())
                .await
                .is_err(),
            "a request made while a read was outstanding woke the sampler again"
        );
        shared.store(None);
        shared.report(|_| {});
        tokio::time::timeout(Duration::from_secs(5), shared.wanted.notified())
            .await
            .expect("a request after the outcome is stored asks for a new read");
    }

    #[test]
    fn the_object_purge_gauge_reports_the_last_snapshot_and_nothing_without_one() {
        let collector = Collector::new();
        let health = ObjectPurgeHealth::default();
        let _metrics = Metrics::new(&collector.meter(), health.clone());
        assert_eq!(named(&collector.collect(), "proxiima.object_purge."), []);

        health.record(ObjectPurgeSnapshot {
            due: 2,
            deferred: 3,
            stuck: 1,
        });
        assert_eq!(
            named(&collector.collect(), "proxiima.object_purge."),
            [
                point("proxiima.object_purge.debt", &[("state", "deferred")], 3),
                point("proxiima.object_purge.debt", &[("state", "due")], 2),
                point("proxiima.object_purge.debt", &[("state", "stuck")], 1),
            ]
        );
    }
}
