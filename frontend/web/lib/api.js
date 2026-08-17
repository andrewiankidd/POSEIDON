// POSEIDEN API client - the three-way data-source dispatch.
//
// This is the frontend counterpart to the shared `Service` in Rust: one set of
// calls (`api.dashboard()`, `api.tickets()`, …) that resolve to whichever
// backend is active, decided at call time from a single setting:
//
//   instance URL configured   → fetch() the remote hosted instance   (repointed)
//   else Tauri webview         → invoke() the embedded Service        (desktop/mobile standalone)
//   else                       → fetch() same-origin                  (the Docker/web instance)
//
// "Repointing" the Android app at your web instance is therefore just setting
// this one localStorage value - no separate code path, no rebuild.

const INSTANCE_KEY = 'poseiden-instance-url';
const TEAM_KEY = 'poseiden-team-scope';
const PAGE_SIZE_KEY = 'poseiden-page-size';
const DEFAULT_PAGE_SIZE = 500;

// --- Demo mode ------------------------------------------------------------
// A backend-free mode for static hosting (GitHub Pages): the landing page opens
// the app at `app.html?demo=1`. In demo mode every GET resolves from a shipped
// JSON fixture under `assets/demo/`, and every write is a benign no-op - so the
// whole app renders real sample data with no server, no Tauri, no network.
const DEMO_KEY = 'poseiden-demo';
let _demoCache = null;

/** True when the app is running against static demo fixtures instead of a
 *  backend. Detected from `?demo=1` (persisted for the session so it survives
 *  in-app navigation that drops the query string) and memoised. */
export function isDemo() {
  if (_demoCache !== null) return _demoCache;
  let on = false;
  try {
    const q = typeof location !== 'undefined' && /(?:^|[?&])demo=1(?:&|$)/.test(location.search || '');
    let stored = false;
    try { stored = sessionStorage.getItem(DEMO_KEY) === '1'; } catch { /* storage blocked */ }
    if (q) { try { sessionStorage.setItem(DEMO_KEY, '1'); } catch { /* storage blocked */ } }
    on = Boolean(q || stored);
  } catch { on = false; }
  _demoCache = on;
  return on;
}

// Map an API path to its fixture basename: strip a leading `/api`, drop the
// query string, trim slashes, and turn the rest into a `-`-joined name. So
// `/tickets` -> `tickets`, `/reports/run/work-items-by-tag` ->
// `reports-run-work-items-by-tag`, `/pull-requests/2001/url` ->
// `pull-requests-2001-url`.
function demoFixtureName(path) {
  let p = String(path || '');
  p = p.replace(/^\/api(?=\/|$)/, '');
  p = p.split('?')[0];
  p = p.replace(/^\/+/, '').replace(/\/+$/, '');
  p = p.replace(/\//g, '-');
  return p || 'index';
}

// Resolve a GET from a static fixture. A missing/broken fixture resolves to a
// benign empty object so the screen shows an empty state instead of throwing.
async function demoGet(path) {
  const name = demoFixtureName(path);
  try {
    const url = new URL('../assets/demo/' + name + '.json', import.meta.url);
    const res = await fetch(url);
    if (!res.ok) return {};
    return await res.json();
  } catch { return {}; }
}

/** The selected team scope, or '' for "all teams". Persisted so the scope
 *  survives reloads and is shared across every view. */
export function getTeamScope() {
  return localStorage.getItem(TEAM_KEY) || '';
}

/** Set (or clear, with '') the team scope. */
export function setTeamScope(team) {
  if (team) localStorage.setItem(TEAM_KEY, team);
  else localStorage.removeItem(TEAM_KEY);
}

/** The configured remote instance URL, trailing slash stripped, or '' if none. */
export function getInstanceUrl() {
  // A user's own choice (localStorage) wins; otherwise fall back to the deployment
  // default the server injects via env.js (window.__POSEIDEN_ENV__.instanceUrl).
  // That's how a "client" deployment boots pre-pointed at its server instance.
  const stored = localStorage.getItem(INSTANCE_KEY);
  const url = stored != null ? stored : (window.__POSEIDEN_ENV__?.instanceUrl || '');
  return url.replace(/\/+$/, '');
}

/** Set (or clear, with a falsy value) the remote instance URL. */
export function setInstanceUrl(url) {
  const clean = (url || '').trim().replace(/\/+$/, '');
  if (clean) localStorage.setItem(INSTANCE_KEY, clean);
  else localStorage.removeItem(INSTANCE_KEY);
}

/** Rows shown per page in the list tables. 0 = show all (no pagination).
 *  A client-side display preference; defaults to 500. */
export function getPageSize() {
  const raw = localStorage.getItem(PAGE_SIZE_KEY);
  if (raw == null) return DEFAULT_PAGE_SIZE;
  const n = parseInt(raw, 10);
  return Number.isFinite(n) && n >= 0 ? n : DEFAULT_PAGE_SIZE;
}

/** Set the rows-per-page preference (0 = all). Out-of-range clamps sensibly. */
export function setPageSize(n) {
  const v = Math.max(0, Math.min(100000, parseInt(n, 10) || 0));
  localStorage.setItem(PAGE_SIZE_KEY, String(v));
}

const DEV_OWNER_KEY = 'poseiden-dev-owner';

/** Dev-mode owner override. When set, same-origin API calls carry it as
 *  `X-Auth-Request-Email`, so you can view any tenant in an auth-off playground
 *  without a real login. A real auth proxy overwrites that header at the ingress,
 *  so this is a dev-only convenience, never a production bypass - and it's only
 *  sent same-origin (never to a configured remote instance). */
export function getDevOwner() {
  return (localStorage.getItem(DEV_OWNER_KEY) || '').trim();
}
export function setDevOwner(email) {
  const v = (email || '').trim();
  if (v) localStorage.setItem(DEV_OWNER_KEY, v);
  else localStorage.removeItem(DEV_OWNER_KEY);
}
function devHeaders() {
  if (getInstanceUrl()) return {}; // same-origin playground only
  const owner = getDevOwner();
  return owner ? { 'X-Auth-Request-Email': owner } : {};
}

function isTauri() {
  return typeof window !== 'undefined' && window.__TAURI__ != null;
}

/** Which backend the next call will hit - surfaced in the sidebar badge. */
export function mode() {
  if (getInstanceUrl()) return 'remote';
  if (isTauri()) return 'desktop';
  return 'web';
}

function isMobileShell() {
  return typeof navigator !== 'undefined' && /android|iphone|ipad|ipod/i.test(navigator.userAgent);
}

/** Probe what the current shell can actually do, so onboarding offers only real
 *  options instead of guessing from "is this desktop?". Resolved once at boot.
 *
 *    localService  - can host a durable local Service in-process (standalone).
 *                    Only the native shell can; a browser tab has no Rust/SQLite
 *                    runtime to persist into at all, so it is always a client.
 *    sameOriginApi - a poseiden server is already serving this bundle (we are ON
 *                    an instance). Distinguishes a served web instance (Docker/
 *                    Helm) from a static host (GitHub Pages) with no backend.
 *    portableMode  - the standalone data-location choice is meaningful here
 *                    (desktop filesystem). Mobile sandboxes its own storage.
 */
export async function capabilities() {
  // Demo mode presents as "already on an instance" so boot skips onboarding and
  // renders straight from fixtures (and never pings a backend that isn't there).
  if (isDemo()) return { localService: false, sameOriginApi: true, portableMode: false };
  const localService = isTauri();
  const sameOriginApi = localService ? false : await pingSameOriginApi();
  return { localService, sameOriginApi, portableMode: localService && !isMobileShell() };
}

/** Is a poseiden backend answering at our own origin? (Ignores any configured
 *  remote - this asks "did a server serve us", not "can we reach a remote".) */
async function pingSameOriginApi() {
  try {
    const res = await fetch('/api/health', { method: 'GET' });
    return res.ok;
  } catch { return false; }
}

/** The caller's proxy identity - { owner, authenticated } - for the signed-in
 *  menu. `authenticated` reflects whether the ingress injected an identity header.
 *  HTTP-only: a desktop/local shell has no proxy, so it's always unauthenticated
 *  and we skip the call. Never throws - the menu just stays hidden on failure. */
export async function identity() {
  // Demo mode has no proxy; report an unauthenticated default so the signed-in
  // menu stays hidden and no /api/identity request is attempted.
  if (isDemo()) return { owner: 'demo', authenticated: false };
  if (mode() === 'desktop') {
    const dev = getDevOwner();
    return dev ? { owner: dev, authenticated: true } : { owner: 'default', authenticated: false };
  }
  try {
    const res = await fetch((getInstanceUrl() || '') + '/api/identity', { headers: devHeaders() });
    if (!res.ok) return { owner: 'default', authenticated: false };
    return await res.json();
  } catch { return { owner: 'default', authenticated: false }; }
}

function tauriInvoke(cmd, args) {
  const t = window.__TAURI__;
  const invoke = t.core?.invoke ?? t.invoke;
  return invoke(cmd, args || {});
}

// One request definition per logical call: the HTTP shape AND the invoke shape,
// so `request` can satisfy either transport from the same spec.
async function request(path, { method = 'GET', query, body, invokeCmd, invokeArgs } = {}) {
  // Demo mode short-circuits before any transport: GETs come from static
  // fixtures, writes resolve to a benign value (never a network call, never a
  // throw) so the UI stays interactive without a backend.
  if (isDemo()) {
    if (method === 'GET') return demoGet(path);
    return {};
  }

  const instance = getInstanceUrl();

  // Standalone Tauri (no remote configured) → invoke the embedded Service.
  if (!instance && isTauri()) {
    return tauriInvoke(invokeCmd, invokeArgs);
  }

  // Otherwise HTTP: remote instance if set, else same-origin.
  const base = instance || '';
  let url = base + '/api' + path;
  if (query) {
    const params = new URLSearchParams(
      Object.entries(query).filter(([, v]) => v != null && v !== '')
    );
    const qs = params.toString();
    if (qs) url += '?' + qs;
  }

  const opts = { method };
  const headers = { ...devHeaders() };
  if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  if (Object.keys(headers).length) opts.headers = headers;
  const res = await fetch(url, opts);
  if (!res.ok) {
    let message = `HTTP ${res.status}`;
    try {
      const body = await res.json();
      if (body && body.error) message = body.error;
    } catch { /* non-JSON error body */ }
    throw new Error(message);
  }
  return res.json();
}

// The current team scope rides on every scoped call - read fresh each time so
// changing the selector takes effect immediately, and pass it as both a query
// param (HTTP) and an invoke arg (Tauri). '' means "all teams" → sent as null.
function team() {
  return getTeamScope() || null;
}

export const api = {
  /** Liveness + build stamp - { status, service, version }. The version lets a
   *  long-open tab detect a redeploy and offer a reload. */
  health: () => request('/health', { invokeCmd: 'get_health' }),
  /** Auth state - { signed_in, method, message } - for the sign-in banner. */
  auth: () => request('/auth', { invokeCmd: 'get_auth' }),
  /** Whether an AI tag suggester is configured ({ enabled }). */
  aiStatus: () => request('/ai/status', { invokeCmd: 'get_ai_status' }),
  /** The LLM integration registry: { integrations (keys redacted, each with
   *  compatible/active), caps, active_id, presets }. */
  llmConfig: () => request('/llm-config', { invokeCmd: 'get_llm_config' }),
  /** Persist the whole registry + reload the tagger. `config` = { integrations }.
   *  A blank api_key on an integration keeps its previously stored key (by id). */
  setLlmConfig: (config) =>
    request('/llm-config', { method: 'POST', body: config, invokeCmd: 'set_llm_config', invokeArgs: { config } }),
  /** Discard the saved registry so it reverts to the seeded default catalog; returns
   *  the refreshed view. */
  resetLlmConfig: () =>
    request('/llm-config/reset', { method: 'POST', body: {}, invokeCmd: 'reset_llm_config' }),
  /** Auto-configure the registry for this platform, sizing each local model to
   *  capability. `caps` = the BROWSER-detected caps the server can't see. No-ops on a
   *  hand-edited registry. Returns the refreshed view. */
  autotuneLlm: (caps) =>
    request('/llm-config/autotune', { method: 'POST', body: caps, invokeCmd: 'autotune_llm_config', invokeArgs: { caps } }),
  /** Time one test query against every server-runnable configured integration.
   *  Returns { results: [{id,name,kind,status,ms,tags,error}], webgpu: [...] }.
   *  WebGPU entries are timed client-side (they run in the browser). */
  benchmarkLlms: () =>
    request('/llm-benchmark', { method: 'POST', body: {}, invokeCmd: 'benchmark_llms' }),
  /** Start a background AI tag-suggestion run over the scoped items; returns the
   *  initial status. Poll `tagSuggestionsStatus` until state is done/failed - the
   *  run can be slow (offline models especially). Results land as advisory chips in
   *  the Suggested column; nothing is auto-applied. */
  runTagSuggestions: (team, ids) =>
    request('/tag-suggestions/run', {
      method: 'POST',
      query: { team },
      body: { ids },
      invokeCmd: 'run_tag_suggestions',
      invokeArgs: { team, ids },
    }),
  /** Current run state: { state: 'idle'|'running'|'done'|'failed', ... }. */
  tagSuggestionsStatus: () =>
    request('/tag-suggestions/status', { invokeCmd: 'tag_suggestions_status' }),
  /** Store browser (WebGPU) computed suggestions - the server re-validates them.
   *  `items` = [{ id, tags: [{ tag, reason }] }]. Returns a run summary. */
  storeTagSuggestions: (team, items) =>
    request('/tag-suggestions/store', {
      method: 'POST', query: { team }, body: { items },
      invokeCmd: 'store_tag_suggestions', invokeArgs: { team, items },
    }),
  /** Start a background AI healthcheck audit over the scoped items (server-side
   *  model); returns the initial status. Poll `healthcheckAuditStatus`. Findings
   *  land as advisory `ai_audit` flags. */
  runHealthcheckAudit: (team, ids) =>
    request('/healthcheck/audit/run', {
      method: 'POST', query: { team }, body: { ids },
      invokeCmd: 'run_healthcheck_audit', invokeArgs: { team, ids },
    }),
  /** Current audit run state: { state: 'idle'|'running'|'done'|'failed', ... }. */
  healthcheckAuditStatus: () =>
    request('/healthcheck/audit/status', { invokeCmd: 'healthcheck_audit_status' }),
  /** Per-item audit prompts for the browser (WebGPU) path: [{ id, system, user }]. */
  healthcheckAuditPrompts: (team, ids) =>
    request('/healthcheck/audit/prompts', {
      method: 'POST', query: { team }, body: { ids },
      invokeCmd: 'healthcheck_audit_prompts', invokeArgs: { team, ids },
    }),
  /** Store browser (WebGPU) computed audit replies - the server re-parses them.
   *  `results` = [{ id, text }]. Returns a run summary. */
  storeHealthcheckAudit: (team, results) =>
    request('/healthcheck/audit/store', {
      method: 'POST', query: { team }, body: { results },
      invokeCmd: 'store_healthcheck_audit', invokeArgs: { team, results },
    }),
  /** Tag-input settings: { use_description }. */
  tagSettings: () => request('/tag-settings', { invokeCmd: 'get_tag_settings' }),
  setTagSettings: (useDescription) =>
    request('/tag-settings', {
      method: 'POST', body: { use_description: useDescription },
      invokeCmd: 'set_tag_settings', invokeArgs: { useDescription },
    }),
  /** Descriptions for the given ids ({ id: text }); for the WebGPU tagger, which the
   *  server otherwise omits from the tickets payload. Empty if the owner opted out. */
  workItemDescriptions: (team, ids) =>
    request('/work-items/descriptions', {
      method: 'POST', query: { team }, body: { ids },
      invokeCmd: 'work_item_descriptions', invokeArgs: { team, ids },
    }),
  /** Add a team at runtime. `team` = { name, organization, project, area_path?, tenant? }. */
  addTeam: (team) => request('/teams', { method: 'POST', body: team, invokeCmd: 'add_team', invokeArgs: { team } }),
  /** Update a team (matched by its original name). */
  updateTeam: (original, team) =>
    request('/teams/' + encodeURIComponent(original), {
      method: 'PUT', body: team, invokeCmd: 'update_team', invokeArgs: { original, team },
    }),
  /** Remove a team by name. */
  removeTeam: (name) =>
    request('/teams/' + encodeURIComponent(name), {
      method: 'DELETE', invokeCmd: 'remove_team', invokeArgs: { name },
    }),
  /** Doctor health report - { health, checks, checked_at }. */
  doctor: () => request('/doctor', { invokeCmd: 'get_doctor' }),
  /** Run the Doctor with auto-fixes and return the fresh report (Re-check). */
  doctorRecheck: () => request('/doctor', { method: 'POST', invokeCmd: 'doctor_recheck' }),
  /** Run one check's server-side fix. Returns { ok, message }. */
  doctorFix: (id) =>
    request('/doctor/fix/' + encodeURIComponent(id), {
      method: 'POST',
      invokeCmd: 'doctor_fix',
      invokeArgs: { id },
    }),
  /** Edit a work item's state / tags - writes through to Azure DevOps and
   *  returns the updated item. `changes` = { team, state?, tags? }. */
  updateWorkItem: (id, changes) =>
    request('/work-items/' + encodeURIComponent(id), {
      method: 'PUT',
      body: changes,
      invokeCmd: 'update_work_item',
      invokeArgs: { team: changes.team, id, state: changes.state ?? null, tags: changes.tags ?? null },
    }),
  /** Resolve a single PR's web URL by id (for a linked chip outside the polled
   *  window). Returns `{ url }`. */
  prUrl: (id, team) =>
    request('/pull-requests/' + encodeURIComponent(id) + '/url', {
      query: { team },
      invokeCmd: 'pull_request_url',
      invokeArgs: { team, prId: id },
    }),
  /** Add or remove a work-item <-> PR link - writes the ADO artifact-link
   *  relation and returns the updated item. `link` true = add, false = remove. */
  linkWorkItemPr: (id, { team, prId, link }) =>
    request('/work-items/' + encodeURIComponent(id) + '/pr-link', {
      method: 'POST',
      body: { team, pr_id: prId, link },
      invokeCmd: 'link_work_item_pr',
      invokeArgs: { team, workItemId: id, prId, link },
    }),
  /** The provider-discovered editable fields for a work item (the editor modal).
   *  Returns { fields: [{reference,label,kind,value,options,read_only,required,help}] }.
   *  Type-specific on Azure DevOps; title + body on GitHub/GitLab. */
  workItemFields: (id, team) =>
    request('/work-items/' + encodeURIComponent(id) + '/fields', { query: { team } }),
  /** Save edited fields - writes each changed field through to the provider and
   *  returns { item, flags }. `changes` = [{ reference, value }] (markdown for rich
   *  fields). */
  updateWorkItemFields: (id, { team, changes }) =>
    request('/work-items/' + encodeURIComponent(id) + '/fields', {
      method: 'PUT',
      body: { team, changes },
    }),
  /** AI-draft (or improve) one field from the item's context. Returns { value }
   *  (markdown) for the editor to drop in - never auto-saved. */
  draftWorkItemField: (id, { team, reference, improve, fields }) =>
    request('/work-items/' + encodeURIComponent(id) + '/fields/draft', {
      method: 'POST',
      body: { team, reference, improve: !!improve, fields: fields || [] },
    }),
  /** Replace the instance-wide default ruleset. `rules` = a full RuleSet. */
  updateRules: (rules) =>
    request('/rules', { method: 'PUT', body: rules, invokeCmd: 'update_rules', invokeArgs: { rules } }),
  /** Set a team's rules override. `rules` = a full RuleSet. */
  updateTeamRules: (name, rules) =>
    request('/teams/' + encodeURIComponent(name) + '/rules', {
      method: 'PUT', body: rules, invokeCmd: 'update_team_rules', invokeArgs: { team: name, rules },
    }),
  /** Clear a team's override so it inherits the instance default again. */
  clearTeamRules: (name) =>
    request('/teams/' + encodeURIComponent(name) + '/rules', {
      method: 'DELETE', invokeCmd: 'update_team_rules', invokeArgs: { team: name, rules: null },
    }),
  /** Configured team names for the scope selector. */
  teams: () => request('/teams', { invokeCmd: 'get_teams' }),
  dashboard: () =>
    request('/dashboard', {
      query: { team: team() },
      invokeCmd: 'get_dashboard',
      invokeArgs: { team: team() },
    }),
  tickets: () =>
    request('/tickets', {
      query: { team: team() },
      invokeCmd: 'get_tickets',
      invokeArgs: { team: team() },
    }),
  pipelines: () =>
    request('/pipelines', {
      query: { team: team() },
      invokeCmd: 'get_pipelines',
      invokeArgs: { team: team() },
    }),
  pullRequests: () =>
    request('/pull-requests', {
      query: { team: team() },
      invokeCmd: 'get_pull_requests',
      invokeArgs: { team: team() },
    }),
  reports: (from, to) =>
    request('/reports', {
      query: { from, to, team: team() },
      invokeCmd: 'get_reports',
      invokeArgs: { from: from || null, to: to || null, team: team() },
    }),
  /** Built-in + saved report specs. */
  reportSpecs: () => request('/reports/specs', { invokeCmd: 'get_report_specs' }),
  /** Run a saved/built-in report by name (scoped to the current team). */
  runReportNamed: (name) =>
    request('/reports/run/' + encodeURIComponent(name), {
      query: { team: team() },
      invokeCmd: 'run_report_named',
      invokeArgs: { name, team: team() },
    }),
  /** Run an unsaved spec (builder preview), scoped to the current team. */
  runReportSpec: (spec) =>
    request('/reports/run', {
      method: 'POST', query: { team: team() }, body: spec,
      invokeCmd: 'run_report_spec', invokeArgs: { spec, team: team() },
    }),
  /** Save (create/replace) a user report. Built-in names are rejected server-side. */
  saveReport: (spec) =>
    request('/reports/specs', {
      method: 'POST', body: spec, invokeCmd: 'save_report', invokeArgs: { spec },
    }),
  /** Delete a saved report by name. */
  deleteReport: (name) =>
    request('/reports/specs/' + encodeURIComponent(name), {
      method: 'DELETE', invokeCmd: 'delete_report', invokeArgs: { name },
    }),
  /** Forward an uncaught frontend error into the backend log stream. */
  logClientError: (payload) =>
    request('/client-error', { method: 'POST', body: payload, invokeCmd: 'log_client_error', invokeArgs: payload }),
  config: () => request('/config', { invokeCmd: 'get_config' }),
  /** Tell the backend which team is selected, so the active-team poll fetches it. */
  setActiveTeam: (name) =>
    request('/active-team', {
      method: 'POST', query: { team: name || '' },
      invokeCmd: 'set_active_team', invokeArgs: { team: name || null },
    }),
  /** Toggle whether polls fetch all teams (true) or just the active one (false). */
  setPollAllTeams: (all) =>
    request('/settings/poll-all-teams', {
      method: 'POST', body: { all }, invokeCmd: 'set_poll_all_teams', invokeArgs: { all },
    }),
  pollNow: () => request('/poll', { method: 'POST', invokeCmd: 'poll_now' }),
};

/** Trigger an interactive device-code sign-in on whichever backend is active,
 *  resolving with the refreshed auth status once it completes.
 *
 *  `onCode({ url, code })` is invoked with the device-code prompt to display.
 *
 *  - **Desktop** (embedded Tauri): invokes the `sign_in` command; the prompt
 *    arrives via the `auth-device-code` event, bridged to `onCode`.
 *  - **Web / remote** (hosted instance): `POST /api/sign-in` kicks off
 *    `az login` in the owner's isolated token cache and returns the prompt;
 *    we surface it via `onCode`, then poll `GET /api/sign-in` until the flow
 *    completes and return the refreshed auth status. */
export async function signIn(onCode) {
  // Demo mode has nothing to sign into; resolve as already-signed-in without any
  // device-code flow or network so the sign-in action never errors.
  if (isDemo()) return { signed_in: true, method: 'demo', message: 'Demo mode' };
  if (mode() === 'desktop') {
    let unlisten = () => {};
    const listen = window.__TAURI__?.event?.listen;
    if (listen && typeof onCode === 'function') {
      unlisten = await listen('auth-device-code', (e) => onCode(e.payload));
    }
    try {
      return await tauriInvoke('sign_in', {});
    } finally {
      if (typeof unlisten === 'function') unlisten();
    }
  }

  // Web / remote: HTTP device-code flow against the hosted instance.
  const prompt = await request('/sign-in', { method: 'POST' });
  if (prompt && prompt.url && typeof onCode === 'function') {
    onCode({ url: prompt.url, code: prompt.code });
  }
  // Poll the sign-in state until it resolves (bounded ~5 min, matching the
  // device-code lifetime). Errors surface the recorded failure detail.
  const deadline = Date.now() + 5 * 60 * 1000;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 2500));
    const st = await request('/sign-in', { method: 'GET' });
    if (st.state === 'done') return api.auth();
    if (st.state === 'failed') throw new Error(st.error || 'Sign-in did not complete');
  }
  throw new Error('Sign-in timed out - the device code expired before you finished.');
}

/** Whether the desktop shell's local runtime is set up. On first run the store +
 *  Service are deferred until the user finishes onboarding, so this is `false`
 *  until `initialize()` runs. Web / remote have no local runtime to build, so
 *  they're always "ready"; and if the check can't run (older shell) we report
 *  ready so onboarding never wedges anything. */
export async function isInitialized() {
  if (mode() !== 'desktop') return true;
  try { return await tauriInvoke('is_initialized', {}); }
  catch { return true; }
}

/** Complete first-run LOCAL setup: optionally enable portable mode (data confined
 *  beside the app), then build the local store + Service. Desktop-only - a
 *  repointed (remote) client never calls this. */
export async function initialize(portable) {
  return tauriInvoke('initialize', { portable: !!portable });
}

/** Export the current owner's config as a YAML string. Works on every backend:
 *  desktop invokes the command; web/remote GETs the export endpoint (which serves
 *  YAML text, not JSON, so it can't ride the generic `request()` helper). */
export async function exportConfig() {
  if (isDemo()) return '';
  if (!getInstanceUrl() && isTauri()) {
    const v = await tauriInvoke('export_config', {});
    return typeof v === 'string' ? v : (v && v.yaml) || '';
  }
  const res = await fetch((getInstanceUrl() || '') + '/api/config/export');
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.text();
}

/** Import a YAML config document. `replace` overwrites; otherwise merges. Returns
 *  the import summary `{ teams, reports, replaced }`. */
export async function importConfig(yaml, replace) {
  if (isDemo()) return { teams: 0, reports: 0, replaced: !!replace };
  if (!getInstanceUrl() && isTauri()) {
    return tauriInvoke('import_config', { yaml, replace: !!replace });
  }
  const url = (getInstanceUrl() || '') + '/api/config/import?replace=' + (replace ? 'true' : 'false');
  const res = await fetch(url, {
    method: 'POST', headers: { 'Content-Type': 'application/x-yaml' }, body: yaml,
  });
  if (!res.ok) {
    let message = `HTTP ${res.status}`;
    try { const b = await res.json(); if (b && b.error) message = b.error; } catch { /* non-JSON */ }
    throw new Error(message);
  }
  return res.json();
}

/** Invoke a window-level Tauri command (quit / toggle_devtools). Desktop-only;
 *  a no-op elsewhere (the menu hides those items off desktop anyway). */
export async function windowAction(cmd) {
  if (mode() !== 'desktop') return;
  return tauriInvoke(cmd, {});
}

/** Subscribe to the device-code prompt emitted during `signIn()`. The callback
 *  receives `{ url, code }`. Returns an unsubscribe function. No-op off desktop. */
export async function onDeviceCode(callback) {
  if (mode() !== 'desktop') return () => {};
  const listen = window.__TAURI__.event?.listen;
  if (!listen) return () => {};
  return listen('auth-device-code', (e) => callback(e.payload));
}

/** Open a URL in the OS browser (Tauri) or a new tab (web). Used for the
 *  work-item / pipeline "open" links. */
export async function openExternal(url) {
  if (isTauri()) {
    try {
      const shell = window.__TAURI__.shell ?? (await import(/* @vite-ignore */ '@tauri-apps/plugin-shell'));
      if (shell?.open) return shell.open(url);
    } catch { /* fall through to window.open */ }
  }
  window.open(url, '_blank', 'noopener');
}
