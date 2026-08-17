# Roadmap

Committed direction - the near-term work we intend to do, in rough order. Ideas
not yet committed live in [BACKLOG.md](BACKLOG.md).

## Now (PoC - landed)

The day-one feature set is in place; see [PROJECT_STATUS.md](PROJECT_STATUS.md):
polling across Azure DevOps, GitHub, and GitLab, config-driven hygiene rules with
per-team overrides, dashboard / work items / pull requests / pipelines / reports /
doctor views, inline work-item State/Tags write-back and work-item↔PR links
(Azure DevOps), Azure DevOps PAT and native device-code OAuth sign-in (GitHub /
GitLab use an optional token, none for public repos), AI + deterministic tag
suggestions, telemetry (OTLP +
rolling file), CLI (`poll`/`lint`/`report`/`config`/`tag`),
Docker image, Helm chart, portable mode, and the standalone-or-repointed client
model.

## Next

1. **Harden against live orgs.** POSEIDON has been run against a real Azure DevOps
   org (polling + write-backs exercised end-to-end); the remaining work is
   tuning the default WIQL + field list and validating rule outcomes across more
   projects. Polling needs read scopes; the write-back features need Work Items
   Write.
2. **In-app demo mode.** The `stub` provider + demo tenant bundle
   (`tenants/demo-data.poseidon.import.yaml`) already give a deterministic offline
   dataset (used for the e2e + documentation screenshots); the remaining piece is a
   built-in one-click `--demo` mode so the UI can be evaluated without importing anything.
3. **Sprint / iteration view.** The first view beyond the day-one set - the data
   (`iteration_path`) is already normalised.
4. **Assignee load view.** Second new view; `assigned_to` already captured.
5. **Historical snapshots.** Daily backlog snapshots - the foundation for trend
   reporting and the standup helper.

## After

- **Multi-user auth** (real `owner` ids; per-user boards/tags/views) once the
  hosted instance graduates past the Istio-gated PoC.
- **Postgres store** for shared, scalable deployments.
- **More providers.** GitHub and GitLab already ship alongside Azure DevOps
  (Issues/PRs/Actions and Issues/MRs/Pipelines respectively; public repos read
  without a token, covered by fixture + live smoke tests). Jira / Linear are next,
  each a new `impl Provider` into the same core shapes. Extending write-back
  beyond Azure DevOps to the GitHub / GitLab providers is a separate, later step.

Sequencing is deliberate: validate against reality first, add views over data we
already collect, then take on the bigger subsystems (snapshots → auth →
Postgres → further providers).
