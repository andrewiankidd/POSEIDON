//! Service-catalog ingestion (Stage 1: CSV source).
//!
//! A [`CatalogSource`] reads an internal developer portal's service catalog and
//! normalises it into vendor-neutral [`CatalogEntity`] rows (`repo -> product ->
//! team`). It's the sibling of [`crate::Provider`]: the trait is the seam that
//! keeps the rest of POSEIDON catalog-agnostic, so Port / Backstage / ServiceNow
//! each become one more `impl` and nothing else changes.
//!
//! Design: [`docs/design/catalog-integration.md`]. This stage ships [`CsvCatalog`]
//! (a Port "Service" export) end-to-end; [`PortCatalog`] / [`BackstageCatalog`] are
//! stubs behind the same trait.
//!
//! All parsing is pure + sync ([`CsvCatalog::parse_entities`]) so it's unit-tested
//! against sample CSV with no network or runtime (build principle #8).

use async_trait::async_trait;

// The `CatalogEntity` domain type + the canonicalisation / repo->product transform
// live in `poseidon-core` (so the store + rules can use them without depending on
// this integration crate); re-exported here for callers that only touch providers.
pub use poseidon_core::{canonical_product_slug, repo_product_map, CatalogEntity};

/// Errors a catalog source can surface. Coarse on purpose - a sync only reads.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("catalog parse error: {0}")]
    Parse(String),
    #[error("catalog is missing the expected column: {0}")]
    MissingColumn(String),
    #[error("catalog source not implemented yet: {0}")]
    Unsupported(&'static str),
}

/// Which catalog columns carry which logical field. Catalog schemas are
/// user-defined (Port blueprints vary, Backstage kinds differ), so the mapping is
/// config, not code - this is what keeps [`CsvCatalog`] generic rather than
/// Port-specific.
#[derive(Debug, Clone)]
pub struct FieldMap {
    pub product: String,
    pub team: String,
    /// Column holding the repo, either a bare name or a git URL (`…/_git/<repo>`).
    pub repo_source: String,
    pub kind: Option<String>,
    pub domain: Option<String>,
}

impl FieldMap {
    /// The column names in a Port "Service" catalog export.
    pub fn port_service_export() -> Self {
        Self {
            product: "Product".into(),
            team: "Owning Teams".into(),
            repo_source: "Source".into(),
            kind: Some("Type".into()),
            domain: None,
        }
    }
}

impl Default for FieldMap {
    fn default() -> Self {
        Self::port_service_export()
    }
}

/// A catalog read from an exported CSV (the manual interface). Holds the raw CSV
/// text + the field map; [`parse_entities`](Self::parse_entities) does the work.
pub struct CsvCatalog {
    csv: String,
    map: FieldMap,
}

impl CsvCatalog {
    pub fn new(csv: impl Into<String>, map: FieldMap) -> Self {
        Self {
            csv: csv.into(),
            map,
        }
    }

    /// Parse the CSV into catalog entities. Pure + sync: the whole point of the
    /// test surface. Rows with neither a repo nor a product are dropped.
    pub fn parse_entities(&self) -> Result<Vec<CatalogEntity>, CatalogError> {
        let rows = parse_csv(&self.csv);
        let mut it = rows.into_iter();
        let header = it
            .next()
            .ok_or_else(|| CatalogError::Parse("empty CSV (no header row)".into()))?;
        let col = |name: &str| header.iter().position(|h| h == name);
        let need =
            |name: &str| col(name).ok_or_else(|| CatalogError::MissingColumn(name.to_string()));

        let i_product = need(&self.map.product)?;
        let i_team = need(&self.map.team)?;
        let i_repo = need(&self.map.repo_source)?;
        let i_kind = self.map.kind.as_deref().and_then(col);
        let i_domain = self.map.domain.as_deref().and_then(col);

        let get = |row: &[String], i: usize| -> Option<String> {
            row.get(i)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };

        let mut out = Vec::new();
        for row in it {
            if row.iter().all(|f| f.trim().is_empty()) {
                continue; // blank line
            }
            let entity = CatalogEntity {
                repo: get(&row, i_repo).and_then(|s| repo_from_source(&s)),
                product: get(&row, i_product),
                team: get(&row, i_team),
                domain: i_domain.and_then(|i| get(&row, i)),
                kind: i_kind.and_then(|i| get(&row, i)),
            };
            if entity.is_useful() {
                out.push(entity);
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl CatalogSource for CsvCatalog {
    fn source_name(&self) -> &str {
        "csv"
    }
    async fn fetch(&self) -> Result<Vec<CatalogEntity>, CatalogError> {
        self.parse_entities()
    }
}

/// A catalog source POSEIDON syncs. One instance per configured catalog.
#[async_trait]
pub trait CatalogSource: Send + Sync {
    /// Stable slug for the source kind (`"csv"`, `"port"`, …).
    fn source_name(&self) -> &str;
    /// All catalog entities, normalised.
    async fn fetch(&self) -> Result<Vec<CatalogEntity>, CatalogError>;
}

/// Live Port (getport.io) API source. STUB - the real fix, not yet built; the CSV
/// source ships first behind this same trait, so this becomes a drop-in `impl`.
pub struct PortCatalog;

#[async_trait]
impl CatalogSource for PortCatalog {
    fn source_name(&self) -> &str {
        "port"
    }
    async fn fetch(&self) -> Result<Vec<CatalogEntity>, CatalogError> {
        Err(CatalogError::Unsupported("port"))
    }
}

/// Backstage catalog source. STUB - same trait, later `impl`.
pub struct BackstageCatalog;

#[async_trait]
impl CatalogSource for BackstageCatalog {
    fn source_name(&self) -> &str {
        "backstage"
    }
    async fn fetch(&self) -> Result<Vec<CatalogEntity>, CatalogError> {
        Err(CatalogError::Unsupported("backstage"))
    }
}

/// Extract a bare repo name from a catalog `Source` value: either an Azure DevOps
/// git URL (`https://…/_git/<repo>`) or an already-bare name. Percent-decodes the
/// segment. Mirrors how `WorkItem.linked_repos` are derived, so the two join.
fn repo_from_source(source: &str) -> Option<String> {
    let s = source.trim();
    if s.is_empty() {
        return None;
    }
    let seg = match s.find("/_git/") {
        Some(idx) => {
            let rest = &s[idx + "/_git/".len()..];
            rest.split(['/', '?', '#']).next().unwrap_or("")
        }
        // Not a git URL: treat the whole value as a bare repo name only if it isn't
        // some other URL (a bare name has no scheme).
        None if !s.contains("://") => s,
        None => return None,
    };
    let decoded = percent_decode(seg);
    let decoded = decoded.trim();
    if decoded.is_empty() {
        None
    } else {
        Some(decoded.to_string())
    }
}

/// Minimal percent-decode (enough for catalog URL path segments: `%20` etc.).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse RFC 4180-ish CSV: double-quoted fields, `""` escapes, and commas /
/// newlines inside quotes. Returns rows of fields; a trailing newline yields no
/// extra empty row. Self-contained so the crate takes no CSV dependency for this.
fn parse_csv(input: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => row.push(std::mem::take(&mut field)),
                '\r' => {}
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                _ => field.push(c),
            }
        }
    }
    // Trailing field/row when the file doesn't end in a newline.
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Synthetic catalog data only - never real org/product/repo names (those live in
    // the gitignored tenant bundle). The double-dash-trailing id mirrors the shape a
    // real catalog emits so the canonicaliser is exercised faithfully.
    fn aliases() -> HashMap<String, String> {
        [
            ("product-contoso-pay", "pay"),
            ("widget-assistant--wa-", "wa"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    // ── CSV parser ───────────────────────────────────────────────────────────
    #[test]
    fn parse_csv_handles_quoted_commas_escaped_quotes_and_crlf() {
        let csv = "A,B,C\r\n\"x,y\",\"she said \"\"hi\"\"\",z\r\n";
        let rows = parse_csv(csv);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["A", "B", "C"]);
        assert_eq!(rows[1], vec!["x,y", "she said \"hi\"", "z"]);
    }

    #[test]
    fn parse_csv_keeps_final_row_without_trailing_newline() {
        let rows = parse_csv("A,B\n1,2");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], vec!["1", "2"]);
    }

    // ── repo extraction ──────────────────────────────────────────────────────
    #[test]
    fn repo_from_source_parses_git_url_and_bare_name() {
        assert_eq!(
            repo_from_source("https://dev.azure.com/contoso/ProjectA/_git/Contoso.Service"),
            Some("Contoso.Service".to_string())
        );
        // project segment carries %20; the repo is the /_git/ segment, decoded.
        assert_eq!(
            repo_from_source("https://dev.azure.com/contoso/Team%20One/_git/My%20Repo"),
            Some("My Repo".to_string())
        );
        assert_eq!(
            repo_from_source("BareRepoName"),
            Some("BareRepoName".to_string())
        );
        assert_eq!(repo_from_source("  "), None);
        // some other URL (no /_git/) isn't a repo
        assert_eq!(repo_from_source("https://sharepoint/doc"), None);
    }

    // (canonicalisation is tested in poseidon-core::catalog, where it lives)

    // ── end-to-end CSV -> entities -> repo/product map ───────────────────────
    fn sample_csv() -> &'static str {
        // Header mirrors a Port "Service" export; rows cover: a service with a product,
        // a quoted-comma description, an empty-Product row (repo only), and a blank line.
        "Title,Type,Owning Teams,Source,Description,Product,Framework,Language,Link\n\
         Assistant API,API,team-alpha,https://dev.azure.com/contoso/ProjectA/_git/Contoso.Assistant,\"Merges products, cleanly\",widget-assistant--wa-,.NET,C#,\n\
         Ledger,API,team-beta,https://dev.azure.com/contoso/ProjectA/_git/Ledger,,,,,C#,\n\
         \n"
    }

    #[test]
    fn csv_catalog_parses_entities_dropping_useless_rows() {
        let cat = CsvCatalog::new(sample_csv(), FieldMap::port_service_export());
        let ents = cat.parse_entities().unwrap();
        assert_eq!(ents.len(), 2, "Assistant + Ledger; blank line dropped");

        let assistant = &ents[0];
        assert_eq!(assistant.repo.as_deref(), Some("Contoso.Assistant"));
        assert_eq!(assistant.product.as_deref(), Some("widget-assistant--wa-"));
        assert_eq!(assistant.team.as_deref(), Some("team-alpha"));
        assert_eq!(assistant.kind.as_deref(), Some("API"));

        // repo present, product empty -> still useful (kept), product None.
        let ledger = &ents[1];
        assert_eq!(ledger.repo.as_deref(), Some("Ledger"));
        assert_eq!(ledger.product, None);
    }

    #[test]
    fn repo_product_map_derives_catalog_tags() {
        let cat = CsvCatalog::new(sample_csv(), FieldMap::port_service_export());
        let ents = cat.parse_entities().unwrap();
        let map = repo_product_map(&ents, &aliases());
        // Assistant repo -> product:wa (via alias); Ledger has no product -> absent.
        assert_eq!(
            map.get("Contoso.Assistant").map(String::as_str),
            Some("product:wa")
        );
        assert!(!map.contains_key("Ledger"));
    }

    #[test]
    fn missing_required_column_is_an_error() {
        let cat = CsvCatalog::new("Title,Source\nx,y\n", FieldMap::port_service_export());
        assert!(matches!(
            cat.parse_entities(),
            Err(CatalogError::MissingColumn(_))
        ));
    }

    #[tokio::test]
    async fn stubs_report_unsupported_behind_the_trait() {
        assert!(matches!(
            PortCatalog.fetch().await,
            Err(CatalogError::Unsupported("port"))
        ));
        assert!(matches!(
            BackstageCatalog.fetch().await,
            Err(CatalogError::Unsupported("backstage"))
        ));
    }
}
