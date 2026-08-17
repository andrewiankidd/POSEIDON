//! POSEIDON's Tauri shell.
//!
//! Hosts the shared `frontend/web/` bundle in a native window (Windows / macOS
//! / Linux today; iOS / Android via Tauri Mobile). Two runtime modes, chosen
//! entirely on the frontend:
//!
//! - **Standalone** - this shell embeds a full [`poseidon_server::Service`] +
//!   [`poseidon_server::Scheduler`] over a local SQLite store, and the webview
//!   reaches it through the invoke handlers in [`commands`]. No socket is
//!   opened; the desktop app is self-contained.
//! - **Repointed** - the user sets an instance URL in Settings; the frontend
//!   then talks to that remote instance over HTTP and ignores the invoke
//!   handlers. The local Service still exists but simply isn't consulted.
//!
//! Either way the *logic* is `poseidon-server`'s `Service` - the same code the
//! Docker web instance runs. See `frontend/web/lib/api.js` for the dispatch.

mod commands;
mod state;

use crate::state::AppState;

/// Tauri 2 entry point. `mobile_entry_point` lets iOS/Android call the same
/// `run()` the desktop bin does.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Shell plugin - the frontend's external-link interceptor hands
        // work-item / pipeline URLs to the OS browser. Permissions in
        // capabilities/default.json.
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            // The store + Service + scheduler + telemetry are DEFERRED: a
            // returning user (their DB exists) is brought up immediately by
            // `AppState::boot`; a first run stays empty until the onboarding UI
            // calls `initialize`, so nothing is written before the user has
            // chosen local-vs-remote and a storage location. See `state.rs`.
            use tauri::Manager;
            app.manage(AppState::boot());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::is_initialized,
            commands::initialize,
            commands::get_teams,
            commands::add_team,
            commands::update_team,
            commands::remove_team,
            commands::get_auth,
            commands::get_doctor,
            commands::doctor_recheck,
            commands::doctor_fix,
            commands::sign_in,
            commands::quit,
            commands::toggle_devtools,
            commands::get_dashboard,
            commands::get_tickets,
            commands::get_ai_status,
            commands::run_tag_suggestions,
            commands::tag_suggestions_status,
            commands::store_tag_suggestions,
            commands::run_healthcheck_audit,
            commands::healthcheck_audit_status,
            commands::healthcheck_audit_prompts,
            commands::store_healthcheck_audit,
            commands::scan_duplicates,
            commands::get_llm_config,
            commands::set_llm_config,
            commands::reset_llm_config,
            commands::benchmark_llms,
            commands::get_tag_settings,
            commands::set_tag_settings,
            commands::work_item_descriptions,
            commands::update_work_item,
            commands::link_work_item_pr,
            commands::mark_work_item_duplicate,
            commands::pull_request_url,
            commands::update_rules,
            commands::update_team_rules,
            commands::get_pipelines,
            commands::get_pull_requests,
            commands::get_reports,
            commands::get_report_specs,
            commands::run_report_named,
            commands::run_report_spec,
            commands::save_report,
            commands::delete_report,
            commands::get_config,
            commands::export_config,
            commands::import_config,
            commands::import_catalog,
            commands::catalog,
            commands::set_active_team,
            commands::set_poll_all_teams,
            commands::log_client_error,
            commands::poll_now,
        ])
        .run(tauri::generate_context!())
        .expect("error while running POSEIDON");
}
