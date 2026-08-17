# AI activity bar + job queue

Status: **built (frontend), not yet verified in-browser / deployed**

## Problem

AI operations gave poor feedback: progress was buried in a button label
(`Auditing… 244/1109`), and a second AI action was **hard-disabled** while one ran
(the old `setAiBusy` boolean lock) with no indication of why or when it'd free.

## Shape

One progressive-disclosure surface, not three separate things:

- **Bottom bar** (`lib/activityBar.js`) across the content area (right of the 220px
  sidebar), shown only while AI work runs, **gone when idle**. Current job + progress
  bar + queued count.
- **"View details" → panel**: `Running` / `Queued` / `Recent` sections. Each row shows
  where it runs — `▣ your GPU` (WebGPU, in-browser) vs `▤ server` (polled background
  job) — the client/server split made visible.
- **Per-op outcome** in Recent (`tagged 312/757`) — the lightweight "log", no console.

## The queue (`lib/aiQueue.js`)

Replaces the `setAiBusy` hard lock with a **single-slot job runner**: one heavy job at
a time (WebGPU can't run two inference loops on the GPU at once), the rest **queue**
instead of being blocked. A second click enqueues; the button no longer disables while
busy (only when there's no selection). Pub/sub; the bar is a pure view over the snapshot.

- **Queued jobs are cancellable** (drop from the list); a **running job is not** — an
  in-browser WebGPU pass can't be cleanly interrupted mid-inference. So no ✕ on the
  running row.
- Double-click guard: `aiQueue.has(name)` — the same sweep can't enqueue twice.

## Wired

The four toolbar ops in `renderWorkItems` now `aiQueue.enqueue(...)` with progress via
`report({done,total})`: **Suggest tags**, **Run healthcheck**, **Find duplicates**
(server, not AI, but shares the slot so its whole-backlog scan + refresh don't overlap
an AI run), **Improve all fields** (bulk). Mounted once in `finishBoot()`.

## Interactive per-field AI (now queued)

The editor's per-field draft/improve (`buildAiAssist.run`) now enqueues too, so an
interactive draft never runs a second GPU loop alongside a toolbar sweep — it waits its
turn and shows in the bar. `aiQueue.enqueue` returns a promise resolving to the job's
outcome, which `run` awaits to get the drafted text (fire-and-forget toolbar callers
ignore it). The in-modal Improve-all's phase-1 goes through this too; its phase-2
consistency WebGPU call is still direct (low-risk edge: could overlap a job started
mid-flow).

## Follow-ups

- Pad the content bottom while the bar is visible (it currently overlays ~50px).
- ~~`left: 220px` hardcoded~~ → now `var(--sidebar-w)` on `:root` (drives the shell grid
  + the bar).
- ~~`beforeunload` guard~~ → done: warns on refresh/close while `aiQueue` has active work.
- The in-modal Improve-all phase-2 consistency call is not yet queued (see above).
