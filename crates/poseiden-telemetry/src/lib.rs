//! Centralised, portable observability for POSEIDEN (and reusable across Rust
//! apps). One [`init`] call wires a global `tracing` subscriber with any mix of
//! three sinks - console, rolling JSON file, and OTLP to a Grafana LGTM stack -
//! and returns a [`TelemetryGuard`] whose `Drop` flushes and shuts everything
//! down cleanly.
//!
//! # Why this crate exists
//!
//! Every other POSEIDEN crate logs through the `tracing` facade (`info!`,
//! `#[instrument]`, spans). None of them know or care where the data goes. This
//! crate is the single place that decides: the app code stays sink-agnostic, and
//! swapping Grafana for something else, or adding a file dump, is a change here
//! and nowhere else. Because the subscriber is global, *all* `tracing` output -
//! ours and our dependencies' - is captured the moment `init` runs.
//!
//! # Signals
//!
//! - **Logs** - `tracing` events → console + file + (OTLP) the collector's log
//!   pipeline (Loki).
//! - **Traces** - `tracing` spans → (OTLP) Tempo, via `tracing-opentelemetry`.
//! - **Metrics** - a meter provider (Mimir/Prometheus) plus a metrics layer that
//!   derives counters/histograms from specially-named span fields, so existing
//!   instrumentation yields metrics for free.
//!
//! # Verbosity
//!
//! Resolved in order: `RUST_LOG` env → the config `level` → the environment
//! default (dev `debug`, prod `warn`). See [`Environment`].
//!
//! # Example
//!
//! ```no_run
//! use poseiden_telemetry::{init, RuntimeInfo, Environment, TelemetrySettings};
//! # fn main() -> Result<(), poseiden_telemetry::TelemetryError> {
//! let settings = TelemetrySettings::default();
//! let rt = RuntimeInfo::new("my-service", "1.0.0", Environment::Dev, "./logs".into());
//! let _guard = init(&settings, rt)?; // keep the guard for the process lifetime
//! tracing::info!("telemetry is live");
//! # Ok(())
//! # }
//! ```

mod config;
#[cfg(feature = "otlp")]
mod otlp;

pub use config::{Environment, OtlpSettings, RuntimeInfo, TelemetrySettings};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

/// A type-erased sink layer over the base registry. Boxing lets a heterogeneous
/// set of sinks (console, file, OTLP) collect into one `Vec` under a single
/// global filter.
pub(crate) type BoxLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// Errors from telemetry setup. Deliberately coarse - a telemetry failure should
/// never be fatal to the host; callers typically log-and-continue.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// The global subscriber was already set (init called twice).
    #[error("telemetry already initialised")]
    AlreadyInitialised,
    /// A sink failed to build (bad OTLP endpoint, unwritable log dir, ...).
    #[error("telemetry sink setup failed: {0}")]
    Sink(String),
}

/// Keeps telemetry alive and shuts it down on drop. Hold it for the whole
/// process lifetime (bind it to a `let _guard = ...` in `main`); dropping it
/// flushes the file writer and any OTLP batch exporters so nothing is lost on
/// exit.
#[must_use = "dropping the guard immediately tears telemetry down"]
pub struct TelemetryGuard {
    // Keeps the non-blocking file writer's worker thread alive.
    _file: Option<tracing_appender::non_blocking::WorkerGuard>,
    #[cfg(feature = "otlp")]
    otel: Option<otlp::OtelProviders>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        #[cfg(feature = "otlp")]
        if let Some(otel) = self.otel.take() {
            otel.shutdown();
        }
    }
}

/// Initialise global telemetry from `settings` + runtime `rt`. Returns a guard
/// to hold for the process lifetime. Idempotency is the caller's concern: call
/// once, early, in `main`.
pub fn init(
    settings: &TelemetrySettings,
    rt: RuntimeInfo,
) -> Result<TelemetryGuard, TelemetryError> {
    let filter = build_filter(settings, rt.environment);

    // Every sink is an independently-toggled boxed layer. Boxing to
    // `Layer<Registry>` lets us collect a heterogeneous set into one Vec and
    // apply the global filter over all of them.
    let mut layers: Vec<BoxLayer> = Vec::new();

    if settings.console {
        layers.push(console_layer(settings, &rt));
    }

    let mut file_guard = None;
    if settings.file {
        let (layer, guard) = file_layer(&rt)?;
        layers.push(layer);
        file_guard = Some(guard);
    }

    #[cfg(feature = "otlp")]
    let otel = if settings.otlp.enabled {
        match otlp::build(&settings.otlp, &rt) {
            Ok((otel_layers, providers)) => {
                layers.extend(otel_layers);
                Some(providers)
            }
            Err(e) => {
                // Don't fail the host over telemetry - warn (once the subscriber
                // is up) and carry on with the local sinks.
                eprintln!("[telemetry] OTLP export disabled: {e}");
                None
            }
        }
    } else {
        None
    };

    Registry::default()
        .with(layers)
        .with(filter)
        .try_init()
        .map_err(|_| TelemetryError::AlreadyInitialised)?;

    if settings.otlp.enabled {
        #[cfg(not(feature = "otlp"))]
        tracing::warn!(
            "telemetry: otlp enabled in config but the crate was built without the `otlp` feature"
        );
    }

    Ok(TelemetryGuard {
        _file: file_guard,
        #[cfg(feature = "otlp")]
        otel,
    })
}

/// Build the global level filter. Order of precedence: `RUST_LOG` env, then the
/// config `level`, then the environment default - each layered with sane
/// suppression of chatty dependencies so `debug` doesn't drown in framework
/// internals.
fn build_filter(settings: &TelemetrySettings, env: Environment) -> EnvFilter {
    // RUST_LOG wins outright, verbatim.
    if std::env::var("RUST_LOG").is_ok() {
        return EnvFilter::from_default_env();
    }
    let base = settings
        .level
        .clone()
        .unwrap_or_else(|| env.default_level().to_string());
    // Suppress the usual noisy transitive crates regardless of the base level.
    let directives = format!(
        "{base},hyper=warn,h2=warn,tower=warn,tower_http=info,reqwest=warn,sqlx=warn,\
         tonic=warn,tokio_util=warn,opentelemetry=warn,opentelemetry_sdk=warn,\
         tao=warn,wry=warn,webview2_com=warn"
    );
    EnvFilter::new(directives)
}

/// The console sink: pretty text (dev) or JSON (collector-friendly), to stdout
/// or stderr per [`RuntimeInfo::console_stderr`].
fn console_layer(settings: &TelemetrySettings, rt: &RuntimeInfo) -> BoxLayer {
    let stderr = rt.console_stderr;
    if settings.console_json {
        let l = tracing_subscriber::fmt::layer().json().with_target(true);
        if stderr {
            l.with_writer(std::io::stderr).boxed()
        } else {
            l.boxed()
        }
    } else {
        let l = tracing_subscriber::fmt::layer().with_target(true);
        if stderr {
            l.with_writer(std::io::stderr).boxed()
        } else {
            l.boxed()
        }
    }
}

/// The file sink: a daily-rolling JSON log under the host's log dir, written on
/// a background thread (non-blocking). The returned worker guard must be kept
/// alive by the caller (it lives in [`TelemetryGuard`]).
fn file_layer(
    rt: &RuntimeInfo,
) -> Result<(BoxLayer, tracing_appender::non_blocking::WorkerGuard), TelemetryError> {
    let appender = tracing_appender::rolling::daily(&rt.log_dir, "poseiden.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let layer = tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_writer(writer)
        .boxed();
    Ok((layer, guard))
}
