//! Per-owner configuration store (multi-tenant).
//!
//! Configuration splits in two:
//! - **Instance settings** (`[server]` bind/port/poll_interval, telemetry) are
//!   read once from TOML/env at boot. They configure the *process*, so they can't
//!   be per-user. Held here as an immutable snapshot.
//! - **Per-owner config** (teams, rules, Doctor state, poll scope) is a
//!   [`UserConfig`] persisted as a JSON blob keyed by `owner` in the DB. Standalone
//!   has exactly one owner (`"default"`); a hosted deployment maps each
//!   authenticated user to their own row.
//!
//! There is no config file: an owner's config is created empty on first access
//! (configured via the UI or `config import`) and every edit persists to the DB.
//!
//! Reads are cached per owner (write-through) so a hot path doesn't hit the DB
//! every call. The `RwLock` is never held across an `.await`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use poseiden_core::{RuleSet, ServerConfig, TeamConfig, UserConfig, DEFAULT_OWNER};
use poseiden_store::Store;

#[derive(Clone)]
pub struct ConfigStore {
    store: Store,
    /// Instance-level settings, immutable after boot.
    instance: ServerConfig,
    /// Seed for the `default` owner when it has no stored config yet (the
    /// standalone bootstrap from TOML).
    seed: UserConfig,
    /// owner -> cached UserConfig (write-through to the DB).
    cache: Arc<RwLock<HashMap<String, UserConfig>>>,
}

impl ConfigStore {
    pub fn new(store: Store, instance: ServerConfig, seed: UserConfig) -> Self {
        Self {
            store,
            instance,
            seed,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Instance-level settings (bind/port/poll_interval).
    pub fn instance(&self) -> ServerConfig {
        self.instance.clone()
    }

    /// An owner's config, cache-through. Seeds `default` from the bootstrap and
    /// any other owner from an empty default, persisting the seed so the owner
    /// exists as a tenant (and the scheduler will poll it).
    pub async fn user_config(&self, owner: &str) -> anyhow::Result<UserConfig> {
        if let Some(c) = self.cache.read().unwrap().get(owner).cloned() {
            return Ok(c);
        }
        let cfg = match self.store.get_user_config(owner).await? {
            Some(c) => c,
            None => {
                let seeded = if owner == DEFAULT_OWNER {
                    self.seed.clone()
                } else {
                    UserConfig::default()
                };
                self.store.put_user_config(owner, &seeded).await?;
                seeded
            }
        };
        self.cache
            .write()
            .unwrap()
            .insert(owner.to_string(), cfg.clone());
        Ok(cfg)
    }

    /// Replace an owner's entire config (import / restore, replace semantics).
    /// Write-through: persists and refreshes the cache.
    pub async fn set_user_config(&self, owner: &str, cfg: UserConfig) -> anyhow::Result<()> {
        self.store.put_user_config(owner, &cfg).await?;
        self.cache.write().unwrap().insert(owner.to_string(), cfg);
        Ok(())
    }

    /// Load, mutate, and persist an owner's config in one shot. The closure's
    /// return value is passed back (e.g. "did anything change?").
    async fn update<R>(
        &self,
        owner: &str,
        f: impl FnOnce(&mut UserConfig) -> R,
    ) -> anyhow::Result<R> {
        let mut cfg = self.user_config(owner).await?;
        let r = f(&mut cfg);
        self.store.put_user_config(owner, &cfg).await?;
        self.cache.write().unwrap().insert(owner.to_string(), cfg);
        Ok(r)
    }

    pub async fn teams(&self, owner: &str) -> anyhow::Result<Vec<TeamConfig>> {
        Ok(self.user_config(owner).await?.teams)
    }

    /// Registered Doctor check keys for an owner.
    pub async fn registered_checks(&self, owner: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.user_config(owner).await?.doctor.checks)
    }

    /// Replace the registered-check set. Returns whether it changed.
    pub async fn set_registered_checks(
        &self,
        owner: &str,
        keys: Vec<String>,
    ) -> anyhow::Result<bool> {
        self.update(owner, |c| {
            if c.doctor.checks == keys {
                false
            } else {
                c.doctor.checks = keys;
                true
            }
        })
        .await
    }

    /// Append a team (deduped by name). `false` if the name already existed.
    pub async fn add_team(&self, owner: &str, team: TeamConfig) -> anyhow::Result<bool> {
        self.update(owner, move |c| {
            if c.teams
                .iter()
                .any(|t| t.name.eq_ignore_ascii_case(&team.name))
            {
                false
            } else {
                c.teams.push(team);
                true
            }
        })
        .await
    }

    /// Remove a team by name. `false` if no such team.
    pub async fn remove_team(&self, owner: &str, name: &str) -> anyhow::Result<bool> {
        self.update(owner, |c| {
            let before = c.teams.len();
            c.teams.retain(|t| !t.name.eq_ignore_ascii_case(name));
            c.teams.len() != before
        })
        .await
    }

    /// Replace an existing team (matched by `original`). `false` if none matched.
    pub async fn update_team(
        &self,
        owner: &str,
        original: &str,
        team: TeamConfig,
    ) -> anyhow::Result<bool> {
        self.update(owner, move |c| {
            match c
                .teams
                .iter_mut()
                .find(|t| t.name.eq_ignore_ascii_case(original))
            {
                Some(slot) => {
                    *slot = team;
                    true
                }
                None => false,
            }
        })
        .await
    }

    /// Replace the owner's default ruleset (`[rules]`).
    pub async fn set_rules(&self, owner: &str, rules: RuleSet) -> anyhow::Result<()> {
        self.update(owner, move |c| c.rules = rules).await
    }

    /// Toggle whether polls fetch all teams or just the active one.
    pub async fn set_poll_all_teams(&self, owner: &str, all: bool) -> anyhow::Result<()> {
        self.update(owner, move |c| c.poll_all_teams = all).await
    }

    /// Set (or clear) a team's rules override. `false` if no team matched.
    pub async fn set_team_rules(
        &self,
        owner: &str,
        name: &str,
        rules: Option<RuleSet>,
    ) -> anyhow::Result<bool> {
        self.update(owner, move |c| {
            match c
                .teams
                .iter_mut()
                .find(|t| t.name.eq_ignore_ascii_case(name))
            {
                Some(team) => {
                    team.rules = rules;
                    true
                }
                None => false,
            }
        })
        .await
    }
}
