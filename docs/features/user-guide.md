# User Guide

Cross-cutting reference: where POSEIDON runs, where it keeps its data, how
secrets are handled, and the shapes it ships in. Not a feature - the context the
feature pages sit inside.

## Delivery shells - one logic layer, several shells

POSEIDON is one codebase with a single logic layer (`Service`) exposed three
ways, so the desktop app, the CLI, and the hosted web instance can never drift:

- **Desktop** (primary) - a native window (Windows / macOS / Linux; mobile via
  the same web bundle) that embeds the full service over a local SQLite store.
- **CLI** - the same service behind `poll` / `lint` / `report`, for smoke tests,
  CI gating, and flow reports.
- **Web / Docker** - the same frontend bundle served by a slim container image,
  talking to the service over HTTP.
- **Helm** - a Kubernetes chart for the hosted instance (single replica +
  ReadWriteOnce volume, since SQLite is a single writer).

The desktop app can also be **repointed** at a remote instance (set an instance
URL in Settings) - it then talks HTTP to that instance and ignores its embedded
service. No separate build.

## Storage - where data lands

All writable locations resolve through one path layer; nothing is ever written
somewhere unexpected:

- **Portable mode** - a `.portable` dir beside the binary (or
  `POSEIDON_PORTABLE_MODE=true`) confines everything - DB, logs, cache - beneath
  `./.portable/`.
- **Desktop** - SQLite under the OS app-data directory.
- **Container** - `POSEIDON_DATA_DIR` points at a mounted volume.

The database is SQLite, statically bundled (no runtime DB dependency); the schema
auto-provisions on first run wherever the DB lands.

## Secrets

A provider token is read from the environment variable named by the team's **Token
env var** - never stored in config, never persisted to the DB, never sent to the
browser. It differs by provider:

- **Azure DevOps** - a read-only PAT (Work Items Read + Build Read is sufficient;
  don't grant more), in `POSEIDON_AZURE_PAT` by default. Interactive sign-in
  instead brokers a token via the Azure CLI, which POSEIDON never persists.
- **GitHub / GitLab** - **public** repos read with no token at all (anonymous
  polling); set a read-only token only for private repos or to lift API rate
  limits.

See [Setup](setup.md).

## Configuration

There's no config file. Teams, rules, tags, and saved reports live in the
**database** (per owner) - edited in the GUI (applied without a restart), or
backed up / seeded with `poseidon config export` / `import` (portable YAML).
Instance settings (bind/port/poll, telemetry) come from **env vars**.

## AI backends

POSEIDON can optionally suggest tags with an AI model - though it never needs one:
the deterministic keyword/alias rules ([Rules](rules.md)) do the job with no AI.
When enabled, three backend kinds are offered, picked as a reorderable priority
list (first supported-and-configured one wins):

- **On-device embedded** - a small Qwen2.5 model run in-process (`candle`); CPU,
  or an NVIDIA GPU with the `cuda` build. Nothing leaves the machine.
- **In-browser (WebGPU)** - the model runs on your GPU inside the browser
  (Chrome/Edge); experimental, no server GPU or install.
- **Cloud** - Claude (Anthropic), OpenAI, or Gemini, with your own API key.

Suggestions are advisory - a person reviews and applies them on the Work Items
screen, never auto-applied. Configure them in the first-run AI step or later in
Settings; see [Setup](setup.md) for the details.

## Documentation screenshots

The screenshots in these pages are captured against the **demo tenant** - a team
backed by the built-in offline `stub` provider
([`tenants/demo-data.poseidon.import.yaml`](../../tenants/demo-data.poseidon.import.yaml)),
never a real remote work tracker. They're taken from the running web UI - the Helm
**`localhost` playground** (`./poseidon.sh up` with the localhost values) stands up
a demo-seeded instance, plus a web client pointed at it, to shoot against.

The capture itself is an IdleOps playbook,
[`tools/screenshots/poseidon-web-screenshots.idleops.yaml`](../../tools/screenshots/poseidon-web-screenshots.idleops.yaml),
which drives a chrome-less browser window over the playground server and shoots the
six in-app screens. The four onboarding shots aren't in it: the onboarding overlay
only renders in desktop mode (`mode() === 'desktop'`), so those stay a desktop-app
capture until the web/remote empty-state onboarding lands.

## Credits

POSEIDON is a Product Owner support tool for backlog hygiene + flow. Nautical
name, nautical mottos - the CLI banner and the app pick one at random per launch
("Weather the storm", and friends).

## See also

- [Setup](setup.md) - sign-in, teams, config.
- [SCOPE.md](../SCOPE.md) - what POSEIDON deliberately is not.
- [DISTRIBUTION.md](../DISTRIBUTION.md) - the full deploy-target detail.
