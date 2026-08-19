//! POSEIDON server library.
//!
//! Exposes the [`Service`] (shared logic), the [`Scheduler`] (background
//! polling), the axum [`router`], and a [`serve`] convenience that wires them
//! together for the Docker/web binary. The Tauri shell depends on this crate
//! as a library - it constructs a `Service`, spawns a `Scheduler`, and calls
//! `Service` methods from its invoke handlers, without ever binding a socket.

mod checks;
mod config_store;
mod http;
mod scheduler;
mod service;
mod token;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use poseidon_core::PoseidonConfig;
use poseidon_store::Store;
use tracing::info;

pub use http::router;
pub use poseidon_ai::{AiConfig, LlmConfig, OFFLINE_MODELS, ONLINE_PROVIDERS};
pub use scheduler::Scheduler;
pub use service::{
    AuthStatus, BrowserAuditResult, BrowserSuggestion, DraftOutcome, PollOutcome, RefineOutcome,
    Service, SharedService, SigninState,
};

/// Env var pointing at the static frontend bundle. Set in the Docker image;
/// falls back to `frontend/web` relative to the working directory for local
/// `cargo run`.
pub const ENV_STATIC_DIR: &str = "POSEIDON_STATIC_DIR";

/// Instance-level configuration, sourced entirely from the environment.
///
/// There is no config *file*: per-owner config (teams, rules, tags, reports)
/// lives in the DB - set via the UI, or `poseidon config import` for headless /
/// GitOps - and instance settings (bind/port/poll, telemetry) come from env vars
/// (see [`ServerConfig::from_env`] / [`TelemetrySettings::from_env`]). This keeps
/// one source of truth per concern and makes hosting a matter of the Deployment's
/// env, with no ConfigMap to mount.
pub fn load_config() -> PoseidonConfig {
    PoseidonConfig {
        server: poseidon_core::ServerConfig::from_env(),
        telemetry: poseidon_telemetry::TelemetrySettings::from_env(),
    }
}

/// Initialise global telemetry for a named binary from a loaded config's
/// `[telemetry]` settings. Returns the guard to hold for the process lifetime
/// (dropping it flushes + tears telemetry down). A telemetry failure is logged,
/// never fatal. `console_stderr` keeps a CLI's stdout clean for `--json`.
///
/// The environment (which sets default verbosity: dev `debug`, prod `warn`) is
/// derived from the build - a `local` build is Dev, a stamped release is Prod.
/// Must be called from within a Tokio runtime when OTLP is enabled (its batch
/// exporters spawn background tasks); every POSEIDON binary is.
pub fn init_telemetry(
    settings: &poseidon_telemetry::TelemetrySettings,
    service_name: &str,
    log_dir: PathBuf,
    console_stderr: bool,
) -> Option<poseidon_telemetry::TelemetryGuard> {
    let environment = if poseidon_core::is_local_build() {
        poseidon_telemetry::Environment::Dev
    } else {
        poseidon_telemetry::Environment::Prod
    };
    let rt = poseidon_telemetry::RuntimeInfo::new(
        service_name,
        poseidon_core::version(),
        environment,
        log_dir,
    )
    .with_console_stderr(console_stderr);
    match poseidon_telemetry::init(settings, rt) {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("[telemetry] init failed ({e}); continuing without telemetry");
            None
        }
    }
}

/// Resolve the static-frontend directory from the environment.
pub fn static_dir() -> PathBuf {
    std::env::var(ENV_STATIC_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("frontend/web"))
}

/// Boot the full web instance: connect the store, build the service, start the
/// scheduler, and serve the API + frontend until the process is signalled.
/// This is what the Docker binary runs.
pub async fn serve(
    config: PoseidonConfig,
    db_path: &Path,
    static_dir: &Path,
    az_sessions_dir: &Path,
) -> anyhow::Result<()> {
    let bind = format!("{}:{}", config.server.bind_addr, config.server.port);
    let addr: SocketAddr = bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address {bind}: {e}"))?;

    let store = Store::connect(db_path).await?;
    // Optional defence-in-depth: when POSEIDON_AUTH_JWKS_URL is set, verify the
    // oauth2-proxy-forwarded access token instead of trusting the identity header.
    let verifier = token::TokenVerifier::from_env();
    if verifier.is_some() {
        info!("auth: verifying forwarded access tokens against the configured JWKS");
    }
    let service: SharedService = std::sync::Arc::new(
        Service::new(config, store, az_sessions_dir.to_path_buf()).with_verifier(verifier),
    );
    // AI taggers are built lazily per owner from each owner's persisted config
    // (see Service::ai_tagger), so there is nothing to preload here.

    // Hold the scheduler for the process lifetime - dropping it would abort
    // polling.
    let _scheduler = Scheduler::spawn(service.clone());

    let app = router(service, static_dir);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, static_dir = %static_dir.display(), "POSEIDON server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve to a ready future on Ctrl-C - lets the container stop cleanly.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received");
}

/// Normalise an optional `(from, to)` into RFC3339 bounds. Bare `YYYY-MM-DD`
/// dates are widened to cover the whole day (`from` → start, `to` → end);
/// full timestamps pass through. Missing values default to the last 30 days.
/// Shared by the HTTP `reports` route and the Tauri `get_reports` command so
/// both transports interpret ranges identically.
pub fn normalise_range(from: Option<&str>, to: Option<&str>) -> (String, String) {
    use chrono::{Duration, Utc};
    let now = Utc::now();
    let to = match to {
        Some(s) if !s.is_empty() => widen(s, true),
        _ => now.to_rfc3339(),
    };
    let from = match from {
        Some(s) if !s.is_empty() => widen(s, false),
        _ => (now - Duration::days(30)).to_rfc3339(),
    };
    (from, to)
}

/// Expand a `YYYY-MM-DD` date to a full RFC3339 timestamp at the start (or end)
/// of that day. Full timestamps pass through unchanged.
fn widen(s: &str, end_of_day: bool) -> String {
    if s.len() == 10 && s.as_bytes().get(4) == Some(&b'-') {
        if end_of_day {
            format!("{s}T23:59:59Z")
        } else {
            format!("{s}T00:00:00Z")
        }
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widen_expands_bare_dates() {
        assert_eq!(widen("2026-07-01", false), "2026-07-01T00:00:00Z");
        assert_eq!(widen("2026-07-31", true), "2026-07-31T23:59:59Z");
        assert_eq!(widen("2026-07-01T09:30:00Z", false), "2026-07-01T09:30:00Z");
    }

    #[test]
    fn normalise_range_defaults_to_last_30_days() {
        let (from, to) = normalise_range(None, None);
        let f = chrono::DateTime::parse_from_rfc3339(&from).unwrap();
        let t = chrono::DateTime::parse_from_rfc3339(&to).unwrap();
        assert!(f < t);
    }
}
