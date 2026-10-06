//! Supervised task spawning (SPEC-P16 §2).
//!
//! Every long-lived spawned task in the daemon goes through [`spawn`]: a
//! panic (or an early clean exit) from the task's future never kills the
//! daemon — the attempt is caught via its [`JoinHandle`], logged with the task
//! name and a restart counter, and re-run after an exponential backoff
//! (`1s`, `2s`, `4s`, … capped at `30s`).
//!
//! The supervisor is shutdown-aware: after every attempt exits (and again
//! after the backoff sleep completes) it checks the *currently received value*
//! of the shutdown watch. Once the watch reads `true`, the loop ends and the
//! task is **not** restarted. This is what lets tasks that legitimately return
//! on shutdown (the health server finishing its drain, the anchor/bot services
//! stopping, the signal forwarder that raises the flag itself, the pipeline
//! run awaited in `run_*`) terminate cleanly, while a task that died early is
//! retried.
//!
//! ```text
//! spawn(name, shutdown, make_fut):
//!   loop {
//!       outcome = run make_fut() to completion (panics caught by tokio::spawn)
//!       if shutdown received -> log + stop
//!       log restart (task = name, restarts = <counter>, reason = panic|exit|aborted)
//!       sleep backoff(restarts)        // 1s doubled, 30s cap
//!       if shutdown received -> log + stop
//!   }
//! ```

use std::future::Future;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Delay before the first restart (doubles per consecutive restart).
const INITIAL_BACKOFF_SECS: u64 = 1;
/// Upper bound for the restart delay ("1s..30s cap", SPEC-P16 §2).
const MAX_BACKOFF_SECS: u64 = 30;

/// Spawn a long-lived task under supervision.
///
/// `make_fut` rebuilds the task's future for every attempt: the first at
/// spawn time and a fresh one after each caught exit. The supervisor awaits
/// each attempt's [`JoinHandle`]:
///
/// - a **panic** is caught (`JoinError::is_panic`), logged with the task name
///   and the running restart counter, and the task is restarted;
/// - a **clean exit** is treated the same way — for a long-lived server an
///   early return is worth restarting — unless it happened because the
///   shared shutdown watch flipped (`shutdown` reads `true` when checked);
/// - before each restart, and again after the backoff sleep, the currently
///   received shutdown value is checked: once set, supervision ends and the
///   task is not restarted.
///
/// Each attempt runs inside [`tokio::spawn`], so a panic inside the future is
/// caught by the runtime and never tears down the supervisor (or the daemon).
///
/// The restart counter is observable through the tracing fields of every
/// restart event (`task`, `restarts`, `reason`).
pub fn spawn<F, Fut>(
    name: &'static str,
    shutdown: watch::Receiver<bool>,
    mut make_fut: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut restarts: u64 = 0;
        loop {
            let outcome = tokio::spawn(make_fut()).await;
            // The received value decides: a flip means "stop, do not restart".
            if *shutdown.borrow() {
                tracing::info!(
                    task = name,
                    restarts,
                    "supervised task stopped after shutdown"
                );
                return;
            }
            restarts = restarts.saturating_add(1);
            match outcome {
                Ok(()) => tracing::warn!(
                    task = name,
                    restarts,
                    reason = "exit",
                    "supervised task exited early; restarting"
                ),
                Err(err) if err.is_panic() => tracing::warn!(
                    task = name,
                    restarts,
                    reason = "panic",
                    error = %err,
                    "supervised task panicked; restarting"
                ),
                Err(err) => tracing::warn!(
                    task = name,
                    restarts,
                    reason = "aborted",
                    error = %err,
                    "supervised task was aborted; restarting"
                ),
            }
            tokio::time::sleep(backoff_for(restarts)).await;
            // A shutdown that arrived mid-backoff also stops the restarts.
            if *shutdown.borrow() {
                tracing::info!(
                    task = name,
                    restarts,
                    "shutdown received during backoff; not restarting"
                );
                return;
            }
        }
    })
}

/// Restart delay after `restarts` consecutive restarts: `1s`, `2s`, `4s`,
/// `8s`, `16s`, then the [`MAX_BACKOFF_SECS`] cap.
fn backoff_for(restarts: u64) -> Duration {
    let shift = restarts.saturating_sub(1).min(5);
    Duration::from_secs((INITIAL_BACKOFF_SECS << shift).min(MAX_BACKOFF_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_starts_at_one_second_and_caps_at_thirty() {
        let observed: Vec<u64> = (0..=8).map(|n| backoff_for(n).as_secs()).collect();
        assert_eq!(observed, vec![1, 1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(
            backoff_for(u64::MAX).as_secs(),
            30,
            "cap holds for any count"
        );
    }
}
