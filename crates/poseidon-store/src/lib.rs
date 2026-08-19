//! POSEIDON persistence - SQLite via `sqlx`.
//!
//! Migrations are embedded in the binary (`sqlx::migrate!`), so first run
//! auto-provisions the schema wherever the DB file lands - desktop app-data
//! dir, container PV, or a portable `.portable/` directory. No external DB, no
//! `sqlx-cli` prepare step: queries are runtime-checked, keeping the build and
//! CI free of a database dependency.
//!
//! The [`Store`] is the whole persistence surface. It's a thin, typed layer
//! over the pool - upserts for the pollers, list queries for the views, and two
//! date-range report aggregates. Everything is scoped by `owner` (see the
//! migration for why). Swapping SQLite for Postgres later means a second impl
//! of this same shape, not a rewrite.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use poseidon_core::{
    AiActivityRecord, AiFieldDraft, CatalogEntity, Pipeline, PipelineReport, PipelineRun, PrStatus,
    PullRequest, ReportSpec, RunStatus, TagCount, TicketReport, UserConfig, WorkItem,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{FromRow, Row, SqlitePool};

/// Persistence errors. Coarse on purpose - callers log + skip.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

type Result<T> = std::result::Result<T, StoreError>;

/// A connected, migrated SQLite store. Cheap to clone (wraps an `Arc`ed pool).
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open (creating if absent) the SQLite database at `path`, run pending
    /// migrations, and return a ready store. WAL journalling is enabled for
    /// better concurrent read/write behaviour under the single-writer server.
    pub async fn connect(path: &std::path::Path) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;
        Self::from_pool(pool).await
    }

    /// In-memory store for tests. `max_connections(1)` so the single shared
    /// connection keeps the schema alive for the pool's lifetime.
    pub async fn connect_in_memory() -> Result<Self> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        Self::from_pool(pool).await
    }

    async fn from_pool(pool: SqlitePool) -> Result<Self> {
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    // ─────────────────────────── Upserts ───────────────────────────
    // Each poll replaces what it saw. `INSERT ... ON CONFLICT DO UPDATE` keeps
    // the primary key stable so re-polling an item updates it in place rather
    // than duplicating.

    /// Upsert a batch of work items for `owner`. Runs in one transaction so a
    /// poll is atomic - a mid-batch failure leaves the previous state intact.
    /// Used for single-item write-backs (edits, links); the poll uses
    /// [`Store::replace_team_work_items`] so out-of-scope items get pruned.
    pub async fn upsert_work_items(&self, owner: &str, items: &[WorkItem]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        insert_work_items(&mut tx, owner, items).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Replace a team's stored work items with exactly `items` - upsert them and
    /// delete any previously-stored rows for that team no longer in the set. This
    /// is what keeps the store honest when a team's query scope tightens (e.g.
    /// strict area path), so items that fell out of scope don't linger. Atomic:
    /// a failure rolls back, leaving the previous state intact. Only call after a
    /// SUCCESSFUL fetch - an errored fetch must not be treated as "empty".
    pub async fn replace_team_work_items(
        &self,
        owner: &str,
        team: &str,
        items: &[WorkItem],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM work_items WHERE owner = ? AND team = ?")
            .bind(owner)
            .bind(team)
            .execute(&mut *tx)
            .await?;
        insert_work_items(&mut tx, owner, items).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Upsert pipelines for `owner`.
    pub async fn upsert_pipelines(&self, owner: &str, pipelines: &[Pipeline]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        insert_pipelines(&mut tx, owner, pipelines).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Replace a team's stored pipelines with exactly `pipelines` - the same
    /// prune-on-poll contract as [`Self::replace_team_work_items`], so a pipeline
    /// that fell out of the team's scope (e.g. after an area-path change, or a
    /// deleted definition) doesn't linger on the pipelines screen. Atomic; only
    /// call after a SUCCESSFUL fetch, never on an errored one (which would look
    /// like "empty" and wipe the team). Runs are deliberately NOT pruned this way
    /// - they're a time-windowed history that feeds flow reports.
    pub async fn replace_team_pipelines(
        &self,
        owner: &str,
        team: &str,
        pipelines: &[Pipeline],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM pipelines WHERE owner = ? AND team = ?")
            .bind(owner)
            .bind(team)
            .execute(&mut *tx)
            .await?;
        insert_pipelines(&mut tx, owner, pipelines).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Upsert pipeline runs for `owner`.
    pub async fn upsert_runs(&self, owner: &str, runs: &[PipelineRun]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for r in runs {
            sqlx::query(
                "INSERT INTO pipeline_runs
                    (owner, provider, team, id, pipeline_id, status, started_at,
                     finished_at, source_branch, url)
                 VALUES (?,?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(owner, provider, team, id) DO UPDATE SET
                    pipeline_id=excluded.pipeline_id, status=excluded.status,
                    started_at=excluded.started_at, finished_at=excluded.finished_at,
                    source_branch=excluded.source_branch, url=excluded.url",
            )
            .bind(owner)
            .bind(&r.provider)
            .bind(&r.team)
            .bind(r.id)
            .bind(r.pipeline_id)
            .bind(status_to_str(r.status))
            .bind(r.started_at.map(|d| d.to_rfc3339()))
            .bind(r.finished_at.map(|d| d.to_rfc3339()))
            .bind(&r.source_branch)
            .bind(&r.url)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // ─────────────────────────── Lists ───────────────────────────

    /// Work items for `owner`, most-recently-changed first. When `team` is
    /// `Some`, scope to that team; `None` returns every team. The
    /// `team = COALESCE(?, team)` idiom binds the optional filter once - a NULL
    /// bind degrades to `team = team` (always true), i.e. no filter.
    pub async fn list_work_items(&self, owner: &str, team: Option<&str>) -> Result<Vec<WorkItem>> {
        let rows = sqlx::query_as::<_, WorkItemRow>(
            "SELECT * FROM work_items
             WHERE owner = ? AND team = COALESCE(?, team)
             ORDER BY changed_at DESC",
        )
        .bind(owner)
        .bind(team)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(WorkItemRow::into_domain).collect())
    }

    /// Work-item ids (in `team` scope) whose origin area hasn't been fetched yet,
    /// capped at `limit` - the backfill worklist for the "moved-in" source signal.
    pub async fn work_item_ids_missing_origin(
        &self,
        owner: &str,
        team: Option<&str>,
        limit: i64,
    ) -> Result<Vec<i64>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT w.id FROM work_items w
             LEFT JOIN work_item_origin o ON o.owner = w.owner AND o.work_item_id = w.id
             WHERE w.owner = ? AND w.team = COALESCE(?, w.team) AND o.work_item_id IS NULL
             LIMIT ?",
        )
        .bind(owner)
        .bind(team)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Record fetched origin areas (id -> first-revision area path). Immutable, so
    /// this only ever inserts; a re-fetch of the same id is a harmless no-op.
    pub async fn set_origins(&self, owner: &str, origins: &[(i64, String)]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for (id, area) in origins {
            sqlx::query(
                "INSERT INTO work_item_origin (owner, work_item_id, origin_area) VALUES (?,?,?)
                 ON CONFLICT(owner, work_item_id) DO UPDATE SET origin_area = excluded.origin_area",
            )
            .bind(owner)
            .bind(id)
            .bind(area)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Origin area per work-item id for the given scope (id -> origin area path).
    pub async fn origins(
        &self,
        owner: &str,
        team: Option<&str>,
    ) -> Result<std::collections::HashMap<i64, String>> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT o.work_item_id, o.origin_area FROM work_item_origin o
             JOIN work_items w ON w.owner = o.owner AND w.id = o.work_item_id
             WHERE o.owner = ? AND w.team = COALESCE(?, w.team)",
        )
        .bind(owner)
        .bind(team)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// Replace this owner's service-catalog snapshot with `entities`. A sync is a
    /// wholesale replace (like a poll rewrites work items), so the old rows are
    /// cleared first. Only repo-bearing entities are stored - the table is
    /// repo-keyed (the join to `WorkItem.linked_repos`); a duplicate repo in the
    /// input keeps the last row. Returns the number of rows stored.
    pub async fn replace_catalog(&self, owner: &str, entities: &[CatalogEntity]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM catalog WHERE owner = ?")
            .bind(owner)
            .execute(&mut *tx)
            .await?;
        for e in entities {
            let Some(repo) = e.repo.as_deref() else {
                continue;
            };
            sqlx::query(
                "INSERT INTO catalog (owner, repo, product, team, domain, kind)
                 VALUES (?,?,?,?,?,?)
                 ON CONFLICT(owner, repo) DO UPDATE SET
                   product = excluded.product, team = excluded.team,
                   domain = excluded.domain, kind = excluded.kind",
            )
            .bind(owner)
            .bind(repo)
            .bind(e.product.as_deref())
            .bind(e.team.as_deref())
            .bind(e.domain.as_deref())
            .bind(e.kind.as_deref())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.catalog_count(owner).await
    }

    /// This owner's catalog rows (repo-keyed), sorted by repo. Raw product ids -
    /// canonicalisation to `product:*` happens in the tagging layer.
    pub async fn catalog(&self, owner: &str) -> Result<Vec<CatalogEntity>> {
        // (repo, product, team, domain, kind) - aliased to keep clippy's type-complexity
        // lint happy and the mapping below readable.
        type CatalogRow = (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<CatalogRow> = sqlx::query_as(
            "SELECT repo, product, team, domain, kind FROM catalog
                 WHERE owner = ? ORDER BY repo",
        )
        .bind(owner)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(repo, product, team, domain, kind)| CatalogEntity {
                repo: Some(repo),
                product,
                team,
                domain,
                kind,
            })
            .collect())
    }

    /// Number of catalog rows for this owner (sync status / "is it populated?").
    pub async fn catalog_count(&self, owner: &str) -> Result<u64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM catalog WHERE owner = ?")
            .bind(owner)
            .fetch_one(&self.pool)
            .await?;
        Ok(n as u64)
    }

    /// Replace the AI-suggested tags for one work item. `suggestions` is
    /// `(tag, reason)` pairs; an empty slice clears the item's suggestions.
    pub async fn set_ai_suggestions(
        &self,
        owner: &str,
        team: &str,
        work_item_id: i64,
        suggestions: &[(String, String)],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM ai_tag_suggestions WHERE owner = ? AND work_item_id = ?")
            .bind(owner)
            .bind(work_item_id)
            .execute(&mut *tx)
            .await?;
        for (tag, reason) in suggestions {
            sqlx::query(
                "INSERT OR REPLACE INTO ai_tag_suggestions
                   (owner, team, work_item_id, tag, reason) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(owner)
            .bind(team)
            .bind(work_item_id)
            .bind(tag)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// AI suggestions for `owner`, optionally scoped to one team, grouped by
    /// work-item id -> `(tag, reason)` pairs. Read on the way to merging them into
    /// each work item's `tag_suggestions` for the Suggested column.
    pub async fn ai_suggestions(
        &self,
        owner: &str,
        team: Option<&str>,
    ) -> Result<std::collections::HashMap<i64, Vec<(String, String)>>> {
        let rows = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT work_item_id, tag, reason FROM ai_tag_suggestions
             WHERE owner = ? AND team = COALESCE(?, team)",
        )
        .bind(owner)
        .bind(team)
        .fetch_all(&self.pool)
        .await?;
        let mut map: std::collections::HashMap<i64, Vec<(String, String)>> =
            std::collections::HashMap::new();
        for (id, tag, reason) in rows {
            map.entry(id).or_default().push((tag, reason));
        }
        Ok(map)
    }

    /// Replace the AI healthcheck findings for one work item. `findings` is
    /// `(kind, detail)` pairs; an empty slice clears the item's findings (a clean
    /// re-audit). Mirrors [`Self::set_ai_suggestions`]: delete-all then re-insert,
    /// so a re-run never leaves stale concerns behind.
    pub async fn set_ai_audit(
        &self,
        owner: &str,
        team: &str,
        work_item_id: i64,
        findings: &[(String, String)],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM ai_audit_findings WHERE owner = ? AND work_item_id = ?")
            .bind(owner)
            .bind(work_item_id)
            .execute(&mut *tx)
            .await?;
        for (kind, detail) in findings {
            sqlx::query(
                "INSERT INTO ai_audit_findings
                   (owner, team, work_item_id, kind, detail) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(owner)
            .bind(team)
            .bind(work_item_id)
            .bind(kind)
            .bind(detail)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// AI healthcheck findings for `owner`, optionally scoped to one team, grouped
    /// by work-item id -> `(kind, detail)` pairs. Read on the way to merging them
    /// into the flag stream as `ai_audit` flags.
    pub async fn ai_audit(
        &self,
        owner: &str,
        team: Option<&str>,
    ) -> Result<std::collections::HashMap<i64, Vec<(String, String)>>> {
        let rows = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT work_item_id, kind, detail FROM ai_audit_findings
             WHERE owner = ? AND team = COALESCE(?, team)",
        )
        .bind(owner)
        .bind(team)
        .fetch_all(&self.pool)
        .await?;
        let mut map: std::collections::HashMap<i64, Vec<(String, String)>> =
            std::collections::HashMap::new();
        for (id, kind, detail) in rows {
            map.entry(id).or_default().push((kind, detail));
        }
        Ok(map)
    }

    // ── Durable "Improve all" field drafts (owner-scoped) ───────────────────────

    /// Replace the pending improve-all drafts for one work item with `drafts`
    /// (`reference` -> proposed `value`). An empty slice just clears them.
    pub async fn set_ai_field_drafts(
        &self,
        owner: &str,
        team: &str,
        work_item_id: i64,
        drafts: &[AiFieldDraft],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM ai_field_drafts WHERE owner = ? AND work_item_id = ?")
            .bind(owner)
            .bind(work_item_id)
            .execute(&mut *tx)
            .await?;
        for d in drafts {
            sqlx::query(
                "INSERT INTO ai_field_drafts (owner, team, work_item_id, field_ref, value)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(owner)
            .bind(team)
            .bind(work_item_id)
            .bind(&d.reference)
            .bind(&d.value)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// All pending improve-all drafts for `owner` (optionally one team), grouped by
    /// work-item id. The frontend uses the keys for the ✨ badges and the values to
    /// pre-fill the editor's review panes.
    pub async fn ai_field_drafts(
        &self,
        owner: &str,
        team: Option<&str>,
    ) -> Result<std::collections::HashMap<i64, Vec<AiFieldDraft>>> {
        let rows = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT work_item_id, field_ref, value FROM ai_field_drafts
             WHERE owner = ? AND team = COALESCE(?, team)
             ORDER BY work_item_id, field_ref",
        )
        .bind(owner)
        .bind(team)
        .fetch_all(&self.pool)
        .await?;
        let mut map: std::collections::HashMap<i64, Vec<AiFieldDraft>> =
            std::collections::HashMap::new();
        for (id, reference, value) in rows {
            map.entry(id)
                .or_default()
                .push(AiFieldDraft { reference, value });
        }
        Ok(map)
    }

    /// Clear the pending drafts for one work item (called when it's reviewed/applied).
    pub async fn clear_ai_field_drafts(&self, owner: &str, work_item_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM ai_field_drafts WHERE owner = ? AND work_item_id = ?")
            .bind(owner)
            .bind(work_item_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── AI activity log (owner-scoped): queue-across-refresh + audit trail ───────

    /// Upsert one activity record by (owner, id) - inserted on job start, updated as it
    /// progresses and on completion, so the queue survives a refresh.
    pub async fn upsert_ai_activity(&self, owner: &str, rec: &AiActivityRecord) -> Result<()> {
        let items = serde_json::to_string(&rec.items).unwrap_or_else(|_| "[]".to_string());
        sqlx::query(
            "INSERT INTO ai_activity
               (id, owner, team, name, where_at, status, done, total, outcome, items_json, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'))
             ON CONFLICT(owner, id) DO UPDATE SET
               team=excluded.team, name=excluded.name, where_at=excluded.where_at,
               status=excluded.status, done=excluded.done, total=excluded.total,
               outcome=excluded.outcome, items_json=excluded.items_json,
               updated_at=datetime('now')",
        )
        .bind(&rec.id)
        .bind(owner)
        .bind(&rec.team)
        .bind(&rec.name)
        .bind(&rec.where_at)
        .bind(&rec.status)
        .bind(rec.done)
        .bind(rec.total)
        .bind(&rec.outcome)
        .bind(items)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The owner's most-recent activity records (newest first, capped at `limit`).
    pub async fn list_ai_activity(&self, owner: &str, limit: i64) -> Result<Vec<AiActivityRecord>> {
        let rows = sqlx::query_as::<_, (String, String, String, String, String, i64, i64, String, String, String, String)>(
            "SELECT id, team, name, where_at, status, done, total, outcome, items_json, started_at, updated_at
             FROM ai_activity WHERE owner = ? ORDER BY updated_at DESC LIMIT ?",
        )
        .bind(owner)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    id,
                    team,
                    name,
                    where_at,
                    status,
                    done,
                    total,
                    outcome,
                    items_json,
                    started_at,
                    updated_at,
                )| {
                    AiActivityRecord {
                        id,
                        team,
                        name,
                        where_at,
                        status,
                        done,
                        total,
                        outcome,
                        items: serde_json::from_str(&items_json)
                            .unwrap_or(serde_json::Value::Array(vec![])),
                        started_at,
                        updated_at,
                    }
                },
            )
            .collect())
    }

    /// Replace the near-duplicate findings for a team scope in one shot: delete the
    /// scope's existing rows, then insert `findings` = `(work_item_id, team, detail)`.
    /// A `None` team replaces every team's findings for the owner (an all-teams scan);
    /// `Some(team)` replaces just that team's. Owner-scoped.
    pub async fn replace_near_duplicates(
        &self,
        owner: &str,
        team: Option<&str>,
        findings: &[(i64, String, String)],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM near_duplicate_findings WHERE owner = ? AND team = COALESCE(?, team)",
        )
        .bind(owner)
        .bind(team)
        .execute(&mut *tx)
        .await?;
        for (work_item_id, item_team, detail) in findings {
            sqlx::query(
                "INSERT INTO near_duplicate_findings
                   (owner, team, work_item_id, detail) VALUES (?, ?, ?, ?)",
            )
            .bind(owner)
            .bind(item_team)
            .bind(work_item_id)
            .bind(detail)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Near-duplicate findings for `owner`, optionally scoped to one team, grouped by
    /// work-item id -> detail. Read on the way to merging them into the flag stream.
    pub async fn near_duplicates(
        &self,
        owner: &str,
        team: Option<&str>,
    ) -> Result<std::collections::HashMap<i64, String>> {
        let rows = sqlx::query_as::<_, (i64, String)>(
            "SELECT work_item_id, detail FROM near_duplicate_findings
             WHERE owner = ? AND team = COALESCE(?, team)",
        )
        .bind(owner)
        .bind(team)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// Pipelines for `owner`, optionally scoped to one team.
    pub async fn list_pipelines(&self, owner: &str, team: Option<&str>) -> Result<Vec<Pipeline>> {
        let rows = sqlx::query_as::<_, PipelineRow>(
            "SELECT * FROM pipelines
             WHERE owner = ? AND team = COALESCE(?, team)
             ORDER BY name",
        )
        .bind(owner)
        .bind(team)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(PipelineRow::into_domain).collect())
    }

    /// Upsert pull requests for `owner`.
    pub async fn upsert_pull_requests(&self, owner: &str, prs: &[PullRequest]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        insert_pull_requests(&mut tx, owner, prs).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Replace a team's stored pull requests with exactly `prs` - the prune-on-
    /// poll contract (see [`Self::replace_team_work_items`]). A poll fetches the
    /// active set plus a bounded recent-closed window (the closed ones only colour
    /// work-item PR chips); replacing keeps the store an honest mirror of that, so
    /// a PR that left scope, or aged past the closed window, doesn't linger (its
    /// chip degrades to "unknown", as intended). Atomic; only after a SUCCESSFUL
    /// fetch.
    pub async fn replace_team_pull_requests(
        &self,
        owner: &str,
        team: &str,
        prs: &[PullRequest],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM pull_requests WHERE owner = ? AND team = ?")
            .bind(owner)
            .bind(team)
            .execute(&mut *tx)
            .await?;
        insert_pull_requests(&mut tx, owner, prs).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Pull requests for `owner`, optionally scoped to one team, newest first.
    pub async fn list_pull_requests(
        &self,
        owner: &str,
        team: Option<&str>,
    ) -> Result<Vec<PullRequest>> {
        let rows = sqlx::query_as::<_, PullRequestRow>(
            "SELECT * FROM pull_requests
             WHERE owner = ? AND team = COALESCE(?, team)
             ORDER BY created_at DESC, id DESC",
        )
        .bind(owner)
        .bind(team)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(PullRequestRow::into_domain).collect())
    }

    /// Runs for `owner` started at/after `since` (RFC3339), newest first,
    /// optionally scoped to one team.
    pub async fn list_runs(
        &self,
        owner: &str,
        since: DateTime<Utc>,
        team: Option<&str>,
    ) -> Result<Vec<PipelineRun>> {
        let rows = sqlx::query_as::<_, RunRow>(
            "SELECT * FROM pipeline_runs
             WHERE owner = ? AND team = COALESCE(?, team)
               AND (started_at IS NULL OR started_at >= ?)
             ORDER BY started_at DESC",
        )
        .bind(owner)
        .bind(team)
        .bind(since.to_rfc3339())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(RunRow::into_domain).collect())
    }

    // ─────────────────────────── Meta ───────────────────────────

    pub async fn set_meta(&self, owner: &str, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO meta (owner, key, value) VALUES (?,?,?)
             ON CONFLICT(owner, key) DO UPDATE SET value = excluded.value",
        )
        .bind(owner)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_meta(&self, owner: &str, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM meta WHERE owner = ? AND key = ?")
            .bind(owner)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("value")))
    }

    /// Remove a meta key for an owner (no-op if absent). Lets a caller reset a
    /// stored value back to its computed default (e.g. the seeded LLM registry).
    pub async fn delete_meta(&self, owner: &str, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM meta WHERE owner = ? AND key = ?")
            .bind(owner)
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ─────────────────────── Per-owner config ───────────────────────

    /// The stored [`UserConfig`] for an owner, or `None` if none saved yet.
    /// A malformed row degrades to `None` (the caller re-seeds a default).
    pub async fn get_user_config(&self, owner: &str) -> Result<Option<UserConfig>> {
        let row = sqlx::query("SELECT config_json FROM user_config WHERE owner = ?")
            .bind(owner)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| {
            serde_json::from_str::<UserConfig>(&r.get::<String, _>("config_json")).ok()
        }))
    }

    /// Insert or replace an owner's [`UserConfig`].
    pub async fn put_user_config(&self, owner: &str, config: &UserConfig) -> Result<()> {
        let json = serde_json::to_string(config).unwrap_or_else(|_| "{}".to_string());
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO user_config (owner, config_json, updated_at) VALUES (?,?,?)
             ON CONFLICT(owner) DO UPDATE SET
                config_json = excluded.config_json, updated_at = excluded.updated_at",
        )
        .bind(owner)
        .bind(json)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every owner that has a config row - the tenants the scheduler must poll.
    pub async fn list_config_owners(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT owner FROM user_config ORDER BY owner")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("owner"))
            .collect())
    }

    // ─────────────────────── Saved reports ───────────────────────

    /// Every saved (user-created) report spec for an owner, name-ordered.
    /// Malformed rows are skipped rather than failing the whole list.
    pub async fn list_reports(&self, owner: &str) -> Result<Vec<ReportSpec>> {
        let rows = sqlx::query("SELECT spec_json FROM reports WHERE owner = ? ORDER BY name")
            .bind(owner)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                serde_json::from_str::<ReportSpec>(&r.get::<String, _>("spec_json")).ok()
            })
            .collect())
    }

    /// One saved report by name, or `None`.
    pub async fn get_report(&self, owner: &str, name: &str) -> Result<Option<ReportSpec>> {
        let row = sqlx::query("SELECT spec_json FROM reports WHERE owner = ? AND name = ?")
            .bind(owner)
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| {
            serde_json::from_str::<ReportSpec>(&r.get::<String, _>("spec_json")).ok()
        }))
    }

    /// Insert or replace a saved report (keyed by owner + spec name).
    pub async fn upsert_report(&self, owner: &str, spec: &ReportSpec) -> Result<()> {
        let json = serde_json::to_string(spec).unwrap_or_else(|_| "{}".to_string());
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO reports (owner, name, spec_json, created_at, updated_at)
             VALUES (?,?,?,?,?)
             ON CONFLICT(owner, name) DO UPDATE SET
                spec_json = excluded.spec_json,
                updated_at = excluded.updated_at",
        )
        .bind(owner)
        .bind(&spec.name)
        .bind(json)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replace an owner's saved reports with exactly `reports` (config import,
    /// replace semantics). Atomic: clears the owner's set, then inserts the new
    /// one, in one transaction.
    pub async fn replace_reports(&self, owner: &str, reports: &[ReportSpec]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM reports WHERE owner = ?")
            .bind(owner)
            .execute(&mut *tx)
            .await?;
        let now = Utc::now().to_rfc3339();
        for spec in reports {
            let json = serde_json::to_string(spec).unwrap_or_else(|_| "{}".to_string());
            sqlx::query(
                "INSERT INTO reports (owner, name, spec_json, created_at, updated_at)
                 VALUES (?,?,?,?,?)",
            )
            .bind(owner)
            .bind(&spec.name)
            .bind(json)
            .bind(&now)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Delete a saved report; returns whether a row was removed.
    pub async fn delete_report(&self, owner: &str, name: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM reports WHERE owner = ? AND name = ?")
            .bind(owner)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    // ─────────────────────────── Reports ───────────────────────────

    /// Ticket-flow report over `[from, to]` (RFC3339 bounds). Whole-range
    /// totals plus a closed-by-tag breakdown. Tag grouping happens in Rust
    /// because tags are a JSON column - fine at PoC volume, and it keeps the
    /// SQL portable to Postgres later.
    pub async fn ticket_report(
        &self,
        owner: &str,
        from: &str,
        to: &str,
        team: Option<&str>,
    ) -> Result<TicketReport> {
        let opened: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work_items
             WHERE owner = ? AND team = COALESCE(?, team)
               AND created_at >= ? AND created_at <= ?",
        )
        .bind(owner)
        .bind(team)
        .bind(from)
        .bind(to)
        .fetch_one(&self.pool)
        .await?;

        // Closed-in-range items, with their tags, for both the count and the
        // breakdown.
        let closed_rows = sqlx::query(
            "SELECT tags FROM work_items
             WHERE owner = ? AND team = COALESCE(?, team) AND closed_at IS NOT NULL
               AND closed_at >= ? AND closed_at <= ?",
        )
        .bind(owner)
        .bind(team)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;

        let closed = closed_rows.len() as i64;
        let closed_by_tag = group_tags(closed_rows.iter().map(|r| r.get::<String, _>("tags")));

        Ok(TicketReport {
            from: from.to_string(),
            to: to.to_string(),
            opened,
            closed,
            closed_by_tag,
        })
    }

    /// Pipeline-flow report over `[from, to]` (RFC3339 bounds on `finished_at`).
    pub async fn pipeline_report(
        &self,
        owner: &str,
        from: &str,
        to: &str,
        team: Option<&str>,
    ) -> Result<PipelineReport> {
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT status, COUNT(*) FROM pipeline_runs
             WHERE owner = ? AND team = COALESCE(?, team) AND finished_at IS NOT NULL
               AND finished_at >= ? AND finished_at <= ?
             GROUP BY status",
        )
        .bind(owner)
        .bind(team)
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await?;

        let (mut succeeded, mut failed, mut canceled) = (0i64, 0i64, 0i64);
        for (status, count) in &rows {
            match status_from_str(status) {
                RunStatus::Succeeded => succeeded += count,
                RunStatus::Failed => failed += count,
                RunStatus::Canceled => canceled += count,
                _ => {}
            }
        }
        let completed = succeeded + failed;
        let success_rate = (completed > 0).then(|| succeeded as f64 / completed as f64);

        Ok(PipelineReport {
            from: from.to_string(),
            to: to.to_string(),
            total_runs: succeeded + failed + canceled,
            succeeded,
            failed,
            canceled,
            success_rate,
        })
    }
}

/// Insert/upsert work items onto an open transaction - the shared body of
/// [`Store::upsert_work_items`] and [`Store::replace_team_work_items`].
async fn insert_work_items(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    owner: &str,
    items: &[WorkItem],
) -> Result<()> {
    for wi in items {
        let tags = serde_json::to_string(&wi.tags).unwrap_or_else(|_| "[]".to_string());
        let linked = serde_json::to_string(&wi.linked_pr_ids).unwrap_or_else(|_| "[]".to_string());
        let repos = serde_json::to_string(&wi.linked_repos).unwrap_or_else(|_| "[]".to_string());
        sqlx::query(
            "INSERT INTO work_items
                (owner, provider, team, id, title, work_item_type, state, tags,
                 assigned_to, created_at, changed_at, closed_at, iteration_path,
                 story_points, url, linked_pr_ids, description, parent_id, linked_repos)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(owner, provider, team, id) DO UPDATE SET
                title=excluded.title, work_item_type=excluded.work_item_type,
                state=excluded.state, tags=excluded.tags,
                assigned_to=excluded.assigned_to, created_at=excluded.created_at,
                changed_at=excluded.changed_at, closed_at=excluded.closed_at,
                iteration_path=excluded.iteration_path,
                story_points=excluded.story_points, url=excluded.url,
                linked_pr_ids=excluded.linked_pr_ids, description=excluded.description,
                parent_id=excluded.parent_id, linked_repos=excluded.linked_repos",
        )
        .bind(owner)
        .bind(&wi.provider)
        .bind(&wi.team)
        .bind(wi.id)
        .bind(&wi.title)
        .bind(&wi.work_item_type)
        .bind(&wi.state)
        .bind(tags)
        .bind(&wi.assigned_to)
        .bind(wi.created_at.to_rfc3339())
        .bind(wi.changed_at.to_rfc3339())
        .bind(wi.closed_at.map(|d| d.to_rfc3339()))
        .bind(&wi.iteration_path)
        .bind(wi.story_points)
        .bind(&wi.url)
        .bind(linked)
        .bind(&wi.description)
        .bind(wi.parent_id)
        .bind(repos)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Upsert pipelines within an open transaction. Shared by [`Store::upsert_pipelines`]
/// (bare) and [`Store::replace_team_pipelines`] (after a delete), so both use one
/// column list + conflict clause.
async fn insert_pipelines(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    owner: &str,
    pipelines: &[Pipeline],
) -> Result<()> {
    for p in pipelines {
        sqlx::query(
            "INSERT INTO pipelines (owner, provider, team, id, name, folder, url, last_run_status, last_run_at, last_run_url)
             VALUES (?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(owner, provider, team, id) DO UPDATE SET
                name=excluded.name, folder=excluded.folder, url=excluded.url,
                last_run_status=excluded.last_run_status, last_run_at=excluded.last_run_at,
                last_run_url=excluded.last_run_url",
        )
        .bind(owner)
        .bind(&p.provider)
        .bind(&p.team)
        .bind(p.id)
        .bind(&p.name)
        .bind(&p.folder)
        .bind(&p.url)
        .bind(p.last_run_status.map(status_to_str))
        .bind(p.last_run_at.map(|d| d.to_rfc3339()))
        .bind(&p.last_run_url)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Upsert pull requests within an open transaction. Shared by
/// [`Store::upsert_pull_requests`] and [`Store::replace_team_pull_requests`].
async fn insert_pull_requests(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    owner: &str,
    prs: &[PullRequest],
) -> Result<()> {
    for pr in prs {
        sqlx::query(
            "INSERT INTO pull_requests
                (owner, provider, team, id, title, status, is_draft, repository, author,
                 created_at, source_branch, target_branch, reviewer_count, url)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(owner, provider, team, id) DO UPDATE SET
                title=excluded.title, status=excluded.status, is_draft=excluded.is_draft,
                repository=excluded.repository, author=excluded.author,
                created_at=excluded.created_at, source_branch=excluded.source_branch,
                target_branch=excluded.target_branch, reviewer_count=excluded.reviewer_count,
                url=excluded.url",
        )
        .bind(owner)
        .bind(&pr.provider)
        .bind(&pr.team)
        .bind(pr.id)
        .bind(&pr.title)
        .bind(pr.status.as_str())
        .bind(pr.is_draft as i64)
        .bind(&pr.repository)
        .bind(&pr.author)
        .bind(pr.created_at.map(|d| d.to_rfc3339()))
        .bind(&pr.source_branch)
        .bind(&pr.target_branch)
        .bind(pr.reviewer_count)
        .bind(&pr.url)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Serialise a [`RunStatus`] to its stored string. Kept explicit (not via
/// serde_json, which would add quotes) so the column holds `succeeded`, not
/// `"succeeded"`.
fn status_to_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Running => "running",
        RunStatus::Canceled => "canceled",
        RunStatus::Unknown => "unknown",
    }
}

fn status_from_str(s: &str) -> RunStatus {
    match s {
        "succeeded" => RunStatus::Succeeded,
        "failed" => RunStatus::Failed,
        "running" => RunStatus::Running,
        "canceled" => RunStatus::Canceled,
        _ => RunStatus::Unknown,
    }
}

/// Group a stream of JSON tag-arrays into descending `TagCount`s. Closed items
/// with no tags fall under a synthetic `(untagged)` bucket so the totals in the
/// report always add up to the closed count.
fn group_tags(tag_jsons: impl Iterator<Item = String>) -> Vec<TagCount> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, i64> = HashMap::new();
    for json in tag_jsons {
        let tags: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
        if tags.is_empty() {
            *counts.entry("(untagged)".to_string()).or_default() += 1;
        } else {
            for t in tags {
                *counts.entry(t).or_default() += 1;
            }
        }
    }
    let mut out: Vec<TagCount> = counts
        .into_iter()
        .map(|(tag, count)| TagCount { tag, count })
        .collect();
    // Descending count, then tag name for a stable order on ties.
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tag.cmp(&b.tag)));
    out
}

// ─────────────────────────── Row mappers ───────────────────────────
// Internal FromRow structs read TEXT timestamps as String, then convert to the
// domain type. Keeps the SQLite column types (all TEXT) decoupled from the
// domain's chrono types.

fn parse_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_default()
}

fn parse_dt_opt(s: Option<String>) -> Option<DateTime<Utc>> {
    s.and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&Utc))
}

#[derive(FromRow)]
struct WorkItemRow {
    provider: String,
    team: String,
    id: i64,
    title: String,
    work_item_type: String,
    state: String,
    tags: String,
    assigned_to: Option<String>,
    created_at: String,
    changed_at: String,
    closed_at: Option<String>,
    iteration_path: Option<String>,
    story_points: Option<f64>,
    url: String,
    linked_pr_ids: String,
    description: Option<String>,
    parent_id: Option<i64>,
    linked_repos: Option<String>,
}

impl WorkItemRow {
    fn into_domain(self) -> WorkItem {
        WorkItem {
            id: self.id,
            provider: self.provider,
            team: self.team,
            title: self.title,
            work_item_type: self.work_item_type,
            state: self.state,
            tags: serde_json::from_str(&self.tags).unwrap_or_default(),
            assigned_to: self.assigned_to,
            created_at: parse_dt(&self.created_at),
            changed_at: parse_dt(&self.changed_at),
            closed_at: parse_dt_opt(self.closed_at),
            iteration_path: self.iteration_path,
            story_points: self.story_points,
            url: self.url,
            linked_pr_ids: serde_json::from_str(&self.linked_pr_ids).unwrap_or_default(),
            parent_id: self.parent_id,
            linked_repos: self
                .linked_repos
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default(),
            linked_prs: Vec::new(),
            tag_suggestions: Vec::new(),
            description: self.description,
        }
    }
}

#[derive(FromRow)]
struct PipelineRow {
    provider: String,
    team: String,
    id: i64,
    name: String,
    folder: Option<String>,
    url: String,
    last_run_status: Option<String>,
    last_run_at: Option<String>,
    last_run_url: Option<String>,
}

impl PipelineRow {
    fn into_domain(self) -> Pipeline {
        Pipeline {
            id: self.id,
            provider: self.provider,
            team: self.team,
            name: self.name,
            folder: self.folder,
            url: self.url,
            last_run_status: self.last_run_status.map(|s| status_from_str(&s)),
            last_run_at: parse_dt_opt(self.last_run_at),
            last_run_url: self.last_run_url,
        }
    }
}

#[derive(FromRow)]
struct PullRequestRow {
    provider: String,
    team: String,
    id: i64,
    title: String,
    status: String,
    is_draft: i64,
    repository: Option<String>,
    author: Option<String>,
    created_at: Option<String>,
    source_branch: Option<String>,
    target_branch: Option<String>,
    reviewer_count: i64,
    url: String,
}

impl PullRequestRow {
    fn into_domain(self) -> PullRequest {
        PullRequest {
            id: self.id,
            provider: self.provider,
            team: self.team,
            title: self.title,
            status: PrStatus::from_slug(&self.status),
            is_draft: self.is_draft != 0,
            repository: self.repository,
            author: self.author,
            created_at: parse_dt_opt(self.created_at),
            source_branch: self.source_branch,
            target_branch: self.target_branch,
            reviewer_count: self.reviewer_count,
            url: self.url,
            flags: Vec::new(),
            linked_work_items: Vec::new(),
        }
    }
}

#[derive(FromRow)]
struct RunRow {
    provider: String,
    team: String,
    id: i64,
    pipeline_id: i64,
    status: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    source_branch: Option<String>,
    url: String,
}

impl RunRow {
    fn into_domain(self) -> PipelineRun {
        PipelineRun {
            id: self.id,
            pipeline_id: self.pipeline_id,
            provider: self.provider,
            team: self.team,
            status: status_from_str(&self.status),
            started_at: parse_dt_opt(self.started_at),
            finished_at: parse_dt_opt(self.finished_at),
            source_branch: self.source_branch,
            url: self.url,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use poseidon_core::DEFAULT_OWNER;

    #[tokio::test]
    async fn ai_drafts_and_activity_round_trip() {
        let store = Store::connect_in_memory().await.unwrap();

        // Drafts: set (grouped read), overwrite replaces, clear removes.
        store
            .set_ai_field_drafts(
                DEFAULT_OWNER,
                "Platform",
                42,
                &[
                    AiFieldDraft {
                        reference: "System.Description".into(),
                        value: "improved".into(),
                    },
                    AiFieldDraft {
                        reference: "AcceptanceCriteria".into(),
                        value: "GWT".into(),
                    },
                ],
            )
            .await
            .unwrap();
        let map = store.ai_field_drafts(DEFAULT_OWNER, None).await.unwrap();
        assert_eq!(map.get(&42).unwrap().len(), 2);
        store
            .set_ai_field_drafts(
                DEFAULT_OWNER,
                "Platform",
                42,
                &[AiFieldDraft {
                    reference: "System.Description".into(),
                    value: "v2".into(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .ai_field_drafts(DEFAULT_OWNER, None)
                .await
                .unwrap()
                .get(&42)
                .unwrap()
                .len(),
            1,
            "overwrite replaced, did not append"
        );
        store
            .clear_ai_field_drafts(DEFAULT_OWNER, 42)
            .await
            .unwrap();
        assert!(store
            .ai_field_drafts(DEFAULT_OWNER, None)
            .await
            .unwrap()
            .get(&42)
            .is_none());

        // Activity: same id upserts in place (not appends); items JSON round-trips.
        let mut rec = AiActivityRecord {
            id: "job-1".into(),
            team: "Platform".into(),
            name: "Suggest tags".into(),
            where_at: "gpu".into(),
            status: "running".into(),
            done: 1,
            total: 3,
            outcome: String::new(),
            items: serde_json::json!([{"id": 1, "tags": ["area:x"]}]),
            started_at: String::new(),
            updated_at: String::new(),
        };
        store.upsert_ai_activity(DEFAULT_OWNER, &rec).await.unwrap();
        rec.status = "done".into();
        rec.done = 3;
        rec.outcome = "tagged 3/3".into();
        store.upsert_ai_activity(DEFAULT_OWNER, &rec).await.unwrap();
        let list = store.list_ai_activity(DEFAULT_OWNER, 50).await.unwrap();
        assert_eq!(list.len(), 1, "same id upserts, not appends");
        assert_eq!(list[0].status, "done");
        assert_eq!(list[0].done, 3);
        assert_eq!(list[0].items[0]["id"], 1);
    }

    fn wi(id: i64, tags: &[&str], created: &str, changed: &str, closed: Option<&str>) -> WorkItem {
        WorkItem {
            id,
            provider: "azure-devops".into(),
            team: "Platform".into(),
            title: format!("Item {id}"),
            work_item_type: "User Story".into(),
            state: if closed.is_some() {
                "Closed".into()
            } else {
                "Active".into()
            },
            tags: tags.iter().map(|s| s.to_string()).collect(),
            assigned_to: None,
            created_at: parse_dt(created),
            changed_at: parse_dt(changed),
            closed_at: closed.map(parse_dt),
            iteration_path: None,
            story_points: None,
            url: format!("https://example/{id}"),
            description: None,
            linked_pr_ids: Vec::new(),
            parent_id: None,
            linked_repos: Vec::new(),
            linked_prs: Vec::new(),
            tag_suggestions: Vec::new(),
        }
    }

    #[tokio::test]
    async fn upsert_then_list_round_trips() {
        let store = Store::connect_in_memory().await.unwrap();
        let items = vec![
            wi(
                1,
                &["team:platform"],
                "2026-07-01T00:00:00Z",
                "2026-07-10T00:00:00Z",
                None,
            ),
            wi(2, &[], "2026-07-02T00:00:00Z", "2026-07-11T00:00:00Z", None),
        ];
        store
            .upsert_work_items(DEFAULT_OWNER, &items)
            .await
            .unwrap();
        let back = store.list_work_items(DEFAULT_OWNER, None).await.unwrap();
        assert_eq!(back.len(), 2);
        // Newest changed first.
        assert_eq!(back[0].id, 2);
        assert_eq!(back[1].tags, vec!["team:platform"]);
    }

    #[tokio::test]
    async fn catalog_replace_round_trips_skips_repoless_and_is_owner_isolated() {
        let store = Store::connect_in_memory().await.unwrap();
        let ent = |repo: Option<&str>, product: Option<&str>, team: Option<&str>| CatalogEntity {
            repo: repo.map(str::to_string),
            product: product.map(str::to_string),
            team: team.map(str::to_string),
            domain: None,
            kind: None,
        };
        let entities = vec![
            ent(
                Some("Contoso.Assistant"),
                Some("widget-assistant--wa-"),
                Some("team-alpha"),
            ),
            ent(Some("Ledger"), None, Some("team-beta")),
            ent(None, Some("product-with-no-repo"), None), // skipped: no repo
        ];
        let n = store
            .replace_catalog(DEFAULT_OWNER, &entities)
            .await
            .unwrap();
        assert_eq!(
            n, 2,
            "two repo-bearing rows stored; the repo-less one skipped"
        );

        let back = store.catalog(DEFAULT_OWNER).await.unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].repo.as_deref(), Some("Contoso.Assistant")); // sorted by repo
        assert_eq!(back[0].product.as_deref(), Some("widget-assistant--wa-"));
        assert_eq!(back[1].repo.as_deref(), Some("Ledger"));
        assert_eq!(back[1].product, None);

        // A re-sync REPLACES the snapshot wholesale (not merge): fewer rows, updated.
        let n2 = store
            .replace_catalog(
                DEFAULT_OWNER,
                &[ent(Some("Contoso.Assistant"), Some("new-product"), None)],
            )
            .await
            .unwrap();
        assert_eq!(n2, 1);
        let back2 = store.catalog(DEFAULT_OWNER).await.unwrap();
        assert_eq!(back2.len(), 1);
        assert_eq!(back2[0].product.as_deref(), Some("new-product"));

        // Owner isolation: another owner sees nothing.
        assert_eq!(store.catalog("someone-else").await.unwrap().len(), 0);
        assert_eq!(store.catalog_count("someone-else").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn user_config_is_owner_isolated() {
        let store = Store::connect_in_memory().await.unwrap();
        let a = poseidon_core::UserConfig {
            poll_all_teams: true,
            ..Default::default()
        };
        store.put_user_config("alice@x.com", &a).await.unwrap();
        // A different owner is independent and starts empty.
        assert!(store.get_user_config("bob@x.com").await.unwrap().is_none());
        assert!(
            store
                .get_user_config("alice@x.com")
                .await
                .unwrap()
                .unwrap()
                .poll_all_teams
        );

        store
            .put_user_config("bob@x.com", &poseidon_core::UserConfig::default())
            .await
            .unwrap();
        let owners = store.list_config_owners().await.unwrap();
        assert_eq!(
            owners,
            vec!["alice@x.com".to_string(), "bob@x.com".to_string()]
        );
    }

    #[tokio::test]
    async fn replace_team_work_items_prunes_out_of_scope() {
        let store = Store::connect_in_memory().await.unwrap();
        // Seed two items (team "Platform").
        store
            .upsert_work_items(
                DEFAULT_OWNER,
                &[
                    wi(1, &[], "2026-07-01T00:00:00Z", "2026-07-10T00:00:00Z", None),
                    wi(2, &[], "2026-07-02T00:00:00Z", "2026-07-11T00:00:00Z", None),
                ],
            )
            .await
            .unwrap();
        // A poll now sees only item 1 (item 2 fell out of scope). Replace prunes it.
        store
            .replace_team_work_items(
                DEFAULT_OWNER,
                "Platform",
                &[wi(
                    1,
                    &[],
                    "2026-07-01T00:00:00Z",
                    "2026-07-12T00:00:00Z",
                    None,
                )],
            )
            .await
            .unwrap();
        let back = store.list_work_items(DEFAULT_OWNER, None).await.unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, 1);

        // A different team's items are untouched by another team's replace.
        let mut other = wi(3, &[], "2026-07-03T00:00:00Z", "2026-07-13T00:00:00Z", None);
        other.team = "DevOps".into();
        store
            .upsert_work_items(DEFAULT_OWNER, &[other])
            .await
            .unwrap();
        store
            .replace_team_work_items(DEFAULT_OWNER, "Platform", &[])
            .await
            .unwrap();
        let back = store.list_work_items(DEFAULT_OWNER, None).await.unwrap();
        assert_eq!(back.len(), 1); // DevOps item 3 survived; Platform emptied
        assert_eq!(back[0].id, 3);
    }

    #[tokio::test]
    async fn replace_team_pipelines_prunes_out_of_scope_but_spares_other_teams() {
        let store = Store::connect_in_memory().await.unwrap();
        let pipe = |id: i64, team: &str| Pipeline {
            id,
            provider: "azure-devops".into(),
            team: team.into(),
            name: format!("pipe-{id}"),
            folder: None,
            url: format!("https://example/pipeline/{id}"),
            last_run_status: None,
            last_run_at: None,
            last_run_url: None,
        };
        store
            .upsert_pipelines(DEFAULT_OWNER, &[pipe(1, "Platform"), pipe(2, "Platform")])
            .await
            .unwrap();
        store
            .upsert_pipelines(DEFAULT_OWNER, &[pipe(3, "DevOps")])
            .await
            .unwrap();
        // A poll now sees only pipeline 1 for Platform; 2 fell out of scope.
        store
            .replace_team_pipelines(DEFAULT_OWNER, "Platform", &[pipe(1, "Platform")])
            .await
            .unwrap();
        let ids: Vec<i64> = store
            .list_pipelines(DEFAULT_OWNER, None)
            .await
            .unwrap()
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&1)); // kept in scope
        assert!(ids.contains(&3)); // other team untouched
        assert!(!ids.contains(&2)); // pruned
    }

    #[tokio::test]
    async fn replace_team_pull_requests_prunes_out_of_scope_but_spares_other_teams() {
        let store = Store::connect_in_memory().await.unwrap();
        let pr = |id: i64, team: &str| PullRequest {
            id,
            provider: "azure-devops".into(),
            team: team.into(),
            title: format!("PR {id}"),
            status: PrStatus::Active,
            is_draft: false,
            repository: Some("repo".into()),
            author: Some("a".into()),
            created_at: None,
            source_branch: None,
            target_branch: None,
            reviewer_count: 0,
            url: format!("https://example/pr/{id}"),
            flags: Vec::new(),
            linked_work_items: Vec::new(),
        };
        store
            .upsert_pull_requests(DEFAULT_OWNER, &[pr(1, "Platform"), pr(2, "Platform")])
            .await
            .unwrap();
        store
            .upsert_pull_requests(DEFAULT_OWNER, &[pr(3, "DevOps")])
            .await
            .unwrap();
        store
            .replace_team_pull_requests(DEFAULT_OWNER, "Platform", &[pr(1, "Platform")])
            .await
            .unwrap();
        let ids: Vec<i64> = store
            .list_pull_requests(DEFAULT_OWNER, None)
            .await
            .unwrap()
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&1)); // kept in scope
        assert!(ids.contains(&3)); // other team untouched
        assert!(!ids.contains(&2)); // pruned
    }

    #[tokio::test]
    async fn list_scopes_to_a_single_team_when_given() {
        let store = Store::connect_in_memory().await.unwrap();
        let mut a = wi(
            1,
            &["x"],
            "2026-07-01T00:00:00Z",
            "2026-07-10T00:00:00Z",
            None,
        );
        a.team = "Platform Engineering".into();
        let mut b = wi(
            2,
            &["y"],
            "2026-07-02T00:00:00Z",
            "2026-07-11T00:00:00Z",
            None,
        );
        b.team = "DevOps".into();
        store
            .upsert_work_items(DEFAULT_OWNER, &[a, b])
            .await
            .unwrap();

        // No filter → both teams.
        assert_eq!(
            store
                .list_work_items(DEFAULT_OWNER, None)
                .await
                .unwrap()
                .len(),
            2
        );
        // Scoped → just that team.
        let devops = store
            .list_work_items(DEFAULT_OWNER, Some("DevOps"))
            .await
            .unwrap();
        assert_eq!(devops.len(), 1);
        assert_eq!(devops[0].id, 2);
    }

    #[tokio::test]
    async fn upsert_is_idempotent_updates_in_place() {
        let store = Store::connect_in_memory().await.unwrap();
        store
            .upsert_work_items(
                DEFAULT_OWNER,
                &[wi(
                    1,
                    &["a"],
                    "2026-07-01T00:00:00Z",
                    "2026-07-01T00:00:00Z",
                    None,
                )],
            )
            .await
            .unwrap();
        // Re-poll the same id with a new state/tags.
        let mut updated = wi(
            1,
            &["b"],
            "2026-07-01T00:00:00Z",
            "2026-07-05T00:00:00Z",
            None,
        );
        updated.title = "Renamed".into();
        store
            .upsert_work_items(DEFAULT_OWNER, &[updated])
            .await
            .unwrap();
        let back = store.list_work_items(DEFAULT_OWNER, None).await.unwrap();
        assert_eq!(back.len(), 1, "same id must update, not duplicate");
        assert_eq!(back[0].title, "Renamed");
        assert_eq!(back[0].tags, vec!["b"]);
    }

    #[tokio::test]
    async fn ticket_report_counts_and_groups_by_tag() {
        let store = Store::connect_in_memory().await.unwrap();
        let items = vec![
            // Closed in range, two tags.
            wi(
                1,
                &["team:platform", "type:bug"],
                "2026-06-01T00:00:00Z",
                "2026-07-05T00:00:00Z",
                Some("2026-07-05T00:00:00Z"),
            ),
            // Closed in range, untagged.
            wi(
                2,
                &[],
                "2026-06-01T00:00:00Z",
                "2026-07-06T00:00:00Z",
                Some("2026-07-06T00:00:00Z"),
            ),
            // Closed OUTSIDE range - excluded.
            wi(
                3,
                &["team:platform"],
                "2026-06-01T00:00:00Z",
                "2026-05-01T00:00:00Z",
                Some("2026-05-01T00:00:00Z"),
            ),
            // Open, created in range.
            wi(
                4,
                &["type:feature"],
                "2026-07-02T00:00:00Z",
                "2026-07-02T00:00:00Z",
                None,
            ),
        ];
        store
            .upsert_work_items(DEFAULT_OWNER, &items)
            .await
            .unwrap();

        let report = store
            .ticket_report(
                DEFAULT_OWNER,
                "2026-07-01T00:00:00Z",
                "2026-07-31T23:59:59Z",
                None,
            )
            .await
            .unwrap();
        assert_eq!(report.closed, 2, "items 1 + 2 closed in range");
        assert_eq!(report.opened, 1, "only item 4 was created within July");
        // team:platform appears once (item 1), type:bug once, (untagged) once.
        let platform = report
            .closed_by_tag
            .iter()
            .find(|t| t.tag == "team:platform");
        assert_eq!(platform.map(|t| t.count), Some(1));
        assert!(report
            .closed_by_tag
            .iter()
            .any(|t| t.tag == "(untagged)" && t.count == 1));
    }

    #[tokio::test]
    async fn pipeline_report_computes_success_rate() {
        let store = Store::connect_in_memory().await.unwrap();
        let run = |id: i64, status: RunStatus, finished: &str| PipelineRun {
            id,
            pipeline_id: 10,
            provider: "azure-devops".into(),
            team: "Platform".into(),
            status,
            started_at: Some(parse_dt(finished)),
            finished_at: Some(parse_dt(finished)),
            source_branch: Some("refs/heads/main".into()),
            url: format!("https://example/run/{id}"),
        };
        let runs = vec![
            run(1, RunStatus::Succeeded, "2026-07-10T00:00:00Z"),
            run(2, RunStatus::Succeeded, "2026-07-11T00:00:00Z"),
            run(3, RunStatus::Failed, "2026-07-12T00:00:00Z"),
            run(4, RunStatus::Canceled, "2026-07-13T00:00:00Z"),
        ];
        store.upsert_runs(DEFAULT_OWNER, &runs).await.unwrap();
        let report = store
            .pipeline_report(
                DEFAULT_OWNER,
                "2026-07-01T00:00:00Z",
                "2026-07-31T23:59:59Z",
                None,
            )
            .await
            .unwrap();
        assert_eq!(report.succeeded, 2);
        assert_eq!(report.failed, 1);
        assert_eq!(report.canceled, 1);
        // 2 / (2 + 1) - canceled excluded from the rate.
        assert!((report.success_rate.unwrap() - 2.0 / 3.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn meta_round_trips() {
        let store = Store::connect_in_memory().await.unwrap();
        assert_eq!(
            store
                .get_meta(DEFAULT_OWNER, "last_polled_at")
                .await
                .unwrap(),
            None
        );
        let ts = Utc
            .with_ymd_and_hms(2026, 7, 30, 12, 0, 0)
            .unwrap()
            .to_rfc3339();
        store
            .set_meta(DEFAULT_OWNER, "last_polled_at", &ts)
            .await
            .unwrap();
        assert_eq!(
            store
                .get_meta(DEFAULT_OWNER, "last_polled_at")
                .await
                .unwrap(),
            Some(ts)
        );
    }

    #[tokio::test]
    async fn description_survives_round_trip_and_updates_in_place() {
        let store = Store::connect_in_memory().await.unwrap();
        let mut item = wi(
            1,
            &["x"],
            "2026-07-01T00:00:00Z",
            "2026-07-01T00:00:00Z",
            None,
        );
        item.description = Some("Original body <b>html</b>".into());
        store
            .upsert_work_items(DEFAULT_OWNER, &[item])
            .await
            .unwrap();
        let back = store.list_work_items(DEFAULT_OWNER, None).await.unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(
            back[0].description.as_deref(),
            Some("Original body <b>html</b>")
        );

        // Re-upsert the same id with a changed description -> updated in place.
        let mut updated = wi(
            1,
            &["x"],
            "2026-07-01T00:00:00Z",
            "2026-07-02T00:00:00Z",
            None,
        );
        updated.description = Some("Edited body".into());
        store
            .upsert_work_items(DEFAULT_OWNER, &[updated])
            .await
            .unwrap();
        let back = store.list_work_items(DEFAULT_OWNER, None).await.unwrap();
        assert_eq!(back.len(), 1, "same id updates, no duplicate");
        assert_eq!(back[0].description.as_deref(), Some("Edited body"));
    }

    #[tokio::test]
    async fn list_work_items_is_owner_scoped() {
        let store = Store::connect_in_memory().await.unwrap();
        // Same work-item id under two owners: the PK includes owner, so both coexist.
        store
            .upsert_work_items(
                "alice@x.com",
                &[wi(
                    1,
                    &["a"],
                    "2026-07-01T00:00:00Z",
                    "2026-07-01T00:00:00Z",
                    None,
                )],
            )
            .await
            .unwrap();
        store
            .upsert_work_items(
                "bob@x.com",
                &[wi(
                    1,
                    &["b"],
                    "2026-07-01T00:00:00Z",
                    "2026-07-01T00:00:00Z",
                    None,
                )],
            )
            .await
            .unwrap();
        let alice = store.list_work_items("alice@x.com", None).await.unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].tags, vec!["a"], "alice sees only her own row");
        let bob = store.list_work_items("bob@x.com", None).await.unwrap();
        assert_eq!(bob.len(), 1);
        assert_eq!(bob[0].tags, vec!["b"]);
    }

    #[tokio::test]
    async fn ai_suggestions_round_trip_overwrite_and_clear() {
        let store = Store::connect_in_memory().await.unwrap();
        store
            .set_ai_suggestions(
                DEFAULT_OWNER,
                "Platform",
                1,
                &[
                    ("area:platform".into(), "path match".into()),
                    ("type:bug".into(), "title says bug".into()),
                ],
            )
            .await
            .unwrap();
        let map = store.ai_suggestions(DEFAULT_OWNER, None).await.unwrap();
        let got = map.get(&1).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got
            .iter()
            .any(|(t, r)| t == "area:platform" && r == "path match"));

        // Overwrite replaces the whole set rather than appending.
        store
            .set_ai_suggestions(
                DEFAULT_OWNER,
                "Platform",
                1,
                &[("area:devops".into(), "reclassified".into())],
            )
            .await
            .unwrap();
        let map = store.ai_suggestions(DEFAULT_OWNER, None).await.unwrap();
        let got = map.get(&1).unwrap();
        assert_eq!(got.len(), 1, "overwrite replaced, did not append");
        assert_eq!(got[0].0, "area:devops");

        // An empty slice clears the item's suggestions entirely.
        store
            .set_ai_suggestions(DEFAULT_OWNER, "Platform", 1, &[])
            .await
            .unwrap();
        let map = store.ai_suggestions(DEFAULT_OWNER, None).await.unwrap();
        assert!(!map.contains_key(&1), "empty set clears the item");
    }

    #[tokio::test]
    async fn ai_audit_round_trip_overwrite_and_clear() {
        let store = Store::connect_in_memory().await.unwrap();
        store
            .set_ai_audit(
                DEFAULT_OWNER,
                "Platform",
                1,
                &[
                    ("unclear".into(), "Title says nothing".into()),
                    ("bad_data".into(), "Body contradicts title".into()),
                ],
            )
            .await
            .unwrap();
        let map = store.ai_audit(DEFAULT_OWNER, None).await.unwrap();
        let got = map.get(&1).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got
            .iter()
            .any(|(k, d)| k == "unclear" && d == "Title says nothing"));

        // Re-audit overwrites the whole set (no stale concerns left behind).
        store
            .set_ai_audit(
                DEFAULT_OWNER,
                "Platform",
                1,
                &[("bad_title".into(), "Placeholder title".into())],
            )
            .await
            .unwrap();
        let got = store.ai_audit(DEFAULT_OWNER, None).await.unwrap();
        let got = got.get(&1).unwrap();
        assert_eq!(got.len(), 1, "re-audit replaced, did not append");
        assert_eq!(got[0].0, "bad_title");

        // A clean re-audit (empty slice) clears the item entirely.
        store
            .set_ai_audit(DEFAULT_OWNER, "Platform", 1, &[])
            .await
            .unwrap();
        assert!(!store
            .ai_audit(DEFAULT_OWNER, None)
            .await
            .unwrap()
            .contains_key(&1));
    }

    #[tokio::test]
    async fn near_duplicates_replace_by_team_scope() {
        let store = Store::connect_in_memory().await.unwrap();
        store
            .replace_near_duplicates(
                DEFAULT_OWNER,
                Some("Platform"),
                &[
                    (1, "Platform".into(), "resembles #2 (85%)".into()),
                    (2, "Platform".into(), "resembles #1 (85%)".into()),
                ],
            )
            .await
            .unwrap();
        let map = store.near_duplicates(DEFAULT_OWNER, None).await.unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&1).unwrap(), "resembles #2 (85%)");

        // Re-scanning the Platform team replaces only Platform's rows.
        store
            .replace_near_duplicates(
                DEFAULT_OWNER,
                Some("Platform"),
                &[(1, "Platform".into(), "resembles #9 (90%)".into())],
            )
            .await
            .unwrap();
        let map = store.near_duplicates(DEFAULT_OWNER, None).await.unwrap();
        assert_eq!(map.len(), 1, "team re-scan replaced its rows");
        assert_eq!(map.get(&1).unwrap(), "resembles #9 (90%)");
    }

    #[tokio::test]
    async fn ai_suggestions_scope_by_owner_and_team() {
        let store = Store::connect_in_memory().await.unwrap();
        store
            .set_ai_suggestions("alice@x.com", "Platform", 1, &[("t".into(), "r".into())])
            .await
            .unwrap();
        store
            .set_ai_suggestions("bob@x.com", "Platform", 2, &[("t".into(), "r".into())])
            .await
            .unwrap();
        // Owner isolation: alice never sees bob's item 2.
        let alice = store.ai_suggestions("alice@x.com", None).await.unwrap();
        assert!(alice.contains_key(&1));
        assert!(!alice.contains_key(&2));

        // Read-side team scoping: a DevOps suggestion is excluded when scoped to Platform.
        store
            .set_ai_suggestions("alice@x.com", "DevOps", 3, &[("t".into(), "r".into())])
            .await
            .unwrap();
        let platform = store
            .ai_suggestions("alice@x.com", Some("Platform"))
            .await
            .unwrap();
        assert!(platform.contains_key(&1));
        assert!(
            !platform.contains_key(&3),
            "DevOps item excluded by team scope"
        );
    }

    #[tokio::test]
    async fn delete_meta_removes_only_that_key_for_that_owner() {
        let store = Store::connect_in_memory().await.unwrap();
        store.set_meta(DEFAULT_OWNER, "k1", "v1").await.unwrap();
        store.set_meta(DEFAULT_OWNER, "k2", "v2").await.unwrap();
        store.set_meta("other@x.com", "k1", "other").await.unwrap();

        store.delete_meta(DEFAULT_OWNER, "k1").await.unwrap();
        // k1 is gone for default...
        assert_eq!(store.get_meta(DEFAULT_OWNER, "k1").await.unwrap(), None);
        // ...but the same owner's other key, and another owner's k1, are untouched.
        assert_eq!(
            store.get_meta(DEFAULT_OWNER, "k2").await.unwrap(),
            Some("v2".to_string())
        );
        assert_eq!(
            store.get_meta("other@x.com", "k1").await.unwrap(),
            Some("other".to_string())
        );

        // Deleting an absent key is a no-op, not an error.
        store.delete_meta(DEFAULT_OWNER, "missing").await.unwrap();
    }

    #[tokio::test]
    async fn ticket_report_is_owner_scoped() {
        let store = Store::connect_in_memory().await.unwrap();
        // Alice: one item closed in range, tagged.
        store
            .upsert_work_items(
                "alice@x.com",
                &[wi(
                    1,
                    &["team:platform"],
                    "2026-06-01T00:00:00Z",
                    "2026-07-05T00:00:00Z",
                    Some("2026-07-05T00:00:00Z"),
                )],
            )
            .await
            .unwrap();
        // Bob: two items closed in the same range - must not leak into alice's totals.
        store
            .upsert_work_items(
                "bob@x.com",
                &[
                    wi(
                        2,
                        &[],
                        "2026-06-01T00:00:00Z",
                        "2026-07-06T00:00:00Z",
                        Some("2026-07-06T00:00:00Z"),
                    ),
                    wi(
                        3,
                        &[],
                        "2026-06-01T00:00:00Z",
                        "2026-07-07T00:00:00Z",
                        Some("2026-07-07T00:00:00Z"),
                    ),
                ],
            )
            .await
            .unwrap();
        let report = store
            .ticket_report(
                "alice@x.com",
                "2026-07-01T00:00:00Z",
                "2026-07-31T23:59:59Z",
                None,
            )
            .await
            .unwrap();
        assert_eq!(report.closed, 1, "only alice's closed item is counted");
        assert_eq!(
            report
                .closed_by_tag
                .iter()
                .find(|t| t.tag == "team:platform")
                .map(|t| t.count),
            Some(1)
        );
    }

    #[tokio::test]
    async fn pipeline_report_success_rate_is_none_without_completed_runs() {
        let store = Store::connect_in_memory().await.unwrap();
        let run = |id: i64, status: RunStatus, finished: &str| PipelineRun {
            id,
            pipeline_id: 10,
            provider: "azure-devops".into(),
            team: "Platform".into(),
            status,
            started_at: Some(parse_dt(finished)),
            finished_at: Some(parse_dt(finished)),
            source_branch: None,
            url: format!("https://example/run/{id}"),
        };
        // Only a canceled run finished in range - nothing succeeded or failed.
        store
            .upsert_runs(
                DEFAULT_OWNER,
                &[run(1, RunStatus::Canceled, "2026-07-10T00:00:00Z")],
            )
            .await
            .unwrap();
        let report = store
            .pipeline_report(
                DEFAULT_OWNER,
                "2026-07-01T00:00:00Z",
                "2026-07-31T23:59:59Z",
                None,
            )
            .await
            .unwrap();
        assert_eq!(report.succeeded, 0);
        assert_eq!(report.failed, 0);
        assert_eq!(report.canceled, 1);
        assert_eq!(report.total_runs, 1);
        assert!(
            report.success_rate.is_none(),
            "no completed runs -> rate undefined, not 0"
        );
    }
}
