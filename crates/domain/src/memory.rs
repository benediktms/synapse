use std::fmt;

use crate::error::Error;

const ID_BODY_LEN: usize = 22;
const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemoryId(String);

impl MemoryId {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("OS randomness unavailable");
        let mut n = u128::from_le_bytes(bytes);
        let mut body = [b'0'; ID_BODY_LEN];
        for slot in body.iter_mut().rev() {
            *slot = BASE62[(n % 62) as usize];
            n /= 62;
        }
        Self(format!(
            "m_{}",
            std::str::from_utf8(&body).expect("base62 is ascii")
        ))
    }

    pub fn parse(s: &str) -> Result<Self, Error> {
        let body = s
            .strip_prefix("m_")
            .ok_or_else(|| Error::InvalidMemoryId(s.to_string()))?;
        if body.len() == ID_BODY_LEN && body.bytes().all(|b| b.is_ascii_alphanumeric()) {
            Ok(Self(s.to_string()))
        } else {
            Err(Error::InvalidMemoryId(s.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MemoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    User,
    Feedback,
    Project,
    Reference,
}

impl MemoryKind {
    pub fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "user" => Ok(Self::User),
            "feedback" => Ok(Self::Feedback),
            "project" => Ok(Self::Project),
            "reference" => Ok(Self::Reference),
            _ => Err(Error::InvalidKind(s.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Workspace,
    Project(String),
}

const SCOPE_SEGMENT_MAX_LEN: usize = 64;

fn is_scope_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= SCOPE_SEGMENT_MAX_LEN
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

impl Scope {
    pub fn parse(s: &str) -> Result<Self, Error> {
        if s == "workspace" {
            return Ok(Self::Workspace);
        }
        match s.split_once('/') {
            Some((owner, repo)) if is_scope_segment(owner) && is_scope_segment(repo) => {
                Ok(Self::Project(s.to_string()))
            }
            _ => Err(Error::InvalidScope(s.to_string())),
        }
    }

    /// The grammar `parse` enforces on each half of `owner/repo`, applied to a bare
    /// owner segment (an org name in an org rule has no accompanying repo).
    pub fn validate_owner(owner: &str) -> Result<(), Error> {
        if is_scope_segment(owner) {
            Ok(())
        } else {
            Err(Error::InvalidScope(owner.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Workspace => "workspace",
            Self::Project(slug) => slug,
        }
    }
}

// ponytail: ordering is lexicographic — callers must supply uniform RFC3339 UTC
// ("2026-08-02T10:00:00Z"); a proper datetime type comes only if formats ever vary.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(String);

impl Timestamp {
    pub fn new(rfc3339_utc: impl Into<String>) -> Self {
        Self(rfc3339_utc.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Importance tiers. Higher rank = more important; the digest sorts rank desc.
/// The public surface is a tier word; the persisted and stored form is the integer rank so
/// future tiers need no schema migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Importance {
    Low,
    Medium,
    High,
}

impl Importance {
    pub const DEFAULT: Self = Self::Medium;

    pub const ALL: [Self; 3] = [Self::Low, Self::Medium, Self::High];

    pub fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => Err(Error::InvalidImportance(s.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
        }
    }

    // ponytail: a stored rank that isn't a known tier clamps to the nearest edge rather than
    // erroring, so a corrupted integer can never produce an out-of-domain tier in memory.
    pub fn from_rank(rank: u8) -> Self {
        match rank {
            0 => Self::Low,
            2.. => Self::High,
            _ => Self::Medium,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memory {
    pub id: MemoryId,
    pub content: String,
    pub kind: MemoryKind,
    pub scope: Scope,
    pub tags: Vec<String>,
    pub pinned: bool,
    pub importance: Importance,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_id_has_expected_format() {
        let id = MemoryId::generate();
        let s = id.as_str();
        assert!(s.starts_with("m_"));
        assert_eq!(s.len(), 2 + ID_BODY_LEN);
        assert!(s[2..].bytes().all(|b| b.is_ascii_alphanumeric()));
    }

    #[test]
    fn generated_ids_are_unique() {
        let ids: std::collections::HashSet<_> = (0..1000).map(|_| MemoryId::generate()).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn id_parse_roundtrips_generated_ids() {
        let id = MemoryId::generate();
        assert_eq!(MemoryId::parse(id.as_str()).unwrap(), id);
    }

    #[test]
    fn id_parse_rejects_malformed_ids() {
        for bad in [
            "",
            "m_",
            "x_0000000000000000000000",
            "m_short",
            "m_00000000000000000000000",
            "m_000000000000000000000!",
        ] {
            assert!(MemoryId::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn kind_parse_roundtrips() {
        for kind in ["user", "feedback", "project", "reference"] {
            assert_eq!(MemoryKind::parse(kind).unwrap().as_str(), kind);
        }
        assert!(MemoryKind::parse("note").is_err());
    }

    #[test]
    fn scope_parse_distinguishes_workspace_and_project() {
        assert_eq!(Scope::parse("workspace").unwrap(), Scope::Workspace);
        for slug in ["fresha/offers", "my-org/repo.js", "a_b/c-d.e", "0/1"] {
            assert_eq!(
                Scope::parse(slug).unwrap(),
                Scope::Project(slug.to_string()),
                "rejected {slug:?}"
            );
        }
    }

    #[test]
    fn scope_parse_enforces_the_owner_repo_grammar() {
        for bad in [
            "",
            "shared",
            "global",
            "has space",
            "fresha",
            "fresha/",
            "/offers",
            "group/sub/repo",
            "fresha/off ers",
            "fresha/off\ters",
            &format!("fresha/{}", "o".repeat(65)),
        ] {
            assert_eq!(
                Scope::parse(bad),
                Err(Error::InvalidScope(bad.to_string())),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn validate_owner_shares_the_grammar_parse_enforces_on_each_half() {
        for good in ["fresha", "my-org", "a_b.c"] {
            assert_eq!(Scope::validate_owner(good), Ok(()), "rejected {good:?}");
        }
        for bad in ["", "has space", "off/ers", &"o".repeat(65)] {
            assert_eq!(
                Scope::validate_owner(bad),
                Err(Error::InvalidScope(bad.to_string())),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn importance_parse_and_rank_roundtrip() {
        assert_eq!(Importance::DEFAULT, Importance::Medium);
        for tier in ["low", "medium", "high"] {
            let parsed = Importance::parse(tier).unwrap();
            assert_eq!(parsed.as_str(), tier);
            assert_eq!(Importance::from_rank(parsed.rank()), parsed);
        }
        assert_eq!(
            Importance::parse("urgent"),
            Err(Error::InvalidImportance("urgent".to_string()))
        );
    }

    #[test]
    fn importance_ranks_are_ordered_low_medium_high() {
        assert!(Importance::Low < Importance::Medium);
        assert!(Importance::Medium < Importance::High);
        assert_eq!(Importance::from_rank(0), Importance::Low);
        assert_eq!(Importance::from_rank(1), Importance::Medium);
        assert_eq!(Importance::from_rank(2), Importance::High);
        assert_eq!(Importance::from_rank(9), Importance::High);
    }
}
