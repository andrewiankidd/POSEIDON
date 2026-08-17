# CLI guide - `poseidon`

The `poseidon` CLI drives the **same logic** as the web and desktop apps (the
shared `Service`) over the same local SQLite store, across every provider (Azure
DevOps, GitHub, GitLab). It needs only read access - an Azure DevOps PAT with
read scopes, or a GitHub / GitLab token (none at all for public repos) - so it
runs anywhere that credential env var is set: a laptop, a cron job, or a CI stage.

```
poseidon [--json] [--team <name>] [--owner <email>] <command>

Commands:
  poll     Poll every configured team once and report what was fetched
  lint     Evaluate backlog hygiene and print flags (exits 1 on error-severity)
  report   Print the work-item + pipeline flow report for a date range
  config   Export / import configuration (teams, rules, reports) as YAML
  tag      Generate + store advisory AI tag suggestions over the backlog
```

Global flags:

- `--json` - emit machine-readable JSON instead of human text (for piping to
  `jq`, dashboards, or another tool). Logs go to stderr, so stdout stays clean.
- `--team <name>` - scope `lint` / `report` / `tag` to a single team (its
  configured `name`). Omit for all teams. `poll` always polls every team.
- `--owner <email>` - run as a specific tenant on a multi-tenant instance (rows
  are keyed by owner, the user's auth email). Omit for the single-tenant
  `default` owner. Mirrors `config import --owner`.

Every command prints the POSEIDON emblem + a motto to stderr first (suppressed
under `--json`), so stdout stays machine-parseable.

Setup is the same as the server: config (teams, rules) lives in the local DB and
any provider credential in an env var (Azure DevOps defaults to
`POSEIDON_AZURE_PAT`; GitHub / GitLab name an optional token env var, or none for
public repos). In a fresh/CI environment,
load config declaratively with `poseidon config import config.yaml` (export it
elsewhere with `poseidon config export`); the shared DB is otherwise empty.

---

## Use case 1 - a quick hygiene check on your machine

Poll fresh and see what's out of hygiene, right now:

```bash
export POSEIDON_AZURE_PAT=<read-only-pat>
poseidon lint
```

```
12 hygiene flag(s):
  [ERROR] #4321 (Platform) - missing required tag matching "team:*"
  [ERROR] #4390 (Platform) - item has no tags
  [warn]  #4118 (Platform) - tag "wip" is on the disallowed list
  [warn]  #4290 (Platform) - stale: 9 days in "In Progress" (limit 5)
  [warn]  #4302 (Data)     - tag "spike" is not on the allowed list
  ...

3 error(s), 9 warning(s).
```

When everything's clean:

```
✓ No hygiene flags - backlog is clean.
```

To evaluate what's already stored without polling first (fast, offline):

```bash
poseidon lint --no-poll
```

---

## Use case 2 - gate a CI pipeline on backlog hygiene

`poseidon lint` **exits non-zero when any error-severity flag is present**, so a
pipeline stage can fail the build when the backlog drifts. Warnings don't fail
the build; only errors do (tune which rules are errors via `untagged_is_error`
and the required-tags list in your rules).

Azure Pipelines:

```yaml
- script: |
    curl -sSL -o poseidon.tar.gz \
      https://github.com/andrewiankidd/POSEIDON/releases/latest/download/poseidon-cli-linux.tar.gz
    tar xzf poseidon.tar.gz --strip-components=1
    ./poseidon lint
  displayName: Backlog hygiene gate
  env:
    POSEIDON_AZURE_PAT: $(POSEIDON_AZURE_PAT)   # read-only PAT from a secret
```

GitHub Actions:

```yaml
- name: Backlog hygiene gate
  run: ./poseidon lint
  env:
    POSEIDON_AZURE_PAT: ${{ secrets.POSEIDON_AZURE_PAT }}
```

The step fails (red) when there are error-severity flags, and its log lists
exactly which items and why.

---

## Use case 3 - a report for standup / retro

```bash
poseidon report --from 2026-07-01 --to 2026-07-31
```

```
Report 2026-07-01 → 2026-07-31

Work items:
  opened: 34
  closed: 41
  closed by tag:
    team:platform            18
    type:bug                 15
    type:feature             12
    priority:p1               6
    (untagged)                2

Pipelines:
  runs: 96
  succeeded / failed / canceled: 80 / 12 / 4
  success rate: 87.0%
```

Omit the dates to default to the last 30 days. Add `--poll` to fetch fresh data
before reporting (otherwise it reports over whatever's in the local store):

```bash
poseidon report --poll
```

---

## Use case 4 - machine output for dashboards / jq

Every command takes `--json`. Stdout carries only the JSON; logs go to stderr.

```bash
poseidon --json poll
```

```json
{
  "teams_polled": 2,
  "work_items": 148,
  "pipelines": 6,
  "runs": 96,
  "pull_requests": 12,
  "errors": []
}
```

Pull just the failing pipeline count from a report:

```bash
poseidon --json report --from 2026-07-01 --to 2026-07-31 | jq '.pipelines.failed'
```

```
12
```

List every error-severity flag as JSON for an external notifier:

```bash
poseidon --json lint --no-poll | jq '[.[] | select(.severity == "error")]'
```

---

## Use case 5 - portable / air-gapped run

Drop a `.portable` file next to the binary (or set
`POSEIDON_PORTABLE_MODE=true`) and everything - the SQLite DB, logs, cache -
stays under `./.portable/`. Nothing is written to your home directory.

```bash
POSEIDON_PORTABLE_MODE=true poseidon poll
```

Useful for running POSEIDON from a USB stick, or in an environment where you
don't want it touching system paths.

---

## Use case 6 - regenerate AI tag suggestions headlessly

`poseidon tag` runs the owner's configured AI tagger over the backlog and
**stores** the results as advisory suggestions. Nothing is applied - a person
reviews and applies them in the GUI (the tool never auto-applies tags). Polls
fresh first unless `--no-poll`.

```bash
poseidon tag                          # tag the whole scope
poseidon tag --team Platform          # one team
poseidon tag --assignee "Andrew Kidd" # only items assigned to a person
```

`--assignee` is a case-insensitive substring of the display name. On a
multi-tenant instance, target a tenant with the global `--owner <email>`.

The backend is whatever that owner configured in Settings: a hosted/cloud
provider, or the embedded on-device model. Build the CLI with `--features cuda`
on an NVIDIA host to run the embedded model on the GPU - this is the headless GPU
path (the in-browser WebGPU backend is browser-only and is not reachable from the
CLI). Add `--json` to print the per-item suggestions instead of a summary.

---

## Exit codes

| Command | Exit 0 | Exit 1 |
|---------|--------|--------|
| `poll`   | always (per-project errors are reported, not fatal) | unexpected failure (bad config, DB error) |
| `lint`   | no error-severity flags | one or more **error**-severity flags |
| `report` | always | unexpected failure |
| `config` | import/export succeeded | bad file / DB error |
| `tag`    | run completed (per-item errors logged, not fatal) | AI not configured, or unexpected failure |

Only `lint` uses the exit code as a signal - that's the CI-gating contract.
