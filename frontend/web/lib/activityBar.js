// The AI activity bar: a strip across the bottom of the content area (right of the
// 220px sidebar) that appears whenever AI work is running and vanishes when idle.
// "View details" expands a panel with the running job, the queue behind it, and recent
// results. It's a pure view over `aiQueue` - it reads the snapshot and renders; all
// state lives in the queue. Mounted once, for the life of the page.

import { el, clear } from './dom.js';
import { aiQueue } from './aiQueue.js';

// Where a job runs, shown so the client/server split is visible (a WebGPU job runs in
// this browser on the user's GPU; a server job runs behind the API and is polled).
function whereLabel(where) {
  return where === 'gpu'
    ? el('span', { class: 'ai-where ai-where-gpu', title: 'runs in this browser on your GPU' }, '▣ your GPU')
    : el('span', { class: 'ai-where', title: 'runs on the server (polled)' }, '▤ server');
}

function progressText(job) {
  const p = job.progress;
  if (!p) return '';
  if (p.done != null && p.total != null) return `${p.done} / ${p.total}`;
  return p.text || '';
}

// An outcome may be a short summary ("tagged 5/10") or, for an interactive draft, the
// whole generated field text - cap it so a Recent row stays one tidy line.
function outcomeText(job) {
  const s = (job.outcome || 'done').replace(/\s+/g, ' ').trim();
  return s.length > 48 ? s.slice(0, 47) + '…' : s;
}

// Compact relative time ("just now" / "5m" / "2h" / "3d") for a finished job; the full
// local timestamp goes in the tooltip. `ms` may be null (older records without a time).
function fmtAgo(ms) {
  if (!ms) return '';
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 45) return 'just now';
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

function jobRow(job, kind) {
  const when = kind === 'done' && job.finishedAt
    ? el('span', {
        class: 'ai-row-time',
        title: new Date(job.finishedAt).toLocaleString(),
      }, fmtAgo(job.finishedAt))
    : null;
  const meta = el('div', { class: 'ai-row-meta' }, [
    whereLabel(job.where),
    el('span', { class: 'ai-row-sub' },
      kind === 'done'
        ? (job.status === 'failed' ? (job.error || 'failed')
          : job.status === 'cancelled' ? 'cancelled'
          : outcomeText(job))
        : progressText(job) || (kind === 'queued' ? 'waiting' : '')),
    when,
  ].filter(Boolean));
  let right;
  if (kind === 'queued') {
    right = el('button', {
      class: 'btn btn-xs ai-row-cancel', type: 'button', title: 'remove from queue',
      onclick: () => aiQueue.cancelQueued(job.id),
    }, '✕');
  } else if (kind === 'done') {
    const ok = job.status === 'done';
    right = el('span', { class: 'ai-row-mark ' + (ok ? 'ok' : 'bad') }, ok ? '✓' : (job.status === 'cancelled' ? '⊘' : '!'));
  } else {
    right = el('span', { class: 'ai-row-mark running' }, '•');
  }
  return el('div', { class: 'ai-row' }, [
    el('span', { class: 'ai-row-icon' }, job.icon || '✨'),
    el('div', { class: 'ai-row-body' }, [el('div', { class: 'ai-row-name' }, job.name), meta]),
    right,
  ]);
}

// One row in any job's per-item list: `#id → …`. A tagging job passes `tags` (rendered
// as +chips); every other job passes a `note` string (e.g. 'audited', 'ready (3 fields)',
// 'too sparse'). `tone: 'warn'` dims/marks a skip or failure.
// A tag may be a plain string or a `{ tag, reason }` suggestion object (the WebGPU tagger
// returns the latter) - normalise to the name so we never render "[object Object]".
const tagName = (t) => (t && typeof t === 'object' ? t.tag : t);
function itemLine(it) {
  let detail;
  if (it.tags && it.tags.length) {
    detail = el('span', { class: 'ai-item-tags' }, it.tags.map((t) => el('span', { class: 'ai-item-tag' }, '+' + tagName(t))));
  } else {
    detail = el('span', { class: it.tone === 'warn' ? 'ai-item-skip' : 'ai-item-none' }, it.note || 'none');
  }
  return el('div', { class: 'ai-item' }, [
    el('span', { class: 'ai-item-id' }, '#' + it.id),
    el('span', { class: 'ai-item-arrow' }, '→'),
    detail,
  ]);
}

function section(title, rows) {
  if (!rows.length) return null;
  return el('div', { class: 'ai-section' }, [
    el('div', { class: 'ai-section-head' }, title),
    ...rows,
  ]);
}

/** Mount the activity bar once. Safe to call repeatedly (no-op after the first). */
export function mountActivityBar() {
  if (document.getElementById('ai-activity-bar')) return;

  // Guard against losing an in-flight (or queued) AI run to an accidental refresh/close -
  // a WebGPU pass runs in this tab and can't survive a reload. The browser shows its own
  // generic "leave site?" prompt when returnValue is set.
  window.addEventListener('beforeunload', (e) => {
    // Only warn for work that would actually be LOST (running or queued) - a leftover
    // finished-log doesn't count, even though it keeps the bar visible.
    const s = aiQueue.snapshot();
    if (s.running || s.queued.length) {
      e.preventDefault();
      e.returnValue = '';
    }
  });

  const panel = el('div', { class: 'ai-activity-panel', hidden: true });
  const runName = el('span', { class: 'ai-bar-name' }, '');
  const runCount = el('span', { class: 'ai-bar-count' }, '');
  const fill = el('div', { class: 'ai-bar-fill' });
  const track = el('div', { class: 'ai-bar-track' }, fill);
  const queuedInfo = el('span', { class: 'ai-bar-queued' }, '');
  const toggle = el('button', { class: 'btn btn-xs', type: 'button' }, 'View details ▴');

  let open = false;
  let lastSnap = aiQueue.snapshot();
  // Which finished jobs have their per-item list expanded. Kept here (not on the job)
  // so it survives the full-rebuild renders below.
  const expanded = new Set();

  // A job's row plus, if it processed items, their per-item list. The running job shows
  // it expanded and live (tailing to the newest); a finished job collapses it behind a
  // toggle so a long Completed history stays compact.
  function jobBlock(job, kind) {
    const row = jobRow(job, kind);
    const items = job.items || [];
    if (!items.length) return row;
    const list = el('div', { class: 'ai-items' }, items.map(itemLine));
    if (kind === 'running') {
      list.dataset.tail = '1';
      return el('div', { class: 'ai-job-block' }, [row, list]);
    }
    const isOpen = expanded.has(job.id);
    const toggle = el('button', {
      class: 'btn btn-xs ai-items-toggle', type: 'button',
      onclick: () => {
        if (isOpen) expanded.delete(job.id); else expanded.add(job.id);
        renderPanel(lastSnap);
      },
    }, `${isOpen ? '▾' : '▸'} ${items.length} item${items.length === 1 ? '' : 's'}`);
    return el('div', { class: 'ai-job-block' }, isOpen ? [row, toggle, list] : [row, toggle]);
  }

  // Rebuild the panel body from a snapshot. Called on every queue update while open AND
  // the instant the panel is opened - the latter matters because a server-polled job only
  // notify()s once per poll, so without an on-open render the panel sits empty until the
  // next tick (which read as "details panel empty while a job is running").
  function renderPanel(snap) {
    clear(panel);
    [
      section('Running', snap.running ? [jobBlock(snap.running, 'running')] : []),
      section('Queued', snap.queued.map((j) => jobRow(j, 'queued'))),
      section('Completed', snap.completed.map((j) => jobBlock(j, 'done'))),
    ].filter(Boolean).forEach((s) => panel.appendChild(s));
    // Keep the live running list scrolled to the newest item.
    const tail = panel.querySelector('.ai-items[data-tail="1"]');
    if (tail) tail.scrollTop = tail.scrollHeight;
  }

  toggle.addEventListener('click', () => {
    open = !open;
    panel.hidden = !open;
    toggle.textContent = open ? 'Hide details ▾' : 'View details ▴';
    if (open) renderPanel(lastSnap);
  });

  const spin = el('span', { class: 'ai-bar-spin' }, '◐');
  // Clears the finished list; once empty (and nothing running/queued) the bar hides.
  const clearBtn = el('button', { class: 'btn btn-xs', type: 'button', title: 'Clear finished items', style: 'display:none' }, 'Clear');
  clearBtn.addEventListener('click', () => aiQueue.clearCompleted());

  const bar = el('div', { class: 'ai-activity-bar' }, [
    spin,
    el('div', { class: 'ai-bar-main' }, [
      el('div', { class: 'ai-bar-line' }, [runName, runCount]),
      track,
    ]),
    queuedInfo,
    clearBtn,
    toggle,
  ]);

  const root = el('div', { id: 'ai-activity-bar', class: 'ai-activity', hidden: true }, [panel, bar]);
  document.body.appendChild(root);

  aiQueue.subscribe((snap) => {
    lastSnap = snap;
    // Reserve space at the bottom of the content column while the bar is up, so it never
    // overlays the last row / pagination (the bar is position:fixed).
    document.body.classList.toggle('ai-bar-visible', snap.active);
    if (!snap.active) {
      // Truly empty (nothing running/queued AND the finished list was cleared): hide the
      // bar and reset the panel so it reopens clean next time.
      root.hidden = true;
      open = false;
      panel.hidden = true;
      toggle.textContent = 'View details ▴';
      return;
    }
    root.hidden = false;

    const r = snap.running;
    if (r) {
      // Active run: spinner + live progress.
      spin.style.visibility = 'visible';
      track.style.display = '';
      runName.style.color = 'var(--ink)';
      runName.textContent = r.name;
      runCount.textContent = progressText(r);
      const p = r.progress;
      const pct = p && p.done != null && p.total ? Math.round((p.done / p.total) * 100) : null;
      // Known ratio -> real width; unknown -> indeterminate sliver.
      fill.style.width = pct != null ? pct + '%' : '30%';
      fill.classList.toggle('indeterminate', pct == null);
    } else {
      // Idle, but finished items remain (persist until cleared): resting state, no
      // spinner or progress bar - just a summary and the Clear affordance.
      spin.style.visibility = 'hidden';
      track.style.display = 'none';
      runName.style.color = 'var(--ink-soft)';
      const n = snap.completed.length;
      runName.textContent = `No active runs · ${n} finished`;
      runCount.textContent = '';
    }

    queuedInfo.textContent = snap.queued.length ? `${snap.queued.length} queued` : '';
    clearBtn.style.display = snap.completed.length ? '' : 'none';

    if (open) renderPanel(snap);
  });

  // Rebuild the finished list from the server activity log so the queue + per-item
  // results are visible after a refresh (and interrupted runs surface with partials).
  aiQueue.hydrateFromServer();
}
