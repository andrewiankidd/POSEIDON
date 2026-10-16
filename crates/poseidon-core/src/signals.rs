//! Signals: demand that arrives in a chat channel rather than the backlog. The first source is a
//! Microsoft Teams channel; everything here is provider-neutral metadata about *conversations*
//! (who started a thread, who answered, how long it took) and deliberately carries no message text,
//! so colleagues' words never land in the database.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A chat channel a team watches for incoming asks (a Teams channel today).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// The Teams team's (group's) id.
    pub team_id: String,
    /// The channel's thread id (`19:...@thread.tacv2`).
    pub channel_id: String,
    /// A friendly name, filled in when the channel is first read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChannelConfig {
    /// The stable key a channel's threads are stored under.
    pub fn key(&self) -> String {
        format!("{}/{}", self.team_id, self.channel_id)
    }
}

/// How POSEIDON signs in to Microsoft Graph for Teams: the one-off single-tenant app registration
/// the owner made (a public client - there is no secret). Neither value is sensitive, so they
/// live in the config rather than the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TeamsConnector {
    /// The app registration's Application (client) id.
    pub client_id: String,
    /// The directory (tenant) id, or a verified domain.
    pub tenant_id: String,
}

impl TeamsConnector {
    pub fn is_empty(&self) -> bool {
        self.client_id.is_empty() && self.tenant_id.is_empty()
    }

    pub fn is_configured(&self) -> bool {
        !self.client_id.is_empty() && !self.tenant_id.is_empty()
    }

    /// Keep only plausible ids. Both end up in URLs and form posts, and arrive from untrusted
    /// places (an imported YAML, the API), so anything other than a GUID (client) or a GUID /
    /// domain (tenant) is dropped.
    pub fn sanitized(self) -> Self {
        let guid = |s: &str| {
            let s = s.trim().to_ascii_lowercase();
            let ok = s.len() == 36
                && s.chars().enumerate().all(|(i, c)| match i {
                    8 | 13 | 18 | 23 => c == '-',
                    _ => c.is_ascii_hexdigit(),
                });
            ok.then_some(s)
        };
        let tenant = |s: &str| {
            let s = s.trim().to_ascii_lowercase();
            let ok = !s.is_empty()
                && s.len() <= 253
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
            ok.then_some(s)
        };
        Self {
            client_id: guid(&self.client_id).unwrap_or_default(),
            tenant_id: tenant(&self.tenant_id).unwrap_or_default(),
        }
    }
}

/// One answer in a thread: who and when - never what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadReply {
    pub author: String,
    pub at: DateTime<Utc>,
}

/// A conversation: the post that started it and the answers it got.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelThread {
    /// The root message id.
    pub id: String,
    /// The POSEIDON team whose profile lists this channel.
    pub team: String,
    /// `<team id>/<channel id>`.
    pub channel_key: String,
    pub channel_name: String,
    pub author: String,
    /// True for a bot / connector / app post (alert feeds, workflows) rather than a person.
    pub automated: bool,
    pub created_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    /// Marked urgent by whoever posted it.
    pub urgent: bool,
    pub web_url: Option<String>,
    pub replies: Vec<ThreadReply>,
}

impl ChannelThread {
    /// When the first answer from someone other than the poster arrived.
    pub fn first_reply_at(&self) -> Option<DateTime<Utc>> {
        self.replies
            .iter()
            .filter(|r| !r.author.eq_ignore_ascii_case(&self.author))
            .map(|r| r.at)
            .min()
    }
}

// ── The report ──────────────────────────────────────────────────────────────

/// Threads per ISO week.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalWeek {
    pub week_start: String,
    pub threads: u32,
    pub replies: u32,
}

/// One watched channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelSummary {
    pub name: String,
    pub threads: u32,
    pub automated: u32,
    pub unanswered: u32,
}

/// Someone who starts threads (an asker) or answers them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalPerson {
    pub name: String,
    pub count: u32,
    /// On the team's roster.
    pub on_team: bool,
}

/// How quickly threads get a first answer from someone other than the poster.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResponseTimes {
    /// Threads that got an answer.
    pub answered: u32,
    pub median_minutes: Option<f64>,
    pub p85_minutes: Option<f64>,
}

/// An open conversation nobody has answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitingThread {
    pub channel: String,
    pub author: String,
    pub age_hours: i64,
    pub urgent: bool,
    pub url: Option<String>,
}

/// Everything the Interruptions card shows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SignalsReport {
    /// False when no channel has been read yet.
    pub available: bool,
    pub generated_at: Option<DateTime<Utc>>,
    pub weeks: u32,
    /// Human-started threads in the window.
    pub threads: u32,
    /// Threads started by bots and connectors, left out of the human numbers.
    pub automated: u32,
    pub replies: u32,
    /// Share of human threads started by someone outside the team - an ask from elsewhere.
    pub outside_share: Option<f64>,
    /// Share of human threads started outside 09:00-18:00 Monday-Friday (poster's own timezone
    /// when known, else the viewer's).
    pub after_hours_share: Option<f64>,
    pub urgent: u32,
    pub weekly: Vec<SignalWeek>,
    pub channels: Vec<ChannelSummary>,
    pub response: ResponseTimes,
    /// Open threads nobody has answered, oldest first.
    pub waiting: Vec<WaitingThread>,
    pub unanswered_over_day: u32,
    /// Who starts the threads.
    pub askers: Vec<SignalPerson>,
    /// Who answers them - team members' load of the replying.
    pub responders: Vec<SignalPerson>,
    /// The share of all replies written by the single busiest responder.
    pub top_responder_share: Option<f64>,
    pub facts: Vec<String>,
    pub notes: Vec<String>,
}

/// Pull the team and channel ids out of a Teams channel link
/// (`https://teams.microsoft.com/l/channel/19%3A...%40thread.tacv2/Name?groupId=<guid>&tenantId=<guid>`).
/// `None` when it isn't one. Pure - the name is left empty until the channel is read.
pub fn parse_channel_link(link: &str) -> Option<ChannelConfig> {
    let link = link.trim();
    let (path, query) = link.split_once('?')?;
    let after = path.split("/l/channel/").nth(1)?;
    let raw_channel = after.split('/').next()?;
    let channel_id = percent_decode(raw_channel);
    if !channel_id.starts_with("19:") || !channel_id.contains("@thread") {
        return None;
    }
    let team_id = query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        k.eq_ignore_ascii_case("groupId").then(|| percent_decode(v))
    })?;
    let guid_like = !team_id.is_empty()
        && team_id.len() <= 64
        && team_id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    guid_like.then(|| ChannelConfig {
        team_id: team_id.to_ascii_lowercase(),
        channel_id,
        name: None,
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Clean a channel list from an untrusted place: parse links, drop what isn't a channel,
/// dedupe by key, cap the count.
pub fn sanitize_channels(channels: Vec<ChannelConfig>) -> Vec<ChannelConfig> {
    let mut out: Vec<ChannelConfig> = Vec::new();
    for mut c in channels {
        c.team_id = c.team_id.trim().to_ascii_lowercase();
        c.channel_id = c.channel_id.trim().to_string();
        c.name = c
            .name
            .map(|n| n.trim().chars().take(80).collect::<String>())
            .filter(|n| !n.is_empty());
        let ok = !c.team_id.is_empty()
            && c.team_id.len() <= 64
            && c.team_id
                .chars()
                .all(|ch| ch.is_ascii_hexdigit() || ch == '-')
            && c.channel_id.starts_with("19:")
            && c.channel_id.len() <= 200
            && !c.channel_id.chars().any(char::is_whitespace);
        if ok && !out.iter().any(|x| x.key() == c.key()) {
            out.push(c);
        }
    }
    out.truncate(50);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str = "https://teams.microsoft.com/l/channel/19%3Aabc123DEF%40thread.tacv2/General?groupId=0A1B2C3D-0000-1111-2222-333344445555&tenantId=6c891bd8-96af-4ed3-93bb-155d620876ae";

    #[test]
    fn a_channel_link_yields_its_team_and_channel_ids() {
        let c = parse_channel_link(LINK).unwrap();
        assert_eq!(c.channel_id, "19:abc123DEF@thread.tacv2");
        assert_eq!(c.team_id, "0a1b2c3d-0000-1111-2222-333344445555");
        assert_eq!(
            c.key(),
            "0a1b2c3d-0000-1111-2222-333344445555/19:abc123DEF@thread.tacv2"
        );
    }

    #[test]
    fn things_that_are_not_channel_links_are_rejected() {
        assert!(parse_channel_link("https://example.com/").is_none());
        assert!(
            parse_channel_link("https://teams.microsoft.com/l/channel/xyz/Gen?groupId=abc")
                .is_none()
        );
        assert!(parse_channel_link(
            "https://teams.microsoft.com/l/channel/19%3Aa%40thread.tacv2/G?tenantId=1"
        )
        .is_none());
        assert!(parse_channel_link(
            "https://teams.microsoft.com/l/channel/19%3Aa%40thread.tacv2/G?groupId=not%20a%20guid"
        )
        .is_none());
        assert!(parse_channel_link("").is_none());
    }

    #[test]
    fn percent_decoding_handles_edges() {
        assert_eq!(percent_decode("a%3Ab"), "a:b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4"), "%4");
    }

    #[test]
    fn sanitising_channels_drops_junk_and_duplicates() {
        let good = ChannelConfig {
            team_id: " ABC-123 ".into(),
            channel_id: "19:x@thread.tacv2".into(),
            name: Some("  Ops  ".into()),
        };
        let out = sanitize_channels(vec![
            good.clone(),
            good,
            ChannelConfig {
                team_id: "".into(),
                channel_id: "19:y@thread.tacv2".into(),
                name: None,
            },
            ChannelConfig {
                team_id: "abc".into(),
                channel_id: "not a channel".into(),
                name: None,
            },
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].team_id, "abc-123");
        assert_eq!(out[0].name.as_deref(), Some("Ops"));
    }

    #[test]
    fn the_connector_keeps_only_guids_and_domains() {
        let ok = TeamsConnector {
            client_id: " 04B07795-8DDB-461A-BBEE-02F9E1BF7B46 ".into(),
            tenant_id: "Contoso.onmicrosoft.com".into(),
        }
        .sanitized();
        assert_eq!(ok.client_id, "04b07795-8ddb-461a-bbee-02f9e1bf7b46");
        assert_eq!(ok.tenant_id, "contoso.onmicrosoft.com");
        assert!(ok.is_configured());
        let bad = TeamsConnector {
            client_id: "not-a-guid".into(),
            tenant_id: "x; rm -rf /".into(),
        }
        .sanitized();
        assert!(!bad.is_configured() && bad.is_empty());
    }

    #[test]
    fn a_threads_first_reply_ignores_the_posters_own_follow_ups() {
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let thread = ChannelThread {
            id: "1".into(),
            team: "T".into(),
            channel_key: "k".into(),
            channel_name: "c".into(),
            author: "Ana".into(),
            automated: false,
            created_at: t("2026-10-05T09:00:00Z"),
            last_activity_at: t("2026-10-05T12:00:00Z"),
            urgent: false,
            web_url: None,
            replies: vec![
                ThreadReply {
                    author: "ana".into(),
                    at: t("2026-10-05T09:05:00Z"),
                },
                ThreadReply {
                    author: "Ben".into(),
                    at: t("2026-10-05T10:30:00Z"),
                },
            ],
        };
        assert_eq!(thread.first_reply_at(), Some(t("2026-10-05T10:30:00Z")));
    }
}
