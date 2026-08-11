//! Telemetry configuration: the [`TelemetrySettings`] a deployment tunes (via
//! environment variables - see `TelemetrySettings::from_env`) and the runtime
//! [`RuntimeInfo`] the host binary injects (service identity + where files may
//! be written).
//!
//! The split keeps the crate app-agnostic: nothing here knows it's POSEIDEN.
//! A future app reuses it by constructing its own [`RuntimeInfo`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which sinks are on and how loud (see [`TelemetrySettings::from_env`]). Every
/// field defaults, so an unconfigured environment is valid and yields
/// console-only logging at the environment default level.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetrySettings {
    /// Human-readable console output. On by default (dev ergonomics). In a
    /// container you may leave it on (captured by the platform's log driver) or
    /// off in favour of OTLP.
    pub console: bool,
    /// Emit console lines as JSON rather than pretty text. Off by default;
    /// turn on when a log collector scrapes stdout and wants structured lines.
    pub console_json: bool,
    /// Rolling JSON log file under the host's log dir. Off by default - opt in
    /// for a local audit trail without standing up a collector.
    pub file: bool,
    /// Explicit filter directive (`RUST_LOG` syntax, e.g. `"debug,sqlx=warn"`).
    /// `None` = use the environment default (dev: debug, prod: warn). The
    /// `RUST_LOG` env var, if set, overrides even this.
    pub level: Option<String>,
    /// OpenTelemetry / Grafana LGTM export.
    pub otlp: OtlpSettings,
}

impl Default for TelemetrySettings {
    fn default() -> Self {
        Self {
            console: true,
            console_json: false,
            file: false,
            level: None,
            otlp: OtlpSettings::default(),
        }
    }
}

impl TelemetrySettings {
    /// Build from environment variables - the sole config source now that the
    /// TOML file is gone. All optional; the defaults give console-only logging.
    /// `RUST_LOG` still overrides `level` at runtime.
    ///
    /// - `POSEIDEN_LOG_CONSOLE` / `POSEIDEN_LOG_CONSOLE_JSON` / `POSEIDEN_LOG_FILE` - bools
    /// - `POSEIDEN_LOG_LEVEL` - explicit `RUST_LOG`-syntax filter
    /// - `POSEIDEN_OTLP_ENDPOINT` - set it to enable OTLP export to that collector
    /// - `POSEIDEN_OTLP_ENABLED` - force-enable/disable independently of the endpoint
    pub fn from_env() -> Self {
        fn flag(key: &str, default: bool) -> bool {
            std::env::var(key)
                .ok()
                .map(|v| {
                    matches!(
                        v.trim().to_ascii_lowercase().as_str(),
                        "1" | "true" | "yes" | "on"
                    )
                })
                .unwrap_or(default)
        }
        let mut s = Self::default();
        s.console = flag("POSEIDEN_LOG_CONSOLE", s.console);
        s.console_json = flag("POSEIDEN_LOG_CONSOLE_JSON", s.console_json);
        s.file = flag("POSEIDEN_LOG_FILE", s.file);
        s.level = std::env::var("POSEIDEN_LOG_LEVEL")
            .ok()
            .filter(|v| !v.trim().is_empty());
        if let Ok(ep) = std::env::var("POSEIDEN_OTLP_ENDPOINT") {
            if !ep.trim().is_empty() {
                s.otlp.endpoint = ep;
                s.otlp.enabled = true;
            }
        }
        s.otlp.enabled = flag("POSEIDEN_OTLP_ENABLED", s.otlp.enabled);
        s
    }
}

/// OTLP export settings. Disabled by default so a fresh instance never blocks or
/// errors trying to reach a collector that isn't there; a dev enables it (with
/// the local `grafana/otel-lgtm` stack) via `POSEIDEN_OTLP_ENDPOINT`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OtlpSettings {
    /// Master switch for all three OTLP signals (traces, logs, metrics).
    pub enabled: bool,
    /// Collector base endpoint (OTLP/HTTP). The per-signal paths (`/v1/traces`
    /// etc.) are appended automatically. `grafana/otel-lgtm` listens here.
    pub endpoint: String,
}

impl Default for OtlpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "http://localhost:4318".to_string(),
        }
    }
}

/// Deployment environment, which sets the default verbosity when no explicit
/// `level` / `RUST_LOG` is given. The host binary decides which it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    /// Local development: verbose (`debug`) by default.
    Dev,
    /// Deployed: quiet (`warn`) by default - best practice for production noise.
    Prod,
}

impl Environment {
    /// The default filter directive for this environment (before dep-noise
    /// suppression is layered on).
    pub fn default_level(self) -> &'static str {
        match self {
            Environment::Dev => "debug",
            Environment::Prod => "warn",
        }
    }
}

/// Runtime facts the host binary injects at init: who is emitting, and where
/// files may be written. Kept out of the settings so config stays portable and
/// secret-free, and so paths always resolve through the host's path policy
/// (portable mode etc.) rather than being hand-typed.
#[derive(Debug, Clone)]
pub struct RuntimeInfo {
    /// `service.name` on every span/log/metric (e.g. `"poseiden-server"`).
    pub service_name: String,
    /// `service.version` resource attribute (e.g. the build's version string).
    pub service_version: String,
    /// Dev vs prod - drives the default verbosity.
    pub environment: Environment,
    /// Directory the file sinks write under. Must already exist / be writable;
    /// the host resolves it (POSEIDEN uses `Paths::log_dir()`).
    pub log_dir: PathBuf,
    /// Send console output to stderr instead of stdout. The CLI sets this so
    /// `--json` on stdout stays clean; servers leave it false.
    pub console_stderr: bool,
}

impl RuntimeInfo {
    /// Construct with the common defaults (stdout console).
    pub fn new(
        service_name: impl Into<String>,
        service_version: impl Into<String>,
        environment: Environment,
        log_dir: PathBuf,
    ) -> Self {
        Self {
            service_name: service_name.into(),
            service_version: service_version.into(),
            environment,
            log_dir,
            console_stderr: false,
        }
    }

    /// Route console output to stderr (for CLIs that emit data on stdout).
    pub fn with_console_stderr(mut self, yes: bool) -> Self {
        self.console_stderr = yes;
        self
    }
}
