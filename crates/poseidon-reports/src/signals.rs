//! The Interruptions report: a pure function from stored chat-channel threads to a
//! [`SignalsReport`] - how many asks arrive, who they come from, how quickly they get an answer,
//! who answers, and what is still waiting. No IO and no clock (`now` is injected).
//!
//! It works from thread metadata only (who started it, who answered, when) - never message text.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Datelike, Duration, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use poseidon_core::{
    identity_tokens, ChannelSummary, ChannelThread, PersonProfile, ResponseTimes, SignalPerson,
    SignalWeek, SignalsReport, WaitingThread,
};

use crate::{is_member, is_restricted, is_service_identity, roster_sets};

/// Everything the report is computed from.
pub struct SignalsInput<'a> {
    pub threads: &'a [ChannelThread],
    /// Team name (lowercase) -> identity tokens of the people on that team.
    pub rosters: &'a HashMap<String, Vec<String>>,
    /// Per-person timezone, so "after hours" is the poster's own evening.
    pub people: &'a [PersonProfile],
    pub now: DateTime<Utc>,
    /// Window length in weeks (clamped to 1-52).
    pub weeks: u32,
    /// Minutes east of UTC for the viewer's clock (the fallback for people with no timezone).
    pub tz_offset_minutes: i32,
}

const MAX_LIST: usize = 8;
const MAX_WAITING: usize = 10;
/// A thread younger than this is not "waiting" yet - nobody has had a chance.
const WAITING_AFTER_HOURS: i64 = 4;

fn nearest_rank(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

fn pct(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

fn duration_text(minutes: f64) -> String {
    if minutes < 90.0 {
        format!("{minutes:.0} minutes")
    } else if minutes < 48.0 * 60.0 {
        format!("{:.1} hours", minutes / 60.0)
    } else {
        format!("{:.1} days", minutes / 1440.0)
    }
}

fn monday(d: NaiveDate) -> NaiveDate {
    d - Duration::days(i64::from(d.weekday().num_days_from_monday()))
}

pub fn signals(input: &SignalsInput) -> SignalsReport {
    let weeks = input.weeks.clamp(1, 52);
    let now = input.now;
    let window_start = now - Duration::weeks(i64::from(weeks));
    let viewer = Duration::minutes(i64::from(input.tz_offset_minutes));
    let rosters = roster_sets(input.rosters);

    let in_window: Vec<&ChannelThread> = input
        .threads
        .iter()
        .filter(|t| t.created_at >= window_start && t.created_at <= now)
        .collect();
    if in_window.is_empty() {
        let mut r = SignalsReport {
            weeks,
            ..Default::default()
        };
        r.available = !input.threads.is_empty();
        r.generated_at = Some(now);
        r.notes = notes(&r, false);
        return r;
    }

    let is_human = |t: &ChannelThread| !t.automated && !is_service_identity(&t.author);
    let on_team = |team: &str, name: &str| {
        is_restricted(&rosters, team)
            && is_member(&rosters, team, &identity_tokens(Some(name), None))
    };
    let zone_of = |name: &str| -> Option<Tz> {
        let tokens = identity_tokens(Some(name), None);
        input
            .people
            .iter()
            .find(|p| p.tokens().iter().any(|t| tokens.contains(t)))
            .and_then(|p| p.timezone.as_deref())
            .and_then(|z| z.parse::<Tz>().ok())
    };

    let humans: Vec<&ChannelThread> = in_window.iter().copied().filter(|t| is_human(t)).collect();
    let automated = (in_window.len() - humans.len()) as u32;

    // Weeks, oldest first, every one present so a quiet week shows as zero.
    let first = monday((window_start + viewer).date_naive());
    let last = monday((now + viewer).date_naive());
    let mut weekly: BTreeMap<NaiveDate, (u32, u32)> = BTreeMap::new();
    let mut d = first;
    while d <= last {
        weekly.insert(d, (0, 0));
        d += Duration::days(7);
    }

    let mut replies_total = 0u32;
    let mut urgent = 0u32;
    let mut after = 0u32;
    let (mut outside, mut outside_den) = (0u32, 0u32);
    let mut minutes: Vec<f64> = Vec::new();
    let mut waiting: Vec<WaitingThread> = Vec::new();
    let mut by_channel: BTreeMap<String, ChannelSummary> = BTreeMap::new();
    let mut askers: HashMap<(String, String), (u32, bool)> = HashMap::new();
    let mut responders: HashMap<(String, String), (u32, bool)> = HashMap::new();

    for t in &in_window {
        let ch = by_channel
            .entry(t.channel_name.clone())
            .or_insert_with(|| ChannelSummary {
                name: t.channel_name.clone(),
                threads: 0,
                automated: 0,
                unanswered: 0,
            });
        if !is_human(t) {
            ch.automated += 1;
            continue;
        }
        ch.threads += 1;
        let others: Vec<&poseidon_core::ThreadReply> = t
            .replies
            .iter()
            .filter(|r| !r.author.eq_ignore_ascii_case(&t.author))
            .collect();
        replies_total += others.len() as u32;
        let wk = monday((t.created_at + viewer).date_naive());
        if let Some(e) = weekly.get_mut(&wk) {
            e.0 += 1;
            e.1 += others.len() as u32;
        }
        if t.urgent {
            urgent += 1;
        }
        let local = match zone_of(&t.author) {
            Some(tz) => t.created_at.with_timezone(&tz).naive_local(),
            None => (t.created_at + viewer).naive_utc(),
        };
        if local.hour() < 9 || local.hour() >= 18 || local.weekday().num_days_from_monday() >= 5 {
            after += 1;
        }
        let member = on_team(&t.team, &t.author);
        if is_restricted(&rosters, &t.team) {
            outside_den += 1;
            if !member {
                outside += 1;
            }
        }
        let a = askers
            .entry((t.author.to_lowercase(), t.author.clone()))
            .or_insert((0, member));
        a.0 += 1;

        match t.first_reply_at() {
            Some(at) => minutes.push(((at - t.created_at).num_seconds().max(0)) as f64 / 60.0),
            None => {
                ch.unanswered += 1;
                let age = (now - t.created_at).num_hours();
                if age >= WAITING_AFTER_HOURS {
                    waiting.push(WaitingThread {
                        channel: t.channel_name.clone(),
                        author: t.author.clone(),
                        age_hours: age,
                        urgent: t.urgent,
                        url: t.web_url.clone(),
                    });
                }
            }
        }
        for r in &others {
            if is_service_identity(&r.author) {
                continue;
            }
            let e = responders
                .entry((r.author.to_lowercase(), r.author.clone()))
                .or_insert((0, on_team(&t.team, &r.author)));
            e.0 += 1;
        }
    }

    minutes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let response = ResponseTimes {
        answered: minutes.len() as u32,
        median_minutes: nearest_rank(&minutes, 0.5),
        p85_minutes: nearest_rank(&minutes, 0.85),
    };
    waiting.sort_by_key(|w| std::cmp::Reverse(w.age_hours));
    let unanswered_over_day = waiting.iter().filter(|w| w.age_hours >= 24).count() as u32;
    waiting.truncate(MAX_WAITING);

    let rank = |m: HashMap<(String, String), (u32, bool)>| {
        let mut v: Vec<SignalPerson> = m
            .into_iter()
            .map(|((_, name), (count, on_team))| SignalPerson {
                name,
                count,
                on_team,
            })
            .collect();
        v.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
        v.truncate(MAX_LIST);
        v
    };
    let responders = rank(responders);
    let askers = rank(askers);
    let top_responder_share = responders
        .first()
        .filter(|_| replies_total > 0)
        .map(|p| f64::from(p.count) / f64::from(replies_total));

    let mut channels: Vec<ChannelSummary> = by_channel.into_values().collect();
    channels.sort_by(|a, b| b.threads.cmp(&a.threads).then(a.name.cmp(&b.name)));

    let human_n = humans.len() as u32;
    let mut report = SignalsReport {
        available: true,
        generated_at: Some(now),
        weeks,
        threads: human_n,
        automated,
        replies: replies_total,
        outside_share: (outside_den > 0).then(|| f64::from(outside) / f64::from(outside_den)),
        after_hours_share: (human_n > 0).then(|| f64::from(after) / f64::from(human_n)),
        urgent,
        weekly: weekly
            .into_iter()
            .map(|(d, (threads, replies))| SignalWeek {
                week_start: d.format("%Y-%m-%d").to_string(),
                threads,
                replies,
            })
            .collect(),
        channels,
        response,
        waiting,
        unanswered_over_day,
        askers,
        responders,
        top_responder_share,
        facts: Vec::new(),
        notes: Vec::new(),
    };
    report.facts = facts(&report);
    report.notes = notes(&report, outside_den > 0);
    report
}

fn facts(r: &SignalsReport) -> Vec<String> {
    let mut out = Vec::new();
    if r.threads == 0 {
        return out;
    }
    let per_week = f64::from(r.threads) / f64::from(r.weeks);
    let outside = r
        .outside_share
        .map(|s| format!(", {} of them from someone outside the team", pct(s)))
        .unwrap_or_default();
    out.push(format!(
        "{} conversations were started in the watched channels over {} weeks (about {:.0} a week){outside}.",
        r.threads, r.weeks, per_week
    ));
    if let (Some(m), Some(p)) = (r.response.median_minutes, r.response.p85_minutes) {
        out.push(format!(
            "The first answer to a conversation took {} at the median and {} for 85% ({} answered).",
            duration_text(m),
            duration_text(p),
            r.response.answered
        ));
    }
    if r.unanswered_over_day > 0 {
        out.push(format!(
            "{} conversations have had no answer for more than a day.",
            r.unanswered_over_day
        ));
    }
    if let (Some(p), Some(share)) = (r.responders.first(), r.top_responder_share) {
        out.push(format!(
            "{} wrote {} of the {} answers given.",
            p.name,
            pct(share),
            r.replies
        ));
    }
    if let Some(a) = r.after_hours_share.filter(|a| *a > 0.0) {
        out.push(format!(
            "{} of conversations were started outside 09:00-18:00 Monday-Friday in the poster's own timezone.",
            pct(a)
        ));
    }
    if r.urgent > 0 {
        out.push(format!(
            "{} were marked urgent by the person asking.",
            r.urgent
        ));
    }
    if r.automated > 0 {
        out.push(format!(
            "{} automated posts (bots, connectors) are left out of these numbers.",
            r.automated
        ));
    }
    out
}

fn notes(r: &SignalsReport, has_roster: bool) -> Vec<String> {
    let mut out = vec![
        "Only conversations in channels you are a member of can be read, and only metadata (who started a thread, who answered, when) is kept - never the message text.".to_string(),
        "A conversation counts once however much work it caused: a quick question and a week-long escalation are each one. Read it as how often the team is interrupted, not how much effort it cost.".to_string(),
        "Answers to threads older than about two weeks are not re-read, and \"no answer\" can mean it was resolved in a call or a side chat.".to_string(),
    ];
    if !has_roster {
        out.push("No team roster is known, so conversations are not split into \"inside\" and \"outside\" the team.".to_string());
    }
    if !r.available {
        out.clear();
        out.push("No channel has been read yet.".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use poseidon_core::ThreadReply;

    fn now() -> DateTime<Utc> {
        "2026-10-14T12:00:00Z".parse().unwrap() // a Wednesday
    }

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn thread(id: &str, author: &str, created: &str, replies: Vec<(&str, &str)>) -> ChannelThread {
        ChannelThread {
            id: id.into(),
            team: "Platform".into(),
            channel_key: "g/19:c@thread".into(),
            channel_name: "Ops".into(),
            author: author.into(),
            automated: false,
            created_at: t(created),
            last_activity_at: t(created),
            urgent: false,
            web_url: Some(format!("https://teams.microsoft.com/l/message/{id}")),
            replies: replies
                .into_iter()
                .map(|(a, at)| ThreadReply {
                    author: a.into(),
                    at: t(at),
                })
                .collect(),
        }
    }

    fn run(threads: &[ChannelThread], rosters: HashMap<String, Vec<String>>) -> SignalsReport {
        signals(&SignalsInput {
            threads,
            rosters: &rosters,
            people: &[],
            now: now(),
            weeks: 4,
            tz_offset_minutes: 0,
        })
    }

    #[test]
    fn no_threads_gives_an_honest_empty_report() {
        let r = run(&[], HashMap::new());
        assert!(!r.available);
        assert_eq!(r.threads, 0);
        assert!(r.notes[0].contains("No channel has been read"));
    }

    #[test]
    fn response_times_use_the_first_answer_from_someone_other_than_the_poster() {
        let threads = vec![
            // answered after 30 min; the poster's own follow-up doesn't count
            thread(
                "1",
                "Zed",
                "2026-10-12T09:00:00Z",
                vec![
                    ("Zed", "2026-10-12T09:05:00Z"),
                    ("Ana", "2026-10-12T09:30:00Z"),
                ],
            ),
            // answered after 3 hours
            thread(
                "2",
                "Zed",
                "2026-10-12T10:00:00Z",
                vec![("Ben", "2026-10-12T13:00:00Z")],
            ),
            // never answered, and old
            thread("3", "Zed", "2026-10-09T10:00:00Z", vec![]),
        ];
        let r = run(&threads, HashMap::new());
        assert_eq!(r.threads, 3);
        assert_eq!(r.response.answered, 2);
        assert_eq!(r.response.median_minutes, Some(30.0));
        assert_eq!(r.response.p85_minutes, Some(180.0));
        assert_eq!(r.unanswered_over_day, 1);
        assert_eq!(r.waiting.len(), 1);
        assert_eq!(r.waiting[0].age_hours, 122);
        assert_eq!(r.replies, 2, "the poster's own reply is not an answer");
    }

    #[test]
    fn fresh_unanswered_threads_are_not_yet_waiting() {
        let threads = vec![thread("1", "Zed", "2026-10-14T11:00:00Z", vec![])];
        let r = run(&threads, HashMap::new());
        assert!(r.waiting.is_empty());
        assert_eq!(r.channels[0].unanswered, 1, "but they are still unanswered");
    }

    #[test]
    fn bots_are_counted_separately_and_left_out_of_the_human_numbers() {
        let mut bot = thread("1", "Alert Bot", "2026-10-12T09:00:00Z", vec![]);
        bot.automated = true;
        let threads = vec![bot, thread("2", "Zed", "2026-10-12T09:00:00Z", vec![])];
        let r = run(&threads, HashMap::new());
        assert_eq!((r.threads, r.automated), (1, 1));
        assert!(r.facts.iter().any(|f| f.contains("automated")));
        assert_eq!(r.channels[0].automated, 1);
    }

    #[test]
    fn a_roster_splits_asks_into_inside_and_outside_the_team() {
        let threads = vec![
            thread("1", "Ana", "2026-10-12T09:00:00Z", vec![]),
            thread(
                "2",
                "Zed",
                "2026-10-12T10:00:00Z",
                vec![("Ana", "2026-10-12T10:10:00Z")],
            ),
            thread(
                "3",
                "Yan",
                "2026-10-12T11:00:00Z",
                vec![("Ana", "2026-10-12T11:10:00Z")],
            ),
            thread(
                "4",
                "Zed",
                "2026-10-13T11:00:00Z",
                vec![("Ana", "2026-10-13T11:20:00Z")],
            ),
        ];
        let mut rosters = HashMap::new();
        rosters.insert("platform".to_string(), vec!["ana".to_string()]);
        let r = run(&threads, rosters);
        assert_eq!(r.outside_share, Some(0.75));
        let zed = r.askers.iter().find(|p| p.name == "Zed").unwrap();
        assert!(!zed.on_team && zed.count == 2);
        let ana = r.responders.iter().find(|p| p.name == "Ana").unwrap();
        assert!(ana.on_team);
        assert_eq!(r.top_responder_share, Some(1.0));
        assert!(r.facts.iter().any(|f| f.contains("Ana wrote 100%")));
    }

    #[test]
    fn without_a_roster_no_inside_outside_claim_is_made() {
        let r = run(
            &[thread("1", "Zed", "2026-10-12T09:00:00Z", vec![])],
            HashMap::new(),
        );
        assert_eq!(r.outside_share, None);
        assert!(r.notes.iter().any(|n| n.contains("No team roster")));
    }

    #[test]
    fn after_hours_uses_the_posters_own_timezone() {
        // Tue 13 Oct 02:00 UTC: 10:00 in Manila, but 02:00 on a UTC viewer's clock.
        let threads = vec![thread("1", "Ben", "2026-10-13T02:00:00Z", vec![])];
        let unset = run(&threads, HashMap::new());
        assert_eq!(unset.after_hours_share, Some(1.0));
        let people = vec![PersonProfile {
            name: "Ben".into(),
            timezone: Some("Asia/Manila".into()),
            ..Default::default()
        }];
        let rosters = HashMap::new();
        let set = signals(&SignalsInput {
            threads: &threads,
            rosters: &rosters,
            people: &people,
            now: now(),
            weeks: 4,
            tz_offset_minutes: 0,
        });
        assert_eq!(set.after_hours_share, Some(0.0));
    }

    #[test]
    fn weeks_are_complete_and_threads_outside_the_window_are_ignored() {
        let threads = vec![
            thread("1", "Zed", "2026-10-12T09:00:00Z", vec![]), // this week (Mon 12 Oct)
            thread("2", "Zed", "2026-08-01T09:00:00Z", vec![]), // far outside 4 weeks
        ];
        let r = run(&threads, HashMap::new());
        assert_eq!(r.threads, 1);
        let last = r.weekly.last().unwrap();
        assert_eq!((last.week_start.as_str(), last.threads), ("2026-10-12", 1));
        assert!(
            r.weekly.iter().any(|w| w.threads == 0),
            "quiet weeks show as zero"
        );
        assert!(r
            .weekly
            .windows(2)
            .all(|p| p[0].week_start < p[1].week_start));
    }

    #[test]
    fn urgent_threads_are_counted() {
        let mut th = thread("1", "Zed", "2026-10-12T09:00:00Z", vec![]);
        th.urgent = true;
        let r = run(&[th], HashMap::new());
        assert_eq!(r.urgent, 1);
        assert!(r.facts.iter().any(|f| f.contains("urgent")));
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration_text(30.0), "30 minutes");
        assert_eq!(duration_text(180.0), "3.0 hours");
        assert_eq!(duration_text(3.0 * 1440.0), "3.0 days");
    }
}
