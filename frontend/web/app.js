// POSEIDEN frontend controller. Hash-routed, no framework. Each view fetches
// through the `api` dispatch (which resolves to invoke / remote / same-origin)
// and renders into #main.

import {
  api, mode, capabilities, identity, getDevOwner, setDevOwner, getInstanceUrl, setInstanceUrl, getTeamScope, setTeamScope,
  getPageSize, setPageSize, signIn, isInitialized, initialize, exportConfig, importConfig, windowAction, openExternal,
} from './lib/api.js';
import { el, clear, esc, ago, shortDate, toast } from './lib/dom.js';
import { barChart, gauge, pieChart, lineChart } from './lib/charts.js';
import { renderMarkdown } from './lib/markdown.js';
import { dataTable } from './lib/table.js';
import { webgpuAvailable, runWebGpuTagging, runWebGpuChat, prepareModel, isModelCached, detectBrowserCaps } from './lib/webgpu.js';
import { effectiveActiveId, activeBackend, resolveAiText } from './lib/ai.js';
import { renderDeck } from './lib/recap-slides.js';

const main = document.getElementById('main');

const ROUTES = {
  dashboard: renderDashboard,
  'work-items': renderWorkItems,
  'pull-requests': renderPulls,
  pipelines: renderPipelines,
  reports: renderReports,
  recap: renderRecap,
  doctor: renderDoctor,
  rules: renderRules,
};

// ── Routing ─────────────────────────────────────────────────────────
function currentRoute() {
  const r = (location.hash || '#dashboard').slice(1).split('?')[0];
  return ROUTES[r] ? r : 'dashboard';
}

// Query params on the current hash route (e.g. `#work-items?flagged=1`), used to
// carry intent between screens - the dashboard's flag headings pre-filter here.
function routeParams() {
  return new URLSearchParams((location.hash || '').split('?')[1] || '');
}

// Persist the list-view toggles (Rule Breaks, Hide empty body) across refreshes,
// keyed per view - mirrors the table's own filter/sort persistence. Best-effort.
function loadToggles(key) {
  try { return JSON.parse(localStorage.getItem(`poseiden.toggles.${key}`) || 'null') || {}; }
  catch { return {}; }
}
function saveToggles(state) {
  if (!state || !state.persistKey) return;
  try {
    const k = `poseiden.toggles.${state.persistKey}`;
    const view = state.view && state.view !== 'table' ? state.view : null;
    // Drop the entry at defaults so we don't leave empty state lying around.
    if (!state.flaggedOnly && !state.hideEmpty && !view) { localStorage.removeItem(k); return; }
    localStorage.setItem(k, JSON.stringify({
      flaggedOnly: !!state.flaggedOnly, hideEmpty: !!state.hideEmpty, view: state.view || 'table',
    }));
  } catch { /* private mode / quota - persistence is best-effort */ }
}
// Build the shared filter state for a list view, restoring the persisted toggles +
// view - unless a deep-link (?flagged=1 / ?flag=<code>) explicitly overrides them.
function listState(persistKey, flagCode) {
  const saved = loadToggles(persistKey);
  return {
    persistKey,
    flagCode,
    flaggedOnly: routeParams().get('flagged') === '1' || !!flagCode || !!saved.flaggedOnly,
    hideEmpty: !!saved.hideEmpty,
    view: saved.view || 'table',
  };
}

// A "Rule Breaks" toggle for a list screen: flips `state.flaggedOnly` and
// refreshes the table (fetched lazily via `getTable`, since the table is often
// built after this button). Reused across Work Items, PRs, and Pipelines. Also
// clears any single-flag filter (and its chip) so it behaves as a clean all/none.
function ruleBreaksToggle(state, getTable) {
  return el('button', {
    class: 'toggle-btn' + (state.flaggedOnly ? ' on' : ''), type: 'button',
    'aria-pressed': String(state.flaggedOnly), title: 'Show only rows that break a hygiene rule',
    onclick: (e) => {
      state.flaggedOnly = !state.flaggedOnly;
      state.flagCode = null;
      e.currentTarget.parentNode?.querySelector('.filter-chip')?.remove();
      e.currentTarget.classList.toggle('on', state.flaggedOnly);
      e.currentTarget.setAttribute('aria-pressed', String(state.flaggedOnly));
      saveToggles(state);
      getTable().refresh();
    },
  }, [
    el('span', { class: 'toggle-switch' }, el('span', { class: 'toggle-knob' })),
    el('span', {}, 'Rule Breaks'),
  ]);
}

// When a screen is opened filtered to one flag (e.g. from a Health-check row),
// show a clearable chip naming it. Clearing broadens to all rule-breaks.
function flagFilterChip(state, route) {
  if (!state.flagCode) return null;
  const label = FLAG_LABELS[state.flagCode] || state.flagCode;
  return el('span', { class: 'filter-chip' }, [
    el('span', {}, `Flag: ${label}`),
    el('button', {
      type: 'button', class: 'filter-chip-x', title: 'Clear flag filter',
      onclick: () => { location.hash = `#${route}?flagged=1`; },
    }, '×'),
  ]);
}

// Does an entity carry the flag we're filtering to (or any flag if just "flagged
// only")? `codes` is the entity's flag-code list. Shared predicate logic.
function passesFlagFilter(state, codes) {
  // "Hide empty body" - drop underspecified items so you can work the ones with real
  // content first. Skipped when you're explicitly filtering TO empty-body items.
  if (state.hideEmpty && state.flagCode !== 'underspecified' && codes.includes('underspecified')) {
    return false;
  }
  if (state.flagCode) return codes.includes(state.flagCode);
  if (state.flaggedOnly) return codes.length > 0;
  return true;
}

// A "Hide empty body" toggle for the Work Items toolbar - filters out underspecified
// items (the "to refine" pile), so you can prioritise the ones with a real description
// before tackling the empties.
function hideEmptyToggle(state, getTable) {
  return el('button', {
    class: 'toggle-btn' + (state.hideEmpty ? ' on' : ''), type: 'button',
    'aria-pressed': String(!!state.hideEmpty),
    title: 'Hide items with an empty/very thin description (the "to refine" pile)',
    onclick: (e) => {
      state.hideEmpty = !state.hideEmpty;
      e.currentTarget.classList.toggle('on', state.hideEmpty);
      e.currentTarget.setAttribute('aria-pressed', String(state.hideEmpty));
      saveToggles(state);
      getTable().refresh();
    },
  }, [
    el('span', { class: 'toggle-switch' }, el('span', { class: 'toggle-knob' })),
    el('span', {}, 'Hide empty body'),
  ]);
}

// Auth state ({ signed_in, method, message }) - refreshed at boot + after a
// poll. Drives the dashboard's "Sign in to load data" empty state; the Doctor
// (its "Action required" indicator + panel) is the home for the sign-in action
// and the full error detail.
let authState = null;

async function refreshAuth() {
  try { authState = await api.auth(); }
  catch { authState = null; } // no backend / not connected - views handle it
}

/// The device-code sign-in flow, triggered by the Doctor's sign-in fix (and the
/// "Sign in" fix. Subscribes for the code (shown in a modal + browser opened),
/// invokes sign-in, then refreshes auth + doctor + polls on success.
async function runSignIn() {
  try {
    // The prompt is delivered the same way on every backend: desktop bridges the
    // Tauri event, web/remote returns it from `POST /api/sign-in`.
    const result = await signIn(({ url, code }) => { showDeviceCodeModal(url, code); openExternal(url); });
    authState = result;
    toast(result.signed_in ? 'Signed in - polling…' : 'Sign-in did not complete', !result.signed_in);
    if (result.signed_in) { await api.pollNow(); await refreshAuth(); await refreshDoctor(); }
    return result;
  } catch (e) {
    toast(`Sign-in failed: ${e.message || e}`, true);
    throw e;
  } finally {
    hideDeviceCodeModal();
  }
}

function showDeviceCodeModal(url, code) {
  hideDeviceCodeModal();
  const overlay = el('div', { class: 'dc-overlay', id: 'dc-overlay' }, [
    el('div', { class: 'dc-modal' }, [
      el('h3', {}, 'Finish signing in'),
      el('div', {}, [
        'Open ',
        el('a', { class: 'link', href: url, onclick: (e) => { e.preventDefault(); openExternal(url); } }, url),
        ' and enter this code:',
      ]),
      el('div', { class: 'dc-big', title: 'Click to copy', style: 'cursor:pointer',
        onclick: () => copyDeviceCode(code) }, code),
      el('div', { class: 'row', style: 'justify-content:center' }, [
        el('button', { class: 'btn', onclick: () => copyDeviceCode(code) }, 'Copy code'),
        el('button', { class: 'btn', onclick: hideDeviceCodeModal }, 'Close'),
      ]),
      el('div', { class: 'muted', style: 'margin-top:8px' }, 'This closes automatically when sign-in completes.'),
    ]),
  ]);
  document.body.appendChild(overlay);
  // Auto-copy the code so the user only has to paste it at the Microsoft page.
  copyDeviceCode(code, true);
}

// Copy the device code, with a fallback for non-secure contexts where
// navigator.clipboard is unavailable. `silentOnFail` suppresses the error toast
// for the automatic copy-on-open (the Copy button + manual selection still work).
async function copyDeviceCode(code, silentOnFail) {
  let ok = false;
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(code);
      ok = true;
    }
  } catch { /* fall through to the legacy path */ }
  if (!ok) {
    try {
      const ta = el('textarea', { style: 'position:fixed;opacity:0;top:0' });
      ta.value = code;
      document.body.appendChild(ta);
      ta.select();
      ok = document.execCommand('copy');
      ta.remove();
    } catch { ok = false; }
  }
  if (ok) toast('Code copied to clipboard');
  else if (!silentOnFail) toast('Could not copy - select the code manually', true);
}

function hideDeviceCodeModal() {
  const o = document.getElementById('dc-overlay');
  if (o) o.remove();
}

// ── Doctor (system health) ──────────────────────────────────────────
let doctorReport = null;
let doctorTimer = null;

async function refreshDoctor() {
  try { doctorReport = await api.doctor(); }
  catch { doctorReport = null; }
  updateDoctorIndicator();
}

function updateDoctorIndicator() {
  const dot = document.getElementById('doctor-dot');
  const label = document.getElementById('doctor-label');
  const link = document.getElementById('doctor-indicator');
  if (!dot || !label) return;
  const health = (doctorReport && doctorReport.health) || 'unknown';
  dot.className = 'doctor-dot ' + health;
  const labels = { green: 'Healthy', amber: 'Needs attention', red: 'Action required', unknown: 'Checking…' };
  label.textContent = labels[health] || health;
  const failing = ((doctorReport && doctorReport.checks) || []).filter((c) => !c.ok);
  if (link) link.title = failing.length ? ('Needs attention: ' + failing.map((c) => c.label).join(', ')) : 'All checks passing';
}

// The build stamp this tab booted with (from env.js, loaded before app.js). A
// long-open SPA keeps running the JS it loaded, so an HTTP no-cache header alone
// can't save it from a mid-session redeploy - we compare against the live stamp.
const BOOT_VERSION = (window.__POSEIDEN_ENV__ && window.__POSEIDEN_ENV__.version) || '';
let updateNudged = false;

// Detect a redeploy: the server's build stamp no longer matches the one this tab
// booted with. Nudge a reload once. Skipped for dev/empty stamps (comparison is
// meaningless) and while offline (any fetch error is ignored).
async function checkForUpdate() {
  if (updateNudged || !BOOT_VERSION || BOOT_VERSION === 'dev') return;
  let live;
  try { live = (await api.health()).version; } catch { return; }
  if (live && live !== BOOT_VERSION) { updateNudged = true; showUpdateBanner(); }
}

function showUpdateBanner() {
  if (document.getElementById('update-banner')) return;
  // The whole banner is the reload affordance (it reads as a notification, so a click
  // anywhere on it should act) - the ✕ dismisses without reloading.
  const bar = el('div', {
    id: 'update-banner', class: 'update-banner', role: 'button', tabindex: '0',
    title: 'Reload to get the latest version',
    onclick: () => location.reload(),
  },
    el('span', {}, '✨ A new version of POSEIDEN is available — '),
    el('span', { class: 'update-banner-cta' }, 'Reload'),
    el('button', {
      class: 'update-banner-x', title: 'Dismiss',
      onclick: (e) => { e.stopPropagation(); bar.remove(); },
    }, '✕'));
  document.body.appendChild(bar);
}

function startDoctorPolling() {
  refreshDoctor();
  checkForUpdate();
  if (doctorTimer) clearInterval(doctorTimer);
  // Keep the light current, like crosspose's background monitor. Piggyback the
  // version check on the same cadence so a redeploy is noticed within ~45s.
  doctorTimer = setInterval(() => { refreshDoctor(); checkForUpdate(); }, 45000);
}

async function renderDoctor() {
  if (!doctorReport) await refreshDoctor();
  const wrap = el('div', {});
  wrap.appendChild(pageHead('Doctor', 'System health - dependencies & configuration',
    el('button', {
      class: 'btn', onclick: async (e) => {
        const b = e.currentTarget; b.disabled = true; b.textContent = 'Re-checking…';
        try { doctorReport = await api.doctorRecheck(); } catch { /* keep last */ }
        updateDoctorIndicator();
        await route();
      },
    }, '↻ Re-check')));

  if (!doctorReport || !doctorReport.checks || !doctorReport.checks.length) {
    wrap.appendChild(el('div', { class: 'card' }, el('div', { class: 'empty' }, 'No checks registered yet - add a team to start monitoring its access.')));
    return wrap;
  }
  const table = el('table', {});
  table.appendChild(el('tr', {}, [th('Check'), th('Status'), th('Detail'), th('')]));
  doctorReport.checks.forEach((c) => {
    table.appendChild(el('tr', {}, [
      el('td', { class: 'wrap' }, c.label),
      el('td', {}, doctorPill(c)),
      el('td', { class: 'wrap muted' }, c.message),
      el('td', {}, (!c.ok && c.can_fix) ? doctorFixButton(c) : ''),
    ]));
  });
  wrap.appendChild(el('div', { class: 'table-wrap' }, table));
  return wrap;
}

function doctorPill(c) {
  if (c.ok) return el('span', { class: 'pill ok' }, 'OK');
  return el('span', { class: 'pill ' + (c.severity === 'critical' ? 'err' : 'warn') }, c.severity === 'critical' ? 'Failed' : 'Warning');
}

function doctorFixButton(c) {
  const isSignIn = c.fix_action === 'sign-in';
  const restore = isSignIn ? 'Sign in' : 'Fix';
  return el('button', {
    class: 'btn btn-primary', onclick: async (e) => {
      const b = e.currentTarget; b.disabled = true; b.textContent = 'Fixing…';
      try {
        if (isSignIn) {
          await runSignIn();
        } else {
          const r = await api.doctorFix(c.id);
          toast(r.ok ? r.message : ('Fix failed: ' + r.message), !r.ok);
        }
        await refreshDoctor(); await route();
      } catch (err) {
        toast('Fix failed: ' + (err.message || err), true);
      } finally {
        b.disabled = false; b.textContent = restore;
      }
    },
  }, restore);
}

async function route() {
  const name = currentRoute();
  document.querySelectorAll('.sidebar nav a').forEach((a) => {
    a.classList.toggle('active', a.dataset.route === name);
  });
  updateModeBadge();
  clear(main).appendChild(el('div', { class: 'loading' }, 'Loading…'));
  try {
    const view = await ROUTES[name]();
    const container = clear(main);
    container.appendChild(view);
  } catch (e) {
    reportClientError(e && e.message, e && e.stack, '#' + name);
    clear(main).appendChild(errorPanel(name, e));
  }
}

// Forward an uncaught/handled frontend error to the backend so it lands in the
// same telemetry/log file as the Rust side (webview console errors are otherwise
// invisible to our logs). Best-effort - never throws.
function reportClientError(message, stack, url) {
  try {
    api.logClientError({ message: String(message || 'unknown'), stack: String(stack || ''), url: String(url || location.hash || '') });
  } catch { /* swallow - logging must never cascade */ }
}
if (typeof window !== 'undefined') {
  window.addEventListener('error', (e) => reportClientError(e.message, e.error && e.error.stack, e.filename));
  window.addEventListener('unhandledrejection', (e) => {
    const r = e.reason;
    reportClientError((r && r.message) || r, r && r.stack, location.hash);
  });
}

function updateModeBadge() {
  const badge = document.getElementById('mode-badge');
  if (!badge) return;
  const m = mode();
  const labels = { remote: '● Remote instance', desktop: '● Local (desktop)', web: '● Local (web)' };
  badge.textContent = labels[m] || m;
  badge.title = m === 'remote' ? `Connected to ${getInstanceUrl()}` : 'Using this instance’s own data';
}

function errorPanel(name, e) {
  // On a static host (this bundle served from GitHub/Cloudflare Pages) there's
  // no backend at this origin, so API calls fail. Rather than a bare error,
  // guide the user to connect the client to their running instance.
  if (mode() === 'web' && !getInstanceUrl()) {
    return el('div', { class: 'card' }, [
      el('h2', {}, 'Connect POSEIDEN to an instance'),
      el('p', { class: 'muted' },
        'This is the POSEIDEN web client. It needs a running POSEIDEN instance to show data - ' +
        'either the hosted (container) instance, or your own. Enter its URL in Settings to connect.'),
      el('div', { style: 'margin-top:10px' }, [
        el('button', { class: 'btn btn-primary', onclick: showSettings }, 'Open Settings →'),
      ]),
    ]);
  }
  return el('div', { class: 'card' }, [
    el('h2', {}, 'Could not load ' + name),
    el('p', { class: 'muted' }, String(e.message || e)),
    e && e.stack ? el('pre', { style: 'font-size:11px;white-space:pre-wrap;color:var(--ink-soft);max-height:200px;overflow:auto' }, String(e.stack)) : null,
    el('p', { class: 'muted' }, 'If you just started POSEIDEN, the first poll may still be running. Try Refresh.'),
  ].filter(Boolean));
}

function pageHead(title, sub, actions) {
  return el('div', { class: 'page-head' }, [
    el('div', {}, [el('h1', {}, title), sub ? el('div', { class: 'sub' }, sub) : null]),
    actions || null,
  ]);
}

// ── Dashboard ───────────────────────────────────────────────────────
async function renderDashboard() {
  const d = await api.dashboard();
  // A never-polled instance has no data - NOT zero data. Showing a green "0"
  // there reads as "verified clean" when we simply never looked (fresh boot,
  // not signed in, or the first poll hasn't run). Distinguish the two: the
  // tiles show a muted placeholder until there's a real poll behind them.
  const polled = !!d.last_polled_at;
  const wrap = el('div', {});
  wrap.appendChild(pageHead('Dashboard', polled ? `Last polled ${ago(d.last_polled_at)}` : 'Not polled yet'));

  // Velocity metrics come from the report engine (built-in Stat specs), run in
  // parallel. Failures degrade to a placeholder rather than breaking the page.
  const statOf = (r) => (r && r.series[0] && r.series[0].points[0] ? r.series[0].points[0].value : null);
  const pctOf = (r) => { const v = statOf(r); return v == null ? null : `${Math.round(v * 100)}%`; };
  let created = null, closed = null, mergeRate = null, successRate = null;
  if (polled) {
    [created, closed, mergeRate, successRate] = await Promise.all([
      api.runReportNamed('work-items-created-7d').catch(() => null),
      api.runReportNamed('work-items-closed-7d').catch(() => null),
      api.runReportNamed('pr-merge-rate').catch(() => null),
      api.runReportNamed('pipeline-success-rate').catch(() => null),
    ]);
  }
  const rateTone = (r) => { const v = statOf(r); return v == null ? '' : (v >= 0.85 ? 'ok' : v >= 0.6 ? 'warn' : 'err'); };

  // Topline totals (clickable) interleaved with engine-driven velocity metrics.
  const tiles = el('div', { class: 'grid', style: 'grid-template-columns: repeat(auto-fit, minmax(150px, 1fr))' }, [
    statTile(polled ? d.total_work_items : null, 'Work items', '', '#work-items'),
    statTile(polled ? statOf(created) : null, 'Created (7d)', '', '#reports?report=work-items-created-7d'),
    statTile(polled ? statOf(closed) : null, 'Closed (7d)', 'ok', '#reports?report=work-items-closed-7d'),
    statTile(polled ? d.open_prs : null, 'Open PRs', '', '#pull-requests'),
    statTile(polled ? pctOf(mergeRate) : null, 'PR merge rate', rateTone(mergeRate), '#reports?report=pr-merge-rate'),
    statTile(polled ? (d.pipelines || []).length : null, 'Pipelines', '', '#pipelines'),
    statTile(polled ? pctOf(successRate) : null, 'Pipeline success', rateTone(successRate), '#reports?report=pipeline-success-rate'),
  ]);
  wrap.appendChild(tiles);

  // Until we've polled, the breakdown can't claim "clean" / "none" - say why
  // there's nothing to show (the Doctor is where the fix lives).
  const pending = polled ? null : pendingMessage();
  wrap.appendChild(el('div', { style: 'margin-top:16px' }, flagBreakdownCard(d, pending)));
  return wrap;
}

// Why the dashboard has no data yet. Not signed in is the actionable case (the
// Doctor's "Action required" indicator + panel offer the fix); otherwise the
// first poll just hasn't landed. Kept neutral - it never asserts a clean backlog.
function pendingMessage() {
  return authState && !authState.signed_in
    ? 'Sign in to load data.'
    : 'No data loaded yet.';
}

// value === null means "unknown" (not yet polled) - render a muted en-dash
// placeholder rather than a 0 that would look like a real, reassuring count.
// value === null means "unknown" (not yet polled) - render a muted en-dash
// placeholder rather than a 0 that would look like a real, reassuring count. An
// `href` makes the whole tile a link to its (focused) screen.
function statTile(value, label, tone, href) {
  const unknown = value == null;
  const cls = 'card stat ' + (unknown ? 'unknown' : (tone || '')) + (href ? ' stat-link' : '');
  const kids = [
    el('div', { class: 'value' }, unknown ? '–' : String(value)),
    el('div', { class: 'label' }, label),
  ];
  if (href) return el('a', { class: cls, href, title: `View ${label.toLowerCase()}` }, kids);
  return el('div', { class: cls }, kids);
}

const FLAG_LABELS = {
  untagged: 'Untagged',
  missing_required_tag: 'Missing required tag',
  disallowed_tag: 'Disallowed tag',
  stale: 'Stale',
  stale_state_tag: 'Stale tag on resolved item',
  underspecified: 'Empty body',
  duplicate: 'Duplicate title',
  bad_title: 'Bad title',
  near_duplicate: 'Near-duplicate',
  ai_audit: 'AI healthcheck',
  // Pull-request + pipeline entity-flag codes.
  'stale-open': 'Stale open PR',
  'stale-draft': 'Stale draft PR',
  'no-work-item': 'No work item',
  'never-run': 'Never run',
  failing: 'Failing',
};

// Flag codes that are errors (red); everything else is a warning (amber). Keeps
// the dashboard's colouring aligned with each flag's real severity.
const FLAG_ERROR_CODES = new Set(['disallowed_tag', 'failing']);

function flagBreakdownCard(d, pending) {
  const card = el('div', { class: 'card' }, [el('h2', {}, 'Health check')]);
  if (pending) {
    card.appendChild(el('div', { class: 'empty' }, pending));
    return card;
  }
  // One group per domain, each showing its rule-break counts (coloured by
  // severity) plus a green "passing" count - the healthy side of the ledger.
  const totalPrs = (d.open_prs || 0) + (d.draft_prs || 0);
  const groups = [
    { title: 'Work items', route: 'work-items', codes: d.flags_by_code, total: d.total_work_items, flagged: d.flagged_items, okLabel: 'Clean' },
    { title: 'Pull requests', route: 'pull-requests', codes: d.pr_flags_by_code, total: totalPrs, flagged: d.flagged_prs, okLabel: 'Passing' },
    { title: 'Pipelines', route: 'pipelines', codes: d.pipeline_flags_by_code, total: (d.pipelines || []).length, flagged: d.flagged_pipelines, okLabel: 'Healthy' },
  ].filter((g) => (g.total || 0) > 0);
  if (!groups.length) {
    card.appendChild(el('div', { class: 'empty' }, 'Nothing polled yet.'));
    return card;
  }
  groups.forEach((g) => {
    card.appendChild(el('a', { class: 'flag-group-head', href: `#${g.route}?flagged=1`, title: `Go to flagged ${g.title.toLowerCase()}` }, g.title));
    // Totals subtitle: how many of the domain are flagged.
    card.appendChild(el('div', { class: 'flag-group-sub' }, `${g.flagged} flagged of ${g.total} total`));
    const table = el('table', {});
    (g.codes || []).forEach((c) => {
      const sev = FLAG_ERROR_CODES.has(c.tag) ? 'err' : 'warn';
      const label = FLAG_LABELS[c.tag] || c.tag;
      // Each row drills into its screen filtered to just this flag.
      table.appendChild(el('tr', {
        class: 'hc-row', title: `View ${g.title.toLowerCase()} flagged "${label}"`,
        onclick: () => { location.hash = `#${g.route}?flag=${encodeURIComponent(c.tag)}`; },
      }, [
        el('td', {}, label),
        el('td', { class: 'hc-count hc-' + sev }, String(c.count)),
      ]));
    });
    // The success side: entities with no flag at all.
    const clean = Math.max(0, (g.total || 0) - (g.flagged || 0));
    table.appendChild(el('tr', { class: 'hc-ok-row' }, [
      el('td', {}, g.okLabel),
      el('td', { class: 'hc-count hc-ok' }, String(clean)),
    ]));
    card.appendChild(el('div', { class: 'table-wrap' }, table));
  });
  return card;
}


// ── Work Items ──────────────────────────────────────────────────────
async function renderWorkItems() {
  const { items, flags } = await api.tickets();
  const flagsById = new Map();
  for (const f of flags) {
    if (!flagsById.has(f.work_item_id)) flagsById.set(f.work_item_id, []);
    flagsById.get(f.work_item_id).push(f);
  }
  const flagsOf = (it) => flagsById.get(it.id) || [];

  // Re-fetch tickets and swap the table's data in place - used after an AI run so the
  // new suggestions (and any changed flags) appear WITHOUT a full re-render, which
  // would wipe the user's filter/sort/selection. Falls back to a full route re-render.
  async function refreshRows() {
    try {
      const fresh = await api.tickets();
      flagsById.clear();
      for (const f of fresh.flags) {
        if (!flagsById.has(f.work_item_id)) flagsById.set(f.work_item_id, []);
        flagsById.get(f.work_item_id).push(f);
      }
      if (table && table.setRows) table.setRows(fresh.items);
      else route();
    } catch { route(); }
  }

  const wrap = el('div', { class: 'view-fill' });
  wrap.appendChild(pageHead('Work Items', `${items.length} work items`,
    el('button', {
      class: 'btn btn-xs', title: 'Export the current filtered view (or your selection) to CSV - id, tags, suggestions, flags',
      onclick: () => {
        const sel = table ? table.getSelection() : [];
        const rows = sel.length ? sel : (table && table.getVisibleRows ? table.getVisibleRows() : items);
        if (!rows.length) { toast('Nothing to export.'); return; }
        downloadBlob(new Blob([workItemsToCsv(rows, flagsOf)], { type: 'text/csv;charset=utf-8' }), 'work-items.csv');
      },
    }, '⭳ CSV')));

  // Show a Team column only in the "All teams" roll-up - redundant when the view
  // is already scoped to one team.
  const showTeam = !getTeamScope();

  // Effective ruleset for the current scope, so the tag cloud can colour tags
  // by validity (allowed = green, disallowed / not-allow-listed = red). A team's
  // own [team.rules] override wins; otherwise the instance default.
  let cfg;
  try { cfg = await api.config(); } catch { cfg = null; }
  const scopedTeam = (cfg?.team || []).find((t) => t.name === getTeamScope());
  const rules = (scopedTeam && scopedTeam.rules) || cfg?.rules || {};
  // "Good data" for the tag chip editor: the sanctioned tags (wildcards dropped,
  // they're patterns not literals) plus any keyword-mapped tags. Freetext still
  // allowed; this just powers autocomplete.
  const tagOptions = [...new Set([
    ...(rules.allowed_tags || []).filter((t) => !t.includes('*')),
    ...(rules.tag_keywords || []).map((tk) => tk.tag).filter(Boolean),
  ])];
  // The AI candidate set (superset of tagOptions): expands wildcard-required slots
  // (`area:*`/`source:*`) into the concrete values in use across the loaded scope,
  // so the in-browser model can actually satisfy them. Mirrors the server.
  const aiCandidates = aiCandidateTags(rules, items.flatMap((it) => it.tags || []));
  // Per-tag keyword hints (tag -> keywords), so the model knows what each candidate
  // means (e.g. area:platform-deployment keys on "platform deployment"). Mirrors the
  // server's tag_hints; keyed lowercase to match webgpu.js annotate().
  const aiHints = Object.fromEntries(
    (rules.tag_keywords || [])
      .filter((k) => k.tag && (k.keywords || []).length)
      .map((k) => [k.tag.toLowerCase(), k.keywords]));
  // Pre-filter to rule-breaks (or one specific flag) when arrived from a
  // dashboard tile / Health-check row.
  const flagCode = routeParams().get('flag');
  const state = listState('work-items', flagCode);

  // "Rule Breaks" toggle - a switch in the table toolbar that flips a predicate
  // the table re-reads (only items with hygiene-rule violations).
  let table;
  const flaggedToggle = ruleBreaksToggle(state, () => table);
  const emptyToggle = hideEmptyToggle(state, () => table);

  // After an inline edit writes back to ADO, patch everything locally so the
  // view is correct without a re-poll: the row's fields, its recomputed flags
  // (returned by the server), and the table itself. Per-column filters (incl.
  // the Tags column) live in the table and re-evaluate on refresh.
  function afterEdit(it, res) {
    it.state = res.item.state;
    it.tags = res.item.tags;
    it.linked_pr_ids = res.item.linked_pr_ids || [];
    it.linked_prs = res.item.linked_prs || [];
    // Reconcile suggestions against the freshly-written tags so the Suggested
    // column can't keep showing a tag the item now carries (whatever the apply
    // path was: chip, inline editor, or bulk). Drop a suggestion when its tag is
    // already applied, or - for a rewrite - when its legacy `replaces` tag is gone
    // (nothing left to migrate). Mirrors the server's drop-if-applied on refetch.
    const applied = new Set((it.tags || []).map((t) => t.toLowerCase()));
    it.tag_suggestions = (it.tag_suggestions || []).filter((s) =>
      !applied.has(s.tag.toLowerCase())
      && !(s.replaces && !applied.has(s.replaces.toLowerCase())));
    flagsById.set(it.id, res.flags || []);
    table.refresh();
  }

  const columns = [
    { label: 'ID', sort: 'number', value: (it) => it.id, render: (it) => linkOut('#' + it.id, it.url) },
    showTeam ? { label: 'Team', filterChoices: true, value: (it) => it.team || '' } : null,
    {
      label: 'Title', class: 'wrap', value: (it) => it.title || '',
      render: (it) => el('span', { class: 'wi-title' }, [
        el('button', {
          class: 'wi-edit', type: 'button', title: 'Edit work-item fields',
          onclick: (e) => { e.stopPropagation(); openWorkItemEditor(it, refreshRows); },
        }, '✎'),
        el('span', {}, it.title || '(untitled)'),
      ]),
    },
    { label: 'Type', filterChoices: true, value: (it) => it.work_item_type || '' },
    {
      label: 'State', filterChoices: true, value: (it) => it.state || '',
      render: (it) => el('span', { class: 'pill muted' }, it.state || ''),
      // Editable: pick a state; commit writes it back to Azure DevOps.
      edit: {
        type: 'select',
        options: ['New', 'Resolved', 'Closed'],
        get: (it) => it.state || '',
        commit: async (it, value) => {
          const res = await api.updateWorkItem(it.id, { team: it.team, state: value });
          afterEdit(it, res);
          toast(`#${it.id} → ${it.state}`);
        },
      },
    },
    { label: 'Assignee', filterChoices: true, value: (it) => it.assigned_to || '', render: (it) => it.assigned_to || el('span', { class: 'muted' }, '-') },
    {
      label: 'Tags', class: 'wrap', value: (it) => (it.tags || []).join(' '),
      // Multi-value checkbox filter: pick tags to keep items carrying ANY of them.
      // The dropdown has a search box (backlogs have many tags) and a "(blank)"
      // entry for untagged items.
      filterChoices: true, choiceValues: (it) => it.tags || [],
      render: (it) => {
        const tags = it.tags || [];
        if (!tags.length) return el('span', { class: 'muted' }, '-');
        // Colour each chip by rule validity (green = allowed, red = not), the
        // cue the standalone tag bar used to carry.
        return tags.map((t) => {
          const cls = classifyTag(t, rules);
          return el('span', { class: 'tag' + (cls === 'neutral' ? '' : ' ' + cls) }, t);
        });
      },
      // Editable: add/remove tag chips; commit writes the full set back to ADO.
      // `suggestions` powers autocomplete from the sanctioned/keyword tags.
      edit: {
        type: 'chips',
        suggestions: tagOptions,
        get: (it) => it.tags || [],
        commit: async (it, tags) => {
          const res = await api.updateWorkItem(it.id, { team: it.team, tags });
          afterEdit(it, res);
          toast(`#${it.id} tags updated`);
        },
      },
    },
    {
      label: 'Suggested', class: 'wrap',
      value: (it) => (it.tag_suggestions || []).map((s) => s.tag).join(' '),
      render: (it) => suggestionChips(it, afterEdit, flagsOf(it), refreshRows),
    },
    {
      label: 'PRs', class: 'wrap',
      value: (it) => (it.linked_prs || []).map((p) => p.id).join(' '),
      render: (it) => prLinkChips(it.linked_prs, it.team),
      // Editable: add a PR by id / remove a chip. Each delta is a discrete
      // artifact-link write; we diff the set and issue one call per change.
      edit: {
        type: 'chips',
        placeholder: 'add PR id…',
        get: (it) => (it.linked_pr_ids || []).map(String),
        commit: async (it, ids) => {
          const after = new Set(ids.map((s) => parseInt(s, 10)).filter((n) => Number.isInteger(n) && n > 0));
          const before = new Set((it.linked_pr_ids || []).map(Number));
          const toRemove = [...before].filter((n) => !after.has(n));
          const toAdd = [...after].filter((n) => !before.has(n));
          if (!toRemove.length && !toAdd.length) return;
          let res = null;
          for (const prId of toRemove) res = await api.linkWorkItemPr(it.id, { team: it.team, prId, link: false });
          for (const prId of toAdd) res = await api.linkWorkItemPr(it.id, { team: it.team, prId, link: true });
          if (res) { afterEdit(it, res); toast(`#${it.id} PR links updated`); }
        },
      },
    },
    {
      label: 'Flags', class: 'wrap',
      value: (it) => flagsOf(it).map(flagShort).join(' ') || 'clean',
      render: (it) => {
        const fl = flagsOf(it);
        return fl.length
          ? fl.map((f) => el('span', { class: 'flagchip ' + (f.severity === 'error' ? 'err' : 'warn'), title: f.message }, flagShort(f)))
          : el('span', { class: 'pill ok' }, 'clean');
      },
    },
  ].filter(Boolean);

  // AI tag suggester - only when a model is configured. Runs over the SELECTED
  // rows (disabled until you tick some), stores advisory suggestions, then reloads
  // so they appear in the Suggested column. Never auto-applies: each suggestion
  // chip is click-to-apply. Selection-scoped so a slow model never grinds the whole
  // backlog - you pick the handful you want.
  let aiBtn = null;
  let aiRunning = false;
  let hcBtn = null;
  let hcRunning = false;
  try {
    const st = await api.aiStatus().catch(() => ({ enabled: false }));
    // Decide the engine: if the top client-usable integration is a WebGPU one (and
    // this browser has WebGPU), inference runs IN-PAGE on the user's GPU; otherwise
    // the server handles it (offline candle / online / custom endpoint).
    // Shared client dispatcher decides the backend (see lib/ai.js). A 'browser' verdict
    // means WebGPU inference runs in-page on the user's GPU.
    const be = await activeBackend();
    const webgpuInteg = be.where === 'browser' ? { offline_model: be.model } : null;

    if ((st && st.enabled) || webgpuInteg) {
      aiBtn = el('button', {
        class: 'btn btn-xs', type: 'button', disabled: true,
        title: webgpuInteg
          ? 'Suggest tags for the selected items IN YOUR BROWSER on the GPU (WebGPU, experimental)'
          : 'Select work items, then suggest canonical tags for them with AI (advisory)',
        onclick: async (e) => {
          const b = e.currentTarget;
          const rows = table ? table.getSelection() : [];
          if (!rows.length) return; // disabled when empty; guard anyway
          const ids = rows.map((r) => r.id);
          const orig = b.textContent;
          aiRunning = true;
          b.disabled = true;
          b.textContent = 'Starting…';
          try {
            if (webgpuInteg) {
              // Pull descriptions for the selected items (the list omits bodies) so the
              // in-browser tagger can use them when the owner opted in; title-only if not.
              let rowsForTagging = rows;
              try {
                const descs = await api.workItemDescriptions(getTeamScope(), ids);
                if (descs && Object.keys(descs).length) {
                  rowsForTagging = rows.map((r) => ({ ...r, description: descs[r.id] ?? null }));
                }
              } catch { /* body optional - fall back to title-only */ }
              // Skip underspecified items (too little body to tag from): they get the
              // deterministic "to refine" suggestion server-side; don't let the model
              // guess an area from nothing. Mirrors poseiden-rules::is_underspecified.
              const refineTag = (rules.refine_tag || '').trim();
              if (refineTag) {
                const min = rules.refine_min_chars ?? 40;
                const before = rowsForTagging.length;
                rowsForTagging = rowsForTagging.filter((r) => (r.description || '').trim().length >= min);
                const skipped = before - rowsForTagging.length;
                // Don't claim a "to refine" suggestion was made - the server only adds
                // it when the item is missing a required tag AND isn't already tagged
                // (an item already carrying it, like one flagged "To Refine", gets
                // nothing). Just state honestly why these were left out of the AI run.
                if (skipped) toast(`Skipped ${skipped} item${skipped === 1 ? '' : 's'} too sparse to AI-tag - needs refinement.`);
              }
              if (!rowsForTagging.length) { await refreshRows(); return; }
              // In-browser inference on the user's GPU, then POST results to the
              // server which re-validates + stores them (trust boundary).
              const results = await runWebGpuTagging(
                webgpuInteg.offline_model, rowsForTagging, aiCandidates,
                (s) => { b.textContent = s || 'Loading…'; },
                (done, total) => { b.textContent = `Suggesting… ${done}/${total}`; },
                rules.required_tags || [], aiHints, rules.team_background || '',
                rules.max_suggestions);
              b.textContent = 'Storing…';
              const sum = await api.storeTagSuggestions(getTeamScope(), results);
              toast(`WebGPU: tagged ${sum.with_suggestions ?? 0}/${sum.considered ?? 0} items (${sum.suggestions ?? 0} tags)`);
              await refreshRows();
              return;
            }
            // Server path: background run + poll (slow offline models never block).
            await api.runTagSuggestions(getTeamScope(), ids);
            for (;;) {
              await new Promise((r) => setTimeout(r, 2000));
              let s;
              try { s = await api.tagSuggestionsStatus(); } catch { continue; }
              if (s.state === 'running') {
                b.textContent = s.total ? `Suggesting… ${s.done}/${s.total}` : 'Suggesting…';
                continue;
              }
              if (s.state === 'done') {
                const sum = s.summary || {};
                toast(`AI suggested tags for ${sum.with_suggestions ?? 0}/${sum.considered ?? 0} items (${sum.suggestions ?? 0} tags)`);
                await refreshRows();
                return;
              }
              toast('AI suggestion failed: ' + (s.error || 'unknown error'), true);
              break;
            }
          } catch (err) {
            toast('AI suggestion failed: ' + (err?.message || err), true);
          } finally {
            // Runs even on the early returns above, so the button always resets and
            // the (preserved) selection re-enables it.
            aiRunning = false;
            b.textContent = orig;
            b.disabled = !(table && table.getSelection().length); // re-enable only if still selected
          }
        },
      }, '✨ Suggest tags');
    }

    // "Run healthcheck": an on-demand AI audit of the selected items' DATA QUALITY
    // (vague titles, contradictory / boilerplate bodies) - distinct from tagging.
    // Same backend split as tagging: WebGPU runs in-page, otherwise the server. The
    // findings land as advisory `ai_audit` flags (chips + dashboard count + filter).
    if ((st && st.enabled) || webgpuInteg) {
      hcBtn = el('button', {
        class: 'btn btn-xs', type: 'button', disabled: true,
        title: webgpuInteg
          ? 'Audit the selected items for data-quality problems IN YOUR BROWSER on the GPU (WebGPU, experimental)'
          : 'Select work items, then run an AI healthcheck for data-quality problems (advisory)',
        onclick: async (e) => {
          const b = e.currentTarget;
          const rows = table ? table.getSelection() : [];
          if (!rows.length) return;
          const ids = rows.map((r) => r.id);
          const orig = b.textContent;
          hcRunning = true;
          b.disabled = true;
          b.textContent = 'Starting…';
          try {
            if (webgpuInteg) {
              // The server builds the exact prompts (single source of truth); the
              // browser's GPU runs each, and replies are posted back to be re-parsed
              // + stored server-side (trust boundary). Mirrors the drafting handshake.
              const prompts = await api.healthcheckAuditPrompts(getTeamScope(), ids);
              const results = [];
              for (let i = 0; i < prompts.length; i++) {
                const p = prompts[i];
                b.textContent = `Auditing… ${i + 1}/${prompts.length}`;
                try {
                  const text = await runWebGpuChat(webgpuInteg.offline_model, p.system, p.user,
                    (s) => { if (s) b.textContent = s; });
                  results.push({ id: p.id, text });
                } catch (err) {
                  console.warn('WebGPU audit failed for item', p.id, err);
                }
              }
              b.textContent = 'Storing…';
              const sum = await api.storeHealthcheckAudit(getTeamScope(), results);
              toast(`Healthcheck: flagged ${sum.flagged ?? 0}/${sum.considered ?? 0} items (${sum.findings ?? 0} concerns)`);
              await refreshRows();
              return;
            }
            // Server path: background run + poll (slow models never block the request).
            await api.runHealthcheckAudit(getTeamScope(), ids);
            for (;;) {
              await new Promise((r) => setTimeout(r, 2000));
              let s;
              try { s = await api.healthcheckAuditStatus(); } catch { continue; }
              if (s.state === 'running') {
                b.textContent = s.total ? `Auditing… ${s.done}/${s.total}` : 'Auditing…';
                continue;
              }
              if (s.state === 'done') {
                const sum = s.summary || {};
                toast(`Healthcheck: flagged ${sum.flagged ?? 0}/${sum.considered ?? 0} items (${sum.findings ?? 0} concerns)`);
                await refreshRows();
                return;
              }
              toast('Healthcheck failed: ' + (s.error || 'unknown error'), true);
              break;
            }
          } catch (err) {
            toast('Healthcheck failed: ' + (err?.message || err), true);
          } finally {
            hcRunning = false;
            b.textContent = orig;
            b.disabled = !(table && table.getSelection().length);
          }
        },
      }, '🩺 Run healthcheck');
    }
  } catch { /* status unavailable - just hide the button */ }

  // Bulk-retag bar - the payoff of the multi-select checkboxes. Reuses the exact
  // single-row write path (`api.updateWorkItem` -> `afterEdit`), one call per
  // selected item, so a bulk apply is just N inline edits and stays inside the
  // "explicit, user-initiated write-back" scope (each op is one click, on rows
  // the user hand-picked). Skips no-op rows (already has the tag / already in the
  // target state) so nothing pointless is written back to ADO.
  const bulkTagList = el('datalist', { id: 'bulk-tag-options' },
    tagOptions.map((t) => el('option', { value: t })));
  const bulkTag = el('input', {
    type: 'text', class: 'bulk-tag-input', placeholder: 'tag…', list: 'bulk-tag-options',
  });
  const bulkState = el('select', { class: 'bulk-state-select' },
    [el('option', { value: '' }, 'set state…'), ...['New', 'Resolved', 'Closed'].map((s) => el('option', { value: s }, s))]);
  const bulkCount = el('span', { class: 'bulk-count muted' }, '');
  const bulkStatus = el('span', { class: 'bulk-status muted' }, '');
  const bulkButtons = [];
  const setBulkBusy = (busy) => {
    bulkButtons.forEach((b) => { b.disabled = busy; });
    bulkTag.disabled = busy;
    bulkState.disabled = busy;
  };

  // Apply `mutate(it)` to every selected row. `mutate` returns the changes object
  // for `updateWorkItem`, or null to skip that row (no-op). Sequential on purpose
  // - ordered progress, and it keeps ADO write pressure gentle.
  async function applyBulk(label, mutate, opts = {}) {
    const rows = table.getSelection();
    if (!rows.length) { toast('Select some work items first.'); return; }
    const changed = rows.map((it) => [it, mutate(it)]).filter(([, ch]) => ch);
    if (!changed.length) { toast(`Nothing to change (${label.toLowerCase()}).`); return; }
    if (!confirm(`${label} on ${changed.length} work item${changed.length === 1 ? '' : 's'}? This writes back to Azure DevOps.`)) return;
    setBulkBusy(true);
    let ok = 0; const failed = [];
    for (let i = 0; i < changed.length; i++) {
      const [it, ch] = changed[i];
      bulkStatus.textContent = `${label}… ${i + 1}/${changed.length}`;
      try {
        const res = await api.updateWorkItem(it.id, { team: it.team, ...ch });
        afterEdit(it, res);
        ok++;
      } catch (err) {
        const detail = err?.message || String(err);
        // The toast can only show the first failure, truncated - log the full
        // detail per item so the real reason survives (e.g. an unrelated
        // required-field validation error on the work item makes ADO reject the
        // whole PATCH with a 400, which is otherwise invisible).
        console.error(`[${label}] #${it.id} failed:`, detail);
        failed.push(`#${it.id}: ${detail}`);
      }
    }
    setBulkBusy(false);
    bulkStatus.textContent = '';
    // Re-fetch when asked (apply-suggestions) so consumed suggestions + cleared flags
    // settle; otherwise a light in-place refresh. Both keep the filter/selection.
    if (opts.refetch) { await refreshRows(); } else { table.refresh(); }
    if (failed.length) {
      console.error(`[${label}] ${ok} updated, ${failed.length} failed:\n${failed.join('\n')}`);
      toast(`${label}: ${ok} updated, ${failed.length} failed (${failed[0]}${failed.length > 1 ? ', …' : ''})`, true);
    }
    else toast(`${label}: ${ok} work item${ok === 1 ? '' : 's'} updated.`);
  }

  const mkBulkBtn = (text, title, fn) => {
    const b = el('button', { class: 'btn bulk-btn', type: 'button', title, onclick: fn }, text);
    bulkButtons.push(b);
    return b;
  };
  const addTagBtn = mkBulkBtn('+ Add tag', 'Add the typed tag to every selected item', () => {
    const t = bulkTag.value.trim();
    if (!t) { bulkTag.focus(); return; }
    applyBulk('Add tag', (it) => {
      const cur = it.tags || [];
      return cur.some((x) => x.toLowerCase() === t.toLowerCase()) ? null : { tags: [...cur, t] };
    });
  });
  const removeTagBtn = mkBulkBtn('- Remove tag', 'Remove the typed tag from every selected item', () => {
    const t = bulkTag.value.trim();
    if (!t) { bulkTag.focus(); return; }
    applyBulk('Remove tag', (it) => {
      const cur = it.tags || [];
      const next = cur.filter((x) => x.toLowerCase() !== t.toLowerCase());
      return next.length === cur.length ? null : { tags: next };
    });
  });
  bulkState.addEventListener('change', () => {
    const s = bulkState.value;
    bulkState.value = '';
    if (!s) return;
    applyBulk('Set state', (it) => (it.state === s ? null : { state: s }));
  });
  // Backfill accelerator: accept EVERY suggestion (keyword + AI adds, and alias
  // rewrites) on each selected row in one sweep - the Suggested column, applied en
  // masse. Rewrites drop their legacy tag; adds that are already present are skipped.
  const applySuggBtn = mkBulkBtn('✓ Apply suggestions', 'Apply every suggested add, rewrite, and flagged removal on each selected item', () => {
    applyBulk('Apply suggestions', (it) => {
      // Mirror the per-row Suggested column: apply the ADD/REWRITE chips (from
      // tag_suggestions) AND the "- tag" REMOVAL chips (from stale/disallowed flags).
      // Removals were previously skipped here, so a bulk apply left "to refine on a
      // resolved item" (and other flagged tags) in place - "apply did nothing".
      const sugg = it.tag_suggestions || [];
      const flags = flagsOf(it) || [];
      let tags = [...(it.tags || [])];
      const rewritten = new Set(sugg.filter((s) => s.replaces).map((s) => s.replaces.toLowerCase()));
      for (const s of sugg) {
        if (s.replaces) tags = tags.filter((t) => t.toLowerCase() !== s.replaces.toLowerCase());
        if (!tags.some((t) => t.toLowerCase() === s.tag.toLowerCase())) tags.push(s.tag);
      }
      // Removals last, so a removal wins over any add; skip ones a rewrite migrates.
      for (const f of flags) {
        if ((f.code === 'stale_state_tag' || f.code === 'disallowed_tag') && f.tag
            && !rewritten.has(f.tag.toLowerCase())) {
          tags = tags.filter((t) => t.toLowerCase() !== f.tag.toLowerCase());
        }
      }
      const cur = it.tags || [];
      const unchanged = tags.length === cur.length && tags.every((t) => cur.some((c) => c.toLowerCase() === t.toLowerCase()));
      return unchanged ? null : { tags };
    }, { refetch: true });
  });

  const bulkBar = el('div', { class: 'bulk-bar', hidden: true }, [
    bulkCount, bulkTag, addTagBtn, removeTagBtn, applySuggBtn, bulkState, bulkStatus, bulkTagList,
  ]);

  // "Find duplicates": a whole-backlog scan (not a selection) for reworded near-dupes,
  // deterministic + server-side (no AI), storing near_duplicate flags. Always available.
  const dupBtn = el('button', {
    class: 'btn btn-xs', type: 'button',
    title: 'Scan the whole backlog for near-duplicate (reworded) titles — advisory flags',
    onclick: async () => {
      const b = dupBtn;
      const orig = b.textContent;
      b.disabled = true;
      b.textContent = '🔎 Scanning…';
      try {
        const sum = await api.scanDuplicates(getTeamScope());
        toast(`Duplicate scan: ${sum.flagged ?? 0} of ${sum.scanned ?? 0} items resemble another`);
        await refreshRows();
      } catch (err) {
        toast('Duplicate scan failed: ' + (err?.message || err), true);
      } finally {
        b.disabled = false;
        b.textContent = orig;
      }
    },
  }, '🔎 Find duplicates');

  table = dataTable(columns, items, {
    persistKey: 'work-items',
    fill: true, // fill the view height; scroll rows, keep header + toolbar fixed
    initialSort: { index: 0, dir: -1 }, // ID descending - newest work items first
    emptyText: 'No matching work items.',
    // Rule-break filtering (all flagged, or one specific flag code); tag
    // filtering is the Tags column's own per-column filter input.
    predicate: (it) => passesFlagFilter(state, flagsOf(it).map((f) => f.code)),
    // View switcher leads; then action buttons in the order you'd run them:
    // dedupe -> triage quality -> classify.
    toolbar: [mkViewSelect(), flaggedToggle, emptyToggle, dupBtn, hcBtn, aiBtn, flagFilterChip(state, 'work-items')].filter(Boolean),
    pageSize: getPageSize(),
    selectable: true,
    rowKey: (it) => it.id,
    onCountClick: () => showSettings('display'),
    // Reveal the bulk bar only while rows are selected; keep the count in sync.
    onSelectionChange: (n) => {
      bulkBar.hidden = n === 0;
      bulkCount.textContent = n ? `${n} selected` : '';
      // Enable "Suggest tags" only when rows are selected (and not mid-run); label
      // it with the count so it's clear it acts on the selection.
      if (aiBtn && !aiRunning) {
        aiBtn.disabled = n === 0;
        aiBtn.textContent = n ? `✨ Suggest tags (${n})` : '✨ Suggest tags';
      }
      if (hcBtn && !hcRunning) {
        hcBtn.disabled = n === 0;
        hcBtn.textContent = n ? `🩺 Run healthcheck (${n})` : '🩺 Run healthcheck';
      }
    },
  });
  wrap.appendChild(bulkBar);
  // The content host swaps between the table and the board (Kanban) view. Both read
  // the same `items`/`flags`; the board is rebuilt on entry (no per-board state to
  // keep), while the table node persists so its selection/filters survive a round trip.
  const host = el('div', { class: 'wi-host' });
  wrap.appendChild(host);
  renderHost();
  return wrap;

  // ── view switching (declarations - hoisted, so the toolbar above can use them) ──
  function renderHost() {
    clear(host);
    host.appendChild(state.view && state.view !== 'table' ? buildBoard(state.view) : table);
  }

  // The View <select>: Table, Board · State, and a Board · <prefix> per tag axis found
  // in the data (area / product / source / …). A fresh node each call (it lives in
  // whichever toolbar is currently mounted).
  function mkViewSelect() {
    const prefixes = [...new Set((items || [])
      .flatMap((it) => it.tags || [])
      .map((t) => (t.includes(':') ? t.split(':')[0].toLowerCase() : ''))
      .filter(Boolean))].sort();
    const opts = [
      { v: 'table', label: '▤ Table' },
      { v: 'state', label: '▦ Board · State' },
      ...prefixes.map((p) => ({ v: p, label: `▦ Board · ${p[0].toUpperCase()}${p.slice(1)}` })),
    ];
    const sel = el('select', {
      class: 'view-select', title: 'Switch between table and board views', 'aria-label': 'View',
      onchange: (e) => { state.view = e.target.value; saveToggles(state); renderHost(); },
    }, opts.map((o) => {
      const opt = el('option', { value: o.v }, o.label);
      if ((state.view || 'table') === o.v) opt.selected = true;
      return opt;
    }));
    return sel;
  }

  // Build the Kanban board for `axis` ('state' or a tag prefix). Same predicate
  // filters as the table (Rule Breaks / Hide empty / flag chip) apply to which cards
  // show; the toggles re-render the host so they work in board mode too.
  function buildBoard(axis) {
    const shown = items.filter((it) => passesFlagFilter(state, flagsOf(it).map((f) => f.code)));
    const toolbar = el('div', { class: 'dt-toolbar' }, [
      mkViewSelect(),
      ruleBreaksToggle(state, () => ({ refresh: renderHost })),
      hideEmptyToggle(state, () => ({ refresh: renderHost })),
      el('span', { class: 'dt-spacer' }),
      el('span', { class: 'dt-count' }, `${shown.length} of ${items.length}`),
      flagFilterChip(state, 'work-items'),
    ].filter(Boolean));
    const cols = el('div', { class: 'board-cols' });
    const groups = groupForBoard(shown, axis);
    for (const g of groups) {
      cols.appendChild(el('div', { class: 'board-col' }, [
        el('div', { class: 'board-col-head' }, [
          el('span', { class: 'board-col-name', title: g.key }, g.key),
          el('span', { class: 'board-col-count' }, String(g.items.length)),
        ]),
        el('div', { class: 'board-col-cards' }, g.items.map(boardCard)),
      ]));
    }
    if (!groups.length) cols.appendChild(el('div', { class: 'empty' }, 'No matching work items.'));
    return el('div', { class: 'board-view' }, [toolbar, cols]);
  }

  // One card. Id links out to the provider item; the pencil opens the POSEIDEN editor.
  function boardCard(it) {
    const fl = flagsOf(it) || [];
    return el('div', { class: 'board-card' }, [
      el('div', { class: 'board-card-head' }, [
        linkOut('#' + it.id, it.url),
        el('span', { class: 'board-card-type' }, it.work_item_type || ''),
        el('span', { class: 'dt-spacer' }),
        el('button', {
          class: 'wi-edit', type: 'button', title: 'Edit work-item fields',
          onclick: () => openWorkItemEditor(it, () => route()),
        }, '✎'),
      ]),
      el('div', { class: 'board-card-title' }, it.title || ''),
      it.assigned_to ? el('div', { class: 'board-card-assignee' }, it.assigned_to) : null,
      (it.tags || []).length
        ? el('div', { class: 'board-card-tags' }, it.tags.map((t) => el('span', { class: 'board-chip' }, t)))
        : null,
      fl.length
        ? el('div', { class: 'board-card-flags' }, fl.map((f) => el('span', {
            class: 'flagchip ' + (f.severity === 'error' ? 'err' : 'warn'), title: f.message,
          }, flagShort(f))))
        : null,
    ].filter(Boolean));
  }
}

// Group work items into ordered board columns. State axis uses a sensible lifecycle
// order; a tag axis (`area`/`product`/…) groups by each matching tag value (an item
// with two `area:` tags shows in both columns), untagged items in a "(none)" column.
const BOARD_STATE_ORDER = [
  'new', 'proposed', 'to do', 'approved', 'open', 'active', 'committed',
  'in progress', 'doing', 'review', 'resolved', 'done', 'completed', 'closed', 'removed',
];
function groupForBoard(items, axis) {
  const map = new Map();
  const push = (k, it) => { (map.get(k) || map.set(k, []).get(k)).push(it); };
  if (axis === 'state') {
    for (const it of items) push(it.state || '(no state)', it);
    const keys = [...map.keys()].sort((a, b) => {
      const ia = BOARD_STATE_ORDER.indexOf(a.toLowerCase());
      const ib = BOARD_STATE_ORDER.indexOf(b.toLowerCase());
      return (ia === -1 ? 500 : ia) - (ib === -1 ? 500 : ib) || a.localeCompare(b);
    });
    return keys.map((k) => ({ key: k, items: map.get(k) }));
  }
  const pfx = axis + ':';
  for (const it of items) {
    const vals = (it.tags || []).filter((t) => t.toLowerCase().startsWith(pfx)).map((t) => t.slice(pfx.length));
    if (!vals.length) push('(none)', it);
    else for (const v of vals) push(v, it);
  }
  const keys = [...map.keys()].sort((a, b) =>
    (a === '(none)' ? 1 : b === '(none)' ? -1 : a.localeCompare(b)));
  return keys.map((k) => ({ key: k, items: map.get(k) }));
}

// Mirror of poseiden-rules::tag_matches - trailing-`*` prefix wildcard, else
// exact match, case-insensitive.
function tagMatches(pattern, tag) {
  const p = (pattern || '').trim().toLowerCase();
  const t = (tag || '').trim().toLowerCase();
  return p.endsWith('*') ? t.startsWith(p.slice(0, -1)) : t === p;
}

// Mirror of the server's candidate_tags: the concrete tags the AI may pick from
// for this scope. Literal required/allowed patterns are candidates as-is;
// wildcard patterns (`area:*`) are open-vocabulary slots with no literal to
// offer, so they're expanded into concrete values already in the taxonomy (each
// keyword's canonical tag, each alias's canonical target) and every tag observed
// on the backlog. Everything is filtered back through the approved patterns (so a
// stray tag outside the taxonomy is never offered) and de-duped case-insensitively.
// This is what lets the in-browser (WebGPU) model fill a wildcard-required slot
// with a real value; the server re-validates against the same expansion.
function aiCandidateTags(rules, observed) {
  const patterns = [...(rules?.required_tags || []), ...(rules?.allowed_tags || [])]
    .map((t) => (t || '').trim())
    .filter(Boolean);
  if (!patterns.length) return [];
  const pool = [
    ...patterns,
    ...(rules?.tag_keywords || []).map((k) => (k.tag || '').trim()),
    ...(rules?.tag_aliases || []).map((a) => (a.to || '').trim()),
    ...(observed || []).map((t) => (t || '').trim()),
  ].filter((t) => t && !t.includes('*'));
  const seen = new Set();
  const out = [];
  for (const t of pool) {
    if (!patterns.some((p) => tagMatches(p, t))) continue;
    const key = t.toLowerCase();
    if (!seen.has(key)) { seen.add(key); out.push(t); }
  }
  return out;
}

// Classify a tag against a ruleset (mirrors poseiden-rules: trailing-`*`
// wildcard, case-insensitive). Returns 'valid', 'invalid', or 'neutral' (no tag
// policy configured, so no colour).
function classifyTag(tag, rules) {
  const disallowed = rules?.disallowed_tags || [];
  const allowed = rules?.allowed_tags || [];
  if (!disallowed.length && !allowed.length) return 'neutral';
  const t = (tag || '').toLowerCase();
  const matches = (pattern) => {
    const p = (pattern || '').toLowerCase();
    return p.endsWith('*') ? t.startsWith(p.slice(0, -1)) : t === p;
  };
  if (disallowed.some(matches)) return 'invalid';
  if (allowed.length && !allowed.some(matches)) return 'invalid';
  return 'valid';
}

function flagShort(f) {
  // Name the tag inline so the chip is legible without hovering (the full message
  // stays in the title tooltip). missing -> the required PATTERN; bad/stale -> the tag.
  if (f.code === 'missing_required_tag') return f.tag ? `missing ${f.tag}` : 'missing tag';
  if (f.code === 'disallowed_tag') return f.tag ? `bad: ${f.tag}` : 'bad tag';
  if (f.code === 'stale_state_tag') return f.tag ? `stale: ${f.tag}` : 'stale tag';
  return { untagged: 'untagged', stale: 'stale', underspecified: 'empty body', duplicate: 'duplicate', bad_title: 'bad title', near_duplicate: 'near-dup', ai_audit: 'AI flag' }[f.code] || f.code;
}

// Work-items -> CSV: id, title, type, state, assignee, tags, suggestions (adds and
// `old->new` rewrites), flags. For exporting a filtered view (e.g. one assignee) to
// share or review the tag suggestions outside the app.
function workItemsToCsv(rows, flagsOf) {
  const esc = (v) => `"${String(v ?? '').replace(/"/g, '""')}"`;
  const sugg = (it) => (it.tag_suggestions || [])
    .map((s) => (s.replaces ? `${s.replaces}->${s.tag}` : `+${s.tag}`)).join('; ');
  const header = ['ID', 'Title', 'Type', 'State', 'Assignee', 'Tags', 'Suggested', 'Flags'].map(esc).join(',');
  const lines = rows.map((it) => [
    it.id, it.title, it.work_item_type, it.state, it.assigned_to || '',
    (it.tags || []).join('; '),
    sugg(it),
    (flagsOf(it) || []).map(flagShort).join('; '),
  ].map(esc).join(','));
  return [header, ...lines].join('\r\n');
}

// Render an EntityFlag list (pipelines / PRs) as flag chips, or an "ok" pill.
function entityFlagChips(flags) {
  return (flags && flags.length)
    ? flags.map((f) => el('span', { class: 'flagchip ' + (f.severity === 'error' ? 'err' : 'warn'), title: f.message }, f.code))
    : el('span', { class: 'pill ok' }, 'ok');
}

// Linked-PR chips on a work item, coloured by status (merged=green, active=blue,
// draft=grey, abandoned=red, unknown=dashed). Click opens the PR in ADO.
function prLinkChips(prs, team) {
  if (!prs || !prs.length) return el('span', { class: 'muted' }, '-');
  // Open the PR in ADO. A chip outside the polled window has no stored URL, so
  // resolve it live by id on first click - which also returns its real status,
  // so we can recolour the grey "unknown" chip green/red at the same time.
  const recolor = (chip, p) => {
    const kind = p.is_draft ? 'draft' : (p.status || 'unknown');
    chip.className = 'pr-chip pr-chip-' + kind;
    chip.title = `PR !${p.id} - ${p.is_draft ? 'draft' : (p.status || 'unknown')}`;
    if (p.url) chip.href = p.url;
  };
  const open = async (p, chip) => {
    if (p.url) { openExternal(p.url); return; }
    chip.classList.add('pr-chip-loading');
    try {
      const res = await api.prUrl(p.id, team);
      if (res && res.url) {
        p.url = res.url;
        if (res.status) p.status = res.status;
        if (typeof res.is_draft === 'boolean') p.is_draft = res.is_draft;
        recolor(chip, p);
        openExternal(res.url);
      } else {
        toast(`Could not resolve PR !${p.id}`, true);
      }
    } catch (err) {
      toast(`Could not resolve PR !${p.id}: ${err?.message || err}`, true);
    } finally {
      chip.classList.remove('pr-chip-loading');
    }
  };
  return prs.map((p) => {
    const kind = p.is_draft ? 'draft' : (p.status || 'unknown');
    const chip = el('a', {
      class: 'pr-chip pr-chip-' + kind, href: p.url || '#',
      title: `PR !${p.id} - ${p.is_draft ? 'draft' : (p.status || 'unknown')}`,
      onclick: (e) => { e.preventDefault(); open(p, chip); },
    }, '!' + p.id);
    return chip;
  });
}

// Tag suggestions for a work item. ADDITIONS ("+ tag") come from the keyword/AI
// suggester; REMOVALS ("- tag") come from `stale_state_tag` flags - a tag implying
// open work on a resolved item (e.g. "to refine" on a Closed story), which the
// add-only AI can't propose. Both apply through the normal explicit write-back.
// `flags` = this item's flags; `onChanged` = light refresh (re-fetches so a removed
// tag's flag clears while keeping the user's filter). Both optional.
function suggestionChips(it, afterEdit, flags, onChanged) {
  const sugg = it.tag_suggestions || [];
  // Removal suggestions come from flags that name a specific tag the item shouldn't
  // carry: a disallowed / not-allow-listed tag ("bad tag"), or a stale open-work tag
  // on a resolved item. Both remediate the same way - take the tag off - so both get a
  // one-click "- tag" chip (the add-only AI proposes neither). Deduped by tag; the
  // flag message is the tooltip so the reason (and the "or allow-list it" option) reads.
  // A tag that ALSO has a rewrite (alias) suggestion shouldn't get a plain "- remove"
  // chip too - the rewrite supersedes it (migrate, don't just strip). Dedupe those out.
  const rewritten = new Set(sugg.filter((s) => s.replaces).map((s) => s.replaces.toLowerCase()));
  const removals = [];
  const seenRemoval = new Set();
  for (const f of flags || []) {
    const removable = f.code === 'disallowed_tag' || f.code === 'stale_state_tag';
    const key = f.tag && f.tag.toLowerCase();
    if (removable && key && !seenRemoval.has(key) && !rewritten.has(key)) {
      seenRemoval.add(key);
      removals.push({ tag: f.tag, why: f.message || 'flagged tag' });
    }
  }
  if (!sugg.length && !removals.length) return el('span', { class: 'muted' }, '-');

  const addChips = sugg.map((s) => {
    // A suggestion with `replaces` is a REWRITE (alias, e.g. SSA -> area:ssa): apply
    // drops the legacy tag AND adds the canonical one in a single edit.
    const rewrite = !!s.replaces;
    return el('button', {
      class: 'suggest-chip' + (rewrite ? ' suggest-chip-rewrite' : ''), type: 'button',
      title: rewrite
        ? `replace "${s.replaces}" with "${s.tag}"`
        : 'contains ' + s.reasons.map((r) => `"${r}"`).join(', ') + ' - click to add',
      onclick: async (e) => {
        e.stopPropagation();
        const tags = [
          ...(it.tags || []).filter((t) => !rewrite || t.toLowerCase() !== s.replaces.toLowerCase()),
          s.tag,
        ];
        try {
          const res = await api.updateWorkItem(it.id, { team: it.team, tags });
          it.tag_suggestions = (it.tag_suggestions || []).filter((x) => x.tag !== s.tag);
          if (rewrite) {
            toast(`#${it.id}: ${s.replaces} → ${s.tag}`);
            if (onChanged) await onChanged(); else afterEdit(it, res); // refresh so the old tag's flag clears
          } else {
            afterEdit(it, res);
            toast(`#${it.id} tagged "${s.tag}"`);
          }
        } catch (err) { toast('Failed: ' + (err?.message || err), true); }
      },
    }, rewrite ? `${s.replaces} → ${s.tag}` : '+ ' + s.tag);
  });

  const removeChips = removals.map(({ tag, why }) => el('button', {
    class: 'suggest-chip suggest-chip-remove', type: 'button',
    title: `${why} - click to remove (or allow-list it in Rules)`,
    onclick: async (e) => {
      e.stopPropagation();
      const tags = (it.tags || []).filter((t) => t.toLowerCase() !== tag.toLowerCase());
      try {
        const res = await api.updateWorkItem(it.id, { team: it.team, tags });
        toast(`#${it.id} removed "${tag}"`);
        // Re-fetch so the now-cleared stale flag (and this chip) disappear, keeping
        // the user's filter/sort/selection. Fall back to a light in-place edit.
        if (onChanged) await onChanged(); else afterEdit(it, res);
      } catch (err) { toast('Failed: ' + (err?.message || err), true); }
    },
  }, '− ' + tag));

  return el('span', {}, [...addChips, ...removeChips]);
}

// ─────────────────────────── Work-item field editor ───────────────────────────
// A modal to edit a work item's provider fields (type-specific on Azure DevOps -
// Repro Steps, Acceptance Criteria, …; title + body on GitHub/GitLab). Fields are
// discovered live from the provider and rendered by kind; rich fields get an AI
// "Draft/Improve" button. Save writes ONLY the changed fields back (explicit,
// user-initiated). `onSaved(item)` refreshes the row.
async function openWorkItemEditor(it, onSaved) {
  const overlay = el('div', {
    class: 'dc-overlay',
    onclick: (e) => { if (e.target === overlay) close(); },
  });
  const onKey = (e) => { if (e.key === 'Escape') { e.stopPropagation(); close(); } };
  function close() { overlay.remove(); document.removeEventListener('keydown', onKey, true); }
  document.addEventListener('keydown', onKey, true);

  const body = el('div', { class: 'editor-body' }, el('span', { class: 'muted' }, 'Loading fields…'));
  const card = el('div', { class: 'dc-modal editor-modal', onclick: (e) => e.stopPropagation() }, [
    el('h3', {}, `Edit #${it.id} — ${it.title || ''}`),
    body,
  ]);
  overlay.appendChild(card);
  document.body.appendChild(overlay);

  let fields = [];
  try {
    fields = (await api.workItemFields(it.id, it.team)).fields || [];
  } catch (err) {
    clear(body).appendChild(el('div', { class: 'err' }, 'Failed to load fields: ' + (err?.message || err)));
    return;
  }
  if (!fields.length) {
    clear(body).appendChild(el('span', { class: 'muted' }, 'No editable fields for this item.'));
    return;
  }

  // The editor's current (unsaved) values for every writable field - handed to the AI
  // so a draft operates on what's ON SCREEN (a just-generated body feeds a later title
  // improve), not the last-saved provider state. Forward-referenced: the closure reads
  // `controls` at call time, after it's populated below.
  let controls = [];
  const workingFields = () => controls
    .filter((c) => !c.field.read_only)
    .map((c) => ({ reference: c.field.reference, value: c.get() }));
  controls = fields.map((f) => ({ field: f, ...buildFieldControl(f, it, workingFields) }));
  clear(body);
  const form = el('div', { class: 'editor-fields' });
  for (const c of controls) {
    form.appendChild(el('div', { class: 'editor-field' }, [
      el('div', { class: 'editor-field-head' }, [
        el('span', { class: 'editor-field-label' }, c.field.label + (c.field.required ? ' *' : '')),
        c.field.read_only ? el('span', { class: 'editor-field-tag' }, 'read-only') : null,
        c.field.help ? el('span', { class: 'editor-field-help', title: c.field.help }, 'ⓘ') : null,
      ]),
      c.node,
    ]));
  }
  // Top-level "Improve all": draft/improve every AI-eligible field individually (the
  // same per-field calls), then ONE consistency sweep over the proposed set so the
  // fields read as a coherent ticket. Every result lands in its field's review pane -
  // the user keeps or discards each, nothing is auto-applied.
  const aiControls = controls.filter((c) => c.ai);
  if (aiControls.length) {
    const allBtn = el('button', {
      class: 'btn btn-xs', type: 'button',
      title: 'Draft or improve every field, then a consistency pass — review and keep each suggestion individually',
    }, '✨ Improve all fields');
    allBtn.addEventListener('click', async () => {
      allBtn.disabled = true;
      const orig = allBtn.textContent;
      aiControls.forEach((c) => c.ai.setBusy(true));
      try {
        // Phase 1: draft/improve each field on its own (fills each review pane). Collect
        // the proposed text per field (fall back to the current value when it declined).
        const proposals = {};
        for (let i = 0; i < aiControls.length; i++) {
          const c = aiControls[i];
          allBtn.textContent = `✨ Field ${i + 1}/${aiControls.length}…`;
          try {
            const text = await c.ai.run();
            proposals[c.field.reference] = (text && text.trim()) ? text : c.get();
          } catch (err) {
            console.warn('Improve all: draft failed for', c.field.reference, err);
            proposals[c.field.reference] = c.get();
          }
        }
        // Phase 2: one consistency sweep over the proposed rich fields.
        allBtn.textContent = '✨ Harmonising…';
        const proposed = aiControls.map((c) => ({ reference: c.field.reference, value: proposals[c.field.reference] ?? c.get() }));
        let res;
        try {
          res = await api.refineFields(it.id, { team: it.team, fields: proposed });
        } catch (err) {
          toast('Consistency pass failed: ' + (err?.message || err), true);
          return;
        }
        let refined = res.fields;
        if (!refined && res.prompt) {
          // Browser (WebGPU) path: run the built prompt locally, then re-parse server-side.
          const be = await activeBackend();
          if (be.where !== 'browser') { toast('No AI model available for the consistency pass.', true); return; }
          const text = await runWebGpuChat(be.model, res.prompt.system, res.prompt.user,
            (s) => { allBtn.textContent = '✨ ' + (s || 'Harmonising…'); });
          const parsed = await api.parseFieldsConsistency(it.id, { team: it.team, fields: proposed, text });
          refined = parsed.fields;
        }
        // Replace each harmonised field's pane; fields the sweep didn't touch keep their
        // phase-1 proposal, so nothing is lost.
        const byRef = {};
        (refined || []).forEach((f) => { byRef[f.reference] = f.value; });
        let shown = 0;
        for (const c of aiControls) {
          const v = byRef[c.field.reference];
          if (v != null && v !== '') {
            c.ai.showSuggestion(v, true, '✨ Consistency pass — review, then keep or discard');
            shown++;
          }
        }
        toast(shown ? `Improve all: ${shown} field${shown === 1 ? '' : 's'} to review` : 'Improve all: nothing to change');
      } catch (err) {
        toast('Improve all failed: ' + (err?.message || err), true);
      } finally {
        aiControls.forEach((c) => c.ai.setBusy(false));
        allBtn.disabled = false;
        allBtn.textContent = orig;
      }
    });
    body.appendChild(el('div', { class: 'editor-topbar' }, [
      el('span', { class: 'editor-topbar-hint muted' }, 'Draft every field, then harmonise for consistency — review each before saving.'),
      allBtn,
    ]));
  }
  body.appendChild(form);

  const status = el('span', { class: 'editor-status' }, '');
  const saveBtn = el('button', { class: 'btn btn-primary', onclick: save }, 'Save changes');
  card.appendChild(el('div', { class: 'editor-footer' }, [
    status, el('span', { class: 'dt-spacer' }),
    el('button', { class: 'btn', onclick: close }, 'Cancel'),
    saveBtn,
  ]));

  async function save() {
    const changes = controls
      .filter((c) => !c.field.read_only && c.get() !== (c.field.value || ''))
      .map((c) => ({ reference: c.field.reference, value: c.get() }));
    if (!changes.length) { status.textContent = 'No changes to save.'; return; }
    saveBtn.disabled = true;
    status.textContent = `Saving ${changes.length} field${changes.length === 1 ? '' : 's'}…`;
    try {
      const res = await api.updateWorkItemFields(it.id, { team: it.team, changes });
      toast(`#${it.id}: ${changes.length} field${changes.length === 1 ? '' : 's'} saved`);
      close();
      if (onSaved) await onSaved(res.item);
    } catch (err) {
      saveBtn.disabled = false;
      status.textContent = 'Save failed: ' + (err?.message || err);
    }
  }
}

// Wrap the textarea's selection with `before`/`after` (e.g. ** ** for bold). With no
// selection, inserts the markers and puts the cursor between them.
function mdSurround(ta, before, after) {
  const s = ta.selectionStart, e = ta.selectionEnd;
  const sel = ta.value.slice(s, e);
  ta.value = ta.value.slice(0, s) + before + sel + after + ta.value.slice(e);
  ta.focus();
  const pos = sel ? s + before.length + sel.length + after.length : s + before.length;
  ta.selectionStart = ta.selectionEnd = pos;
}

// Prefix every line touched by the selection with `prefix` (headings, lists, quotes).
function mdLinePrefix(ta, prefix) {
  const s = ta.selectionStart, e = ta.selectionEnd, val = ta.value;
  const lineStart = val.lastIndexOf('\n', s - 1) + 1;
  const seg = val.slice(lineStart, e);
  const out = seg.split('\n').map((l) => prefix + l).join('\n');
  ta.value = val.slice(0, lineStart) + out + val.slice(e);
  ta.focus();
  ta.selectionStart = lineStart;
  ta.selectionEnd = lineStart + out.length;
}

// A markdown field: a formatting toolbar + an Edit/Preview toggle (rendered with the
// shared `renderMarkdown`) + the AI Draft/Improve button. Returns { node, get }.
function buildMarkdownField(f, it, workingFields) {
  const ta = el('textarea', { class: 'editor-textarea', rows: 9 });
  ta.value = f.value || '';
  const preview = el('div', { class: 'editor-md-preview docs-view', hidden: true });
  let previewing = false;

  const tbBtn = (label, title, fn) => el('button', {
    class: 'md-tb-btn', type: 'button', title,
    onclick: (e) => { e.preventDefault(); if (!previewing) fn(); },
  }, label);

  const previewBtn = el('button', { class: 'md-tb-btn md-tb-preview', type: 'button', title: 'Toggle preview' }, 'Preview');
  previewBtn.addEventListener('click', (e) => {
    e.preventDefault();
    previewing = !previewing;
    if (previewing) preview.innerHTML = renderMarkdown(ta.value || '') || '<span class="muted">(empty)</span>';
    ta.hidden = previewing;
    preview.hidden = !previewing;
    previewBtn.textContent = previewing ? 'Edit' : 'Preview';
    previewBtn.classList.toggle('md-tb-on', previewing);
  });

  const toolbar = el('div', { class: 'md-toolbar' }, [
    tbBtn('B', 'Bold', () => mdSurround(ta, '**', '**')),
    tbBtn('I', 'Italic', () => mdSurround(ta, '_', '_')),
    tbBtn('H', 'Heading', () => mdLinePrefix(ta, '## ')),
    tbBtn('“', 'Quote', () => mdLinePrefix(ta, '> ')),
    tbBtn('•', 'Bulleted list', () => mdLinePrefix(ta, '- ')),
    tbBtn('1.', 'Numbered list', () => mdLinePrefix(ta, '1. ')),
    tbBtn('</>', 'Inline code', () => mdSurround(ta, '`', '`')),
    tbBtn('🔗', 'Link', () => mdSurround(ta, '[', '](url)')),
    el('span', { class: 'md-tb-spacer' }),
    previewBtn,
  ]);

  // AI draft/improve, shown in a review pane before it replaces the field. Drops out
  // of preview first so the result lands in the editable box.
  const ai = buildAiAssist(f, it, () => ta.value, (t) => { ta.value = t; },
    () => { if (previewing) previewBtn.click(); }, workingFields);

  return {
    node: el('div', { class: 'editor-rich' }, [toolbar, ta, preview, ai.row, ai.result]),
    get: () => ta.value,
    set: (t) => { ta.value = t; if (previewing) previewBtn.click(); },
    ai,
  };
}

// Shared AI Draft/Improve affordance: a ✨ button + a REVIEW pane. The generated text
// is shown in the pane (the current value stays put) with Use / Discard, so nothing is
// silently overwritten - the user sees the proposal and chooses. `read()`/`apply(text)`
// access the field's value; `beforeRun` (optional) runs before generating.
function buildAiAssist(f, it, read, apply, beforeRun, workingFields) {
  const result = el('div', { class: 'editor-ai-result-slot' });
  const btn = el('button', { class: 'btn btn-xs editor-ai', type: 'button' });
  const label = () => (read().trim() ? '✨ Improve' : '✨ Draft');
  btn.textContent = label();

  // Render a proposal into the review pane with Use / Discard. Shared by the per-field
  // button and the top-level "Improve all" sweep, so both surface suggestions the same
  // way - nothing is silently overwritten; the user keeps or discards each.
  function showSuggestion(text, improve, head) {
    if (!text) return;
    clear(result);
    result.appendChild(el('div', { class: 'editor-ai-result' }, [
      el('div', { class: 'editor-ai-result-head' }, head
        || (improve ? '✨ Suggested rewrite — review, then keep or discard' : '✨ Draft — review, then keep or discard')),
      el('div', { class: 'editor-ai-result-body' }, text),
      el('div', { class: 'editor-ai-result-actions' }, [
        el('button', { class: 'btn btn-xs btn-primary', type: 'button',
          onclick: () => { if (beforeRun) beforeRun(); apply(text); clear(result); btn.textContent = label(); } }, '✓ Use this'),
        el('button', { class: 'btn btn-xs', type: 'button', onclick: () => clear(result) }, '✕ Discard'),
      ]),
    ]));
  }

  // Run one draft/improve and show it in the review pane; return the proposed text (''
  // if the model declined). One call to the server (runs it if it has a model, else
  // hands back the prompt); the shared dispatcher resolves it - in-browser on the same
  // WebGPU model the tagger uses. `fields` carries the editor's current UNSAVED values
  // so the AI drafts from what's on screen, not the saved state.
  async function run(onStatus) {
    const improve = !!read().trim();
    const fields = workingFields ? workingFields() : [];
    const res = await api.draftWorkItemField(it.id, { team: it.team, reference: f.reference, improve, fields });
    const text = await resolveAiText(res, onStatus || (() => {}));
    if (text) showSuggestion(text, improve);
    return text;
  }

  btn.addEventListener('click', async () => {
    if (beforeRun) beforeRun();
    btn.disabled = true;
    btn.textContent = '✨ Thinking…';
    try {
      await run((s) => { btn.textContent = '✨ ' + (s || 'Loading…'); });
    } catch (err) {
      toast('AI draft failed: ' + (err?.message || err), true);
    } finally {
      btn.disabled = false;
      btn.textContent = label();
    }
  });
  return { row: el('div', { class: 'editor-ai-row' }, btn), result, run, showSuggestion, setBusy: (b) => { btn.disabled = b; } };
}

// Whether a field should offer AI drafting: rich/plain text always; a single-line text
// field only when it's the Title (drafting an Area/Iteration path is nonsensical).
function fieldAllowsAi(f) {
  if (f.read_only) return false;
  if (f.kind === 'markdown' || f.kind === 'plain_text') return true;
  if (f.kind === 'text') {
    const r = (f.reference || '').toLowerCase();
    return r === 'title' || r.endsWith('.title');
  }
  return false;
}

// Build the input control for one field, keyed by its `kind`. Returns { node, get }
// where `get()` reads the current value as the string the API expects (markdown for
// rich fields). Rich fields also get an AI Draft/Improve button.
function buildFieldControl(f, it, workingFields) {
  if (f.read_only) {
    return {
      node: el('div', { class: 'editor-ro' }, f.value || el('span', { class: 'muted' }, '(empty)')),
      get: () => f.value || '',
    };
  }
  switch (f.kind) {
    case 'markdown':
      return buildMarkdownField(f, it, workingFields);
    case 'plain_text': {
      const ta = el('textarea', { class: 'editor-textarea', rows: 4 });
      ta.value = f.value || '';
      const ai = buildAiAssist(f, it, () => ta.value, (t) => { ta.value = t; }, null, workingFields);
      return { node: el('div', { class: 'editor-rich' }, [ta, ai.row, ai.result]), get: () => ta.value, set: (t) => { ta.value = t; }, ai };
    }
    case 'select': {
      const sel = el('select', { class: 'editor-input' }, [
        el('option', { value: '' }, '(none)'),
        ...(f.options || []).map((o) => el('option', { value: o }, o)),
      ]);
      sel.value = f.value || '';
      return { node: sel, get: () => sel.value };
    }
    case 'integer':
    case 'float': {
      const inp = el('input', { class: 'editor-input', type: 'number', step: f.kind === 'float' ? 'any' : '1' });
      inp.value = f.value || '';
      return { node: inp, get: () => inp.value.trim() };
    }
    case 'boolean': {
      const inp = el('input', { type: 'checkbox' });
      inp.checked = /^true$/i.test(f.value || '');
      return {
        node: el('label', { class: 'editor-check' }, [inp, el('span', {}, 'Yes')]),
        get: () => (inp.checked ? 'true' : 'false'),
      };
    }
    default: {
      // text / date_time (edited as its ISO string) / anything unmapped.
      const inp = el('input', { class: 'editor-input', type: 'text' });
      inp.value = f.value || '';
      if (fieldAllowsAi(f)) {
        // e.g. Title - offer a suggestion with the same review pane.
        const ai = buildAiAssist(f, it, () => inp.value, (t) => { inp.value = t.replace(/\s+/g, ' ').trim(); }, null, workingFields);
        return { node: el('div', { class: 'editor-rich' }, [inp, ai.row, ai.result]), get: () => inp.value, set: (t) => { inp.value = t.replace(/\s+/g, ' ').trim(); }, ai };
      }
      return { node: inp, get: () => inp.value };
    }
  }
}

// Linked-work-item chips on a PR, each a link to the item in ADO. `meta` is the
// team's config ({ organization, project }); without it the chip is a plain tag.
function wiLinkChips(ids, meta) {
  if (!ids || !ids.length) return el('span', { class: 'muted' }, '-');
  const org = (meta?.organization || '').replace(/\/$/, '');
  const project = meta?.project || '';
  return ids.map((id) => {
    const url = org && project ? `${org}/${encodeURIComponent(project)}/_workitems/edit/${id}` : '';
    return el('a', {
      class: 'tag tag-link', href: url || '#', title: `open work item #${id}`,
      onclick: (e) => { e.preventDefault(); if (url) openExternal(url); },
    }, '#' + id);
  });
}

// ── Pipelines ───────────────────────────────────────────────────────
async function renderPipelines() {
  const pipelines = await api.pipelines();
  const wrap = el('div', {});
  wrap.className = 'view-fill';
  wrap.appendChild(pageHead('Pipelines', `${pipelines.length} monitored`));

  if (!pipelines.length) {
    wrap.appendChild(el('div', { class: 'card' }, el('div', { class: 'empty' }, 'No pipelines observed yet. Configure projects and Refresh.')));
    return wrap;
  }

  let pipeTable;
  const flagCode = routeParams().get('flag');
  const state = listState('pipelines', flagCode);

  const columns = [
    { label: 'ID', sort: 'number', value: (p) => p.pipeline_id, render: (p) => linkOut('#' + p.pipeline_id, p.url) },
    {
      label: 'Pipeline', class: 'wrap',
      // Include the provider's virtual-folder path so it's sortable/filterable
      // and the name is disambiguated (two pipelines can share a name).
      value: (p) => (p.folder ? p.folder.replace(/^\\/, '') + '\\' : '') + (p.name || ''),
      render: (p) => p.folder
        ? el('div', {}, [el('div', { class: 'pipe-folder' }, p.folder), el('div', {}, p.name || '')])
        : (p.name || ''),
    },
    { label: 'Team', value: (p) => p.team || '' },
    {
      label: 'Status', filterChoices: true, value: (p) => p.last_status || '',
      render: (p) => p.last_run_at
        ? el('div', {}, [statusPill(p.last_status), el('div', { class: 'pipe-lastrun', title: p.last_run_at }, ago(p.last_run_at))])
        : statusPill(p.last_status),
    },
    { label: '✓', sort: 'number', filter: false, align: 'right', value: (p) => p.succeeded },
    { label: '✗', sort: 'number', filter: false, align: 'right', value: (p) => p.failed },
    { label: '⟳', sort: 'number', filter: false, align: 'right', value: (p) => p.running },
    {
      label: 'Last failure', filter: false, value: (p) => p.last_failure_at || '',
      render: (p) => p.last_failure_at ? el('span', { title: p.last_failure_at }, ago(p.last_failure_at)) : el('span', { class: 'muted' }, '-'),
    },
    { label: 'Flags', class: 'wrap', value: (p) => (p.flags || []).map((f) => f.code).join(' ') || 'ok', render: (p) => entityFlagChips(p.flags) },
    { label: '', sort: false, filter: false, value: () => '', render: (p) => p.last_run_url ? linkOut('logs ↗', p.last_run_url) : '' },
  ];

  pipeTable = dataTable(columns, pipelines, {
    persistKey: 'pipelines',
    fill: true,
    initialSort: { index: 0, dir: 1 },
    emptyText: 'No matching pipelines.',
    predicate: (p) => passesFlagFilter(state, (p.flags || []).map((f) => f.code)),
    toolbar: [ruleBreaksToggle(state, () => pipeTable), flagFilterChip(state, 'pipelines')].filter(Boolean),
    pageSize: getPageSize(),
    selectable: true,
    rowKey: (p) => p.pipeline_id,
    onCountClick: () => showSettings('display'),
  });
  wrap.appendChild(pipeTable);
  return wrap;
}

// ── Pull Requests ───────────────────────────────────────────────────
async function renderPulls() {
  const prs = await api.pullRequests();
  const wrap = el('div', {});
  wrap.className = 'view-fill';
  wrap.appendChild(pageHead('Pull Requests', `${prs.length} open`));

  if (!prs.length) {
    wrap.appendChild(el('div', { class: 'card' }, el('div', { class: 'empty' }, 'No open pull requests. They appear here after the next poll.')));
    return wrap;
  }

  // Show a Team column only in the "All teams" roll-up.
  const showTeam = !getTeamScope();
  // Team org/project, so a linked-work-item chip can link out to ADO. A PR's
  // linked work items share its team scope, so the team's project is correct.
  let cfg;
  try { cfg = await api.config(); } catch { cfg = null; }
  const teamMeta = (name) => (cfg?.team || []).find((t) => t.name === name) || {};
  let prTable;
  const flagCode = routeParams().get('flag');
  const state = listState('pull-requests', flagCode);
  // After a PR-side link edit, patch the row locally (the write returns the work
  // item, not the PR): update the linked ids and clear the "no work item" flag
  // once at least one link exists.
  const prAfterLink = (p, after) => {
    p.linked_work_items = [...after];
    if (after.size) p.flags = (p.flags || []).filter((f) => f.code !== 'no-work-item');
    prTable.refresh();
  };
  const columns = [
    { label: 'ID', sort: 'number', value: (p) => p.id, render: (p) => linkOut('!' + p.id, p.url) },
    showTeam ? { label: 'Team', value: (p) => p.team || '' } : null,
    { label: 'Title', class: 'wrap', value: (p) => p.title || '', render: (p) => p.title || '(untitled)' },
    {
      label: 'Work item', class: 'wrap',
      value: (p) => (p.linked_work_items || []).join(' '),
      render: (p) => wiLinkChips(p.linked_work_items, teamMeta(p.team)),
      // Editable: link/unlink work items by id. Writes the same artifact-link
      // relation (on the work item), one call per delta.
      edit: {
        type: 'chips',
        placeholder: 'add work item id…',
        get: (p) => (p.linked_work_items || []).map(String),
        commit: async (p, ids) => {
          const after = new Set(ids.map((s) => parseInt(s, 10)).filter((n) => Number.isInteger(n) && n > 0));
          const before = new Set((p.linked_work_items || []).map(Number));
          const toRemove = [...before].filter((n) => !after.has(n));
          const toAdd = [...after].filter((n) => !before.has(n));
          if (!toRemove.length && !toAdd.length) return;
          for (const wiId of toRemove) await api.linkWorkItemPr(wiId, { team: p.team, prId: p.id, link: false });
          for (const wiId of toAdd) await api.linkWorkItemPr(wiId, { team: p.team, prId: p.id, link: true });
          prAfterLink(p, after);
          toast(`!${p.id} work-item links updated`);
        },
      },
    },
    { label: 'Repo', filterChoices: true, value: (p) => p.repository || '' },
    { label: 'Status', filterChoices: true, value: (p) => (p.is_draft ? 'draft' : (p.status || '')), render: (p) => prPill(p) },
    { label: 'Author', filterChoices: true, value: (p) => p.author || '', render: (p) => p.author || el('span', { class: 'muted' }, '-') },
    { label: 'Target', filterChoices: true, value: (p) => p.target_branch || '', render: (p) => (p.target_branch ? el('span', { class: 'mono' }, p.target_branch) : el('span', { class: 'muted' }, '-')) },
    { label: 'Reviewers', sort: 'number', filter: false, align: 'right', value: (p) => p.reviewer_count },
    { label: 'Created', filter: false, value: (p) => p.created_at || '', render: (p) => (p.created_at ? el('span', { title: p.created_at }, ago(p.created_at)) : el('span', { class: 'muted' }, '-')) },
    { label: 'Flags', class: 'wrap', value: (p) => (p.flags || []).map((f) => f.code).join(' ') || 'ok', render: (p) => entityFlagChips(p.flags) },
  ].filter(Boolean);

  prTable = dataTable(columns, prs, {
    persistKey: 'pull-requests',
    fill: true,
    initialSort: { index: 0, dir: -1 }, // newest PRs first (highest id)
    emptyText: 'No matching pull requests.',
    predicate: (p) => passesFlagFilter(state, (p.flags || []).map((f) => f.code)),
    toolbar: [ruleBreaksToggle(state, () => prTable), flagFilterChip(state, 'pull-requests')].filter(Boolean),
    pageSize: getPageSize(),
    selectable: true,
    rowKey: (p) => p.id,
    onCountClick: () => showSettings('display'),
  });
  wrap.appendChild(prTable);
  return wrap;
}

function prPill(p) {
  if (p.is_draft) return el('span', { class: 'pill muted' }, 'draft');
  const map = { active: ['run', 'active'], completed: ['ok', 'completed'], abandoned: ['muted', 'abandoned'] };
  const [cls, label] = map[p.status] || ['muted', p.status || 'unknown'];
  return el('span', { class: 'pill ' + cls }, label);
}

// ── Reports ─────────────────────────────────────────────────────────
async function renderReports() {
  const wrap = el('div', {});
  wrap.appendChild(pageHead('Reports', 'Select a report to view and edit, or build a new one',
    el('button', { class: 'btn btn-primary', onclick: () => showEditor(null) }, '+ New report')));

  const layout = el('div', { class: 'reports-layout' });
  const listCol = el('div', { class: 'reports-list' });
  const editorCol = el('div', { class: 'reports-view' });
  layout.appendChild(listCol);
  layout.appendChild(editorCol);
  wrap.appendChild(layout);

  let specs = [];
  let currentName = null;

  async function refreshList() {
    try { specs = (await api.reportSpecs()) || []; } catch (e) { specs = []; clear(listCol).appendChild(errorPanel('reports', e)); return; }
    clear(listCol);
    specs.forEach((spec) => {
      const card = el('div', { class: 'report-card' + (spec.name === currentName ? ' active' : ''), onclick: () => showEditor(spec) }, [
        el('div', { class: 'report-card-name' }, spec.name),
        spec.builtin ? el('span', { class: 'pill muted report-badge' }, 'built-in') : null,
        spec.description ? el('div', { class: 'report-card-desc' }, spec.description) : null,
        spec.builtin ? null : el('div', { class: 'report-card-actions' }, [
          el('button', { class: 'btn btn-xs', onclick: (e) => { e.stopPropagation(); removeReport(spec.name); } }, 'Delete'),
        ]),
      ].filter(Boolean));
      listCol.appendChild(card);
    });
  }

  // Selecting a report opens it straight in the editor (form + live preview);
  // saved copies land selected after a save.
  function showEditor(spec) {
    currentName = spec ? spec.name : null;
    listCol.querySelectorAll('.report-card').forEach((c) =>
      c.classList.toggle('active', c.querySelector('.report-card-name').textContent === currentName));
    clear(editorCol).appendChild(reportEditorPanel(spec, {
      onSaved: async (savedName) => {
        await refreshList();
        const saved = specs.find((s) => s.name === savedName);
        if (saved) showEditor(saved);
      },
    }));
  }

  async function removeReport(name) {
    if (!confirm(`Delete report "${name}"?`)) return;
    try {
      await api.deleteReport(name);
      toast(`Deleted "${name}"`);
      if (currentName === name) { currentName = null; clear(editorCol); }
      await refreshList();
    } catch (e) { toast('Delete failed: ' + (e?.message || e), true); }
  }

  await refreshList();
  // Deep-link: a dashboard velocity tile links to #reports?report=<name>.
  const wanted = routeParams().get('report');
  const initial = (wanted && specs.find((s) => s.name === wanted)) || specs[0];
  if (initial) showEditor(initial);
  return wrap;
}

/// A small "name this report" prompt overlay. Resolves to the name, or null on
/// cancel. Used for the "save a built-in under a new name" flow.
function promptName(title, defaultValue) {
  return new Promise((resolve) => {
    const overlay = el('div', { class: 'dc-overlay', onclick: (e) => { if (e.target === overlay) { overlay.remove(); resolve(null); } } });
    const input = el('input', { class: 'inp', style: 'width:100%', value: defaultValue || '' });
    const done = (val) => { overlay.remove(); resolve(val); };
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); done(input.value.trim() || null); }
      else if (e.key === 'Escape') { e.stopPropagation(); done(null); }
    });
    overlay.appendChild(el('div', { class: 'dc-modal', style: 'text-align:left;min-width:360px' }, [
      el('h3', {}, title),
      input,
      el('div', { class: 'row', style: 'margin-top:14px;justify-content:flex-end' }, [
        el('button', { class: 'btn', onclick: () => done(null) }, 'Cancel'),
        el('button', { class: 'btn btn-primary', onclick: () => done(input.value.trim() || null) }, 'Save'),
      ]),
    ]));
    document.body.appendChild(overlay);
    setTimeout(() => input.focus(), 0);
  });
}

// Inline report editor: the form + a live preview, always editable. A Save
// control only appears once the draft differs from what's stored (or it's a new
// report). Saving a built-in template prompts for a new name (never overwrites).
function reportEditorPanel(spec, { onSaved }) {
  const isNew = !spec;
  const draft = spec ? JSON.parse(JSON.stringify(spec)) : blankSpecDraft();
  draft.time_range = draft.time_range || { kind: 'all_time' };
  const baseline = spec ? JSON.stringify(buildSpec(draft)) : null; // dirty comparison

  const preview = el('div', { class: 'report-preview' });
  let previewTimer = null;
  let lastResult = null;
  const runPreview = async () => {
    clear(preview).appendChild(el('div', { class: 'loading' }, 'Running…'));
    try {
      const result = await api.runReportSpec(buildSpec(draft));
      lastResult = result;
      clear(preview).appendChild(renderReportResult(result));
    } catch (e) { lastResult = null; clear(preview).appendChild(el('div', { class: 'empty' }, 'Preview failed: ' + (e?.message || e))); }
  };
  const schedulePreview = () => { clearTimeout(previewTimer); previewTimer = setTimeout(runPreview, 450); };

  const saveBar = el('div', { class: 'editor-savebar' });
  const isDirty = () => isNew || JSON.stringify(buildSpec(draft)) !== baseline;
  const refreshSave = () => {
    clear(saveBar);
    if (!isDirty()) return;
    saveBar.appendChild(el('span', { class: 'editor-dirty' }, isNew ? 'New report' : 'Unsaved changes'));
    saveBar.appendChild(el('button', { class: 'btn btn-primary btn-xs', onclick: doSave },
      spec && spec.builtin ? 'Save as new…' : 'Save'));
  };
  const onChange = () => { refreshSave(); schedulePreview(); };

  async function doSave() {
    let name = draft.name;
    // Built-in templates are read-only: always branch into a new named report.
    if (spec && spec.builtin) {
      name = await promptName('Save report as', (draft.name || 'report') + ' copy');
      if (!name) return;
    }
    if (!name || !name.trim()) { toast('Name is required', true); return; }
    try {
      await api.saveReport(buildSpec({ ...draft, name }));
      // Renaming an existing saved report: drop the old row.
      if (spec && !spec.builtin && spec.name !== name) { try { await api.deleteReport(spec.name); } catch { /* best effort */ } }
      toast(`Saved "${name}"`);
      await onSaved(name);
    } catch (e) { toast('Save failed: ' + (e?.message || e), true); }
  }

  // Form controls (each mutation calls onChange -> dirty + debounced preview).
  const nameInput = el('input', { class: 'inp', placeholder: 'Report name', value: draft.name, oninput: (e) => { draft.name = e.target.value; onChange(); } });

  const seriesWrap = el('div', {});
  const drawSeries = () => {
    clear(seriesWrap);
    draft.series.forEach((s) => seriesWrap.appendChild(
      seriesBlock(s, () => { draft.series = draft.series.filter((x) => x !== s); drawSeries(); onChange(); }, draft.series.length > 1, onChange)));
    seriesWrap.appendChild(el('button', { class: 'btn btn-xs', type: 'button', onclick: () => { draft.series.push(newSeries()); drawSeries(); onChange(); } }, '+ add series'));
  };
  drawSeries();

  const daysInputEl = el('input', { class: 'inp', type: 'number', min: '1', style: 'width:80px', value: draft.time_range.days || 30, oninput: (e) => { if (draft.time_range.kind === 'last_days') { draft.time_range.days = parseInt(e.target.value, 10) || 30; onChange(); } } });
  const rangeRow = el('div', { class: 'row' }, [
    selectEl([['all_time', 'All time'], ['last_days', 'Last N days']], draft.time_range.kind, (v) => {
      draft.time_range = v === 'last_days' ? { kind: 'last_days', days: parseInt(daysInputEl.value, 10) || 30 } : { kind: 'all_time' };
      daysInputEl.style.display = v === 'last_days' ? '' : 'none';
      onChange();
    }),
    daysInputEl,
  ]);
  daysInputEl.style.display = draft.time_range.kind === 'last_days' ? '' : 'none';

  const panel = el('div', { class: 'card report-editor' }, [
    el('div', { class: 'report-editor-grid' }, [
      el('div', { class: 'builder-form' }, [
        rfield('Name', spec && spec.builtin ? 'built-in templates are read-only; saving prompts for a new name' : 'shown in the report list', nameInput),
        rfield('Description', 'optional', el('input', { class: 'inp', value: draft.description || '', oninput: (e) => { draft.description = e.target.value; onChange(); } })),
        rfield('Render as', 'how to draw the result', selectEl(REPORT_RENDERS, draft.render, (v) => { draft.render = v; onChange(); })),
        rfield('Time range', 'window applied to each series', rangeRow),
        seriesWrap,
      ]),
      el('div', { class: 'builder-preview-col' }, [
        el('div', { class: 'row', style: 'justify-content:space-between;align-items:center;gap:8px' }, [
          el('h3', { style: 'margin:0' }, 'Preview'),
          reportExportBar(() => preview, () => lastResult, () => draft.name),
        ]),
        preview,
      ]),
    ]),
    saveBar,
  ]);
  refreshSave();
  runPreview();
  return panel;
}

const fmtValue = (v, percent) => percent ? `${Math.round(v * 100)}%` : (Number.isInteger(v) ? String(v) : v.toFixed(2));

// ── Report export: PDF (native print), PNG (rasterise the SVG), CSV (raw data) ──
function downloadBlob(blob, filename) {
  const url = URL.createObjectURL(blob);
  const a = el('a', { href: url, download: filename });
  document.body.appendChild(a); a.click(); a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

// Print just `node` (theme + vector intact) by isolating it with `@media print`, rather
// than a new window - which would drop the app's CSS variables (the chart colours).
function printNode(node) {
  if (!node) return;
  node.classList.add('print-target');
  document.body.classList.add('printing');
  const cleanup = () => {
    document.body.classList.remove('printing');
    node.classList.remove('print-target');
    window.removeEventListener('afterprint', cleanup);
  };
  window.addEventListener('afterprint', cleanup);
  window.print();
  setTimeout(cleanup, 1500); // fallback where afterprint doesn't fire
}

// Rasterise an inline chart SVG to a PNG at `scale`x. CSS-variable colours won't resolve
// once the SVG is a standalone image, so inline the COMPUTED fill/stroke/font first.
async function svgToPng(svg, scale = 2) {
  const clone = svg.cloneNode(true);
  const src = svg.querySelectorAll('*');
  const dst = clone.querySelectorAll('*');
  src.forEach((s, i) => {
    const cs = getComputedStyle(s); const d = dst[i]; if (!d) return;
    ['fill', 'stroke', 'color'].forEach((p) => {
      const v = cs.getPropertyValue(p);
      if (v && v !== 'none' && !v.startsWith('var(')) d.style[p] = v;
    });
    const fs = cs.fontSize; if (fs) d.style.fontSize = fs;
    const ff = cs.fontFamily; if (ff) d.style.fontFamily = ff;
  });
  const rect = svg.getBoundingClientRect();
  const w = Math.max(1, rect.width || (svg.viewBox.baseVal && svg.viewBox.baseVal.width) || 600);
  const h = Math.max(1, rect.height || (svg.viewBox.baseVal && svg.viewBox.baseVal.height) || 400);
  clone.setAttribute('width', w); clone.setAttribute('height', h);
  const xml = new XMLSerializer().serializeToString(clone);
  const img = new Image();
  await new Promise((res, rej) => {
    img.onload = res; img.onerror = rej;
    img.src = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(xml);
  });
  const canvas = el('canvas', { width: Math.round(w * scale), height: Math.round(h * scale) });
  const ctx = canvas.getContext('2d');
  const bg = getComputedStyle(document.body).getPropertyValue('--panel').trim() || '#ffffff';
  ctx.fillStyle = bg; ctx.fillRect(0, 0, canvas.width, canvas.height);
  ctx.setTransform(scale, 0, 0, scale, 0, 0);
  ctx.drawImage(img, 0, 0);
  return new Promise((res) => canvas.toBlob(res, 'image/png'));
}

function seriesToCsv(result) {
  const series = result.series || [];
  const labels = [];
  series.forEach((s) => s.points.forEach((p) => { if (!labels.includes(p.label)) labels.push(p.label); }));
  const esc = (v) => `"${String(v ?? '').replace(/"/g, '""')}"`;
  const header = ['', ...series.map((s) => s.label)].map(esc).join(',');
  const rows = labels.map((lab) => [
    lab || '(total)',
    ...series.map((s) => { const pt = s.points.find((p) => p.label === lab); return pt ? pt.value : ''; }),
  ].map(esc).join(','));
  return [header, ...rows].join('\r\n');
}

// Export toolbar for a report result. Reads the live preview node + result at click time.
function reportExportBar(getNode, getResult, getName) {
  const safe = () => (getName() || 'report').replace(/[^a-z0-9._-]+/gi, '-').replace(/^-+|-+$/g, '') || 'report';
  return el('div', { class: 'report-export' }, [
    el('button', { class: 'btn btn-xs', title: 'Print / Save as PDF', onclick: () => printNode(getNode()) }, '⎙ PDF'),
    el('button', { class: 'btn btn-xs', title: 'Download the chart as a PNG (chart renders only)', onclick: async () => {
      const svg = getNode() && getNode().querySelector('svg');
      if (!svg) { toast('PNG is for chart renders - use CSV for table/stat.'); return; }
      try { downloadBlob(await svgToPng(svg), safe() + '.png'); }
      catch (e) { toast('PNG failed: ' + (e?.message || e), true); }
    } }, '⭳ PNG'),
    el('button', { class: 'btn btn-xs', title: 'Download the underlying data as CSV', onclick: () => {
      const r = getResult(); if (!r) { toast('Run the report first.'); return; }
      downloadBlob(new Blob([seriesToCsv(r)], { type: 'text/csv;charset=utf-8' }), safe() + '.csv');
    } }, '⭳ CSV'),
  ]);
}

function renderReportResult(result) {
  const series = result.series || [];
  if (!series.length || series.every((s) => !s.points.length)) {
    return el('div', { class: 'empty' }, 'No data for this report.');
  }
  switch (result.render) {
    case 'stat': {
      // One tile per series (grouped stats show their first point).
      return el('div', { class: 'grid', style: 'grid-template-columns: repeat(auto-fit, minmax(150px, 1fr))' },
        series.map((s) => {
          const v = s.points[0] ? s.points[0].value : 0;
          return statTile(fmtValue(v, s.percent), s.label);
        }));
    }
    case 'pie':
      return pieChart(series[0].points);
    case 'line':
      return lineChart(series, { percent: series.some((s) => s.percent) });
    case 'table':
      return reportTable(series);
    case 'plaintext':
      return el('pre', { class: 'report-plaintext' },
        series.map((s) => `${s.label}\n` + s.points.map((p) => `  ${p.label || '(total)'}: ${fmtValue(p.value, s.percent)}`).join('\n')).join('\n\n'));
    case 'bar':
    default:
      // One bar chart per series (labelled when there is more than one).
      return el('div', {}, series.map((s) => el('div', { style: 'margin-bottom:14px' }, [
        series.length > 1 ? el('div', { class: 'report-series-label' }, s.label) : null,
        barChart(s.points.map((p) => ({ label: p.label || '(total)', value: p.value }))),
      ].filter(Boolean))));
  }
}

function reportTable(series) {
  // Union of point labels across series -> rows; one value column per series.
  const labels = [];
  series.forEach((s) => s.points.forEach((p) => { if (!labels.includes(p.label)) labels.push(p.label); }));
  const head = el('tr', {}, [el('th', {}, ''), ...series.map((s) => el('th', { style: 'text-align:right' }, s.label))]);
  const rows = labels.map((lab) => el('tr', {}, [
    el('td', {}, lab || '(total)'),
    ...series.map((s) => {
      const pt = s.points.find((p) => p.label === lab);
      return el('td', { style: 'text-align:right;font-weight:600' }, pt ? fmtValue(pt.value, s.percent) : '-');
    }),
  ]));
  return el('div', { class: 'table-wrap' }, el('table', {}, [el('thead', {}, head), el('tbody', {}, rows)]));
}

// ── Report builder ──────────────────────────────────────────────────
// Labelled form field (reuses the rules editor's field styling).
function rfield(label, hint, editor) {
  return el('div', { class: 'rule-field' }, [
    el('div', { class: 'rule-field-label' }, [el('span', {}, label), hint ? el('span', { class: 'rule-field-hint' }, hint) : null]),
    editor,
  ]);
}
const REPORT_SOURCES = [
  ['work_items', 'Work items'], ['pull_requests', 'Pull requests'],
  ['pipelines', 'Pipelines'], ['pipeline_runs', 'Pipeline runs'],
];
const REPORT_GROUPBYS = [
  ['', 'None (single total)'], ['tag', 'Tag'], ['state', 'State'], ['status', 'Status'],
  ['team', 'Team'], ['work_item_type', 'Work item type'], ['day', 'Day'], ['week', 'Week'],
];
const REPORT_RENDERS = [
  ['stat', 'Stat'], ['bar', 'Bar'], ['pie', 'Pie'], ['line', 'Line'], ['table', 'Table'], ['plaintext', 'Plain text'],
];
const REPORT_OPS = [['eq', '='], ['ne', '≠'], ['in', 'in'], ['contains', 'contains']];

function newSeries() {
  return { label: null, source: 'work_items', metric: { kind: 'count' }, group_by: 'tag', filters: [], time_field: null };
}
function blankSpecDraft() {
  return { name: '', description: '', render: 'bar', time_range: { kind: 'all_time' }, series: [newSeries()] };
}

function selectEl(options, value, onChange) {
  const sel = el('select', { class: 'inp', onchange: (e) => onChange(e.target.value) });
  options.forEach(([v, label]) => {
    const o = el('option', { value: v }, label);
    if (v === (value ?? '')) o.selected = true;
    sel.appendChild(o);
  });
  return sel;
}

// Editable list of {field, op, value} conditions bound to `list`. `onChange`
// fires on every mutation so the editor can mark itself dirty + re-preview.
function conditionEditor(list, onChange = () => {}) {
  const box = el('div', { class: 'cond-list' });
  const draw = () => {
    clear(box);
    list.forEach((c, i) => box.appendChild(el('div', { class: 'cond-row' }, [
      el('input', { class: 'inp', placeholder: 'field (e.g. status)', value: c.field, oninput: (e) => { c.field = e.target.value; onChange(); } }),
      selectEl(REPORT_OPS, c.op, (v) => { c.op = v; onChange(); }),
      el('input', { class: 'inp', placeholder: 'value', value: c.value, oninput: (e) => { c.value = e.target.value; onChange(); } }),
      el('button', { class: 'btn btn-xs', type: 'button', onclick: () => { list.splice(i, 1); draw(); onChange(); } }, '×'),
    ])));
    box.appendChild(el('button', { class: 'btn btn-xs', type: 'button', onclick: () => { list.push({ field: '', op: 'eq', value: '' }); draw(); onChange(); } }, '+ condition'));
  };
  draw();
  return box;
}

function seriesBlock(s, onRemove, canRemove, onChange = () => {}) {
  // Backend omits empty/optional fields (filters, metric) via skip_serializing;
  // normalise so the editors always get real arrays/objects to bind to.
  s.filters = s.filters || [];
  s.metric = s.metric || { kind: 'count' };
  const body = el('div', { class: 'series-body' });
  const metricExtra = el('div', {});
  const drawMetricExtra = () => {
    clear(metricExtra);
    if (s.metric.kind === 'ratio') {
      s.metric.numerator = s.metric.numerator || [];
      s.metric.denominator = s.metric.denominator || [];
      metricExtra.appendChild(rfield('Numerator', 'rows counted on top', conditionEditor(s.metric.numerator, onChange)));
      metricExtra.appendChild(rfield('Denominator', 'rows counted on the bottom', conditionEditor(s.metric.denominator, onChange)));
    }
  };
  drawMetricExtra();
  body.append(
    rfield('Source', 'which entity to query', selectEl(REPORT_SOURCES, s.source, (v) => { s.source = v; onChange(); })),
    rfield('Metric', 'count rows, or a ratio of two subsets', selectEl([['count', 'Count'], ['ratio', 'Ratio (rate)']], s.metric.kind, (v) => {
      s.metric = v === 'ratio' ? { kind: 'ratio', numerator: [], denominator: [] } : { kind: 'count' };
      drawMetricExtra();
      onChange();
    })),
    metricExtra,
    rfield('Group by', 'bucket the results', selectEl(REPORT_GROUPBYS, s.group_by || '', (v) => { s.group_by = v || null; onChange(); })),
    rfield('Time field', 'timestamp the window applies to (blank = default)', el('input', { class: 'inp', placeholder: 'created / closed / finished', value: s.time_field || '', oninput: (e) => { s.time_field = e.target.value || null; onChange(); } })),
    rfield('Filters', 'only rows matching all of these', conditionEditor(s.filters, onChange)),
  );
  const head = el('div', { class: 'series-head' }, [
    el('span', {}, 'Series'),
    canRemove ? el('button', { class: 'btn btn-xs', type: 'button', onclick: onRemove }, 'Remove') : null,
  ].filter(Boolean));
  return el('div', { class: 'series-card' }, [head, body]);
}

// Draft (UI shape) -> ReportSpec (API shape). Omits None-y fields so serde reads
// them as absent rather than null where that matters.
function buildSpec(draft) {
  return {
    name: (draft.name || '').trim(),
    description: draft.description ? draft.description : null,
    builtin: false,
    team: null,
    time_range: draft.time_range,
    series: draft.series.map((s) => ({
      label: s.label || null,
      source: s.source,
      metric: s.metric.kind === 'ratio'
        ? { kind: 'ratio', numerator: s.metric.numerator || [], denominator: s.metric.denominator || [] }
        : { kind: 'count' },
      ...(s.group_by ? { group_by: s.group_by } : {}),
      filters: (s.filters || []).filter((c) => c.field && c.value),
      ...(s.time_field ? { time_field: s.time_field } : {}),
    })),
    render: draft.render,
  };
}

// ── Settings ────────────────────────────────────────────────────────
// ── Recap (a shareable highlights deck, generated from closed work) ──
// POSEIDEN builds the data-driven SKELETON - what closed, grouped by area:/
// source:, internal vs external - and renders it as slides. The human finishes
// the narrative + screenshots (that context isn't in the backlog). Merged in
// from the OCTOGON slide tool; the renderer lives in lib/recap-slides.js.
async function renderRecap() {
  const wrap = el('div', {});
  wrap.appendChild(pageHead('Recap',
    'A shareable highlights deck generated from your closed work. Pick a window, then finish the narrative yourself.'));

  const periodSel = el('select', { class: 'inp', style: 'width:auto' }, [
    el('option', { value: '30' }, 'Last 30 days'),
    el('option', { value: '60' }, 'Last 60 days'),
    el('option', { value: '90' }, 'Last 90 days'),
  ]);
  const deckHost = el('div', { class: 'recap-host' });
  let lastDeck = null;
  const dlBtn = el('button', {
    class: 'btn', disabled: true,
    title: 'Download this deck as a single self-contained HTML file - opens and presents in any browser, no POSEIDEN needed',
    onclick: () => { if (lastDeck) downloadRecapHtml(lastDeck); },
  }, '⬇ Download deck');

  async function build() {
    lastDeck = null; dlBtn.disabled = true;
    clear(deckHost).appendChild(el('div', { class: 'loading' }, 'Generating deck…'));
    let items = [];
    try { ({ items } = await api.tickets()); }
    catch (e) { clear(deckHost).appendChild(el('div', { class: 'empty' }, 'Could not load work items: ' + (e?.message || e))); return; }
    const deck = buildRecapDeck(items, parseInt(periodSel.value, 10));
    clear(deckHost);
    if (!deck.slides.length) {
      deckHost.appendChild(el('div', { class: 'empty' }, 'No closed work items in this window to recap yet.'));
      return;
    }
    lastDeck = deck; dlBtn.disabled = false;
    renderDeck(deck, deckHost);
  }
  periodSel.onchange = build;
  wrap.appendChild(el('div', { class: 'row', style: 'gap:8px;align-items:center;margin-bottom:10px' }, [
    el('span', { class: 'muted' }, 'Window:'), periodSel,
    el('button', { class: 'btn', onclick: build }, '↻ Regenerate'),
    dlBtn,
  ]));
  wrap.appendChild(deckHost);
  build();
  return wrap;
}

// Turn work items into an OCTOGON-format deck object: title -> at-a-glance
// metrics -> one feature slide per top area:* bucket -> a by-source breakdown ->
// a "finish this" checklist. Closed = a resolved-family state changed within the
// window (a frontend approximation of the ruleset's resolved_states).
function buildRecapDeck(items, days) {
  const now = Date.now();
  const cutoff = now - days * 86400000;
  const RESOLVED = new Set(['closed', 'done', 'resolved', 'completed', 'removed']);
  const closed = (items || []).filter((it) => {
    const st = (it.state || '').toLowerCase();
    const when = Date.parse(it.changed_at || it.closed_at || it.created_at || '');
    return RESOLVED.has(st) && !Number.isNaN(when) && when >= cutoff;
  });
  if (!closed.length) return { title: 'Recap', slides: [] };

  const byArea = new Map(), bySource = new Map();
  let internal = 0, external = 0;
  const addTo = (map, key, it) => { if (!map.has(key)) map.set(key, []); map.get(key).push(it); };
  for (const it of closed) {
    const tags = (it.tags || []).map((t) => t.toLowerCase());
    for (const t of tags) {
      if (t.startsWith('area:')) addTo(byArea, t, it);
      else if (t.startsWith('source:')) addTo(bySource, t, it);
    }
    if (tags.includes('internal')) internal++;
    if (tags.includes('external')) external++;
  }

  const periodLabel = new Date(now).toLocaleDateString('en-GB', { month: 'long', year: 'numeric' });
  const slides = [];
  slides.push({
    type: 'title', eyebrow: 'Recap', title: periodLabel,
    subtitle: `Highlights from the last ${days} days`,
    meta: [new Date(now).toLocaleDateString('en-GB'), `${closed.length} item${closed.length === 1 ? '' : 's'} closed`],
  });
  slides.push({
    type: 'metrics', title: 'At a glance', metrics: [
      { value: closed.length, label: 'Closed' },
      { value: byArea.size, label: 'Areas touched' },
      { value: internal, label: 'Internal', color: 'green' },
      { value: external, label: 'External', color: 'accent' },
    ],
  });
  const topAreas = [...byArea.entries()].sort((a, b) => b[1].length - a[1].length).slice(0, 6);
  for (const [area, its] of topAreas) {
    slides.push({
      type: 'feature', label: 'Area', title: area,
      description: `${its.length} item${its.length === 1 ? '' : 's'} closed. Add the story: what shipped, why it mattered.`,
      highlights: its.slice(0, 6).map((it) => `#${it.id} ${it.title || ''}`.trim()),
    });
  }
  if (bySource.size) {
    slides.push({
      type: 'bullets', title: 'By source',
      items: [...bySource.entries()].sort((a, b) => b[1].length - a[1].length).map(([s, its]) => `${s} - ${its.length}`),
    });
  }
  slides.push({
    type: 'bullets', title: 'Before you present',
    items: [
      'Replace these auto-generated highlights with the real narrative.',
      'Add screenshots for the marquee items.',
      'Trim to a tight 10-minute story.',
    ],
  });
  return { title: `${periodLabel} Recap`, slides };
}

// Export the deck as ONE self-contained HTML file - the deck data, the slide
// renderer, and the styles all inlined - so it opens and presents in any browser
// with no POSEIDEN and no network. This is the shareable artifact: hand it to
// management, attach it to an email, drop it in Teams.
async function downloadRecapHtml(deck) {
  let js, css;
  try {
    [js, css] = await Promise.all([
      fetch(new URL('./lib/recap-slides.js', import.meta.url)).then((r) => r.text()),
      fetch(new URL('./styles.css', import.meta.url)).then((r) => r.text()),
    ]);
  } catch (e) { toast('Could not build the download: ' + (e?.message || e), true); return; }
  // Stop any stray closing-script tag in the fetched module from breaking out of
  // the inline <script>; escape `<` in the JSON so a title can't either.
  js = js.replace(/<\/script>/gi, '<\\/script>');
  const deckLiteral = JSON.stringify(deck).replace(/</g, '\\u003c');
  const esc = (s) => String(s || '').replace(/[<&>]/g, (c) => ({ '<': '&lt;', '>': '&gt;', '&': '&amp;' }[c]));
  const html = `<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(deck.title || 'Recap')}</title>
<style>
${css}
/* Standalone reset: the inlined app stylesheet lays the body out as an app
   shell (sidebar column + main). This file has ONLY the deck, so a lone child
   would land in that narrow sidebar column (the deck looked compressed). Force
   a plain full-viewport block layout. */
html, body { margin: 0 !important; height: 100% !important; }
body { display: block !important; background: var(--bg, #0b0f14) !important; }
#recap-deck { width: 100vw !important; height: 100vh !important; max-width: none !important; }
</style></head>
<body><div id="recap-deck"></div>
<script type="module">
${js}
renderDeck(${deckLiteral}, document.getElementById('recap-deck'));
</scr${''}ipt>
</body></html>`;
  const stamp = new Date().toISOString().slice(0, 10);
  downloadBlob(new Blob([html], { type: 'text/html;charset=utf-8' }), `poseiden-recap-${stamp}.html`);
}

// ── Rules (team-scoped hygiene policy) ──────────────────────────────
// The sidebar's team-scoped policy view. Every other sidebar screen follows the
// team selector; so does this one. It shows the EFFECTIVE ruleset for the
// selected team - its own `[team.rules]` override, or the inherited instance
// default - and says which. Global instance config (Connection, poll interval,
// team roster) lives in File > Settings, not here. Rules are edited on this
// screen and persisted per team to the DB.
async function renderRules() {
  const wrap = el('div', {});
  let cfg;
  try { cfg = await api.config(); } catch { cfg = null; }
  if (!cfg) {
    wrap.appendChild(pageHead('Rules', 'Hygiene policy'));
    wrap.appendChild(el('div', { class: 'card' }, el('div', { class: 'empty' }, 'Configuration unavailable - connect to an instance first.')));
    return wrap;
  }

  const teams = cfg.team || [];
  const scope = getTeamScope();
  const team = teams.find((t) => t.name === scope);

  if (team) {
    const overridden = !!team.rules;
    const rules = overridden ? team.rules : (cfg.rules || {});
    wrap.appendChild(pageHead('Rules', `Hygiene policy for ${team.name}`));
    wrap.appendChild(rulesEditorCard(rules, {
      badge: overridden ? { tone: 'warn', text: 'Team override' } : { tone: 'muted', text: 'Inherited' },
      note: overridden
        ? `${team.name} defines its own rules. Saving updates this override.`
        : `${team.name} inherits the instance defaults. Saving here creates a team-specific override.`,
      onSave: (rs) => api.updateTeamRules(team.name, rs),
      onRevert: overridden ? () => api.clearTeamRules(team.name) : null,
    }));
  } else {
    // "All teams" scope: edit the instance default + show who inherits/overrides.
    wrap.appendChild(pageHead('Rules', 'Default hygiene policy (work items, pipelines, pull requests)'));
    wrap.appendChild(rulesEditorCard(cfg.rules || {}, {
      badge: { tone: 'muted', text: 'Instance default' },
      note: 'Applied to every team that does not define its own. Pick a team above to edit its override.',
      onSave: (rs) => api.updateRules(rs),
      onRevert: null,
    }));
    if (teams.length) wrap.appendChild(ruleInheritanceCard(teams));
  }
  return wrap;
}

// An editable ruleset card. `opts` = { badge, note, onSave(ruleset), onRevert? }.
// Edits mutate a working draft; Save persists through the config store (live -
// no restart). The draft shape matches the RuleSet the backend expects.
function rulesEditorCard(rules, opts) {
  const r = rules || {};
  const draft = {
    required_tags: [...(r.required_tags || [])],
    allowed_tags: [...(r.allowed_tags || [])],
    disallowed_tags: [...(r.disallowed_tags || [])],
    tag_keywords: (r.tag_keywords || []).map((tk) => ({ tag: tk.tag, keywords: [...(tk.keywords || [])] })),
    untagged_is_error: !!r.untagged_is_error,
    stale_days: { ...(r.stale_days || {}) },
    ignore_states: [...(r.ignore_states || [])],
    ignore_types: [...(r.ignore_types || [])],
    resolved_states: [...(r.resolved_states || [])],
    stale_when_resolved_tags: [...(r.stale_when_resolved_tags || [])],
    // Carried through so a UI save doesn't drop them (no dedicated editor for the
    // first two; team_background gets the textarea below).
    refine_tag: r.refine_tag ?? null,
    refine_min_chars: r.refine_min_chars ?? null,
    moved_in_source: r.moved_in_source ?? null,
    team_background: r.team_background || '',
    pipelines: {
      flag_failing: !!(r.pipelines && r.pipelines.flag_failing),
      flag_never_run: !!(r.pipelines && r.pipelines.flag_never_run),
    },
    pull_requests: {
      stale_open_days: r.pull_requests?.stale_open_days ?? null,
      stale_draft_days: r.pull_requests?.stale_draft_days ?? null,
      require_work_item: !!(r.pull_requests && r.pull_requests.require_work_item),
      link_include_abandoned: !!(r.pull_requests && r.pull_requests.link_include_abandoned),
    },
  };

  const card = el('div', { class: 'card' }, [
    el('div', { class: 'row', style: 'justify-content:space-between;align-items:center' }, [
      el('h2', { style: 'margin:0' }, 'Rules'),
      el('span', { class: 'rule-badge ' + (opts.badge.tone || 'muted') }, opts.badge.text),
    ]),
  ]);
  if (opts.note) card.appendChild(el('p', { class: 'muted', style: 'margin:8px 0 14px' }, opts.note));

  const field = (label, hint, editor) => el('div', { class: 'rule-field' }, [
    el('div', { class: 'rule-field-label' }, [el('span', {}, label), hint ? el('span', { class: 'rule-field-hint' }, hint) : null]),
    editor,
  ]);

  // Work Items panel.
  const wiPanel = el('div', { class: 'rule-panel' }, [
    field('Allowed tags', 'sanctioned tags; empty = allow any. Trailing * is a prefix wildcard', chipListEditor(draft.allowed_tags, { recommended: RECOMMENDED_ALLOWED_TAGS })),
    field('Disallowed tags', 'always flagged, even if allowed is empty', chipListEditor(draft.disallowed_tags)),
    field('Required tags', 'every item must carry a tag matching each pattern', chipListEditor(draft.required_tags)),
    field('Untagged item', 'severity when an item has no tags at all', untaggedToggle(draft)),
    field('Auto-suggest keywords', 'suggest a tag when an item title contains a keyword (advisory - never applied automatically)', tagKeywordsEditor(draft.tag_keywords)),
    field('Team background (AI)', 'context / glossary fed verbatim to the AI tagger so it understands your internal naming - never applied as a tag', teamBackgroundEditor(draft)),
    field('Stale limits', 'days an item may sit in a state before it is flagged stale', staleEditor(draft.stale_days)),
    field('Resolved states', 'states that count as "done" - a "still needs work" tag here is flagged (runs even on ignored states)', chipListEditor(draft.resolved_states)),
    field('Stale-when-resolved tags', 'tags meaning outstanding work; flagged on a resolved item, e.g. "to refine" on a Closed story', chipListEditor(draft.stale_when_resolved_tags)),
    field('Ignored states', 'states exempt from all hygiene checks', chipListEditor(draft.ignore_states)),
    field('Ignored types', 'work-item types exempt from all hygiene checks', chipListEditor(draft.ignore_types)),
  ]);

  // Pipelines panel.
  const plPanel = el('div', { class: 'rule-panel' }, [
    field('Failing pipeline', 'flag a pipeline whose most recent run failed', flagToggle(draft.pipelines, 'flag_failing', 'flag failing')),
    field('Never run', 'flag a pipeline that has never executed', flagToggle(draft.pipelines, 'flag_never_run', 'flag never-run')),
  ]);

  // Pull Requests panel.
  const prPanel = el('div', { class: 'rule-panel' }, [
    field('Stale open PR', 'flag an active PR open longer than N days (blank/0 = off)', daysInput(draft.pull_requests, 'stale_open_days')),
    field('Stale draft', 'flag a draft PR open longer than N days (blank/0 = off)', daysInput(draft.pull_requests, 'stale_draft_days')),
    field('Require work item', 'flag an active PR with no linked work item', flagToggle(draft.pull_requests, 'require_work_item', 'flag PRs without a work item')),
    field('Show abandoned links', "include abandoned PRs in a work item's PR chips", flagToggle(draft.pull_requests, 'link_include_abandoned', 'show abandoned')),
  ]);

  // Tabs.
  const tabDefs = [['wi', 'Work Items', wiPanel], ['pl', 'Pipelines', plPanel], ['pr', 'Pull Requests', prPanel]];
  const tabBar = el('div', { class: 'rule-tabs' });
  const show = (id) => {
    tabDefs.forEach(([tid, , panel]) => { panel.style.display = tid === id ? '' : 'none'; });
    tabBar.querySelectorAll('.rule-tab').forEach((b) => b.classList.toggle('active', b.dataset.tab === id));
  };
  tabDefs.forEach(([id, label]) => tabBar.appendChild(
    el('button', { class: 'rule-tab', type: 'button', 'data-tab': id, onclick: () => show(id) }, label)));
  card.appendChild(tabBar);
  card.appendChild(el('div', {}, [wiPanel, plPanel, prPanel]));
  show('wi');

  const saveBtn = el('button', { class: 'btn btn-primary' }, 'Save rules');
  saveBtn.addEventListener('click', async () => {
    saveBtn.disabled = true; saveBtn.textContent = 'Saving…';
    // Drop incomplete keyword rows (no tag, or no non-empty keyword).
    draft.tag_keywords = (draft.tag_keywords || []).filter((t) => (t.tag || '').trim() && (t.keywords || []).some((k) => (k || '').trim()));
    try { await opts.onSave(draft); toast('Rules saved'); route(); }
    catch (e) { toast('Save failed: ' + (e.message || e), true); saveBtn.disabled = false; saveBtn.textContent = 'Save rules'; }
  });
  const controls = el('div', { class: 'row', style: 'margin-top:18px; gap:10px' }, [saveBtn]);
  if (opts.onRevert) {
    const revertBtn = el('button', { class: 'btn' }, 'Revert to defaults');
    revertBtn.addEventListener('click', async () => {
      try { await opts.onRevert(); toast('Reverted to instance defaults'); route(); }
      catch (e) { toast('Revert failed: ' + (e.message || e), true); }
    });
    controls.appendChild(revertBtn);
  }
  card.appendChild(controls);
  return card;
}

// POSEIDEN's recommended sanctioned-tag set (GitHub's canonical labels + our
// additions). Not locked - just a one-click starting point users can restore.
const RECOMMENDED_ALLOWED_TAGS = [
  'bug', 'documentation', 'duplicate', 'enhancement',
  'good first issue', 'help wanted', 'invalid', 'question', 'wontfix',
  'external', 'internal', 'blocked', 'stalled',
  'to refine', 'to review', 'technical debt',
];

// Editor for tag_keywords: rows of [tag][keyword chips]. Mutates `list` (an
// array of { tag, keywords }) in place. Keywords drive tag auto-suggestions.
function tagKeywordsEditor(list) {
  const box = el('div', { class: 'tagkw-box' });
  const draw = () => {
    clear(box);
    list.forEach((entry, i) => {
      entry.keywords = entry.keywords || [];
      box.appendChild(el('div', { class: 'tagkw-row' }, [
        el('input', { class: 'inp tagkw-tag', placeholder: 'tag', value: entry.tag || '', oninput: (e) => { entry.tag = e.target.value; } }),
        el('span', { class: 'tagkw-arrow' }, '←'),
        chipListEditor(entry.keywords),
        el('button', { class: 'btn btn-xs', type: 'button', title: 'remove', onclick: () => { list.splice(i, 1); draw(); } }, '×'),
      ]));
    });
    box.appendChild(el('button', { class: 'btn btn-xs', type: 'button', onclick: () => { list.push({ tag: '', keywords: [] }); draw(); } }, '+ tag + keywords'));
  };
  draw();
  return box;
}

// Free-text team background / AI glossary editor. Bound to draft.team_background.
function teamBackgroundEditor(draft) {
  const ta = el('textarea', {
    class: 'rule-textarea', rows: '10', 'aria-label': 'Team background',
    placeholder: 'Context fed verbatim to the AI tagger, e.g.\n- Our core billing service and its satellites are all product:platform.\n- The internal developer portal = product:idp.\n- Crossplane / Terraform / Argo = product:dev-platform (the platform tooling itself).\n- The "deployment" repos are IaC that provisions customer environments = area:platform-deployment.',
    oninput: (e) => { draft.team_background = e.target.value; },
  });
  ta.value = draft.team_background || '';
  return ta;
}

// A tag/string list editor: removable chips + an add-on-Enter input. Mutates the
// passed array in place. `opts.recommended` adds a "restore recommended" link.
function chipListEditor(values, opts = {}) {
  const box = el('div', { class: 'rule-chip-box' });
  const input = el('input', { class: 'rule-chip-input', type: 'text', placeholder: 'add + Enter' });
  const draw = () => {
    clear(box);
    values.forEach((v, i) => box.appendChild(el('span', { class: 'tag rule-chip' }, [
      v, el('button', { type: 'button', class: 'rule-chip-x', onclick: () => { values.splice(i, 1); draw(); } }, '×'),
    ])));
    box.appendChild(input);
    if (opts.recommended) {
      box.appendChild(el('a', {
        class: 'rule-reset', href: '#',
        onclick: (e) => { e.preventDefault(); values.length = 0; opts.recommended.forEach((v) => values.push(v)); draw(); },
      }, 'restore recommended'));
    }
  };
  input.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter') return;
    e.preventDefault();
    const v = input.value.trim();
    if (v && !values.some((x) => x.toLowerCase() === v.toLowerCase())) values.push(v);
    input.value = '';
    draw();
    input.focus();
  });
  draw();
  return box;
}

// A boolean toggle switch bound to obj[key]. Mutates in place.
function flagToggle(obj, key, label) {
  const btn = el('button', {
    class: 'toggle-btn' + (obj[key] ? ' on' : ''), type: 'button', 'aria-pressed': String(!!obj[key]),
  }, [el('span', { class: 'toggle-switch' }, el('span', { class: 'toggle-knob' })), el('span', {}, label)]);
  btn.addEventListener('click', () => {
    obj[key] = !obj[key];
    btn.classList.toggle('on', obj[key]);
    btn.setAttribute('aria-pressed', String(obj[key]));
  });
  return btn;
}

// A day-threshold input bound to obj[key] (positive int, else null = off).
function daysInput(obj, key) {
  const inp = el('input', {
    type: 'number', min: '0', class: 'rule-stale-days',
    value: obj[key] != null ? String(obj[key]) : '', placeholder: 'off',
  });
  inp.addEventListener('input', () => {
    const v = parseInt(inp.value, 10);
    obj[key] = Number.isFinite(v) && v > 0 ? v : null;
  });
  return inp;
}

// Toggle for untagged_is_error (error vs warning). Mutates draft in place.
function untaggedToggle(draft) {
  const label = el('span', {}, draft.untagged_is_error ? 'error' : 'warning');
  const btn = el('button', {
    class: 'toggle-btn' + (draft.untagged_is_error ? ' on' : ''), type: 'button',
    'aria-pressed': String(draft.untagged_is_error),
  }, [el('span', { class: 'toggle-switch' }, el('span', { class: 'toggle-knob' })), label]);
  btn.addEventListener('click', () => {
    draft.untagged_is_error = !draft.untagged_is_error;
    btn.classList.toggle('on', draft.untagged_is_error);
    btn.setAttribute('aria-pressed', String(draft.untagged_is_error));
    label.textContent = draft.untagged_is_error ? 'error' : 'warning';
  });
  return btn;
}

// Per-state staleness editor: existing (state → days) rows + an add row. Mutates
// the map object in place.
function staleEditor(map) {
  const box = el('div', { class: 'rule-stale' });
  const draw = () => {
    clear(box);
    Object.keys(map).sort().forEach((s) => {
      box.appendChild(el('div', { class: 'rule-stale-row' }, [
        el('span', { class: 'rule-stale-state' }, s),
        el('span', { class: 'muted' }, `${map[s]}d`),
        el('button', { type: 'button', class: 'rule-chip-x', onclick: () => { delete map[s]; draw(); } }, '×'),
      ]));
    });
    const stateIn = el('input', { class: 'rule-stale-input', type: 'text', placeholder: 'state (e.g. Active)' });
    const daysIn = el('input', { class: 'rule-stale-days', type: 'number', min: '1', placeholder: 'days' });
    const add = () => {
      const s = stateIn.value.trim();
      const d = parseInt(daysIn.value, 10);
      if (s && d > 0) { map[s] = d; draw(); }
    };
    daysIn.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); add(); } });
    box.appendChild(el('div', { class: 'rule-stale-row rule-stale-add' }, [
      stateIn, daysIn, el('button', { type: 'button', class: 'btn btn-xs', onclick: add }, 'Add'),
    ]));
  };
  draw();
  return box;
}

// Under "All teams": which teams override the default and which inherit it.
function ruleInheritanceCard(teams) {
  const card = el('div', { class: 'card' }, [el('h2', {}, 'Per-team policy')]);
  const table = el('table', {});
  table.appendChild(el('tr', {}, [th('Team'), th('Policy')]));
  teams.forEach((t) => {
    const overridden = !!t.rules;
    table.appendChild(el('tr', {}, [
      el('td', {}, t.name),
      el('td', {}, el('span', { class: 'rule-badge ' + (overridden ? 'warn' : 'muted') },
        overridden ? 'Team override' : 'Inherits default')),
    ]));
  });
  card.appendChild(el('div', { class: 'table-wrap' }, table));
  return card;
}

// ── Global settings (File > Settings modal) ─────────────────────────
// Import / export the whole configuration (teams, rules, saved reports) as a
// portable YAML file — backup, share, migrate standalone <-> hosted, or seed a
// headless run. Never includes secrets (the PAT stays in the environment).
function configCard() {
  const replace = el('input', { type: 'checkbox' });
  const fileInput = el('input', {
    type: 'file', accept: '.yaml,.yml,.txt', style: 'display:none',
    onchange: async (e) => {
      const f = e.target.files[0];
      e.target.value = '';
      if (!f) return;
      try {
        const summary = await importConfig(await f.text(), replace.checked);
        toast(`Imported ${summary.teams} team(s), ${summary.reports} report(s) (${summary.replaced ? 'replaced' : 'merged'})`);
        await Promise.all([refreshAuth(), initTeamSelector()]);
        route();
        document.getElementById('settings-overlay')?.remove();
      } catch (err) {
        toast('Import failed: ' + (err.message || err), true);
      }
    },
  });
  return el('div', { class: 'card' }, [
    el('h2', {}, 'Configuration'),
    el('p', { class: 'muted', style: 'margin-top:0' },
      'Back up or share your teams, rules, and saved reports as a YAML file. Secrets are never included.'),
    el('label', { class: 'row', style: 'gap:8px;margin:8px 0;align-items:center' }, [
      replace, el('span', { class: 'muted' }, 'On import, replace everything (otherwise merge — add what’s missing)'),
    ]),
    el('div', { class: 'row' }, [
      el('button', { class: 'btn btn-primary', onclick: doExport }, 'Export'),
      el('button', { class: 'btn', onclick: () => fileInput.click() }, 'Import…'),
      fileInput,
    ]),
  ]);
}

async function doExport() {
  try {
    const yaml = await exportConfig();
    const url = URL.createObjectURL(new Blob([yaml], { type: 'application/x-yaml' }));
    const a = el('a', { href: url, download: 'poseiden-config.yaml' });
    document.body.appendChild(a); a.click(); a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    toast('Exported poseiden-config.yaml');
  } catch (err) {
    toast('Export failed: ' + (err.message || err), true);
  }
}

// Instance-wide config that is NOT team-scoped: the remote-repoint control and
// a read-only view of the instance configuration (poll interval + team roster).
// Team rules moved to the team-scoped Rules screen; team add/edit/remove is the
// ✎ modal beside the selector.
async function showSettings(focus) {
  const overlay = el('div', { class: 'dc-overlay', id: 'settings-overlay', onclick: (e) => { if (e.target === overlay) overlay.remove(); } });
  const close = () => overlay.remove();

  const urlInput = el('input', { type: 'url', placeholder: 'https://poseiden.your-company.com', value: getInstanceUrl() });
  const connectionCard = el('div', { class: 'card' }, [
    el('h2', {}, 'Connection'),
    el('p', { class: 'muted', style: 'margin-top:0' },
      'Repoint this client at a hosted POSEIDEN instance to see your web-configured boards on the go. Currently: ' + mode() + '.'),
    el('label', { class: 'field' }, [
      el('span', {}, "Remote instance URL (leave empty to use this instance's own data)"),
      urlInput,
    ]),
    el('div', { class: 'row' }, [
      el('button', { class: 'btn btn-primary', onclick: () => { setInstanceUrl(urlInput.value); toast('Saved - reloading'); setTimeout(() => location.reload(), 500); } }, 'Connect'),
      el('button', { class: 'btn', onclick: () => { setInstanceUrl(''); urlInput.value = ''; toast('Cleared - reloading'); setTimeout(() => location.reload(), 500); } }, 'Use local'),
    ]),
  ]);

  // Display preferences (client-side, localStorage) - pagination for the lists.
  const pageInput = el('input', { type: 'number', min: '0', step: '50', value: String(getPageSize()), style: 'max-width:140px' });
  const displayCard = el('div', { class: 'card' }, [
    el('h2', {}, 'Display'),
    el('p', { class: 'muted', style: 'margin-top:0' },
      'Rows shown per page in the Work Items and Pipelines lists. 0 shows every row on one page (heavier on large lists).'),
    el('label', { class: 'field' }, [el('span', {}, 'Rows per page'), pageInput]),
    el('div', { class: 'row' }, [
      el('button', {
        class: 'btn btn-primary',
        onclick: () => {
          setPageSize(pageInput.value);
          pageInput.value = String(getPageSize());
          toast('Saved - reopen a list to apply');
        },
      }, 'Save'),
    ]),
  ]);

  // Group the cards into horizontal tabs so the modal stays short - one concern at
  // a time instead of a long scroll. Polling/Instance only exist when the instance
  // config loads.
  let cfg;
  try { cfg = await api.config(); } catch { cfg = null; }
  const tabs = [
    { id: 'connection', label: 'Connection 🔗', nodes: [connectionCard] },
    { id: 'ai', label: 'AI ✨', nodes: [llmIntegrationsCard(), tagInputsCard()] },
    { id: 'display', label: 'Display 📺', nodes: [displayCard] },
  ];
  if (cfg) tabs.push({ id: 'polling', label: 'Polling 🔄', nodes: [pollingCard(cfg), instanceConfigCard(cfg)] });
  tabs.push({ id: 'config', label: 'Import / Export 📦', nodes: [configCard()] });

  const panels = tabs.map((t) => el('div', { class: 'settings-tab-panel', 'data-tab': t.id }, t.nodes));
  const tabBar = el('div', { class: 'rule-tabs settings-tabs' });
  const show = (id) => {
    panels.forEach((p) => { p.style.display = p.dataset.tab === id ? '' : 'none'; });
    tabBar.querySelectorAll('.rule-tab').forEach((b) => b.classList.toggle('active', b.dataset.tab === id));
  };
  tabs.forEach((t) => tabBar.appendChild(
    el('button', { class: 'rule-tab', type: 'button', 'data-tab': t.id, onclick: () => show(t.id) }, t.label)));
  const body = el('div', {}, [tabBar, el('div', {}, panels)]);
  // Deep-link from the list's "N of M" count opens straight to Display.
  show(focus === 'display' ? 'display' : tabs[0].id);

  overlay.appendChild(el('div', { class: 'settings-modal' }, [
    el('div', { class: 'settings-head' }, [
      el('h3', { style: 'margin:0' }, 'Settings'),
      el('button', { class: 'icon-btn', title: 'Close', onclick: close }, '✕'),
    ]),
    el('div', { class: 'settings-body' }, [body]),
    el('div', { class: 'row', style: 'justify-content:flex-end' }, [
      el('button', { class: 'btn btn-primary', onclick: close }, 'Close'),
    ]),
  ]));
  document.body.appendChild(overlay);

  // Opened via the list's "N of M" count → jump straight to the pagination
  // control: scroll it into view, flash it, and select the input.
  if (focus === 'display') {
    requestAnimationFrame(() => {
      displayCard.scrollIntoView({ block: 'center', behavior: 'smooth' });
      displayCard.classList.add('flash');
      pageInput.focus();
      pageInput.select();
    });
  }
}

// Polling scope: fetch only the active (selected) team each cycle, or every
// configured team. Persists to the instance config on toggle.
function pollingCard(cfg) {
  const on = !!cfg.poll_all_teams;
  const toggle = el('button', {
    class: 'toggle-btn' + (on ? ' on' : ''), type: 'button', 'aria-pressed': String(on),
  }, [el('span', { class: 'toggle-switch' }, el('span', { class: 'toggle-knob' })), el('span', {}, 'Poll all teams')]);
  toggle.addEventListener('click', async () => {
    const next = !toggle.classList.contains('on');
    try {
      await api.setPollAllTeams(next);
      toggle.classList.toggle('on', next);
      toggle.setAttribute('aria-pressed', String(next));
      toast(next ? 'Polling all teams each cycle' : 'Polling the active team only');
    } catch (e) { toast('Failed: ' + (e?.message || e), true); }
  });
  return el('div', { class: 'card' }, [
    el('h2', {}, 'Polling'),
    el('p', { class: 'muted', style: 'margin-top:0' },
      'By default each poll fetches only the team selected in the top bar - keeping lookups off teams you are not viewing. Turn this on to poll every configured team each cycle.'),
    toggle,
  ]);
}

// A slim instance summary. The full team roster used to live here but that just
// duplicated the ✎ team editor, so this now shows only what ISN'T editable
// elsewhere (the poll interval) and points at the real editors.
function instanceConfigCard(cfg) {
  const teams = cfg.team || [];
  return el('div', { class: 'card' }, [
    el('h2', {}, 'Instance'),
    el('p', { class: 'muted', style: 'margin-top:0' }, [
      `Polling every ${cfg.server?.poll_interval || 'default'} · ${teams.length} team(s). `,
      'Set POSEIDEN_POLL_INTERVAL to change it (applied on restart).',
    ]),
    el('p', { class: 'muted', style: 'margin:0' },
      'Manage teams with the ✎ button beside the Team selector; edit hygiene rules on the Rules screen.'),
  ]);
}

// Tag inputs: what the tagger (AI + keyword) reads. Titles are always used; the
// work-item description is opt-in (richer signal, but for a CLOUD backend the body
// leaves the box). A single per-owner toggle, reusing the pill toggle style.
function tagInputsCard() {
  const card = el('div', { class: 'card' }, [
    el('h2', {}, 'Tag inputs'),
    el('p', { class: 'muted', style: 'margin-top:0' },
      'What the tagger reads. Titles are always used. Including the work-item description gives richer suggestions - but for a cloud backend it sends the body off the box (local/WebGPU backends stay on your machine).'),
  ]);
  const holder = el('div', {}, el('span', { class: 'muted' }, 'Loading…'));
  card.appendChild(holder);
  api.tagSettings().then((s) => {
    clear(holder);
    const on = s?.use_description !== false;
    const toggle = el('button', {
      class: 'toggle-btn' + (on ? ' on' : ''), type: 'button', 'aria-pressed': String(on),
    }, [el('span', { class: 'toggle-switch' }, el('span', { class: 'toggle-knob' })), el('span', {}, 'Use work-item description')]);
    toggle.addEventListener('click', async () => {
      const next = !toggle.classList.contains('on');
      try {
        await api.setTagSettings(next);
        toggle.classList.toggle('on', next);
        toggle.setAttribute('aria-pressed', String(next));
        toast(next ? 'Descriptions will feed tag suggestions' : 'Descriptions excluded from tag suggestions');
      } catch (e) { toast('Failed: ' + (e?.message || e), true); }
    });
    holder.append(toggle);
  }).catch(() => { clear(holder); holder.append(el('span', { class: 'muted' }, 'Unavailable on this instance.')); });
  return card;
}

// LLM Integrations card: a registry of AI backends. The first compatible with this
// platform (top of the list) is active; reorder to prioritise, grey = unusable here
// but kept in your account for another platform.
function llmIntegrationsCard() {
  const card = el('div', { class: 'card' }, [
    el('h2', {}, 'LLM Integrations'),
    el('p', { class: 'muted', style: 'margin-top:0' },
      'AI backends for tag suggestions. The topmost integration compatible with this platform is active - reorder to prioritise. Advisory: you apply the suggestions.'),
  ]);
  const holder = el('div', {}, el('span', { class: 'muted' }, 'Loading…'));
  card.appendChild(holder);
  api.llmConfig()
    .then((data) => renderIntegrations(holder, data))
    .catch(() => { clear(holder); holder.append(el('span', { class: 'muted' }, 'LLM settings unavailable on this instance.')); });
  return card;
}

// Render the registry list into `holder`. Every mutation (add/edit/delete/reorder)
// updates the local list and persists the WHOLE registry, then reloads to re-badge.
function renderIntegrations(holder, data) {
  const list = (data.integrations || []).slice();
  const presets = data.presets || { online: [], offline: [] };
  const caps = data.caps || {};
  // What actually runs for THIS client - the server's "active" flag only knows the
  // server's platform, but WebGPU runs in the browser. So recompute the effective
  // active here (WebGPU judged by the browser, everything else by the server verdict).
  const activeId = effectiveActiveId(list, caps);

  async function persist() {
    const clean = list.map((i) => ({
      id: i.id, name: i.name, kind: i.kind, provider: i.provider || null, endpoint: i.endpoint || null,
      model: i.model || null, api_key: i.api_key ?? null, offline_model: i.offline_model || null, device: i.device || 'cpu',
    }));
    await api.setLlmConfig({ integrations: clean });
    const fresh = await api.llmConfig();      // re-fetch to recompute active/compatible
    renderIntegrations(holder, fresh);
  }

  clear(holder);
  if (!list.length) {
    // Empty state: showcase the range with one-click example templates (sorted
    // most→least powerful). Each greys out where this platform can't run it; "Add"
    // opens the form pre-filled (so cloud ones can take a key) - it never seeds the
    // registry with unconfigured entries.
    holder.append(el('p', { class: 'muted', style: 'font-size:13px' }, 'No integrations yet. Add your own, or start from an example:'));
    holder.append(el('div', { class: 'llm-list' },
      starterTemplates().map((t) => templateCard(t, list, persist, presets, caps, holder))));
  } else {
    holder.append(el('div', { class: 'llm-list' },
      list.map((i, idx) => integrationRow(i, idx, list, persist, presets, caps, holder, activeId))));
  }
  // WebGPU is a BROWSER capability the server can't see (caps.webgpu is the
  // server's view, always false in a web client), so report the real browser check.
  holder.append(el('div', { class: 'muted', style: 'font-size:12px;margin:8px 0' },
    `This platform: embedded ${caps.embedded ? '✓' : '✗'} · GPU ${caps.gpu ? '✓' : '✗'} · WebGPU ${webgpuAvailable() ? '✓' : '✗'}`));
  const benchOut = el('div', { class: 'llm-bench' });
  holder.append(el('div', { class: 'row', style: 'gap:8px;margin-top:2px' }, [
    el('button', { class: 'btn',
      onclick: () => openIntegrationForm(holder, null, list, persist, presets, caps) }, '+ Add integration'),
    el('button', { class: 'btn', title: 'Run one test query against every usable backend and time it (WebGPU runs in your browser and may download its model first)',
      onclick: (e) => runBenchmark(e.currentTarget, benchOut, list, caps) }, '⏱ Benchmark'),
    el('button', { class: 'btn btn-ghost', title: 'Discard your entries and restore the full default catalog',
      onclick: async () => {
        if (!confirm('Reset LLM integrations to the default catalog? Your custom entries and API keys will be removed.')) return;
        try { renderIntegrations(holder, await api.resetLlmConfig()); toast('Reset to the default catalog'); }
        catch (e) { toast('Reset failed: ' + (e?.message || e), true); }
      } }, 'Reset to defaults'),
  ]));
  holder.append(benchOut);
}

// Fixed benchmark probe - a small, unambiguous work item + allowed set, mirrored on
// the server (see Service::benchmark_llms) so server + WebGPU timings are comparable.
const BENCH_ITEM = { id: 0, title: 'Login button unresponsive on mobile Safari after the latest deploy', work_item_type: 'Bug', tags: [] };
const BENCH_ALLOWED = ['type:bug', 'area:frontend', 'priority:high', 'platform:mobile', 'needs:triage'];

function fmtMs(ms) {
  if (typeof ms !== 'number') return '';
  return ms >= 1000 ? (ms / 1000).toFixed(ms >= 10000 ? 0 : 1) + 's' : Math.round(ms) + 'ms';
}

function applyBenchResult(cells, res) {
  const label = { ok: 'ok', timeout: 'timed out', error: 'error', unsupported: 'unsupported', unconfigured: 'needs setup' }[res.status] || res.status;
  const cls = res.status === 'ok' ? 'pill ok' : res.status === 'timeout' ? 'pill warn' : res.status === 'error' ? 'pill err' : 'pill muted';
  const title = res.error || (res.status === 'timeout' ? 'No reply within the 120s benchmark window - too slow to be practical here' : '');
  clear(cells.status); cells.status.append(el('span', { class: cls, title }, label));
  clear(cells.time);
  cells.time.append(document.createTextNode(fmtMs(res.ms)));
  // One-time model load (cold start) shown as secondary context, so the headline
  // number stays the per-query response time.
  if (res.note) cells.time.append(el('div', { class: 'muted', style: 'font-size:10px' }, res.note));
}

// Benchmark every usable + configured backend: server-side ones via one API call
// (timed on the server), WebGPU ones in the browser (loads the model, then times a
// single query). Renders a live table of status + response time.
async function runBenchmark(btn, out, list, caps) {
  btn.disabled = true;
  const orig = btn.textContent;
  btn.textContent = 'Benchmarking…';
  clear(out);
  const usable = (list || []).filter((i) => (i.kind === 'webgpu' ? webgpuAvailable() : i.compatible) && i.configured !== false);
  if (!usable.length) {
    out.append(el('div', { class: 'muted', style: 'font-size:12px;margin-top:8px' },
      'No usable, configured backend to benchmark yet - add an API key, or reorder/enable a local one.'));
    btn.disabled = false; btn.textContent = orig; return;
  }
  const table = el('table', { class: 'bench-table' }, [
    el('tr', {}, [el('th', {}, 'Integration'), el('th', {}, 'Status'), el('th', { style: 'text-align:right' }, 'Time')]),
  ]);
  const cellsById = new Map();
  usable.forEach((i) => {
    const status = el('td', {}, el('span', { class: 'muted' }, i.kind === 'webgpu' ? 'browser…' : 'running…'));
    const time = el('td', { style: 'text-align:right' }, '');
    cellsById.set(i.id, { status, time });
    table.appendChild(el('tr', {}, [el('td', {}, i.name), status, time]));
  });
  out.append(table);

  // The server benchmarks all its backends in one call (up to ~2 min if a local
  // model is cold-loading), so tick a live elapsed counter instead of a dead spinner.
  const serverIds = usable.filter((i) => i.kind !== 'webgpu').map((i) => i.id);
  const startedAt = performance.now();
  const ticker = serverIds.length
    ? setInterval(() => {
        const secs = Math.round((performance.now() - startedAt) / 1000);
        serverIds.forEach((id) => {
          const cells = cellsById.get(id);
          if (cells) { clear(cells.status); cells.status.append(el('span', { class: 'muted' }, `running… ${secs}s`)); }
        });
      }, 1000)
    : null;
  try {
    const data = await api.benchmarkLlms();
    if (ticker) clearInterval(ticker);
    (data.results || []).forEach((res) => {
      const cells = cellsById.get(res.id);
      if (cells) applyBenchResult(cells, res);
    });
  } catch (e) {
    if (ticker) clearInterval(ticker);
    serverIds.forEach((id) => {
      const cells = cellsById.get(id);
      if (cells) applyBenchResult(cells, { status: 'error', error: e?.message || String(e) });
    });
  }

  // WebGPU entries run in the browser (server can't); time them here, one at a time.
  // To measure per-query latency (not the one-time model download/compile), we LOAD
  // the model first, run a throwaway warm-up query, THEN time a clean query.
  const setStatus = (cells, text) => { clear(cells.status); cells.status.append(el('span', { class: 'muted' }, text)); };
  for (const i of usable.filter((x) => x.kind === 'webgpu')) {
    const cells = cellsById.get(i.id);
    if (!cells) continue;
    try {
      setStatus(cells, 'loading model…');
      const loadStart = performance.now();
      await prepareModel(i.offline_model, (p) => setStatus(cells, (p && p.text) ? p.text : 'loading model…'));
      const loadMs = performance.now() - loadStart;
      setStatus(cells, 'warming up…');
      await runWebGpuTagging(i.offline_model, [BENCH_ITEM], BENCH_ALLOWED, () => {}, () => {});
      setStatus(cells, 'timing…');
      const t0 = performance.now();
      await runWebGpuTagging(i.offline_model, [BENCH_ITEM], BENCH_ALLOWED, () => {}, () => {});
      applyBenchResult(cells, { status: 'ok', ms: performance.now() - t0, note: 'model load ' + fmtMs(loadMs) });
    } catch (err) {
      applyBenchResult(cells, { status: 'error', error: err?.message || String(err) });
    }
  }
  btn.disabled = false; btn.textContent = orig;
}

// Example integration templates for the empty-state gallery - the full range, most→
// least powerful, so the platform-aware greying showcases the multiplatform design.
// The Ollama one is the exact endpoint that reaches a host Ollama from the dev pod.
function starterTemplates() {
  return [
    { name: 'On-device GPU (CUDA)', kind: 'offline', offline_model: 'qwen2.5-1.5b', device: 'gpu', note: 'Embedded model on an NVIDIA GPU - fastest local (needs a CUDA build/host).' },
    { name: 'In-browser (WebGPU)', kind: 'webgpu', offline_model: 'qwen2.5-1.5b', note: 'Runs on your GPU in the browser - no install, no server GPU.' },
    { name: 'Local Ollama (your GPU)', kind: 'online', provider: 'custom', endpoint: 'http://host.minikube.internal:11434/v1/chat/completions', model: 'qwen2.5:1.5b', note: 'Your own Ollama over HTTP - this endpoint reaches a host Ollama from the dev/minikube pod.' },
    { name: 'On-device CPU', kind: 'offline', offline_model: 'qwen2.5-0.5b', device: 'cpu', note: 'Embedded model on CPU - runs anywhere, slower.' },
    { name: 'Claude (Anthropic)', kind: 'online', provider: 'anthropic', note: 'Cloud - fast; needs an API key. Item titles are sent to the provider.' },
    { name: 'Gemini (Google)', kind: 'online', provider: 'gemini', note: 'Cloud - needs an API key.' },
    { name: 'ChatGPT (OpenAI)', kind: 'online', provider: 'openai', note: 'Cloud - needs an API key.' },
  ];
}

function templateCompatible(t, caps) {
  if (t.kind === 'webgpu') return webgpuAvailable();
  if (t.kind === 'online') return true;
  if (t.kind === 'offline') return !!caps.embedded && (t.device !== 'gpu' || !!caps.gpu);
  return false;
}

function templateCard(t, list, persist, presets, caps, holder) {
  const ok = templateCompatible(t, caps);
  return el('div', { class: 'llm-row' + (ok ? '' : ' llm-incompatible') }, [
    el('div', { style: 'min-width:0;flex:1 1 auto' }, [
      el('div', {}, [
        el('strong', {}, t.name), ' ',
        ok ? el('span', { class: 'pill muted' }, 'example')
          : el('span', { class: 'pill muted', title: 'Not runnable on this platform' }, 'unavailable here'),
      ]),
      el('div', { class: 'muted', style: 'font-size:12px' }, t.note),
    ]),
    el('div', { class: 'llm-row-actions' }, [
      el('button', { class: 'btn btn-xs', title: 'Add as a new integration (review + save)',
        onclick: () => openIntegrationForm(holder, { ...t }, list, persist, presets, caps) }, '+ Add'),
    ]),
  ]);
}

// Why an integration can't run here - shown under greyed rows so the reason is
// explicit, not just a colour. WebGPU is judged by the browser, the rest by the
// server's platform caps.
function incompatReason(i, caps) {
  if (i.kind === 'webgpu') return 'Unsupported on this platform - needs a WebGPU-capable browser';
  if (i.kind === 'offline' && !caps.embedded) return 'Unsupported on this platform - no in-process model engine here';
  if (i.kind === 'offline' && i.device === 'gpu' && !caps.gpu) return 'Unsupported on this platform - needs a CUDA GPU';
  return 'Unsupported on this platform';
}

function integrationRow(i, idx, list, persist, presets, caps, holder, activeId) {
  // WebGPU compatibility is a BROWSER capability the server can't see, so judge it
  // client-side; every other kind uses the server's platform verdict.
  const compatible = i.kind === 'webgpu' ? webgpuAvailable() : i.compatible;
  const status = i.id === activeId ? el('span', { class: 'pill ok' }, 'Active')
    : !compatible ? el('span', { class: 'pill muted', title: incompatReason(i, caps) }, 'Unsupported here')
      : i.configured === false ? el('span', { class: 'pill warn', title: 'Add an API key (Edit) to activate this backend' }, 'Needs API key')
        : el('span', { class: 'pill muted' }, 'Standby');
  const meta = [i.kind, i.offline_model || i.model || i.provider, i.kind === 'offline' ? i.device : null].filter(Boolean).join(' · ');
  const move = (from, to) => async () => { const [x] = list.splice(from, 1); list.splice(to, 0, x); await persist(); };

  // WebGPU model download: the browser fetches the model into its cache. web-llm
  // reports fractional progress, so show a real bar (hidden until you click Download).
  const progWrap = el('div', { class: 'llm-progress-wrap' });
  const progBar = el('progress', { class: 'llm-progress', max: '1', value: '0' });
  const progText = el('span', { class: 'muted', style: 'font-size:12px' }, '');
  progWrap.append(progBar, progText);

  const actions = [
    el('button', { class: 'btn btn-xs', title: 'Move up', disabled: idx === 0, onclick: move(idx, idx - 1) }, '↑'),
    el('button', { class: 'btn btn-xs', title: 'Move down', disabled: idx === list.length - 1, onclick: move(idx, idx + 1) }, '↓'),
  ];
  if (i.kind === 'webgpu' && webgpuAvailable()) {
    const markCached = () => { dl.disabled = true; dl.textContent = '✓ Downloaded'; dl.title = 'Model is cached in this browser'; };
    const dl = el('button', { class: 'btn btn-xs', title: 'Download this model into your browser cache now (~0.5-1 GB, one time)',
      onclick: async () => {
        dl.disabled = true; dl.textContent = '⬇ Downloading…'; progWrap.style.display = 'flex'; progText.textContent = 'Starting…';
        try {
          await prepareModel(i.offline_model, (p) => {
            if (typeof p.progress === 'number') progBar.value = String(p.progress);
            progText.textContent = p.text || (typeof p.progress === 'number' ? Math.round(p.progress * 100) + '%' : 'Downloading…');
          });
          progBar.value = '1'; progText.textContent = 'Ready ✓ (cached)';
          markCached();
        } catch (err) { progText.textContent = 'Failed: ' + (err?.message || err); dl.disabled = false; dl.textContent = '⬇ Download'; }
      } }, '⬇ Download');
    actions.push(dl);
    // Reflect existing cache state: if already downloaded, show it as done (disabled).
    isModelCached(i.offline_model).then((cached) => { if (cached) markCached(); }).catch(() => {});
  }
  actions.push(
    el('button', { class: 'btn btn-xs', onclick: () => openIntegrationForm(holder, i, list, persist, presets, caps) }, 'Edit'),
    el('button', { class: 'btn btn-xs', onclick: async () => { if (confirm(`Delete "${i.name}"?`)) { list.splice(idx, 1); await persist(); } } }, 'Delete'),
  );

  const detail = !compatible
    ? el('div', { class: 'muted', style: 'font-size:11px;opacity:0.8' }, incompatReason(i, caps))
    : null;
  return el('div', { class: 'llm-row' + (compatible ? '' : ' llm-incompatible') }, [
    el('div', { style: 'min-width:0;flex:1 1 auto' }, [
      el('div', {}, [el('strong', {}, i.name || '(unnamed)'), ' ', status]),
      el('div', { class: 'muted', style: 'font-size:12px' }, meta),
      detail,
      progWrap,
    ].filter(Boolean)),
    el('div', { class: 'llm-row-actions' }, actions),
  ]);
}

// Swap the list for the add/edit form; Save appends/updates + persists, Cancel
// re-renders the table.
function openIntegrationForm(holder, existing, list, persist, presets, caps) {
  const form = el('div', {});
  clear(holder); holder.append(form);
  integrationForm(form, existing, presets, caps,
    async (integ) => {
      const idx = existing ? list.findIndex((x) => x.id === existing.id) : -1;
      if (idx >= 0) list[idx] = integ; else list.push(integ);
      await persist();
    },
    async () => { renderIntegrations(holder, await api.llmConfig()); });
}

// ── Documentation (embedded markdown viewer) ────────────────────────
// Docs ship as static markdown under assets/docs/ (copied from /docs by the
// app's build.rs, and by CI for the web/Docker bundles). The frontend fetches
// them by relative URL - the same path resolves in the Tauri webview and on a
// static web host - and renders them with the vendored zero-dep markdown
// renderer. Because they ride in the bundle, they're always accurate to the
// running build.
// Per-feature guides (GUI + CLI, rendered as tabs) plus higher-level reference
// docs. Feature pages live under docs/features/; build.rs copies the tree.
const DOC_GROUPS = [
  { group: 'Features', docs: [
    { file: 'features/dashboard.md', name: 'Dashboard', blurb: 'At-a-glance + health check.' },
    { file: 'features/work-items.md', name: 'Work Items', blurb: 'The backlog table + editing.' },
    { file: 'features/pull-requests.md', name: 'Pull Requests', blurb: 'In-flight PRs + links.' },
    { file: 'features/pipelines.md', name: 'Pipelines', blurb: 'Pipeline health.' },
    { file: 'features/reports.md', name: 'Reports', blurb: 'Configurable report engine.' },
    { file: 'features/rules.md', name: 'Rules', blurb: 'Hygiene policy, per team.' },
    { file: 'features/setup.md', name: 'Setup', blurb: 'Sign-in, teams, config.' },
    { file: 'features/user-guide.md', name: 'User Guide', blurb: 'Portable, deploy, storage.' },
  ] },
  { group: 'Reference', docs: [
    { file: 'PROJECT_STATUS.md', name: 'Project status', blurb: 'What works today.' },
    { file: 'COMPATIBILITY.md', name: 'Compatibility', blurb: 'Feature × platform support.' },
    { file: 'ROADMAP.md', name: 'Roadmap', blurb: 'Committed next steps.' },
    { file: 'BACKLOG.md', name: 'Backlog', blurb: 'Everything considered, ranked.' },
    { file: 'SCOPE.md', name: 'Scope', blurb: 'What POSEIDEN is not.' },
    { file: 'CLI.md', name: 'CLI guide', blurb: 'Commands + worked examples.' },
    { file: 'DISTRIBUTION.md', name: 'Distribution', blurb: 'Every deploy target.' },
  ] },
];
const DOCS = DOC_GROUPS.flatMap((g) => g.docs);

const _docsCache = new Map();
async function fetchDoc(file) {
  if (_docsCache.has(file)) return _docsCache.get(file);
  const r = await fetch(`assets/docs/${file}`, { cache: 'force-cache' });
  if (!r.ok) throw new Error(`HTTP ${r.status}`);
  const text = await r.text();
  _docsCache.set(file, text);
  return text;
}

function openDocsModal(initialFile) {
  const overlay = el('div', { class: 'dc-overlay', onclick: (e) => { if (e.target === overlay) overlay.remove(); } });
  const close = () => overlay.remove();
  const view = el('article', { class: 'docs-view' });
  const nav = el('div', { class: 'docs-nav' });
  const buttons = new Map();

  async function load(doc) {
    homeBtn.classList.remove('active');
    buttons.forEach((b, f) => b.classList.toggle('active', f === doc.file));
    view.replaceChildren(el('p', { class: 'docs-loading' }, 'Loading…'));
    try {
      const md = await fetchDoc(doc.file);
      view.innerHTML = renderMarkdown(md);
      postProcessDoc(view, load, doc);
    } catch (e) {
      view.replaceChildren(el('div', { class: 'docs-error' }, `Could not load ${doc.name}: ${e?.message || e}`));
    }
    view.scrollTop = 0;
  }

  // Landing view: a card grid of every doc, grouped. The modal opens here; the
  // side nav + a click on the title return to it.
  function showIndex() {
    buttons.forEach((b) => b.classList.remove('active'));
    homeBtn.classList.add('active');
    const grid = el('div', {});
    DOC_GROUPS.forEach((grp) => {
      grid.appendChild(el('div', { class: 'docs-index-group' }, grp.group));
      const cards = el('div', { class: 'docs-cards' });
      grp.docs.forEach((doc) => {
        cards.appendChild(el('button', { type: 'button', class: 'docs-card', onclick: () => load(doc) }, [
          el('span', { class: 'docs-card-name' }, doc.name),
          el('span', { class: 'docs-card-blurb' }, doc.blurb),
        ]));
      });
      grid.appendChild(cards);
    });
    view.replaceChildren(
      el('h1', {}, 'Documentation'),
      el('p', { class: 'docs-index-lead' }, 'Per-feature guides — each covers the GUI and CLI side by side — plus project reference.'),
      grid,
    );
    view.scrollTop = 0;
  }

  const homeBtn = el('button', { type: 'button', class: 'docs-nav-home', onclick: showIndex }, [
    el('span', { class: 'doc-name' }, '← Overview'),
    el('span', { class: 'doc-blurb' }, 'All guides + reference'),
  ]);
  nav.appendChild(homeBtn);
  DOC_GROUPS.forEach((grp) => {
    nav.appendChild(el('div', { class: 'docs-nav-group' }, grp.group));
    grp.docs.forEach((doc) => {
      const btn = el('button', { type: 'button', onclick: () => load(doc) }, [
        el('span', { class: 'doc-name' }, doc.name),
        el('span', { class: 'doc-blurb' }, doc.blurb),
      ]);
      buttons.set(doc.file, btn);
      nav.appendChild(btn);
    });
  });

  overlay.appendChild(el('div', { class: 'docs-modal' }, [
    el('div', { class: 'docs-head' }, [
      el('h3', { class: 'docs-home-link', title: 'Docs home', onclick: showIndex }, '📖 Documentation'),
      el('button', { class: 'icon-btn', title: 'Close', onclick: close }, '✕'),
    ]),
    el('div', { class: 'docs-body' }, [nav, el('div', { class: 'docs-content' }, view)]),
  ]));
  document.body.appendChild(overlay);
  const initial = initialFile && DOCS.find((d) => d.file === initialFile);
  if (initial) load(initial); else showIndex();
}

// Repoint relative asset paths + intercept links in a rendered doc: cross-doc
// `.md` links switch the active doc in-place; external links open in the OS
// browser; relative images resolve under assets/docs/.
function postProcessDoc(container, load, doc) {
  // Resolve relative images against the doc's own directory under assets/docs/
  // (a feature page at features/x.md references screenshots/y.png).
  const dir = doc && doc.file.includes('/') ? doc.file.slice(0, doc.file.lastIndexOf('/')) : '';
  const base = dir ? `assets/docs/${dir}/` : 'assets/docs/';
  container.querySelectorAll('img[src]').forEach((img) => {
    const src = img.getAttribute('src');
    if (src && !/^(https?:|\/|assets\/|data:)/.test(src)) img.setAttribute('src', base + src);
  });
  // Cross-doc `.md` links switch the active doc in-place, matched by basename
  // (feature pages link each other as `rules.md`, or up as `../ROADMAP.md`).
  container.querySelectorAll('a[href]').forEach((a) => {
    const href = a.getAttribute('href');
    if (!href || href.startsWith('#')) return;
    const bn = href.split('/').pop();
    const known = /\.md$/i.test(href) ? DOCS.find((d) => d.file.split('/').pop() === bn) : null;
    if (known) {
      a.addEventListener('click', (e) => { e.preventDefault(); load(known); });
    } else if (/^https?:/i.test(href)) {
      a.addEventListener('click', (e) => { e.preventDefault(); openExternal(href); });
    }
  });
  _wrapDocsTabs(container);
}

// Collapse a doc's `## GUI` and `## CLI` sections into a tabbed widget (the
// content between the GUI h2 and the CLI h2 becomes the GUI panel; CLI h2 to the
// next h2 becomes the CLI panel; everything else stays outside). No-op when the
// page doesn't have both headings.
function _wrapDocsTabs(viewEl) {
  const children = Array.from(viewEl.children);
  let guiIdx = -1;
  let cliIdx = -1;
  let endIdx = children.length;
  for (let i = 0; i < children.length; i++) {
    const node = children[i];
    if (node.tagName !== 'H2') continue;
    const text = node.textContent.trim();
    if (text === 'GUI' && guiIdx < 0) guiIdx = i;
    else if (text === 'CLI' && cliIdx < 0) cliIdx = i;
    else if (cliIdx >= 0 && endIdx === children.length) endIdx = i;
  }
  if (guiIdx < 0 || cliIdx < 0 || guiIdx > cliIdx) return;

  const guiPanel = el('div', { class: 'docs-tab-panel' });
  for (let i = guiIdx + 1; i < cliIdx; i++) guiPanel.appendChild(children[i]);
  const cliPanel = el('div', { class: 'docs-tab-panel' });
  cliPanel.hidden = true;
  for (let i = cliIdx + 1; i < endIdx; i++) cliPanel.appendChild(children[i]);

  const bar = el('div', { class: 'docs-tabs-bar' });
  ['GUI', 'CLI'].forEach((label) => {
    const btn = el('button', {
      type: 'button', class: 'docs-tab' + (label === 'GUI' ? ' active' : ''),
      onclick: () => {
        bar.querySelectorAll('.docs-tab').forEach((b) => b.classList.toggle('active', b === btn));
        guiPanel.hidden = label !== 'GUI';
        cliPanel.hidden = label !== 'CLI';
      },
    }, label);
    bar.appendChild(btn);
  });

  const tabs = el('div', { class: 'docs-tabs' }, [bar, guiPanel, cliPanel]);
  children[guiIdx].parentNode.insertBefore(tabs, children[guiIdx]);
  children[guiIdx].remove();
  children[cliIdx].remove();
}

// ── Shared bits ─────────────────────────────────────────────────────
function th(t) { return el('th', {}, t); }

function statusPill(status) {
  const map = {
    succeeded: ['ok', 'succeeded'], failed: ['err', 'failed'],
    running: ['run', 'running'], canceled: ['muted', 'canceled'], unknown: ['muted', 'unknown'],
  };
  const [cls, label] = map[status] || ['muted', status || 'never run'];
  return el('span', { class: 'pill ' + cls }, label);
}

function linkOut(text, url) {
  return el('a', { class: 'link', href: url || '#', onclick: (e) => { e.preventDefault(); if (url) openExternal(url); } }, text);
}

// ── Team scope selector ─────────────────────────────────────────────
// Populate from the configured teams; changing it re-scopes every view.
async function initTeamSelector() {
  const select = document.getElementById('team-select');
  if (!select) return;
  // Bind the change handler exactly once. initTeamSelector re-runs every time
  // the team modal closes (to reflect edits); re-adding the listener each time
  // would stack duplicate handlers.
  if (!select.dataset.bound) {
    select.addEventListener('change', () => {
      setTeamScope(select.value);
      // Tell the backend which team is active, so an active-team poll fetches it.
      api.setActiveTeam(select.value).catch(() => {});
      route();
    });
    select.dataset.bound = '1';
  }
  // Rebuild from scratch: drop every option except the static "All teams" (the
  // first). Appending without clearing is what duplicated the dropdown on each
  // reopen of the editor.
  while (select.options.length > 1) select.remove(1);
  try {
    const teams = await api.teams();
    for (const name of teams || []) {
      select.appendChild(el('option', { value: name }, name));
    }
    // Re-apply the saved scope now the options exist. If the saved team is gone
    // (config changed), fall back to All teams.
    if (getTeamScope() && !(teams || []).includes(getTeamScope())) {
      setTeamScope('');
    }
    select.value = getTeamScope();
    // Sync the persisted scope to the backend so the scheduled poll targets the
    // right team even before the user touches the selector.
    api.setActiveTeam(getTeamScope()).catch(() => {});
  } catch {
    // No backend reachable yet (static host / not connected) - the selector
    // stays "All teams"; the view's own error panel handles the guidance.
  }
}

// ── Desktop menu (hamburger → File / Edit / View / Help) ────────────
let zoomLevel = 1;

function exec(cmd) { try { document.execCommand(cmd); } catch { /* not editable */ } }
function setZoom(z) {
  zoomLevel = Math.min(3, Math.max(0.5, z));
  document.body.style.zoom = zoomLevel;
}
function toggleFullscreen() {
  if (document.fullscreenElement) document.exitFullscreen();
  else document.documentElement.requestFullscreen?.();
}

function menuDef() {
  const desktop = mode() === 'desktop';
  return [
    { label: 'File', items: [
      { label: 'Settings…', run: showSettings },
      { label: 'Quit', accel: 'Ctrl+Q', run: () => windowAction('quit'), desktopOnly: true },
    ] },
    { label: 'Edit', items: [
      { label: 'Undo', accel: 'Ctrl+Z', run: () => exec('undo') },
      { label: 'Redo', accel: 'Ctrl+Y', run: () => exec('redo') },
      { sep: true },
      { label: 'Cut', accel: 'Ctrl+X', run: () => exec('cut') },
      { label: 'Copy', accel: 'Ctrl+C', run: () => exec('copy') },
      { label: 'Paste', accel: 'Ctrl+V', run: () => exec('paste') },
      { label: 'Delete', run: () => exec('delete') },
      { sep: true },
      { label: 'Select All', accel: 'Ctrl+A', run: () => exec('selectAll') },
    ] },
    { label: 'View', items: [
      { label: 'Back', accel: 'Ctrl+[', run: () => history.back() },
      { label: 'Forward', accel: 'Ctrl+]', run: () => history.forward() },
      { label: 'Reload', accel: 'Ctrl+R', run: () => location.reload() },
      { sep: true },
      { label: 'Toggle Developer Tools', accel: 'Ctrl+Shift+I', run: () => windowAction('toggle_devtools'), desktopOnly: true },
      { sep: true },
      { label: 'Actual Size', accel: 'Ctrl+0', run: () => setZoom(1) },
      { label: 'Zoom In', accel: 'Ctrl++', run: () => setZoom(zoomLevel + 0.1) },
      { label: 'Zoom Out', accel: 'Ctrl+-', run: () => setZoom(zoomLevel - 0.1) },
      { sep: true },
      { label: 'Toggle Full Screen', accel: 'F11', run: toggleFullscreen },
    ] },
    { label: 'Help', items: [ { label: 'About', run: showAbout } ] },
  ].map((top) => ({ ...top, items: top.items.filter((it) => it.sep || !it.desktopOnly || desktop) }));
}

function closeMenu() {
  const m = document.getElementById('app-menu');
  if (m) m.remove();
  document.removeEventListener('click', outsideMenuClose);
}
function outsideMenuClose(e) {
  const m = document.getElementById('app-menu');
  if (m && !m.contains(e.target) && e.target.id !== 'menu-btn') closeMenu();
}
function openMenu() {
  closeMenu();
  const menu = el('div', { class: 'app-menu', id: 'app-menu' });
  menuDef().forEach((top) => {
    const sub = el('div', { class: 'menu-sub' });
    top.items.forEach((it) => {
      if (it.sep) { sub.appendChild(el('div', { class: 'menu-sep' })); return; }
      sub.appendChild(el('div', {
        class: 'menu-item',
        onclick: async () => { closeMenu(); try { await it.run(); } catch (e) { toast(String(e.message || e), true); } },
      }, [el('span', {}, it.label), it.accel ? el('span', { class: 'menu-accel' }, it.accel) : null]));
    });
    menu.appendChild(el('div', { class: 'menu-top' }, [
      el('span', {}, top.label), el('span', { class: 'menu-arrow' }, '▸'), sub,
    ]));
  });
  document.body.appendChild(menu);
  const r = document.getElementById('menu-btn').getBoundingClientRect();
  menu.style.left = r.left + 'px';
  menu.style.top = (r.bottom + 4) + 'px';
  setTimeout(() => document.addEventListener('click', outsideMenuClose), 0);
}

function showAbout() {
  const overlay = el('div', { class: 'dc-overlay', onclick: (e) => { if (e.target === overlay) overlay.remove(); } });
  overlay.appendChild(el('div', { class: 'about-modal' }, [
    el('img', { class: 'about-logo', src: 'assets/logo.png', width: 72, height: 72, alt: 'POSEIDEN' }),
    el('div', { class: 'about-wordmark' }, [el('span', { class: 'po' }, 'PO'), el('span', {}, 'SEIDEN')]),
    el('p', { class: 'about-tag' },
      "Work comes in never-ending waves. POSEIDEN is a Product Owner support tool that helps you see them, sort them, and show what's adrift - backlog hygiene, pipeline observation, and velocity in one place."),
    el('p', { class: 'muted' }, 'Version 0.1.0 · Dual-licensed MIT / Apache-2.0'),
    el('button', { class: 'btn btn-primary', onclick: () => overlay.remove() }, 'Close'),
  ]));
  document.body.appendChild(overlay);
}

// ── Team-management modal (edit button beside the dropdown) ──────────
function openTeamModal() {
  const overlay = el('div', {
    class: 'dc-overlay', id: 'team-modal',
    onclick: (e) => { if (e.target === overlay) closeTeamModal(); },
  });
  const panel = el('div', { class: 'team-modal' });
  overlay.appendChild(panel);
  document.body.appendChild(overlay);
  renderTeamList(panel);
}

// Closing refreshes the selector + reconciles the Doctor (which registers any
// new team's access check) and re-runs it - surfacing the auth check WITHOUT
// having prompted a login during add.
async function closeTeamModal() {
  const o = document.getElementById('team-modal');
  if (o) o.remove();
  await initTeamSelector();
  try { doctorReport = await api.doctorRecheck(); updateDoctorIndicator(); } catch { /* background tick will catch it */ }
  await refreshDoctor();
  await route();
}

async function renderTeamList(panel) {
  clear(panel);
  panel.appendChild(el('div', { class: 'team-modal-head' }, [
    el('h2', {}, 'Manage teams'),
    el('button', { class: 'icon-btn', onclick: closeTeamModal, title: 'Close' }, '✕'),
  ]));

  // Config serialises the teams array under the `team` key (the TOML `[[team]]`
  // block name).
  let teams = [];
  try { teams = (await api.config()).team || []; } catch { /* offline */ }

  const list = el('div', { class: 'team-list' });
  if (!teams.length) list.appendChild(el('div', { class: 'empty' }, 'No teams yet - add one below.'));
  teams.forEach((t) => {
    list.appendChild(el('div', { class: 'team-item' }, [
      el('div', {}, [
        el('div', { class: 'team-item-name' }, t.name),
        el('div', { class: 'muted mono' }, t.project + (t.area_path ? ' · ' + t.area_path : '')),
      ]),
      el('div', { class: 'row' }, [
        el('button', { class: 'btn', onclick: () => renderTeamForm(panel, t) }, 'Edit'),
        el('button', {
          class: 'btn', onclick: async () => {
            if (!confirm(`Remove team "${t.name}"?`)) return;
            try { await api.removeTeam(t.name); toast('Removed - the Doctor will prune its check'); await renderTeamList(panel); }
            catch (e) { toast('Remove failed: ' + (e.message || e), true); }
          },
        }, 'Remove'),
      ]),
    ]));
  });
  panel.appendChild(list);
  panel.appendChild(el('button', { class: 'btn btn-primary', onclick: () => renderTeamForm(panel, null) }, '+ Add team'));
}

function renderTeamForm(panel, existing) {
  clear(panel);
  panel.appendChild(el('div', { class: 'team-modal-head' }, [
    el('h2', {}, existing ? 'Edit team' : 'Add team'),
    el('button', { class: 'icon-btn', onclick: closeTeamModal, title: 'Close' }, '✕'),
  ]));

  // Each backend maps onto the same TeamConfig (organization + project), but the
  // fields mean different things and Azure-only extras (area path, Entra tenant)
  // don't apply elsewhere. `id` is the exact ProviderKind serde value.
  const PROVIDERS = [
    { id: 'azure-dev-ops', label: 'Azure DevOps', azure: true,
      orgLabel: 'Organization URL', orgPh: 'https://dev.azure.com/your-org',
      projLabel: 'Project', projPh: 'Platform Engineering',
      patPh: 'POSEIDEN_AZURE_PAT (optional; sign-in works too)' },
    { id: 'github', label: 'GitHub', azure: false,
      orgLabel: 'Repository owner', orgPh: 'octocat  (user or org)',
      projLabel: 'Repository', projPh: 'hello-world',
      patPh: 'POSEIDEN_GITHUB_TOKEN (optional; public repos need none)' },
    { id: 'gitlab', label: 'GitLab', azure: false,
      orgLabel: 'Namespace or base URL', orgPh: 'gitlab-org  (or https://gitlab.example.com)',
      projLabel: 'Project path', projPh: 'gitlab-runner',
      patPh: 'POSEIDEN_GITLAB_TOKEN (optional; public projects need none)' },
  ];
  const providerOf = (id) => PROVIDERS.find((p) => p.id === id) || PROVIDERS[0];
  let providerId = (existing && existing.provider) || 'azure-dev-ops';

  const name = el('input', { type: 'text', value: existing ? existing.name : '', placeholder: 'Platform Engineering' });
  const org = el('input', { type: 'text', value: existing ? existing.organization : '' });
  const project = el('input', { type: 'text', value: existing ? existing.project : '' });
  const area = el('input', { type: 'text', value: (existing && existing.area_path) || '', placeholder: 'optional - e.g. Platform\\DevOps' });
  const tenant = el('input', { type: 'text', value: (existing && existing.tenant) || '', placeholder: 'optional - e.g. your-org.com' });
  const patEnv = el('input', { type: 'text', value: (existing && existing.auth && existing.auth.pat_env) || '' });
  // Checked = include child areas (the default UNDER scope); unchecked = strict,
  // exact-path only. Only meaningful when an area path is set (Azure only).
  const includeChildren = el('input', { type: 'checkbox' });
  includeChildren.checked = !(existing && existing.area_path_strict);
  const field = (labelText, input) => el('label', { class: 'field' }, [el('span', {}, labelText), input]);

  const providerSel = el('select', {
    class: 'inp', onchange: (ev) => { providerId = ev.target.value; renderFields(); },
  }, PROVIDERS.map((p) => el('option', { value: p.id, selected: p.id === providerId }, p.label)));

  // The provider-dependent fields live in their own host, re-rendered on change
  // so labels/placeholders track the backend and Azure-only fields hide.
  const fieldsHost = el('div', {});
  function renderFields() {
    const p = providerOf(providerId);
    org.placeholder = p.orgPh;
    project.placeholder = p.projPh;
    patEnv.placeholder = p.patPh;
    clear(fieldsHost);
    fieldsHost.append(el('div', { class: 'grid grid-2' }, [field(p.orgLabel, org), field(p.projLabel, project)]));
    if (p.azure) {
      fieldsHost.append(field('Area path', area));
      fieldsHost.append(el('label', { class: 'field-check' }, [includeChildren, el('span', {}, 'Include child boards (area path descendants)')]));
      fieldsHost.append(field('Entra tenant (for sign-in)', tenant));
    }
    fieldsHost.append(field('Token env var (optional)', patEnv));
  }

  const save = el('button', {
    class: 'btn btn-primary', onclick: async (e) => {
      const p = providerOf(providerId);
      if (!name.value.trim() || !org.value.trim() || !project.value.trim()) {
        toast(`Name, ${p.orgLabel.toLowerCase()}, and ${p.projLabel.toLowerCase()} are required`, true); return;
      }
      const b = e.currentTarget; b.disabled = true; b.textContent = 'Saving…';
      const team = {
        name: name.value.trim(), provider: providerId,
        organization: org.value.trim(), project: project.value.trim(),
        area_path: p.azure ? (area.value.trim() || null) : null,
        tenant: p.azure ? (tenant.value.trim() || null) : null,
        area_path_strict: p.azure ? !includeChildren.checked : false,
      };
      const patEnvVal = patEnv.value.trim();
      if (patEnvVal) team.auth = { pat_env: patEnvVal };
      try {
        // Adding/editing NEVER prompts a login - it only writes the definition;
        // the Doctor registers/refreshes its access check, and the auth check
        // surfaces (red until you sign in) once the modal closes.
        const res = existing ? await api.updateTeam(existing.name, team) : await api.addTeam(team);
        if (!existing && res && res.added === false) { toast('A team with that name already exists', true); b.disabled = false; b.textContent = 'Save'; return; }
        toast(existing ? 'Team updated' : 'Team added - its health check will register');
        try { doctorReport = await api.doctorRecheck(); updateDoctorIndicator(); } catch { /* background tick */ }
        await renderTeamList(panel);
      } catch (err) {
        toast('Save failed: ' + (err.message || err), true);
        b.disabled = false; b.textContent = 'Save';
      }
    },
  }, 'Save');

  panel.appendChild(el('div', { class: 'grid grid-2' }, [field('Team name', name), field('Provider', providerSel)]));
  panel.appendChild(fieldsHost);
  renderFields();
  panel.appendChild(el('div', { class: 'row' }, [save, el('button', { class: 'btn', onclick: () => renderTeamList(panel) }, 'Cancel')]));
}

// ── Boot ────────────────────────────────────────────────────────────
document.getElementById('menu-btn').addEventListener('click', (e) => {
  e.stopPropagation();
  if (document.getElementById('app-menu')) closeMenu();
  else openMenu();
});
document.getElementById('team-edit-btn').addEventListener('click', openTeamModal);
document.getElementById('docs-btn').addEventListener('click', () => openDocsModal());
document.getElementById('settings-btn').addEventListener('click', () => showSettings());

document.getElementById('refresh-btn').addEventListener('click', async (e) => {
  const btn = e.currentTarget;
  btn.disabled = true;
  btn.textContent = '↻ Polling…';
  try {
    const outcome = await api.pollNow();
    const errs = (outcome && outcome.errors) || [];
    toast(errs.length
      ? `Polled with ${errs.length} issue(s): ${errs[0]}`
      : `Polled ${outcome.teams_polled} team(s): ${outcome.work_items} items, ${outcome.runs} runs`,
      errs.length > 0);
    await refreshAuth(); // a poll may have changed auth state (e.g. token expiry)
    await route();
  } catch (err) {
    toast('Poll failed: ' + (err.message || err), true);
  } finally {
    btn.disabled = false;
    btn.textContent = '↻ Refresh';
  }
});

// ── Motto ───────────────────────────────────────────────────────────
// Nautical mottos, mirrored from poseiden-core's MOTDS (Rust side, used by the
// CLI banner). One is picked per launch and shown under the wordmark, in the
// browser-tab title, and - on desktop - the native window title.
const MOTDS = [
  'Weather the storm',
  'Stem the tide',
  'Ride the wave',
  'Keep your head above water',
  'Stay afloat',
];

function initMotto() {
  const motto = MOTDS[Math.floor(Math.random() * MOTDS.length)];
  const badge = document.getElementById('brand-motd');
  if (badge) badge.textContent = motto;
  const title = `POSEIDEN - ${motto}`;
  document.title = title;
  // On the desktop shell, also set the native window title (withGlobalTauri).
  try {
    const win = window.__TAURI__?.window?.getCurrentWindow?.();
    if (win?.setTitle) win.setTitle(title);
  } catch { /* not the desktop shell */ }
}

// ── First-run onboarding ────────────────────────────────────────────
// The setup flow is driven by a capabilities probe, not by "is this desktop?":
// we ask the shell what it can actually do and offer only real options.
//
//   already pointed at an instance  → boot as that client (no chooser)
//   can host a local Service        → first-run chooser: run locally OR connect
//     (desktop / mobile native)       to a shared instance; skip if already set up
//   a server is serving us          → we are ON that instance; boot straight in
//     (Docker / Helm web instance)    (empty-state guidance covers a fresh tenant)
//   static host, no backend         → client-only flow: the sole option is to
//     (e.g. GitHub Pages)             point at a remote instance

async function bootApp() {
  // A configured remote (the user's saved choice, or a deployment-injected
  // default via env.js) means we are a client already - nothing to set up.
  if (getInstanceUrl()) return finishBoot();

  const caps = await capabilities();

  // Native shell: offer standalone. First run if the local store isn't built yet.
  if (caps.localService) {
    if (await isInitialized()) return finishBoot();
    return showOnboarding(caps);
  }

  // Browser served by a poseiden server: we are already on that instance.
  if (caps.sameOriginApi) return finishBoot();

  // Static host (no local store, no backend): the only move is to become a
  // client of a remote instance, so show the client-only connect flow.
  return showOnboarding(caps);
}

// The normal startup: pick a default route, resolve auth + teams before the
// first render (both tolerate a missing backend), then start Doctor polling.
function finishBoot() {
  if (!location.hash) location.hash = '#dashboard';
  Promise.all([refreshAuth(), initTeamSelector()]).finally(route);
  startDoctorPolling();
  renderUserMenu();
  autotuneAi();
}

// Best-effort, fire-and-forget: detect this browser's capabilities (WebGPU + coarse
// RAM/cores the server can't see) and let the server size the AI models to the
// platform. No-ops server-side on a hand-edited registry, so it never fights a
// choice the user made in Settings. Silent on failure - AI auto-config is a nicety,
// not load-bearing.
async function autotuneAi() {
  try {
    const caps = await detectBrowserCaps();
    await api.autotuneLlm(caps);
  } catch { /* AI autotune is best-effort */ }
}

// The signed-in user block in the sidebar foot. Shown only for an authenticated
// proxy user (hosted); the name is informational for now, the icon logs out via
// oauth2-proxy's sign-out. Desktop / local / unauthenticated stays hidden.
async function renderUserMenu() {
  const menu = document.getElementById('user-menu');
  if (!menu) return;
  const dev = getDevOwner();
  let id = null;
  try { id = await identity(); } catch { /* offline */ }
  clear(menu);

  if (dev) {
    // Dev impersonation active: acting as an owner via the injected header.
    menu.append(
      el('button', {
        class: 'foot-item user-name', type: 'button',
        title: 'Dev: acting as ' + dev + ' - click to change',
        onclick: promptDevOwner,
      }, [el('span', { class: 'user-dev-tag' }, 'DEV'), ' ' + dev]),
      el('a', {
        class: 'user-logout', href: '#', title: 'Stop impersonating', 'aria-label': 'Stop impersonating',
        onclick: (e) => { e.preventDefault(); setDevOwner(''); location.reload(); },
      }, '✕'),
    );
    menu.hidden = false;
    return;
  }
  if (id && id.authenticated) {
    // Real proxy identity: name (informational) + logout.
    menu.append(
      el('button', { class: 'foot-item user-name', type: 'button', title: 'Signed in as ' + id.owner }, id.owner),
      el('a', {
        class: 'user-logout', title: 'Log out', 'aria-label': 'Log out',
        href: (getInstanceUrl() || '') + '/oauth2/sign_out',
      }, '⎋'),
    );
    menu.hidden = false;
    return;
  }
  // Auth off (dev / playground): offer to view a specific tenant by owner email.
  menu.append(el('button', {
    class: 'foot-item user-name', type: 'button',
    title: 'Dev: view a specific tenant by owner email (auth-off playground)',
    onclick: promptDevOwner,
  }, 'Connect as… (dev)'));
  menu.hidden = false;
}

// Dev-only: set the owner the browser impersonates (sent as X-Auth-Request-Email
// same-origin). A real auth proxy overwrites the header, so this does nothing in
// a hosted auth-on deployment.
function promptDevOwner() {
  const email = window.prompt(
    'Dev: view which tenant? Enter an owner email (e.g. demo@example.com). Clear to reset.',
    getDevOwner() || '',
  );
  if (email == null) return;
  setDevOwner(email);
  location.reload();
}

// The shell's capabilities, captured when onboarding opens, so the step helpers
// can offer only what this environment supports (standalone card, portable
// choice, a Back button) without re-probing.
let onboardCaps = { localService: true, portableMode: true };

function showOnboarding(caps) {
  onboardCaps = caps || onboardCaps;
  hideOnboarding();
  const modal = el('div', { class: 'dc-modal', style: 'max-width:540px;text-align:left' });
  const overlay = el('div', { class: 'dc-overlay', id: 'onboard-overlay' }, [modal]);
  document.body.appendChild(overlay);
  // With no local Service the only option is "connect to an instance", so skip
  // the run-locally-vs-remote chooser and go straight to the connect form.
  if (onboardCaps.localService) onboardWelcome(modal);
  else onboardRemote(modal);
}

function hideOnboarding() {
  const o = document.getElementById('onboard-overlay');
  if (o) o.remove();
}

// A big clickable choice card.
function onboardChoice(title, desc, onClick) {
  return el('button', {
    class: 'btn', style: 'display:block;width:100%;text-align:left;padding:14px;margin:8px 0',
    onclick: onClick,
  }, [
    el('div', {}, el('strong', {}, title)),
    el('div', { class: 'muted', style: 'margin-top:4px' }, desc),
  ]);
}

function onboardWelcome(modal) {
  clear(modal);
  modal.append(
    el('h3', {}, 'Welcome to POSEIDEN'),
    el('div', { class: 'muted', style: 'margin-bottom:12px' }, 'How do you want to run it?'),
    onboardChoice('Run on this device', 'Keep your own local database and poll your work tracker directly. No server needed.', () => onboardStorage(modal)),
    onboardChoice('Connect to a shared instance', 'Point at a POSEIDEN instance your team already hosts.', () => onboardRemote(modal)),
  );
}

function onboardStorage(modal) {
  // Portable is a desktop-OS concept; where it isn't meaningful (mobile sandboxes
  // its own storage) the probe reports portableMode=false, so skip straight to
  // init with portable off.
  if (!onboardCaps.portableMode) return onboardInit(modal, false);
  clear(modal);
  let portable = false;
  const toggle = el('input', { type: 'checkbox', onchange: (e) => { portable = e.target.checked; } });
  modal.append(
    el('h3', {}, 'Where should data live?'),
    el('label', { class: 'row', style: 'gap:10px;align-items:flex-start;margin:12px 0' }, [
      toggle,
      el('div', {}, [
        el('div', {}, el('strong', {}, 'Portable mode')),
        el('div', { class: 'muted' }, 'Keep the database, logs, and settings in a “.portable” folder next to the app instead of the OS data directory — good for a USB stick or a self-contained copy. Chosen now, before anything is written; moving later means moving the data by hand.'),
      ]),
    ]),
    el('div', { class: 'row', style: 'justify-content:flex-end;gap:8px' }, [
      el('button', { class: 'btn', onclick: () => onboardWelcome(modal) }, 'Back'),
      el('button', { class: 'btn btn-primary', onclick: () => onboardInit(modal, portable) }, 'Continue'),
    ]),
  );
}

function onboardRemote(modal) {
  clear(modal);
  const input = el('input', { type: 'url', placeholder: 'https://poseiden.example.com', style: 'width:100%' });
  const connect = el('button', {
    class: 'btn btn-primary', onclick: () => {
      const url = input.value.trim();
      if (!url) { toast('Enter an instance URL', true); return; }
      setInstanceUrl(url);
      location.reload(); // reboot as a remote client
    },
  }, 'Connect');
  // A Back button only makes sense when there was a chooser to come from; a
  // client-only shell (no local Service) opened straight onto this step.
  const actions = onboardCaps.localService
    ? [el('button', { class: 'btn', onclick: () => onboardWelcome(modal) }, 'Back'), connect]
    : [connect];
  const lead = onboardCaps.localService
    ? 'Enter the URL of your team’s POSEIDEN instance. You can change or clear this later in Settings.'
    : 'This is a web client - point it at your team’s POSEIDEN instance to get started. You can change or clear this later in Settings.';
  modal.append(
    el('h3', {}, 'Connect to an instance'),
    el('div', { class: 'muted', style: 'margin-bottom:8px' }, lead),
    input,
    el('div', { class: 'row', style: 'justify-content:flex-end;gap:8px;margin-top:12px' }, actions),
  );
}

async function onboardInit(modal, portable) {
  clear(modal);
  modal.append(el('h3', {}, 'Setting up…'), el('div', { class: 'muted' }, 'Preparing your local database.'));
  try {
    await initialize(portable);
    onboardAi(modal); // ask about AI, then finish into team config
  } catch (e) {
    clear(modal);
    modal.append(
      el('h3', {}, 'Setup failed'),
      el('div', { class: 'muted', style: 'margin:8px 0' }, String((e && e.message) || e)),
      el('div', { class: 'row', style: 'justify-content:flex-end' },
        el('button', { class: 'btn', onclick: () => onboardStorage(modal) }, 'Try again')),
    );
  }
}

// Optional onboarding step: enable AI tag suggestions, via a private on-device
// model or a hosted provider. Skippable; changeable later in Settings.
async function onboardAi(modal) {
  clear(modal);
  modal.append(
    el('h3', {}, 'LLM integration (optional)'),
    el('div', { class: 'muted', style: 'margin-bottom:10px' },
      'POSEIDEN can suggest canonical tags. Add an integration - an on-device model, your own GPU endpoint, or a hosted provider - or skip and add one later in Settings. You can configure several and reorder them.'),
  );
  let data = { presets: { online: [], offline: [] }, caps: {} };
  try { data = await api.llmConfig(); } catch { /* presets unavailable */ }
  const form = el('div', {});
  modal.append(form);
  integrationForm(form, null, data.presets || { online: [], offline: [] }, data.caps || {},
    async (integ) => {
      try { await api.setLlmConfig({ integrations: [integ] }); toast('LLM integration saved'); }
      catch (e2) { toast('Could not save: ' + (e2?.message || e2), true); return; }
      finishOnboarding(modal);
    },
    () => finishOnboarding(modal));
  modal.append(el('div', { class: 'row', style: 'justify-content:flex-end;margin-top:8px' },
    el('button', { class: 'btn', onclick: () => finishOnboarding(modal) }, 'Skip for now')));
}

function finishOnboarding(modal) {
  hideOnboarding();
  finishBoot();
  openTeamModal();
  toast('Add your first team to get started');
}

// Shared add/edit form for ONE integration (registry table + onboarding). Calls
// onSubmit(integration) on save, onCancel() on cancel. `caps` drives the "greyed
// on this platform" hints; a blank key on an existing integration keeps the stored one.
function integrationForm(container, existing, presets, caps, onSubmit, onCancel) {
  const e = existing || {};
  const keySet = e.api_key === ''; // redacted sentinel: '' means a key is stored
  let kind = e.kind || (caps.embedded ? 'offline' : 'online');
  let provider = e.provider || (presets.online[0] && presets.online[0].id) || 'anthropic';
  let offlineModel = e.offline_model || (presets.offline[0] && presets.offline[0].id) || '';
  let device = e.device || 'cpu';
  const nameInput = el('input', { class: 'inp', style: 'width:100%', placeholder: 'Name (e.g. Local GPU, Claude)', value: e.name || '' });
  const fields = el('div', { style: 'margin-top:8px' });
  let endpointInput = null, modelInput = null, keyInput = null;

  const warn = (msg) => el('div', { class: 'muted', style: 'font-size:12px;margin-top:4px;color:var(--warn)' }, msg);

  function rebuild() {
    clear(fields); endpointInput = modelInput = keyInput = null;
    if (kind === 'offline') {
      fields.append(
        el('div', { class: 'muted', style: 'font-size:12px;margin-bottom:4px' }, 'A small model downloaded once and run in-process - private.'),
        el('select', { class: 'inp', style: 'width:100%', onchange: (ev) => { offlineModel = ev.target.value; } },
          presets.offline.map((m) => el('option', { value: m.id, selected: m.id === offlineModel }, m.label))),
        el('select', { class: 'inp', style: 'width:100%;margin-top:6px', onchange: (ev) => { device = ev.target.value; rebuild(); } }, [
          el('option', { value: 'cpu', selected: device === 'cpu' }, 'CPU - runs anywhere (slower)'),
          el('option', { value: 'gpu', selected: device === 'gpu' }, 'GPU (CUDA) - fast, needs an NVIDIA build/host'),
        ]),
      );
      if (device === 'gpu' && !caps.gpu) fields.append(warn('No GPU on this platform - this integration is greyed here, but usable on a GPU build/host.'));
    } else if (kind === 'online') {
      const isCustom = provider === 'custom';
      fields.append(el('select', { class: 'inp', style: 'width:100%',
        onchange: (ev) => { provider = ev.target.value; rebuild(); } }, [
        ...presets.online.map((p) => el('option', { value: p.id, selected: p.id === provider }, p.label)),
        el('option', { value: 'custom', selected: isCustom }, 'Custom - your own OpenAI-compatible endpoint'),
      ]));
      if (isCustom) {
        endpointInput = el('input', { class: 'inp', style: 'width:100%;margin-top:6px', placeholder: 'http://localhost:11434/v1/chat/completions', value: e.endpoint || '' });
        modelInput = el('input', { class: 'inp', style: 'width:100%;margin-top:6px', placeholder: 'model (e.g. qwen2.5:1.5b)', value: e.model || '' });
        keyInput = el('input', { class: 'inp', type: 'password', style: 'width:100%;margin-top:6px', placeholder: keySet ? 'key set - leave blank to keep' : 'API key (blank for local models)' });
        fields.append(
          el('div', { class: 'muted', style: 'font-size:12px;margin:6px 0 4px' },
            'Any OpenAI-compatible endpoint - run a model on your GPU (Ollama/LM Studio/vLLM) and point here; local models usually need no key. From a hosted pod use host.minikube.internal, not localhost.'),
          endpointInput, modelInput, keyInput,
        );
      } else {
        keyInput = el('input', { class: 'inp', type: 'password', style: 'width:100%;margin-top:6px', placeholder: keySet ? 'key set - leave blank to keep' : 'API key' });
        fields.append(el('div', { class: 'muted', style: 'font-size:12px;margin:6px 0 4px' }, 'Item titles are sent to this provider. Paste an API key:'), keyInput);
      }
    } else if (kind === 'webgpu') {
      fields.append(
        el('div', { class: 'muted', style: 'font-size:12px;margin-bottom:4px' }, 'Runs the model in your browser on WebGPU - no install, no server GPU. Experimental; needs a WebGPU-capable browser (Chrome/Edge).'),
        el('select', { class: 'inp', style: 'width:100%', onchange: (ev) => { offlineModel = ev.target.value; } },
          presets.offline.map((m) => el('option', { value: m.id, selected: m.id === offlineModel }, m.label))),
      );
      if (!webgpuAvailable()) fields.append(warn('This browser has no WebGPU - this backend will be unsupported here (try Chrome/Edge).'));
    }
  }

  const kindSel = el('select', { class: 'inp', style: 'width:100%', onchange: (ev) => { kind = ev.target.value; rebuild(); } }, [
    el('option', { value: 'offline', selected: kind === 'offline' }, 'On-device model (embedded, private)'),
    el('option', { value: 'online', selected: kind === 'online' }, 'Online / self-hosted endpoint (Claude, Gemini, GPT, Ollama…)'),
    el('option', { value: 'webgpu', selected: kind === 'webgpu' }, 'In-browser (WebGPU) - experimental'),
  ]);

  const save = el('button', { class: 'btn btn-primary', onclick: async () => {
    const k = keyInput ? keyInput.value.trim() : '';
    const integ = {
      id: e.id || (self.crypto && crypto.randomUUID ? crypto.randomUUID() : 'i' + Date.now()),
      name: nameInput.value.trim() || 'Integration',
      kind,
      device: kind === 'offline' ? device : 'cpu',
      provider: kind === 'online' ? provider : null,
      endpoint: kind === 'online' && provider === 'custom' && endpointInput ? endpointInput.value.trim() : null,
      model: kind === 'online' && provider === 'custom' && modelInput ? modelInput.value.trim() : null,
      offline_model: (kind === 'offline' || kind === 'webgpu') ? offlineModel : null,
      api_key: k ? k : (keySet ? '' : null), // blank + keySet => keep stored key
    };
    save.disabled = true; save.textContent = 'Saving…';
    try { await onSubmit(integ); }
    catch (err) { toast('Could not save: ' + (err?.message || err), true); save.disabled = false; save.textContent = 'Save'; }
  } }, 'Save');

  clear(container);
  container.append(
    el('label', { class: 'field' }, [el('span', {}, 'Name'), nameInput]),
    el('label', { class: 'field' }, [el('span', {}, 'Type'), kindSel]),
    fields,
    el('div', { class: 'row', style: 'gap:8px;margin-top:12px' }, [
      save,
      el('button', { class: 'btn', onclick: () => onCancel && onCancel() }, 'Cancel'),
    ]),
  );
  rebuild();
}

window.addEventListener('hashchange', route);
// Build/version stamp in the sidebar foot (git sha in CI/prod, timestamp locally).
{
  const v = (window.__POSEIDEN_ENV__ && window.__POSEIDEN_ENV__.version) || '';
  const el = document.getElementById('version-stamp');
  if (el && v) el.textContent = v;
}
initMotto();
bootApp();
