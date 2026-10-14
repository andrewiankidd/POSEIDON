//! The Capacity report: a pure function from the stored work items, pull requests and commit
//! activity to a [`CapacityReport`]. No IO and no clock (`now` is injected), so every number is
//! reproducible and testable.
//!
//! The design rule is that it must work from data a team already produces: creation / done dates,
//! assignee, pull requests and commit timestamps. Tags and AI verdicts only *enrich* it, and
//! anything they can't classify is shown as "unknown" rather than guessed or dropped.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Datelike, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use poseidon_core::{
    identity_tokens, CapacityReport, CommitActivity, ConcentrationSummary, CycleTime, DemandClass,
    DemandMix, DemandVerdictRecord, EffortSummary, FlowSummary, PersonEffort, PersonProfile,
    PersonWip, PrStatus, PullRequest, RepoConcentration, SourceCount, StandingItem, WeekFlow,
    WipSummary, WorkItem,
};

use crate::{is_member, is_service_identity, roster_sets};

/// Everything the report is computed from.
pub struct CapacityInput<'a> {
    pub work_items: &'a [WorkItem],
    pub pull_requests: &'a [PullRequest],
    pub commits: &'a [CommitActivity],
    /// AI verdicts for items without a `source:` tag, by work item id.
    pub demand: &'a HashMap<i64, DemandVerdictRecord>,
    /// Team name (lowercase) -> identity tokens of the people on that team (see `Datasets`).
    pub rosters: &'a HashMap<String, Vec<String>>,
    pub now: DateTime<Utc>,
    /// Window length in weeks (clamped to 4-52).
    pub weeks: u32,
    /// Minutes east of UTC for the viewer's clock, so "after hours" means THEIR evening.
    pub tz_offset_minutes: i32,
    /// Per-person timezone and leave. A person with a timezone is read in their own day, whatever
    /// the viewer's clock says; everyone else falls back to `tz_offset_minutes`.
    pub people: &'a [PersonProfile],
}

/// Whether `name` is an IANA timezone this build knows (`Europe/London`).
pub fn timezone_is_valid(name: &str) -> bool {
    name.parse::<Tz>().is_ok()
}

/// Work-item types that are containers or sub-steps, not units of demand.
const NOT_FLOW_TYPES: [&str; 3] = ["task", "epic", "feature"];
/// Phrases that mark an item as a running list rather than a deliverable.
const STANDING_PHRASES: [&str; 8] = [
    "running list",
    "standing item",
    "ongoing",
    "catch-all",
    "catch all",
    "parking lot",
    "grab bag",
    "miscellaneous",
];
/// `source:` values that are demand arriving from outside the team's own plan.
const REACTIVE_SOURCES: [&str; 5] = ["support", "request", "incident", "sre", "external"];
/// `source:` values that are the team's committed plan.
const PLANNED_SOURCES: [&str; 2] = ["roadmap", "fpp"];
/// States that count as in progress for work-in-progress purposes.
const ACTIVE_STATES: [&str; 6] = [
    "active",
    "in progress",
    "committed",
    "doing",
    "started",
    "in review",
];
const MAX_PEOPLE: usize = 12;

fn is_flow_type(t: &str) -> bool {
    !NOT_FLOW_TYPES.iter().any(|n| t.eq_ignore_ascii_case(n))
}

/// Finished as in delivered (a "Removed" item was dropped, not done).
fn is_done(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_lowercase().as_str(),
        "closed" | "resolved" | "done" | "completed"
    )
}

fn is_open(state: &str) -> bool {
    !is_done(state) && !state.trim().eq_ignore_ascii_case("removed")
}

fn is_active(state: &str) -> bool {
    let s = state.trim().to_ascii_lowercase();
    ACTIVE_STATES.iter().any(|a| *a == s)
}

fn monday_of(d: NaiveDate) -> NaiveDate {
    d - Duration::days(i64::from(d.weekday().num_days_from_monday()))
}

/// Nearest-rank percentile of an ascending slice.
fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

fn median_f(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    }
}

fn title_hits_standing(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    STANDING_PHRASES.iter().any(|p| t.contains(p))
}

/// The `source:` slugs on an item, lowercased.
fn sources_of(it: &WorkItem) -> Vec<String> {
    it.tags
        .iter()
        .filter_map(|t| {
            t.trim()
                .to_ascii_lowercase()
                .strip_prefix("source:")
                .map(str::to_string)
        })
        .collect()
}

/// Why an item is a standing (running-list) item, if it is.
fn standing_reason(
    it: &WorkItem,
    demand: &HashMap<i64, DemandVerdictRecord>,
    now: DateTime<Utc>,
) -> Option<&'static str> {
    if !is_open(&it.state) {
        return None;
    }
    if demand
        .get(&it.id)
        .is_some_and(|v| v.class == DemandClass::Standing)
    {
        return Some("ai");
    }
    let age = (now - it.created_at).num_days();
    (title_hits_standing(&it.title) && age > 30).then_some("title")
}

/// How an arriving item is classified: by its tags first, then by an AI verdict.
enum Classified {
    Tagged(DemandClass, Vec<String>),
    Inferred(DemandClass, Option<String>),
    Unknown,
}

fn classify(it: &WorkItem, demand: &HashMap<i64, DemandVerdictRecord>) -> Classified {
    let sources = sources_of(it);
    let reactive = sources
        .iter()
        .any(|s| REACTIVE_SOURCES.contains(&s.as_str()));
    let planned = sources
        .iter()
        .any(|s| PLANNED_SOURCES.contains(&s.as_str()));
    if reactive {
        return Classified::Tagged(DemandClass::Reactive, sources);
    }
    if planned {
        return Classified::Tagged(DemandClass::Planned, sources);
    }
    let debt = it.tags.iter().any(|t| {
        let t = t.trim().to_ascii_lowercase();
        t == "initiative:technical-debt" || t == "technical debt"
    });
    if debt {
        return Classified::Tagged(DemandClass::Maintenance, vec!["technical-debt".into()]);
    }
    match demand.get(&it.id) {
        // a standing verdict is handled before classification; treat the rest as demand
        Some(v) if v.class != DemandClass::Standing => {
            Classified::Inferred(v.class, v.source.clone())
        }
        _ => Classified::Unknown,
    }
}

/// A stable hash of a title (FNV-1a, 64-bit hex), so an AI verdict can be tied to the title it
/// was made from and redone when the item is retitled. Stable across builds, unlike `DefaultHasher`.
pub fn demand_title_hash(title: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in title.trim().to_lowercase().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Items worth sending to the AI for a verdict: new arrivals with no tag-based class, plus old open
/// items that keep getting touched (the shape of a running list) - minus anything already judged
/// from the same title. Arrivals first (newest first), then the old-but-active ones.
pub fn demand_candidates<'a>(
    items: &'a [WorkItem],
    known: &HashMap<i64, String>,
    now: DateTime<Utc>,
    weeks: u32,
) -> Vec<&'a WorkItem> {
    let window_start = now - Duration::days(i64::from(weeks.clamp(4, 52)) * 7);
    let mut arrivals: Vec<&WorkItem> = Vec::new();
    let mut touched: Vec<&WorkItem> = Vec::new();
    for it in items.iter().filter(|i| is_flow_type(&i.work_item_type)) {
        if known
            .get(&it.id)
            .is_some_and(|h| *h == demand_title_hash(&it.title))
        {
            continue; // already judged from this title
        }
        if it.created_at >= window_start {
            if matches!(classify(it, &HashMap::new()), Classified::Unknown) {
                arrivals.push(it);
            }
        } else if is_open(&it.state)
            && (now - it.created_at).num_days() > 60
            && (now - it.changed_at).num_days() <= 30
        {
            touched.push(it);
        }
    }
    arrivals.sort_by_key(|i| std::cmp::Reverse(i.created_at));
    touched.sort_by_key(|i| std::cmp::Reverse(i.changed_at));
    arrivals.extend(touched);
    arrivals
}

/// Compute the report.
pub fn capacity(input: &CapacityInput) -> CapacityReport {
    let weeks = input.weeks.clamp(4, 52);
    let now = input.now;
    let end = now.date_naive();
    let first_monday = monday_of(end) - Duration::days(7 * i64::from(weeks - 1));
    let window_start: DateTime<Utc> = first_monday
        .and_hms_opt(0, 0, 0)
        .expect("midnight is valid")
        .and_utc();
    let in_window = |t: DateTime<Utc>| t >= window_start && t <= now;
    let week_index = |t: DateTime<Utc>| -> usize {
        ((monday_of(t.date_naive()) - first_monday).num_days() / 7).clamp(0, i64::from(weeks) - 1)
            as usize
    };

    // Standing items are tracked separately and kept out of every flow number.
    let mut standing: Vec<StandingItem> = Vec::new();
    let mut flow_items: Vec<&WorkItem> = Vec::new();
    for it in input
        .work_items
        .iter()
        .filter(|i| is_flow_type(&i.work_item_type))
    {
        match standing_reason(it, input.demand, now) {
            Some(reason) => standing.push(StandingItem {
                id: it.id,
                title: it.title.clone(),
                url: it.url.clone(),
                age_days: (now - it.created_at).num_days(),
                reason: reason.to_string(),
            }),
            None => flow_items.push(it),
        }
    }
    standing.sort_by_key(|s| std::cmp::Reverse(s.age_days));
    standing.truncate(25);

    // ── Weekly flow ──
    let mut weekly: Vec<WeekFlow> = (0..weeks)
        .map(|i| WeekFlow {
            week_start: (first_monday + Duration::days(7 * i64::from(i)))
                .format("%Y-%m-%d")
                .to_string(),
            arrived: 0,
            closed: 0,
            bulk: false,
        })
        .collect();
    let mut cycle_days: Vec<f64> = Vec::new();
    for it in &flow_items {
        if in_window(it.created_at) {
            weekly[week_index(it.created_at)].arrived += 1;
        }
        if is_done(&it.state) {
            if let Some(closed) = it.closed_at.filter(|c| in_window(*c)) {
                weekly[week_index(closed)].closed += 1;
                let days = (closed - it.created_at).num_seconds().max(0) as f64 / 86_400.0;
                cycle_days.push(days);
            }
        }
    }
    let mut closed_per_week: Vec<f64> = weekly.iter().map(|w| f64::from(w.closed)).collect();
    let median_closed = median_f(&mut closed_per_week);
    // A clean-up week closes far more than the team's normal week; don't read it as capacity.
    let bulk_floor = (median_closed * 3.0).max(8.0);
    for w in &mut weekly {
        w.bulk = f64::from(w.closed) >= bulk_floor;
    }
    let mut normal: Vec<f64> = weekly
        .iter()
        .filter(|w| !w.bulk)
        .map(|w| f64::from(w.closed))
        .collect();
    let typical = if normal.is_empty() {
        median_closed
    } else {
        median_f(&mut normal)
    };
    let arrived: u32 = weekly.iter().map(|w| w.arrived).sum();
    let closed: u32 = weekly.iter().map(|w| w.closed).sum();
    let bulk_weeks = weekly.iter().filter(|w| w.bulk).count() as u32;
    let bulk_closed = weekly
        .iter()
        .filter(|w| w.bulk)
        .map(|w| (f64::from(w.closed) - typical).max(0.0))
        .sum::<f64>()
        .round() as u32;
    let open_backlog = flow_items.iter().filter(|i| is_open(&i.state)).count() as u32;
    let flow = FlowSummary {
        arrived,
        closed,
        ratio: (closed > 0).then(|| f64::from(arrived) / f64::from(closed)),
        backlog_growth_per_week: (f64::from(arrived) - f64::from(closed)) / f64::from(weeks),
        typical_weekly_throughput: typical,
        bulk_weeks,
        bulk_closed,
        open_backlog,
        weeks_to_clear: (typical > 0.0).then(|| f64::from(open_backlog) / typical),
    };

    // ── Cycle time ──
    cycle_days.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let cycle = CycleTime {
        n: cycle_days.len() as u32,
        median_days: percentile(&cycle_days, 0.5),
        p85_days: percentile(&cycle_days, 0.85),
        p95_days: percentile(&cycle_days, 0.95),
    };

    // ── Work in progress ──
    let mut wip = WipSummary::default();
    let mut people: BTreeMap<String, PersonWip> = BTreeMap::new();
    for it in flow_items.iter().filter(|i| is_open(&i.state)) {
        let in_progress = is_active(&it.state);
        let idle = (now - it.changed_at).num_days();
        if in_progress {
            wip.in_progress += 1;
            if idle >= 30 {
                wip.stalled_30 += 1;
            }
            if idle >= 90 {
                wip.stalled_90 += 1;
            }
        }
        match it.assigned_to.as_deref().filter(|a| !a.trim().is_empty()) {
            None => wip.unassigned_open += 1,
            Some(name) => {
                let p = people.entry(name.to_string()).or_insert_with(|| PersonWip {
                    name: name.to_string(),
                    in_progress: 0,
                    open_assigned: 0,
                });
                p.open_assigned += 1;
                if in_progress {
                    p.in_progress += 1;
                }
            }
        }
    }
    wip.per_person = people.into_values().collect();
    wip.per_person.sort_by(|a, b| {
        b.in_progress
            .cmp(&a.in_progress)
            .then(b.open_assigned.cmp(&a.open_assigned))
    });
    wip.per_person.truncate(MAX_PEOPLE);

    // ── Demand mix (what arrived in the window) ──
    let mut demand = DemandMix::default();
    let mut by_source: BTreeMap<String, SourceCount> = BTreeMap::new();
    for it in flow_items.iter().filter(|i| in_window(i.created_at)) {
        demand.total += 1;
        let mut bump = |slug: &str, inferred: bool| {
            let e = by_source
                .entry(slug.to_string())
                .or_insert_with(|| SourceCount {
                    source: slug.to_string(),
                    count: 0,
                    inferred: 0,
                });
            e.count += 1;
            if inferred {
                e.inferred += 1;
            }
        };
        let class = match classify(it, input.demand) {
            Classified::Tagged(class, slugs) => {
                demand.tagged += 1;
                for s in slugs {
                    bump(&s, false);
                }
                class
            }
            Classified::Inferred(class, source) => {
                demand.inferred += 1;
                bump(source.as_deref().unwrap_or(class.as_str()), true);
                class
            }
            Classified::Unknown => {
                demand.unknown += 1;
                bump("(unclassified)", false);
                continue;
            }
        };
        match class {
            DemandClass::Reactive => demand.reactive += 1,
            DemandClass::Planned => demand.planned += 1,
            DemandClass::Maintenance => demand.maintenance += 1,
            DemandClass::Standing => {}
        }
    }
    let classified = demand.total - demand.unknown;
    demand.reactive_share =
        (classified > 0).then(|| f64::from(demand.reactive) / f64::from(classified));
    demand.by_source = by_source.into_values().collect();
    demand
        .by_source
        .sort_by(|a, b| b.count.cmp(&a.count).then(a.source.cmp(&b.source)));

    let rosters = roster_sets(input.rosters);
    let concentration = concentration(input, &rosters, window_start, now);
    let effort = effort(input, &rosters, window_start, now);

    let mut report = CapacityReport {
        generated_at: now,
        weeks,
        window_start: first_monday.format("%Y-%m-%d").to_string(),
        window_end: end.format("%Y-%m-%d").to_string(),
        flow,
        weekly,
        cycle,
        wip,
        demand,
        standing,
        concentration,
        effort,
        facts: Vec::new(),
        notes: Vec::new(),
    };
    report.facts = facts(&report);
    report.notes = notes(&report);
    report
}

/// Who wrote the merged work, repository by repository.
fn concentration(
    input: &CapacityInput,
    rosters: &HashMap<String, HashSet<String>>,
    window_start: DateTime<Utc>,
    now: DateTime<Utc>,
) -> ConcentrationSummary {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut by_repo: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut by_author: BTreeMap<String, u32> = BTreeMap::new();
    let mut total = 0u32;
    for pr in input
        .pull_requests
        .iter()
        .filter(|p| p.status == PrStatus::Completed)
    {
        let when = pr.closed_at.or(pr.created_at);
        if !when.is_some_and(|t| t >= window_start && t <= now) {
            continue;
        }
        let Some(author) = pr.author.as_deref().filter(|a| !a.trim().is_empty()) else {
            continue;
        };
        if is_service_identity(author)
            || !is_member(
                rosters,
                &pr.team,
                &identity_tokens(Some(author), pr.author_unique.as_deref()),
            )
        {
            continue;
        }
        // The same PR can be stored once per team that polls its project.
        if !pr.url.is_empty() && !seen.insert(pr.url.as_str()) {
            continue;
        }
        total += 1;
        *by_author.entry(author.to_string()).or_default() += 1;
        by_repo
            .entry(pr.repository.clone().unwrap_or_else(|| "(unknown)".into()))
            .or_default()
            .push(author.to_string());
    }
    let mut ranked: Vec<(&String, &u32)> = by_author.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let mut running = 0u32;
    let mut people_for_half = 0u32;
    for (_, n) in &ranked {
        running += **n;
        people_for_half += 1;
        if running * 2 >= total {
            break;
        }
    }
    let mut repos: Vec<RepoConcentration> = by_repo
        .into_iter()
        .filter(|(_, authors)| authors.len() >= 3)
        .map(|(repo, authors)| {
            let mut counts: BTreeMap<&String, u32> = BTreeMap::new();
            for a in &authors {
                *counts.entry(a).or_default() += 1;
            }
            let (top, top_n) = counts
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
                .map(|(a, n)| ((*a).clone(), *n))
                .unwrap_or_default();
            let share = f64::from(top_n) / authors.len() as f64;
            RepoConcentration {
                repo,
                prs: authors.len() as u32,
                authors: counts.len() as u32,
                top_author: top,
                top_share: share,
                single_point: authors.len() >= 5 && share >= 0.8,
            }
        })
        .collect();
    repos.sort_by(|a, b| {
        b.single_point
            .cmp(&a.single_point)
            .then(b.prs.cmp(&a.prs))
            .then(a.repo.cmp(&b.repo))
    });
    repos.truncate(15);
    ConcentrationSummary {
        merged_prs: total,
        people_for_half,
        top_author: ranked.first().map(|(a, _)| (*a).clone()),
        top_share: ranked
            .first()
            .filter(|_| total > 0)
            .map(|(_, n)| f64::from(**n) / f64::from(total)),
        repos,
    }
}

/// Weekdays (Mon-Fri) from `from` to `to` inclusive.
fn weekdays_between(from: NaiveDate, to: NaiveDate) -> u32 {
    let mut n = 0;
    let mut d = from;
    while d <= to {
        if d.weekday().num_days_from_monday() < 5 {
            n += 1;
        }
        d += Duration::days(1);
    }
    n
}

/// When people commit: hours, weekends, late nights, streaks. Each person is read in their own
/// timezone when a profile gives one (a colleague on the other side of the world is not "working
/// late" at their own lunchtime), else in the viewer's clock. Commit timestamps are a rough proxy
/// for effort (they undercount everything that isn't a commit), so this is corroboration, not a
/// timesheet.
fn effort(
    input: &CapacityInput,
    rosters: &HashMap<String, HashSet<String>>,
    window_start: DateTime<Utc>,
    now: DateTime<Utc>,
) -> EffortSummary {
    struct Acc {
        name: String,
        names: HashMap<String, u32>,
        emails: HashMap<String, u32>,
        tokens: HashSet<String>,
        times: Vec<DateTime<Utc>>,
    }
    let viewer = Duration::minutes(i64::from(input.tz_offset_minutes));
    let mut by_person: BTreeMap<String, Acc> = BTreeMap::new();
    for c in input
        .commits
        .iter()
        .filter(|c| c.authored_at >= window_start && c.authored_at <= now)
    {
        let tokens = identity_tokens(Some(&c.author), c.author_email.as_deref());
        if is_service_identity(&c.author) || !is_member(rosters, &c.team, &tokens) {
            continue;
        }
        let key = c
            .author_email
            .as_deref()
            .filter(|e| !e.trim().is_empty())
            .unwrap_or(&c.author)
            .trim()
            .to_ascii_lowercase();
        let acc = by_person.entry(key).or_insert_with(|| Acc {
            name: c.author.clone(),
            names: HashMap::new(),
            emails: HashMap::new(),
            tokens: HashSet::new(),
            times: Vec::new(),
        });
        *acc.names.entry(c.author.clone()).or_default() += 1;
        if let Some(e) = c.author_email.as_deref().filter(|e| !e.trim().is_empty()) {
            *acc.emails.entry(e.trim().to_string()).or_default() += 1;
        }
        acc.tokens.extend(tokens);
        acc.times.push(c.authored_at);
    }
    if by_person.is_empty() {
        return EffortSummary::default();
    }
    let after_hours = |t: &chrono::NaiveDateTime| {
        t.hour() < 9 || t.hour() >= 18 || t.weekday().num_days_from_monday() >= 5
    };
    let win_from = window_start.date_naive();
    let win_to = now.date_naive();
    let all_weekdays = weekdays_between(win_from, win_to);
    let mut hours = vec![0u32; 24];
    let mut heat = vec![vec![0u32; 24]; 7];
    let (mut commits, mut after) = (0u32, 0u32);
    let mut persons: Vec<PersonEffort> = Vec::new();
    for acc in by_person.into_values() {
        let most_common = |m: &HashMap<String, u32>| {
            m.iter()
                .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
                .map(|(n, _)| n.clone())
        };
        let name = most_common(&acc.names).unwrap_or(acc.name);
        let unique_name = most_common(&acc.emails);
        let profile = input
            .people
            .iter()
            .find(|p| p.tokens().iter().any(|t| acc.tokens.contains(t)));
        let zone: Option<(Tz, String)> = profile
            .and_then(|p| p.timezone.as_deref())
            .and_then(|z| z.parse::<Tz>().ok().map(|tz| (tz, z.to_string())));
        let local = |at: DateTime<Utc>| match &zone {
            Some((tz, _)) => at.with_timezone(tz).naive_local(),
            None => (at + viewer).naive_utc(),
        };

        let mut leave: HashSet<NaiveDate> = HashSet::new();
        for l in profile.map(|p| p.leave.as_slice()).unwrap_or_default() {
            let (mut d, end) = (l.start.max(win_from), l.end.min(win_to));
            while d <= end {
                leave.insert(d);
                d += Duration::days(1);
            }
        }
        let leave_days = leave
            .iter()
            .filter(|d| d.weekday().num_days_from_monday() < 5)
            .count() as u32;

        let mut p_hours = vec![0u32; 24];
        let mut days: BTreeMap<NaiveDate, Vec<chrono::NaiveDateTime>> = BTreeMap::new();
        let mut p_after = 0u32;
        for at in &acc.times {
            let t = local(*at);
            let h = t.hour() as usize;
            p_hours[h] += 1;
            hours[h] += 1;
            heat[t.weekday().num_days_from_monday() as usize][h] += 1;
            commits += 1;
            if after_hours(&t) {
                p_after += 1;
                after += 1;
            }
            days.entry(t.date()).or_default().push(t);
        }
        let weekend_days = days
            .keys()
            .filter(|d| d.weekday().num_days_from_monday() >= 5)
            .count() as u32;
        let late_nights = days
            .values()
            .filter(|ts| ts.iter().any(|t| t.hour() >= 22 || t.hour() < 5))
            .count() as u32;
        let worked_on_leave_days = days.keys().filter(|d| leave.contains(d)).count() as u32;
        let (mut streak, mut longest) = (0u32, 0u32);
        let mut prev: Option<NaiveDate> = None;
        for d in days.keys() {
            streak = match prev {
                Some(p) if (*d - p).num_days() == 1 => streak + 1,
                _ => 1,
            };
            longest = longest.max(streak);
            prev = Some(*d);
        }
        let longest_span = days
            .values()
            .map(|ts| {
                let first = ts.iter().min().copied().unwrap_or_default();
                let last = ts.iter().max().copied().unwrap_or_default();
                (last - first).num_seconds() as f64 / 3600.0
            })
            .fold(0.0, f64::max);
        persons.push(PersonEffort {
            name,
            unique_name,
            timezone: zone.map(|(_, z)| z),
            commits: acc.times.len() as u32,
            active_days: days.len() as u32,
            available_days: all_weekdays.saturating_sub(leave_days),
            leave_days,
            worked_on_leave_days,
            after_hours_share: f64::from(p_after) / acc.times.len() as f64,
            weekend_days,
            late_nights,
            longest_streak: longest,
            longest_span_hours: longest_span,
            hours: p_hours,
        });
    }
    persons.sort_by(|a, b| b.commits.cmp(&a.commits).then(a.name.cmp(&b.name)));
    persons.truncate(MAX_PEOPLE);
    EffortSummary {
        available: true,
        commits,
        after_hours_share: Some(f64::from(after) / f64::from(commits)),
        hours,
        heat,
        people: persons,
    }
}

fn pct(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Plain-language findings, computed from the numbers (no AI). They are what the screen leads with
/// and what the AI summary is grounded in, so every sentence here is a fact about the data.
fn facts(r: &CapacityReport) -> Vec<String> {
    let mut out = Vec::new();
    let f = &r.flow;
    if f.arrived > 0 || f.closed > 0 {
        let growth = f.backlog_growth_per_week;
        let trend = if growth.abs() < 0.5 {
            "so the backlog is roughly holding steady".to_string()
        } else if growth > 0.0 {
            format!("so the backlog grew by about {:.0} a week", growth)
        } else {
            format!("so the backlog shrank by about {:.0} a week", -growth)
        };
        let ratio = f
            .ratio
            .map(|x| format!(" ({x:.1} in for every 1 out)"))
            .unwrap_or_default();
        out.push(format!(
            "Over the last {} weeks {} items arrived and {} were finished{ratio}, {trend}.",
            r.weeks, f.arrived, f.closed
        ));
        let bulk = if f.bulk_weeks > 0 {
            format!(
                "; {} looked like bulk clean-ups and closed {} more than that, which is not counted as capacity",
                plural(f.bulk_weeks, "week", "weeks"),
                f.bulk_closed
            )
        } else {
            String::new()
        };
        out.push(format!(
            "A typical week finishes about {:.0} {}{bulk}.",
            f.typical_weekly_throughput,
            if f.typical_weekly_throughput.round() as i64 == 1 {
                "item"
            } else {
                "items"
            }
        ));
    }
    if let (Some(m), Some(p)) = (r.cycle.median_days, r.cycle.p85_days) {
        out.push(format!(
            "Half of finished items took {m:.0} days or less from creation, and 85% were done within {p:.0} days ({} finished).",
            r.cycle.n
        ));
    }
    let d = &r.demand;
    if let Some(share) = d.reactive_share {
        out.push(format!(
            "{} of the demand that could be classified is reactive (escalations, support, requests from other teams); {} planned and {} maintenance. {} of {} new items are not classified yet.",
            pct(share), d.planned, d.maintenance, d.unknown, d.total
        ));
    }
    if let Some(w) = f.weeks_to_clear {
        out.push(format!(
            "{} items are open; at a typical week's pace that is about {:.0} weeks of work even if nothing new arrived.",
            f.open_backlog, w
        ));
    }
    if r.wip.in_progress > 0 || r.wip.unassigned_open > 0 {
        out.push(format!(
            "{} items are in progress ({} untouched for 30+ days) and {} open items have no owner.",
            r.wip.in_progress, r.wip.stalled_30, r.wip.unassigned_open
        ));
    }
    let c = &r.concentration;
    if let (Some(a), Some(s)) = (&c.top_author, c.top_share) {
        let single = c.repos.iter().filter(|x| x.single_point).count() as u32;
        out.push(format!(
            "{} of {} merged pull requests were written by {a}; {} of the merged work rests on {} {}.{}",
            pct(s),
            c.merged_prs,
            "half",
            c.people_for_half,
            if c.people_for_half == 1 { "person" } else { "people" },
            if single > 0 {
                format!(" {} rest almost entirely on one author.", plural(single, "repository", "repositories"))
            } else {
                String::new()
            }
        ));
    }
    if let Some(a) = r.effort.after_hours_share.filter(|_| r.effort.available) {
        out.push(format!(
            "{} of {} commits fall outside 09:00-18:00 Monday-Friday in each person's own timezone.",
            pct(a),
            r.effort.commits
        ));
    }
    let on_leave: Vec<String> = r
        .effort
        .people
        .iter()
        .filter(|p| p.worked_on_leave_days > 0)
        .map(|p| {
            format!(
                "{} ({})",
                p.name,
                plural(p.worked_on_leave_days, "day", "days")
            )
        })
        .collect();
    if !on_leave.is_empty() {
        out.push(format!(
            "Commits were made on days recorded as leave by {}.",
            on_leave.join(", ")
        ));
    }
    out
}

fn notes(r: &CapacityReport) -> Vec<String> {
    let mut out = Vec::new();
    if !r.effort.available {
        out.push("No commit activity has been collected yet, so the working-hours picture is empty. It is gathered in the background after a poll.".into());
    } else {
        out.push("Commit timestamps are a rough proxy for effort: they miss meetings, reviews, escalations and any work that isn't a commit. Treat them as corroboration, not a timesheet.".into());
        let unset = r
            .effort
            .people
            .iter()
            .filter(|p| p.timezone.is_none())
            .count();
        if unset > 0 {
            out.push(format!(
                "{} of {} people have no timezone set, so their hours are read in the viewer's clock; set it under Timezones & leave if they work elsewhere.",
                unset,
                r.effort.people.len()
            ));
        }
        if r.effort.people.iter().all(|p| p.leave_days == 0) {
            out.push("No leave is recorded, so days off read as quiet days. Add leave under Timezones & leave.".into());
        }
    }
    if r.demand.unknown > 0 {
        out.push(format!(
            "{} new items have no source tag and no AI verdict, so they are not in the reactive/planned split. Use Classify with AI to fill them in.",
            r.demand.unknown
        ));
    }
    if r.demand.inferred > 0 {
        out.push(format!(
            "{} items are classified by an AI model rather than a tag; that is a judgement from the title, not a fact.",
            r.demand.inferred
        ));
    }
    if r.cycle.n < 10 {
        out.push(
            "Few items finished in this window, so the cycle-time percentiles are loose.".into(),
        );
    }
    if !r.standing.is_empty() {
        out.push(format!(
            "{} running-list style items are left out of the flow numbers.",
            r.standing.len()
        ));
    }
    out.push("Items in the Task, Epic and Feature types are not counted as demand; they are sub-steps or containers.".into());
    out
}

/// Context for an AI-written summary: the computed facts plus caveats, nothing else.
pub fn summary_facts(r: &CapacityReport) -> Vec<String> {
    let mut lines = r.facts.clone();
    for n in &r.notes {
        lines.push(format!("Caveat: {n}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use poseidon_core::{PrReviewer, WorkItem};

    fn now() -> DateTime<Utc> {
        "2026-10-14T12:00:00Z".parse().unwrap() // a Wednesday
    }

    fn ago(days: i64) -> DateTime<Utc> {
        now() - Duration::days(days)
    }

    fn wi(id: i64, ty: &str, state: &str, created_ago: i64, closed_ago: Option<i64>) -> WorkItem {
        WorkItem {
            id,
            provider: "azure-devops".into(),
            team: "A".into(),
            title: format!("item {id}"),
            work_item_type: ty.into(),
            state: state.into(),
            tags: vec![],
            assigned_to: None,
            created_by: None,
            created_by_unique: None,
            created_at: ago(created_ago),
            changed_at: ago(closed_ago.unwrap_or(created_ago.min(2))),
            closed_at: closed_ago.map(ago),
            iteration_path: None,
            story_points: None,
            description: None,
            url: format!("https://t/{id}"),
            linked_pr_ids: vec![],
            parent_id: None,
            linked_repos: vec![],
            board_column: None,
            board_column_done: None,
            board_lane: None,
            backlog_rank: None,
            linked_prs: vec![],
            tag_suggestions: vec![],
        }
    }

    fn run(items: &[WorkItem]) -> CapacityReport {
        let demand = HashMap::new();
        let rosters = HashMap::new();
        capacity(&CapacityInput {
            work_items: items,
            pull_requests: &[],
            commits: &[],
            demand: &demand,
            rosters: &rosters,
            now: now(),
            weeks: 8,
            tz_offset_minutes: 0,
            people: &[],
        })
    }

    #[test]
    fn flow_counts_arrivals_and_completions_and_skips_containers() {
        let items = vec![
            wi(1, "User Story", "Closed", 20, Some(10)),
            wi(2, "Bug", "Resolved", 5, Some(1)),
            wi(3, "User Story", "New", 3, None),
            wi(4, "Task", "Closed", 5, Some(1)), // a sub-step, not demand
            wi(5, "Epic", "New", 3, None),       // a container
            wi(6, "Bug", "Removed", 8, Some(2)), // dropped, not delivered
        ];
        let r = run(&items);
        assert_eq!(r.flow.arrived, 4, "1, 2, 3 and the removed bug arrived");
        assert_eq!(r.flow.closed, 2, "only delivered items count as closed");
        assert_eq!(r.flow.open_backlog, 1);
        assert_eq!(r.weekly.len(), 8);
        assert!(r.flow.ratio.is_some_and(|x| (x - 2.0).abs() < 1e-9));
    }

    #[test]
    fn a_clean_up_week_is_not_read_as_capacity() {
        // Seven normal weeks closing 2 a week and one week that closes 30 at once.
        let mut items = Vec::new();
        let mut id = 0;
        for (week_ago, n) in [
            (0, 2),
            (1, 2),
            (2, 2),
            (3, 30),
            (4, 2),
            (5, 2),
            (6, 2),
            (7, 2),
        ] {
            for _ in 0..n {
                id += 1;
                items.push(wi(id, "User Story", "Closed", 60, Some(week_ago * 7 + 1)));
            }
        }
        let r = run(&items);
        assert_eq!(r.flow.bulk_weeks, 1);
        assert!(
            (r.flow.typical_weekly_throughput - 2.0).abs() < 1e-9,
            "{:?}",
            r.flow
        );
        assert!(r.flow.bulk_closed >= 26);
        assert!(r.facts.iter().any(|f| f.contains("bulk clean-ups")));
    }

    #[test]
    fn cycle_time_uses_percentiles_not_averages() {
        let mut items = Vec::new();
        // nine quick items (5 days) and one that took a year
        for i in 0..9 {
            items.push(wi(i, "Bug", "Closed", 6, Some(1)));
        }
        items.push(wi(99, "Bug", "Closed", 366, Some(1)));
        let r = run(&items);
        assert_eq!(r.cycle.n, 10);
        assert!((r.cycle.median_days.unwrap() - 5.0).abs() < 0.01);
        // nearest-rank: with 10 items p85 is the 9th, and only p95 reaches the one outlier - a
        // single huge item doesn't distort the headline numbers the way an average would
        assert!((r.cycle.p85_days.unwrap() - 5.0).abs() < 0.01);
        assert!(
            (r.cycle.p95_days.unwrap() - 365.0).abs() < 0.01,
            "the tail shows up at p95"
        );
    }

    #[test]
    fn work_in_progress_flags_stalled_items_and_counts_owners() {
        let mut active_old = wi(1, "User Story", "Active", 200, None);
        active_old.changed_at = ago(100);
        active_old.assigned_to = Some("Ana".into());
        let mut active_new = wi(2, "Bug", "In Progress", 4, None);
        active_new.assigned_to = Some("Ana".into());
        let queued = wi(3, "User Story", "New", 10, None); // open, unowned, not started
        let r = run(&[active_old, active_new, queued]);
        assert_eq!(r.wip.in_progress, 2);
        assert_eq!((r.wip.stalled_30, r.wip.stalled_90), (1, 1));
        assert_eq!(r.wip.unassigned_open, 1);
        assert_eq!(r.wip.per_person[0].name, "Ana");
        assert_eq!(r.wip.per_person[0].in_progress, 2);
    }

    #[test]
    fn demand_is_classified_by_tag_then_ai_and_unknown_is_never_hidden() {
        let mut support = wi(1, "Bug", "New", 5, None);
        support.tags = vec!["source:support".into(), "area:x".into()];
        let mut roadmap = wi(2, "User Story", "New", 5, None);
        roadmap.tags = vec!["source:roadmap".into()];
        let ai_item = wi(3, "User Story", "New", 5, None);
        let mystery = wi(4, "User Story", "New", 5, None);
        let mut debt = wi(5, "Improvement", "New", 5, None);
        debt.tags = vec!["initiative:technical-debt".into()];
        let mut demand = HashMap::new();
        demand.insert(
            3,
            DemandVerdictRecord {
                work_item_id: 3,
                team: "A".into(),
                class: DemandClass::Reactive,
                source: Some("request".into()),
                title_hash: "h".into(),
            },
        );
        let rosters = HashMap::new();
        let r = capacity(&CapacityInput {
            work_items: &[support, roadmap, ai_item, mystery, debt],
            pull_requests: &[],
            commits: &[],
            demand: &demand,
            rosters: &rosters,
            now: now(),
            weeks: 8,
            tz_offset_minutes: 0,
            people: &[],
        });
        let d = &r.demand;
        assert_eq!(
            (d.total, d.reactive, d.planned, d.maintenance, d.unknown),
            (5, 2, 1, 1, 1)
        );
        assert_eq!((d.tagged, d.inferred), (3, 1));
        // 2 of the 4 classified items are reactive
        assert!((d.reactive_share.unwrap() - 0.5).abs() < 1e-9);
        let req = d.by_source.iter().find(|s| s.source == "request").unwrap();
        assert_eq!(
            (req.count, req.inferred),
            (1, 1),
            "the AI-derived source is marked"
        );
        assert!(d.by_source.iter().any(|s| s.source == "(unclassified)"));
        assert!(r.notes.iter().any(|n| n.contains("no source tag")));
    }

    #[test]
    fn running_lists_are_left_out_of_flow_and_listed_separately() {
        let mut running = wi(1, "User Story", "Active", 200, None);
        running.title = "Running list of small requests".into();
        let mut ai_standing = wi(2, "User Story", "New", 50, None);
        ai_standing.title = "Misc platform odds and ends".into();
        let mut demand = HashMap::new();
        demand.insert(
            2,
            DemandVerdictRecord {
                work_item_id: 2,
                team: "A".into(),
                class: DemandClass::Standing,
                source: None,
                title_hash: "h".into(),
            },
        );
        let fresh_title_match = {
            let mut w = wi(3, "User Story", "New", 5, None); // too new to call "standing" by title
            w.title = "Ongoing review of alerts".into();
            w
        };
        let real = wi(4, "Bug", "New", 3, None);
        let rosters = HashMap::new();
        let r = capacity(&CapacityInput {
            work_items: &[running, ai_standing, fresh_title_match, real],
            pull_requests: &[],
            commits: &[],
            demand: &demand,
            rosters: &rosters,
            now: now(),
            weeks: 8,
            tz_offset_minutes: 0,
            people: &[],
        });
        let ids: Vec<i64> = r.standing.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec![1, 2],
            "oldest first; a brand-new 'ongoing' item stays in the flow"
        );
        assert_eq!(r.standing[0].reason, "title");
        assert_eq!(r.standing[1].reason, "ai");
        assert_eq!(r.flow.open_backlog, 2, "only the two non-standing items");
    }

    fn pr(id: i64, repo: &str, author: &str, closed_ago: i64) -> PullRequest {
        PullRequest {
            id,
            provider: "azure-devops".into(),
            team: "A".into(),
            title: "t".into(),
            status: PrStatus::Completed,
            is_draft: false,
            repository: Some(repo.into()),
            author: Some(author.into()),
            author_unique: None,
            created_at: Some(ago(closed_ago + 1)),
            source_branch: None,
            target_branch: None,
            closed_at: Some(ago(closed_ago)),
            reviewer_count: 0,
            reviewers: Vec::<PrReviewer>::new(),
            url: format!("https://pr/{id}"),
            flags: vec![],
            linked_work_items: vec![],
        }
    }

    #[test]
    fn concentration_finds_the_repos_only_one_person_works_on() {
        let mut prs = Vec::new();
        for i in 0..9 {
            prs.push(pr(i, "iac", "Ana", 3)); // Ana does nearly all of iac
        }
        prs.push(pr(100, "iac", "Ben", 3));
        for i in 0..4 {
            prs.push(pr(
                200 + i,
                "tools",
                if i % 2 == 0 { "Ana" } else { "Ben" },
                3,
            )); // shared
        }
        prs.push(pr(300, "old", "Ana", 200)); // outside the window
        let demand = HashMap::new();
        let rosters = HashMap::new();
        let r = capacity(&CapacityInput {
            work_items: &[],
            pull_requests: &prs,
            commits: &[],
            demand: &demand,
            rosters: &rosters,
            now: now(),
            weeks: 8,
            tz_offset_minutes: 0,
            people: &[],
        });
        let c = &r.concentration;
        assert_eq!(c.merged_prs, 14);
        assert_eq!(c.top_author.as_deref(), Some("Ana"));
        assert_eq!(c.people_for_half, 1, "Ana alone is more than half");
        let iac = c.repos.iter().find(|x| x.repo == "iac").unwrap();
        assert!(iac.single_point && (iac.top_share - 0.9).abs() < 1e-9);
        let tools = c.repos.iter().find(|x| x.repo == "tools").unwrap();
        assert!(!tools.single_point, "shared work is not a single point");
        assert_eq!(c.repos[0].repo, "iac", "single points sort first");
    }

    fn commit(id: &str, who: &str, at: &str) -> CommitActivity {
        CommitActivity {
            id: id.into(),
            team: "A".into(),
            repo: "iac".into(),
            author: who.into(),
            author_email: Some(format!("{}@x.com", who.to_lowercase())),
            authored_at: at.parse().unwrap(),
        }
    }

    #[test]
    fn effort_shows_hours_late_nights_weekends_and_streaks_in_the_viewers_timezone() {
        // Mon 12 Oct .. Sun 18 is NOT in range for a "now" of Wed 14 Oct, so use the prior week:
        // Mon 5 .. Sun 11 October 2026.
        let commits = vec![
            commit("1", "Ana", "2026-10-05T10:00:00Z"), // Mon, in hours
            commit("2", "Ana", "2026-10-06T19:30:00Z"), // Tue evening
            commit("3", "Ana", "2026-10-06T23:30:00Z"), // Tue late night
            commit("4", "Ana", "2026-10-07T01:00:00Z"), // Wed 01:00 (overnight)
            commit("5", "Ana", "2026-10-10T11:00:00Z"), // Saturday
            commit("6", "Ben", "2026-10-05T10:00:00Z"),
            commit("7", "Bot", "2026-10-05T10:00:00Z"),
        ];
        let mut commits = commits;
        commits[6].author = "Project Collection Build Service".into();
        let demand = HashMap::new();
        let rosters = HashMap::new();
        let r = capacity(&CapacityInput {
            work_items: &[],
            pull_requests: &[],
            commits: &commits,
            demand: &demand,
            rosters: &rosters,
            now: now(),
            weeks: 8,
            tz_offset_minutes: 0,
            people: &[],
        });
        let e = &r.effort;
        assert!(e.available);
        assert_eq!(e.commits, 6, "the build service is not a person");
        let ana = e.people.iter().find(|p| p.name == "Ana").unwrap();
        assert_eq!(ana.commits, 5);
        assert_eq!(ana.active_days, 4);
        assert_eq!(ana.weekend_days, 1);
        assert_eq!(
            ana.late_nights, 2,
            "Tue 23:30 and Wed 01:00 are separate days"
        );
        // Mon, Tue, Wed run together; Saturday breaks it
        assert_eq!(ana.longest_streak, 3);
        // 4 of 5 are outside 09-18 Mon-Fri (19:30, 23:30, 01:00, Saturday)
        assert!((ana.after_hours_share - 0.8).abs() < 1e-9);
        assert_eq!(ana.hours[19], 1);
        // Tuesday runs 19:30 -> 23:30
        assert!((ana.longest_span_hours - 4.0).abs() < 1e-9);
        assert_eq!(e.heat[5][11], 1, "Saturday 11:00");

        // The same instants in UTC+1 (British Summer Time): Mon 10:00Z is 11:00 local, and
        // Tue 23:30Z is already 00:30 on Wednesday.
        let shifted = capacity(&CapacityInput {
            work_items: &[],
            pull_requests: &[],
            commits: &commits,
            demand: &demand,
            rosters: &rosters,
            now: now(),
            weeks: 8,
            tz_offset_minutes: 60,
            people: &[],
        });
        let ana = shifted
            .effort
            .people
            .iter()
            .find(|p| p.name == "Ana")
            .unwrap();
        assert_eq!(ana.hours[11], 1);
        assert_eq!(ana.hours[0], 1, "23:30Z is 00:30 local");
    }

    fn effort_with(commits: &[CommitActivity], people: &[PersonProfile]) -> EffortSummary {
        let demand = HashMap::new();
        let rosters = HashMap::new();
        capacity(&CapacityInput {
            work_items: &[],
            pull_requests: &[],
            commits,
            demand: &demand,
            rosters: &rosters,
            now: now(),
            weeks: 8,
            tz_offset_minutes: 60, // the viewer is on British Summer Time
            people,
        })
        .effort
    }

    fn profile(name: &str, tz: Option<&str>, leave: Vec<(&str, &str)>) -> PersonProfile {
        PersonProfile {
            name: name.into(),
            timezone: tz.map(String::from),
            leave: leave
                .into_iter()
                .map(|(a, b)| poseidon_core::LeaveRange {
                    start: a.parse().unwrap(),
                    end: b.parse().unwrap(),
                    label: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_persons_own_timezone_decides_what_counts_as_after_hours() {
        // Tue 6 Oct 2026, 02:00 UTC: 10:00 in Manila (UTC+8) but 03:00 on a UK viewer's clock.
        let commits = vec![commit("1", "Ben", "2026-10-06T02:00:00Z")];
        let unset = effort_with(&commits, &[]);
        assert!(
            unset.people[0].after_hours_share > 0.99,
            "read in the viewer's clock"
        );
        assert_eq!(unset.people[0].timezone, None);

        let set = effort_with(&commits, &[profile("Ben", Some("Asia/Manila"), vec![])]);
        assert_eq!(set.people[0].timezone.as_deref(), Some("Asia/Manila"));
        assert!(
            set.people[0].after_hours_share < 0.01,
            "10:00 local is the working day"
        );
        assert_eq!(set.people[0].hours[10], 1);
        assert_eq!(
            set.heat[1][10], 1,
            "the team grid uses each person's own clock"
        );
    }

    #[test]
    fn daylight_saving_is_applied_for_the_commits_own_date() {
        // Sydney moved to daylight time on Sun 4 Oct 2026 (UTC+10 -> UTC+11). Both of these are
        // 09:30 on the wall clock, though the UTC hour differs by one.
        let commits = vec![
            commit("1", "Ana", "2026-09-29T23:30:00Z"), // Wed 30 Sep, 09:30 AEST
            commit("2", "Ana", "2026-10-06T22:30:00Z"), // Wed 7 Oct, 09:30 AEDT
        ];
        let e = effort_with(
            &commits,
            &[profile("Ana", Some("Australia/Sydney"), vec![])],
        );
        assert_eq!(e.people[0].hours[9], 2, "both are 09:30 local");
    }

    #[test]
    fn leave_is_counted_and_work_on_leave_is_flagged() {
        // Leave Mon 5 - Wed 7 Oct (3 weekdays); Ana still commits on the Tuesday.
        let commits = vec![
            commit("1", "Ana", "2026-10-06T10:00:00Z"),
            commit("2", "Ana", "2026-10-08T10:00:00Z"),
        ];
        let e = effort_with(
            &commits,
            &[profile("Ana", None, vec![("2026-10-05", "2026-10-07")])],
        );
        let ana = &e.people[0];
        assert_eq!(ana.leave_days, 3);
        assert_eq!(ana.worked_on_leave_days, 1);
        assert_eq!(ana.active_days, 2);
        let without = effort_with(&commits, &[]);
        assert_eq!(
            ana.available_days + 3,
            without.people[0].available_days,
            "leave comes off the weekdays available"
        );
    }

    #[test]
    fn leave_outside_the_window_is_ignored_and_ranges_are_clamped() {
        let commits = vec![commit("1", "Ana", "2026-10-06T10:00:00Z")];
        let e = effort_with(
            &commits,
            &[profile(
                "Ana",
                None,
                vec![("2020-01-01", "2020-12-31"), ("1900-01-01", "9999-12-31")],
            )],
        );
        let ana = &e.people[0];
        assert!(
            ana.leave_days > 0 && ana.available_days < 5,
            "second range covers the window"
        );
        assert_eq!(ana.worked_on_leave_days, 1);
    }

    #[test]
    fn people_are_matched_by_email_as_well_as_display_name() {
        let commits = vec![commit("1", "Benjamin", "2026-10-06T02:00:00Z")]; // ben... email: benjamin@x.com
        let mut p = profile("Ben", Some("Asia/Manila"), vec![]);
        p.unique_name = Some("benjamin@x.com".into());
        let e = effort_with(&commits, &[p]);
        assert_eq!(e.people[0].timezone.as_deref(), Some("Asia/Manila"));
    }

    #[test]
    fn notes_say_when_zones_or_leave_are_missing_and_facts_flag_work_on_leave() {
        let commits = vec![commit("1", "Ana", "2026-10-06T10:00:00Z")];
        let demand = HashMap::new();
        let rosters = HashMap::new();
        let run = |people: &[PersonProfile]| {
            capacity(&CapacityInput {
                work_items: &[],
                pull_requests: &[],
                commits: &commits,
                demand: &demand,
                rosters: &rosters,
                now: now(),
                weeks: 8,
                tz_offset_minutes: 0,
                people,
            })
        };
        let bare = run(&[]);
        assert!(bare.notes.iter().any(|n| n.contains("no timezone set")));
        assert!(bare
            .notes
            .iter()
            .any(|n| n.contains("No leave is recorded")));

        let full = run(&[profile(
            "Ana",
            Some("Europe/London"),
            vec![("2026-10-05", "2026-10-07")],
        )]);
        assert!(!full.notes.iter().any(|n| n.contains("no timezone set")));
        assert!(!full
            .notes
            .iter()
            .any(|n| n.contains("No leave is recorded")));
        assert!(full
            .facts
            .iter()
            .any(|f| f.contains("recorded as leave") && f.contains("Ana (1 day)")));
    }

    #[test]
    fn timezone_names_are_validated() {
        assert!(timezone_is_valid("Europe/London"));
        assert!(timezone_is_valid("Asia/Manila"));
        assert!(!timezone_is_valid("Mars/Olympus"));
        assert!(!timezone_is_valid(""));
    }

    #[test]
    fn with_no_commits_the_effort_panel_says_so_instead_of_inventing_a_picture() {
        let r = run(&[]);
        assert!(!r.effort.available);
        assert!(r.notes.iter().any(|n| n.contains("No commit activity")));
        assert!(r.effort.people.is_empty());
    }

    #[test]
    fn rosters_keep_other_teams_people_out_of_the_people_panels() {
        let commits = vec![
            commit("1", "Ana", "2026-10-05T10:00:00Z"),
            commit("2", "Zed", "2026-10-05T11:00:00Z"),
        ];
        let demand = HashMap::new();
        let mut rosters = HashMap::new();
        rosters.insert("a".to_string(), vec!["ana@x.com".to_string()]);
        let r = capacity(&CapacityInput {
            work_items: &[],
            pull_requests: &[],
            commits: &commits,
            demand: &demand,
            rosters: &rosters,
            now: now(),
            weeks: 8,
            tz_offset_minutes: 0,
            people: &[],
        });
        let names: Vec<&str> = r.effort.people.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Ana"]);
    }

    #[test]
    fn demand_candidates_pick_untagged_arrivals_and_busy_old_items_but_not_the_judged() {
        let untagged = wi(1, "User Story", "New", 5, None);
        let mut tagged = wi(2, "Bug", "New", 5, None);
        tagged.tags = vec!["source:support".into()];
        let judged = wi(3, "Bug", "New", 4, None);
        let mut retitled = wi(4, "Bug", "New", 4, None);
        retitled.title = "A new title".into();
        let task = wi(5, "Task", "New", 4, None);
        let mut old_busy = wi(6, "User Story", "Active", 200, None);
        old_busy.changed_at = ago(3); // touched this week
        let old_idle = wi(7, "User Story", "New", 200, None);
        let mut old_idle = old_idle;
        old_idle.changed_at = ago(150); // untouched for months
        let mut old_closed = wi(8, "User Story", "Closed", 200, Some(100));
        old_closed.changed_at = ago(2);
        let items = [
            untagged, tagged, judged, retitled, task, old_busy, old_idle, old_closed,
        ];
        let mut known = HashMap::new();
        known.insert(3, demand_title_hash("item 3")); // judged from its current title
        known.insert(4, demand_title_hash("the old title")); // judged from a different one
        let got: Vec<i64> = demand_candidates(&items, &known, now(), 8)
            .iter()
            .map(|i| i.id)
            .collect();
        // arrivals newest-first (1 is 5d old, 4 is 4d old => 4 then 1), then the busy old one
        assert_eq!(got, vec![4, 1, 6]);
    }

    #[test]
    fn the_title_hash_is_stable_and_ignores_case_and_padding() {
        assert_eq!(
            demand_title_hash(" Running LIST "),
            demand_title_hash("running list")
        );
        assert_ne!(demand_title_hash("a"), demand_title_hash("b"));
        // fixed value: verdicts stored by one build must still match in the next
        assert_eq!(demand_title_hash("a"), "af63dc4c8601ec8c");
    }

    #[test]
    fn facts_lead_with_the_arrival_against_completion_story() {
        let mut items = Vec::new();
        for i in 0..30 {
            items.push(wi(i, "User Story", "New", 10, None));
        }
        for i in 100..110 {
            items.push(wi(i, "Bug", "Closed", 12, Some(3)));
        }
        let r = run(&items);
        let first = &r.facts[0];
        assert!(
            first.contains("40 items arrived and 10 were finished"),
            "{first}"
        );
        assert!(first.contains("4.0 in for every 1 out"), "{first}");
        assert!(first.contains("backlog grew"), "{first}");
        assert!(summary_facts(&r).iter().any(|l| l.starts_with("Caveat:")));
    }
}
