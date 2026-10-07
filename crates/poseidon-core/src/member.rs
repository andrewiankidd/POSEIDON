use serde::{Deserialize, Serialize};

/// A person on a team, as the tracker's own team roster lists them. Used to limit
/// per-person reports to the people who actually belong to the team, so a project-wide
/// pull-request feed doesn't drag in one-off contributors from elsewhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    /// Display name as shown on pull requests (e.g. `Ana Example`).
    pub name: String,
    /// Sign-in / email-style identity (e.g. `ana.example@contoso.com`), when the
    /// tracker gives one. Matching on this survives a display name that differs between
    /// the roster and a pull request (`Jonathan` vs `Jon`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unique_name: Option<String>,
}

impl TeamMember {
    /// The lowercase tokens that identify this person: their display name, their
    /// sign-in identity, and that identity's local part (`ana.example`).
    pub fn tokens(&self) -> Vec<String> {
        identity_tokens(Some(&self.name), self.unique_name.as_deref())
    }
}

/// Lowercase match tokens for a person: display name, sign-in identity, and the
/// identity's local part. Shared by roster entries, PR authors and reviewers so they
/// meet on the same keys.
pub fn identity_tokens(name: Option<&str>, unique_name: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim().to_lowercase();
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    };
    if let Some(n) = name {
        push(n);
    }
    if let Some(u) = unique_name {
        push(u);
        if let Some((local, _)) = u.split_once('@') {
            push(local);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_cover_name_identity_and_local_part_case_insensitively() {
        let m = TeamMember {
            name: "Jon Reyes".into(),
            unique_name: Some("Jonathan.Reyes@contoso.com".into()),
        };
        assert_eq!(
            m.tokens(),
            vec!["jon reyes", "jonathan.reyes@contoso.com", "jonathan.reyes"]
        );
    }

    #[test]
    fn a_name_only_person_has_just_the_name_token() {
        assert_eq!(identity_tokens(Some(" Ana "), None), vec!["ana"]);
        assert!(identity_tokens(None, None).is_empty());
    }
}
