# Running & debugging

Every way to run POSEIDON, ranked by how faithfully it mirrors a real
deployment, plus the Docker-only development loop and a troubleshooting index.

POSEIDON is **one codebase, several shells**. The list below isn't "pick your
favourite" - each entry is a different *product scenario*:

| # | Mode | Mirrors | Inner loop |
|---|------|---------|-----------|
| 1 | [Hosted: minikube + Helm](#hosted) | The real shared, multi-user deployment (auth, ingress, per-owner data). | Slow (rebuild image → redeploy) |
| 2 | [Hosted: single container](#hosted-single-container) | A hosted instance on any container host (Swarm, a VPS, Azure Web Apps). | Slow (rebuild image) |
| 3 | [Standalone (native)](#standalone) | The real local app a user installs (desktop / mobile / CLI). | Fast |
| - | [Develop with Docker](#develop-with-docker) | Build vehicle, not a deploy target - iterate on the server with **only Docker installed**. | Fast (live reload) |

**[minikube + Helm](#hosted) is the recommended path** - it runs the real
multi-user, multi-tenant deployment locally, the same artifact (Helm chart +
container) you'd ship to a cluster. [Standalone](#standalone) is the
zero-dependency local app a single user installs; [Develop with
Docker](#develop-with-docker) is the fast contributor loop with only Docker
installed. (`docker compose` is a build vehicle, not a deploy target - it isn't a
recommended way to *run* POSEIDON.)

Every mode reads the same [configuration](#configuration) and uses the same
per-provider [credential model](#authentication).

---

## Hosted

The closest 1:1 to a production deployment: the `poseidon-server` container
behind an ingress that runs the OAuth flow and injects the user's identity, with
each user getting their own teams, rules, and data. Run the whole thing locally
on minikube.

**One command:** [`poseidon.sh`](../poseidon.sh) does the whole thing - checks your
tooling, starts minikube, installs the chart, and imports every bundle in
[`tenants/`](../tenants/):

```bash
./poseidon.sh up       # deps check -> minikube -> helm install -> import tenants/
./poseidon.sh dev      # live-reload loop (skaffold): rebuild + redeploy on save
./poseidon.sh status   # show what's running
./poseidon.sh down     # tear it down  (--clean also stops minikube)
./poseidon.sh help     # the full command list
```

`up` is the one-shot deploy; **`dev`** is the fast inner loop for iterating on the
*deployed* stack - skaffold rebuilds the image and redeploys on save. To iterate
on app *logic* without a cluster at all, prefer
[Develop with Docker](#develop-with-docker) or [Standalone](#standalone) instead;
the stack self-check `verify` is covered under
[Tenants and importing config](#tenants-and-importing-config).

`poseidon.sh` is also the project's build entrypoint - thin wrappers over the same
commands CI runs (see [DISTRIBUTION.md](DISTRIBUTION.md)), so you can produce any
artifact locally to dogfood a build:

```bash
./poseidon.sh build cli       # release CLI binary (target/release/poseidon)
./poseidon.sh build desktop   # Tauri desktop app (installer per OS)
./poseidon.sh build apk       # Tauri Android APK (needs android init + SDK/NDK)
./poseidon.sh build image     # Docker server image
```

The rest of this section is what the stack commands automate, step by step.

**Prerequisites:** Docker, [minikube](https://minikube.sigs.k8s.io/),
[Helm](https://helm.sh/), `kubectl`.

```bash
# 1. Start a cluster with an ingress controller.
minikube start
minikube addons enable ingress

# 2. Install POSEIDON with the bundled-Dex PoC values (nginx + an in-cluster
#    identity provider, no external accounts needed).
helm install poseidon deploy/helm/poseidon -f deploy/helm/poseidon/ci/minikube-dex-values.yaml

# 3. Point a hostname at the cluster and open it.
echo "$(minikube ip) poseidon.localhost" | sudo tee -a /etc/hosts
#    → http://poseidon.localhost   (log in: admin@example.com / password)
```

The image the chart runs is built from the repo [`Dockerfile`](../Dockerfile);
to iterate on server code you rebuild it and load it into minikube
(`minikube image load ...`) or push to a registry, then `helm upgrade`. That's
the slow inner loop - for developing *app logic*, prefer
[Develop with Docker](#develop-with-docker) or [Standalone](#standalone), and use
minikube to validate the *auth + deployment* layer.

**Going further:** real clusters, nginx vs Istio, the auth providers, the
bundled-IdP split-horizon setup, and the multi-tenant model are all in the
**[Helm chart README](../deploy/helm/poseidon/README.md)**. Publishing/other
targets are in [DISTRIBUTION.md](DISTRIBUTION.md).

### Tenants and importing config

The minikube command above uses the bundled-Dex values, so **auth is on and the
instance is multi-tenant**: oauth2-proxy injects each logged-in user's email and
the app maps it to a per-user `owner`. Everyone gets their own teams, rules, tags,
and reports; nobody sees anyone else's. (Run without auth - the single container,
or `auth.enabled=false` - and every request is the one `default` owner.)

A fresh install starts with an **empty** database - no teams until a tenant is
seeded. Config lives in portable bundles under [`tenants/`](../tenants/):
`demo-data.poseidon.import.yaml` (the committed offline demo tenant, backed by the
`stub` provider - no Azure needed) and your own gitignored `*.poseidon.import.yaml`
(see [tenants/README.md](../tenants/README.md)). Three ways to import one:

1. **In the app** - log in (the Dex PoC seeds `admin@example.com` / `password`),
   then **Settings → Configuration → Import** a bundle (or add teams by hand).
   This seeds *your* owner - the tenant you're logged in as.

2. **Over HTTP (headless / CI / scripting)** - port-forward straight to the pod
   (bypassing the proxy) and inject the identity header yourself, so you choose
   which owner to seed:

   ```bash
   kubectl port-forward deploy/poseidon 8737:8737 &
   curl -X POST "http://127.0.0.1:8737/api/config/import?replace=true" \
     -H "X-Auth-Request-Email: you@example.com" \
     -H "Content-Type: application/x-yaml" \
     --data-binary @tenants/demo-data.poseidon.import.yaml
   curl -X POST "http://127.0.0.1:8737/api/poll" -H "X-Auth-Request-Email: you@example.com"
   ```

3. **In the pod (the `default` owner)** - the CLI inside the pod carries no
   identity header, so it writes the `default` tenant:

   ```bash
   kubectl exec -i deploy/poseidon -- poseidon config import --replace - \
     < tenants/demo-data.poseidon.import.yaml
   ```

The **stack check** does exactly this: **`./poseidon.sh verify`** deploys an
isolated release, imports the demo bundle under a test owner, asserts the exact
stub counts, and checks that a second owner sees nothing - so the deploy, the
import path, and multi-tenant isolation are all covered by one headless command.
(To exercise a real *client* against the server, enable the chart's `localhost`
mode - it deploys a web client pointed at the server, no native app needed.)

> The chart does **not** auto-seed the demo tenant on install (a fresh instance is
> intentionally empty). Seeding it automatically - e.g. under the `admin@example.com`
> PoC login - could be a chart option; ask if you want it literally on by default.

### Hosted: single container

The chart is just a container, so any orchestrator works - Docker Swarm, a plain
VPS, Azure Web Apps. The published image is on GHCR:

```bash
docker run -p 8737:8737 \
  -v poseidon-data:/data \
  -e POSEIDON_AZURE_PAT="$POSEIDON_AZURE_PAT" \
  ghcr.io/andrewiankidd/poseidon:latest-main
```

- `/data` is the writable volume - SQLite DB, logs, and per-owner sign-in
  sessions land here. Back it with a persistent volume.
- With no ingress in front, there's no `X-Auth-Request-Email` header, so every
  request is the single `default` owner. Add your own auth proxy (or use the
  Helm chart) to get multi-user.
- Per-user device-code sign-in is a native OAuth flow built into the server -
  nothing extra ships in the image. A shared PAT via env is the simpler
  alternative. See [Authentication](#authentication).

---

## Standalone

The real local app: desktop / mobile / CLI, each embedding the `Service` +
SQLite + background poller in-process. No server, no Docker, no network beyond
your work tracker. Single tenant (the `default` owner).

Until tagged installers land on the Releases page, run from a clone. Needs
[Rust](https://rustup.rs) (desktop additionally needs the
[Tauri prerequisites](https://tauri.app/start/prerequisites/)).

**Desktop app** (the full GUI in a native window):

```bash
cargo tauri dev --config crates/poseidon-app/tauri.conf.json
```

Sign in from the app: **Sign in with Azure** runs a native device-code OAuth
flow built into the app - no PAT to paste, no app registration, no `az` CLI.

**CLI** (`poll` / `lint` / `report` / `config` / `tag` - same engine, same local
database):

```bash
cargo run -p poseidon-cli -- poll     # poll every configured team once
cargo run -p poseidon-cli -- lint     # print hygiene flags; exits 1 on any error
cargo run -p poseidon-cli -- report --from 2026-07-01 --to 2026-07-31
```

`poseidon lint` exits non-zero on any error-severity flag - drop it into a CI
stage to gate a pipeline on backlog hygiene. Full reference + worked use cases:
[CLI.md](CLI.md).

**Web server, natively** (the hosted binary without a container - handy for
poking at the API):

```bash
cargo run -p poseidon-server     # serves the API + frontend on :8737
```

### Portable

Standalone, with **every write confined beside the binary** under `./.portable/`
(DB, logs, cache) - nothing touches your home directory. Desktop OSes only
(iOS/Android sandbox their own dirs). Drop a `.portable` file next to the binary,
or set the env var:

```bash
POSEIDON_PORTABLE_MODE=true cargo run -p poseidon-cli -- poll
```

Config then resolves from the portable dir first. See
[CLI.md → portable / air-gapped run](CLI.md).

### Repointed client

Any client (desktop / mobile / web) can run as a **thin client to a hosted
instance** instead of its own embedded one: open **Settings**, set the instance
URL. The frontend's `mode()` switches to `remote` and every call goes to that
server; clear the field to go back to the embedded instance. Same app, same UI -
only the backend moves. (Installing the Android APK and repointing it is the
whole "mobile story"; see [DISTRIBUTION.md](DISTRIBUTION.md).)

---

## Develop with Docker

The **build vehicle**, not a deploy target: iterate on the server with **only
Docker installed** - no Rust toolchain on the host.

```bash
docker compose up
```

This builds the [`dev` stage](../Dockerfile) (Rust + `cargo-watch`) and runs the
web instance on <http://localhost:8737> with recompile-on-save. The working tree
is bind-mounted; the Cargo registry and build cache are named volumes so rebuilds
stay fast and don't thrash the host filesystem. See [`compose.yaml`](../compose.yaml).

- **Backend changes** (any crate) → recompile + auto-restart, a few seconds once
  warm.
- **Frontend changes** (`frontend/web/*.js`) → **instant**; the server serves them
  live from the mount, just refresh the browser. No recompile.
- **Run the CLI in the same image:**
  `docker compose run --rm dev cargo run -p poseidon-cli -- lint`.

First run compiles the whole workspace (minutes); subsequent runs reuse the
cached `target` volume. No config file to set up - it boots empty; add a team in
the app (or `poseidon config import`) once it's up.

**Windows/macOS note:** the compose command uses `cargo watch --poll` because
inotify file events don't cross the Docker Desktop / WSL2 mount boundary
reliably. Slightly higher idle CPU, but saves actually trigger rebuilds.

**Scope:** this covers the **web server + CLI**. The desktop/mobile GUI (Tauri)
isn't run through Docker - GUI-in-container needs display forwarding and isn't
worth it; build those [natively](#standalone).

---

## Configuration

**There is no config file.** Configuration splits by concern:

- **Instance settings** come from **environment variables**: `POSEIDON_BIND_ADDR`,
  `POSEIDON_PORT` (default `8737`), `POSEIDON_POLL_INTERVAL`, plus telemetry
  (`POSEIDON_OTLP_ENDPOINT`, `POSEIDON_LOG_CONSOLE`/`_FILE`/`_LEVEL`, `RUST_LOG`).
  The container image and Helm chart set these.
- **Per-owner config** (teams, rules, tags, saved reports) lives in the **DB**,
  keyed by owner. Set it in the app's Settings, or declaratively with **config
  import/export** below. A fresh instance starts empty.

### Import / export (backup · share · GitOps)

Config is portable YAML - back it up, share a team's setup, migrate standalone
↔ hosted, or seed a headless run:

```bash
poseidon config export --out config.yaml        # dump current config
poseidon config import config.yaml               # merge (add what's missing)
poseidon config import config.yaml --replace     # overwrite (declarative)
poseidon config export | ...                     # stdout if --out is omitted
```

Same in the app (Settings → Configuration) and over HTTP
(`GET`/`POST /api/config/export`|`import`). Secrets are never included - the PAT
stays in the environment. Full field reference: [features/setup.md](features/setup.md).

## Authentication

**Secrets never go in config or on a command line.** The credential model is
per provider.

**Azure DevOps** - two ways in:

- **Interactive (device code)** - **Sign in with Azure** in the desktop or web
  app runs a native device-code OAuth flow built into the server (no `az` CLI, no
  extra binary in the image). On a hosted instance each owner's session is
  isolated on the data volume, so users never share a token cache.
- **Headless / CI (PAT)** - config names the *environment variable* that holds a
  Personal Access Token (default `POSEIDON_AZURE_PAT`), never the token itself;
  your secret manager or shell sets it. Read-only scopes are enough for polling:
  Work Items (Read) + Build (Read). A PAT, if present, always wins over an
  interactive sign-in.

**GitHub / GitLab** - no sign-in flow. Public repositories poll anonymously; for
private repos or higher rate limits, config names an *environment variable*
holding a token (a GitHub PAT / fine-grained token, or a GitLab access token).
Device-code sign-in is an Azure DevOps concept and does not apply here.

---

## Troubleshooting

**"Not signed in" / polls fail with a credential error.**
No usable credential for a team. Either set the PAT env var named in that team's
`auth.pat_env`, or sign in (Doctor → **Sign in**, or the desktop app's button).
On a hosted instance a user must complete their *own* device-code sign-in (or the
deployment must set a shared PAT).

**Device-code sign-in fails in a container.**
Sign-in is a native OAuth flow inside the server - there's no `az` CLI to install
or put on `PATH`. If the device-code step can't reach Microsoft's endpoints,
check the container's outbound network / proxy, or switch that deployment to a
PAT.

**Frontend edits don't show up.**
- *Desktop app:* the frontend is embedded at build time (`build.rs`), so a rebuild
  is needed. The build script has a `rerun-if-changed` on `frontend/web`, so a
  normal `cargo tauri dev` rebuild picks it up.
- *Server / dev loop:* the server reads `POSEIDON_STATIC_DIR` at runtime; edits to
  the mounted `frontend/web` are live - just refresh. If they're not, check the
  container's `POSEIDON_STATIC_DIR` points at the bind-mount.

**Hosted login redirects in a loop (bundled Dex/Keycloak).**
A known nginx-ingress trap: auth annotations apply to *all* paths on an Ingress,
so the auth endpoints get challenged too. The chart fixes this with a separate,
annotation-free Ingress for `/oauth2` `/dex` `/auth` - if you've customised the
ingress, keep that split. Details in the
[Helm README](../deploy/helm/poseidon/README.md).

**Where's my data / logs / config?**
Under the data root: `poseidon.db`, `logs/`, `cache/`, and hosted `az-sessions/`.
Root resolution: portable (`./.portable/`) → `POSEIDON_DATA_DIR` (container:
`/data`) → OS app-data dir. Logs also stream to the console (`RUST_LOG`, default
`info`).

**A frontend (webview) error I can't see in the browser.**
Uncaught client errors are forwarded to the backend log stream under the
`poseidon_client` target (via `POST /api/client-error`), so they show up in the
server logs alongside backend errors.

**Is anything actually broken?**
The in-app **Doctor** (traffic-light indicator + panel) self-checks sign-in,
connectivity, and whether a newer build is out, with one-click fixes. Start there.

**Observability / traces + metrics.**
POSEIDON emits structured logs, traces, and metrics via
[`poseidon-telemetry`](../crates/poseidon-telemetry/). For a local Grafana LGTM
stack (an opt-in profile on the same compose file):

```bash
docker compose --profile telemetry up -d lgtm
```

Then set `POSEIDON_OTLP_ENDPOINT=http://localhost:4318` (there is no config file;
telemetry is env-driven) and open Grafana at <http://localhost:3000>
(admin / admin).
