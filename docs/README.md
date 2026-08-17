# POSEIDON documentation

POSEIDON is a Product Owner support tool that keeps a backlog legible - hygiene
rules, pipeline health, and flow reporting across Azure DevOps, GitHub, and
GitLab (the provider layer is a trait, so other trackers are a plug-in away). It
runs as a local app or a shared server from **one codebase**; this hub is the map
to running, configuring, and extending it.

New here? The [root README](../README.md) has the pitch and a 60-second start.
This page is the index to everything deeper.

## How it runs

The same logic layer ships in several shells. Every deployment is one of these
modes - the [Running & debugging guide](RUNNING.md) covers each with copy-paste
commands.

| Mode | What it is | Credential model |
|------|-----------|------------------|
| **Standalone** | Desktop / mobile / CLI with embedded SQLite. Zero external dependencies; single tenant (`default` owner). | Azure DevOps: device-code sign-in or a PAT env var. GitHub / GitLab: public repos poll with no sign-in; an optional token env var covers private repos or higher rate limits. |
| **Portable** | Standalone with all state confined under `./.portable/` beside the binary (desktop OSes). | Same as standalone. |
| **Hosted** | The `poseidon-server` container as a shared, multi-user instance. Owner comes from the `X-Auth-Request-Email` header; auth is done at the ingress (oauth2-proxy), not in the app. | Azure DevOps: per-owner device-code sign-in (isolated token cache) or a shared PAT. GitHub / GitLab: optional token env var (none needed for public repos). |
| **Repointed client** | A desktop / mobile / web client pointed at a hosted instance via one Settings field (an instance URL). | The hosted instance's. |

The standalone and hosted shells are the *same* `Service`; the frontend picks
its backend at runtime (`mode()` in `frontend/web/lib/api.js`): an instance URL
in settings → talk to the remote; none → the embedded/same-origin instance.

## Architecture

A multi-crate Cargo workspace: a pure core + one logic layer (`Service`) behind
three transports (HTTP, Tauri IPC, CLI), so the web instance, the desktop app,
and the CLI can never drift. The crate-by-crate breakdown is in the
[root README](../README.md#workspace-layout); the design principles behind it
are in [CLAUDE.md](../CLAUDE.md).

## Repository layout

```
crates/         Rust workspace members - the multi-crate app (see the root README table)
frontend/web/   Dependency-free HTML + JS frontend; no build step, embedded for desktop
deploy/helm/    Kubernetes Helm chart (nginx / Istio, auth, bundled IdPs)
docs/           This documentation
assets/         Logo + shared images
tools/          Dev + demo helpers (e.g. the packaged demo bundle)
Dockerfile      Multi-stage prod image + a `dev` live-reload stage
compose.yaml    Docker-only dev loop (+ an opt-in `telemetry` profile)
```

**Why `crates/`, not `src/`:** in Cargo a workspace groups its member crates
under `crates/` - this is the Rust convention, the equivalent of a `src/` full of
projects in .NET. `src/` in Rust is a *single* crate's source. The repo root
holds only the manifests and tool config that must live there (`Cargo.toml`,
`rust-toolchain.toml`, `Dockerfile`, `compose.yaml`, licences); no application
code sits at the root.

## Reference

**Run & operate**
- [Running & debugging](RUNNING.md) - every run mode, the Docker-only dev loop, and troubleshooting playbooks.
- [Setup](features/setup.md) - sign-in, teams, and first-run configuration.
- [CLI guide](CLI.md) - `poll` / `lint` / `report` / `config` / `tag`, with worked use cases (CI gating, portable/air-gapped, machine output).
- [Distribution & deployment](DISTRIBUTION.md) - every publish/deploy target (Pages, Cloudflare, Docker/GHCR, Helm, desktop, mobile) and how each is built.
- [Helm chart](../deploy/helm/poseidon/README.md) - the Kubernetes chart: nginx vs Istio, auth providers, bundled IdPs, multi-tenant notes.

**Use the app**
- [Feature guides](features/README.md) - Dashboard, Work Items, Pull Requests, Pipelines, Reports, Rules - GUI and CLI side by side.

**Project**
- [Project status](PROJECT_STATUS.md) - what works today.
- [Compatibility](COMPATIBILITY.md) - which features are supported on which platforms.
- [Roadmap](ROADMAP.md) - committed next steps.
- [Backlog](BACKLOG.md) - everything else worth not forgetting.
- [Scope](SCOPE.md) - what POSEIDON deliberately is *not*.
- [CLAUDE.md](../CLAUDE.md) - build principles + conventions for contributors.

## Next steps

- **Just want to see it?** [Run it standalone](RUNNING.md#standalone) - one `cargo run`, no server.
- **Hosting for a team?** Start with the [minikube walkthrough](RUNNING.md#hosted), then the [Helm chart](../deploy/helm/poseidon/README.md) for a real cluster.
- **Contributing?** [Develop with Docker](RUNNING.md#develop-with-docker) (no Rust install), then read [CLAUDE.md](../CLAUDE.md).
