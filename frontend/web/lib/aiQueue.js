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

let seq = 0;
let running = false;
let jobs = []; // active: queued + the one running, in submit order
// Completed/failed/cancelled jobs, newest first. They PERSIST until the user clears them
// (the activity bar stays up as a log, not a transient toast). A high cap only guards
// runaway memory - normal use never hits it.
const completed = [];
const COMPLETED_CAP = 200;
const subscribers = new Set();

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
  try {
    const report = (p) => {
      // Accept { done, total, text } or a bare status string.
      job.progress = typeof p === 'string' ? { text: p } : p || null;
      notify();
    };
    job.outcome = await job.run(report);
    job.status = 'done';
  } catch (err) {
    job.status = 'failed';
    job.error = (err && err.message) || String(err);
    console.error(`AI job "${job.name}" failed`, err);
  }
  jobs = jobs.filter((j) => j !== job);
  completed.unshift(job);
  if (completed.length > COMPLETED_CAP) completed.pop();
  running = false;
  notify();
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
    completed.unshift(job);
    if (completed.length > COMPLETED_CAP) completed.pop();
    notify();
    job._resolve(null); // unblock an awaiting caller
    return true;
  },

  /** Clear the finished (completed/failed/cancelled) list. Once empty, the bar hides if
   *  nothing is running or queued. Does not touch in-flight or queued jobs. */
  clearCompleted() {
    completed.length = 0;
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
