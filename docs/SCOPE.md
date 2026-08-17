# Scope

What POSEIDON deliberately **is not**. Keeping this explicit is what keeps the
product focused and avoids competing with the workflow tools teams already run.

## Visibility first, not a workflow replacement

POSEIDON's job is to make the backlog and its delivery legible - surface what
needs attention and report on flow - and to let you resolve the highest-value
hygiene issues in place via a small, explicit set of write-backs. It is primarily
a lens over the tools your teams already run, not a wholesale replacement. What it
deliberately does not try to be:

- **Not a build/release controller.** It reports pipeline health; queuing,
  cancelling, and retrying runs stay in your CI tool.
- **Not a full board editor.** It flags untagged / stale / mis-tagged items and
  lets you fix the highest-value ones in place - inline State/Tags edits, a
  hand-picked multi-select "bulk retag" of those same fields (add/remove a tag,
  set state on the rows *you* ticked), and work-item↔PR links - each user-initiated,
  explicit, and clearly scoped, never a silent side effect of observation. The line
  it holds is against *automated / policy-driven* mass mutation: it will not
  auto-apply its own tag suggestions, sweep the whole backlog on a rule, reassign
  owners, or nudge assignees - those stay in your tracker. A person selecting rows
  and clicking apply is an extension of the single-item edit, not a board robot.
- **Not a messaging bot.** A future notification digest (see the backlog)
  produces a summary a human sends; POSEIDON doesn't post to Slack/Teams/email as
  an actor on your behalf. Any delivery is opt-in and bounded.
- **Not a sprint-planning tool or PR reviewer.** It's a hygiene + flow lens, not a
  replacement for the tools those jobs live in.

Keeping the surface small keeps the security footprint minimal (read scopes cover
polling across every provider - public GitHub / GitLab repos need no token at all;
only the opt-in Azure DevOps write-back features add a Work Items Write
requirement) and the product complementary rather than competitive.

## Other non-goals

- **No proprietary auth model in the PoC.** Authentication is a mesh concern
  (Istio); POSEIDON ships no login. Multi-user support is a *future* additive
  layer (see CLAUDE.md), not a reason to build a bespoke auth system now.
- **No horizontal scaling of the SQLite instance.** POSEIDON is a single-writer
  service. Scale is the Postgres-store swap, not more replicas against one DB
  file.
- **No bundled analytics/telemetry.** POSEIDON doesn't phone home. What it stores
  about your backlog stays in your SQLite file.
- **Not a data warehouse.** Historical snapshots (a backlog item) are for retro
  questions, not a general BI substrate. Point Grafana/Metabase at the DB if you
  want that.
