// A single-slot client-side AI job runner. One heavy job (tag-suggest, healthcheck
// audit, duplicate scan, bulk improve) runs at a time; the rest QUEUE instead of being
// hard-disabled. Replaces the old `setAiBusy` boolean lock: a second request now lands
// in the queue and runs when the slot frees, and the activity bar renders the state.
//
// Rationale for one-at-a-time: WebGPU can't sanely run two inference loops on the user's
// GPU at once, and overlapping data refreshes blanked the view. The queue keeps that
// guarantee while giving the user visibility + control (cancel a QUEUED job; a RUNNING
// one can't be interrupted mid-inference, so it has no cancel).
//
// A job: { name, icon, where: 'gpu' | 'server', run }. `run(report)` does the work and
// calls `report({ done, total, text })` to drive the progress bar; its resolved value is
// kept as the job's `outcome` (a short string shown in Recent). Throwing marks it failed.

import { api } from './api.js';

let seq = 0;
let running = false;
let jobs = []; // active: queued + the one running, in submit order
// Completed/failed/cancelled jobs, newest first. They PERSIST until the user clears them
// (the activity bar stays up as a log, not a transient toast). A high cap only guards
// runaway memory - normal use never hits it.
const completed = [];
const COMPLETED_CAP = 200;
const subscribers = new Set();

// Server-side persistence of the activity log, so the queue + per-item results survive a
// refresh and double as an audit trail. Best-effort: a failed POST never blocks the run.
// Throttled during a run (a 300-item job would otherwise POST 300×) but always flushed on
// a status change (start / done / failed / cancelled).
const PERSIST_THROTTLE_MS = 2500;
function persist(job, force) {
  if (!job || !job.persistId) return;
  const now = Date.now(); // browser clock (frontend) - fine here
  if (!force && job._lastPersist && now - job._lastPersist < PERSIST_THROTTLE_MS) return;
  job._lastPersist = now;
  const rec = {
    id: job.persistId,
    name: job.name,
    where: job.where,
    status: job.status,
    done: (job.progress && job.progress.done) || 0,
    total: (job.progress && job.progress.total) || 0,
    outcome: job.outcome || '',
    items: job.items || [],
  };
  try { Promise.resolve(api.recordAiActivity(rec)).catch(() => {}); } catch { /* offline */ }
}

function snapshot() {
  return {
    running: jobs.find((j) => j.status === 'running') || null,
    queued: jobs.filter((j) => j.status === 'queued'),
    completed: completed.slice(),
    // The bar stays up while there's ANY running/queued work OR uncleared completed
    // items - it only hides once the user clears the finished list.
    active: jobs.length > 0 || completed.length > 0,
  };
}

function notify() {
  const snap = snapshot();
  subscribers.forEach((fn) => {
    try {
      fn(snap);
    } catch (err) {
      console.error('aiQueue subscriber failed', err);
    }
  });
}

async function pump() {
  if (running) return;
  const job = jobs.find((j) => j.status === 'queued');
  if (!job) return;
  running = true;
  job.status = 'running';
  job.startedAt = Date.now();
  notify();
  persist(job, true);
  try {
    const report = (p) => {
      // Accept { done, total, text } or a bare status string.
      job.progress = typeof p === 'string' ? { text: p } : p || null;
      notify();
      persist(job);
    };
    // Per-item detail: a job that processes N things (e.g. tag-suggest over a
    // selection) can push one row per item, which the activity panel lists live
    // under the running job. { id, status: 'tagged'|'none'|'skipped', tags?, note? }.
    report.item = (it) => {
      (job.items || (job.items = [])).push(it);
      notify();
      persist(job);
    };
    job.outcome = await job.run(report);
    job.status = 'done';
  } catch (err) {
    job.status = 'failed';
    job.error = (err && err.message) || String(err);
    console.error(`AI job "${job.name}" failed`, err);
  }
  job.finishedAt = Date.now();
  jobs = jobs.filter((j) => j !== job);
  completed.unshift(job);
  if (completed.length > COMPLETED_CAP) completed.pop();
  running = false;
  notify();
  persist(job, true); // final state + full item list
  // Resolve the caller's completion promise with the outcome (or null on failure), so an
  // interactive caller (per-field draft/improve) can await its result. Never rejects -
  // fire-and-forget callers (the toolbar sweeps) can ignore it with no unhandled rejection.
  job._resolve(job.status === 'done' ? job.outcome ?? null : null);
  pump(); // next queued job, if any
}

export const aiQueue = {
  /** Subscribe to state changes; immediately called with the current snapshot.
   *  Returns an unsubscribe fn. */
  subscribe(fn) {
    subscribers.add(fn);
    fn(snapshot());
    return () => subscribers.delete(fn);
  },

  /** Enqueue a job. Runs immediately if the slot is free, else waits. Returns a promise
   *  that resolves to the job's outcome (or null on failure/cancel) - awaitable by an
   *  interactive caller, ignorable by a fire-and-forget one. `job.id` is on the promise. */
  enqueue({ name, icon, where, run }) {
    const job = {
      id: ++seq,
      // Stable cross-session id for the server activity log (queue-across-refresh).
      persistId: (typeof crypto !== 'undefined' && crypto.randomUUID)
        ? crypto.randomUUID()
        : `${seq}-${Date.now()}`,
      name,
      icon: icon || 'ti-sparkles',
      where: where === 'gpu' ? 'gpu' : 'server',
      status: 'queued',
      progress: null,
      outcome: null,
      run,
    };
    job.done = new Promise((resolve) => {
      job._resolve = resolve;
    });
    job.done.id = job.id;
    jobs.push(job);
    notify();
    persist(job, true);
    pump();
    return job.done;
  },

  /** Cancel a job that is still QUEUED (a running job can't be interrupted). No-op
   *  otherwise. Returns true if it was cancelled. */
  cancelQueued(id) {
    const job = jobs.find((j) => j.id === id && j.status === 'queued');
    if (!job) return false;
    jobs = jobs.filter((j) => j !== job);
    job.status = 'cancelled';
    job.finishedAt = Date.now();
    completed.unshift(job);
    if (completed.length > COMPLETED_CAP) completed.pop();
    notify();
    persist(job, true);
    job._resolve(null); // unblock an awaiting caller
    return true;
  },

  /** Clear the finished list from the CURRENT view. The server activity log (audit
   *  trail) is untouched, so a refresh re-hydrates it - this is a session-local dismiss. */
  clearCompleted() {
    completed.length = 0;
    notify();
  },

  /** Rebuild the finished list from the server activity log so the queue is visible
   *  after a refresh. Terminal records show as-is; a `running` record left by a dead tab
   *  (its client-side compute is gone) surfaces as an interrupted run with its partial
   *  results. Best-effort; a fetch failure just leaves the log empty. */
  async hydrateFromServer(limit = 50) {
    let records = [];
    try { records = (await api.aiActivity(limit)).activity || []; } catch { return; }
    // Don't duplicate anything we already have this session (by persistId).
    const have = new Set(completed.map((j) => j.persistId).concat(jobs.map((j) => j.persistId)));
    for (const r of records) {
      if (have.has(r.id)) continue;
      const interrupted = r.status === 'running';
      // SQLite datetime('now') is UTC "YYYY-MM-DD HH:MM:SS" - parse as UTC to ms so the
      // bar can show a local relative time.
      const utcMs = (s) => {
        const t = s ? Date.parse(String(s).replace(' ', 'T') + 'Z') : NaN;
        return Number.isNaN(t) ? null : t;
      };
      completed.push({
        persistId: r.id,
        name: r.name,
        icon: '✨',
        where: r.where,
        status: interrupted ? 'cancelled' : r.status,
        progress: { done: r.done, total: r.total },
        outcome: interrupted ? `interrupted at ${r.done}/${r.total}` : (r.outcome || ''),
        items: Array.isArray(r.items) ? r.items : [],
        startedAt: utcMs(r.started_at),
        finishedAt: utcMs(r.updated_at),
      });
    }
    if (completed.length > COMPLETED_CAP) completed.length = COMPLETED_CAP;
    notify();
  },

  /** True while a job is running (the old `aiBusy`). */
  isBusy() {
    return running;
  },

  /** True if a job with this name is currently queued or running - a dedupe guard so a
   *  double-click doesn't enqueue the same sweep twice. */
  has(name) {
    return jobs.some((j) => j.name === name);
  },

  snapshot,
};
