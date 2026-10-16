//! The Microsoft Teams connector on the [`Service`]: sign-in (device code, against the owner's own
//! single-tenant app registration), reading the channels listed on each team's profile, and the
//! Interruptions report built from what was read.
//!
//! Read-only and metadata-only - see `poseidon_providers::teams`. Nothing here ever writes to
//! Teams, and no message text is stored.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use poseidon_core::{ChannelConfig, SignalsReport, TeamConfig, TeamsConnector};
use poseidon_providers::oauth::{self, OAuthApp, PollOutcome};
use poseidon_providers::teams::{GraphClient, TEAMS_SCOPE};
use poseidon_providers::DeviceCode;
use serde::Serialize;
use tracing::{info, warn};

use super::{read_cached_token, write_cached_token, CachedToken, Service, SigninState};

/// How far back a first read of a channel goes.
const THREAD_WINDOW_DAYS: i64 = 90;
/// A later read starts this far before the newest thing already stored, so answers that arrived
/// on recent threads are picked up.
const THREAD_LOOKBACK_DAYS: i64 = 14;
/// Collected at most this often by the background poll (the Refresh button forces it).
const THREAD_COLLECT_EVERY_HOURS: i64 = 3;

/// Where the Teams connector stands, for the Interruptions card.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TeamsStatus {
    /// Client + tenant id have been entered.
    pub configured: bool,
    /// A usable (or silently renewable) sign-in is cached.
    pub signed_in: bool,
    pub client_id: String,
    pub tenant_id: String,
    /// Channels listed across the owner's team profiles.
    pub channels: usize,
    /// When channels were last read successfully.
    pub last_collected: Option<String>,
    /// Why the last read failed, if it did.
    pub last_error: Option<String>,
}

/// The outcome of reading the channels.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TeamsRefresh {
    pub channels: usize,
    pub threads: usize,
    pub errors: Vec<String>,
}

impl Service {
    fn teams_token_path(&self) -> std::path::PathBuf {
        self.token_path().with_file_name("teams-oauth-token.json")
    }

    async fn teams_connector(&self) -> TeamsConnector {
        self.config
            .user_config(&self.owner)
            .await
            .map(|c| c.teams_connector)
            .unwrap_or_default()
    }

    /// A Graph access token from the cached sign-in, silently renewed when near expiry.
    async fn teams_access_token(&self, c: &TeamsConnector) -> Result<String, String> {
        let path = self.teams_token_path();
        let Some(cached) = read_cached_token(&path) else {
            return Err("not connected to Teams - connect first".to_string());
        };
        if cached.expires_at > Utc::now() + Duration::seconds(60) {
            return Ok(cached.access_token);
        }
        let Some(rt) = cached.refresh_token.as_deref() else {
            return Err("the Teams sign-in expired - connect again".to_string());
        };
        let app = OAuthApp {
            client_id: &c.client_id,
            scope: TEAMS_SCOPE,
        };
        match oauth::refresh_for(&reqwest::Client::new(), Some(&c.tenant_id), app, rt).await {
            Ok(ts) => {
                let fresh = CachedToken::from(ts);
                write_cached_token(&path, &fresh);
                Ok(fresh.access_token)
            }
            Err(e) => Err(format!("the Teams sign-in expired - connect again ({e})")),
        }
    }

    pub async fn teams_status(&self) -> TeamsStatus {
        let cfg = self
            .config
            .user_config(&self.owner)
            .await
            .unwrap_or_default();
        let c = cfg.teams_connector.clone();
        let channels = cfg.teams.iter().map(|t| t.channels.len()).sum();
        let last_collected = self
            .store
            .get_meta(&self.owner, "teams_collected_at")
            .await
            .ok()
            .flatten();
        let last_error = self
            .store
            .get_meta(&self.owner, "teams_last_error")
            .await
            .ok()
            .flatten()
            .filter(|e| !e.is_empty());
        TeamsStatus {
            configured: c.is_configured(),
            signed_in: c.is_configured() && read_cached_token(&self.teams_token_path()).is_some(),
            client_id: c.client_id,
            tenant_id: c.tenant_id,
            channels,
            last_collected,
            last_error,
        }
    }

    /// Save the app registration to sign in with. Validated; changing it discards the old
    /// sign-in (it belonged to a different app). Returns what was stored.
    pub async fn set_teams_connector(&self, c: TeamsConnector) -> anyhow::Result<TeamsConnector> {
        let before = self.teams_connector().await;
        let stored = self.config.set_teams_connector(&self.owner, c).await?;
        if stored != before {
            let _ = std::fs::remove_file(self.teams_token_path());
            let _ = self
                .store
                .set_meta(&self.owner, "teams_last_error", "")
                .await;
            self.teams_signins.lock().unwrap().remove(&self.owner);
        }
        Ok(stored)
    }

    /// Begin the device-code sign-in: returns the prompt at once ("open <url>, enter <code>");
    /// the grant completes on a background task whose progress is [`Self::teams_sign_in_status`].
    pub async fn start_teams_sign_in(&self) -> anyhow::Result<DeviceCode> {
        let c = self.teams_connector().await;
        if !c.is_configured() {
            anyhow::bail!("enter the app registration's client id and tenant id first");
        }
        let path = self.teams_token_path();
        let http = reqwest::Client::new();
        let app = OAuthApp {
            client_id: &c.client_id,
            scope: TEAMS_SCOPE,
        };
        let start = oauth::start_device_code_for(&http, Some(&c.tenant_id), app).await?;
        let prompt = DeviceCode {
            url: start.verification_uri.clone(),
            code: start.user_code.clone(),
        };
        self.teams_signins.lock().unwrap().insert(
            self.owner.clone(),
            SigninState::Pending {
                url: prompt.url.clone(),
                code: prompt.code.clone(),
            },
        );

        let signins = self.teams_signins.clone();
        let store = self.store.clone();
        let owner = self.owner.clone();
        let device_code = start.device_code;
        let mut wait = start.interval.max(1);
        let expires_in = start.expires_in.max(60);
        tokio::spawn(async move {
            let deadline = Utc::now() + Duration::seconds(expires_in as i64);
            let state = loop {
                if Utc::now() >= deadline {
                    break SigninState::Failed {
                        error: "sign-in timed out".to_string(),
                    };
                }
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                match oauth::poll_once_for(&http, Some(&c.tenant_id), &c.client_id, &device_code)
                    .await
                {
                    Ok(PollOutcome::Pending) => continue,
                    Ok(PollOutcome::SlowDown) => {
                        wait += 5;
                        continue;
                    }
                    Ok(PollOutcome::Ready(ts)) => {
                        write_cached_token(&path, &CachedToken::from(ts));
                        let _ = store.set_meta(&owner, "teams_last_error", "").await;
                        info!(owner = %owner, "Teams sign-in completed");
                        break SigninState::Done;
                    }
                    Ok(PollOutcome::Failed(msg)) => break SigninState::Failed { error: msg },
                    Err(e) => {
                        break SigninState::Failed {
                            error: e.to_string(),
                        }
                    }
                }
            };
            signins.lock().unwrap().insert(owner, state);
        });
        Ok(prompt)
    }

    pub fn teams_sign_in_status(&self) -> SigninState {
        self.teams_signins
            .lock()
            .unwrap()
            .get(&self.owner)
            .cloned()
            .unwrap_or(SigninState::Idle)
    }

    /// Read every watched channel now (the Refresh button), optionally for one team.
    pub async fn refresh_teams(&self, team: Option<&str>) -> anyhow::Result<TeamsRefresh> {
        let c = self.teams_connector().await;
        if !c.is_configured() {
            anyhow::bail!("Teams is not set up yet");
        }
        let token = self
            .teams_access_token(&c)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let teams: Vec<TeamConfig> = self
            .config
            .teams(&self.owner)
            .await?
            .into_iter()
            .filter(|t| team.is_none_or(|n| t.name.eq_ignore_ascii_case(n)))
            .collect();
        let mut out = TeamsRefresh::default();
        for t in &teams {
            let r = self.collect_team_threads(t, &token).await;
            out.channels += r.channels;
            out.threads += r.threads;
            out.errors.extend(r.errors);
        }
        self.record_teams_outcome(&out).await;
        Ok(out)
    }

    async fn record_teams_outcome(&self, r: &TeamsRefresh) {
        let _ = self
            .store
            .set_meta(&self.owner, "teams_last_error", &r.errors.join(" | "))
            .await;
        if r.errors.is_empty() && r.channels > 0 {
            let _ = self
                .store
                .set_meta(&self.owner, "teams_collected_at", &Utc::now().to_rfc3339())
                .await;
        }
    }

    /// The background hook from a poll: read a team's channels if the connector is set up and
    /// signed in and it has been a few hours. Best-effort - it never fails a poll.
    pub(super) async fn collect_team_channels(&self, team: &str) {
        let c = self.teams_connector().await;
        if !c.is_configured() {
            return;
        }
        let Some(cfg) = self
            .config
            .teams(&self.owner)
            .await
            .ok()
            .and_then(|ts| ts.into_iter().find(|t| t.name.eq_ignore_ascii_case(team)))
        else {
            return;
        };
        if cfg.channels.is_empty() {
            return;
        }
        let key = format!("threads_collected_at:{team}");
        let now = Utc::now();
        if let Ok(Some(last)) = self.store.get_meta(&self.owner, &key).await {
            if let Ok(t) = DateTime::parse_from_rfc3339(&last) {
                if now - t.with_timezone(&Utc) < Duration::hours(THREAD_COLLECT_EVERY_HOURS) {
                    return;
                }
            }
        }
        let Ok(token) = self.teams_access_token(&c).await else {
            return; // not connected (yet): the card says so; nothing to log every poll
        };
        let r = self.collect_team_threads(&cfg, &token).await;
        let _ = self
            .store
            .set_meta(&self.owner, &key, &now.to_rfc3339())
            .await;
        self.record_teams_outcome(&r).await;
        if !r.errors.is_empty() {
            warn!(team, errors = ?r.errors, "could not read some Teams channels");
        } else {
            info!(
                team,
                channels = r.channels,
                threads = r.threads,
                "read Teams channels"
            );
        }
    }

    /// Read one team's channels and store the threads. Per-channel failures are collected, not
    /// fatal, so one removed channel doesn't hide the rest.
    async fn collect_team_threads(&self, team: &TeamConfig, token: &str) -> TeamsRefresh {
        let now = Utc::now();
        let graph = GraphClient::new(token);
        let mut out = TeamsRefresh::default();
        let stored = self
            .store
            .list_threads(
                &self.owner,
                Some(&team.name),
                now - Duration::days(THREAD_WINDOW_DAYS + 30),
            )
            .await
            .unwrap_or_default();
        for ch in &team.channels {
            let latest = stored
                .iter()
                .filter(|t| t.channel_key == ch.key())
                .map(|t| t.last_activity_at)
                .max();
            let floor = now - Duration::days(THREAD_WINDOW_DAYS);
            let since = match latest {
                Some(l) => (l - Duration::days(THREAD_LOOKBACK_DAYS)).max(floor),
                None => floor,
            };
            match graph.fetch_threads(&team.name, ch, since).await {
                Ok((_, threads)) => {
                    out.channels += 1;
                    out.threads += threads.len();
                    if let Err(e) = self.store.upsert_threads(&self.owner, &threads).await {
                        out.errors
                            .push(format!("{}: could not store threads ({e})", label(ch)));
                    }
                }
                Err(e) => out.errors.push(format!("{}: {e}", label(ch))),
            }
        }
        // A channel taken off the profile stops being reported.
        let wanted: HashSet<String> = team.channels.iter().map(ChannelConfig::key).collect();
        let gone: HashSet<&str> = stored
            .iter()
            .map(|t| t.channel_key.as_str())
            .filter(|k| !wanted.contains(*k))
            .collect();
        for key in gone {
            let _ = self
                .store
                .delete_channel_threads(&self.owner, &team.name, key)
                .await;
        }
        let _ = self
            .store
            .prune_threads(&self.owner, now - Duration::days(THREAD_WINDOW_DAYS + 10))
            .await;
        out
    }

    /// The Interruptions report for a team scope: asks arriving in the watched channels, how fast
    /// they are answered, who answers.
    pub async fn signals(
        &self,
        team: Option<&str>,
        weeks: u32,
        tz_offset_minutes: i32,
    ) -> anyhow::Result<SignalsReport> {
        let weeks = weeks.clamp(1, 52);
        let now = Utc::now();
        let since = now - Duration::weeks(i64::from(weeks)) - Duration::days(1);
        let mut threads = self.store.list_threads(&self.owner, team, since).await?;
        if team.is_none() {
            // A channel listed on two teams' profiles is one conversation, not two.
            let mut seen: HashSet<(String, String)> = HashSet::new();
            threads.retain(|t| seen.insert((t.channel_key.clone(), t.id.clone())));
        }
        let rosters: HashMap<String, Vec<String>> = self.rosters_map().await;
        let people = self
            .config
            .user_config(&self.owner)
            .await
            .map(|c| c.people)
            .unwrap_or_default();
        Ok(poseidon_reports::signals::signals(
            &poseidon_reports::signals::SignalsInput {
                threads: &threads,
                rosters: &rosters,
                people: &people,
                now,
                weeks,
                tz_offset_minutes,
            },
        ))
    }
}

fn label(ch: &ChannelConfig) -> String {
    ch.name.clone().unwrap_or_else(|| {
        format!(
            "channel {}",
            ch.channel_id.chars().take(14).collect::<String>()
        )
    })
}
