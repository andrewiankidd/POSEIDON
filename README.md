# POSEIDEN

![POSEIDEN](assets/logo.png)

A support tool that keeps a team's backlog legible - tagged, current, and clear
to people who don't live in the work tracker.

Try it now: **[andrewiankidd.github.io/POSEIDEN](https://andrewiankidd.github.io/POSEIDEN/)**

## About

POSEIDEN polls your backlog, checks it against hygiene rules you define, and flags
what's drifting - untagged or stale items, missing required tags, failing
pipelines, pull requests with no linked work item. It rolls the result into a
dashboard and a few reports you can hand to a manager, lets you fix the worst
issues in place (edit a work item's State or Tags, or link a work item to a pull
request - the change is written straight back and the flags recompute), and runs a
built-in **Doctor** that self-checks the things that quietly break: sign-in,
connectivity, whether a newer build is out.

Run it as a local app or a shared multi-user server - same tool, same logic layer,
your choice.

## How it runs

POSEIDEN is one codebase, several shells - the same logic layer runs as a local
app or a shared server. Pick the mode that fits:

| Mode | What it is | Start here |
|------|-----------|-----------|
| **Hosted** (recommended) | The `poseiden-server` container as a shared, multi-user, multi-tenant instance - auth + per-owner data at the ingress. Run it locally on minikube, or on any container host. | [Deploy hosted](docs/RUNNING.md#hosted) |
| **Repointed client** | A desktop / mobile / web client pointed at a hosted instance (one Settings field). | [Repoint a client](docs/RUNNING.md#repointed-client) |
| **Standalone** | Desktop / mobile / CLI with its own embedded SQLite - zero external dependencies, polls your tracker directly. | [Run natively](docs/RUNNING.md#standalone) |
| **Portable** | Standalone, with every write confined beside the binary (`./.portable/`). Desktop OSes. | [Portable mode](docs/RUNNING.md#portable) |

## Getting started

### Deploy to a Kubernetes cluster

Install the Helm chart onto any cluster:

```bash
helm install poseiden deploy/helm/poseiden -f deploy/helm/poseiden/ci/minikube-dex-values.yaml
```

Those are the proof-of-concept values (bundled Dex login, plain HTTP). For real
clusters - nginx vs Istio, external auth providers, TLS, persistence, and importing
tenant config - see the [Helm chart README](deploy/helm/poseiden/README.md) and
[docs/RUNNING.md](docs/RUNNING.md#hosted).

### Run on your device (client or standalone)

Grab the latest build for your OS from
**[GitHub releases](https://github.com/andrewiankidd/POSEIDEN/releases/latest)** -
desktop app, Android, or the `poseiden` CLI. Run it standalone with its own local
database, or point it at a hosted instance from Settings (one field). See
[docs/RUNNING.md](docs/RUNNING.md#standalone).

### Run in development mode

The dev loop runs the whole multi-user stack locally on **minikube + Helm** - the
same chart and container you'd ship to a real cluster, seeded with the tenant
bundles in [`tenants/`](tenants/). A convenience script checks your tooling,
deploys, and imports the config for you:

```bash
./poseiden.sh up      # check deps -> minikube -> helm install -> import tenants/
./poseiden.sh down    # tear it all down
```

It tells you how to install anything missing (minikube / helm / kubectl). Full
walkthrough: **[docs/RUNNING.md](docs/RUNNING.md)**.

## Workspace layout

Multi-crate Cargo workspace; each crate has a narrow responsibility:

| Crate | Role |
|-------|------|
| [poseiden-core](crates/poseiden-core/) | Domain types + config schema. The shared wire schema across HTTP, Tauri IPC, and the CLI. IO-free. |
| [poseiden-telemetry](crates/poseiden-telemetry/) | Centralised, portable observability - console + rolling file + OTLP to Grafana. App-agnostic; reusable. |
| [poseiden-rules](crates/poseiden-rules/) | Hygiene engine - `evaluate(items, ruleset) → flags`. Pure, test-first. |
| [poseiden-reports](crates/poseiden-reports/) | Report engine - runs a `ReportSpec` over loaded datasets → `ReportResult`. Pure/IO-free; backs the Reports screen + home tiles. |
| [poseiden-ai](crates/poseiden-ai/) | Optional tag-suggestion engine - embedded on-device model (candle/Qwen2.5, CPU or CUDA) plus OpenAI-compatible online providers (Claude/OpenAI/Gemini). |
| [poseiden-providers](crates/poseiden-providers/) | `Provider` trait + Azure DevOps, GitHub, and GitLab clients (public repos read without a token). Fetch + normalise. |
| [poseiden-store](crates/poseiden-store/) | SQLite via `sqlx`, migrations embedded in the binary. Reports + snapshots. |
| [poseiden-paths](crates/poseiden-paths/) | Portable-first path resolution (portable → env → OS → default). |
| [poseiden-doctor](crates/poseiden-doctor/) | Self-checking health engine - registered checks, worst-wins traffic light, auto-fixes. |
| [poseiden-server](crates/poseiden-server/) | The shared `Service`, the poll scheduler, the axum API. Docker binary + Tauri library. |
| [poseiden-cli](crates/poseiden-cli/) | `poseiden` - `poll` / `lint` / `report` / `config` / `tag`. |
| [poseiden-app](crates/poseiden-app/) | Tauri desktop/mobile shell over the same `Service`. |

Frontend is plain HTML + JS ([`frontend/web/`](frontend/web/)) with dependency-free
SVG charts - no build step, self-contained for portable/offline use.

## Documentation

Start at the **[documentation hub](docs/README.md)** - the run + debug guide,
feature guides, configuration, deploy targets, scope, and project status all fan
out from there. Contributors: build principles + conventions live in
[CLAUDE.md](CLAUDE.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option (Rust ecosystem convention).
