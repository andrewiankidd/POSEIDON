//! POSEIDEN CLI - `poseiden`.
//!
//! Drives the exact same [`poseiden_server::Service`] the GUI and web instance
//! use, over the same local SQLite store. Three subcommands:
//!
//! - `poll` - poll every configured project once and report what landed.
//! - `lint` - evaluate backlog hygiene and print the flags. Exits non-zero when
//!   any error-severity flag is present, so a CI stage can gate a pipeline on
//!   "no untagged / stale / disallowed items".
//! - `report` - print the work-item + pipeline flow report for a date range.
//! - `tag` - generate + store AI tag suggestions (advisory; reviewed/applied in
//!   the GUI), optionally scoped to one `--assignee`. Build `--features cuda` to
//!   run the embedded model on an NVIDIA GPU - the headless GPU tagging path.
//!
//! It needs only a token with read scopes, so it's safe to run anywhere the PAT
//! env var is set - including CI, where `poseiden lint` turns backlog hygiene
//! into a check.

mod banner;

use std::sync::Arc;

use anyhow::Context;
use chrono::{Duration, Utc};
use clap::{Parser, Subcommand};
use poseiden_core::Severity;
use poseiden_paths::Paths;
use poseiden_server::{Service, SharedService};
use poseiden_store::Store;

#[derive(Parser)]
#[command(
    name = "poseiden",
    version,
    about = "POSEIDEN - a Product Owner support tool for backlog hygiene + flow."
)]
struct Cli {
    /// Emit machine-readable JSON instead of human text.
    #[arg(long, global = true)]
    json: bool,

    /// Scope `lint` / `report` to a single team (its configured `name`). Omit
    /// for all teams. `poll` always polls every team regardless.
    #[arg(long, global = true)]
    team: Option<String>,

    /// Run as this tenant (owner). Multi-tenant instances store each user's
    /// teams/rules/items under their owner key (their auth email). Omit for the
    /// single-tenant `default` owner. Mirrors `config import --owner`.
    #[arg(long, global = true)]
    owner: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Poll every configured project once and report what was fetched.
    Poll,
    /// Evaluate backlog hygiene and print flags. Exits 1 if any error-severity
    /// flag is found (for CI gating). Polls fresh first unless `--no-poll`.
    Lint {
        /// Evaluate the already-stored items without polling first.
        #[arg(long)]
        no_poll: bool,
    },
    /// Print the work-item + pipeline flow report for a date range.
    Report {
        /// Range start, `YYYY-MM-DD`. Defaults to 30 days ago.
        #[arg(long)]
        from: Option<String>,
        /// Range end, `YYYY-MM-DD`. Defaults to today.
        #[arg(long)]
        to: Option<String>,
        /// Poll fresh before reporting (otherwise uses stored data).
        #[arg(long)]
        poll: bool,
    },
    /// Export or import configuration (teams, rules, reports) as portable YAML.
    /// The import path is how a headless / CI run gets its config without the UI.
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Generate AI tag suggestions over the backlog and store them (advisory -
    /// never applied; review + apply in the GUI). Uses the owner's configured AI
    /// backend: the embedded model (add `--features cuda` to run it on an NVIDIA
    /// GPU) or a cloud provider. Polls fresh first unless `--no-poll`.
    Tag {
        /// Only tag items assigned to this person (case-insensitive substring of
        /// the display name, e.g. "Andrew Kidd"). Omit to tag the whole scope.
        #[arg(long)]
        assignee: Option<String>,
        /// Tag the already-stored items without polling first.
        #[arg(long)]
        no_poll: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Write the current configuration as YAML to a file, or stdout with no `--out`.
    Export {
        /// Destination file. Omit to print to stdout.
        #[arg(long)]
        out: Option<String>,
    },
    /// Load a YAML config document. Merges by default (adds teams/reports not
    /// already present); `--replace` overwrites the whole configuration.
    Import {
        /// Path to the YAML file, or `-` to read from stdin.
        file: String,
        /// Overwrite the entire configuration instead of merging.
        #[arg(long)]
        replace: bool,
        /// Import under this owner (trusted). Overrides the bundle's own `owner`;
        /// absent, the bundle's `owner` is used, else the default tenant.
        #[arg(long)]
        owner: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Greet with the emblem + a motto BEFORE clap parses, so it shows even on a
    // bare `poseiden`, `--help`, or a usage error (clap exits during parse for
    // those, before any of main's body runs). Pre-scan argv for `--json` so
    // machine mode stays clean; the banner goes to stderr regardless.
    if !std::env::args().any(|a| a == "--json") {
        banner::print();
    }
    let cli = Cli::parse();
    // Telemetry is set up inside build_service (it needs the loaded config).
    // Logs go to stderr so `--json` output on stdout stays machine-parseable.
    let (service, _telemetry) = build_service().await?;

    // Re-scope the whole run to a tenant when asked (multi-tenant instances key
    // every row by owner). `config import` still honours its own `--owner`, which
    // overrides this on top of the re-scoped service.
    let owner = cli.owner.as_deref().filter(|s| !s.is_empty());
    let service: SharedService = match owner {
        Some(o) => Arc::new(service.with_owner(o)),
        None => service,
    };

    let team = cli.team.as_deref().filter(|s| !s.is_empty());
    match cli.command {
        Command::Poll => cmd_poll(&service, cli.json).await,
        Command::Lint { no_poll } => cmd_lint(&service, cli.json, no_poll, team).await,
        Command::Report { from, to, poll } => {
            cmd_report(&service, cli.json, from, to, poll, team).await
        }
        Command::Config { action } => cmd_config(&service, action).await,
        Command::Tag { assignee, no_poll } => {
            cmd_tag(&service, cli.json, no_poll, team, assignee.as_deref()).await
        }
    }
}

/// Wire up the same Service the server uses, over the local store.
async fn build_service(
) -> anyhow::Result<(SharedService, Option<poseiden_telemetry::TelemetryGuard>)> {
    let paths = Paths::resolve();
    paths.ensure_dirs().context("creating data directory")?;
    // Instance config from env; per-owner config (teams/rules/reports) from the DB.
    let config = poseiden_server::load_config();
    // Bring telemetry up before any work; stderr console keeps stdout clean.
    let telemetry =
        poseiden_server::init_telemetry(&config.telemetry, "poseiden-cli", paths.log_dir(), true);
    let store = Store::connect(&paths.database_path())
        .await
        .context("opening database")?;
    Ok((
        Arc::new(Service::new(config, store, paths.az_sessions_dir())),
        telemetry,
    ))
}

async fn cmd_poll(service: &Service, json: bool) -> anyhow::Result<()> {
    let outcome = service.poll_once().await;
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!(
            "Polled {} team(s): {} work items, {} pipelines, {} runs.",
            outcome.teams_polled, outcome.work_items, outcome.pipelines, outcome.runs
        );
        for e in &outcome.errors {
            eprintln!("  ! {e}");
        }
    }
    Ok(())
}

async fn cmd_lint(
    service: &Service,
    json: bool,
    no_poll: bool,
    team: Option<&str>,
) -> anyhow::Result<()> {
    if !no_poll {
        service.poll_once().await;
    }
    let flags = service.flags(team).await?;
    let error_count = flags
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();

    if json {
        println!("{}", serde_json::to_string_pretty(&flags)?);
    } else if flags.is_empty() {
        println!("✓ No hygiene flags - backlog is clean.");
    } else {
        println!("{} hygiene flag(s):", flags.len());
        for f in &flags {
            let sev = match f.severity {
                Severity::Error => "ERROR",
                Severity::Warn => "warn",
            };
            println!("  [{sev}] #{} ({}) - {}", f.work_item_id, f.team, f.message);
        }
        println!(
            "\n{error_count} error(s), {} warning(s).",
            flags.len() - error_count
        );
    }

    // Non-zero exit on any error-severity flag so CI can gate on it.
    if error_count > 0 {
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_report(
    service: &Service,
    json: bool,
    from: Option<String>,
    to: Option<String>,
    poll: bool,
    team: Option<&str>,
) -> anyhow::Result<()> {
    if poll {
        service.poll_once().await;
    }
    let now = Utc::now();
    let from = from.unwrap_or_else(|| (now - Duration::days(30)).format("%Y-%m-%d").to_string());
    let to = to.unwrap_or_else(|| now.format("%Y-%m-%d").to_string());
    let from_ts = format!("{from}T00:00:00Z");
    let to_ts = format!("{to}T23:59:59Z");

    let work_items = service.ticket_report(&from_ts, &to_ts, team).await?;
    let pipelines = service.pipeline_report(&from_ts, &to_ts, team).await?;

    if json {
        let combined = serde_json::json!({ "work_items": work_items, "pipelines": pipelines });
        println!("{}", serde_json::to_string_pretty(&combined)?);
    } else {
        println!("Report {from} → {to}");
        println!("\nWork items:");
        println!("  opened: {}", work_items.opened);
        println!("  closed: {}", work_items.closed);
        if !work_items.closed_by_tag.is_empty() {
            println!("  closed by tag:");
            for tc in &work_items.closed_by_tag {
                println!("    {:<28} {}", tc.tag, tc.count);
            }
        }
        println!("\nPipelines:");
        println!("  runs: {}", pipelines.total_runs);
        println!(
            "  succeeded / failed / canceled: {} / {} / {}",
            pipelines.succeeded, pipelines.failed, pipelines.canceled
        );
        match pipelines.success_rate {
            Some(rate) => println!("  success rate: {:.1}%", rate * 100.0),
            None => println!("  success rate: n/a (no completed runs)"),
        }
    }
    Ok(())
}

/// Generate + store AI tag suggestions, optionally only for one assignee. The
/// suggestions are advisory (stored, never applied) - the GUI is where a person
/// reviews and applies them, keeping the "never auto-apply" contract. The model
/// runs on the GPU when this binary was built `--features cuda` on a CUDA host.
async fn cmd_tag(
    service: &Service,
    json: bool,
    no_poll: bool,
    team: Option<&str>,
    assignee: Option<&str>,
) -> anyhow::Result<()> {
    if !no_poll {
        service.poll_once().await;
    }

    // Resolve the assignee filter to a concrete id set (None = the whole scope,
    // which the tagger reads as "every item this team owns").
    let ids: Option<Vec<i64>> = match assignee {
        Some(name) => {
            let needle = name.to_lowercase();
            let ids: Vec<i64> = service
                .work_items(team)
                .await?
                .iter()
                .filter(|it| {
                    it.assigned_to
                        .as_deref()
                        .is_some_and(|a| a.to_lowercase().contains(&needle))
                })
                .map(|it| it.id)
                .collect();
            if ids.is_empty() {
                if json {
                    println!("{}", serde_json::json!({ "considered": 0, "matched": 0 }));
                } else {
                    eprintln!("No work items assigned to \"{name}\" in this scope.");
                }
                return Ok(());
            }
            Some(ids)
        }
        None => None,
    };

    if !json {
        match (&ids, assignee) {
            (Some(v), Some(name)) => eprintln!("Tagging {} item(s) assigned to \"{name}\"...", v.len()),
            _ => eprintln!("Tagging the whole scope..."),
        }
    }

    let summary = service
        .generate_tag_suggestions_with(team, ids.as_deref(), |done, total, sugg| {
            eprint!("\r  {done}/{total} items, {sugg} suggestions");
        })
        .await?;
    eprintln!(); // terminate the progress line

    if json {
        // Re-read so the JSON carries the actual stored suggestions per item.
        let mut items = service.work_items(team).await?;
        if let Some(want) = ids.as_ref() {
            let set: std::collections::HashSet<i64> = want.iter().copied().collect();
            items.retain(|it| set.contains(&it.id));
        }
        let out: Vec<_> = items
            .iter()
            .filter(|it| !it.tag_suggestions.is_empty())
            .map(|it| {
                serde_json::json!({
                    "id": it.id,
                    "title": it.title,
                    "assigned_to": it.assigned_to,
                    "current_tags": it.tags,
                    "suggestions": it.tag_suggestions.iter().map(|s| &s.tag).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "considered": summary.considered,
                "with_suggestions": summary.with_suggestions,
                "suggestions": summary.suggestions,
                "items": out,
            }))?
        );
    } else {
        println!(
            "Tagged {} item(s): {} got suggestions ({} total). Review + apply in the app.",
            summary.considered, summary.with_suggestions, summary.suggestions
        );
    }
    Ok(())
}

async fn cmd_config(service: &Service, action: ConfigCmd) -> anyhow::Result<()> {
    match action {
        ConfigCmd::Export { out } => {
            let yaml = service.export_config().await?;
            match out {
                Some(path) => {
                    std::fs::write(&path, yaml).with_context(|| format!("writing {path}"))?;
                    eprintln!("Exported configuration to {path}");
                }
                None => print!("{yaml}"),
            }
        }
        ConfigCmd::Import {
            file,
            replace,
            owner,
        } => {
            let yaml = if file == "-" {
                use std::io::Read;
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string(&mut s)
                    .context("reading config from stdin")?;
                s
            } else {
                std::fs::read_to_string(&file).with_context(|| format!("reading {file}"))?
            };
            let summary = service
                .import_config_trusted(&yaml, replace, owner.as_deref())
                .await?;
            eprintln!(
                "Imported {} team(s) and {} report(s) ({}).",
                summary.teams,
                summary.reports,
                if summary.replaced { "replaced" } else { "merged" }
            );
        }
    }
    Ok(())
}
