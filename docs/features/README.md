# Features

> Part of the [POSEIDON documentation hub](../README.md). For how to run and
> debug the app, see [Running & debugging](../RUNNING.md).

Per-feature documentation. Each page covers the desktop/web **GUI** and, where
one exists, the **CLI** surface side by side. POSEIDON is GUI-first: the CLI is a
focused tool (`poll`, `lint`, `report`, `config`, `tag`) for refresh, CI gating,
flow reporting, config import/export, and headless tag suggestions, so some
screens are GUI-only.

POSEIDON reads from three work trackers - **Azure DevOps, GitHub, and GitLab**
(public GitHub / GitLab poll with no token). Reading, hygiene flags, and reports
work across all three; the inline write-backs (State / Tags edits, work-item ↔ PR
links) are **Azure DevOps only**.

The sidebar groups these screens the same way this index is ordered: **Dashboard**
on its own, then the trackers (**Work Items / Pull Requests / Pipelines**), then
**Rules**, then the shareable outputs (**Reports / Recap**) below it.

| Feature | What it does |
|---------|--------------|
| [Dashboard](dashboard.md) | At-a-glance counts + a Health-check breakdown of every hygiene flag. |
| [Work Items](work-items.md) | The backlog as a sortable, filterable, inline-editable table with hygiene flags. |
| [Pull Requests](pull-requests.md) | In-flight PRs, their linked work items, and PR hygiene flags. |
| [Pipelines](pipelines.md) | Pipeline health - last status, failing / never-run flags. |
| [Rules](rules.md) | The hygiene policy every flag is evaluated against, per team. |
| [Reports](reports.md) | Configurable report engine (datasource, group, render) + a CLI flow summary. |
| [Recap](recap.md) | A shareable highlights deck built from your closed work, exportable as a self-contained HTML file. |
| [Setup](setup.md) | Providers, sign-in, teams, and configuration. |
| [User Guide](user-guide.md) | Portable mode, deployment targets, storage, secrets, credits. |

For what POSEIDON deliberately is *not*, see [SCOPE.md](../SCOPE.md); for what
works today, [PROJECT_STATUS.md](../PROJECT_STATUS.md).
