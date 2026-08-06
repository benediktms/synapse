use std::fmt;

use crate::error::Error;
use crate::memory::MemoryId;

/// A typed edge between two memories. The public name is a noun; the CLI command and the
/// recall display phrase are separate surfaces and may read as verbs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Relation {
    /// Generic "these two are related" — the fallback for a sensed-but-unspecific relation.
    Relation,
    /// Positive: one memory supports the other.
    Support,
    /// "These two contradict" — modelled as a state, not a direction (we don't know which is
    /// wrong).
    Contradiction,
    /// One memory supersedes another (directed); the only relation with side effects.
    Supersession,
}

impl Relation {
    pub const ALL: [Self; 4] = [
        Self::Relation,
        Self::Support,
        Self::Contradiction,
        Self::Supersession,
    ];

    pub fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "relation" => Ok(Self::Relation),
            "support" => Ok(Self::Support),
            "contradiction" => Ok(Self::Contradiction),
            "supersession" => Ok(Self::Supersession),
            _ => Err(Error::InvalidRelation(s.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Relation => "relation",
            Self::Support => "support",
            Self::Contradiction => "contradiction",
            Self::Supersession => "supersession",
        }
    }

    /// Only `supersession` is directed; the rest are symmetric pairs.
    pub fn is_directed(self) -> bool {
        matches!(self, Self::Supersession)
    }

    /// Recall display phrase as seen from the *this* endpoint of the edge.
    pub fn phrase_from(self, directed: bool, this_is_source: bool) -> &'static str {
        match (self, directed, this_is_source) {
            (Self::Relation, _, _) => "relates to",
            (Self::Support, _, _) => "supports",
            (Self::Contradiction, _, _) => "contradicted by",
            (Self::Supersession, _, true) => "supersedes",
            (Self::Supersession, _, false) => "is superseded by",
        }
    }
}

impl fmt::Display for Relation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed edge between two memories.
///
/// Storage: the schema stores endpoints canonically so symmetric edges collapse under
/// `PRIMARY KEY (low_id, high_id, relation)`. `source`/`target` carry direction: for symmetric
/// relations they are the canonical low/high endpoints (direction is meaningless); for directed
/// `supersession` they are the actual source (the superseder) and target (the superseded).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Link {
    pub source: MemoryId,
    pub target: MemoryId,
    pub relation: Relation,
}

impl Link {
    /// Canonical form for storage: symmetric relations order endpoints low-first so `(a,b)` and
    /// `(b,a)` collapse to one row; directed edges keep true source->target.
    pub fn canonical(mut self) -> Self {
        if !self.relation.is_directed() && self.source > self.target {
            std::mem::swap(&mut self.source, &mut self.target);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(n: u32) -> MemoryId {
        MemoryId::parse(&format!("m_{n:022}")).unwrap()
    }

    #[test]
    fn relation_parse_roundtrips_nouns() {
        for noun in ["relation", "support", "contradiction", "supersession"] {
            let parsed = Relation::parse(noun).unwrap();
            assert_eq!(parsed.as_str(), noun);
        }
        assert_eq!(
            Relation::parse("relates"),
            Err(Error::InvalidRelation("relates".to_string()))
        );
    }

    #[test]
    fn only_supersession_is_directed() {
        assert!(!Relation::Relation.is_directed());
        assert!(!Relation::Support.is_directed());
        assert!(!Relation::Contradiction.is_directed());
        assert!(Relation::Supersession.is_directed());
    }

    #[test]
    fn recall_phrases_per_endpoint() {
        assert_eq!(Relation::Relation.phrase_from(false, true), "relates to");
        assert_eq!(Relation::Support.phrase_from(false, false), "supports");
        assert_eq!(
            Relation::Contradiction.phrase_from(false, true),
            "contradicted by"
        );
        assert_eq!(Relation::Supersession.phrase_from(true, true), "supersedes");
        assert_eq!(
            Relation::Supersession.phrase_from(true, false),
            "is superseded by"
        );
    }

    #[test]
    fn canonical_orders_symmetric_low_first() {
        let hi = mid(9);
        let lo = mid(2);
        let link = Link {
            source: hi.clone(),
            target: lo.clone(),
            relation: Relation::Support,
        }
        .canonical();
        assert_eq!(link.source, lo);
        assert_eq!(link.target, hi);
    }

    #[test]
    fn canonical_preserves_directed_orientation() {
        let source = mid(9);
        let target = mid(2);
        let link = Link {
            source: source.clone(),
            target: target.clone(),
            relation: Relation::Supersession,
        }
        .canonical();
        assert_eq!(link.source, source);
        assert_eq!(link.target, target);
    }
}
