# Azure DevOps response fixtures

Real Azure DevOps REST responses, captured live and **anonymised**, used by the
`*_fixture_*` tests in [`../azure.rs`](../azure.rs) to pin our deserialisation +
normalisation against the true response shape (which hand-written JSON drifts
from). These already caught a real bug: PR artifact-link separators arrive in
mixed case (`%2F` and `%2f`) within a single payload.

Every identifier is scrubbed: organisation, projects, repositories, people
(display names, emails, descriptors, avatars), GUIDs (mapped deterministically so
cross-references stay intact), commit SHAs, work-item titles, area/iteration
paths, and branch names. No real data ships here.

| File | Endpoint |
|------|----------|
| `workitemsbatch.json` | `POST wit/workitemsbatch` (`$expand=relations`) |
| `pullrequests.json` | `GET git/pullrequests` (`searchCriteria.status=all`) |
| `pullrequest_by_id.json` | `GET git/pullrequests/{id}` (org-wide) |
| `definitions.json` | `GET build/definitions` (`includeLatestBuilds=true`) |
| `builds.json` | `GET build/builds` |

To refresh: re-capture against a live instance, then run the same anonymisation
pass (strip every identifier as above) before committing. Never commit a raw
capture.
