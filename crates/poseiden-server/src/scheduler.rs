//! The poll scheduler.
//!
//! A single tokio task that polls immediately on startup, then every
//! `poll_interval`. Shared by the Docker web instance and the Tauri-embedded
//! instance - both just call [`Scheduler::spawn`] with an `Arc<Service>`.
//!
//! Errors never escape: [`Service::poll_once`] handles per-project failures
//! internally, so the loop can't die on a transient upstream hiccup. It keeps
//! ticking; the next poll retries.

use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::info;

use crate::service::SharedService;

/// Owns the background poll task. Dropping it aborts the task, so callers keep
/// it alive for as long as polling should continue (the server holds it for
/// the process lifetime).
pub struct Scheduler {
    handle: JoinHandle<()>,
    doctor_handle: JoinHandle<()>,
}

/// How often the Doctor re-runs (with auto-fixes) in the background - keeps the
/// traffic light current + self-registers team checks. Faster than the data
/// poll because health should react quickly.
const DOCTOR_INTERVAL: Duration = Duration::from_secs(60);

impl Scheduler {
    /// Spawn the background loops for `service`: the data poll (immediately, then
    /// on the configured cadence) and the Doctor tick (auto-fixing, on its own
    /// faster cadence). Both run once right away so the UI is correct on first
    /// load.
    pub fn spawn(service: SharedService) -> Self {
        // `POSEIDEN_NO_POLL` freezes the instance on whatever is already in the
        // store: no upstream polling, no Doctor checks. Used to drive the app
        // against a pre-seeded demo dataset (e.g. for documentation screenshots)
        // without ever contacting a provider. The loops still spawn (so the
        // handles are valid) but do nothing.
        let frozen = std::env::var_os("POSEIDEN_NO_POLL").is_some();

        let poll_svc = service.clone();
        let handle = tokio::spawn(async move {
            if frozen {
                info!("POSEIDEN_NO_POLL set - scheduler frozen, not polling");
                return;
            }
            let interval = poll_svc.poll_interval();
            info!(interval_secs = interval.as_secs(), "scheduler started");
            loop {
                // Every tenant (owner) each tick - standalone has one, a hosted
                // instance has one per authenticated user.
                poll_svc.poll_all_owners().await;
                sleep(interval).await;
            }
        });

        // The Doctor runs on its own interval so team checks self-register and
        // the light stays current independent of the data poll.
        let doctor_handle = tokio::spawn(async move {
            if frozen {
                return;
            }
            loop {
                service.doctor_tick().await;
                sleep(DOCTOR_INTERVAL).await;
            }
        });

        Self {
            handle,
            doctor_handle,
        }
    }

    /// Abort the background loops. Also happens on drop.
    pub fn stop(self) {
        self.handle.abort();
        self.doctor_handle.abort();
    }
}

/// Indirection so the sleep is easy to reason about (and swap in tests).
async fn sleep(d: Duration) {
    tokio::time::sleep(d).await;
}
