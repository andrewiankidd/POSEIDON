//! Microsoft Teams channels, read through Microsoft Graph with the signed-in user's own delegated
//! token (see the `oauth` module for the device-code sign-in and the owner's single-tenant app
//! registration it uses).
//!
//! Read-only, and metadata only: for each conversation it keeps who started it, who answered and
//! when, and discards the message text. That is both a privacy choice (colleagues' words never
//! land in the database) and all the capacity picture needs.
//!
//! Graph returns a channel's *root* posts from `/messages` and each thread's answers from
//! `/messages/{id}/replies`, so a thread costs one extra call. Throttling (429/503) is retried with
//! the server's `Retry-After`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use poseidon_core::{ChannelConfig, ChannelThread, ThreadReply};
use serde::Deserialize;

use crate::ProviderError;

pub const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// What the connector asks the owner's app registration for: read channel messages, and list
/// teams / channels. Delegated, so it only ever sees what the signed-in user can see.
pub const TEAMS_SCOPE: &str = "https://graph.microsoft.com/ChannelMessage.Read.All https://graph.microsoft.com/Team.ReadBasic.All https://graph.microsoft.com/Channel.ReadBasic.All offline_access";

/// Pages of roots read per channel per run - a ceiling so one noisy channel can't run away.
const MAX_ROOT_PAGES: usize = 40;
/// Pages of replies read per thread.
const MAX_REPLY_PAGES: usize = 10;
const PAGE: usize = 50;
const MAX_ATTEMPTS: u32 = 4;

// ── wire types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MessagePage {
    #[serde(default)]
    value: Vec<GraphMessage>,
    #[serde(default, rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessage {
    id: String,
    #[serde(default)]
    message_type: Option<String>,
    #[serde(default)]
    created_date_time: Option<DateTime<Utc>>,
    #[serde(default)]
    last_modified_date_time: Option<DateTime<Utc>>,
    #[serde(default)]
    deleted_date_time: Option<DateTime<Utc>>,
    #[serde(default)]
    importance: Option<String>,
    #[serde(default)]
    from: Option<GraphFrom>,
    #[serde(default)]
    web_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphFrom {
    #[serde(default)]
    user: Option<GraphIdentity>,
    #[serde(default)]
    application: Option<GraphIdentity>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphIdentity {
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChannelInfo {
    #[serde(default, rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphErrorBody {
    #[serde(default)]
    error: Option<GraphErrorDetail>,
}

#[derive(Debug, Deserialize)]
struct GraphErrorDetail {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

// ── normalisation (pure, unit-tested) ────────────────────────────────────────

/// A message reduced to what the report needs - no text.
#[derive(Debug, Clone, PartialEq)]
struct Msg {
    id: String,
    author: String,
    automated: bool,
    created: DateTime<Utc>,
    modified: DateTime<Utc>,
    urgent: bool,
    url: Option<String>,
}

/// Reduce a Graph message, or drop it: system events ("X joined the team"), deleted posts and
/// anything without a timestamp are not conversation.
fn normalise_message(m: &GraphMessage) -> Option<Msg> {
    if m.deleted_date_time.is_some() {
        return None;
    }
    if m.message_type
        .as_deref()
        .is_some_and(|t| !t.eq_ignore_ascii_case("message"))
    {
        return None;
    }
    let created = m.created_date_time?;
    let from = m.from.as_ref();
    let user = from
        .and_then(|f| f.user.as_ref())
        .and_then(|u| u.display_name.as_deref())
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let app = from
        .and_then(|f| f.application.as_ref())
        .and_then(|u| u.display_name.as_deref())
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let (author, automated) = match (user, app) {
        (Some(u), _) => (u.to_string(), false),
        (None, Some(a)) => (a.to_string(), true),
        // Connector / webhook posts carry neither; they are not a person.
        (None, None) => ("(automated)".to_string(), true),
    };
    Some(Msg {
        id: m.id.clone(),
        author,
        automated,
        created,
        modified: m.last_modified_date_time.unwrap_or(created),
        urgent: m
            .importance
            .as_deref()
            .is_some_and(|i| i.eq_ignore_ascii_case("urgent")),
        url: m.web_url.clone().filter(|u| u.starts_with("https://")),
    })
}

/// Assemble a thread from its root and the answers it got.
fn build_thread(
    team: &str,
    channel: &ChannelConfig,
    channel_name: &str,
    root: Msg,
    replies: Vec<Msg>,
) -> ChannelThread {
    let mut replies: Vec<ThreadReply> = replies
        .into_iter()
        .map(|r| ThreadReply {
            author: r.author,
            at: r.created,
        })
        .collect();
    replies.sort_by_key(|r| r.at);
    replies.truncate(500);
    let last = replies
        .iter()
        .map(|r| r.at)
        .chain([root.modified, root.created])
        .max()
        .unwrap_or(root.created);
    ChannelThread {
        id: root.id,
        team: team.to_string(),
        channel_key: channel.key(),
        channel_name: channel_name.to_string(),
        author: root.author,
        automated: root.automated,
        created_at: root.created,
        last_activity_at: last,
        urgent: root.urgent,
        web_url: root.url,
        replies,
    }
}

/// The first URL for a channel's root posts, newest activity first and no older than `since`.
fn roots_url(channel: &ChannelConfig, since: DateTime<Utc>, filtered: bool) -> String {
    let enc = |s: &str| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
    let mut url = format!(
        "{GRAPH_BASE}/teams/{}/channels/{}/messages?$top={PAGE}",
        enc(&channel.team_id),
        enc(&channel.channel_id)
    );
    if filtered {
        url.push_str(&format!(
            "&$orderby={}&$filter={}",
            enc("lastModifiedDateTime desc"),
            enc(&format!(
                "lastModifiedDateTime gt {}",
                since.format("%Y-%m-%dT%H:%M:%SZ")
            ))
        ));
    }
    url
}

// ── the client ───────────────────────────────────────────────────────────────

/// A Graph client holding one user's access token.
pub struct GraphClient {
    http: reqwest::Client,
    token: String,
}

impl GraphClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            token: access_token.into(),
        }
    }

    /// GET and decode, retrying throttling with the server's `Retry-After`.
    async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, ProviderError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let resp = self.http.get(url).bearer_auth(&self.token).send().await?;
            let status = resp.status().as_u16();
            if matches!(status, 429 | 503 | 504) && attempt < MAX_ATTEMPTS {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(1 << attempt)
                    .clamp(1, 30);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            if resp.status().is_success() {
                return Ok(resp.json::<T>().await?);
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(graph_error(status, url, &body));
        }
    }

    /// Cheap proof the sign-in works: the signed-in user's joined-team count.
    pub async fn check(&self) -> Result<usize, ProviderError> {
        #[derive(Deserialize)]
        struct Teams {
            #[serde(default)]
            value: Vec<serde_json::Value>,
        }
        let t: Teams = self
            .get(&format!("{GRAPH_BASE}/me/joinedTeams?$select=id"))
            .await?;
        Ok(t.value.len())
    }

    /// A channel's display name, when Graph will tell us.
    pub async fn channel_name(&self, channel: &ChannelConfig) -> Option<String> {
        let enc = |s: &str| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
        let url = format!(
            "{GRAPH_BASE}/teams/{}/channels/{}?$select=displayName",
            enc(&channel.team_id),
            enc(&channel.channel_id)
        );
        self.get::<ChannelInfo>(&url)
            .await
            .ok()
            .and_then(|c| c.display_name)
            .filter(|n| !n.trim().is_empty())
    }

    /// Every page of a message list, up to `max_pages`.
    async fn pages(
        &self,
        first: String,
        max_pages: usize,
    ) -> Result<Vec<GraphMessage>, ProviderError> {
        let mut out = Vec::new();
        let mut next = Some(first);
        let mut n = 0;
        while let Some(url) = next.take() {
            if n >= max_pages {
                break;
            }
            n += 1;
            let page: MessagePage = self.get(&url).await?;
            out.extend(page.value);
            next = page.next_link.filter(|l| l.starts_with(GRAPH_BASE));
        }
        Ok(out)
    }

    /// The threads active in a channel since `since`: each root post with its answers.
    /// `team` is the POSEIDON team whose profile lists the channel.
    pub async fn fetch_threads(
        &self,
        team: &str,
        channel: &ChannelConfig,
        since: DateTime<Utc>,
    ) -> Result<(String, Vec<ChannelThread>), ProviderError> {
        let name = match &channel.name {
            Some(n) => n.clone(),
            None => self
                .channel_name(channel)
                .await
                .unwrap_or_else(|| "(channel)".to_string()),
        };
        // Newest-activity-first with a server-side cut-off; some tenants reject the filter, so
        // fall back to paging unfiltered and cutting off locally.
        let roots = match self
            .pages(roots_url(channel, since, true), MAX_ROOT_PAGES)
            .await
        {
            Ok(r) => r,
            Err(ProviderError::Api { status: 400, .. }) => {
                let all = self
                    .pages(roots_url(channel, since, false), MAX_ROOT_PAGES)
                    .await?;
                all.into_iter()
                    .filter(|m| {
                        m.last_modified_date_time
                            .or(m.created_date_time)
                            .is_some_and(|t| t >= since)
                    })
                    .collect()
            }
            Err(e) => return Err(e),
        };
        let enc = |s: &str| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
        let mut threads = Vec::new();
        for raw in &roots {
            let Some(root) = normalise_message(raw) else {
                continue;
            };
            let url = format!(
                "{GRAPH_BASE}/teams/{}/channels/{}/messages/{}/replies?$top={PAGE}",
                enc(&channel.team_id),
                enc(&channel.channel_id),
                enc(&root.id)
            );
            // A thread whose answers can't be read is still a thread; record it as such rather
            // than losing the ask.
            let replies = self
                .pages(url, MAX_REPLY_PAGES)
                .await
                .unwrap_or_default()
                .iter()
                .filter_map(normalise_message)
                .collect();
            threads.push(build_thread(team, channel, &name, root, replies));
        }
        Ok((name, threads))
    }
}

/// Turn a Graph error response into something the user can act on.
fn graph_error(status: u16, url: &str, body: &str) -> ProviderError {
    let detail = serde_json::from_str::<GraphErrorBody>(body)
        .ok()
        .and_then(|b| b.error);
    let code = detail
        .as_ref()
        .and_then(|d| d.code.clone())
        .unwrap_or_default();
    let message = detail.and_then(|d| d.message).unwrap_or_default();
    match status {
        401 => ProviderError::NotSignedIn(
            "the Teams sign-in was rejected or has expired - connect again".into(),
        ),
        403 => ProviderError::Api {
            status,
            url: url.to_string(),
            body: format!(
                "Teams refused access ({code}). The app registration needs the ChannelMessage.Read.All delegated permission with admin consent, and you must be a member of the channel. {}",
                message.chars().take(160).collect::<String>()
            ),
        },
        404 => ProviderError::NotFound(format!(
            "that Teams channel was not found, or you are not a member ({code})"
        )),
        _ => ProviderError::Api {
            status,
            url: url.to_string(),
            body: format!("{code} {}", message.chars().take(200).collect::<String>())
                .trim()
                .to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn channel() -> ChannelConfig {
        ChannelConfig {
            team_id: "0a1b2c3d-0000-1111-2222-333344445555".into(),
            channel_id: "19:abc@thread.tacv2".into(),
            name: None,
        }
    }

    fn parse(json: &str) -> GraphMessage {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_person_post_becomes_a_message_with_their_name_and_urgency() {
        let m = normalise_message(&parse(
            r#"{"id":"1","messageType":"message","createdDateTime":"2026-10-05T09:00:00.123Z",
                "lastModifiedDateTime":"2026-10-05T09:30:00Z","importance":"urgent",
                "from":{"user":{"id":"x","displayName":" Ana Example "}},
                "webUrl":"https://teams.microsoft.com/l/message/19:abc/1"}"#,
        ))
        .unwrap();
        assert_eq!(m.author, "Ana Example");
        assert!(!m.automated && m.urgent);
        assert_eq!(m.created, t("2026-10-05T09:00:00.123Z"));
        assert_eq!(m.modified, t("2026-10-05T09:30:00Z"));
        assert!(m.url.is_some());
    }

    #[test]
    fn bots_and_connectors_are_automated_not_people() {
        let app = normalise_message(&parse(
            r#"{"id":"2","messageType":"message","createdDateTime":"2026-10-05T09:00:00Z",
                "from":{"application":{"displayName":"Alert Bot"}}}"#,
        ))
        .unwrap();
        assert!(app.automated);
        assert_eq!(app.author, "Alert Bot");
        let hook = normalise_message(&parse(
            r#"{"id":"3","messageType":"message","createdDateTime":"2026-10-05T09:00:00Z","from":null}"#,
        ))
        .unwrap();
        assert!(hook.automated);
    }

    #[test]
    fn system_events_deleted_posts_and_undated_posts_are_not_conversation() {
        assert!(normalise_message(&parse(
            r#"{"id":"4","messageType":"systemEventMessage","createdDateTime":"2026-10-05T09:00:00Z"}"#
        ))
        .is_none());
        assert!(normalise_message(&parse(
            r#"{"id":"5","messageType":"message","createdDateTime":"2026-10-05T09:00:00Z",
                "deletedDateTime":"2026-10-06T09:00:00Z"}"#
        ))
        .is_none());
        assert!(normalise_message(&parse(r#"{"id":"6","messageType":"message"}"#)).is_none());
    }

    #[test]
    fn a_thread_carries_its_replies_in_order_and_its_latest_activity() {
        let root = normalise_message(&parse(
            r#"{"id":"r","messageType":"message","createdDateTime":"2026-10-05T09:00:00Z",
                "from":{"user":{"displayName":"Ana"}}}"#,
        ))
        .unwrap();
        let reply = |id: &str, who: &str, at: &str| {
            normalise_message(&parse(&format!(
                r#"{{"id":"{id}","messageType":"message","createdDateTime":"{at}",
                    "from":{{"user":{{"displayName":"{who}"}}}}}}"#
            )))
            .unwrap()
        };
        let th = build_thread(
            "Platform",
            &channel(),
            "General",
            root,
            vec![
                reply("b", "Cat", "2026-10-06T08:00:00Z"),
                reply("a", "Ben", "2026-10-05T10:00:00Z"),
            ],
        );
        assert_eq!(th.replies.len(), 2);
        assert_eq!(th.replies[0].author, "Ben", "oldest first");
        assert_eq!(th.last_activity_at, t("2026-10-06T08:00:00Z"));
        assert_eq!(th.channel_key, channel().key());
        assert_eq!(th.first_reply_at(), Some(t("2026-10-05T10:00:00Z")));
    }

    #[test]
    fn the_roots_url_encodes_ids_and_the_cut_off() {
        let u = roots_url(&channel(), t("2026-09-01T00:00:00Z"), true);
        assert!(u.starts_with(GRAPH_BASE));
        assert!(u.contains("19%3Aabc%40thread%2Etacv2"), "{u}");
        assert!(u.contains("$top=50"));
        assert!(
            u.contains("lastModifiedDateTime%20gt%202026%2D09%2D01T00%3A00%3A00Z"),
            "{u}"
        );
        assert!(!roots_url(&channel(), t("2026-09-01T00:00:00Z"), false).contains("filter"));
    }

    #[test]
    fn a_page_exposes_its_next_link() {
        let p: MessagePage = serde_json::from_str(
            r#"{"value":[],"@odata.nextLink":"https://graph.microsoft.com/v1.0/next?$skiptoken=1"}"#,
        )
        .unwrap();
        assert!(p.next_link.is_some());
    }

    #[test]
    fn errors_explain_what_to_do() {
        match graph_error(
            403,
            "u",
            r#"{"error":{"code":"Forbidden","message":"nope"}}"#,
        ) {
            ProviderError::Api { body, .. } => {
                assert!(body.contains("ChannelMessage.Read.All") && body.contains("Forbidden"));
            }
            e => panic!("{e}"),
        }
        assert!(matches!(
            graph_error(401, "u", ""),
            ProviderError::NotSignedIn(_)
        ));
        assert!(matches!(
            graph_error(404, "u", "{}"),
            ProviderError::NotFound(_)
        ));
        assert!(matches!(
            graph_error(500, "u", "x"),
            ProviderError::Api { status: 500, .. }
        ));
    }
}
