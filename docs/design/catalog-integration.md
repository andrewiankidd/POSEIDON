# Catalog integration (service catalog → taxonomy)

Status: **scoped, not built** · Supersedes the hand-maintained `repo_tags` product
map · Backlog: [BACKLOG.md](../BACKLOG.md) "IDP / service-catalog lookup"

## Problem

POSEIDON tags work items on three required axes — `product:*` (which system),
`area:*` (what kind of work), `source:*` (how it arrived). `product:*` is only as
good as the repo→product knowledge in the ruleset, which today is a **hand-seeded
`repo_tags` map** covering a fraction of the real catalog. It drifts the moment a
new product/service lands and goes stale silently — the AI then correctly returns
"none apply" because the mapping it was shown genuinely doesn't contain the product
(observed live: a spike about product "AVA" tagged nothing because AVA — 11 services
in the Port export — was absent from the config).

The repo→service→product→team graph is **owned by an internal developer portal**
(e.g. Port). POSEIDON should consume that graph, not re-curate a copy of it.

## Shape: a generic **Catalog**, mirroring `Provider`

Do **not** hardcode "Port". The generic concept is a **Catalog**, plugged in the
same way providers are (build principle #1: nothing in core names the vendor):

```
trait CatalogSource {           // sibling of Provider
    async fn fetch(&self) -> Vec<CatalogEntity>;
}

struct CatalogEntity {          // normalized, vendor-neutral
    repo:    Option<String>,    // join key to WorkItem.linked_repos
    product: Option<String>,    // raw catalog product id (pre-canonicalisation)
    team:    Option<String>,    // owning team
    domain:  Option<String>,    // optional grouping
    kind:    Option<String>,    // API / SPA / Microservice / … (metadata)
}
```

Implementations, cheapest first:

- **`CsvCatalog`** — a Port "Service" export (`Title, Type, Owning Teams, Source,
  Description, Product, Framework, Language, Link`). This is the manual interface;
  what the one-off import script already parses.
- **`PortCatalog`** — the live Port API (getport.io). The real fix.
- Later: `BackstageCatalog`, `ServiceNowCatalog`, … each just another `impl`.

Because catalog schemas are user-defined (Port blueprints vary, Backstage kinds
differ), each source carries a small **field-mapping config** — "Product field →
`product:` axis, Owning Teams → ownership, Source → repo". That mapping is what
keeps it generic instead of Port-specific.

## What we ingest: **just the service catalog** (one entity)

The service/component record transitively carries everything the tagger needs —
`repo → product → team` in one row. We do **not** separately ingest Product or Team
blueprints unless we want their own metadata (a product description, a team's
on-call). Environments / APIs / Domains / Deployments exist in Port but have **no
work-item use case today** — add them only when one appears. Don't ingest what we
can't consume.

## Integration level: **poll-time sync**, snapshot as the offline fallback

| Tier | Mechanism | Verdict |
|---|---|---|
| A. Import snapshot | export file → materialised map in config | offline/portable fallback |
| **B. Poll-time sync** | scheduler pulls catalog on an interval → `catalog` table | **hosted default** |
| C. Query-time live lookup | look up each repo in the live IDP per evaluate | ❌ latency, hammers the IDP |

Build **B with A as the fallback** — it mirrors POSEIDON's existing dual nature
(hosted polls live providers; portable/desktop runs off imported config):

- The catalog is a **first-class polled resource** alongside work items / pipelines
  / PRs — `sync_catalog()` next to `poll_once()` on the same scheduler.
- It lands in an owner-scoped **`catalog` store table** (`repo → product/team/domain`).
- The tagging engine reads repo→product **from that table**; the static `repo_tags`
  config **demotes to a manual-override layer** for the few cases the catalog gets
  wrong or doesn't cover. Catalog authoritative, config the exception list.
- The allowed `product:*` set can be **derived from the catalog** instead of
  hand-listed in `allowed_tags`.

## The boundary that matters: who owns the vocabulary

- **Port owns the facts** — which services/repos/products/teams exist and relate.
- **POSEIDON owns the taxonomy** — the canonical `product:`/`area:`/`source:` slugs,
  *and* the axes Port doesn't model at all: `area:` (deployment/k8s/observability)
  and `source:` (how it arrived) are POSEIDON-native and never come from the catalog.

The only hand-maintained bridge is a **canonicalisation map**: raw catalog product
id → taxonomy slug (`widget-assistant--wa-` → `product:wa`,
`product-acme-pay` → `product:pay`). ~10 curated lines; everything else auto-slugs.
This is the `OVERRIDE` table in the one-off import script — it graduates into config.

## Config sketch (per-owner ruleset)

```yaml
catalog:
  source: port            # port | csv | backstage | none
  url_env: POSEIDON_PORT_URL
  token_env: POSEIDON_PORT_TOKEN   # secret via env, never in config/DB (principle #6)
  sync_interval: 24h
  field_map: { product: "Product", team: "Owning Teams", repo: "Source" }
  product_aliases:        # raw catalog id -> taxonomy slug (the only hand bit)
    widget-assistant--wa-: wa
    product-acme-pay: pay
  derive_allowed_products: true    # allow-list = catalog products ∪ manual
```

## Decisions

1. **First source — `CsvCatalog`, others stubbed. DECIDED 2026-08-17.** Ship the CSV
   source end-to-end (the `CatalogSource` trait, `CatalogEntity`, canonicalisation,
   `catalog` table, scheduler sync, config schema) with `PortCatalog` /
   `BackstageCatalog` as `todo!()` stubs behind the same trait. The interface and
   the CSV→taxonomy transform land and get proven; `PortCatalog` is then a drop-in
   `impl` with no other change (build principle #1).
2. **`team:` as its own axis?** — DROPPED 2026-08-17. Owning-team stays metadata on
   the catalog row; ownership continues to feed the existing `source:external` signal.
   No first-class `team:*` axis.

## Now authoritative (2026-08-17)

The catalog is the product source: the hand-maintained `repo_tags` product backfill
(57 entries) was **retired** from the tenant config, `catalog.product_aliases` added,
and the CSV uploaded (221 repos). Verified live: 36 items get catalog-sourced product
suggestions (`reason: catalog: repo "X" is product:Y`), covering products the backfill
never had. The remaining `repo_tags` entries (area repos, a handful of curated products
with body-keyword value, `source:external`) stay as the manual override layer.

## Build stages (CSV first)

Status: **the logic layer (stages 1–4 + the config-driven CSV builder) is BUILT and
unit-tested** (TDD, no deploy). Remaining: the thin transport wrappers + scheduler
tick + a frontend upload — all deploy-exercised, deferred until a rebuild is wanted.

1. ✅ **Core:** `CatalogSource` trait + `CsvCatalog`/`FieldMap` in `poseidon-providers`;
   `CatalogEntity` + `canonical_product_slug` + `repo_product_map` in `poseidon-core`
   (so store/rules don't depend on the integration crate); `PortCatalog` /
   `BackstageCatalog` stubbed behind the trait (return `Unsupported`). Self-contained
   CSV parser (no new dep). Unit-tested.
2. ✅ **Store:** `0006_catalog.sql` (`catalog` table, owner-scoped, repo-keyed) +
   `replace_catalog` (wholesale replace) / `catalog` / `catalog_count`. Unit-tested.
3. ✅ **Sync:** `Service::sync_catalog_from(&dyn CatalogSource)` (testable core) +
   `sync_catalog_csv(csv)` (config-driven CSV) + `catalog()` accessor. Unit-tested.
4. ✅ **Tagging:** the `work_items` merge builds a catalog `repo -> product:*` map once
   (owner-wide, aliases from the default ruleset) and suggests it when the item has no
   product yet - config `repo_tags` OVERRIDE it. Unit-tested (override proven).
5. ◑ **Config + upload:** `CatalogConfig` / `CatalogFieldMap` on `RuleSet` - DONE.
   CSV upload wired end-to-end: `POST /api/catalog/import` (raw CSV → `sync_catalog_csv`)
   + `GET /api/catalog` (rows/count) + a **Settings → Import/Export → "Service catalog"**
   card (file upload, shows current mapped-service count). `CatalogEntity` gained serde
   derives so it serialises.
   **Remaining:** a scheduler tick (periodic re-sync), a Tauri command for the desktop
   path (web works today), and `PortCatalog` filling the stub (the live Port API).
   Note: uploading today populates the `catalog` table but the existing `repo_tags`
   backfill still OVERRIDES it, so the visible effect is the Settings count until the
   `repo_tags` product entries are retired in favour of catalog-driven resolution.

## Interim (shipped)

The one-off import script (`scratchpad/gen_catalog.js` + `emit_block.js`) parses a
Port CSV export and regenerated `repo_tags` for all 57 missing products (+ AVA body
keyword). This is tier A done by hand — it unblocks tagging now and validates the
CSV→taxonomy transform the `CsvCatalog` impl will formalise.
