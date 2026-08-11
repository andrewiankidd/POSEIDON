# Distribution & deployment

Every way POSEIDEN ships, and how each is built + published. If a deploy target
feels missing, it should be listed here - this page is the single index of
"where does POSEIDEN run and how does it get there."

## The shape of it

POSEIDEN is **one frontend bundle** (`frontend/web/`) and **one server binary**
(`poseiden-server`), delivered several ways:

| Target | What it is | Built by | Published to |
|--------|-----------|----------|--------------|
| **Landing + web client** | Static `frontend/web/` (landing at `/`, app at `/app.html`) | `pages.yml` | GitHub Pages (or Cloudflare Pages) |
| **Web instance** | Container running `poseiden-server` (API + serves the frontend) | `docker.yml` | GHCR (`ghcr.io/andrewiankidd/poseiden`) |
| **Kubernetes** | The image + a Helm chart (PVC, single replica) | - (chart in `deploy/helm/poseiden`) | your cluster |
| **Desktop** | Tauri installers (Windows/macOS/Linux) | `release.yml` | GitHub Releases |
| **CLI** | `poseiden` archive per platform | `release.yml` | GitHub Releases |
| **Mobile** | Android APK (best-effort) | `release.yml` | GitHub Releases |

The key relationship: **the landing page and the web client are static and
backend-less**; they need a running **web instance** (the container) to show
data. On the hosted instance the client talks to that instance's own API; served
from Pages it prompts for an instance URL (repoint).

## 1. Landing page + web client (GitHub Pages)

`frontend/web/` is served as-is. `index.html` is the landing page (project info,
download buttons, "Launch in browser"); `app.html` is the actual application.
`.github/workflows/pages.yml` uploads the directory on every push to main.

**One-time setup:** Settings → Pages → Build and deployment → **Source: GitHub
Actions**. No `gh-pages` branch, no build step (the frontend is pure static -
POSEIDEN has no WASM, unlike some sibling projects).

The download links on the landing page point at GitHub Releases `latest` assets
by stable filename, so they resolve the same URL release-to-release.

## 2. Landing page + web client (Cloudflare Pages)

Same static bundle, if you prefer Cloudflare. In the Cloudflare Pages dashboard,
connect the repo and set:

- **Build command:** *(none)*
- **Build output directory:** `frontend/web`
- **Root directory:** *(repo root)*

No redirects or headers file is needed - it's a two-page static site
(`index.html` + `app.html`) with relative asset paths. Cloudflare serves
`index.html` at `/` and `app.html` at `/app.html` out of the box.

## 3. Web instance (Docker / GHCR)

`docker.yml` builds the multi-stage `Dockerfile` and pushes to GHCR:

- push to **main** → `latest-main` + a `sha-<short>` tag
- push to a **`v*` tag** → the semver tag + `latest`
- **PRs** build only (no push) as a "does the image still build?" gate

Run it:

```bash
docker run -p 8737:8737 \
  --env-file poseiden.env \
  -v poseiden-data:/data \
  ghcr.io/andrewiankidd/poseiden:latest-main
```

No config file to mount: instance settings come from env (`poseiden.env`), and
teams/rules live in the DB on the volume - add them in the UI, or seed with
`poseiden config import` (see [CLI.md](CLI.md)).

Any provider token is passed via `--env-file` (or a Kubernetes/Compose secret),
**never** inline on the command line where it lands in shell history and the
process list. `poseiden.env` holds `POSEIDEN_AZURE_PAT=...` (and any GitHub /
GitLab token env vars) and stays out of version control. For Azure DevOps,
polling needs Work Items (Read) + Build (Read); because the web instance serves
the full UI (which writes State/Tags and work-item↔PR links back to Azure DevOps),
its Azure DevOps token also needs Work Items (Write) - a poll-only deployment can
stay read-only. GitHub / GitLab need no token for public repos; an optional token
covers private repos or higher rate limits.

The image is Debian-slim, runs as non-root (uid 10001), bundles SQLite
statically, and stores data on the `/data` volume.

## 4. Kubernetes (Helm)

Chart at [`deploy/helm/poseiden`](../deploy/helm/poseiden). Single replica
(SQLite is a single writer) on a **ReadWriteOnce** PVC.

Released versions are published to GHCR as an OCI artifact by
[`chart.yml`](../.github/workflows/chart.yml) on each `v*` tag (chart version +
appVersion set to the tag, so the chart deploys the image built on the same tag).
Install a release without a checkout:

```bash
helm install poseiden oci://ghcr.io/andrewiankidd/charts/poseiden --version 0.2.0 \
  --set azureDevOps.existingSecret=poseiden-pat \
  --set persistence.size=2Gi
```

…or straight from a working copy (unreleased / local changes):

```bash
helm install poseiden deploy/helm/poseiden \
  --set azureDevOps.existingSecret=poseiden-pat \
  --set persistence.size=2Gi
```

- **Auth** is a mesh concern - POSEIDEN ships no login. Gate the service with an
  Istio `AuthorizationPolicy` / `RequestAuthentication`.
- **PAT** - prefer `azureDevOps.existingSecret` (a Secret you manage) over the
  inline `azureDevOps.pat` convenience value.
- **Config** (projects + rules) lives in the DB on the PV - set via the UI, or
  `poseiden config import` for GitOps. Instance settings are env (chart values →
  Deployment env); no ConfigMap.

> **Build any of these locally.** `./poseiden.sh build {desktop | cli | apk | image}`
> runs the same commands documented below - for dogfooding a build without waiting
> on CI. See [RUNNING.md](RUNNING.md#hosted).

## 5. Desktop installers

`release.yml`'s `build-platform` matrix runs `cargo tauri build` per OS:

- **Windows** - `.msi` + NSIS `.exe`
- **macOS** - `.dmg` (single-arch aarch64 today; universal is a follow-up)
- **Linux** - `.deb` + `.AppImage`

Attached to the GitHub Release. `main` pushes refresh a rolling `latest-main`
pre-release; `v*` tags create a draft versioned release.

## 6. CLI

Same `release.yml` matrix builds `poseiden-cli` into a per-platform archive
(`poseiden-cli-{windows,linux,macos}.{zip,tar.gz}`) with the binary and licenses.
See [CLI.md](CLI.md) for usage.

## 7. Mobile (Android)

`build-android` in `release.yml` runs `cargo tauri android build` (debug-signed
APK, best-effort / `continue-on-error`). The primary mobile story is a
**repointed** client: install the APK, open Settings, set your instance URL, and
you're carrying your hosted POSEIDEN on the go. iOS is a backlog item.

## Versioning & tags

- Conventional commits; `v*` tags cut versioned releases.
- The container's `latest` and Release "Latest" badge track the most recent
  `v*` tag; `latest-main` is the always-fresh tip of main (pre-release).
- Bump `version` in the root `Cargo.toml`, `Chart.yaml` (`version` + `appVersion`),
  and `tauri.conf.json` together when tagging a release.
