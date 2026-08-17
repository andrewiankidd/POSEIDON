//! Tauri invoke handlers.
//!
//! Each command is a thin wrapper over the shared [`Service`] - the exact same
//! object the axum API calls. That's the whole point of the architecture: the
//! desktop shell and the web instance run identical logic, so "standalone" and
//! "hosted" can never diverge in behaviour. When the frontend is repointed at a
//! remote instance it stops calling these entirely and uses `fetch` instead -
//! the decision lives in `frontend/web/lib/api.js`, not here.

use crate::state::AppState;
use tauri::{AppHandle, Emitter, State};

/// JSON value or a stringified error - mirrors the HTTP API's error shape so
/// the frontend's two code paths (invoke vs fetch) surface failures the same
/// way.
type CmdResult = Result<serde_json::Value, String>;

fn to_value<T: serde::Serialize>(v: T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

/// Empty-string team → all teams, matching the HTTP side's `scope()`.
fn scope(team: &Option<String>) -> Option<&str> {
    team.as_deref().filter(|s| !s.is_empty())
}

#[tauri::command]
pub async fn get_teams(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    Ok(to_value(
        service.team_names().await.map_err(|e| e.to_string())?,
    ))
}

/// Current auth state (PAT / az / not signed in) for the sign-in banner.
#[tauri::command]
pub async fn get_auth(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    Ok(to_value(service.auth_status().await))
}

/// Doctor health report for the status indicator + panel.
#[tauri::command]
pub async fn get_doctor(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    Ok(to_value(service.doctor_report().await))
}

/// Run the Doctor WITH auto-fixes and return the fresh report (Re-check).
#[tauri::command]
pub async fn doctor_recheck(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    Ok(to_value(service.doctor_tick().await))
}

/// Add a team at runtime + persist. `team` is a TeamConfig-shaped object.
#[tauri::command]
pub async fn add_team(state: State<'_, AppState>, team: poseidon_core::TeamConfig) -> CmdResult {
    let service = state.service()?;
    let added = service.add_team(team).await.map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "added": added }))
}

/// Edit a work item's state / tags - writes through to Azure DevOps, returns
/// the updated item.
#[tauri::command]
pub async fn update_work_item(
    app_state: State<'_, AppState>,
    team: String,
    id: i64,
    state: Option<String>,
    tags: Option<Vec<String>>,
) -> CmdResult {
    let service = app_state.service()?;
    let update = poseidon_core::WorkItemUpdate { state, tags };
    let (item, flags) = service
        .update_work_item(&team, id, update)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "item": item, "flags": flags }))
}

/// Add (`link = true`) or remove a work-item <-> PR link - writes the ADO
/// artifact-link relation through, returns the updated item + its flags.
#[tauri::command]
pub async fn link_work_item_pr(
    state: State<'_, AppState>,
    team: String,
    work_item_id: i64,
    pr_id: i64,
    link: bool,
) -> CmdResult {
    let service = state.service()?;
    let (item, flags) = service
        .link_work_item_pr(&team, work_item_id, pr_id, link)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "item": item, "flags": flags }))
}

/// Mark a work item as a duplicate of another via the provider's native mechanism.
#[tauri::command]
pub async fn mark_work_item_duplicate(
    state: State<'_, AppState>,
    team: String,
    work_item_id: i64,
    duplicate_of: i64,
) -> CmdResult {
    let service = state.service()?;
    let (item, flags) = service
        .mark_work_item_duplicate(&team, work_item_id, duplicate_of)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "item": item, "flags": flags }))
}

/// Resolve a single PR by id (for a linked-PR chip with no stored URL/status).
#[tauri::command]
pub async fn pull_request_url(state: State<'_, AppState>, team: String, pr_id: i64) -> CmdResult {
    let service = state.service()?;
    let pr = service
        .resolve_pull_request(&team, pr_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "url": pr.url, "status": pr.status, "is_draft": pr.is_draft }))
}

/// Update a team (matched by `original` name) with a new definition.
#[tauri::command]
pub async fn update_team(
    state: State<'_, AppState>,
    original: String,
    team: poseidon_core::TeamConfig,
) -> CmdResult {
    let service = state.service()?;
    let updated = service
        .update_team(&original, team)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "updated": updated }))
}

/// Remove a team by name.
#[tauri::command]
pub async fn remove_team(state: State<'_, AppState>, name: String) -> CmdResult {
    let service = state.service()?;
    let removed = service
        .remove_team(&name)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "removed": removed }))
}

/// Replace the instance-wide default ruleset (`[rules]`).
#[tauri::command]
pub async fn update_rules(state: State<'_, AppState>, rules: poseidon_core::RuleSet) -> CmdResult {
    let service = state.service()?;
    service
        .update_rules(rules)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "updated": true }))
}

/// Set (`Some`) or clear (`None`) a team's `[team.rules]` override.
#[tauri::command]
pub async fn update_team_rules(
    state: State<'_, AppState>,
    team: String,
    rules: Option<poseidon_core::RuleSet>,
) -> CmdResult {
    let service = state.service()?;
    let found = service
        .update_team_rules(&team, rules)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "updated": found }))
}

/// Close the app window (File → Quit).
#[tauri::command]
pub async fn quit(window: tauri::WebviewWindow) -> Result<(), String> {
    window.close().map_err(|e| e.to_string())
}

/// Toggle the webview developer tools (View → Toggle Developer Tools). Only
/// wired in debug builds - the API doesn't exist in release.
#[tauri::command]
pub async fn toggle_devtools(window: tauri::WebviewWindow) {
    #[cfg(debug_assertions)]
    {
        if window.is_devtools_open() {
            window.close_devtools();
        } else {
            window.open_devtools();
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = window;
    }
}

/// Run one check's server-side fix (Doctor panel Fix button, non-interactive).
#[tauri::command]
pub async fn doctor_fix(state: State<'_, AppState>, id: String) -> CmdResult {
    let service = state.service()?;
    match service.doctor_fix(&id).await {
        Some(result) => Ok(to_value(result)),
        None => Err(format!("no check with id '{id}'")),
    }
}

/// Interactive sign-in via the Azure CLI **device-code** flow.
///
/// Device code (not the default browser-redirect flow) is used deliberately: it
/// needs no `localhost` listener, which corporate networks / conditional-access
/// routinely break - so it works the same on every coworker's machine. `az`
/// prints the code + URL to stderr; we stream it and emit an `auth-device-code`
/// Tauri event so the UI can show "go to <url> and enter <code>". The command
/// stays pending until `az` finishes (the user completes the browser step),
/// then returns the refreshed auth status.
///
/// `--allow-no-subscriptions` so accounts with Azure DevOps access but no Azure
/// subscription (common) still sign in. `--tenant` targets the org directly.
#[tauri::command]
#[tracing::instrument(skip(app, state))]
pub async fn sign_in(app: AppHandle, state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    // Native device-code sign-in (no Azure CLI): the prompt comes back at once,
    // and the grant completes on the service's background poll. Same flow the
    // hosted web instance uses.
    let prompt = service
        .start_web_sign_in()
        .await
        .map_err(|e| e.to_string())?;
    let _ = app.emit(
        "auth-device-code",
        serde_json::json!({ "url": prompt.url, "code": prompt.code }),
    );
    tracing::info!("device code emitted; awaiting completion");

    // Poll the sign-in state until it resolves (bounded ~15 min, the device-code
    // lifetime). `sign_in_status` reflects the background poll's outcome.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15 * 60);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        match service.sign_in_status() {
            poseidon_server::SigninState::Done => {
                tracing::info!("device-code sign-in completed");
                return Ok(to_value(service.auth_status().await));
            }
            poseidon_server::SigninState::Failed { error } => {
                return Err(format!("sign-in did not complete: {error}"));
            }
            _ => {}
        }
        if std::time::Instant::now() > deadline {
            return Err("sign-in timed out".into());
        }
    }
}

#[tauri::command]
pub async fn get_dashboard(state: State<'_, AppState>, team: Option<String>) -> CmdResult {
    let service = state.service()?;
    service
        .dashboard(scope(&team))
        .await
        .map(to_value)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_tickets(state: State<'_, AppState>, team: Option<String>) -> CmdResult {
    let service = state.service()?;
    let team = scope(&team);
    let items = service.work_items(team).await.map_err(|e| e.to_string())?;
    let flags = service.flags(team).await.map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "items": items, "flags": flags }))
}

#[tauri::command]
pub async fn get_ai_status(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    Ok(serde_json::json!({ "enabled": service.ai_enabled().await }))
}

/// Start a tag-suggestion run in the background and return the initial status;
/// the UI polls `tag_suggestions_status` (inference is slow).
#[tauri::command]
pub async fn run_tag_suggestions(
    state: State<'_, AppState>,
    team: Option<String>,
    ids: Option<Vec<i64>>,
) -> CmdResult {
    let service = state.service()?;
    service
        .start_tag_suggestions(team, ids.filter(|v| !v.is_empty()))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(service.tag_suggestions_status()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn tag_suggestions_status(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    serde_json::to_value(service.tag_suggestions_status()).map_err(|e| e.to_string())
}

/// Store browser (WebGPU) computed suggestions; the service re-validates them.
#[tauri::command]
pub async fn store_tag_suggestions(
    state: State<'_, AppState>,
    team: Option<String>,
    items: Vec<poseidon_server::BrowserSuggestion>,
) -> CmdResult {
    let service = state.service()?;
    let summary = service
        .store_tag_suggestions(team.as_deref(), items)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(summary).map_err(|e| e.to_string())
}

/// Start an on-demand AI healthcheck audit in the background (server-side model);
/// the UI polls `healthcheck_audit_status`.
#[tauri::command]
pub async fn run_healthcheck_audit(
    state: State<'_, AppState>,
    team: Option<String>,
    ids: Option<Vec<i64>>,
) -> CmdResult {
    let service = state.service()?;
    service
        .start_healthcheck_audit(team, ids.filter(|v| !v.is_empty()))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(service.healthcheck_audit_status()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn healthcheck_audit_status(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    serde_json::to_value(service.healthcheck_audit_status()).map_err(|e| e.to_string())
}

/// Per-item audit prompts for the browser (WebGPU) path.
#[tauri::command]
pub async fn healthcheck_audit_prompts(
    state: State<'_, AppState>,
    team: Option<String>,
    ids: Option<Vec<i64>>,
) -> CmdResult {
    let service = state.service()?;
    let prompts = service
        .audit_prompts(team.as_deref(), ids.filter(|v| !v.is_empty()).as_deref())
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(prompts).map_err(|e| e.to_string())
}

/// Store browser (WebGPU) computed audit replies; the service re-parses them.
#[tauri::command]
pub async fn store_healthcheck_audit(
    state: State<'_, AppState>,
    team: Option<String>,
    results: Vec<poseidon_server::BrowserAuditResult>,
) -> CmdResult {
    let service = state.service()?;
    let summary = service
        .store_healthcheck_audit(team.as_deref(), results)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(summary).map_err(|e| e.to_string())
}

/// Run the deterministic near-duplicate scan over a team's items and store the flags.
#[tauri::command]
pub async fn scan_duplicates(state: State<'_, AppState>, team: Option<String>) -> CmdResult {
    let service = state.service()?;
    let summary = service
        .run_duplicate_scan(team.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_value(summary).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_llm_config(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    Ok(service.llm_config_view().await)
}

#[tauri::command]
pub async fn set_llm_config(
    state: State<'_, AppState>,
    config: poseidon_server::LlmConfig,
) -> CmdResult {
    let service = state.service()?;
    service
        .set_llm_config(config)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "enabled": service.ai_enabled().await }))
}

#[tauri::command]
pub async fn reset_llm_config(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    service
        .reset_llm_config()
        .await
        .map_err(|e| e.to_string())?;
    Ok(service.llm_config_view().await)
}

#[tauri::command]
pub async fn benchmark_llms(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    Ok(service.benchmark_llms().await)
}

#[tauri::command]
pub async fn get_tag_settings(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    Ok(serde_json::json!({ "use_description": service.tag_use_description().await }))
}

#[tauri::command]
pub async fn set_tag_settings(state: State<'_, AppState>, use_description: bool) -> CmdResult {
    let service = state.service()?;
    service
        .set_tag_use_description(use_description)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "use_description": use_description }))
}

#[tauri::command]
pub async fn work_item_descriptions(
    state: State<'_, AppState>,
    team: Option<String>,
    ids: Vec<i64>,
) -> CmdResult {
    let service = state.service()?;
    let map = service
        .work_item_descriptions(team.as_deref(), &ids)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::to_value(map).unwrap_or_default())
}

#[tauri::command]
pub async fn get_pipelines(state: State<'_, AppState>, team: Option<String>) -> CmdResult {
    let service = state.service()?;
    service
        .pipeline_health(scope(&team))
        .await
        .map(to_value)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_pull_requests(state: State<'_, AppState>, team: Option<String>) -> CmdResult {
    let service = state.service()?;
    service
        .pull_requests(scope(&team))
        .await
        .map(to_value)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_reports(
    state: State<'_, AppState>,
    from: Option<String>,
    to: Option<String>,
    team: Option<String>,
) -> CmdResult {
    let service = state.service()?;
    let (from, to) = poseidon_server::normalise_range(from.as_deref(), to.as_deref());
    let team = scope(&team);
    let tickets = service
        .ticket_report(&from, &to, team)
        .await
        .map_err(|e| e.to_string())?;
    let pipelines = service
        .pipeline_report(&from, &to, team)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "range": { "from": from, "to": to },
        "tickets": tickets,
        "pipelines": pipelines,
    }))
}

#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    service.config().await.map_err(|e| e.to_string())
}

/// Record the UI's selected team so the active-team poll fetches it. `null`
/// clears it (the "All teams" view).
#[tauri::command]
pub async fn set_active_team(state: State<'_, AppState>, team: Option<String>) -> CmdResult {
    let service = state.service()?;
    service
        .set_active_team(scope(&team))
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "ok": true }))
}

/// Toggle whether polls fetch all teams or just the active one.
#[tauri::command]
pub async fn set_poll_all_teams(state: State<'_, AppState>, all: bool) -> CmdResult {
    let service = state.service()?;
    service
        .set_poll_all_teams(all)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "poll_all_teams": all }))
}

/// Forward an uncaught frontend error into the backend telemetry/log stream.
#[tauri::command]
pub async fn log_client_error(
    state: State<'_, AppState>,
    message: String,
    stack: Option<String>,
    url: Option<String>,
) -> CmdResult {
    let service = state.service()?;
    service.log_client_error(&message, stack.as_deref(), url.as_deref());
    Ok(serde_json::json!({ "ok": true }))
}

// ── Configurable reports ───────────────────────────────────────────────

#[tauri::command]
pub async fn get_report_specs(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    let specs = service.report_specs().await.map_err(|e| e.to_string())?;
    Ok(to_value(specs))
}

#[tauri::command]
pub async fn run_report_named(
    state: State<'_, AppState>,
    name: String,
    team: Option<String>,
) -> CmdResult {
    let service = state.service()?;
    let result = service
        .run_report_named(&name, scope(&team))
        .await
        .map_err(|e| e.to_string())?;
    Ok(to_value(result))
}

#[tauri::command]
pub async fn run_report_spec(
    state: State<'_, AppState>,
    spec: poseidon_core::ReportSpec,
    team: Option<String>,
) -> CmdResult {
    let service = state.service()?;
    let result = service
        .run_report_spec(spec, scope(&team))
        .await
        .map_err(|e| e.to_string())?;
    Ok(to_value(result))
}

#[tauri::command]
pub async fn save_report(state: State<'_, AppState>, spec: poseidon_core::ReportSpec) -> CmdResult {
    let service = state.service()?;
    service.save_report(spec).await.map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "saved": true }))
}

#[tauri::command]
pub async fn delete_report(state: State<'_, AppState>, name: String) -> CmdResult {
    let service = state.service()?;
    let removed = service
        .delete_report(&name)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "deleted": removed }))
}

/// Manual poll trigger - backs the desktop "refresh now" button. Returns the
/// same [`PollOutcome`] shape the HTTP `/api/poll` route does.
#[tauri::command]
pub async fn poll_now(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    Ok(to_value(service.poll_once().await))
}

/// Export this owner's config as a YAML string (the UI offers it as a download).
#[tauri::command]
pub async fn export_config(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    let yaml = service.export_config().await.map_err(|e| e.to_string())?;
    Ok(serde_json::Value::String(yaml))
}

/// Import a YAML config document into this owner. `replace` overwrites; otherwise
/// merges. Returns an import summary.
#[tauri::command]
pub async fn import_config(state: State<'_, AppState>, yaml: String, replace: bool) -> CmdResult {
    let service = state.service()?;
    let summary = service
        .import_config(&yaml, replace)
        .await
        .map_err(|e| e.to_string())?;
    Ok(to_value(summary))
}

/// Import a service catalog from a CSV export (desktop transport for the CSV
/// `CatalogSource` - the same `Service::sync_catalog_csv` the HTTP route calls).
#[tauri::command]
pub async fn import_catalog(state: State<'_, AppState>, csv: String) -> CmdResult {
    let service = state.service()?;
    let rows = service
        .sync_catalog_csv(&csv)
        .await
        .map_err(|e| e.to_string())?;
    Ok(to_value(serde_json::json!({ "rows": rows })))
}

/// The owner's synced service catalog (rows + count), for the Settings catalog panel.
#[tauri::command]
pub async fn catalog(state: State<'_, AppState>) -> CmdResult {
    let service = state.service()?;
    let rows = service.catalog().await.map_err(|e| e.to_string())?;
    Ok(to_value(
        serde_json::json!({ "count": rows.len(), "rows": rows }),
    ))
}

/// Whether the local runtime is set up (store + `Service` built). The onboarding
/// UI checks this on launch - `false` on a first run means show the setup flow;
/// once `initialize` has run (or a returning user booted straight in) it's `true`.
#[tauri::command]
pub fn is_initialized(state: State<'_, AppState>) -> bool {
    state.is_ready()
}

/// Complete first-run setup for a LOCAL install: optionally enable portable mode
/// (all data confined beside the app - written before anything else), then create
/// the store + `Service` + scheduler. Driven by onboarding after the user picks
/// "run locally" and a storage location. A repointed (remote) client never calls
/// this - it has no local runtime.
#[tauri::command]
pub async fn initialize(state: State<'_, AppState>, portable: bool) -> CmdResult {
    state.initialize(portable).await?;
    Ok(serde_json::json!({ "ready": true }))
}
