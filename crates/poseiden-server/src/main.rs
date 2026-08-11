//! POSEIDEN web instance binary - the Dockerised server.
//!
//! Resolves paths (honouring portable mode), loads instance config from the
//! environment, ensures the data dir exists, and hands off to
//! [`poseiden_server::serve`], which owns the store, scheduler, and HTTP loop.

use poseiden_paths::Paths;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let paths = Paths::resolve();
    paths.ensure_dirs()?;

    // Instance config (bind/port/poll + telemetry) is sourced from the
    // environment; per-owner config lives in the DB. No config file.
    let config = poseiden_server::load_config();

    // Telemetry comes up once config is known (it drives sinks + verbosity).
    // Hold the guard for the whole process so batched exporters flush on exit.
    let _telemetry = poseiden_server::init_telemetry(
        &config.telemetry,
        "poseiden-server",
        paths.log_dir(),
        false,
    );

    if paths.is_portable() {
        tracing::info!(root = %paths.data_root().display(), "portable mode - all writes confined here");
    }

    let db_path = paths.database_path();
    let static_dir = poseiden_server::static_dir();

    tracing::info!(db = %db_path.display(), "using database");
    poseiden_server::serve(config, &db_path, &static_dir, &paths.az_sessions_dir()).await
}
