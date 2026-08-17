#!/usr/bin/env bash
# POSEIDON - the one dev CLI for the project: run the stack, and build artifacts.
#
#   run the minikube stack:
#     ./poseidon.sh up                # build -> deploy the full stack -> seed tenants/ -> open the app
#     ./poseidon.sh dev               # live-reload loop (skaffold: rebuild + redeploy on save)
#     ./poseidon.sh down [--clean]    # uninstall the release (--clean also stops minikube)
#     ./poseidon.sh verify            # deploy an isolated release, assert the stack, tear it down
#     ./poseidon.sh status            # show what's running
#   build distributable artifacts (thin wrappers over the SAME commands CI runs):
#     ./poseidon.sh build cli         # release CLI binary (target/release/poseidon)
#     ./poseidon.sh build desktop     # Tauri desktop app (installer per OS)
#     ./poseidon.sh build apk         # Tauri Android APK (needs android init + SDK/NDK)
#     ./poseidon.sh build image       # Docker server image
#     ./poseidon.sh build all         # cli + desktop + image
#   (the stack verbs also work under 'run', e.g. './poseidon.sh run up')
#
# `up` runs the same Helm chart + container you'd ship to a real cluster, then
# imports each bundle in tenants/ into its OWN tenant (owner from the bundle's
# `owner:` field; owner-less falls back to POSEIDON_OWNER) and opens the web UI.
# `verify` is the headless stack test: it deploys an isolated release and asserts
# the chart + image + stub provider + multi-tenant isolation over HTTP, then cleans
# up. (To exercise a real client against it, enable the chart's `localhost` mode -
# it deploys a web client pointed at the server.) Every path to running the chart
# locally goes through this script.
#
# Cross-platform bash (Linux, macOS, Git Bash on Windows). Env overrides:
# POSEIDON_OWNER (fallback owner), POSEIDON_IMAGE_TAG, POSEIDON_RELEASE /
# POSEIDON_HOST / POSEIDON_VALUES.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART="$HERE/deploy/helm/poseidon"
TENANTS="$HERE/tenants"
APP_TAURI_CONF="$HERE/crates/poseidon-app/tauri.conf.json"   # desktop/android builds
RELEASE="${POSEIDON_RELEASE:-poseidon}"
HOST="${POSEIDON_HOST:-poseidon.localhost}"
VALUES="${POSEIDON_VALUES:-$CHART/ci/minikube-dex-values.yaml}"
FALLBACK_OWNER="${POSEIDON_OWNER:-admin@example.com}"
IMAGE_REPO="poseidon"
IMAGE_TAG="${POSEIDON_IMAGE_TAG:-dev}"
IMAGE="$IMAGE_REPO:$IMAGE_TAG"
INGRESS_PIDFILE="${TMPDIR:-/tmp}/poseidon-ingress-pf.pid"
# Recommended minikube resources. The offline AI model + release image builds are
# CPU/RAM hungry, and minikube's docker-driver DEFAULT is only 2 CPUs / ~2GB - which
# makes the in-pod offline tagger crawl (minutes/query on 2 cores). Override with
# POSEIDON_MINIKUBE_CPUS / _MEMORY.
MK_CPUS="${POSEIDON_MINIKUBE_CPUS:-8}"
MK_MEMORY="${POSEIDON_MINIKUBE_MEMORY:-8192}"
# Resolve a Python that actually RUNS - on Windows `python3` is often a Microsoft
# Store stub that only prints "Python was not found", so test execution, not PATH.
PY=""
for _py in python3 python py; do
  if command -v "$_py" >/dev/null 2>&1 && "$_py" -c 'import json,sys' >/dev/null 2>&1; then PY="$_py"; break; fi
done

info() { printf '\033[36m-> %s\033[0m\n' "$*"; }
warn() { printf '\033[33m!  %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[31mx  %s\033[0m\n' "$*" >&2; exit 1; }
req()  { curl -fsS -H "X-Auth-Request-Email: $1" "${@:2}"; }             # owner-scoped API call
jassert() { "$PY" -c "import sys,json; d=json.load(sys.stdin); $1"; }    # assert on JSON from stdin

open_url() {
  case "$(uname -s)" in
    Darwin)               open "$1" >/dev/null 2>&1 || true ;;
    MINGW*|MSYS*|CYGWIN*) start "" "$1" >/dev/null 2>&1 || cmd //c start "" "$1" >/dev/null 2>&1 || true ;;
    *)                    xdg-open "$1" >/dev/null 2>&1 || true ;;
  esac
}

install_hint() {
  case "$1" in
    minikube) echo "  https://minikube.sigs.k8s.io/docs/start/" ;;
    helm)     echo "  https://helm.sh/docs/intro/install/" ;;
    kubectl)  echo "  https://kubernetes.io/docs/tasks/tools/" ;;
    skaffold) echo "  https://skaffold.dev/docs/install/" ;;
    cargo)       echo "  https://rustup.rs  (installs cargo + rustc)"; return ;;
    cargo-tauri) echo "  cargo install tauri-cli --version '^2' --locked"; return ;;
    *)        echo "  (install '$1' and re-run)" ;;
  esac
  case "$(uname -s)" in
    Darwin)              echo "  or: brew install $1" ;;
    Linux)               echo "  or your package manager (apt/dnf/pacman)" ;;
    MINGW*|MSYS*|CYGWIN*) echo "  or: winget install $1   (or choco/scoop)" ;;
  esac
}

require() {
  local missing=0 c
  for c in "$@"; do
    if ! command -v "$c" >/dev/null 2>&1; then
      warn "'$c' is not installed:"; install_hint "$c"; missing=1
    fi
  done
  [ "$missing" -eq 0 ] || die "Install the tool(s) above, then re-run."
}

# Bring the cluster up with enough CPU/RAM. A fresh start is provisioned to the
# recommended size; an already-running but under-provisioned cluster gets a warning
# and (on an interactive terminal) an offer to recreate - minikube's docker driver
# can't resize CPUs in place, so raising them means delete + start.
ensure_cluster() {
  if minikube status >/dev/null 2>&1; then
    local host_cpus cur_cpus nanocpus
    host_cpus="$(nproc 2>/dev/null || echo '?')"
    nanocpus="$(docker inspect minikube --format '{{.HostConfig.NanoCpus}}' 2>/dev/null || echo 0)"
    if [ "${nanocpus:-0}" -gt 0 ] 2>/dev/null; then
      cur_cpus=$(( nanocpus / 1000000000 ))
    else
      cur_cpus="$(minikube config get cpus 2>/dev/null || echo 2)"
    fi
    if [ "${cur_cpus:-2}" -lt "$MK_CPUS" ] 2>/dev/null; then
      warn "minikube is running with ${cur_cpus} CPU(s) (host has ${host_cpus}); ${MK_CPUS} recommended."
      warn "The offline AI model + release builds are CPU-bound - 2 CPUs makes tagging crawl (minutes/query)."
      local hint="minikube delete && minikube start --cpus=${MK_CPUS} --memory=${MK_MEMORY}"
      if [ -t 0 ]; then
        printf '\033[33m!  Recreate the cluster now with %s CPU / %s MB? This wipes local cluster state. [y/N] \033[0m' "$MK_CPUS" "$MK_MEMORY" >&2
        read -r ans || ans=""
        case "$ans" in
          y|Y) info "Recreating minikube..."; minikube delete; minikube start --cpus="$MK_CPUS" --memory="$MK_MEMORY" ;;
          *)   warn "Keeping the ${cur_cpus}-CPU cluster. Raise it later with: $hint" ;;
        esac
      else
        warn "Non-interactive - leaving it. Raise it with: $hint  (or set POSEIDON_MINIKUBE_CPUS/_MEMORY)."
      fi
    fi
  else
    info "Starting minikube with ${MK_CPUS} CPU / ${MK_MEMORY} MB..."
    minikube start --cpus="$MK_CPUS" --memory="$MK_MEMORY"
  fi
  minikube addons enable ingress >/dev/null 2>&1 || true
}

# Build the image, load it into minikube, install/upgrade the chart, wait for the
# app pod. The Dex PoC values don't override the image (a real cluster pulls it),
# so we build from source and load it straight into minikube.
deploy() {
  require docker minikube helm kubectl curl
  info "Ensuring the minikube cluster is up and adequately resourced..."
  ensure_cluster
  info "Building the server image ($IMAGE)..."
  # --provenance=false keeps this a plain single-manifest image (buildx's default
  # attestation-index images are one trigger for the stale-tag problem the load
  # step below works around).
  # Stamp a local BUILD TIMESTAMP as the version (CI passes a git sha instead). The
  # server exposes it via /env.js and the sidebar shows it - so you can tell at a
  # glance which build a browser is running (handy past a stale cache).
  docker build --provenance=false --target runtime \
    --build-arg "POSEIDON_COMMIT=$(date '+%Y%m%d %H:%M')" \
    -t "$IMAGE" "$HERE"
  info "Loading the image into minikube..."
  # `minikube image load <tag>` (docker driver) has repeatedly NOT replaced an
  # existing tag on rebuild - the node keeps the STALE image and the pod runs old
  # code (a code-only change then silently doesn't deploy). `docker save` to a tar
  # + load-from-tar is the reliable path; rm first so a stale copy can't linger.
  minikube image rm "$IMAGE" >/dev/null 2>&1 || true
  local img_tar; img_tar="$(mktemp)"
  docker save "$IMAGE" -o "$img_tar"
  minikube image load "$img_tar"
  rm -f "$img_tar"
  # Private per-tenant Dex logins come from the (gitignored) tenants/ bundles, not
  # the committed values file - so a real org email never lands in source. For each
  # bundle whose owner isn't already a committed static login, append one static
  # password (owner email, "password") at the next free index. Base index = the
  # count already in $VALUES; harmless no-op keys when the values file has no Dex.
  # Emails already committed in $VALUES (pure-bash membership test below - avoids
  # grep, whose -F build aborts on some MSYS/Git-Bash setups, and sidesteps regex
  # metachars in owners like poseidon+demo-data@…).
  local dex_sets=() idx owner committed
  committed="$(sed -n -E 's/^[[:space:]]*- email:[[:space:]]*([^[:space:]]+).*/\1/p' "$VALUES" 2>/dev/null)"
  idx=0; [ -z "$committed" ] || idx="$(printf '%s\n' "$committed" | wc -l | tr -d ' ')"
  shopt -s nullglob
  for f in "$TENANTS"/*.poseidon.import.yaml; do
    owner="$(sed -n -E 's/^[[:space:]]*owner:[[:space:]]*([^[:space:]]+).*/\1/p' "$f" | head -1)"
    [ -n "$owner" ] || continue
    case $'\n'"$committed"$'\n' in *$'\n'"$owner"$'\n'*) continue ;; esac
    dex_sets+=(--set "auth.dex.bundled.staticPasswords[$idx].email=$owner")
    dex_sets+=(--set "auth.dex.bundled.staticPasswords[$idx].password=password")
    dex_sets+=(--set "auth.dex.bundled.staticPasswords[$idx].username=$(printf '%s' "$owner" | sed -E 's/@.*//; s/[^A-Za-z0-9]+/-/g')")
    idx=$((idx + 1))
  done
  info "Installing the Helm chart ($RELEASE -> $HOST)..."
  helm upgrade --install "$RELEASE" "$CHART" -f "$VALUES" \
    --set "image.repository=$IMAGE_REPO" \
    --set "image.tag=$IMAGE_TAG" \
    --set "image.pullPolicy=IfNotPresent" \
    --set "ingress.host=$HOST" \
    ${dex_sets[@]+"${dex_sets[@]}"}
  # Force a fresh pod every deploy. We rebuild the image behind the SAME tag
  # (poseidon:dev), so a code-only change leaves the Deployment manifest identical
  # - helm upgrade sees no diff and the pod keeps running the OLD binary. A rollout
  # restart recreates it, picking up the freshly-loaded image (IfNotPresent uses
  # the local minikube copy).
  info "Rolling the app pod onto the new image..."
  kubectl rollout restart "deploy/$RELEASE"
  info "Waiting for the pod to be ready..."
  kubectl rollout status "deploy/$RELEASE" --timeout=300s
}

# Port-forward straight to the app pod (bypassing oauth2-proxy, so we can inject
# the identity header ourselves) on a RANDOM free local port - so it never
# collides with a dev's own forward, a leftover, or another tool. Sets globals
# BASE + PF_PID.
port_forward_up() {
  local log; log="$(mktemp)"
  kubectl port-forward "deploy/$RELEASE" ":8737" >"$log" 2>&1 &
  PF_PID=$!
  local i port=""
  for i in $(seq 1 40); do
    [ -n "$port" ] || port="$(sed -n -E 's#.*127\.0\.0\.1:([0-9]+) -> 8737.*#\1#p' "$log" | head -1)"
    if [ -n "$port" ]; then
      BASE="http://127.0.0.1:$port"
      curl -fsS "$BASE/api/health" >/dev/null 2>&1 && { rm -f "$log"; return 0; }
    fi
    sleep 0.5
  done
  rm -f "$log"
  return 1
}

# Make the cluster reachable from the host with NO admin, tunnel, or hosts-file
# edits: port-forward the ingress controller to 127.0.0.1:80 (Windows/macOS allow
# this without elevation). *.localhost - which Chrome/Edge resolve to 127.0.0.1 -
# then reaches the ingress and routes by host to the server / client / hub. The
# forward persists after `up` returns (nohup + disown); `down` stops it.
ingress_forward_start() {
  if [ -f "$INGRESS_PIDFILE" ] && kill -0 "$(cat "$INGRESS_PIDFILE" 2>/dev/null)" 2>/dev/null; then
    return 0                                        # already forwarding
  fi
  nohup kubectl port-forward -n ingress-nginx svc/ingress-nginx-controller \
    80:80 --address 127.0.0.1 >/dev/null 2>&1 &
  local pid=$!
  echo "$pid" >"$INGRESS_PIDFILE"
  disown 2>/dev/null || true
  local i
  for i in $(seq 1 20); do
    curl -s -o /dev/null --max-time 2 -H "Host: $HOST" http://127.0.0.1:80/ && return 0
    kill -0 "$pid" 2>/dev/null || return 1          # died (e.g. :80 already in use)
    sleep 0.5
  done
  return 1
}
ingress_forward_stop() {
  [ -f "$INGRESS_PIDFILE" ] || return 0
  kill "$(cat "$INGRESS_PIDFILE" 2>/dev/null)" 2>/dev/null || true
  rm -f "$INGRESS_PIDFILE"
}

# Import every tenants/*.poseidon.import.yaml into its OWN tenant. The owner comes
# from each bundle's `owner:` field (fallback if absent); we import over HTTP,
# which keys the tenant off the injected header - one tenant per file.
import_tenants() {
  shopt -s nullglob
  local files=("$TENANTS"/*.poseidon.import.yaml)
  [ ${#files[@]} -gt 0 ] || { warn "No tenant bundles in tenants/ - skipping import."; return 0; }

  # Import EVERY bundle's config under its own `owner:` (config import needs no
  # credentials - only polling a real provider does). In localhost mode (auth off)
  # the demo bundle is instead seeded under `default`, the tenant a header-less
  # playground browser maps to, so the demo shows with no login; the other tenants
  # still get imported under their own owner (reachable via the dev owner-switcher
  # or a Dex login), their real-provider poll just failing gracefully without a PAT.
  local localhost_mode=0
  kubectl get "deploy/$RELEASE-hub" >/dev/null 2>&1 && localhost_mode=1

  info "Port-forwarding to import tenant config..."
  port_forward_up || { warn "instance did not become reachable - skipping import."; return 0; }
  local f owner
  for f in "${files[@]}"; do
    owner="$(sed -n -E 's/^[[:space:]]*owner:[[:space:]]*([^[:space:]]+).*/\1/p' "$f" | head -1)"
    [ -n "$owner" ] || owner="$FALLBACK_OWNER"
    # In the localhost playground, the demo goes to `default` so a header-less
    # browser sees it straight away without signing in.
    if [ "$localhost_mode" -eq 1 ]; then
      case "$(basename "$f")" in
        demo-data.*) owner="default" ;;
      esac
    fi
    info "  import $(basename "$f") -> tenant $owner"
    curl -fsS -X POST "$BASE/api/config/import?replace=true" \
      -H "X-Auth-Request-Email: $owner" -H "Content-Type: application/x-yaml" \
      --data-binary @"$f" >/dev/null || { warn "  import failed for $(basename "$f")"; continue; }
    # Do NOT force poll_all_teams here: a real provider with a large second team (e.g. an
    # 86k ops queue) would get polled on every deploy and rate-limit the provider. The
    # imported config's own poll_all_teams (default false) stands, so the auto-poll below
    # only touches the ACTIVE team. A user opts into all-teams polling in Settings.
    curl -fsS -X POST "$BASE/api/poll" -H "X-Auth-Request-Email: $owner" >/dev/null \
      || warn "  poll for $owner had errors (a real-provider team may need POSEIDON_AZURE_PAT)"
  done
  kill "$PF_PID" 2>/dev/null || true
}

# Behavioural assertions against a running instance backed by the stub provider:
# health, config import, exact stub counts, dashboard rollup + flags, and
# multi-tenant isolation. Auth is bypassed via the port-forward + header.
run_assertions() {
  local base="$1" bundle="$TENANTS/demo-data.poseidon.import.yaml"
  local alice="poseidon+e2e@example.com" bob="poseidon+e2e-other@example.com"
  info "health"
  curl -fsS "$base/api/health" | jassert "assert d['status']=='ok', d"
  info "import demo bundle over HTTP (owner $alice)"
  curl -fsS -X POST "$base/api/config/import?replace=true" \
    -H "X-Auth-Request-Email: $alice" -H "Content-Type: application/x-yaml" \
    --data-binary @"$bundle" | jassert "assert d['teams']==1 and d['replaced'] is True, d"
  info "poll the stub team (exact counts)"
  req "$alice" -X POST "$base/api/poll" \
    | jassert "assert d['work_items']==14 and d['pipelines']==3 and d['pull_requests']==3, d; print('   14 items, 3 pipelines, 3 PRs')"
  info "dashboard rollup + hygiene flags"
  req "$alice" "$base/api/dashboard" \
    | jassert "assert d['total_work_items']==14 and d['flagged_items']==3, d; print('   14 items, 3 flagged')"
  info "multi-tenant isolation ($bob, never configured, sees nothing)"
  req "$bob" "$base/api/dashboard" \
    | jassert "assert d['total_work_items']==0, d; print('   isolated: 0 items for another owner')"
}

up() {
  deploy
  import_tenants
  info "Up."
  echo
  # In localhost mode a landing page (the "hub") links the server + client - open
  # that; otherwise open the server. (Detect the hub deployment to know the mode.)
  local target
  if kubectl get "deploy/$RELEASE-hub" >/dev/null 2>&1; then
    target="http://hub-$HOST"
    echo "  Playground: a server + a web client pointed at it (single-tenant, auth off)."
  else
    target="http://$HOST"
    echo "  Log in (demo tenant):  poseidon+demo-data@example.com / password"
    echo "  (each bundle is its own tenant/login - see the owners in tenants/*.import.yaml)"
  fi
  echo
  info "Making the cluster reachable (ingress -> 127.0.0.1:80)..."
  if ingress_forward_start; then
    info "Opening $target"
    open_url "$target"
    echo "  Reachable at $target - via an ingress forward on :80 that keeps running;"
    echo "  './poseidon.sh down' stops it. (*.localhost resolves to 127.0.0.1 in Chrome/Edge.)"
  else
    warn "Couldn't bind 127.0.0.1:80 - is something already using port 80?"
    echo "  Free it and re-run 'up', or forward manually and open $target :"
    echo "    kubectl port-forward -n ingress-nginx svc/ingress-nginx-controller 80:80 --address 127.0.0.1"
  fi
}

verify() {
  RELEASE="poseidon-verify"
  HOST="poseidon-verify.localhost"       # unique host so it can't collide with a dev's release
  require docker minikube helm kubectl curl
  [ -n "$PY" ] || die "the verify assertions need Python (python3 / python) - none found that runs."
  info "verify: deploying isolated release '$RELEASE' -> $HOST ..."
  deploy
  info "verify: asserting the running instance..."
  local rc=0
  if port_forward_up; then
    run_assertions "$BASE" || rc=$?
    kill "$PF_PID" 2>/dev/null || true
  else
    rc=1; warn "instance did not become reachable"
  fi
  info "verify: tearing down '$RELEASE'..."
  helm uninstall "$RELEASE" >/dev/null 2>&1 || true
  [ "$rc" -eq 0 ] && info "VERIFY PASSED - chart + image + stub provider + multi-tenant isolation ✓" \
                  || die "VERIFY FAILED"
}

# Live-reload loop: watch source, rebuild + redeploy on save (via Skaffold). The
# cargo-chef Dockerfile keeps the rebuild to ~seconds. Ctrl-C tears it down.
dev() {
  require docker minikube helm kubectl skaffold
  info "dev: starting the live-reload loop (skaffold dev). Ctrl-C stops + cleans up."
  exec skaffold dev
}

down() {
  require helm
  ingress_forward_stop
  info "Uninstalling $RELEASE..."
  helm uninstall "$RELEASE" >/dev/null 2>&1 || warn "release '$RELEASE' was not installed"
  if [ "${1:-}" = "--clean" ]; then
    require minikube
    info "Stopping minikube..."
    minikube stop || true
  fi
}

status() {
  require kubectl
  kubectl get deploy,pods,svc -l "app.kubernetes.io/instance=$RELEASE" 2>/dev/null \
    || warn "nothing running for release '$RELEASE'"
}

# ── build: distributable artifacts ────────────────────────────────────────────
# Thin, transparent wrappers over the SAME commands CI runs (docs/DISTRIBUTION.md)
# - local builds for dogfooding, not a second source of truth. Each echoes the
# underlying command (prefixed '+') before running it, so a failure points you
# straight at the real cargo/tauri/docker line.

# Build (only) the server image - no minikube load. `up` builds AND loads it.
build_image() {
  require docker
  info "build: Docker server image ($IMAGE)"
  echo "  + docker build --provenance=false --target runtime -t $IMAGE $HERE"
  docker build --provenance=false --target runtime \
    --build-arg "POSEIDON_COMMIT=$(date '+%Y%m%d %H:%M')" \
    -t "$IMAGE" "$HERE"
  info "Image built: $IMAGE  ('up' loads it into minikube; or push to a registry)."
}

build_cli() {
  require cargo
  info "build: CLI (release)"
  echo "  + cargo build --release -p poseidon-cli"
  cargo build --release -p poseidon-cli
  info "CLI binary: target/release/poseidon"
}

# The frontend is a dependency-free static bundle - there is nothing to compile.
build_web() {
  info "build: the web frontend (frontend/web/) is a no-build static bundle - nothing to compile."
  info "  it ships embedded in the desktop app and copied into the Docker image."
}

build_desktop() {
  require cargo cargo-tauri
  info "build: desktop app (Tauri) - same as CI's build-platform matrix"
  echo "  + cargo tauri build --config $APP_TAURI_CONF"
  cargo tauri build --config "$APP_TAURI_CONF"
  info "Bundles under target/release/bundle/ (installer per OS)."
}

build_apk() {
  require cargo cargo-tauri
  if [ ! -d "$HERE/crates/poseidon-app/gen/android" ]; then
    warn "Android project not initialized (crates/poseidon-app/gen/android missing)."
    echo "  One-time setup - needs Android SDK + NDK + JDK (ANDROID_HOME / NDK_HOME set):"
    echo "    cargo tauri android init --config $APP_TAURI_CONF"
    die "Initialize Android first, then re-run 'build apk'."
  fi
  info "build: Android APK (Tauri) - same as CI's build-android"
  echo "  + cargo tauri android build --config $APP_TAURI_CONF"
  cargo tauri android build --config "$APP_TAURI_CONF"
  info "APK under crates/poseidon-app/gen/android/app/build/outputs/."
}

build() {
  case "${1:-}" in
    image)   build_image ;;
    cli)     build_cli ;;
    web)     build_web ;;
    desktop) build_desktop ;;
    apk)     build_apk ;;
    # 'all' skips apk on purpose: it needs a one-time android init + SDK/NDK, so
    # it can't run unattended everywhere.
    all)     build_cli; build_desktop; build_image ;;
    ""|*)    die "usage: ./poseidon.sh build {image | desktop | apk | cli | web | all}" ;;
  esac
}

usage() {
  cat <<'EOF'
POSEIDON dev CLI - usage: ./poseidon.sh <command>

  run the minikube stack:
    up               build image, deploy the chart, seed tenants/, open the app
    dev              live-reload loop (skaffold): rebuild + redeploy on save
    down [--clean]   uninstall the release (--clean also stops minikube)
    verify           deploy an isolated release, assert the stack, tear it down
    status           show what's running

  build artifacts (same commands CI runs - see docs/DISTRIBUTION.md):
    build cli        release CLI binary (target/release/poseidon)
    build desktop    Tauri desktop app (installer per OS)
    build apk        Tauri Android APK (needs android init + SDK/NDK)
    build image      Docker server image
    build web        (no-op: the frontend is a static, no-build bundle)
    build all        cli + desktop + image

  The stack verbs also work under 'run' (e.g. './poseidon.sh run up').
EOF
}

# Dispatch. Read the command, then pass the rest to the handler. The stack verbs
# are available both bare (muscle memory) and under 'run'.
cmd="${1:-}"; shift 2>/dev/null || true
case "$cmd" in
  run)
    sub="${1:-}"; shift 2>/dev/null || true
    case "$sub" in
      up)     up ;;
      dev)    dev ;;
      down)   down "${1:-}" ;;
      verify) verify ;;
      status) status ;;
      *)      usage; exit 1 ;;
    esac ;;
  build)  build "${1:-}" ;;
  up)     up ;;
  dev)    dev ;;
  down)   down "${1:-}" ;;
  verify) verify ;;
  status) status ;;
  help|-h|--help) usage ;;
  *)      usage; exit 1 ;;
esac
