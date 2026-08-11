# Changelog

All notable changes to POSEIDEN are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-08-11

Initial public release. POSEIDEN is a Product Owner support tool that keeps a
work-tracker backlog legible: it polls your tracker and CI on a schedule,
validates work items and pipelines against config-defined hygiene rules,
surfaces what needs attention, and reports flow over time. One Rust core, one
logic layer, three shells (hosted web, desktop, CLI).

### Providers
- Poll **Azure DevOps, GitHub, and GitLab** through a provider-agnostic core.
  Public GitHub / GitLab repos poll with no token; a token is optional (private
  repos or higher rate limits).
- Each provider normalises into the same `WorkItem` / `Pipeline` / `PullRequest`
  shapes (Azure DevOps work items + build pipelines + PRs; GitHub Issues +
  Actions + PRs; GitLab Issues + Pipelines + MRs), covered by captured-fixture
  and live smoke tests.
- A deterministic offline `stub` provider powers demo mode and the end-to-end
  tests.

### Hygiene rules
- Config-driven rules interpreted per team: required tags, allowed / disallowed
  tag lists (with `*` wildcard patterns), untagged detection, per-state
  staleness, a stale-state-tag check, and pull-request + pipeline checks.
- Per-team rule overrides that inherit the instance default.

### Tagging
- Advisory tag suggestions from a deterministic keyword + alias engine (no model
  needed) and an optional AI tagger; add / rewrite / remove chips, applied only
  by a person (POSEIDEN never auto-applies).
- AI backends: on-device embedded model (candle / Qwen2.5, CPU or CUDA),
  in-browser WebGPU, or a hosted provider (Claude / OpenAI / Gemini). Configured
  in a reorderable priority list.

### Views, reports, and Recap
- Dashboard, Work Items (sortable / filterable / inline-editable table), Pull
  Requests, Pipelines, Rules, Reports, and Recap.
- A configurable **report engine** (datasource + conditions + render type) that
  also drives the home velocity tiles; export to PDF / PNG / CSV.
- **Recap**: a shareable highlights deck generated from your closed work, grouped
  by `area:` / `source:` and internal vs external, exportable as a single
  self-contained HTML file.

### Write-back (Azure DevOps)
- User-initiated inline State / Tags edits, a bounded multi-select bulk apply,
  and work-item to pull-request links, each written through the provider. Write-
  back is Azure DevOps only today; GitHub / GitLab are read-focused.

### Deployment and distribution
- Ships as a slim Docker image, a Helm chart (nginx or Istio, oauth2-proxy,
  bundled Dex / Keycloak for a self-contained PoC), a single desktop / CLI
  binary, and a repointable web / mobile client.
- Portable mode confines every write beneath `./.portable/`.
- GitHub Pages landing page with a client-side demo mode (sample data, no
  backend).

### CLI
- `poseiden poll` / `lint` / `report` / `config` / `tag`. `lint` exits non-zero
  on error-severity flags for CI gating; `tag` generates advisory suggestions
  (build `--features cuda` for GPU tagging).

### Platform
- Multi-tenant owner scoping (identity from the `X-Auth-Request-Email` header,
  `default` owner when unauthenticated); per-owner teams, rules, tags, reports.
- Native device-code OAuth sign-in for Azure DevOps (no `az` CLI dependency).
- A self-checking Doctor (traffic-light health, per-provider access checks,
  update check) and centralised telemetry (console, rolling file, OTLP).

[0.1.0]: https://github.com/andrewiankidd/POSEIDEN/releases/tag/v0.1.0
