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

function jobRow(job, kind) {
  const meta = el('div', { class: 'ai-row-meta' }, [
    whereLabel(job.where),
    el('span', { class: 'ai-row-sub' },
      kind === 'done'
        ? (job.status === 'failed' ? (job.error || 'failed')
          : job.status === 'cancelled' ? 'cancelled'
          : outcomeText(job))
        : progressText(job) || (kind === 'queued' ? 'waiting' : '')),
  ]);
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
  toggle.addEventListener('click', () => {
    open = !open;
    panel.hidden = !open;
    toggle.textContent = open ? 'Hide details ▾' : 'View details ▴';
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

    if (open) {
      clear(panel);
      const running = r ? [jobRow(r, 'running')] : [];
      [
        section('Running', running),
        section('Queued', snap.queued.map((j) => jobRow(j, 'queued'))),
        section('Completed', snap.completed.map((j) => jobRow(j, 'done'))),
      ].filter(Boolean).forEach((s) => panel.appendChild(s));
    }
  });
}
