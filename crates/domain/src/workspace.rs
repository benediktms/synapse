use std::fmt;

use crate::error::Error;

const SHARED: &str = "shared";

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
        if name == SHARED {
            return Err(Error::ReservedWorkspaceName);
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
        assert_eq!(Workspace::new("shared"), Err(Error::ReservedWorkspaceName));
        assert!(Workspace::shared().is_shared());
        assert!(!Workspace::new("work").unwrap().is_shared());
    }
}
