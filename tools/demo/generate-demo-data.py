#!/usr/bin/env python3
"""Regenerate the static demo-mode fixtures in frontend/web/assets/demo/.

Demo mode (app.html?demo=1, the public GitHub Pages demo) has no backend: every GET
is served from these JSON files. This script owns the work-item, dashboard, pipeline,
pull-request and report fixtures and writes them from one fictional dataset, so the
numbers on the Dashboard / Reports / Recap screens always agree with the work items.

All data is invented (a made-up "Platform" team) - never paste real tracker data here.

DATES: every timestamp is written relative to ANCHOR, with the newest data on the anchor
day. At load time the frontend (lib/api.js, demoShiftMs) slides all timestamps forward by
whole days so ANCHOR becomes "yesterday" - the demo therefore always looks current and
"last 30 days" always has data, however old these files get. ANCHOR below MUST equal
DEMO_ANCHOR in lib/api.js.

Run from anywhere:  python tools/demo/generate-demo-data.py
"""
import datetime as dt
import json
import random
from collections import Counter
from pathlib import Path

ANCHOR = dt.date(2026, 10, 2)  # keep in sync with DEMO_ANCHOR in frontend/web/lib/api.js
OUT = Path(__file__).resolve().parents[2] / "frontend" / "web" / "assets" / "demo"
rng = random.Random(7)  # deterministic: re-running yields identical files

PEOPLE = ["Jordan Kim", "Alex Rivera", "Sam Lee", "Priya Nair", "Marcus Chen", "Elena Rossi"]
CLOSED_STATES = {"Closed", "Done", "Resolved", "Completed", "Removed"}


def ts(days_ago, hh=9, mm=0, ss=0):
    d = ANCHOR - dt.timedelta(days=days_ago)
    return dt.datetime(d.year, d.month, d.day, hh, mm, ss, tzinfo=dt.timezone.utc)


def z(t):
    return t.strftime("%Y-%m-%dT%H:%M:%SZ")


def rand_time(days_ago):
    return ts(days_ago, rng.randint(8, 17), rng.choice([0, 5, 10, 15, 20, 30, 40, 45, 50]))


# ── work items ───────────────────────────────────────────────────────────────
TYPE_TAG = {
    "Epic": "type:epic", "Feature": "type:feature", "User Story": "type:story",
    "Bug": "type:bug", "Task": "type:task", "Spike": "type:spike",
}
items = []


def add(id_, title, wit, state, area, source=None, *, parent=None, closed=None, created=None,
        changed=None, tags=None, who=None, pts="auto", prs=None, sprint=None):
    """closed/created/changed are 'days before ANCHOR'. `tags` overrides the derived set."""
    if tags is None:
        tags = ["team:platform", TYPE_TAG[wit]]
        if area:
            tags.append(f"area:{area}")
        if source:
            tags.append(f"source:{source}")
    closed_at = rand_time(closed) if closed is not None else None
    if created is None:
        created = (closed + rng.randint(2, 21)) if closed is not None else rng.randint(4, 60)
    created_at = rand_time(created)
    if closed_at is not None and created_at >= closed_at:
        created_at = closed_at - dt.timedelta(days=2)
    changed_at = closed_at or rand_time(changed if changed is not None else rng.randint(0, 8))
    if pts == "auto":
        pts = None if wit in ("Epic", "Feature") else float(rng.choice([1, 2, 3, 5, 8]))
    if sprint is None:
        sprint = 24 - ((closed if closed is not None else 0) // 14)
    items.append({
        "assigned_to": who or rng.choice(PEOPLE),
        "changed_at": z(changed_at),
        "closed_at": z(closed_at) if closed_at else None,
        "created_at": z(created_at),
        "id": id_,
        "iteration_path": f"Platform\\Sprint {sprint}",
        "linked_pr_ids": [p["id"] for p in (prs or [])],
        "linked_prs": prs or [],
        "linked_repos": [],
        "parent_id": parent,
        "provider": "stub",
        "state": state,
        "story_points": pts,
        "tags": tags,
        "team": "Platform",
        "title": title,
        "url": f"https://stub.example/Platform/_workitems/edit/{id_}",
        "work_item_type": wit,
    })


PR_2001 = {"id": 2001, "is_draft": False, "status": "active",
           "url": "https://stub.example/Platform/_git/platform-core/pullrequest/2001"}

# Parents: three epics, six features (the recap groups children under these).
add(1100, "Kubernetes platform hardening", "Epic", "Active", "kubernetes", "roadmap", changed=2, created=70)
add(1101, "Developer self-service portal", "Epic", "Active", "idp", "roadmap", changed=1, created=64)
add(1102, "Observability and SLOs", "Epic", "Active", "observability", "roadmap", changed=3, created=58)
add(1110, "Cluster upgrade automation", "Feature", "Active", "kubernetes", "roadmap", parent=1100, changed=1, created=45)
add(1111, "Network policy rollout", "Feature", "Active", "networking", "roadmap", parent=1100, changed=2, created=40)
add(1112, "Golden-path service templates", "Feature", "Active", "idp", "roadmap", parent=1101, changed=1, created=38)
add(1113, "Alerting and on-call routing", "Feature", "Active", "observability", "roadmap", parent=1102, changed=2, created=36)
add(1114, "CI/CD pipeline standardisation", "Feature", "In Progress", "azuredevops", "roadmap", changed=3, created=33)
add(1115, "Secrets and access hygiene", "Feature", "Active", "security", "roadmap", changed=1, created=30)

# The original stub items. 1001-1003 deliberately trip the hygiene rules (untagged, missing
# type:*, disallowed "wip") so the Dashboard has three real flags to show.
add(1001, "Investigate flaky integration test", "Task", "New", None, tags=[], created=21, changed=1, who="Jordan Kim", pts=3.0)
add(1002, "Document the config import format", "Task", "In Progress", None,
    tags=["team:platform", "area:kubernetes"], created=17, changed=2, who="Alex Rivera", pts=3.0)
add(1003, "Spike: cache provider responses", "User Story", "Active", None,
    tags=["team:platform", "type:story", "wip"], created=19, changed=4, who="Sam Lee", pts=3.0)
add(1004, "Add retry/backoff to the poller", "Bug", "Active", None,
    tags=["team:platform", "type:bug", "priority:high"], parent=1005, created=12, changed=1,
    who="Jordan Kim", pts=3.0, prs=[PR_2001])
add(1005, "Ship the reports engine", "Feature", "New", None,
    tags=["team:platform", "type:feature", "area:frontend"], created=26, changed=3, who="Alex Rivera", pts=None)
add(1006, "Wire up the live-reload dev loop", "Task", "In Progress", None,
    tags=["team:platform", "type:task", "area:backend"], parent=1005, created=14, changed=5, who="Sam Lee", pts=3.0)
add(1007, "Migrate config to the database", "User Story", "Closed", "kubernetes", "roadmap", closed=2)
add(1008, "Fix node-pool autoscaling", "Bug", "Closed", "kubernetes", "incident", closed=5)
add(1009, "Add OpenTelemetry traces to the API", "Task", "Closed", "observability", "support", closed=8)
add(1010, "Roll out Argo CD to staging", "User Story", "Closed", "dev-platform", "roadmap", closed=11)
add(1011, "Self-service portal access request", "Feature", "Closed", "idp", "request", closed=14, parent=1101)
add(1012, "Standardise pipeline YAML templates", "Task", "Closed", "azuredevops", "support", closed=18, parent=1114)
add(1013, "Investigate cluster DNS latency", "Bug", "Closed", "networking", "incident", closed=22)
add(1014, "Dashboards for uptime SLOs", "User Story", "Closed", "observability", "roadmap", closed=26, parent=1102)

# Closed work in the last ~4 weeks, spread across nine areas.
CLOSED_RECENT = [
    # kubernetes
    (1201, "Upgrade staging clusters to Kubernetes 1.31", "User Story", "kubernetes", "roadmap", 1110, 3),
    (1202, "Automate node image patching", "Task", "kubernetes", "roadmap", 1110, 6),
    (1203, "Fix pod eviction during cluster upgrade", "Bug", "kubernetes", "incident", 1110, 9),
    (1204, "Pre-flight checks for control-plane upgrades", "User Story", "kubernetes", "roadmap", 1110, 13),
    (1205, "Right-size node pools for batch workloads", "Task", "kubernetes", "request", None, 16),
    (1206, "Spike: evaluate event-driven node autoscaling", "Spike", "kubernetes", "roadmap", None, 21),
    # networking
    (1220, "Default-deny network policies in staging", "User Story", "networking", "roadmap", 1111, 4),
    (1221, "Document egress allow-lists", "Task", "networking", "support", 1111, 10),
    (1222, "Fix intermittent DNS timeouts in the build pool", "Bug", "networking", "incident", None, 7),
    (1223, "Private endpoints for the artifact store", "User Story", "networking", "request", None, 19),
    (1224, "Spike: service mesh mTLS overhead", "Spike", "networking", "roadmap", None, 24),
    # observability
    (1230, "Burn-rate alerts for the API SLOs", "User Story", "observability", "roadmap", 1113, 2),
    (1231, "Route low-priority alerts to chat only", "Task", "observability", "support", 1113, 5),
    (1232, "Reduce alert noise from the poller", "Bug", "observability", "support", 1113, 12),
    (1233, "On-call handover dashboard", "User Story", "observability", "request", 1113, 17),
    (1234, "Trace sampling for the report engine", "Task", "observability", "roadmap", 1102, 9),
    (1235, "Fix missing labels on node metrics", "Bug", "observability", "incident", None, 15),
    (1236, "Log retention policy per environment", "User Story", "observability", "roadmap", None, 23),
    # idp
    (1240, "Service template: background worker", "User Story", "idp", "roadmap", 1112, 3),
    (1241, "Service template: scheduled job", "User Story", "idp", "roadmap", 1112, 8),
    (1242, "Scaffold CLI prints the next steps", "Task", "idp", "request", 1112, 11),
    (1243, "Catalog: show the owning team on service pages", "User Story", "idp", "request", 1101, 6),
    (1244, "Fix broken links in generated READMEs", "Bug", "idp", "support", 1112, 13),
    (1245, "Access request form validates repo names", "Task", "idp", "support", 1101, 20),
    (1246, "Spike: self-service environment TTLs", "Spike", "idp", "roadmap", 1101, 25),
    # azuredevops
    (1250, "Shared pipeline template for container builds", "User Story", "azuredevops", "roadmap", 1114, 3),
    (1251, "Cache dependencies across CI jobs", "Task", "azuredevops", "request", 1114, 7),
    (1252, "Fix flaky release-gate approvals", "Bug", "azuredevops", "incident", 1114, 10),
    (1253, "Migrate legacy classic pipelines (batch 3)", "Task", "azuredevops", "roadmap", 1114, 14),
    (1254, "Pipeline failure summary in chat", "User Story", "azuredevops", "request", 1114, 21),
    # security
    (1260, "Rotate service credentials automatically", "User Story", "security", "roadmap", 1115, 4),
    (1261, "Block privileged containers by policy", "User Story", "security", "roadmap", 1115, 9),
    (1262, "Patch critical CVE in the base image", "Bug", "security", "incident", None, 1),
    (1263, "Audit stale access grants", "Task", "security", "support", 1115, 16),
    (1264, "Spike: workload identity for CI", "Spike", "security", "roadmap", 1115, 22),
    # dev-platform / backend / frontend
    (1270, "Faster local dev loop with hot reload", "User Story", "dev-platform", "roadmap", None, 12),
    (1272, "Reproducible dev containers", "Task", "dev-platform", "request", None, 19),
    (1280, "Paginate the work-items endpoint", "User Story", "backend", "roadmap", None, 6),
    (1281, "Fix retry loop on provider 429 responses", "Bug", "backend", "incident", None, 3),
    (1290, "Keyboard navigation for the board view", "User Story", "frontend", "request", None, 8),
    (1291, "Fix column overflow on narrow screens", "Bug", "frontend", "support", None, 18),
]
for id_, title, wit, area, src, parent, closed in CLOSED_RECENT:
    add(id_, title, wit, "Closed", area, src, parent=parent, closed=closed)

# Older closed work: outside the default 30-day window, so only the "Last 60 / 90 days"
# choices pick it up - the window selector visibly does something.
CLOSED_OLDER = [
    (1300, "Replace the legacy ingress controller", "User Story", "kubernetes", "roadmap", 1100, 34),
    (1301, "Establish a quarterly SLO review", "Task", "observability", "roadmap", 1102, 41),
    (1302, "Move artifact feeds to the new registry", "Task", "azuredevops", "roadmap", None, 48),
    (1303, "Portal: first-run onboarding checklist", "User Story", "idp", "request", 1101, 57),
    (1304, "Fix certificate renewal alert gaps", "Bug", "observability", "incident", None, 63),
    (1305, "Spike: baseline cluster cost report", "Spike", "kubernetes", "roadmap", None, 72),
]
for id_, title, wit, area, src, parent, closed in CLOSED_OLDER:
    add(id_, title, wit, "Closed", area, src, parent=parent, closed=closed)

# Open work in flight.
add(1310, "Rate-limit the public API", "User Story", "New", "backend", "request", created=3, changed=1)
add(1311, "Rework the dashboard empty states", "Task", "In Progress", "frontend", "request", created=6, changed=1)
add(1312, "Canary releases for the portal", "User Story", "Active", "idp", "roadmap", parent=1112, changed=1)
add(1313, "Cost dashboard per namespace", "User Story", "New", "observability", "roadmap", created=5, changed=2)
add(1314, "Spike: replace the poller scheduler", "Spike", "Active", "backend", "roadmap", created=4, changed=2)
add(1315, "Fix sporadic 502s through the ingress", "Bug", "Active", "networking", "incident", created=1, changed=0)

items.sort(key=lambda i: i["id"])
ids = [i["id"] for i in items]
assert len(ids) == len(set(ids)), "duplicate work-item ids"
assert all(i["parent_id"] in (None, *ids) for i in items), "dangling parent_id"

# Closed items that count for the default 30-day recap must all fall inside it.
recent = [i for i in items if i["closed_at"] and i["id"] not in {x[0] for x in CLOSED_OLDER}]
assert all(dt.datetime.fromisoformat(i["closed_at"].replace("Z", "+00:00")) >= ts(28, 0) for i in recent)

# ── aggregates (computed from the items so the screens agree) ────────────────
def parse(s):
    return dt.datetime.fromisoformat(s.replace("Z", "+00:00"))


def tag_counts(rows):
    c = Counter()
    for it in rows:
        for t in (it["tags"] or ["(untagged)"]):
            c[t] += 1
    return [{"count": n, "tag": t} for t, n in sorted(c.items(), key=lambda kv: (-kv[1], kv[0]))]


range_to = ts(0, 23, 59, 59)
range_from = ts(30, 0, 0, 0)
closed_in_range = [i for i in items if i["closed_at"] and range_from <= parse(i["closed_at"]) <= range_to]
opened_in_range = [i for i in items if range_from <= parse(i["created_at"]) <= range_to]
last7 = range_to - dt.timedelta(days=7)
closed_7d = sum(1 for i in items if i["closed_at"] and parse(i["closed_at"]) > last7)
created_7d = sum(1 for i in items if parse(i["created_at"]) > last7)


def stat(name, value, percent=False):
    points = [{"label": "", "value": float(value)}] if value else []
    return {"name": name, "render": "stat",
            "series": [{"label": "Work items", "percent": percent, "points": points}]}


def write(name, data):
    (OUT / f"{name}.json").write_text(
        json.dumps(data, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n", encoding="utf-8")


def read(name):
    return json.loads((OUT / f"{name}.json").read_text(encoding="utf-8"))


flags = read("tickets")["flags"]  # the three hygiene flags belong to items 1001-1003, unchanged
write("tickets", {"flags": flags, "items": items})

write("reports-run-work-items-by-tag", {
    "name": "work-items-by-tag", "render": "bar",
    "series": [{"label": "Work items", "percent": False,
                "points": [{"label": c["tag"], "value": float(c["count"])} for c in tag_counts(items)]}]})
write("reports-run-work-items-closed-7d", stat("work-items-closed-7d", closed_7d))
write("reports-run-work-items-created-7d", stat("work-items-created-7d", created_7d))

pipelines_block = {"canceled": 1, "failed": 1, "from": z(range_from), "succeeded": 3,
                   "success_rate": 0.75, "to": z(range_to), "total_runs": 5}
write("reports", {
    "pipelines": pipelines_block,
    "range": {"from": z(range_from), "to": z(range_to)},
    "tickets": {"closed": len(closed_in_range), "closed_by_tag": tag_counts(closed_in_range),
                "from": z(range_from), "opened": len(opened_in_range), "to": z(range_to)}})


# Pipelines + PRs + dashboard: small hand-written set, dated relative to the anchor.
def iso(t):
    return t.isoformat()


pipelines = [
    {"failed": 0, "flags": [], "folder": "\\platform", "last_failure_at": None,
     "last_run_at": iso(ts(1, 9)), "last_run_url": "https://stub.example/Platform/_build/results?buildId=9001",
     "last_status": "succeeded", "name": "platform-ci", "pipeline_id": 10, "running": 0, "succeeded": 2,
     "team": "Platform", "url": "https://stub.example/Platform/_build?definitionId=10"},
    {"failed": 1, "flags": [], "folder": "\\platform", "last_failure_at": iso(ts(1, 9)),
     "last_run_at": iso(ts(1, 9)), "last_run_url": "https://stub.example/Platform/_build/results?buildId=9003",
     "last_status": "failed", "name": "platform-nightly", "pipeline_id": 11, "running": 0, "succeeded": 1,
     "team": "Platform", "url": "https://stub.example/Platform/_build?definitionId=11"},
    {"failed": 0, "flags": [], "folder": "\\platform", "last_failure_at": None, "last_run_at": None,
     "last_run_url": None, "last_status": None, "name": "platform-release", "pipeline_id": 12, "running": 0,
     "succeeded": 0, "team": "Platform", "url": "https://stub.example/Platform/_build?definitionId=12"},
]
write("pipelines", pipelines)

pull_requests = [
    {"author": "Alex Rivera", "created_at": z(ts(1, 9)), "id": 2001, "is_draft": False,
     "linked_work_items": [1004], "provider": "stub", "repository": "platform-core", "reviewer_count": 2,
     "source_branch": "refs/heads/feature/x", "status": "active", "target_branch": "refs/heads/main",
     "team": "Platform", "title": "Add retry/backoff to the poller",
     "url": "https://stub.example/Platform/_git/platform-core/pullrequest/2001"},
    {"author": "Sam Lee", "created_at": z(ts(2, 9)), "id": 2002, "is_draft": True, "provider": "stub",
     "repository": "platform-core", "reviewer_count": 2, "source_branch": "refs/heads/feature/x",
     "status": "active", "target_branch": "refs/heads/main", "team": "Platform", "title": "Bump dependencies",
     "url": "https://stub.example/Platform/_git/platform-core/pullrequest/2002"},
]
write("pull-requests", pull_requests)

dash = read("dashboard")
dash.update({"last_polled_at": ts(0, 15, 39, 30).isoformat(), "pipelines": pipelines,
             "total_work_items": len(items)})
write("dashboard", dash)

# Canned "AI" blurbs for the Recap: the public demo has no model, so the frontend serves
# these in place of the POST /recap/summaries call (see recapSummaries in lib/api.js).
RECAP_SUMMARIES = {
    "area:observability": "Observability took a big step forward: burn-rate alerts now guard the API SLOs, low-priority noise is routed to chat only, and a new on-call handover dashboard gives every shift a clear picture. Alert noise from the poller is down, so the signals that remain are the ones worth acting on.",
    "area:kubernetes": "Cluster upgrades are becoming routine. Staging moved to Kubernetes 1.31 with pre-flight checks guarding the control plane, node image patching is automated, and a pod-eviction bug that bit during upgrades is fixed. Batch workloads now run on right-sized node pools.",
    "area:idp": "Self-service got easier for every team. Background-worker and scheduled-job templates join the golden paths, service pages now show their owning team, and the scaffold CLI tells developers exactly what to do next. Broken README links and repo-name validation were tidied up along the way.",
    "area:azuredevops": "Pipelines are more consistent and less flaky: a shared container-build template is live, dependency caching trims CI time, flaky release-gate approvals are fixed, and another batch of classic pipelines has moved across. Failure summaries now land straight in chat.",
    "area:networking": "Network posture tightened without slowing anyone down. Default-deny policies are live in staging with documented egress allow-lists, intermittent DNS timeouts in the build pool are resolved, and the artifact store now sits behind private endpoints.",
    "area:security": "Security hygiene moved from policy to practice: service credentials rotate automatically, privileged containers are blocked by policy, stale access grants were audited, and a critical base-image CVE was patched within a day of disclosure.",
    "area:dev-platform": "Day-to-day developer experience improved with a faster hot-reload loop and reproducible dev containers, so a new contributor is productive in minutes rather than hours.",
    "area:backend": "The platform API got sturdier: the work-items endpoint is paginated for large backlogs and the retry loop that mishandled provider rate-limit responses is fixed.",
    "area:frontend": "The interface got friendlier: the board view is now fully keyboard-navigable and column overflow on narrow screens is fixed.",
}
write("recap-summaries", {"ai_available": True, "errors": [], "summaries": RECAP_SUMMARIES})

areas = Counter(t[5:] for i in closed_in_range for t in i["tags"] if t.startswith("area:"))
print(f"{len(items)} work items | {len(closed_in_range)} closed in the 30-day window "
      f"| {len(opened_in_range)} opened | closed 7d={closed_7d} created 7d={created_7d}")
print("closed by area (30d):", dict(areas.most_common()))
