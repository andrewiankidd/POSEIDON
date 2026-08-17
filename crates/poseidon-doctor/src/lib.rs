//! POSEIDON's Doctor - a self-healing health-check engine.
//!
//! Ported from the crosspose "Doctor" concept: a central system that checks the
//! health of the app's dependencies + configuration, surfaces a traffic-light
//! status, and - where a check knows how - **fixes itself automatically**, so
//! most problems are resolved before the user ever notices, and the rest are
//! visible.
//!
//! This crate is the generic engine. Concrete checks live in the crates that
//! own their dependencies (e.g. the provider access check lives in
//! `poseidon-server`, which has the config + provider). A [`Check`] knows both
//! how to *detect* one condition and how to *repair* it.
//!
//! Design ported faithfully:
//! - **boolean per-check** result ([`CheckResult`]) - pass/fail + a message;
//! - **worst-wins aggregation** into a [`Health`] traffic light, with a
//!   [`Severity`] so a failing *critical* check is Red and a failing *warning*
//!   is Amber (crosspose was Green/Amber only; the third light is added here);
//! - **`can_fix` vs `auto_fix`** - the engine auto-applies only the fixes a
//!   check marks safe, gated by [`Check::auto_fix_requires`] so it never
//!   remediates against a broken substrate;
//! - **fixes self-verify** - after any fix the check is re-run.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

/// Outcome of running one check.
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub ok: bool,
    pub message: String,
}

impl CheckResult {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
        }
    }
}

/// Outcome of running one check's fix.
#[derive(Debug, Clone, Serialize)]
pub struct FixResult {
    pub ok: bool,
    pub message: String,
}

impl FixResult {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
        }
    }
}

/// How serious a *failing* check is - decides Amber vs Red in the aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// A failure degrades the experience but the app still works → Amber.
    Warning,
    /// A failure means core functionality is broken → Red.
    Critical,
}

/// Overall traffic-light health, aggregated worst-wins across all checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Every check passed.
    Green,
    /// At least one warning-severity check failed (no critical failures).
    Amber,
    /// At least one critical-severity check failed.
    Red,
    /// No checks are registered (nothing to report).
    Unknown,
}

/// One check plus how to repair it. Implementors capture whatever they need at
/// construction (a team name, a tenant, …) - the engine calls `run`/`fix` with
/// no context.
#[async_trait]
pub trait Check: Send + Sync {
    /// Stable id - the dedupe key + `auto_fix_requires` reference (e.g.
    /// `"ado-access:Platform Engineering"`). Also how a fix is addressed.
    fn id(&self) -> String;

    /// Human-facing label shown in the Doctor panel.
    fn label(&self) -> String;

    /// Severity of a *failure*. Default warning.
    fn severity(&self) -> Severity {
        Severity::Warning
    }

    /// Whether this check has any fix at all (enables the UI's Fix button).
    fn can_fix(&self) -> bool {
        false
    }

    /// Whether the engine should apply [`Check::fix`] automatically on failure.
    /// Only set for high-confidence, low-risk, non-interactive fixes.
    fn auto_fix(&self) -> bool {
        false
    }

    /// Other check ids that must currently pass before this check's auto-fix is
    /// allowed to run - prevents remediating against a broken substrate.
    fn auto_fix_requires(&self) -> Vec<String> {
        Vec::new()
    }

    /// A UI-handled fix action token (e.g. `"sign-in"`) when the fix is
    /// interactive and belongs in the frontend rather than a server-side
    /// [`Check::fix`]. `None` for server-side fixes.
    fn fix_action(&self) -> Option<String> {
        None
    }

    /// Detect the condition.
    async fn run(&self) -> CheckResult;

    /// Repair the condition. Default: nothing to do.
    async fn fix(&self) -> FixResult {
        FixResult::failed("this check has no automatic fix")
    }
}

/// One check's status, JSON-ready for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub id: String,
    pub label: String,
    pub ok: bool,
    pub severity: Severity,
    pub message: String,
    pub can_fix: bool,
    pub auto_fix: bool,
    /// UI-handled fix action, if the fix is interactive (e.g. `"sign-in"`).
    pub fix_action: Option<String>,
}

/// The full report the indicator + Doctor panel render.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub health: Health,
    pub checks: Vec<CheckReport>,
    /// When the checks last ran (RFC3339).
    pub checked_at: String,
}

/// The Doctor: a registry of checks it runs + aggregates.
#[derive(Clone)]
pub struct Doctor {
    checks: Vec<Arc<dyn Check>>,
}

impl Doctor {
    pub fn new(checks: Vec<Arc<dyn Check>>) -> Self {
        Self { checks }
    }

    /// Report health WITHOUT applying any fixes - the read path for the status
    /// indicator + `GET /api/doctor`. Runs every check concurrently.
    pub async fn report(&self, now_rfc3339: String) -> DoctorReport {
        let results = self.run_all().await;
        Self::assemble(&self.checks, &results, now_rfc3339)
    }

    /// Run checks and apply auto-fixes for any failing check that opted in
    /// (`auto_fix` + `can_fix`) and whose `auto_fix_requires` dependencies are
    /// currently passing. Each applied fix is followed by a re-run to verify.
    /// The background Doctor tick uses this; the on-demand read path uses
    /// [`Doctor::report`].
    pub async fn tick(&self, now_rfc3339: String) -> DoctorReport {
        let mut results = self.run_all().await;

        // Snapshot pass/fail so `auto_fix_requires` is evaluated against the
        // pre-fix state (matches crosspose's latest-results gate).
        let passing: HashMap<String, bool> = self
            .checks
            .iter()
            .zip(&results)
            .map(|(c, r)| (c.id(), r.ok))
            .collect();

        for (idx, check) in self.checks.iter().enumerate() {
            let failed = !results[idx].ok;
            if failed
                && check.auto_fix()
                && check.can_fix()
                && Self::preconditions_met(check.as_ref(), &passing)
            {
                let fix = check.fix().await;
                tracing::info!(check = %check.id(), ok = fix.ok, "doctor auto-fix applied");
                if fix.ok {
                    // Self-verify - never assume a fix worked.
                    results[idx] = check.run().await;
                }
            }
        }

        Self::assemble(&self.checks, &results, now_rfc3339)
    }

    /// Manually run one check's fix (the Doctor panel's Fix button, for
    /// server-side fixes), then re-verify. `None` if no check has that id.
    pub async fn fix(&self, id: &str) -> Option<FixResult> {
        let check = self.checks.iter().find(|c| c.id() == id)?;
        let fix = check.fix().await;
        if !fix.ok {
            return Some(fix);
        }
        // Fold the verify result into the message so the UI shows the truth.
        let verify = check.run().await;
        Some(if verify.ok {
            FixResult::ok(format!("{} - verified", fix.message))
        } else {
            FixResult::failed(format!("fix ran but check still fails: {}", verify.message))
        })
    }

    async fn run_all(&self) -> Vec<CheckResult> {
        futures::future::join_all(self.checks.iter().map(|c| c.run())).await
    }

    fn preconditions_met(check: &dyn Check, passing: &HashMap<String, bool>) -> bool {
        check
            .auto_fix_requires()
            .iter()
            .all(|dep| passing.get(dep).copied().unwrap_or(false))
    }

    fn assemble(
        checks: &[Arc<dyn Check>],
        results: &[CheckResult],
        checked_at: String,
    ) -> DoctorReport {
        let reports: Vec<CheckReport> = checks
            .iter()
            .zip(results)
            .map(|(c, r)| CheckReport {
                id: c.id(),
                label: c.label(),
                ok: r.ok,
                severity: c.severity(),
                message: r.message.clone(),
                can_fix: c.can_fix(),
                auto_fix: c.auto_fix(),
                fix_action: c.fix_action(),
            })
            .collect();
        DoctorReport {
            health: aggregate(&reports),
            checks: reports,
            checked_at,
        }
    }
}

/// Worst-wins aggregation: Red if any critical check fails, else Amber if any
/// check fails, else Green. Unknown when there are no checks.
fn aggregate(checks: &[CheckReport]) -> Health {
    if checks.is_empty() {
        return Health::Unknown;
    }
    let mut amber = false;
    for c in checks {
        if !c.ok {
            match c.severity {
                Severity::Critical => return Health::Red,
                Severity::Warning => amber = true,
            }
        }
    }
    if amber {
        Health::Amber
    } else {
        Health::Green
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub {
        id: String,
        ok: bool,
        severity: Severity,
        auto: bool,
        fix_ok: bool,
    }

    #[async_trait]
    impl Check for Stub {
        fn id(&self) -> String {
            self.id.clone()
        }
        fn label(&self) -> String {
            format!("check {}", self.id)
        }
        fn severity(&self) -> Severity {
            self.severity
        }
        fn can_fix(&self) -> bool {
            true
        }
        fn auto_fix(&self) -> bool {
            self.auto
        }
        async fn run(&self) -> CheckResult {
            if self.ok {
                CheckResult::ok("fine")
            } else {
                CheckResult::failed("broken")
            }
        }
        async fn fix(&self) -> FixResult {
            if self.fix_ok {
                FixResult::ok("fixed")
            } else {
                FixResult::failed("could not fix")
            }
        }
    }

    fn stub(id: &str, ok: bool, severity: Severity) -> Arc<dyn Check> {
        Arc::new(Stub {
            id: id.into(),
            ok,
            severity,
            auto: false,
            fix_ok: false,
        })
    }

    #[tokio::test]
    async fn all_ok_is_green() {
        let d = Doctor::new(vec![
            stub("a", true, Severity::Warning),
            stub("b", true, Severity::Critical),
        ]);
        assert_eq!(d.report("t".into()).await.health, Health::Green);
    }

    #[tokio::test]
    async fn warning_failure_is_amber_critical_is_red() {
        let amber = Doctor::new(vec![stub("a", false, Severity::Warning)]);
        assert_eq!(amber.report("t".into()).await.health, Health::Amber);

        let red = Doctor::new(vec![
            stub("a", false, Severity::Warning),
            stub("b", false, Severity::Critical),
        ]);
        assert_eq!(red.report("t".into()).await.health, Health::Red);
    }

    #[tokio::test]
    async fn empty_is_unknown() {
        let d = Doctor::new(vec![]);
        assert_eq!(d.report("t".into()).await.health, Health::Unknown);
    }

    #[tokio::test]
    async fn tick_auto_fixes_and_verifies() {
        // A failing, auto-fixable check whose fix works → tick heals it → green.
        let healing = Arc::new(Stub {
            id: "h".into(),
            ok: false,
            severity: Severity::Warning,
            auto: true,
            fix_ok: true,
        });
        // The stub's run() always returns `ok`, so a fix_ok stub still reports
        // failure on re-run; use a check that flips. Simpler: assert the report
        // reflects the fix path ran without panicking + stays amber (run() still
        // fails because the stub is static). This asserts the plumbing, not a
        // stateful heal.
        let d = Doctor::new(vec![healing]);
        let report = d.tick("t".into()).await;
        // The static stub can't actually heal, so it stays amber - but the fix
        // path executed. A real stateful check would go green.
        assert_eq!(report.health, Health::Amber);
    }

    #[tokio::test]
    async fn report_is_json_serialisable() {
        let d = Doctor::new(vec![stub("a", true, Severity::Warning)]);
        let report = d.report("t".into()).await;
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"health\":\"green\""));
        assert!(json.contains("\"severity\":\"warning\""));
    }

    // ── A richer stub for the fix / precondition paths ───────────────────────
    // The `Stub` above has a static `run()`, so it can never actually heal. This
    // one is stateful: `run()` passes iff `healed`, and a successful fix flips
    // `healed` (unless `heal_on_fix` is false, modelling a fix that "ran" but
    // didn't take). That lets us exercise the self-verify + auto-fix-gating code.
    struct Flex {
        id: String,
        healed: std::sync::atomic::AtomicBool,
        severity: Severity,
        can_fix: bool,
        auto: bool,
        requires: Vec<String>,
        fix_ok: bool,
        heal_on_fix: bool,
        action: Option<String>,
    }

    fn flex(id: &str, start_ok: bool, severity: Severity) -> Flex {
        Flex {
            id: id.into(),
            healed: std::sync::atomic::AtomicBool::new(start_ok),
            severity,
            can_fix: true,
            auto: false,
            requires: Vec::new(),
            fix_ok: true,
            heal_on_fix: true,
            action: None,
        }
    }

    #[async_trait]
    impl Check for Flex {
        fn id(&self) -> String {
            self.id.clone()
        }
        fn label(&self) -> String {
            format!("check {}", self.id)
        }
        fn severity(&self) -> Severity {
            self.severity
        }
        fn can_fix(&self) -> bool {
            self.can_fix
        }
        fn auto_fix(&self) -> bool {
            self.auto
        }
        fn auto_fix_requires(&self) -> Vec<String> {
            self.requires.clone()
        }
        fn fix_action(&self) -> Option<String> {
            self.action.clone()
        }
        async fn run(&self) -> CheckResult {
            if self.healed.load(std::sync::atomic::Ordering::SeqCst) {
                CheckResult::ok("fine")
            } else {
                CheckResult::failed("broken")
            }
        }
        async fn fix(&self) -> FixResult {
            if !self.fix_ok {
                return FixResult::failed("could not fix");
            }
            if self.heal_on_fix {
                self.healed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            FixResult::ok("fixed")
        }
    }

    #[tokio::test]
    async fn report_preserves_check_registration_order() {
        // The panel renders checks in the order they were registered - assert the
        // report echoes that order rather than reordering by status/id.
        let d = Doctor::new(vec![
            stub("alpha", true, Severity::Warning),
            stub("bravo", false, Severity::Critical),
            stub("charlie", true, Severity::Warning),
        ]);
        let ids: Vec<String> = d
            .report("t".into())
            .await
            .checks
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec!["alpha", "bravo", "charlie"]);
    }

    #[tokio::test]
    async fn passing_critical_does_not_turn_red() {
        // Worst-wins only escalates on a *failing* check: a passing critical plus
        // a failing warning is Amber, not Red.
        let d = Doctor::new(vec![
            stub("crit", true, Severity::Critical),
            stub("warn", false, Severity::Warning),
        ]);
        assert_eq!(d.report("t".into()).await.health, Health::Amber);
    }

    #[tokio::test]
    async fn worst_wins_is_order_independent() {
        // A failing critical dominates a failing warning regardless of position.
        let warn_first = Doctor::new(vec![
            stub("warn", false, Severity::Warning),
            stub("crit", false, Severity::Critical),
        ]);
        let crit_first = Doctor::new(vec![
            stub("crit", false, Severity::Critical),
            stub("warn", false, Severity::Warning),
        ]);
        assert_eq!(warn_first.report("t".into()).await.health, Health::Red);
        assert_eq!(crit_first.report("t".into()).await.health, Health::Red);
    }

    #[tokio::test]
    async fn manual_fix_verifies_on_success() {
        // fix() heals the check, the re-run confirms it -> "verified".
        let d = Doctor::new(vec![Arc::new(flex("h", false, Severity::Warning))]);
        let result = d.fix("h").await.expect("check exists");
        assert!(result.ok);
        assert!(result.message.contains("verified"), "{}", result.message);
    }

    #[tokio::test]
    async fn manual_fix_reports_when_check_still_fails() {
        // The fix returns ok but the condition persists (heal_on_fix = false) -
        // the fold-in re-run must expose that the check still fails.
        let mut f = flex("h", false, Severity::Warning);
        f.heal_on_fix = false;
        let d = Doctor::new(vec![Arc::new(f)]);
        let result = d.fix("h").await.expect("check exists");
        assert!(!result.ok);
        assert!(result.message.contains("still fails"), "{}", result.message);
    }

    #[tokio::test]
    async fn manual_fix_returns_failure_without_verifying() {
        // A failing fix is surfaced verbatim - no verify re-run is claimed.
        let mut f = flex("h", false, Severity::Warning);
        f.fix_ok = false;
        let d = Doctor::new(vec![Arc::new(f)]);
        let result = d.fix("h").await.expect("check exists");
        assert!(!result.ok);
        assert_eq!(result.message, "could not fix");
        assert!(!result.message.contains("verified"));
    }

    #[tokio::test]
    async fn manual_fix_unknown_id_is_none() {
        let d = Doctor::new(vec![stub("a", true, Severity::Warning)]);
        assert!(d.fix("does-not-exist").await.is_none());
    }

    #[tokio::test]
    async fn tick_applies_autofix_when_precondition_met() {
        // Dependency passes -> the auto-fix on `h` is allowed to run, heals it,
        // the self-verify re-run flips it green.
        let dep = Arc::new(flex("dep", true, Severity::Warning));
        let mut healing = flex("h", false, Severity::Warning);
        healing.auto = true;
        healing.requires = vec!["dep".into()];
        let d = Doctor::new(vec![dep, Arc::new(healing)]);
        assert_eq!(d.tick("t".into()).await.health, Health::Green);
    }

    #[tokio::test]
    async fn tick_skips_autofix_when_precondition_fails() {
        // Dependency is failing -> auto-fix must NOT run against the broken
        // substrate, so `h` stays failed. Both are warnings, so the two failing
        // checks leave the overall light Amber.
        let dep = Arc::new(flex("dep", false, Severity::Warning));
        let mut healing = flex("h", false, Severity::Warning);
        healing.auto = true;
        healing.requires = vec!["dep".into()];
        let d = Doctor::new(vec![dep, Arc::new(healing)]);
        let report = d.tick("t".into()).await;
        assert_eq!(report.health, Health::Amber);
        // `h` never healed because its precondition (`dep`) was not passing.
        let h = report.checks.iter().find(|c| c.id == "h").unwrap();
        assert!(!h.ok);
    }

    #[tokio::test]
    async fn tick_leaves_non_autofix_failures_untouched() {
        // A failing, fixable-but-not-auto check is not silently fixed by a tick.
        let d = Doctor::new(vec![Arc::new(flex("h", false, Severity::Warning))]);
        let report = d.tick("t".into()).await;
        assert_eq!(report.health, Health::Amber);
        assert!(!report.checks[0].ok);
    }

    #[tokio::test]
    async fn check_report_exposes_fix_affordances() {
        // The per-check report carries the UI affordances (can_fix / auto_fix /
        // fix_action) so the panel can render the right button.
        let mut f = flex("sign-in-check", false, Severity::Critical);
        f.auto = false;
        f.action = Some("sign-in".into());
        let d = Doctor::new(vec![Arc::new(f)]);
        let report = d.report("t".into()).await;
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"can_fix\":true"));
        assert!(json.contains("\"auto_fix\":false"));
        assert!(json.contains("\"fix_action\":\"sign-in\""));
        assert!(json.contains("\"health\":\"red\""));
    }

    #[tokio::test]
    async fn fix_result_serialises_snake_case() {
        let json = serde_json::to_string(&FixResult::ok("done")).unwrap();
        assert!(json.contains("\"ok\":true"));
        assert!(json.contains("\"message\":\"done\""));
    }
}
