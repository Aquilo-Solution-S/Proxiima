//! Wake the in-process background drains when a write commits.
//!
//! The outbox publisher and the embedding worker claim queue rows that
//! writes enqueue. Instead of claiming on a short fixed interval, they sleep
//! until a write commits in this process, and otherwise poll on a fallback
//! interval that backs off while their claims come back empty. A storage
//! adapter passes its commit to [`commit_and_wake`]; a drain holds a
//! [`WriteWake`] and awaits [`WriteWake::changed`] beside its fallback sleep.
//!
//! The signal is process-wide and carries no data: any committed write wakes
//! every drain in the process, and a drain that wakes for a write it has no
//! work for pays one empty claim. A wake is a hint, never a source of truth.
//! Writes committed by another process (another replica, a command-line
//! tool) are not signalled; the fallback poll finds them.

use std::sync::LazyLock;

use tokio::sync::watch;

/// The process's one signal; [`Writes`] instances other than this one exist
/// only in tests, so a test never sees another test's writes.
static WRITES: LazyLock<Writes> = LazyLock::new(Writes::new);

/// Run a storage adapter's commit, waking the drains only on success.
///
/// # Errors
///
/// Returns the error produced by the commit future.
pub async fn commit_and_wake<T, E>(commit: impl Future<Output = Result<T, E>>) -> Result<T, E> {
    WRITES.after_commit(commit).await
}

/// A write-committed signal: an epoch every commit bumps.
struct Writes(watch::Sender<u64>);

impl Writes {
    fn new() -> Self {
        Self(watch::channel(0).0)
    }

    async fn after_commit<T, E>(&self, commit: impl Future<Output = Result<T, E>>) -> Result<T, E> {
        // `Proxiima.Operations.wakeAfterCommit`
        let value = commit.await?;
        self.committed();
        Ok(value)
    }

    fn committed(&self) {
        // `Proxiima.Operations.bumpsWakeEpoch`
        self.0.send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    fn subscribe(&self) -> WriteWake {
        WriteWake {
            writes: self.0.subscribe(),
        }
    }
}

/// One drain's view of [`commit_and_wake`] signals.
#[derive(Debug)]
pub struct WriteWake {
    writes: watch::Receiver<u64>,
}

impl WriteWake {
    /// Subscribe. Writes committed before this call do not wake it.
    #[must_use]
    pub fn subscribe() -> Self {
        WRITES.subscribe()
    }

    /// A private signal for a test drain, independent of concurrent database writes.
    #[cfg(feature = "testkit")]
    #[must_use]
    pub fn channel_for_tests() -> (Self, impl Fn()) {
        let writes = Writes::new();
        (writes.subscribe(), move || writes.committed())
    }

    /// Resolve once a write has committed since the last call (or since
    /// [`Self::subscribe`]). Several writes in between resolve it once.
    pub async fn changed(&mut self) {
        if self.writes.changed().await.is_err() {
            // The process signal is a static and never dropped.
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Writes;
    use futures_util::FutureExt as _;
    use std::time::Duration;

    // Each test owns its signal: the process one is shared by every test
    // running in this binary, whose writes would wake (or race) these.

    #[tokio::test]
    async fn a_commit_after_subscribing_wakes() {
        let writes = Writes::new();
        let mut wake = writes.subscribe();
        writes.committed();
        writes.committed();
        tokio::time::timeout(Duration::from_secs(1), wake.changed())
            .await
            .expect("a committed write wakes the subscriber");
        assert!(
            wake.changed().now_or_never().is_none(),
            "two writes before one wait wake it once"
        );
    }

    #[tokio::test]
    async fn commit_wake_matches_ledger_vectors() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../tests/ledger_vectors.json")).unwrap();
        for row in vectors["Proxiima.Operations.wakeAfterCommit"]
            .as_array()
            .unwrap()
        {
            let writes = Writes::new();
            let mut wake = writes.subscribe();
            let result = match row["in"][0].as_str().unwrap() {
                "committed" => Some(std::future::ready(Ok(()))),
                "failed" => Some(std::future::ready(Err(()))),
                "pending" => None,
                other => panic!("unknown commit outcome: {other}"),
            };
            if let Some(result) = result {
                let _ = writes.after_commit(result).await;
            } else {
                assert!(
                    writes
                        .after_commit(std::future::pending::<Result<(), ()>>())
                        .now_or_never()
                        .is_none()
                );
            }
            assert_eq!(
                wake.changed().now_or_never().is_some(),
                row["out"].as_bool().unwrap()
            );
        }
    }

    #[tokio::test]
    async fn nothing_committed_since_subscribing_does_not_wake() {
        let writes = Writes::new();
        writes.committed();
        let mut wake = writes.subscribe();
        assert!(
            wake.changed().now_or_never().is_none(),
            "a write committed before subscribing is not a wake"
        );
    }

    #[tokio::test]
    async fn the_process_signal_wakes_a_subscriber() {
        let mut wake = super::WriteWake::subscribe();
        super::commit_and_wake(async { Ok::<_, ()>(()) })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), wake.changed())
            .await
            .expect("a successful commit wakes a WriteWake");
    }
}
