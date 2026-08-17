//! The portable configuration bundle - the unit of import / export.
//!
//! One owner's full configuration as a single, self-describing document (YAML on
//! the wire). It's what powers backup/restore, sharing a team's setup, migrating
//! standalone <-> hosted, and declarative/GitOps config for headless runs. It
//! deliberately carries **no secrets** (the PAT lives in the environment) and no
//! per-device UI preferences (those stay client-side).

use serde::{Deserialize, Serialize};

use crate::{ReportSpec, UserConfig};

/// Current bundle schema version. Bump on a breaking format change; an import
/// refuses a bundle whose `schema` is newer than the running build supports.
pub const CONFIG_BUNDLE_SCHEMA: u32 = 1;

/// A portable, owner-scoped configuration document.
///
/// The envelope sits under a `poseidon:` key so the file self-identifies (like a
/// manifest's `apiVersion`); the owner's config is flattened to the top level so
/// the document stays flat and readable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigBundle {
    /// Envelope: schema version + provenance.
    pub poseidon: BundleMeta,
    /// The owner's configuration - teams, rules, doctor checks, poll scope.
    #[serde(flatten)]
    pub config: UserConfig,
    /// Saved report specs (stored separately from [`UserConfig`] in the DB).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reports: Vec<ReportSpec>,
}

/// Bundle envelope - version + provenance, all informational except `schema`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleMeta {
    /// Bundle format version (see [`CONFIG_BUNDLE_SCHEMA`]).
    pub schema: u32,
    /// App version that produced the bundle.
    #[serde(default)]
    pub app_version: String,
    /// RFC3339 export timestamp.
    #[serde(default)]
    pub exported_at: String,
    /// Target tenant for a **trusted** (CLI / startup / backend) import - which
    /// owner's config this bundle seeds. Optional; absent means the default
    /// tenant. It is deliberately **ignored by the public HTTP import**, which
    /// always writes the authenticated caller's own tenant - so a user can never
    /// import a file that overwrites another tenant. Export sets it so a bundle
    /// round-trips through a trusted re-import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}
