# POSEIDEN Helm chart

Deploys the POSEIDEN web instance (single-replica, SQLite on a PV) with **optional
ingress-layer authentication**. One chart, two environments, switched by values
alone — no chart edits:

- **Standalone** (minikube / bare k8s + nginx ingress): `ingress.type: nginx`.
  Self-contained; with `auth.enabled` it brings its own oauth2-proxy and, for
  `dex`/`keycloak`, a bundled identity provider.
- **Enterprise** (Istio mesh + shared gateway): `ingress.type: istio`. Creates a
  `VirtualService` (+ `AuthorizationPolicy` when auth is on) and delegates the
  auth challenge to the mesh's ext-authz pattern.

The app itself has **zero auth code**. When auth is enabled, oauth2-proxy performs
the OAuth2/OIDC flow and injects `X-Auth-Request-*` identity headers into every
upstream request; the app reads them like any other header. Disable auth and
traffic goes straight to the app (gate it at your ingress/mesh yourself).

## Quick start

Released versions are published to GHCR (OCI), so you can install without a
checkout:

```bash
helm install poseiden oci://ghcr.io/andrewiankidd/charts/poseiden --version 0.2.0
```

The examples below install from the source tree (`deploy/helm/poseiden`) for
unreleased / local changes - swap that path for the `oci://…` ref to use a
published version.

### Local POC on minikube (nginx + bundled Dex, no external accounts)

```bash
minikube start
minikube addons enable ingress
helm install poseiden deploy/helm/poseiden -f deploy/helm/poseiden/ci/minikube-dex-values.yaml
echo "$(minikube ip) poseiden.localhost" | sudo tee -a /etc/hosts
# open http://poseiden.localhost  → login admin@example.com / password
```

### Standalone with a real IdP (GitHub org)

See [`ci/nginx-github-values.yaml`](ci/nginx-github-values.yaml). Register a GitHub
OAuth app with callback `https://<host>/oauth2/callback`.

### Enterprise on Istio (Azure AD)

See [`ci/istio-azure-values.yaml`](ci/istio-azure-values.yaml). The chart deploys
oauth2-proxy; a cluster admin registers it once (below).

## Auth providers

Set `auth.enabled: true`, pick one `auth.provider`, fill in only that block. Each
supports inline credentials or an `existingSecret` (keys configurable).

| Provider | Notes |
|----------|-------|
| `google` | `allowedDomains` to restrict to a workspace. |
| `github` | `orgs` / `teams` to restrict. |
| `azure` | `tenant` (GUID), `emailDomains`. |
| `auth0` | `domain`. |
| `keycloak` | External (`url`) or `bundled.enabled` (POC, embedded H2). |
| `dex` | `bundled.enabled` (POC, in-memory, `staticPasswords`). |

### Bundled IdP details (Dex / Keycloak)

A bundled IdP is exposed **on the app's own host** under a path — `/dex` or
`/auth` — added to the same ingress. Its OIDC *issuer* is that external URL, so the
browser can complete the login, while oauth2-proxy makes its server-side
token/JWKS calls to the in-cluster service directly (a **split-horizon** setup, so
no in-cluster resolution of the external host is needed). This is what makes the
single-host minikube POC work end to end.

Caveats, by design (POC-grade):

- **Storage is ephemeral.** Dex is in-memory; Keycloak bundled uses `start-dev`
  with embedded H2 — state is lost on pod restart (the realm re-imports on boot).
  For production use an external IdP (`existingSecret`) or external Keycloak
  (`bundled.enabled: false`, `url: …`).
- **The client secret is derived deterministically** from the release name (not
  random), so oauth2-proxy and the IdP agree on first install without
  coordination. Predictable — fine for a self-hosted POC IdP; override
  `keycloak.clientSecret` for anything real.
- **Dex static passwords are bcrypt-hashed by the chart** (Dex requires hashes,
  not plaintext). The bcrypt salt is regenerated on each `helm upgrade`, so the
  Dex pod rolls on upgrade (the password still validates).
- **`auth.cookieSecure` also picks the URL scheme** — `true` (default) → `https`,
  `false` → `http`. Set it `false` only for a plain-HTTP local cluster.
- Bundled IdPs are wired for the **nginx** ingress path (the standalone POC).
  Enterprise/Istio deployments should use an external IdP.

## Enterprise cluster: meshConfig registration

The `AuthorizationPolicy` references an ext-authz provider by name. That provider
must be registered **once** in istiod `meshConfig.extensionProviders` (a cluster-
admin op, in the istiod Helm values):

```yaml
meshConfig:
  extensionProviders:
    - name: poseiden-oauth2-proxy                 # matches AuthorizationPolicy provider.name
      envoyExtAuthzHttp:
        service: poseiden-oauth2-proxy.<ns>.svc.cluster.local
        port: 80
        headersToUpstreamOnAllow:
          - authorization
          - x-auth-request-email
          - x-auth-request-user
          - x-auth-request-access-token
        includeRequestHeadersInCheck: [authorization, cookie]
        includeAdditionalHeadersInCheck:
          X-Auth-Request-Redirect: 'https://%REQ(:authority)%%REQ(:path)%'
```

If your cluster already has a shared oauth2-proxy registered, set
`ingress.istio.externalProviderName` to its name — the chart then creates only the
`VirtualService` + `AuthorizationPolicy` (no oauth2-proxy). See
[`ci/istio-external-values.yaml`](ci/istio-external-values.yaml).

## AI tag suggestions

POSEIDEN can suggest canonical tags for work items from each team's approved set.
It's **advisory only** - suggestions surface as chips and are never auto-applied -
and **off by default** (`ai.enabled: false`). Turn it on and point it at an
OpenAI-compatible chat/completions endpoint, or let the chart bundle one.

| Key | Default | Description |
|-----|---------|-------------|
| `ai.enabled` | `false` | Master switch for the hosted AI tagger. |
| `ai.model` | `llama3.2:1b` | Model name (a small 1-3B model stays runnable on a modest node). |
| `ai.endpoint` | `""` | External OpenAI-compatible chat/completions URL. Used ONLY when `ai.bundled.enabled` is off. |
| `ai.apiKey` / `.existingSecret` | `""` | Bearer key for a hosted endpoint - injected from a Secret, never in config. |
| `ai.bundled.enabled` | `false` | Run a bundled Ollama sidecar in-cluster instead of calling out. |
| `ai.modelCacheSize` | `""` (→ `4Gi`) | Size cap for the ephemeral in-process embedded-model cache. |
| `aiCpuThreads` | `""` | CPU thread cap for in-process inference (`RAYON_NUM_THREADS`). |

**Bundled Ollama.** With `ai.bundled.enabled: true` the chart deploys an Ollama
sidecar, auto-pulls `ai.model` on first start, and wires the app to it - AI works
with zero external setup and item titles never leave the cluster. It costs CPU +
RAM for the model (hence the small default) and disk for the model cache (a PVC,
`ai.bundled.persistence`). `ai.bundled.gpu.enabled` runs the model on an NVIDIA
GPU (needs a GPU node + the NVIDIA device plugin; `minikube start --gpus all`).

**Injected env.** When `ai.enabled`, the deployment sets `POSEIDEN_AI_ENDPOINT`
(the bundled Ollama service URL, or `ai.endpoint`), `POSEIDEN_AI_MODEL`, and
`POSEIDEN_AI_API_KEY` (hosted, from the Secret). It always sets `HF_HOME=/models`
- a writable ephemeral volume for the offline embedded-model cache, since the
rootfs is read-only - and, when `aiCpuThreads` is set, `RAYON_NUM_THREADS` to cap
the in-process inference thread pool (the pod sees the node's logical CPUs, not
its cgroup CPU limit, so without this it oversubscribes).

## Key values

| Key | Default | Description |
|-----|---------|-------------|
| `image.repository` / `image.tag` | `ghcr.io/andrewiankidd/poseiden` / chart appVersion | App image. |
| `persistence.*` | 1Gi RWO PVC at `/data` | SQLite volume; single-replica only. |
| `azureDevOps.pat` / `.existingSecret` | — | PAT source (env-injected, never in config). |
| `pollInterval` | `15m` | Poll cadence (env `POSEIDEN_POLL_INTERVAL`). |
| `telemetry.otlpEndpoint` | `""` | OTLP collector URL; set to enable export. |
| `ingress.type` | `nginx` | `nginx` \| `istio`. |
| `ingress.host` | `poseiden.example.com` | Ingress/VirtualService host. |
| `auth.enabled` | `false` | Turn on ingress-layer auth. |
| `auth.provider` | `none` | `google`/`github`/`azure`/`auth0`/`keycloak`/`dex`. |
| `auth.cookieSecure` | `true` | Set `false` only for plain-HTTP local clusters. |
| `auth.tokenVerification.enabled` | `false` | Verify the forwarded access token against the IdP JWKS (defence-in-depth). |
| `networkPolicy.enabled` | `false` | Lock the app Service to the ingress so the identity header can't be spoofed pod-to-pod. |
| `ai.enabled` | `false` | Turn on the hosted AI tag suggester (see below). |
| `localhost.enabled` | `false` | Local server+client playground/demo (single-tenant, auth OFF). |

Full annotated reference: [`values.yaml`](values.yaml).

## Notes + caveats

- **Single replica.** SQLite is a single writer on a ReadWriteOnce volume;
  strategy is `Recreate`. Horizontal scale needs the Postgres store swap.
- **PAT scopes.** Polling needs Work Items (Read) + Build (Read); the UI's
  write-backs additionally need Work Items (Write). A poll-only deployment can
  stay read-only.
- **Bundled IdPs are POC-only** (in-memory / embedded H2, nginx path) — see
  "Bundled IdP details" above; use an external IdP for production.
- **Token verification (defence-in-depth).** By default the app trusts the
  identity header. `auth.tokenVerification.enabled` makes it additionally verify
  the oauth2-proxy-forwarded access token against the IdP JWKS (`jwksUrl`, plus
  the expected `issuer` / `audience`) and take the owner from the verified
  `email` claim, so a spoofed header without a validly signed token is rejected.
- **NetworkPolicy.** The app trusts `X-Auth-Request-Email` as identity, so any pod
  that can reach the app Service directly could set it and impersonate a tenant.
  `networkPolicy.enabled` restricts ingress to the app pod to your ingress
  controller / gateway only (`allowFrom`) - turn it on for any shared or
  multi-tenant cluster (needs a NetworkPolicy-enforcing CNI).
- **localhost playground.** `localhost.enabled` stands up a second pod running the
  same image as a web *client* of the server, plus a small landing page - a
  server+client demo pair. It's single-tenant (`default` owner) and the chart
  refuses to render it with `auth.enabled: true`; never use it in a real deployment.
- **Multi-user is real.** The app consumes `X-Auth-Request-Email` and maps it to
  the `owner` key, so each authenticated user gets their **own** teams, rules,
  tags, and reports (stored per-owner in the DB). The nginx ingress forwards the
  header via `auth-response-headers`; on the Istio path, ensure the mesh's
  ext-authz provider is configured to pass `X-Auth-Request-Email` upstream
  (`headersToUpstreamOnAllow`). With no header (standalone / unauthenticated),
  the owner falls back to a single `default` tenant.
- **Hosted auth: per-user device-code sign-in, isolated on the volume (or a
  shared PAT).** Each authenticated user can sign in from the web with the Azure
  CLI **device-code** flow, exactly like the desktop app. The catch the CLI
  normally has - its cache (`~/.azure`) is single-machine and NOT request-isolated,
  so a shared container would let one `az login` clobber another and a poll mint a
  token as the wrong user - is sidestepped by giving **each owner its own
  `AZURE_CONFIG_DIR`** on the data volume (`/data/az-sessions/<owner>/`). Sessions
  are isolated, survive restarts, and a poll only ever mints a token from the
  caller's own cache. The image ships the Azure CLI for this (sign-in and every
  token refresh shell out to `az`). Alternatively, a deployment can still set
  **one shared PAT** (env from a Secret; a service principal / workload identity
  works too) that reads every project any user configures - a PAT on a team always
  takes precedence over `az`. Either way, multitenancy here
  is per-user **config + credential cache**, keyed off `X-Auth-Request-Email`.
  True per-user ADO identity via the user's own OIDC access token (the Azure
  DevOps scope, available as `X-Auth-Request-Access-Token`) passed through to the
  provider is a separate, larger piece of work.
