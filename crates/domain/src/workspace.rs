use std::fmt;

use crate::error::Error;

const SHARED: &str = "shared";

/// Throwaway databases the cloud tests provision carry this prefix. The daemon adopts every
/// database in the org whose name is a workspace, so a stranded one would otherwise be
/// re-adopted at every boot for good.
pub const THROWAWAY_PREFIX: &str = "synapse-";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Workspace(String);

impl Workspace {
    pub fn new(name: &str) -> Result<Self, Error> {
        let len_ok = (1..=32).contains(&name.len());
        let chars_ok = name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !(len_ok && chars_ok) {
            return Err(Error::InvalidWorkspaceName(name.to_string()));
        }
        if name == SHARED || name.starts_with(THROWAWAY_PREFIX) {
            return Err(Error::ReservedWorkspaceName(name.to_string()));
        }
        Ok(Self(name.to_string()))
    }

    pub fn shared() -> Self {
        Self(SHARED.to_string())
    }

    pub fn is_shared(&self) -> bool {
        self.0 == SHARED
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Workspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        for name in ["work", "a", "my-ws-2", "0", &"a".repeat(32)] {
            assert!(Workspace::new(name).is_ok(), "rejected {name:?}");
        }
    }

    #[test]
    fn rejects_invalid_names() {
        for name in [
            "",
            "Work",
            "wo_rk",
            "wo/rk",
            "wo rk",
            "wörk",
            "a.b",
            &"a".repeat(33),
        ] {
            assert_eq!(
                Workspace::new(name),
                Err(Error::InvalidWorkspaceName(name.to_string())),
                "accepted {name:?}"
            );
        }
    }

    #[test]
    fn shared_is_reserved() {
        assert_eq!(
            Workspace::new("shared"),
            Err(Error::ReservedWorkspaceName("shared".to_string()))
        );
        assert!(Workspace::shared().is_shared());
        assert!(!Workspace::new("work").unwrap().is_shared());
    }

    #[test]
    fn a_throwaway_database_name_is_never_a_workspace() {
        for name in ["synapse-test-4711", "synapse-migration-4711", "synapse-"] {
            assert_eq!(
                Workspace::new(name),
                Err(Error::ReservedWorkspaceName(name.to_string())),
                "accepted {name:?}"
            );
        }
    }
}
