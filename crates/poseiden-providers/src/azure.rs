//! Azure DevOps provider.
//!
//! Talks to the Azure DevOps REST API (api-version 7.1) with a PAT. Three
//! fetches: work items (WIQL id query → batched detail fetch), pipelines
//! (build definitions), and runs (builds, windowed by time). The JSON→core
//! normalisation lives in free functions at the bottom so it's testable
//! against captured sample payloads with no network.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use poseiden_core::{
    EditableField, FieldChange, FieldKind, Pipeline, PipelineRun, PrStatus, PullRequest, RunStatus,
    TeamConfig, WorkItem, WorkItemUpdate,
};
use serde::{Deserialize, Serialize};

use crate::{Credential, Provider, ProviderError};

const API_VERSION: &str = "7.1";
const PROVIDER: &str = "azure-devops";

/// Azure DevOps' well-known Entra resource / application id. An OAuth access
/// token must target this audience to be accepted by the Azure DevOps REST API.
pub const AZURE_DEVOPS_RESOURCE: &str = "499b84ac-1321-427f-aa17-267ca6975798";

/// A pending device-code prompt: the user opens `url` and enters `code`.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceCode {
    pub url: String,
    pub code: String,
}

/// Azure DevOps caps the work-item batch endpoint at 200 ids per call.
const BATCH_LIMIT: usize = 200;

/// Page size when walking active PRs. The `git/pullrequests` endpoint has no
/// continuation token, so we page with `$skip` until a short page ends the walk.
const PR_PAGE: u32 = 100;

/// Cap on how many recently-closed PRs (completed / abandoned) we pull. These
/// are never shown on the PR screen - they exist only to colour a work item's
/// linked-PR chips (merged = green, abandoned = red). Older links beyond this
/// window fall back to "unknown" rather than driving an unbounded history pull.
const PR_CLOSED_CAP: u32 = 200;

/// Characters to percent-encode in a URL path segment. Azure DevOps project
/// names routinely contain spaces (`"Platform Engineering"`), which must be
/// encoded to `%20` or the request URL is malformed. Alphanumerics and the
/// unreserved `-._~` pass through.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// Percent-encode one URL path segment (e.g. an Azure DevOps project name).
fn enc(segment: &str) -> String {
    utf8_percent_encode(segment, PATH_SEGMENT).to_string()
}

// ── Field editing: Azure DevOps type introspection + html<->markdown ──

/// Azure DevOps rich-text fields are HTML; the editor works in markdown. Convert
/// on the way out so the modal only ever sees markdown.
fn html_to_md(html: &str) -> String {
    html2md::parse_html(html)
}

/// …and back on save. Markdown -> HTML for the write-back.
fn md_to_html(md: &str) -> String {
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, pulldown_cmark::Parser::new(md));
    out
}

/// Map an Azure DevOps field data `type` onto a UI [`FieldKind`]. `has_options`
/// promotes a string/integer with an allowed-value list to a `Select`. Returns
/// `None` for types with nothing meaningful to edit (guid, history, …).
fn map_field_kind(ftype: &str, has_options: bool) -> Option<FieldKind> {
    Some(match ftype {
        "html" => FieldKind::Markdown,
        "plainText" => FieldKind::PlainText,
        "string" | "picklistString" => {
            if has_options {
                FieldKind::Select
            } else {
                FieldKind::Text
            }
        }
        "integer" | "picklistInteger" => {
            if has_options {
                FieldKind::Select
            } else {
                FieldKind::Integer
            }
        }
        "double" | "picklistDouble" => FieldKind::Float,
        "dateTime" => FieldKind::DateTime,
        "boolean" => FieldKind::Boolean,
        "identity" => FieldKind::Identity,
        "treePath" => FieldKind::Text, // Area / Iteration path - edited as text in v1
        _ => return None,
    })
}

/// Render an Azure DevOps field value (from the item's field bag) to the string the
/// editor seeds its control with. HTML fields come back as markdown; an identity is
/// its display name; everything else is stringified.
fn field_value_to_string(raw: Option<&serde_json::Value>, kind: FieldKind) -> String {
    let v = match raw {
        Some(v) => v,
        None => return String::new(),
    };
    match kind {
        FieldKind::Markdown => html_to_md(v.as_str().unwrap_or("")),
        FieldKind::Identity => v
            .get("displayName")
            .and_then(|d| d.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| v.as_str().unwrap_or("").to_string()),
        FieldKind::Boolean => v.as_bool().unwrap_or(false).to_string(),
        _ => match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => String::new(),
        },
    }
}

/// A JSON scalar (string or number) as a string; `None` for objects/arrays/null.
/// Used to render a field's allowed-value list into `Select` options.
fn json_scalar_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Fields that are noise in an editor: internal bookkeeping (ids, revs, counts,
/// area/iteration tree ids), Kanban board columns (`WEF_…`, `*Board*`/`*Kanban*`),
/// and fields POSEIDEN already edits with a purpose-built control (State, Tags are
/// the inline row editors; Description etc. remain). Keeps the modal to fields a
/// person would actually write.
fn is_noise_field(reference: &str) -> bool {
    if reference.starts_with("WEF_") || reference.contains("Kanban") || reference.contains("Board")
    {
        return true;
    }
    const NOISE: &[&str] = &[
        "System.Id",
        "System.Rev",
        "System.Watermark",
        "System.AreaId",
        "System.NodeName",
        "System.IterationId",
        "System.CommentCount",
        "System.AuthorizedDate",
        "System.AuthorizedAs",
        "System.PersonId",
        "System.RelatedLinkCount",
        "System.ExternalLinkCount",
        "System.HyperLinkCount",
        "System.AttachedFileCount",
        "System.RemoteLinkCount",
        "System.IsDeleted",
        "System.TeamProject",
        "System.Tags", // edited via the row's tag-chip editor
        // System-maintained audit fields: shown on the item but never user-edited, and
        // often not flagged read-only by the process, so filter them explicitly.
        "System.CreatedBy",
        "System.CreatedDate",
        "System.ChangedBy",
        "System.ChangedDate",
        "Microsoft.VSTS.Common.ActivatedBy",
        "Microsoft.VSTS.Common.ActivatedDate",
        "Microsoft.VSTS.Common.ClosedBy",
        "Microsoft.VSTS.Common.ClosedDate",
        "Microsoft.VSTS.Common.ResolvedBy",
        "Microsoft.VSTS.Common.ResolvedDate",
        "Microsoft.VSTS.Common.StateChangeDate",
        "Microsoft.VSTS.Common.StackRank",
    ];
    NOISE.contains(&reference)
        || reference.starts_with("System.AreaLevel")
        || reference.starts_with("System.IterationLevel")
}

/// The set of field reference names that appear as CONTROLS on a work-item type's
/// form, parsed from the type's `xmlForm` (returned by `wit/workitemtypes/{type}`).
/// This is the only signal that distinguishes a field the process actually SHOWS
/// from one merely associated with the type: a Bug carries `System.Description` as a
/// type field but hides it on the form (Repro Steps takes its place), so it must not
/// be offered as an editable field. Refs are lowercased (ADO treats them case-
/// insensitively). An empty result (no form / unparseable) means "unknown" - callers
/// fall back to not filtering rather than hiding everything.
fn form_field_refs(xml_form: &str) -> std::collections::HashSet<String> {
    // The form XML is `<Control FieldName="System.Description" .../>` (attribute order
    // and quoting vary). Scan for each `FieldName=` attribute rather than parsing XML,
    // so a formatting quirk can never make us drop the whole form. Case-insensitive on
    // the attribute name; the value is taken verbatim up to the closing quote.
    let mut refs = std::collections::HashSet::new();
    let bytes = xml_form.as_bytes();
    let lower = xml_form.to_ascii_lowercase();
    let needle = "fieldname";
    let skip_ws = |mut i: usize| {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(needle) {
        let after = from + rel + needle.len();
        // Require `=` (optionally whitespace-separated) then a quote, so the value is an
        // attribute value - tolerating `FieldName="x"`, `FieldName = 'x'`, etc.
        let mut i = skip_ws(after);
        if i < bytes.len() && bytes[i] == b'=' {
            i = skip_ws(i + 1);
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                let start = i + 1;
                if let Some(endrel) = xml_form[start..].find(quote as char) {
                    let val = xml_form[start..start + endrel].trim();
                    if !val.is_empty() {
                        refs.insert(val.to_ascii_lowercase());
                    }
                    from = start + endrel + 1;
                    continue;
                }
            }
        }
        from = after;
    }
    refs
}

/// Build the default WIQL for a team: non-removed items in the project, most-
/// recently-changed first, narrowed to an area path when the team defines one.
/// This is what lets two teams share one Azure DevOps project - each scoped to
/// a different `[System.AreaPath] UNDER '...'`.
/// Whether an error is Azure DevOps' "result set exceeds 20000" WIQL cap (400
/// `VS402337`) - the signal to chunk the query by id range.
fn is_result_limit(e: &ProviderError) -> bool {
    matches!(e, ProviderError::Api { status: 400, body, .. }
        if body.contains("VS402337") || body.contains("size limit"))
}

/// Largest id literal Azure DevOps WIQL accepts. Its id comparisons are 32-bit;
/// a larger number returns 400 TF51601 ("unsupported number value"). Work-item ids
/// never exceed this, so it is a safe upper bound for id-range chunking.
const MAX_WIQL_ID: i64 = i32::MAX as i64;

fn build_wiql(area_path: Option<&str>, strict: bool, id_range: Option<(i64, i64)>) -> String {
    let mut q = String::from(
        "SELECT [System.Id] FROM WorkItems \
         WHERE [System.TeamProject] = @project AND [System.State] <> 'Removed'",
    );
    if let Some(area) = area_path.map(str::trim).filter(|s| !s.is_empty()) {
        // Escape single quotes so an area path can't break out of the string.
        let escaped = area.replace('\'', "''");
        // Strict = the exact area only; otherwise UNDER includes child areas.
        let op = if strict { "=" } else { "UNDER" };
        q.push_str(&format!(" AND [System.AreaPath] {op} '{escaped}'"));
    }
    // A half-open [lo, hi) id window used when chunking a query that would
    // otherwise exceed Azure DevOps' 20000-result WIQL cap. Ordered by id so a
    // chunk is stable; the unchunked query keeps recency order.
    if let Some((lo, hi)) = id_range {
        q.push_str(&format!(
            " AND [System.Id] >= {lo} AND [System.Id] < {hi} ORDER BY [System.Id] ASC"
        ));
    } else {
        q.push_str(" ORDER BY [System.ChangedDate] DESC");
    }
    q
}

pub struct AzureDevOpsProvider {
    client: reqwest::Client,
    /// The team's display name (stamped onto every entity as `team`).
    team_name: String,
    /// Organisation base URL, trailing slash trimmed (`https://dev.azure.com/acme`).
    organization: String,
    /// Azure DevOps project name (path segment in API URLs; encoded on use).
    ado_project: String,
    wiql: String,
    /// Area path scope (if any), kept so we can rebuild id-ranged WIQL when
    /// chunking a query that would exceed the 20000-result cap.
    area_path: Option<String>,
    /// Restrict `area_path` to the exact path (no child areas).
    area_path_strict: bool,
    /// True when `wiql` is a caller-supplied override (so we must NOT rewrite it
    /// for chunking - we don't know its shape).
    wiql_is_custom: bool,
    /// Configured pipeline subset; empty = all pipelines in the project.
    pipeline_ids: Vec<i64>,
}

impl AzureDevOpsProvider {
    pub fn new(cfg: &TeamConfig, credential: Credential) -> Result<Self, ProviderError> {
        // The credential (PAT → Basic, or OAuth → Bearer) becomes the default
        // Authorization header. The provider is rebuilt each poll, so a
        // short-lived bearer token is always freshly resolved.
        let mut headers = reqwest::header::HeaderMap::new();
        let mut auth = reqwest::header::HeaderValue::from_str(&credential.header_value())
            .map_err(|e| ProviderError::Client(e.to_string()))?;
        auth.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, auth);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent("poseiden")
            .build()?;

        Ok(Self {
            client,
            team_name: cfg.name.clone(),
            organization: cfg.organization.trim_end_matches('/').to_string(),
            ado_project: cfg.project.clone(),
            // An explicit `wiql` override takes full control (area_path ignored,
            // as documented); otherwise build the default scoped by area_path.
            wiql: cfg.wiql.clone().unwrap_or_else(|| {
                build_wiql(cfg.area_path.as_deref(), cfg.area_path_strict, None)
            }),
            area_path: cfg.area_path.clone(),
            area_path_strict: cfg.area_path_strict,
            wiql_is_custom: cfg.wiql.is_some(),
            pipeline_ids: cfg.pipeline_ids.clone(),
        })
    }

    /// `{org}/{project}/_apis/{tail}` with the api-version appended. The project
    /// segment is percent-encoded so names with spaces work.
    fn project_url(&self, tail: &str, extra_query: &str) -> String {
        let sep = if extra_query.is_empty() { "" } else { "&" };
        format!(
            "{}/{}/_apis/{}?api-version={}{}{}",
            self.organization,
            enc(&self.ado_project),
            tail,
            API_VERSION,
            sep,
            extra_query
        )
    }

    /// `{org}/_apis/{tail}` (org-scoped, e.g. the work-item batch endpoint).
    fn org_url(&self, tail: &str) -> String {
        format!(
            "{}/_apis/{}?api-version={}",
            self.organization, tail, API_VERSION
        )
    }

    /// GET + JSON-decode with a uniform error surface (non-2xx → `Api`).
    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T, ProviderError> {
        let resp = self.client.get(url).send().await?;
        self.decode(resp, url).await
    }

    async fn decode<T: for<'de> Deserialize<'de>>(
        &self,
        resp: reqwest::Response,
        url: &str,
    ) -> Result<T, ProviderError> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Api {
                status: status.as_u16(),
                url: url.to_string(),
                body: body.chars().take(400).collect(),
            });
        }
        resp.json::<T>().await.map_err(ProviderError::Http)
    }

    /// All matching work-item ids. Azure DevOps caps a WIQL result at 20000 and
    /// returns a 400 (`VS402337`) when a query would exceed it. We run the normal
    /// query first (one request for the common case) and only if it trips the cap
    /// fall back to chunking by id range - which needs the default query shape, so
    /// a caller-supplied `wiql` override can't be chunked.
    async fn query_ids(&self) -> Result<Vec<i64>, ProviderError> {
        match self.wiql_ids(&self.wiql).await {
            Ok(ids) => Ok(ids),
            Err(e) if is_result_limit(&e) && !self.wiql_is_custom => {
                tracing::warn!(team = %self.team_name, "WIQL exceeds 20000-row cap; chunking by id range");
                self.query_ids_chunked().await
            }
            Err(e) => Err(e),
        }
    }

    /// POST a WIQL string and return the matching ids (or the raw error, so the
    /// caller can detect the 20000-row cap).
    async fn wiql_ids(&self, query: &str) -> Result<Vec<i64>, ProviderError> {
        let url = self.project_url("wit/wiql", "");
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "query": query }))
            .send()
            .await?;
        let parsed: WiqlResponse = self.decode(resp, &url).await?;
        Ok(parsed.work_items.into_iter().map(|w| w.id).collect())
    }

    /// Chunked id fetch: find a ceiling above the highest id, then recursively
    /// split the `[1, ceiling)` id range, subdividing any slice that still trips
    /// the cap. Merged, the slices cover every id with no overlap or gaps.
    async fn query_ids_chunked(&self) -> Result<Vec<i64>, ProviderError> {
        // Grow a ceiling until nothing lives above it (an over-cap slice above the
        // ceiling counts as "still items up there", so we keep raising it). Every
        // bound is capped at MAX_WIQL_ID: Azure DevOps rejects id literals beyond
        // 32-bit range with TF51601 ("unsupported number value"), and no work-item
        // id can exceed it anyway.
        let mut hi: i64 = 4_000_000;
        loop {
            let empty_above = self
                .wiql_ids(&build_wiql(
                    self.area_path.as_deref(),
                    self.area_path_strict,
                    Some((hi, MAX_WIQL_ID)),
                ))
                .await
                .map(|ids| ids.is_empty())
                .unwrap_or(false);
            if empty_above || hi >= MAX_WIQL_ID {
                break;
            }
            hi = hi.saturating_mul(2).min(MAX_WIQL_ID);
        }
        self.query_id_range(1, hi.min(MAX_WIQL_ID)).await
    }

    /// Ids in `[lo, hi)`, splitting the range when it exceeds the WIQL cap.
    fn query_id_range<'a>(
        &'a self,
        lo: i64,
        hi: i64,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<i64>, ProviderError>> + Send + 'a>,
    > {
        Box::pin(async move {
            match self
                .wiql_ids(&build_wiql(
                    self.area_path.as_deref(),
                    self.area_path_strict,
                    Some((lo, hi)),
                ))
                .await
            {
                Ok(ids) => Ok(ids),
                Err(e) if is_result_limit(&e) && hi - lo > 1 => {
                    let mid = lo + (hi - lo) / 2;
                    let mut left = self.query_id_range(lo, mid).await?;
                    let right = self.query_id_range(mid, hi).await?;
                    left.extend(right);
                    Ok(left)
                }
                Err(e) => Err(e),
            }
        })
    }

    /// One page of PRs at a given status (`active` / `completed` / `abandoned`),
    /// normalised. `skip` walks pages; `top` bounds the page.
    async fn fetch_pr_page(
        &self,
        status: &str,
        top: u32,
        skip: u32,
    ) -> Result<Vec<PullRequest>, ProviderError> {
        let query = format!("searchCriteria.status={status}&$top={top}&$skip={skip}");
        let url = self.project_url("git/pullrequests", &query);
        let parsed: PullRequestsResponse = self.get_json(&url).await?;
        Ok(parsed
            .value
            .into_iter()
            .map(|pr| {
                normalise_pull_request(pr, &self.team_name, &self.organization, &self.ado_project)
            })
            .collect())
    }

    /// PATCH a work item with a JSON-Patch op list and return its normalised,
    /// post-write view. `$expand=relations` on the response is load-bearing: it
    /// keeps `linked_pr_ids` populated so an edit (state/tag OR link) never
    /// round-trips an item with its relations dropped.
    async fn patch_work_item(
        &self,
        id: i64,
        ops: Vec<serde_json::Value>,
    ) -> Result<WorkItem, ProviderError> {
        // Content-Type MUST be application/json-patch+json, so build the body
        // ourselves rather than `.json()` (which would force application/json).
        let url = self.project_url(&format!("wit/workitems/{id}"), "$expand=relations");
        let body = serde_json::to_vec(&serde_json::Value::Array(ops)).unwrap_or_default();
        let resp = self
            .client
            .patch(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json-patch+json")
            .body(body)
            .send()
            .await?;
        let raw: AdoWorkItem = self.decode(resp, &url).await?;
        Ok(normalise_work_item(
            raw,
            &self.team_name,
            &self.organization,
            &self.ado_project,
        ))
    }

    /// GET a single work item with its relations (for computing a relation's
    /// array index before a remove).
    async fn get_work_item_raw(&self, id: i64) -> Result<AdoWorkItem, ProviderError> {
        let url = self.project_url(&format!("wit/workitems/{id}"), "$expand=relations");
        self.get_json(&url).await
    }

    /// Resolve a PR's artifact-link uri (`vstfs:///Git/PullRequestId/{project}%2F
    /// {repo}%2F{pr}`) from its repo + project GUIDs, fetched org-wide by id.
    async fn pr_artifact_uri(&self, pr_id: i64) -> Result<String, ProviderError> {
        let url = self.org_url(&format!("git/pullrequests/{pr_id}"));
        let pr: AdoPullRequest = self.get_json(&url).await?;
        let repo = pr
            .repository
            .ok_or_else(|| ProviderError::Config(format!("PR {pr_id} has no repository")))?;
        let repo_id = repo
            .id
            .ok_or_else(|| ProviderError::Config(format!("PR {pr_id} repository has no id")))?;
        let project_id = repo
            .project
            .and_then(|p| p.id)
            .ok_or_else(|| ProviderError::Config(format!("PR {pr_id} has no project id")))?;
        Ok(format!(
            "vstfs:///Git/PullRequestId/{project_id}%2F{repo_id}%2F{pr_id}"
        ))
    }

    /// Batch-fetch full details for a chunk of ids (≤ [`BATCH_LIMIT`]).
    async fn fetch_batch(&self, ids: &[i64]) -> Result<Vec<WorkItem>, ProviderError> {
        let url = self.org_url("wit/workitemsbatch");
        // `$expand=relations` (mutually exclusive with `fields`) returns every
        // field plus relations, from which we extract linked pull requests.
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "ids": ids, "$expand": "relations" }))
            .send()
            .await?;
        let parsed: BatchResponse = self.decode(resp, &url).await?;
        Ok(parsed
            .value
            .into_iter()
            .map(|raw| {
                normalise_work_item(raw, &self.team_name, &self.organization, &self.ado_project)
            })
            .collect())
    }
}

#[async_trait]
impl Provider for AzureDevOpsProvider {
    fn provider_name(&self) -> &str {
        PROVIDER
    }

    fn team_name(&self) -> &str {
        &self.team_name
    }

    async fn fetch_work_items(&self) -> Result<Vec<WorkItem>, ProviderError> {
        let ids = self.query_ids().await?;
        let mut items = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(BATCH_LIMIT) {
            items.extend(self.fetch_batch(chunk).await?);
        }
        Ok(items)
    }

    async fn fetch_origin_area(&self, id: i64) -> Result<Option<String>, ProviderError> {
        // The FIRST (oldest) revision is where the item was created; `$top=1` returns
        // just it, so this is one small call per item (and it never changes, so the
        // caller caches it). `System.AreaPath` on that revision = the creating board.
        let url = format!(
            "{}&$top=1",
            self.org_url(&format!("wit/workItems/{id}/revisions"))
        );
        let resp: RevisionsResponse = self.get_json(&url).await?;
        Ok(resp
            .value
            .into_iter()
            .next()
            .and_then(|r| r.fields)
            .and_then(|f| f.area_path)
            .filter(|s| !s.trim().is_empty()))
    }

    async fn fetch_pipelines(&self) -> Result<Vec<Pipeline>, ProviderError> {
        // `includeLatestBuilds` returns each definition's most recent build
        // inline, so a pipeline's real last status survives the runs window.
        let url = self.project_url("build/definitions", "includeLatestBuilds=true");
        let parsed: DefinitionsResponse = self.get_json(&url).await?;
        let all = parsed
            .value
            .into_iter()
            .map(|d| normalise_pipeline(d, &self.team_name, &self.organization, &self.ado_project));
        // Honour the configured subset if one is set.
        if self.pipeline_ids.is_empty() {
            Ok(all.collect())
        } else {
            Ok(all.filter(|p| self.pipeline_ids.contains(&p.id)).collect())
        }
    }

    async fn fetch_runs(&self, since: DateTime<Utc>) -> Result<Vec<PipelineRun>, ProviderError> {
        // Builds API, windowed by minTime + capped. When a pipeline subset is
        // configured, filter server-side via &definitions=.
        let mut query = format!("$top=200&minTime={}", since.to_rfc3339());
        if !self.pipeline_ids.is_empty() {
            let ids: Vec<String> = self.pipeline_ids.iter().map(|i| i.to_string()).collect();
            query.push_str(&format!("&definitions={}", ids.join(",")));
        }
        let url = self.project_url("build/builds", &query);
        let parsed: BuildsResponse = self.get_json(&url).await?;
        Ok(parsed
            .value
            .into_iter()
            .map(|b| normalise_run(b, &self.team_name))
            .collect())
    }

    async fn fetch_pull_requests(&self) -> Result<Vec<PullRequest>, ProviderError> {
        let mut out = Vec::new();
        // Active PRs = the in-flight set the PR screen shows. Page fully (the
        // endpoint has no continuation token) so the count is real, not capped.
        let mut skip = 0;
        loop {
            let page = self.fetch_pr_page("active", PR_PAGE, skip).await?;
            let full = page.len() as u32 == PR_PAGE;
            out.extend(page);
            if !full {
                break;
            }
            skip += PR_PAGE;
        }
        // Recently completed + abandoned PRs, bounded. These are NOT listed on
        // the PR screen (the service filters them out); they exist only to give
        // a work item's linked-PR chips a real colour. Most-recent first.
        out.extend(self.fetch_pr_page("completed", PR_CLOSED_CAP, 0).await?);
        out.extend(self.fetch_pr_page("abandoned", PR_CLOSED_CAP, 0).await?);
        Ok(out)
    }

    async fn fetch_pull_request(&self, id: i64) -> Result<PullRequest, ProviderError> {
        // Org-wide by id (the linked PR may live in any repo/project we scope to).
        let url = self.org_url(&format!("git/pullrequests/{id}"));
        let raw: AdoPullRequest = self.get_json(&url).await?;
        Ok(normalise_pull_request(
            raw,
            &self.team_name,
            &self.organization,
            &self.ado_project,
        ))
    }

    async fn update_work_item(
        &self,
        id: i64,
        update: &WorkItemUpdate,
    ) -> Result<WorkItem, ProviderError> {
        if update.is_empty() {
            return Err(ProviderError::Config("no fields to update".into()));
        }
        // Azure DevOps updates are JSON-Patch documents against `/fields/...`.
        let mut ops: Vec<serde_json::Value> = Vec::new();
        if let Some(state) = &update.state {
            ops.push(serde_json::json!({
                "op": "add", "path": "/fields/System.State", "value": state
            }));
        }
        if let Some(tags) = &update.tags {
            // ADO stores tags as one "; "-separated string, but treats the field
            // as a COLLECTION: `op: "add"` MERGES (appends) and never removes,
            // so a tag left out of the list survives. `op: "replace"` sets the
            // exact set - the semantics we want (we always send the full list).
            ops.push(serde_json::json!({
                "op": "replace", "path": "/fields/System.Tags", "value": tags.join("; ")
            }));
        }
        self.patch_work_item(id, ops).await
    }

    async fn link_pr(&self, work_item_id: i64, pr_id: i64) -> Result<WorkItem, ProviderError> {
        // The relation's url is the PR's vstfs artifact uri, which needs the repo
        // + project GUIDs - so resolve them from the PR first, then add the link.
        let artifact = self.pr_artifact_uri(pr_id).await?;
        let ops = vec![serde_json::json!({
            "op": "add",
            "path": "/relations/-",
            "value": {
                "rel": "ArtifactLink",
                "url": artifact,
                "attributes": { "name": "Pull Request" }
            }
        })];
        self.patch_work_item(work_item_id, ops).await
    }

    async fn unlink_pr(&self, work_item_id: i64, pr_id: i64) -> Result<WorkItem, ProviderError> {
        // Relations are removed by array index, so read the item's current
        // relations, find the one pointing at this PR, and remove that slot.
        let raw = self.get_work_item_raw(work_item_id).await?;
        let idx = raw
            .relations
            .iter()
            .position(|r| pr_id_from_relation(r) == Some(pr_id))
            .ok_or_else(|| {
                ProviderError::Config(format!(
                    "work item {work_item_id} has no link to PR {pr_id}"
                ))
            })?;
        let ops = vec![serde_json::json!({
            "op": "remove", "path": format!("/relations/{idx}")
        })];
        self.patch_work_item(work_item_id, ops).await
    }

    async fn editable_fields(&self, id: i64) -> Result<Vec<EditableField>, ProviderError> {
        // 1. The item's current field values (full bag) + its work-item TYPE.
        let item: serde_json::Value = self
            .get_json(&self.project_url(&format!("wit/workitems/{id}"), ""))
            .await?;
        let empty = serde_json::Map::new();
        let values = item
            .get("fields")
            .and_then(|f| f.as_object())
            .unwrap_or(&empty);
        let wit = values
            .get("System.WorkItemType")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if wit.is_empty() {
            return Ok(Vec::new());
        }
        // 2. The TYPE's fields (which fields are on this type + required/help/options).
        let type_fields: AdoTypeFieldsResponse = self
            .get_json(&self.project_url(
                &format!("wit/workitemtypes/{}/fields", enc(&wit)),
                "$expand=all",
            ))
            .await?;
        // 2b. The TYPE's FORM layout, to tell fields the process SHOWS from ones merely
        //     associated with the type (a Bug hides System.Description in favour of Repro
        //     Steps). Best-effort: on any error we get an empty set and simply don't
        //     filter (better to show a stray field than hide a real one).
        let form_refs = self
            .get_json::<serde_json::Value>(
                &self.project_url(&format!("wit/workitemtypes/{}", enc(&wit)), ""),
            )
            .await
            .ok()
            .and_then(|t| {
                t.get("xmlForm")
                    .and_then(|x| x.as_str())
                    .map(form_field_refs)
            })
            .unwrap_or_default();
        // 3. The org FIELD CATALOG (each field's data type + readOnly). One call; the
        //    type-fields endpoint doesn't carry the data type, so we cross-reference.
        let catalog: AdoFieldCatalogResponse = self.get_json(&self.org_url("wit/fields")).await?;
        let defs: std::collections::HashMap<String, AdoFieldDef> = catalog
            .value
            .into_iter()
            .map(|d| (d.reference_name.clone(), d))
            .collect();

        let mut out: Vec<EditableField> = Vec::new();
        for tf in type_fields.value {
            if is_noise_field(&tf.reference_name) {
                continue;
            }
            let def = defs.get(&tf.reference_name);
            let ftype = def.map(|d| d.field_type.as_str()).unwrap_or("");
            if def.map(|d| d.read_only).unwrap_or(false) {
                continue; // computed / process-locked - not editable
            }
            // Identity fields report `type: string`; the `isIdentity` flag is the only
            // signal. Map them to Identity (so the value is read as a display name) -
            // otherwise the object value is lost and the field looks blank.
            let kind = if def.map(|d| d.is_identity).unwrap_or(false) {
                FieldKind::Identity
            } else {
                match map_field_kind(ftype, !tf.allowed_values.is_empty()) {
                    Some(k) => k,
                    None => continue, // guid, history, … - nothing to edit
                }
            };
            let label = if tf.name.trim().is_empty() {
                def.map(|d| d.name.clone())
                    .unwrap_or_else(|| tf.reference_name.clone())
            } else {
                tf.name.clone()
            };
            let value = field_value_to_string(values.get(&tf.reference_name), kind);
            // A field the process hides on the form AND that carries no value is not
            // something the user edits here (e.g. a Bug's System.Description, replaced by
            // Repro Steps). Drop it. We keep any off-form field that HAS a value, so we
            // never lose real data, and keep all fields when the form is unknown.
            let on_form =
                form_refs.is_empty() || form_refs.contains(&tf.reference_name.to_ascii_lowercase());
            if !on_form && value.trim().is_empty() {
                continue;
            }
            out.push(EditableField {
                reference: tf.reference_name.clone(),
                label,
                kind,
                value,
                options: tf
                    .allowed_values
                    .iter()
                    .filter_map(json_scalar_string)
                    .collect(),
                // Identity/tree-path fields are shown for context but not edited in v1
                // (writing them needs a resolvable identity / existing node path).
                read_only: matches!(kind, FieldKind::Identity),
                required: tf.always_required,
                help: tf.help_text.filter(|s| !s.trim().is_empty()),
            });
        }
        // Rich narrative fields first (what people edit + what AI drafts), then the rest
        // alphabetically - so Description / Repro Steps / Acceptance Criteria lead.
        out.sort_by_key(|f| (!f.kind.is_draftable(), f.label.to_lowercase()));
        Ok(out)
    }

    async fn update_fields(
        &self,
        id: i64,
        changes: &[FieldChange],
    ) -> Result<WorkItem, ProviderError> {
        if changes.is_empty() {
            return Err(ProviderError::Config("no fields to update".into()));
        }
        // Rich fields are html on the wire; the editor sends markdown. Fetch the field
        // catalog to know which references are html, then convert those md -> html.
        let catalog: AdoFieldCatalogResponse = self.get_json(&self.org_url("wit/fields")).await?;
        let html_fields: std::collections::HashSet<String> = catalog
            .value
            .into_iter()
            .filter(|d| d.field_type.eq_ignore_ascii_case("html"))
            .map(|d| d.reference_name)
            .collect();
        let ops: Vec<serde_json::Value> = changes
            .iter()
            .map(|c| {
                let value = if html_fields.contains(&c.reference) {
                    md_to_html(&c.value)
                } else {
                    c.value.clone()
                };
                serde_json::json!({ "op": "add", "path": format!("/fields/{}", c.reference), "value": value })
            })
            .collect();
        self.patch_work_item(id, ops).await
    }
}

// ─────────────────────────── Azure DevOps DTOs ───────────────────────────
// Only the fields POSEIDEN consumes. `Option` everywhere the API might omit a
// value so a sparse payload degrades gracefully rather than failing the poll.

/// `wit/workitemtypes/{type}/fields?$expand=all` - the fields ON a work-item type,
/// with per-type metadata (required?, help, allowed values). Does NOT carry the
/// field's data type, so it's cross-referenced with the org field catalog below.
#[derive(Debug, Deserialize)]
struct AdoTypeFieldsResponse {
    #[serde(default)]
    value: Vec<AdoTypeField>,
}

#[derive(Debug, Deserialize)]
struct AdoTypeField {
    #[serde(rename = "referenceName")]
    reference_name: String,
    #[serde(default)]
    name: String,
    #[serde(rename = "alwaysRequired", default)]
    always_required: bool,
    #[serde(rename = "helpText", default)]
    help_text: Option<String>,
    #[serde(rename = "allowedValues", default)]
    allowed_values: Vec<serde_json::Value>,
}

/// `wit/fields` - the org-wide field catalog. The authoritative source for each
/// field's data `type` (html / string / integer / …) and whether it's read-only.
#[derive(Debug, Deserialize)]
struct AdoFieldCatalogResponse {
    #[serde(default)]
    value: Vec<AdoFieldDef>,
}

#[derive(Debug, Deserialize)]
struct AdoFieldDef {
    #[serde(rename = "referenceName")]
    reference_name: String,
    #[serde(default)]
    name: String,
    #[serde(rename = "type", default)]
    field_type: String,
    #[serde(rename = "readOnly", default)]
    read_only: bool,
    /// Identity fields report `type: "string"` but carry `isIdentity: true` - the only
    /// way to tell "Assigned To" from a plain string, and to know the value is an
    /// identity object (displayName/uniqueName) rather than a bare string.
    #[serde(rename = "isIdentity", default)]
    is_identity: bool,
}

#[derive(Debug, Deserialize)]
struct WiqlResponse {
    #[serde(rename = "workItems", default)]
    work_items: Vec<WiqlRef>,
}

#[derive(Debug, Deserialize)]
struct WiqlRef {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct BatchResponse {
    #[serde(default)]
    value: Vec<AdoWorkItem>,
}

#[derive(Debug, Deserialize)]
struct AdoWorkItem {
    id: i64,
    #[serde(default)]
    fields: AdoFields,
    #[serde(rename = "_links")]
    links: Option<AdoLinks>,
    #[serde(default)]
    relations: Vec<AdoRelation>,
}

/// A work-item relation. PR links are `ArtifactLink`s whose url is a
/// `vstfs:///Git/PullRequestId/{proj}%2F{repo}%2F{prId}` artifact uri.
#[derive(Debug, Deserialize)]
struct AdoRelation {
    rel: String,
    url: String,
}

/// Extract the pull-request id from a PR artifact-link url, or `None` if the
/// relation isn't a PR link.
fn pr_id_from_relation(rel: &AdoRelation) -> Option<i64> {
    if !rel.rel.eq_ignore_ascii_case("ArtifactLink") {
        return None;
    }
    let url = &rel.url;
    if !url.contains("PullRequestId") {
        return None;
    }
    // The artifact uri is `vstfs:///Git/PullRequestId/{proj}<sep>{repo}<sep>{pr}`
    // where the separator is a url-encoded slash - and Azure DevOps emits it in
    // BOTH cases (`%2F` on some links, `%2f` on others), so normalise before we
    // split off the trailing id. Real links also occasionally use a literal `/`.
    let normalised = url.replace("%2f", "%2F");
    normalised
        .rsplit(['/'])
        .next()
        .and_then(|s| s.rsplit("%2F").next())
        .and_then(|s| s.parse::<i64>().ok())
}

/// The parent work-item id from a hierarchy relation (`Hierarchy-Reverse` points at
/// the parent), or `None` for any other relation. The url ends `/workItems/{id}`.
fn parent_id_from_relation(rel: &AdoRelation) -> Option<i64> {
    if !rel
        .rel
        .eq_ignore_ascii_case("System.LinkTypes.Hierarchy-Reverse")
    {
        return None;
    }
    rel.url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|s| s.parse::<i64>().ok())
}

/// Repo names referenced in text via Azure DevOps `/_git/{repo}/...` URLs (in a
/// pasted link OR an `<a href>` in the raw body). De-duplicated case-insensitively,
/// `%20`-decoded. A high-precision product/area signal.
fn extract_repos(text: &str) -> Vec<String> {
    const MARK: &str = "/_git/";
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find(MARK) {
        let after = &rest[pos + MARK.len()..];
        let seg: String = after
            .chars()
            .take_while(|c| {
                !matches!(
                    c,
                    '/' | '?' | '&' | '#' | ')' | '"' | '\'' | '<' | ' ' | '\n' | '\r' | '\t'
                )
            })
            .collect();
        let repo = seg.replace("%20", " ").trim().to_string();
        if !repo.is_empty() && !out.iter().any(|r| r.eq_ignore_ascii_case(&repo)) {
            out.push(repo);
        }
        rest = after;
    }
    out
}

#[derive(Debug, Default, Deserialize)]
struct AdoFields {
    #[serde(rename = "System.Title")]
    title: Option<String>,
    #[serde(rename = "System.WorkItemType")]
    work_item_type: Option<String>,
    #[serde(rename = "System.State")]
    state: Option<String>,
    #[serde(rename = "System.Tags")]
    tags: Option<String>,
    #[serde(rename = "System.AssignedTo")]
    assigned_to: Option<AdoIdentity>,
    #[serde(rename = "System.CreatedDate")]
    created_date: Option<DateTime<Utc>>,
    #[serde(rename = "System.ChangedDate")]
    changed_date: Option<DateTime<Utc>>,
    #[serde(rename = "Microsoft.VSTS.Common.ClosedDate")]
    closed_date: Option<DateTime<Utc>>,
    #[serde(rename = "System.IterationPath")]
    iteration_path: Option<String>,
    #[serde(rename = "Microsoft.VSTS.Scheduling.StoryPoints")]
    story_points: Option<f64>,
    #[serde(rename = "System.Description")]
    description: Option<String>,
    /// Bugs carry their body in Repro Steps, NOT Description (which is empty for
    /// them). Fall back to this so a bug with full repro steps isn't seen as
    /// empty-bodied by the tagger / underspecified check.
    #[serde(rename = "Microsoft.VSTS.TCM.ReproSteps")]
    repro_steps: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdoIdentity {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

/// Minimal shape of the work-item revisions response - just the area path of each
/// revision (we read the first one, the creation revision).
#[derive(Debug, Deserialize)]
struct RevisionsResponse {
    value: Vec<RevisionItem>,
}
#[derive(Debug, Deserialize)]
struct RevisionItem {
    fields: Option<RevisionFields>,
}
#[derive(Debug, Deserialize)]
struct RevisionFields {
    #[serde(rename = "System.AreaPath")]
    area_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdoLinks {
    html: Option<AdoHref>,
}

#[derive(Debug, Deserialize)]
struct AdoHref {
    href: String,
}

#[derive(Debug, Deserialize)]
struct DefinitionsResponse {
    #[serde(default)]
    value: Vec<AdoDefinition>,
}

#[derive(Debug, Deserialize)]
struct AdoDefinition {
    id: i64,
    name: String,
    /// Folder path, e.g. `\\Team\\CI`. Azure DevOps uses `"\\"` for the root.
    path: Option<String>,
    #[serde(rename = "_links")]
    links: Option<DefLinks>,
    /// The pipeline's most recent completed build, returned inline when the
    /// definitions call passes `includeLatestBuilds=true`. Gives the true last
    /// status without a per-pipeline query or a date window.
    #[serde(rename = "latestCompletedBuild")]
    latest_completed_build: Option<AdoBuild>,
}

#[derive(Debug, Deserialize)]
struct DefLinks {
    web: Option<AdoHref>,
}

#[derive(Debug, Deserialize)]
struct BuildsResponse {
    #[serde(default)]
    value: Vec<AdoBuild>,
}

#[derive(Debug, Deserialize)]
struct AdoBuild {
    id: i64,
    status: Option<String>,
    result: Option<String>,
    #[serde(rename = "startTime")]
    start_time: Option<DateTime<Utc>>,
    #[serde(rename = "finishTime")]
    finish_time: Option<DateTime<Utc>>,
    #[serde(rename = "sourceBranch")]
    source_branch: Option<String>,
    definition: Option<BuildDefinitionRef>,
    #[serde(rename = "_links")]
    links: Option<DefLinks>,
}

#[derive(Debug, Deserialize)]
struct BuildDefinitionRef {
    id: i64,
}

// ─────────────────────────── Normalisation ───────────────────────────
// Pure functions: raw DTO → core type. Unit-tested below.

/// Split Azure DevOps' `"a; b; c"` tag string into a clean list.
fn split_tags(raw: Option<&str>) -> Vec<String> {
    raw.map(|s| {
        s.split(';')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect()
    })
    .unwrap_or_default()
}

fn normalise_work_item(
    raw: AdoWorkItem,
    team_name: &str,
    organization: &str,
    ado_project: &str,
) -> WorkItem {
    let f = raw.fields;
    // Timestamps: prefer changed, fall back to created (and vice versa) so a
    // sparse payload never leaves staleness anchored on the Unix epoch.
    let changed = f.changed_date.or(f.created_date).unwrap_or_default();
    let created = f.created_date.or(f.changed_date).unwrap_or_default();
    let url = raw
        .links
        .and_then(|l| l.html)
        .map(|h| h.href)
        .unwrap_or_else(|| {
            format!(
                "{organization}/{}/_workitems/edit/{}",
                enc(ado_project),
                raw.id
            )
        });

    let linked_pr_ids: Vec<i64> = raw
        .relations
        .iter()
        .filter_map(pr_id_from_relation)
        .collect();
    let parent_id = raw.relations.iter().find_map(parent_id_from_relation);
    // Repo names from git links: scan the RAW body (so href="..." URLs survive, not
    // just visible link text) plus any Hyperlink relations.
    let mut repo_src = format!(
        "{} {}",
        f.description.as_deref().unwrap_or_default(),
        f.repro_steps.as_deref().unwrap_or_default()
    );
    for r in &raw.relations {
        if r.rel.eq_ignore_ascii_case("Hyperlink") {
            repo_src.push(' ');
            repo_src.push_str(&r.url);
        }
    }
    let linked_repos = extract_repos(&repo_src);

    WorkItem {
        id: raw.id,
        provider: PROVIDER.to_string(),
        team: team_name.to_string(),
        title: f.title.unwrap_or_default(),
        work_item_type: f.work_item_type.unwrap_or_default(),
        state: f.state.unwrap_or_default(),
        tags: split_tags(f.tags.as_deref()),
        assigned_to: f.assigned_to.and_then(|a| a.display_name),
        created_at: created,
        changed_at: changed,
        closed_at: f.closed_date,
        iteration_path: f.iteration_path,
        story_points: f.story_points,
        // Description first; for bugs it's empty, so fall back to Repro Steps.
        description: f
            .description
            .as_deref()
            .map(strip_html)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                f.repro_steps
                    .as_deref()
                    .map(strip_html)
                    .filter(|s| !s.is_empty())
            }),
        url,
        linked_pr_ids,
        parent_id,
        linked_repos,
        linked_prs: Vec::new(),
        tag_suggestions: Vec::new(),
    }
}

/// Crude HTML -> plain text for a work-item description: drop tags, decode the handful
/// of entities ADO emits, collapse whitespace. Good enough to feed a tagger (we never
/// render it), and dependency-free - no HTML parser pulled in just for this.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalise_pipeline(
    raw: AdoDefinition,
    team_name: &str,
    organization: &str,
    ado_project: &str,
) -> Pipeline {
    // Azure DevOps uses "\\" for the root folder - treat that as "no folder".
    let folder = raw.path.filter(|p| p != "\\" && !p.is_empty());
    let url = raw
        .links
        .and_then(|l| l.web)
        .map(|h| h.href)
        .unwrap_or_else(|| {
            format!(
                "{organization}/{}/_build?definitionId={}",
                enc(ado_project),
                raw.id
            )
        });
    // Most-recent completed build (from includeLatestBuilds), if any.
    let latest = raw.latest_completed_build;
    let last_run_status = latest
        .as_ref()
        .map(|b| map_run_status(b.status.as_deref(), b.result.as_deref()));
    let last_run_at = latest.as_ref().and_then(|b| b.finish_time);
    let last_run_url = latest
        .and_then(|b| b.links)
        .and_then(|l| l.web)
        .map(|h| h.href);

    Pipeline {
        id: raw.id,
        provider: PROVIDER.to_string(),
        team: team_name.to_string(),
        name: raw.name,
        folder,
        url,
        last_run_status,
        last_run_at,
        last_run_url,
    }
}

/// Map Azure DevOps' split status/result into POSEIDEN's single [`RunStatus`].
fn map_run_status(status: Option<&str>, result: Option<&str>) -> RunStatus {
    match status.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("completed") => match result.map(|r| r.to_ascii_lowercase()).as_deref() {
            Some("succeeded") | Some("partiallysucceeded") => RunStatus::Succeeded,
            Some("failed") => RunStatus::Failed,
            Some("canceled") | Some("cancelled") => RunStatus::Canceled,
            _ => RunStatus::Unknown,
        },
        Some("inprogress") | Some("notstarted") | Some("postponed") | Some("cancelling") => {
            RunStatus::Running
        }
        _ => RunStatus::Unknown,
    }
}

fn normalise_run(raw: AdoBuild, team_name: &str) -> PipelineRun {
    let url = raw
        .links
        .and_then(|l| l.web)
        .map(|h| h.href)
        .unwrap_or_default();
    PipelineRun {
        id: raw.id,
        pipeline_id: raw.definition.map(|d| d.id).unwrap_or(0),
        provider: PROVIDER.to_string(),
        team: team_name.to_string(),
        status: map_run_status(raw.status.as_deref(), raw.result.as_deref()),
        started_at: raw.start_time,
        finished_at: raw.finish_time,
        source_branch: raw.source_branch,
        url,
    }
}

// ── Pull requests ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PullRequestsResponse {
    #[serde(default)]
    value: Vec<AdoPullRequest>,
}

#[derive(Debug, Deserialize)]
struct AdoPullRequest {
    #[serde(rename = "pullRequestId")]
    pull_request_id: i64,
    title: Option<String>,
    status: Option<String>,
    #[serde(rename = "isDraft")]
    is_draft: Option<bool>,
    #[serde(rename = "createdBy")]
    created_by: Option<AdoIdentity>,
    #[serde(rename = "creationDate")]
    creation_date: Option<DateTime<Utc>>,
    repository: Option<AdoRepository>,
    #[serde(rename = "sourceRefName")]
    source_ref: Option<String>,
    #[serde(rename = "targetRefName")]
    target_ref: Option<String>,
    #[serde(default)]
    reviewers: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct AdoRepository {
    name: Option<String>,
    /// Repository GUID (present on the by-id PR fetch; used to build a PR's
    /// artifact-link uri when linking a work item).
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    project: Option<AdoProjectRef>,
}

#[derive(Debug, Deserialize)]
struct AdoProjectRef {
    #[serde(default)]
    id: Option<String>,
    /// Project name - the URL segment. A linked PR can live in a different
    /// project than the team's configured one, so the URL must use this, not the
    /// provider's `ado_project`.
    #[serde(default)]
    name: Option<String>,
}

/// Strip the `refs/heads/` prefix Azure DevOps puts on branch refs for display.
fn short_branch(refname: Option<String>) -> Option<String> {
    refname.map(|r| r.trim_start_matches("refs/heads/").to_string())
}

fn normalise_pull_request(
    raw: AdoPullRequest,
    team_name: &str,
    organization: &str,
    ado_project: &str,
) -> PullRequest {
    // The PR's repo may belong to a different project than the team's configured
    // one (a work item can link a PR from any project we can read), so take the
    // project from the repo itself, falling back to the configured project.
    let repo = raw.repository;
    let project = repo
        .as_ref()
        .and_then(|r| r.project.as_ref())
        .and_then(|p| p.name.clone())
        .unwrap_or_else(|| ado_project.to_string());
    let repository = repo.and_then(|r| r.name);
    // Azure DevOps PR objects don't carry a ready web URL, so build one:
    // {org}/{project}/_git/{repo}/pullrequest/{id}.
    let url = match &repository {
        Some(repo) => format!(
            "{organization}/{}/_git/{}/pullrequest/{}",
            enc(&project),
            enc(repo),
            raw.pull_request_id
        ),
        None => format!("{organization}/{}", enc(&project)),
    };
    PullRequest {
        id: raw.pull_request_id,
        provider: PROVIDER.to_string(),
        team: team_name.to_string(),
        title: raw.title.unwrap_or_default(),
        status: raw
            .status
            .as_deref()
            .map(PrStatus::from_slug)
            .unwrap_or(PrStatus::Unknown),
        is_draft: raw.is_draft.unwrap_or(false),
        repository,
        author: raw.created_by.and_then(|c| c.display_name),
        created_at: raw.creation_date,
        source_branch: short_branch(raw.source_ref),
        target_branch: short_branch(raw.target_ref),
        reviewer_count: raw.reviewers.len() as i64,
        url,
        flags: Vec::new(),
        linked_work_items: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_kind_mapping_covers_the_common_ado_types() {
        assert_eq!(map_field_kind("html", false), Some(FieldKind::Markdown));
        assert_eq!(
            map_field_kind("plainText", false),
            Some(FieldKind::PlainText)
        );
        assert_eq!(map_field_kind("string", false), Some(FieldKind::Text));
        // A string WITH an allowed-value list becomes a Select (e.g. System.State).
        assert_eq!(map_field_kind("string", true), Some(FieldKind::Select));
        assert_eq!(map_field_kind("integer", false), Some(FieldKind::Integer));
        assert_eq!(map_field_kind("double", false), Some(FieldKind::Float));
        assert_eq!(map_field_kind("dateTime", false), Some(FieldKind::DateTime));
        assert_eq!(map_field_kind("boolean", false), Some(FieldKind::Boolean));
        assert_eq!(map_field_kind("identity", false), Some(FieldKind::Identity));
        assert_eq!(map_field_kind("treePath", false), Some(FieldKind::Text));
        // Types with nothing to edit are dropped.
        assert_eq!(map_field_kind("history", false), None);
        assert_eq!(map_field_kind("guid", false), None);
    }

    #[test]
    fn form_field_refs_extracts_on_form_controls() {
        // A Bug form: Repro Steps + a custom field are on the form; System.Description
        // is NOT (that's the whole point). Mixed quoting + attribute order + casing.
        let xml = r#"
            <FORM><Layout>
              <Group><Control FieldName="Microsoft.VSTS.TCM.ReproSteps" Type="HtmlFieldControl" Label="Repro Steps"/></Group>
              <Group><Control Label='Expected' fieldname='Custom.ExpectedBehaviour' Type="HtmlFieldControl"/></Group>
              <Control  FieldName = "System.AreaPath" Type="WorkItemClassificationControl"/>
            </Layout></FORM>"#;
        let refs = form_field_refs(xml);
        assert!(refs.contains("microsoft.vsts.tcm.reprosteps"));
        assert!(refs.contains("custom.expectedbehaviour")); // lowercased attr name + value
        assert!(refs.contains("system.areapath")); // whitespace around '=' tolerated
        assert!(!refs.contains("system.description")); // absent from the form
                                                       // No form / junk -> empty set (callers then fall back to not filtering).
        assert!(form_field_refs("").is_empty());
        assert!(form_field_refs("<Form>no controls here</Form>").is_empty());
    }

    #[test]
    fn noise_fields_are_filtered_but_real_ones_kept() {
        assert!(is_noise_field("System.Id"));
        assert!(is_noise_field("System.Tags")); // edited via the tag-chip control
        assert!(is_noise_field("System.AreaLevel1"));
        assert!(is_noise_field("System.ChangedBy")); // system audit field
        assert!(is_noise_field("Microsoft.VSTS.Common.StackRank"));
        assert!(is_noise_field("WEF_ABC123_Kanban.Column"));
        assert!(!is_noise_field("System.Description"));
        assert!(!is_noise_field("Microsoft.VSTS.TCM.ReproSteps"));
        assert!(!is_noise_field("System.Title"));
    }

    #[test]
    fn html_field_value_comes_back_as_markdown() {
        let v = serde_json::json!("<div>Retry <b>&amp; backoff</b></div>");
        let md = field_value_to_string(Some(&v), FieldKind::Markdown);
        assert!(md.contains("Retry"));
        assert!(md.contains("backoff"));
        assert!(!md.contains("<div>")); // html stripped to markdown
    }

    #[test]
    fn identity_field_value_is_the_display_name() {
        let v = serde_json::json!({ "displayName": "Ada Lovelace", "uniqueName": "ada@x.io" });
        assert_eq!(
            field_value_to_string(Some(&v), FieldKind::Identity),
            "Ada Lovelace"
        );
    }

    #[test]
    fn markdown_round_trips_back_to_html_on_save() {
        // The save path converts the editor's markdown to html for the ADO write.
        let html = md_to_html("**bold** and a list:\n\n- one\n- two");
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<li>one</li>"));
    }

    #[test]
    fn split_tags_handles_joined_string_and_whitespace() {
        assert_eq!(
            split_tags(Some("team:platform; type:bug ;  priority:p1")),
            vec!["team:platform", "type:bug", "priority:p1"]
        );
        assert!(split_tags(Some("")).is_empty());
        assert!(split_tags(None).is_empty());
        assert!(split_tags(Some("  ;  ; ")).is_empty());
    }

    #[test]
    fn wiql_id_bounds_stay_within_azure_32bit_limit() {
        // Regression for TF51601: id-range chunking once emitted i64::MAX as a
        // bound, which Azure DevOps WIQL rejects ("unsupported number value").
        // Every id literal the builder emits must fit in 32 bits.
        assert!(MAX_WIQL_ID <= i32::MAX as i64);
        let q = build_wiql(None, false, Some((4_000_000, MAX_WIQL_ID)));
        assert!(q.contains(&MAX_WIQL_ID.to_string()));
        assert!(!q.contains(&i64::MAX.to_string()));
        for tok in q.split(|c: char| !c.is_ascii_digit()) {
            if let Ok(n) = tok.parse::<i64>() {
                assert!(n <= i32::MAX as i64, "WIQL emitted out-of-range number {n}");
            }
        }
    }

    #[test]
    fn map_run_status_covers_the_matrix() {
        assert_eq!(
            map_run_status(Some("completed"), Some("succeeded")),
            RunStatus::Succeeded
        );
        assert_eq!(
            map_run_status(Some("completed"), Some("partiallySucceeded")),
            RunStatus::Succeeded
        );
        assert_eq!(
            map_run_status(Some("completed"), Some("failed")),
            RunStatus::Failed
        );
        assert_eq!(
            map_run_status(Some("completed"), Some("canceled")),
            RunStatus::Canceled
        );
        assert_eq!(map_run_status(Some("inProgress"), None), RunStatus::Running);
        assert_eq!(map_run_status(Some("notStarted"), None), RunStatus::Running);
        assert_eq!(map_run_status(None, None), RunStatus::Unknown);
        assert_eq!(map_run_status(Some("completed"), None), RunStatus::Unknown);
    }

    #[test]
    fn normalise_work_item_from_sample_payload() {
        // A representative workitemsbatch element.
        let json = r#"{
            "id": 4321,
            "fields": {
                "System.Title": "Wire up ingress",
                "System.WorkItemType": "User Story",
                "System.State": "Active",
                "System.Tags": "team:platform; type:feature",
                "System.AssignedTo": { "displayName": "Ada Lovelace" },
                "System.CreatedDate": "2026-07-01T09:00:00Z",
                "System.ChangedDate": "2026-07-20T15:30:00Z",
                "System.IterationPath": "Platform\\Sprint 12",
                "Microsoft.VSTS.Scheduling.StoryPoints": 5.0
            },
            "_links": { "html": { "href": "https://dev.azure.com/acme/Platform/_workitems/edit/4321" } }
        }"#;
        let raw: AdoWorkItem = serde_json::from_str(json).unwrap();
        // Team name (with a space) is distinct from the ADO project segment.
        let wi = normalise_work_item(
            raw,
            "Platform Engineering",
            "https://dev.azure.com/acme",
            "Platform Engineering",
        );
        assert_eq!(wi.id, 4321);
        assert_eq!(wi.provider, "azure-devops");
        assert_eq!(wi.team, "Platform Engineering");
        assert_eq!(wi.title, "Wire up ingress");
        assert_eq!(wi.tags, vec!["team:platform", "type:feature"]);
        assert_eq!(wi.assigned_to.as_deref(), Some("Ada Lovelace"));
        assert_eq!(wi.story_points, Some(5.0));
        assert_eq!(wi.iteration_path.as_deref(), Some("Platform\\Sprint 12"));
        assert!(wi.url.contains("/edit/4321"));
    }

    #[test]
    fn strip_html_drops_tags_decodes_entities_and_collapses_whitespace() {
        // Tags are removed, leaving only their text content.
        assert_eq!(strip_html("<p>hello <b>world</b></p>"), "hello world");
        // The handful of entities ADO emits are decoded.
        assert_eq!(strip_html("Tom &amp; Jerry"), "Tom & Jerry");
        assert_eq!(strip_html("a&nbsp;b"), "a b");
        assert_eq!(strip_html("1 &lt; 2 &gt; 0"), "1 < 2 > 0");
        assert_eq!(
            strip_html("say &quot;hi&quot; it&#39;s fine"),
            "say \"hi\" it's fine"
        );
        // Runs of whitespace (incl. newlines/tabs from block tags) collapse to one
        // space, and leading/trailing whitespace is trimmed.
        assert_eq!(strip_html("  foo   bar\n\tbaz  "), "foo bar baz");
        assert_eq!(strip_html("<div>one</div>\n<div>two</div>"), "one two");
        // Nothing but markup / nothing at all -> empty string.
        assert_eq!(strip_html("<br/>"), "");
        assert_eq!(strip_html(""), "");
        assert_eq!(strip_html("   "), "");
    }

    #[test]
    fn normalise_work_item_strips_html_description() {
        // A description with markup + entities is normalised to clean plain text.
        let json = r#"{
            "id": 5,
            "fields": {
                "System.Title": "Has a body",
                "System.Description": "<div>Retry <b>&amp; backoff</b></div>"
            }
        }"#;
        let raw: AdoWorkItem = serde_json::from_str(json).unwrap();
        let wi = normalise_work_item(raw, "Platform", "https://dev.azure.com/acme", "Platform");
        assert_eq!(wi.description.as_deref(), Some("Retry & backoff"));
    }

    #[test]
    fn normalise_work_item_falls_back_to_repro_steps_for_bugs() {
        // Bugs have an empty Description; their body is in Repro Steps. The normaliser
        // must fall back to it so the item isn't seen as empty-bodied.
        let json = r#"{
            "id": 8,
            "fields": {
                "System.WorkItemType": "Bug",
                "System.Title": "Pipeline broken",
                "System.Description": "",
                "Microsoft.VSTS.TCM.ReproSteps": "<div>Error: invalid <b>for_each</b> argument</div>"
            }
        }"#;
        let raw: AdoWorkItem = serde_json::from_str(json).unwrap();
        let wi = normalise_work_item(raw, "Platform", "https://dev.azure.com/acme", "Platform");
        assert_eq!(
            wi.description.as_deref(),
            Some("Error: invalid for_each argument")
        );
        // A real Description still wins over Repro Steps when both are present.
        let both = r#"{ "id": 9, "fields": {
            "System.Description": "<p>real body</p>",
            "Microsoft.VSTS.TCM.ReproSteps": "<p>steps</p>" } }"#;
        let raw2: AdoWorkItem = serde_json::from_str(both).unwrap();
        let wi2 = normalise_work_item(raw2, "Platform", "https://dev.azure.com/acme", "Platform");
        assert_eq!(wi2.description.as_deref(), Some("real body"));
    }

    #[test]
    fn normalise_work_item_maps_empty_description_to_none() {
        // A markup-only (or whitespace-only) description strips to nothing, which
        // the normaliser filters to None rather than storing an empty string.
        let json = r#"{ "id": 6, "fields": { "System.Description": "<p>  </p>" } }"#;
        let raw: AdoWorkItem = serde_json::from_str(json).unwrap();
        let wi = normalise_work_item(raw, "Platform", "https://dev.azure.com/acme", "Platform");
        assert!(wi.description.is_none());
        // A wholly absent description field is also None.
        let bare: AdoWorkItem = serde_json::from_str(r#"{ "id": 7, "fields": {} }"#).unwrap();
        let wi2 = normalise_work_item(bare, "Platform", "https://dev.azure.com/acme", "Platform");
        assert!(wi2.description.is_none());
    }

    #[test]
    fn normalise_work_item_tolerates_sparse_payload() {
        // Missing fields must not panic - degrade to sensible defaults + a
        // constructed URL.
        let json = r#"{ "id": 7, "fields": {} }"#;
        let raw: AdoWorkItem = serde_json::from_str(json).unwrap();
        let wi = normalise_work_item(raw, "Data", "https://dev.azure.com/acme", "DataPlatform");
        assert_eq!(wi.id, 7);
        assert!(wi.title.is_empty());
        assert!(wi.tags.is_empty());
        assert!(wi.assigned_to.is_none());
        assert_eq!(
            wi.url,
            "https://dev.azure.com/acme/DataPlatform/_workitems/edit/7"
        );
    }

    #[test]
    fn pr_id_parsed_from_artifact_link_only() {
        // A PR artifact link yields its trailing id (segments are url-encoded).
        let pr_link = AdoRelation {
            rel: "ArtifactLink".to_string(),
            url: "vstfs:///Git/PullRequestId/proj%2Frepo%2F252573".to_string(),
        };
        assert_eq!(pr_id_from_relation(&pr_link), Some(252573));
        // Azure DevOps also emits the separator lower-cased (%2f) - both must
        // parse (a real captured payload mixes the two; see the fixture test).
        let lower = AdoRelation {
            rel: "ArtifactLink".to_string(),
            url: "vstfs:///Git/PullRequestId/proj%2frepo%2f248657".to_string(),
        };
        assert_eq!(pr_id_from_relation(&lower), Some(248657));
        // A non-PR artifact link (e.g. a build) is ignored.
        let build_link = AdoRelation {
            rel: "ArtifactLink".to_string(),
            url: "vstfs:///Build/Build/900".to_string(),
        };
        assert_eq!(pr_id_from_relation(&build_link), None);
        // A parent/child hierarchy relation is not an artifact link at all.
        let child = AdoRelation {
            rel: "System.LinkTypes.Hierarchy-Forward".to_string(),
            url: "https://dev.azure.com/acme/_apis/wit/workItems/55".to_string(),
        };
        assert_eq!(pr_id_from_relation(&child), None);
    }

    #[test]
    fn normalise_work_item_extracts_linked_pr_ids_from_relations() {
        // Two PR links + one unrelated relation -> only the PR ids survive.
        let json = r#"{
            "id": 900,
            "fields": { "System.Title": "Linked" },
            "relations": [
                { "rel": "ArtifactLink", "url": "vstfs:///Git/PullRequestId/p%2Fr%2F11" },
                { "rel": "System.LinkTypes.Hierarchy-Forward", "url": "https://dev.azure.com/acme/_apis/wit/workItems/7" },
                { "rel": "ArtifactLink", "url": "vstfs:///Git/PullRequestId/p%2Fr%2F22" }
            ]
        }"#;
        let raw: AdoWorkItem = serde_json::from_str(json).unwrap();
        let wi = normalise_work_item(raw, "Platform", "https://dev.azure.com/acme", "Platform");
        assert_eq!(wi.linked_pr_ids, vec![11, 22]);
    }

    #[test]
    fn normalise_pipeline_treats_root_folder_as_none() {
        let json = r#"{ "id": 12, "name": "platform-ci", "path": "\\",
            "_links": { "web": { "href": "https://dev.azure.com/acme/Platform/_build?definitionId=12" } } }"#;
        let raw: AdoDefinition = serde_json::from_str(json).unwrap();
        let p = normalise_pipeline(raw, "Platform", "https://dev.azure.com/acme", "Platform");
        assert_eq!(p.id, 12);
        assert_eq!(p.name, "platform-ci");
        assert!(p.folder.is_none());
    }

    #[test]
    fn normalise_pull_request_from_sample() {
        let json = r#"{
            "pullRequestId": 4321,
            "title": "Add retry to poller",
            "status": "active",
            "isDraft": true,
            "createdBy": { "displayName": "Ada Lovelace" },
            "creationDate": "2026-07-28T09:00:00Z",
            "repository": { "name": "platform-svc" },
            "sourceRefName": "refs/heads/feature/retry",
            "targetRefName": "refs/heads/main",
            "reviewers": [ { "vote": 10 }, { "vote": 0 } ]
        }"#;
        let raw: AdoPullRequest = serde_json::from_str(json).unwrap();
        let pr = normalise_pull_request(raw, "Platform", "https://dev.azure.com/acme", "Platform");
        assert_eq!(pr.id, 4321);
        assert_eq!(pr.title, "Add retry to poller");
        assert_eq!(pr.status, PrStatus::Active);
        assert!(pr.is_draft);
        assert_eq!(pr.author.as_deref(), Some("Ada Lovelace"));
        assert_eq!(pr.repository.as_deref(), Some("platform-svc"));
        assert_eq!(pr.source_branch.as_deref(), Some("feature/retry"));
        assert_eq!(pr.target_branch.as_deref(), Some("main"));
        assert_eq!(pr.reviewer_count, 2);
        assert!(pr.url.contains("_git/platform-svc/pullrequest/4321"));
    }

    #[test]
    fn pr_url_uses_repo_project_not_configured_project() {
        // A linked PR can live in a different project than the team's own. The URL
        // must use the repo's project ("Payments"), not the provider's ("Platform").
        let json = r#"{
            "pullRequestId": 252573,
            "status": "completed",
            "repository": { "name": "repo-c", "project": { "name": "Payments" } }
        }"#;
        let raw: AdoPullRequest = serde_json::from_str(json).unwrap();
        let pr = normalise_pull_request(
            raw,
            "Platform",
            "https://dev.azure.com/contoso",
            "Platform Engineering",
        );
        assert_eq!(
            pr.url,
            "https://dev.azure.com/contoso/Payments/_git/repo-c/pullrequest/252573"
        );
    }

    #[test]
    fn normalise_run_from_sample_build() {
        let json = r#"{
            "id": 99001,
            "status": "completed",
            "result": "failed",
            "startTime": "2026-07-29T10:00:00Z",
            "finishTime": "2026-07-29T10:12:00Z",
            "sourceBranch": "refs/heads/main",
            "definition": { "id": 12 },
            "_links": { "web": { "href": "https://dev.azure.com/acme/Platform/_build/results?buildId=99001" } }
        }"#;
        let raw: AdoBuild = serde_json::from_str(json).unwrap();
        let run = normalise_run(raw, "Platform");
        assert_eq!(run.id, 99001);
        assert_eq!(run.pipeline_id, 12);
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.source_branch.as_deref(), Some("refs/heads/main"));
        assert!(run.url.contains("buildId=99001"));
    }

    #[test]
    fn provider_builds_with_either_credential_kind() {
        let cfg = TeamConfig {
            name: "Platform Engineering".into(),
            provider: Default::default(),
            organization: "https://dev.azure.com/acme".into(),
            project: "Platform Engineering".into(),
            area_path: None,
            area_path_strict: false,
            tenant: None,
            auth: Default::default(),
            wiql: None,
            pipeline_ids: vec![],
            rules: None,
        };
        // Both a PAT (→ Basic) and an OAuth token (→ Bearer) construct cleanly.
        assert!(AzureDevOpsProvider::new(&cfg, Credential::Pat("tok".into())).is_ok());
        assert!(AzureDevOpsProvider::new(&cfg, Credential::Bearer("jwt".into())).is_ok());
    }

    #[test]
    fn credential_header_values() {
        // Bearer is verbatim; PAT is Basic base64(":<pat>").
        assert_eq!(
            Credential::Bearer("abc".into()).header_value(),
            "Bearer abc"
        );
        assert!(Credential::Pat("p".into())
            .header_value()
            .starts_with("Basic "));
    }

    #[test]
    fn build_wiql_scopes_by_area_path_when_set() {
        // No area path → project-wide, no AreaPath clause.
        let all = build_wiql(None, false, None);
        assert!(all.contains("[System.TeamProject] = @project"));
        assert!(!all.contains("AreaPath"));

        // Area path → adds an UNDER clause (backslash preserved verbatim).
        let scoped = build_wiql(Some(r"Platform\DevOps"), false, None);
        assert!(scoped.contains(r"[System.AreaPath] UNDER 'Platform\DevOps'"));

        // Strict mode → exact match (`=`), children excluded.
        let strict = build_wiql(Some("Platform"), true, None);
        assert!(strict.contains("[System.AreaPath] = 'Platform'"));
        assert!(!strict.contains("UNDER"));

        // A single quote in the area path is escaped so it can't break the WIQL.
        let tricky = build_wiql(Some("A'B"), false, None);
        assert!(tricky.contains("UNDER 'A''B'"));

        // Empty/whitespace area path is treated as "no scope".
        assert!(!build_wiql(Some("   "), false, None).contains("AreaPath"));

        // An id range adds a half-open [lo, hi) clause and orders by id (for
        // chunking past the 20000-row cap).
        let ranged = build_wiql(None, false, Some((1, 5000)));
        assert!(ranged.contains("[System.Id] >= 1 AND [System.Id] < 5000"));
        assert!(ranged.contains("ORDER BY [System.Id] ASC"));
    }

    #[test]
    fn result_limit_error_is_detected() {
        let cap = ProviderError::Api {
            status: 400,
            url: "u".into(),
            body: "VS402337: exceeds the size limit of 20000".into(),
        };
        assert!(is_result_limit(&cap));
        let other = ProviderError::Api {
            status: 400,
            url: "u".into(),
            body: "bad query".into(),
        };
        assert!(!is_result_limit(&other));
        assert!(!is_result_limit(&ProviderError::Config("x".into())));
    }

    #[test]
    fn enc_percent_encodes_spaces_in_project_names() {
        assert_eq!(enc("Platform Engineering"), "Platform%20Engineering");
        // Unreserved chars pass through untouched.
        assert_eq!(enc("Data-Platform_2.0"), "Data-Platform_2.0");
    }

    // ── Fixture tests ───────────────────────────────────────────────────
    // Captured live from Azure DevOps and anonymised (org/project/repo/people/
    // GUIDs/branches/titles all scrubbed; see the fixtures dir). They pin our
    // deserialisation + normalisation against the REAL response shape, which
    // hand-written JSON can't - e.g. these caught PR artifact links whose
    // separator is emitted in mixed case (%2F and %2f in the same payload).

    #[test]
    fn workitemsbatch_fixture_extracts_pr_links_across_separator_casing() {
        let json = include_str!("fixtures/workitemsbatch.json");
        let parsed: BatchResponse = serde_json::from_str(json).expect("real batch payload");
        let items: Vec<WorkItem> = parsed
            .value
            .into_iter()
            .map(|raw| {
                normalise_work_item(
                    raw,
                    "Example",
                    "https://dev.azure.com/contoso",
                    "Example Project",
                )
            })
            .collect();
        let by_id = |id: i64| items.iter().find(|w| w.id == id).unwrap();
        // 889555's PR link uses an upper-case %2F separator...
        assert_eq!(by_id(889555).linked_pr_ids, vec![252573]);
        // ...while 865173's two links use lower-case %2f - both must be captured,
        // and the sibling commit/hierarchy relations must NOT leak in as PR ids.
        assert_eq!(by_id(865173).linked_pr_ids, vec![248657, 248674]);
    }

    #[test]
    fn pullrequests_fixture_deserialises_and_normalises() {
        let json = include_str!("fixtures/pullrequests.json");
        let parsed: PullRequestsResponse =
            serde_json::from_str(json).expect("real PR list payload");
        assert!(!parsed.value.is_empty());
        for raw in parsed.value {
            let pr = normalise_pull_request(
                raw,
                "Example",
                "https://dev.azure.com/contoso",
                "Example Project",
            );
            assert!(pr.id > 0);
            // Every PR builds a usable web URL and a mapped status.
            assert!(pr.url.starts_with("https://dev.azure.com/contoso/"));
            assert_ne!(pr.status, PrStatus::Unknown);
        }
    }

    #[test]
    fn pull_request_by_id_fixture_builds_cross_project_url() {
        let json = include_str!("fixtures/pullrequest_by_id.json");
        let raw: AdoPullRequest = serde_json::from_str(json).expect("real single-PR payload");
        let pr = normalise_pull_request(
            raw,
            "Example",
            "https://dev.azure.com/contoso",
            "Example Project",
        );
        // The captured PR lives in a different project than the configured one;
        // its URL must use the repo's own project segment.
        assert_eq!(pr.id, 252573);
        assert_eq!(pr.status, PrStatus::Completed);
        assert!(
            pr.url
                .contains("/Second%20Project/_git/repo-c/pullrequest/252573"),
            "unexpected URL: {}",
            pr.url
        );
    }

    #[test]
    fn definitions_fixture_folds_in_latest_build() {
        let json = include_str!("fixtures/definitions.json");
        let parsed: DefinitionsResponse =
            serde_json::from_str(json).expect("real definitions payload");
        assert!(!parsed.value.is_empty());
        let pipelines: Vec<_> = parsed
            .value
            .into_iter()
            .map(|d| {
                normalise_pipeline(
                    d,
                    "Example",
                    "https://dev.azure.com/contoso",
                    "Example Project",
                )
            })
            .collect();
        // includeLatestBuilds=true means a completed definition carries its last
        // run status/time (the fix for "never run" on long-idle pipelines).
        assert!(
            pipelines
                .iter()
                .any(|p| p.last_run_status.is_some() && p.last_run_at.is_some()),
            "expected at least one pipeline with a folded-in last run"
        );
    }

    #[test]
    fn builds_fixture_deserialises_and_normalises() {
        let json = include_str!("fixtures/builds.json");
        let parsed: BuildsResponse = serde_json::from_str(json).expect("real builds payload");
        assert!(!parsed.value.is_empty());
        for raw in parsed.value {
            let run = normalise_run(raw, "Example");
            assert!(run.id > 0);
            assert!(
                run.pipeline_id > 0,
                "a build should reference its definition"
            );
        }
    }
}

#[cfg(test)]
mod link_roundtrip {
    use super::{html_to_md, md_to_html};

    #[test]
    fn a_valid_link_survives_html_md_html_round_trip() {
        // A real ADO link with a gnarly URL (underscore, &, //, %40) must round-trip
        // through the editor's markdown intact - the field editor's save path is
        // html(from ADO) -> md(edit) -> html(back). Regression guard: proves a broken
        // link after saving is the model's doing, not our conversion.
        let html = "<div>See <a href=\"https://dev.azure.com/x/_search?text=a%40b.com&amp;lp=custom-Collection&amp;result=DevOps/App/GBmaster//azure-pipelines/cd.yml\">the pipeline</a>.</div>";
        let back = md_to_html(&html_to_md(html));
        assert!(
            back.contains("<a href=\"https://dev.azure.com/x/_search?text=a%40b.com"),
            "link lost in round-trip: {back}"
        );
        assert!(back.contains(">the pipeline</a>"), "link text lost: {back}");
    }
}
