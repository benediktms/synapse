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

impl Scope {
    pub fn parse(s: &str) -> Result<Self, Error> {
        if s == "workspace" {
            return Ok(Self::Workspace);
        }
        if s.is_empty() || s.chars().any(char::is_whitespace) {
            return Err(Error::InvalidScope(s.to_string()));
        }
        Ok(Self::Project(s.to_string()))
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memory {
    pub id: MemoryId,
    pub content: String,
    pub kind: MemoryKind,
    pub scope: Scope,
    pub tags: Vec<String>,
    pub pinned: bool,
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
        assert_eq!(
            Scope::parse("fresha/offers").unwrap(),
            Scope::Project("fresha/offers".to_string())
        );
        assert!(Scope::parse("").is_err());
        assert!(Scope::parse("has space").is_err());
    }
}
