//! The axum HTTP API + static-frontend serving.
//!
//! Every route is a thin wrapper over [`Service`] - the handlers here do no
//! logic beyond parsing query params and shaping the response. The Tauri shell
//! reuses the same `Service` via invoke handlers, so these two transports stay
//! in lockstep by construction.
//!
//! Authentication is delegated to the deployment, not handled in-app: an ingress
//! / oauth2-proxy (or the Istio mesh's ext-authz) authenticates the user and
//! injects their email as the `X-Auth-Request-Email` header. The [`Scoped`]
//! extractor maps that header to the request's `owner`, so every user sees only
//! their own teams, rules, and reports (multi-tenant). With no header - a
//! standalone or unauthenticated local instance - the owner falls back to the
//! single-tenant default, so nothing changes there. CORS is permissive so a
//! repointed client (a mobile app whose webview origin differs) can call the API.

use std::path::Path;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{delete, get, post, put};
use axum::Router;
use serde::Deserialize;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::service::{Service, SharedService};

/// A [`Service`] scoped to the request's owner. In a hosted (multi-tenant)
/// deployment the owner is the authenticated user's email, injected as the
/// `X-Auth-Request-Email` header by the ingress / oauth2-proxy; when the header
/// is absent (standalone, or an unauthenticated local instance) it falls back to
/// the single-tenant default. Derefs to `Service`, so handlers call methods
/// exactly as before - they just operate on the right owner's data.
struct Scoped(Service);

impl axum::extract::FromRequestParts<SharedService> for Scoped {
    // Fails closed (401) when a token verifier is configured and the forwarded
    // token is missing / invalid; otherwise never fails (plain header trust).
    type Rejection = axum::response::Response;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &SharedService,
    ) -> Result<Self, Self::Rejection> {
        let owner: String = match state.verifier() {
            // Defence-in-depth: derive the owner from the cryptographically
            // verified access token, not the plaintext (spoofable) email header.
            Some(verifier) => {
                let token = parts
                    .headers
                    .get("x-auth-request-access-token")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                match verifier.verify_email(token).await {
                    Ok(email) => email,
                    Err(e) => {
                        tracing::warn!("rejected request: {e}");
                        return Err((
                            axum::http::StatusCode::UNAUTHORIZED,
                            "token verification failed",
                        )
                            .into_response());
                    }
                }
            }
            // No verifier: trust the header the ingress injected (the default;
            // the tenant boundary then rests on networkPolicy - see the chart).
            None => parts
                .headers
                .get("x-auth-request-email")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string(),
        };
        Ok(Scoped(state.with_owner(&owner)))
    }
}

impl std::ops::Deref for Scoped {
    type Target = Service;
    fn deref(&self) -> &Service {
        &self.0
    }
}

/// Build the full application router: API routes with the shared service as
/// state, plus a static-file fallback serving the frontend bundle.
pub fn router(service: SharedService, static_dir: &Path) -> Router {
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/auth", get(auth))
        .route("/api/identity", get(identity))
        .route("/api/sign-in", get(sign_in_status).post(start_sign_in))
        .route("/api/doctor", get(doctor).post(doctor_tick))
        .route("/api/doctor/fix/{id}", post(doctor_fix))
        .route("/api/teams", get(teams).post(add_team))
        .route("/api/teams/{name}", put(update_team).delete(remove_team))
        .route("/api/rules", put(update_rules))
        .route(
            "/api/teams/{name}/rules",
            put(update_team_rules).delete(clear_team_rules),
        )
        .route("/api/dashboard", get(dashboard))
        .route("/api/tickets", get(tickets))
        .route("/api/work-items/{id}", put(update_work_item))
        .route("/api/work-items/{id}/pr-link", post(link_work_item_pr))
        .route("/api/pipelines", get(pipelines))
        .route("/api/pull-requests", get(pull_requests))
        .route("/api/pull-requests/{id}/url", get(pull_request_url))
        .route("/api/reports", get(reports))
        .route("/api/reports/specs", get(report_specs).post(save_report))
        .route("/api/reports/specs/{name}", delete(delete_report))
        .route("/api/reports/run/{name}", get(run_report_named))
        .route("/api/reports/run", post(run_report_spec))
        .route("/api/config", get(config))
        .route("/api/config/export", get(export_config))
        .route("/api/config/import", post(import_config))
        .route("/api/active-team", post(set_active_team))
        .route("/api/settings/poll-all-teams", post(set_poll_all_teams))
        .route("/api/client-error", post(log_client_error))
        .route("/api/poll", post(poll))
        .route("/api/ai/status", get(ai_status))
        .route("/api/llm-config", get(llm_config_get).post(llm_config_set))
        .route("/api/llm-config/reset", post(llm_config_reset))
        .route("/api/llm-benchmark", post(llm_benchmark))
        .route(
            "/api/tag-settings",
            get(tag_settings_get).post(tag_settings_set),
        )
        .route("/api/work-items/descriptions", post(work_item_descriptions))
        .route("/api/tag-suggestions/run", post(run_tag_suggestions))
        .route("/api/tag-suggestions/status", get(tag_suggestions_status))
        .route("/api/tag-suggestions/store", post(store_tag_suggestions))
        .route("/env.js", get(env_js))
        .with_state(service);

    // Static frontend. `append_index_html_on_directories` serves index.html at
    // `/`. Unknown paths fall through to the API's 404 - the frontend is a
    // hash-routed single page, so it never needs deep-link rewriting.
    let static_service = ServeDir::new(static_dir).append_index_html_on_directories(true);

    api.fallback_service(static_service)
        // `no-cache` = the browser MUST revalidate before reusing a cached asset, so
        // a redeploy is picked up on an ordinary refresh (no hard-refresh needed) -
        // ServeDir's Last-Modified/ETag makes that a cheap 304 when unchanged.
        // `if_not_present` leaves handlers that set their own (e.g. env.js's no-store).
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache"),
        ))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
}

/// Map any error to a 500 with a JSON body. Callers can't do much but retry, so
/// a plain message is enough.
fn err500(e: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
}

type ApiResult = Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)>;

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "poseiden" }))
}

/// Runtime environment for the browser. The frontend runs client-side and can't
/// read the server's env vars, so it loads this synchronously (in `<head>`,
/// before app.js) and reads `window.__POSEIDEN_ENV__`. Values are resolved from
/// env at *request* time, so one static bundle serves any deployment - no build.
/// Currently exposes `instanceUrl` (from `POSEIDEN_REMOTE_URL`): when set, the
/// served frontend boots as a remote client of that instance (used by the Helm
/// `localhost` mode's client pod). Served `no-store` so a stale cache can't pin it.
async fn env_js() -> impl IntoResponse {
    // Version/build stamp shown in the sidebar foot. CI passes POSEIDEN_COMMIT (git
    // sha) + POSEIDEN_VERSION (tag); a local `poseiden.sh` build passes a timestamp
    // as POSEIDEN_COMMIT. Prefer the commit/build stamp, else the version tag, else
    // "dev"; a long git sha is shortened for display.
    let commit = std::env::var("POSEIDEN_COMMIT").unwrap_or_default();
    let ver = std::env::var("POSEIDEN_VERSION").unwrap_or_default();
    let raw = if !commit.is_empty() && commit != "unknown" {
        commit
    } else if !ver.is_empty() && ver != "latest-main" {
        ver
    } else {
        "dev".to_string()
    };
    let version = if raw.len() > 12 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        raw[..12].to_string() // git sha -> short form; timestamps/tags stay whole
    } else {
        raw
    };
    let env = serde_json::json!({
        "instanceUrl": std::env::var("POSEIDEN_REMOTE_URL").unwrap_or_default(),
        "version": version,
    });
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        format!("window.__POSEIDEN_ENV__ = {env};\n"),
    )
}

/// Whether POSEIDEN can authenticate (PAT or `az`), for the sign-in banner.
async fn auth(svc: Scoped) -> ApiResult {
    Ok(Json(serde_json::to_value(svc.auth_status().await).unwrap()))
}

/// The caller's proxy identity, for the UI's signed-in indicator. Distinct from
/// `/api/auth` (the provider credential): this echoes the identity the
/// ingress injected via `X-Auth-Request-Email`. `authenticated` is false when
/// there is no such header (desktop / local / an unauthenticated instance), in
/// which case the owner falls back to `default` and the UI shows no user menu.
async fn identity(headers: axum::http::HeaderMap) -> ApiResult {
    let email = headers
        .get("x-auth-request-email")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim();
    let authenticated = !email.is_empty();
    let owner = if authenticated {
        email
    } else {
        poseiden_core::DEFAULT_OWNER
    };
    Ok(Json(
        serde_json::json!({ "owner": owner, "authenticated": authenticated }),
    ))
}

/// Begin a hosted **device-code** sign-in for the request's owner. Kicks off
/// `az login` against that owner's isolated token cache and returns the prompt
/// (`{ url, code }`) for the browser to display; the browser then polls
/// `GET /api/sign-in` until the state is `done`/`failed`. The desktop app uses
/// its own Tauri command instead - this is the web equivalent.
async fn start_sign_in(svc: Scoped) -> ApiResult {
    match svc.start_web_sign_in().await {
        Ok(code) => Ok(Json(serde_json::to_value(code).unwrap())),
        Err(e) => Err(err500(e)),
    }
}

/// Current device-code sign-in state for the request's owner (idle / pending /
/// done / failed). Polled by the browser after `POST /api/sign-in`.
async fn sign_in_status(svc: Scoped) -> ApiResult {
    Ok(Json(serde_json::to_value(svc.sign_in_status()).unwrap()))
}

/// Doctor health report - the traffic-light + per-check list (no fixes).
async fn doctor(svc: Scoped) -> ApiResult {
    Ok(Json(
        serde_json::to_value(svc.doctor_report().await).unwrap(),
    ))
}

/// Run the Doctor WITH auto-fixes (registers team checks, etc.) and return the
/// fresh report - the "Re-check" button + used right after adding a team.
async fn doctor_tick(svc: Scoped) -> ApiResult {
    Ok(Json(serde_json::to_value(svc.doctor_tick().await).unwrap()))
}

/// Add a team at runtime + persist. Body is a `TeamConfig` JSON (name +
/// organization + project required; area_path / tenant optional). The Doctor's
/// reconciler registers its access check on the next tick.
async fn add_team(svc: Scoped, Json(team): Json<poseiden_core::TeamConfig>) -> ApiResult {
    match svc.add_team(team).await {
        Ok(added) => Ok(Json(serde_json::json!({ "added": added }))),
        Err(e) => Err(err500(e)),
    }
}

/// Update a team (matched by path `name`) with the JSON body.
/// Body for a work-item edit: which team owns it + the fields to change.
#[derive(serde::Deserialize)]
struct UpdateWorkItemBody {
    team: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

/// Edit a work item's state / tags - writes through to the provider (Azure
/// DevOps), then returns the updated item.
async fn update_work_item(
    svc: Scoped,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(body): Json<UpdateWorkItemBody>,
) -> ApiResult {
    let update = poseiden_core::WorkItemUpdate {
        state: body.state,
        tags: body.tags,
    };
    match svc.update_work_item(&body.team, id, update).await {
        Ok((item, flags)) => Ok(Json(serde_json::json!({ "item": item, "flags": flags }))),
        Err(e) => Err(err500(e)),
    }
}

/// Body for a PR-link edit: the owning team, the PR, and whether to add or remove.
#[derive(serde::Deserialize)]
struct PrLinkBody {
    team: String,
    pr_id: i64,
    link: bool,
}

/// Add or remove a work-item <-> PR link, writing the artifact-link relation
/// through to the provider.
async fn link_work_item_pr(
    svc: Scoped,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(body): Json<PrLinkBody>,
) -> ApiResult {
    match svc
        .link_work_item_pr(&body.team, id, body.pr_id, body.link)
        .await
    {
        Ok((item, flags)) => Ok(Json(serde_json::json!({ "item": item, "flags": flags }))),
        Err(e) => Err(err500(e)),
    }
}

/// Replace the instance-wide default ruleset (`[rules]`).
async fn update_rules(svc: Scoped, Json(rules): Json<poseiden_core::RuleSet>) -> ApiResult {
    match svc.update_rules(rules).await {
        Ok(()) => Ok(Json(serde_json::json!({ "updated": true }))),
        Err(e) => Err(err500(e)),
    }
}

/// Set a team's `[team.rules]` override.
async fn update_team_rules(
    svc: Scoped,
    axum::extract::Path(name): axum::extract::Path<String>,
    Json(rules): Json<poseiden_core::RuleSet>,
) -> ApiResult {
    match svc.update_team_rules(&name, Some(rules)).await {
        Ok(true) => Ok(Json(serde_json::json!({ "updated": true }))),
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no team named '{name}'") })),
        )),
        Err(e) => Err(err500(e)),
    }
}

/// Clear a team's override so it inherits the instance default again.
async fn clear_team_rules(
    svc: Scoped,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> ApiResult {
    match svc.update_team_rules(&name, None).await {
        Ok(true) => Ok(Json(serde_json::json!({ "updated": true }))),
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no team named '{name}'") })),
        )),
        Err(e) => Err(err500(e)),
    }
}

async fn update_team(
    svc: Scoped,
    axum::extract::Path(name): axum::extract::Path<String>,
    Json(team): Json<poseiden_core::TeamConfig>,
) -> ApiResult {
    match svc.update_team(&name, team).await {
        Ok(true) => Ok(Json(serde_json::json!({ "updated": true }))),
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no team named '{name}'") })),
        )),
        Err(e) => Err(err500(e)),
    }
}

/// Remove a team by path `name`.
async fn remove_team(
    svc: Scoped,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> ApiResult {
    match svc.remove_team(&name).await {
        Ok(true) => Ok(Json(serde_json::json!({ "removed": true }))),
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no team named '{name}'") })),
        )),
        Err(e) => Err(err500(e)),
    }
}

/// Run one check's server-side fix. Interactive fixes (auth's device-code
/// sign-in) are handled by the frontend, not here - this is for auto/manual
/// server-side fixes. 404 if the id is unknown.
async fn doctor_fix(
    svc: Scoped,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult {
    match svc.doctor_fix(&id).await {
        Some(result) => Ok(Json(serde_json::to_value(result).unwrap())),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no check with id '{id}'") })),
        )),
    }
}

/// Configured team names - populates the UI scope selector.
async fn teams(svc: Scoped) -> ApiResult {
    Ok(Json(
        serde_json::to_value(svc.team_names().await.map_err(err500)?).unwrap(),
    ))
}

/// Optional `?team=<name>` scope. Empty / missing → all teams.
#[derive(Debug, Deserialize)]
struct ScopeQuery {
    team: Option<String>,
}

/// Treat an empty-string team the same as absent (the "All teams" default).
fn scope(team: &Option<String>) -> Option<&str> {
    team.as_deref().filter(|s| !s.is_empty())
}

async fn dashboard(svc: Scoped, Query(q): Query<ScopeQuery>) -> ApiResult {
    let summary = svc.dashboard(scope(&q.team)).await.map_err(err500)?;
    Ok(Json(serde_json::to_value(summary).unwrap()))
}

/// Tickets view: the work items plus the hygiene flags against them, in one
/// round-trip (the UI joins them client-side by `work_item_id`).
async fn tickets(svc: Scoped, Query(q): Query<ScopeQuery>) -> ApiResult {
    let team = scope(&q.team);
    let items = svc.work_items(team).await.map_err(err500)?;
    let flags = svc.flags(team).await.map_err(err500)?;
    Ok(Json(serde_json::json!({ "items": items, "flags": flags })))
}

async fn pipelines(svc: Scoped, Query(q): Query<ScopeQuery>) -> ApiResult {
    let health = svc.pipeline_health(scope(&q.team)).await.map_err(err500)?;
    Ok(Json(serde_json::to_value(health).unwrap()))
}

async fn pull_requests(svc: Scoped, Query(q): Query<ScopeQuery>) -> ApiResult {
    let prs = svc.pull_requests(scope(&q.team)).await.map_err(err500)?;
    Ok(Json(serde_json::to_value(prs).unwrap()))
}

/// Resolve a single PR's web URL by id (a linked-PR chip may point at a PR
/// outside the polled window). `team` names which project's credential to use.
async fn pull_request_url(
    svc: Scoped,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Query(q): Query<ScopeQuery>,
) -> ApiResult {
    let team = q.team.as_deref().unwrap_or_default();
    match svc.resolve_pull_request(team, id).await {
        Ok(pr) => Ok(Json(
            serde_json::json!({ "url": pr.url, "status": pr.status, "is_draft": pr.is_draft }),
        )),
        Err(e) => Err(err500(e)),
    }
}

/// Optional `from` / `to` (`YYYY-MM-DD` or RFC3339; default last 30 days) plus
/// an optional `team` scope.
#[derive(Debug, Deserialize)]
struct RangeQuery {
    from: Option<String>,
    to: Option<String>,
    team: Option<String>,
}

async fn reports(svc: Scoped, Query(q): Query<RangeQuery>) -> ApiResult {
    let (from, to) = crate::normalise_range(q.from.as_deref(), q.to.as_deref());
    let team = scope(&q.team);
    let tickets = svc.ticket_report(&from, &to, team).await.map_err(err500)?;
    let pipelines = svc
        .pipeline_report(&from, &to, team)
        .await
        .map_err(err500)?;
    Ok(Json(serde_json::json!({
        "range": { "from": from, "to": to },
        "tickets": tickets,
        "pipelines": pipelines,
    })))
}

// ── Configurable reports ───────────────────────────────────────────────

/// Built-in + saved report specs.
async fn report_specs(svc: Scoped) -> ApiResult {
    let specs = svc.report_specs().await.map_err(err500)?;
    Ok(Json(serde_json::to_value(specs).unwrap()))
}

/// Run a named report, optionally overriding its team scope with `?team=`.
async fn run_report_named(
    svc: Scoped,
    axum::extract::Path(name): axum::extract::Path<String>,
    Query(q): Query<ScopeQuery>,
) -> ApiResult {
    match svc.run_report_named(&name, scope(&q.team)).await {
        Ok(result) => Ok(Json(serde_json::to_value(result).unwrap())),
        Err(e) => Err(err500(e)),
    }
}

/// Run an unsaved spec (builder preview). Body is a `ReportSpec`; `?team=`
/// overrides its scope.
async fn run_report_spec(
    svc: Scoped,
    Query(q): Query<ScopeQuery>,
    Json(spec): Json<poseiden_core::ReportSpec>,
) -> ApiResult {
    match svc.run_report_spec(spec, scope(&q.team)).await {
        Ok(result) => Ok(Json(serde_json::to_value(result).unwrap())),
        Err(e) => Err(err500(e)),
    }
}

/// Save (create/replace) a user report. Built-in names are rejected.
async fn save_report(svc: Scoped, Json(spec): Json<poseiden_core::ReportSpec>) -> ApiResult {
    match svc.save_report(spec).await {
        Ok(()) => Ok(Json(serde_json::json!({ "saved": true }))),
        Err(e) => Err(err500(e)),
    }
}

/// Delete a saved report.
async fn delete_report(
    svc: Scoped,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> ApiResult {
    match svc.delete_report(&name).await {
        Ok(removed) => Ok(Json(serde_json::json!({ "deleted": removed }))),
        Err(e) => Err(err500(e)),
    }
}

/// Record the UI's selected team (query `?team=`; empty clears it).
async fn set_active_team(svc: Scoped, Query(q): Query<ScopeQuery>) -> ApiResult {
    match svc.set_active_team(scope(&q.team)).await {
        Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
        Err(e) => Err(err500(e)),
    }
}

#[derive(Debug, Deserialize)]
struct PollAllBody {
    all: bool,
}

/// Toggle the poll-all-teams setting.
async fn set_poll_all_teams(svc: Scoped, Json(body): Json<PollAllBody>) -> ApiResult {
    match svc.set_poll_all_teams(body.all).await {
        Ok(()) => Ok(Json(serde_json::json!({ "poll_all_teams": body.all }))),
        Err(e) => Err(err500(e)),
    }
}

/// Body for a forwarded frontend error.
#[derive(Debug, Deserialize)]
struct ClientErrorBody {
    message: String,
    #[serde(default)]
    stack: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

/// Log a frontend error into the backend telemetry stream.
async fn log_client_error(svc: Scoped, Json(body): Json<ClientErrorBody>) -> ApiResult {
    svc.log_client_error(&body.message, body.stack.as_deref(), body.url.as_deref());
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn config(svc: Scoped) -> ApiResult {
    // The config carries no secrets (PATs live in env), so this is safe to
    // return verbatim for the Settings screen.
    Ok(Json(svc.config().await.map_err(err500)?))
}

/// Export this owner's config as a YAML document (download). Content-typed as
/// YAML so a browser saves it sensibly; carries no secrets.
async fn export_config(svc: Scoped) -> impl IntoResponse {
    match svc.export_config().await {
        Ok(yaml) => (
            [
                (axum::http::header::CONTENT_TYPE, "application/x-yaml"),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    "attachment; filename=\"poseiden-config.yaml\"",
                ),
            ],
            yaml,
        )
            .into_response(),
        Err(e) => err500(e).into_response(),
    }
}

/// Query for [`import_config`]: `?replace=true` overwrites the owner's config;
/// otherwise merge (add teams/reports not already present).
#[derive(Deserialize)]
struct ImportQuery {
    #[serde(default)]
    replace: bool,
}

/// Import a YAML config document (raw request body) into this owner.
async fn import_config(svc: Scoped, Query(q): Query<ImportQuery>, body: String) -> ApiResult {
    let summary = svc.import_config(&body, q.replace).await.map_err(err500)?;
    Ok(Json(serde_json::to_value(summary).unwrap()))
}

/// Manual poll trigger - backs the UI's "refresh now" button and returns the
/// poll outcome so the caller can surface counts + any per-project errors.
async fn poll(svc: Scoped) -> ApiResult {
    let outcome = svc.poll_once().await;
    Ok(Json(serde_json::to_value(outcome).unwrap()))
}

/// Whether an AI tag suggester is configured (so the UI shows the action or not).
async fn ai_status(svc: Scoped) -> ApiResult {
    Ok(Json(
        serde_json::json!({ "enabled": svc.ai_enabled().await }),
    ))
}

/// The LLM integration registry for the settings UI: integrations (keys redacted,
/// annotated with compatible/active), platform caps, and the preset choices.
async fn llm_config_get(svc: Scoped) -> ApiResult {
    Ok(Json(svc.llm_config_view().await))
}

/// Persist the LLM registry (from the LLM Integrations table) and reload the tagger.
async fn llm_config_set(svc: Scoped, Json(cfg): Json<poseiden_ai::LlmConfig>) -> ApiResult {
    svc.set_llm_config(cfg).await.map_err(err500)?;
    Ok(Json(
        serde_json::json!({ "enabled": svc.ai_enabled().await }),
    ))
}

/// Drop the saved registry so it reverts to the seeded default catalog.
async fn llm_config_reset(svc: Scoped) -> ApiResult {
    svc.reset_llm_config().await.map_err(err500)?;
    Ok(Json(svc.llm_config_view().await))
}

/// Time a fixed test query against every server-runnable configured integration.
async fn llm_benchmark(svc: Scoped) -> ApiResult {
    Ok(Json(svc.benchmark_llms().await))
}

/// Optional body for a tag-suggestion run: the specific work-item ids to process
/// (the UI sends the selected rows). Absent/empty body = the whole team scope.
#[derive(serde::Deserialize)]
struct RunBody {
    #[serde(default)]
    ids: Option<Vec<i64>>,
}

/// Start an AI tag-suggestion run in the BACKGROUND (model inference is slow, so
/// running it inline 504s over a large backlog) and return the initial status.
/// Scoped to the posted `ids` when present. The browser polls
/// `GET /api/tag-suggestions/status`.
async fn run_tag_suggestions(
    svc: Scoped,
    Query(q): Query<ScopeQuery>,
    body: Option<Json<RunBody>>,
) -> ApiResult {
    let ids = body.and_then(|Json(b)| b.ids).filter(|v| !v.is_empty());
    svc.start_tag_suggestions(scope(&q.team).map(str::to_string), ids)
        .await
        .map_err(err500)?;
    Ok(Json(
        serde_json::to_value(svc.tag_suggestions_status()).unwrap(),
    ))
}

/// Current AI tag-suggestion run state for this owner (idle / running / done /
/// failed), for the browser to poll after starting a run.
async fn tag_suggestions_status(svc: Scoped) -> ApiResult {
    Ok(Json(
        serde_json::to_value(svc.tag_suggestions_status()).unwrap(),
    ))
}

/// Tag-input settings: whether work-item descriptions feed the tagger (AI + keyword).
async fn tag_settings_get(svc: Scoped) -> ApiResult {
    Ok(Json(
        serde_json::json!({ "use_description": svc.tag_use_description().await }),
    ))
}

#[derive(serde::Deserialize)]
struct TagSettingsBody {
    #[serde(default)]
    use_description: bool,
}

async fn tag_settings_set(svc: Scoped, Json(b): Json<TagSettingsBody>) -> ApiResult {
    svc.set_tag_use_description(b.use_description)
        .await
        .map_err(err500)?;
    Ok(Json(
        serde_json::json!({ "use_description": b.use_description }),
    ))
}

/// Descriptions (HTML-stripped) for the given work-item ids, for the client-side
/// WebGPU tagger (the tickets list omits bodies to stay lean).
#[derive(serde::Deserialize)]
struct DescBody {
    #[serde(default)]
    ids: Vec<i64>,
}

async fn work_item_descriptions(
    svc: Scoped,
    Query(q): Query<ScopeQuery>,
    Json(b): Json<DescBody>,
) -> ApiResult {
    let map = svc
        .work_item_descriptions(scope(&q.team), &b.ids)
        .await
        .map_err(err500)?;
    Ok(Json(serde_json::to_value(map).unwrap_or_default()))
}

/// Store browser-computed (WebGPU) suggestions. The server re-validates every tag
/// against the item's team canonical set before storing (trust boundary).
#[derive(serde::Deserialize)]
struct StoreBody {
    #[serde(default)]
    items: Vec<crate::service::BrowserSuggestion>,
}

async fn store_tag_suggestions(
    svc: Scoped,
    Query(q): Query<ScopeQuery>,
    Json(body): Json<StoreBody>,
) -> ApiResult {
    let summary = svc
        .store_tag_suggestions(scope(&q.team), body.items)
        .await
        .map_err(err500)?;
    Ok(Json(serde_json::to_value(summary).unwrap()))
}
