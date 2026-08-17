//! Service-catalog domain types (vendor-neutral).
//!
//! A [`CatalogEntity`] is the normalised shape a catalog source (Port, Backstage,
//! a CSV export) yields: one service/component carrying `repo -> product -> team`.
//! It lives in core - like [`crate::WorkItem`] - so the store can persist it and
//! the rules/tagging layer can consume it without depending on the integration
//! crate (the `CatalogSource` trait itself lives in `poseidon-providers`).
//!
//! Design: `docs/design/catalog-integration.md`.

use std::collections::{BTreeMap, HashMap};

/// A normalised catalog record - one service/component. Transitively carries the
/// mapping POSEIDON needs: which repo belongs to which product, owned by which team.
/// Every field is optional: a catalog row may name a repo with no product, a product
/// with no repo, etc.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogEntity {
    /// Git repo name - the join key to `WorkItem.linked_repos`.
    pub repo: Option<String>,
    /// Raw catalog product id, BEFORE canonicalisation to a taxonomy slug.
    pub product: Option<String>,
    /// Owning team.
    pub team: Option<String>,
    /// Optional grouping (Port domain / Backstage system).
    pub domain: Option<String>,
    /// Service kind (API / SPA / Microservice / …) - metadata, not a tag.
    pub kind: Option<String>,
}

impl CatalogEntity {
    /// Whether this row carries anything worth storing (a repo or a product).
    pub fn is_useful(&self) -> bool {
        self.repo.is_some() || self.product.is_some()
    }
}

/// Canonicalise a raw catalog product id to a taxonomy slug (the value after
/// `product:`). An explicit alias wins; otherwise derive: lowercase, drop a leading
/// `product-`/`product_`, collapse any run of non-alphanumerics to a single `-`, and
/// trim. `aliases` is the one hand-maintained bridge - the catalog owns the facts,
/// POSEIDON owns the vocabulary.
pub fn canonical_product_slug(raw: &str, aliases: &HashMap<String, String>) -> String {
    if let Some(slug) = aliases.get(raw) {
        return slug.clone();
    }
    let lower = raw.trim().to_ascii_lowercase();
    let stripped = lower
        .strip_prefix("product-")
        .or_else(|| lower.strip_prefix("product_"))
        .unwrap_or(&lower);
    let mut slug = String::with_capacity(stripped.len());
    let mut prev_dash = false;
    for ch in stripped.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

/// Collapse catalog entities to a deterministic `repo -> product:<slug>` map - the
/// `repo_tags` product signal, but derived from the catalog instead of hand-listed.
/// Entities with both a repo and a product contribute; sorted (BTreeMap) for stable
/// output. Ownership/team axes are layered separately.
pub fn repo_product_map(
    entities: &[CatalogEntity],
    aliases: &HashMap<String, String>,
) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for e in entities {
        if let (Some(repo), Some(product)) = (&e.repo, &e.product) {
            let slug = canonical_product_slug(product, aliases);
            if !slug.is_empty() {
                map.insert(repo.clone(), format!("product:{slug}"));
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic data only - never real org/product/repo names.
    fn aliases() -> HashMap<String, String> {
        [
            ("product-contoso-pay", "pay"),
            ("widget-assistant--wa-", "wa"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    #[test]
    fn canonical_slug_prefers_alias_then_derives() {
        let a = aliases();
        assert_eq!(canonical_product_slug("product-contoso-pay", &a), "pay");
        assert_eq!(canonical_product_slug("widget-assistant--wa-", &a), "wa");
        assert_eq!(
            canonical_product_slug("product-foo-bar-module", &HashMap::new()),
            "foo-bar-module"
        );
        assert_eq!(
            canonical_product_slug("core_app", &HashMap::new()),
            "core-app"
        );
        assert_eq!(
            canonical_product_slug("widget-assistant--wa-", &HashMap::new()),
            "widget-assistant-wa"
        );
    }

    #[test]
    fn repo_product_map_keeps_only_repo_and_product_rows() {
        let ents = vec![
            CatalogEntity {
                repo: Some("Contoso.Assistant".into()),
                product: Some("widget-assistant--wa-".into()),
                ..Default::default()
            },
            CatalogEntity {
                repo: Some("Ledger".into()),
                product: None, // no product -> excluded
                ..Default::default()
            },
        ];
        let map = repo_product_map(&ents, &aliases());
        assert_eq!(
            map.get("Contoso.Assistant").map(String::as_str),
            Some("product:wa")
        );
        assert!(!map.contains_key("Ledger"));
    }
}
