//! Deferred runtime for the desktop shell.
//!
//! POSEIDEN's first run asks the user *how* to run (local vs a remote instance)
//! and, for a local install, *where* to keep data (portable or OS-standard) -
//! and it must decide **before anything is written**. So the store, `Service`,
//! scheduler, and file telemetry are NOT built at boot. They're deferred until
//! [`AppState::initialize`], which the onboarding UI calls once the choices are
//! made.
//!
//! Three cases:
//! - **Returning user** - a database already exists at the resolved location, so
//!   [`AppState::boot`] brings the runtime up immediately (no prompt).
//! - **First run (local)** - nothing exists; the state stays empty until
//!   onboarding calls `initialize`, which optionally enables portable mode
//!   (writing the sentinel first) and *then* creates dirs + DB.
//! - **Repointed (remote) client** - onboarding sets an instance URL on the
//!   frontend and never calls `initialize`; no local runtime is ever built.

use std::sync::{Arc, Mutex};

use poseiden_paths::{enable_portable_sentinel, Paths};
use poseiden_server::{Scheduler, Service, SharedService};
use poseiden_store::Store;
use tracing::info;

/// The live local runtime, present only once initialised. The scheduler and
/// telemetry guard are held for the process lifetime (dropping the scheduler
/// aborts polling; dropping the guard tears down exporters).
struct Runtime {
    service: SharedService,
    _scheduler: Scheduler,
    _telemetry: Option<poseiden_telemetry::TelemetryGuard>,
}

/// Managed Tauri state. Starts empty on a first run; [`Self::initialize`] fills
/// it in. The mutex is never held across an `.await` - callers clone the
/// `Arc<Service>` out and drop the lock.
pub struct AppState {
    inner: Mutex<Option<Runtime>>,
}

impl AppState {
    /// Boot. If a database already exists at the resolved location, the user has
    /// run before - bring the runtime up now. Otherwise leave it empty for the
    /// onboarding flow (or a repointed client that never needs it).
    pub fn boot() -> Self {
        let inner = Mutex::new(None);
        let paths = Paths::resolve();
        if paths.database_path().exists() {
            match tauri::async_runtime::block_on(build_runtime(paths.is_portable())) {
                Ok(rt) => *inner.lock().unwrap() = Some(rt),
                // Don't abort the whole app if an existing store fails to open;
                // the UI shows "not ready" and the user can retry / re-onboard.
                Err(e) => eprintln!("POSEIDEN: failed to open existing store: {e}"),
            }
        }
        Self { inner }
    }

    /// Whether the local runtime is up.
    pub fn is_ready(&self) -> bool {
        self.inner.lock().unwrap().is_some()
    }

    /// The `Service`, or an error if not initialised yet. Clones the `Arc` out so
    /// the lock is never held across the caller's `.await`s.
    pub fn service(&self) -> Result<SharedService, String> {
        self.inner
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.service.clone())
            .ok_or_else(|| "POSEIDEN is not set up yet".to_string())
    }

    /// First-run initialisation, driven by the onboarding UI. Optionally enables
    /// portable mode - writing the sentinel **before** any data is created - then
    /// builds the store, `Service`, scheduler, and telemetry. Idempotent: a
    /// second call once the runtime is up is a no-op.
    pub async fn initialize(&self, portable: bool) -> Result<(), String> {
        if self.is_ready() {
            return Ok(());
        }
        if portable {
            enable_portable_sentinel()
                .map_err(|e| format!("could not enable portable mode: {e}"))?;
        }
        let portable_now = Paths::resolve().is_portable();
        let rt = build_runtime(portable_now).await.map_err(|e| e.to_string())?;
        *self.inner.lock().unwrap() = Some(rt);
        Ok(())
    }
}

/// Create dirs, connect the store, build the `Service`, start the scheduler, and
/// bring up telemetry, for the freshly resolved paths. Store connect comes first
/// so a failure returns before telemetry's global subscriber is set - keeping a
/// retry clean (the subscriber can only be installed once per process).
async fn build_runtime(portable: bool) -> anyhow::Result<Runtime> {
    let paths = Paths::resolve();
    paths.ensure_dirs()?;
    let config = poseiden_server::load_config();
    let store = Store::connect(&paths.database_path()).await?;
    let telemetry =
        poseiden_server::init_telemetry(&config.telemetry, "poseiden-app", paths.log_dir(), false);
    if portable {
        info!(root = %paths.data_root().display(), "portable mode - writes confined here");
    }
    let service: SharedService =
        Arc::new(Service::new(config, store, paths.az_sessions_dir()));
    let scheduler = Scheduler::spawn(service.clone());
    Ok(Runtime {
        service,
        _scheduler: scheduler,
        _telemetry: telemetry,
    })
}
