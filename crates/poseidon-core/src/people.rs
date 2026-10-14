use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::member::identity_tokens;

/// Most people / leave ranges one owner may store - a sanity bound on imported config.
pub const MAX_PROFILES: usize = 200;
pub const MAX_LEAVE_RANGES: usize = 200;

/// A stretch of days a person was away (annual leave, a public holiday, sick leave), inclusive
/// at both ends. Used so working-hours and capacity numbers don't read an absence as a lull.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaveRange {
    pub start: NaiveDate,
    pub end: NaiveDate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// What POSEIDON should know about a person that the tracker does not say: where they work and
/// when they were away. Matched to commit authors by the same identity tokens as team rosters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonProfile {
    /// Display name as shown on commits / pull requests.
    pub name: String,
    /// Sign-in / email identity, when known; makes the match robust to display-name variants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unique_name: Option<String>,
    /// IANA timezone name (`Europe/London`). `None` = read their hours in the viewer's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leave: Vec<LeaveRange>,
}

impl PersonProfile {
    /// The lowercase tokens that identify this person (see [`identity_tokens`]).
    pub fn tokens(&self) -> Vec<String> {
        identity_tokens(Some(&self.name), self.unique_name.as_deref())
    }

    /// True when the profile carries nothing worth storing.
    pub fn is_blank(&self) -> bool {
        self.timezone.is_none() && self.leave.is_empty()
    }
}

/// Clean a list of profiles that arrived from an untrusted place (the API, an imported YAML):
/// trim names, drop profiles with no name or nothing to say, put reversed leave ranges the right
/// way round, order them, dedupe by name, and cap the sizes. `valid_timezone` decides which
/// timezone names are real - it lives with the clock code, not in this IO-free crate - and an
/// unknown name is dropped rather than stored.
pub fn sanitize_people(
    people: Vec<PersonProfile>,
    valid_timezone: impl Fn(&str) -> bool,
) -> Vec<PersonProfile> {
    let mut out: Vec<PersonProfile> = Vec::new();
    for mut p in people {
        p.name = p.name.trim().to_string();
        if p.name.is_empty() {
            continue;
        }
        p.unique_name = p
            .unique_name
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty());
        p.timezone = p
            .timezone
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty() && valid_timezone(t));
        for l in &mut p.leave {
            if l.end < l.start {
                std::mem::swap(&mut l.start, &mut l.end);
            }
            l.label = l
                .label
                .take()
                .map(|s| s.trim().chars().take(60).collect::<String>())
                .filter(|s| !s.is_empty());
        }
        p.leave.sort_by_key(|l| (l.start, l.end));
        p.leave.dedup();
        p.leave.truncate(MAX_LEAVE_RANGES);
        if p.is_blank() {
            continue;
        }
        // The last entry for a name wins.
        out.retain(|x| !x.name.eq_ignore_ascii_case(&p.name));
        out.push(p);
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out.truncate(MAX_PROFILES);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn known(tz: &str) -> bool {
        tz == "Europe/London" || tz == "Asia/Manila"
    }

    #[test]
    fn sanitising_drops_unknown_zones_blank_profiles_and_fixes_reversed_leave() {
        let clean = sanitize_people(
            vec![
                PersonProfile {
                    name: "  Ana  ".into(),
                    timezone: Some("Mars/Olympus".into()),
                    leave: vec![LeaveRange {
                        start: d("2026-08-14"),
                        end: d("2026-08-03"),
                        label: Some(" Holiday ".into()),
                    }],
                    ..Default::default()
                },
                PersonProfile {
                    name: "Ben".into(),
                    timezone: Some("Asia/Manila".into()),
                    ..Default::default()
                },
                PersonProfile {
                    name: "Nothing".into(),
                    timezone: Some("Nowhere".into()),
                    ..Default::default()
                },
                PersonProfile {
                    name: "".into(),
                    timezone: Some("Europe/London".into()),
                    ..Default::default()
                },
            ],
            known,
        );
        let names: Vec<&str> = clean.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Ana", "Ben"]);
        assert_eq!(clean[0].timezone, None, "an unknown zone is not stored");
        assert_eq!(clean[0].leave[0].start, d("2026-08-03"));
        assert_eq!(clean[0].leave[0].end, d("2026-08-14"));
        assert_eq!(clean[0].leave[0].label.as_deref(), Some("Holiday"));
        assert_eq!(clean[1].timezone.as_deref(), Some("Asia/Manila"));
    }

    #[test]
    fn the_last_profile_for_a_name_wins() {
        let clean = sanitize_people(
            vec![
                PersonProfile {
                    name: "Ana".into(),
                    timezone: Some("Europe/London".into()),
                    ..Default::default()
                },
                PersonProfile {
                    name: "ana".into(),
                    timezone: Some("Asia/Manila".into()),
                    ..Default::default()
                },
            ],
            known,
        );
        assert_eq!(clean.len(), 1);
        assert_eq!(clean[0].timezone.as_deref(), Some("Asia/Manila"));
    }
}
