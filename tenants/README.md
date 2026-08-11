# tenants/

Portable **config bundles** (`*.poseiden.import.yaml`) - one per tenant. Each is
the YAML that `poseiden config import` consumes: teams, rules, doctor checks,
poll scope. Bundles carry **config only, never data** (work items come from
polling the provider) and **no secrets** (the Azure PAT is read from the env var
named in `auth.pat_env`).

On a multi-tenant instance each bundle is imported under its own **owner**, so
several coexist side by side:

| Bundle | Committed? | Owner | Provider | Purpose |
|--------|-----------|-------|----------|---------|
| `demo-data.poseiden.import.yaml` | ✅ yes | `poseiden+demo-data@example.com` | `stub` | Offline showcase + docs screenshots + a populated first look. Data is synthesised by the StubProvider. |
| *(the e2e run)* | - | throwaway | `stub` | The e2e imports the demo bundle under its own owner, asserts exact counts, then tears down. |
| `azuredevops.poseiden.import.yaml` | 🚫 gitignored | your email | `azure-dev-ops` | Your real team(s). Author your own; it stays local. |

## Add your own tenant

1. Copy the demo bundle to `<name>.poseiden.import.yaml` (anything but
   `demo-data` is gitignored automatically).
2. Point it at your provider (org / project / area path) and rules.
3. Set the PAT env var it names, then import:

   ```bash
   export POSEIDEN_AZURE_PAT=...            # the value of auth.pat_env
   poseiden config import tenants/<name>.poseiden.import.yaml --replace
   poseiden poll
   ```

Only `demo-data.poseiden.import.yaml` is tracked (see `.gitignore`).
