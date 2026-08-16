# POSEIDEN Backlog

Ideas and directions worth not forgetting - not commitments. See
[ROADMAP.md](ROADMAP.md) for what's crossed the line into "we're doing this,"
and [PROJECT_STATUS.md](PROJECT_STATUS.md) for what already works.

## Views & analysis

- **Sprint / iteration view.** Group work items by current iteration; burndown;
  flag anything in-progress longer than the iteration length. (`iteration_path`
  is already normalised onto `WorkItem` - the data is there.)
- **Assignee load view.** Work items per person; flag overload; spot bottlenecks.
  (`assigned_to` is already captured.)
- **Stale item detection, richer.** Beyond per-state day limits: "In Progress
  > 5 days *without a linked commit* is stale." Needs commit linkage (PR linkage
  now ships).
- **Blocked-chain visualisation.** Show dependency chains; highlight where a
  block propagates downstream. Relations are already fetched (`$expand=relations`,
  used today for PR links); this needs the non-PR relation graph surfaced.
- **Standup helper.** A one-page "what changed since yesterday" per assignee,
  ready to paste. Reads the daily snapshot diff (below).
- **Custom queries as saved views.** WIQL-style queries surfaced as UI tabs, so a
  team can pin their own slices alongside the built-in views.

## Editing & write-back

Inline State + Tags write-back to Azure DevOps ships (State/Tags editors + WI↔PR
link chips, via `Service::update_work_item` / `link_work_item_pr`). Remaining:

- **Assignee inline edit.** The Assignee column is read-only today; make it an
  inline picker that writes `assigned_to` back through the same write path (needs
  Work Items Write).
- **Team-edit field round-trip (verify).** `update_team` replaces with a full
  `TeamConfig`, so confirm the team-edit *form* sends back every field it didn't
  touch - `area_path_strict`, `tenant`, the `[team.rules]` override - instead of
  dropping them on save. A round-trip test plus a manual check.

## Reporting depth

- **Cost / effort tracking.** Story points / estimates, planned vs delivered per
  iteration. (`story_points` is already normalised - needs the report + chart.)
- **Historical snapshots.** Daily backlog snapshots persisted so retro questions
  ("how many items were untagged three weeks ago?") are answerable. A new table
  keyed by snapshot date; the report engine reads across snapshots.
- **Trend lines.** Flag counts + success rate over time, not just point-in-time -
  needs the snapshots above.
- **CLI report parity.** `poseiden report` still prints the fixed flow summary
  (`Service::ticket_report`); migrate it onto the configurable report engine the
  GUI uses (`/reports/specs` + `run`), so CLI and GUI reports come from one place.

## Integrations

- **WI-side PR hygiene flags.** PR linkage itself now ships (WI↔PR link chips + a
  PR-side require-work-item flag). Remaining are the work-item-side checks: flag
  done work items with no merged PR, and in-progress work items with no branch/PR
  activity.
- **PR checks + comments columns** *(assessed, deferred)*. Surface CI check state
  and comment counts as Pull Request columns. Parked as lower-value than the
  write-back + view work; kept here so the decision isn't re-litigated from scratch.
- **Notification digest.** Daily/weekly summary (flagged items, failing
  pipelines) as an email or Slack/Teams message. POSEIDEN produces the summary;
  delivery is an explicit, opt-in, bounded capability
  (see SCOPE.md - POSEIDEN is not a general messaging actor).
- **More providers.** Jira, Linear next (GitHub and GitLab already ship). Each is
  a new `impl Provider`; the rules engine, store, server, and frontend are
  unchanged.
- **IDP / service-catalog lookup for product tagging.** Linked-repo→tag matching
  ships as a config-driven map (`repo_tags`), but the repo→service→product graph
  is really owned by an internal developer portal (e.g. Port). An optional catalog
  integration would pull that mapping live and resolve `product:*` from the repos an
  item touches - no hand-maintained map, and the allowed product set could be
  derived from the catalog rather than hand-listed. The static `repo_tags` map stays
  as the offline fallback. Deferred: seeding the map from a one-off catalog export
  covers the near-term need without the integration.
- **Write-back beyond Azure DevOps.** Inline State/Tags edits and WI↔PR links
  currently write through the Azure DevOps provider only; the GitHub and GitLab
  providers are read-focused. Extending the write path (GitHub issue state /
  labels, GitLab issue state / labels) is a later step behind the same explicit,
  user-initiated write-back contract.

## Platform & delivery

- **iOS build in CI.** The release matrix builds desktop + Android today; wire an
  iOS build (simulator first, then signed) once signing certs exist. The primary
  mobile story is a *repointed* client, so this is lower priority.
- **Postgres store.** Second `impl` of the `Store` trait for teams that want a
  shared, horizontally-scalable backend. Unlocks multi-replica web.
- **Multi-user auth.** Real user ids replacing `DEFAULT_OWNER`; per-user boards,
  tags, and saved views. The `owner` column already exists on every table, so
  this is additive. Gate remains Istio at the edge; app-level identity layers on
  top.
- **Register a dedicated Entra app for device-code** *(hardening)*. Native
  device-code sign-in now ships (pure-HTTP OAuth in `poseiden-providers::oauth`,
  no `az`), reusing the Azure CLI's public client id. Register a multi-tenant
  POSEIDEN public client (allow public-client flows + Azure DevOps
  `user_impersonation`) so it doesn't piggyback on Microsoft's client and survives
  tenants whose Conditional Access restricts the az client. One-time, publisher-side.
- **Per-user ADO identity via OIDC token passthrough** *(deferred)*. Pass the
  signed-in user's token through to Azure DevOps for true per-user identity.
  Shelved in favour of the isolated per-owner device-code sessions
  (`AZURE_CONFIG_DIR` per owner); revisit if that isolation proves insufficient.
- **Native chart library option.** The frontend ships dependency-free SVG charts
  (portable-first). If richer interactivity is wanted, chart.js is a drop-in - the
  report DTOs already carry everything it needs.

## Hygiene rules depth

- **Field-level rule-override merge.** Whole-ruleset per-team overrides now ship
  (`[team.rules]` on a `[[team]]`, via `PoseidenConfig::rules_for`). Remaining: let
  a team override *individual* rule fields and inherit the rest, rather than
  replacing the whole ruleset.
- **Field-completeness rules.** Flag items missing an estimate, an area path, or
  an acceptance-criteria field - configurable required fields, not just tags.
- **Regex / composite tag rules.** Beyond prefix wildcards: "exactly one `type:`
  tag," "if `type:bug` then `severity:*` required."
