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

/// Importance tiers, low < medium < high. The public surface is the tier word; the persisted
/// form is an integer rank so future tiers need no schema migration. Higher ranks are more
/// important — consumers that order by importance should sort `rank()` descending.
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

    // ponytail: a stored rank outside the known tiers clamps to the nearest edge rather than
    // erroring, so a corrupted integer — including any negative from a signed DB column — can
    // never produce an out-of-domain tier in memory. Accepts the signed i64 the DB hands back.
    pub fn from_rank(rank: i64) -> Self {
        match rank {
            ..=0 => Self::Low,
            1 => Self::Medium,
            _ => Self::High,
        }
    }
}

/// The character cap on a title, and on the summary derived for a memory that has none.
pub const TITLE_MAX_CHARS: usize = 120;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Memory {
    pub id: MemoryId,
    pub content: String,
    /// The short level-of-detail form. Empty means the memory carries no title, and
    /// `short_form` derives one from the content instead.
    pub title: String,
    pub kind: MemoryKind,
    pub scope: Scope,
    pub tags: Vec<String>,
    pub pinned: bool,
    pub importance: Importance,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The shortest level of detail: the stored title, or one derived from the content when there
/// is none. The derived form is the content's first sentence on one line. A trailing ellipsis
/// marks whatever the derivation dropped — a later sentence as much as an over-long cut — so a
/// short line never reads as the whole fact. The result never exceeds `TITLE_MAX_CHARS`, which
/// is also the cap a hand-written title is held to.
pub fn short_form(title: &str, content: &str) -> String {
    if !title.is_empty() {
        return title.to_string();
    }
    let single_line = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let sentence = first_sentence(&single_line);
    let dropped_text = sentence.len() < single_line.len();
    let over_cap = sentence.chars().count() > TITLE_MAX_CHARS;
    if !dropped_text && !over_cap {
        return sentence.to_string();
    }
    let kept = match sentence.char_indices().nth(TITLE_MAX_CHARS - 1) {
        Some((byte, _)) => &sentence[..byte],
        None => sentence,
    };
    format!("{}…", kept.trim_end().trim_end_matches('.'))
}

/// The text a memory is embedded from. A hand-written title summarises rather than quotes, so
/// its words need not appear in the body — and recall is the surface that shows the title, so
/// a title the vector lane cannot match is a fact the reader is shown but cannot find.
pub fn embed_text(title: &str, content: &str) -> String {
    if title.is_empty() {
        content.to_string()
    } else {
        format!("{title}\n{content}")
    }
}

fn first_sentence(text: &str) -> &str {
    for (byte, ch) in text.char_indices() {
        if matches!(ch, '.' | '?' | '!') && ends_a_sentence(text, byte) {
            return &text[..byte + ch.len_utf8()];
        }
    }
    text
}

/// A terminator ends a sentence when whitespace follows it and the next word opens with a
/// capital. What separates a full stop from `argocd.yaml`, `1.38.0` or `e.g.` is what comes
/// after it, not what comes before — a technical fact is full of dotted tokens, and a rule
/// that reads backwards either splits the abbreviations or swallows the filenames.
fn ends_a_sentence(text: &str, byte: usize) -> bool {
    let rest = &text[byte + 1..];
    if !rest.starts_with(char::is_whitespace) {
        return false;
    }
    rest.trim_start()
        .chars()
        .next()
        .is_some_and(char::is_uppercase)
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
            assert_eq!(Importance::from_rank(i64::from(parsed.rank())), parsed);
        }
        assert_eq!(
            Importance::parse("urgent"),
            Err(Error::InvalidImportance("urgent".to_string()))
        );
    }

    #[test]
    fn short_form_takes_the_first_sentence_on_one_line() {
        assert_eq!(
            short_form("", "Deploys go through ArgoCD. That is all."),
            "Deploys go through ArgoCD…"
        );
        assert_eq!(short_form("", "No terminator here"), "No terminator here");
        assert_eq!(
            short_form("", "Deploys go through  ArgoCD.\nAlways."),
            "Deploys go through ArgoCD…"
        );
        assert_eq!(
            short_form("", "Why does it retry? Because of Oban."),
            "Why does it retry?…"
        );
    }

    #[test]
    fn a_sentence_that_is_the_whole_content_carries_no_ellipsis() {
        assert_eq!(
            short_form("", "Deploys go through ArgoCD."),
            "Deploys go through ArgoCD."
        );
    }

    /// The dotted tokens a technical fact is full of, on both sides of the rule: an
    /// abbreviation must not split the sentence, and a filename must not swallow it.
    #[test]
    fn short_form_reads_through_dotted_tokens() {
        for (content, want) in [
            (
                "Scoped per repo, e.g. fresha/offers. The rest is workspace-wide.",
                "Scoped per repo, e.g. fresha/offers…",
            ),
            (
                "Deploys are configured in argocd.yaml. The staging lane is separate.",
                "Deploys are configured in argocd.yaml…",
            ),
            (
                "Dashboards live on grafana.fresha.com. Ask before changing them.",
                "Dashboards live on grafana.fresha.com…",
            ),
            (
                "The chart is pinned to 1.38.0. Bumping it needs a review.",
                "The chart is pinned to 1.38.0…",
            ),
            (
                "The cutoff is 0.6 for cosine similarity.",
                "The cutoff is 0.6 for cosine similarity.",
            ),
        ] {
            assert_eq!(short_form("", content), want, "{content}");
        }
    }

    #[test]
    fn short_form_never_exceeds_the_cap_a_title_is_held_to() {
        for content in [
            format!("{} and then some more", "word ".repeat(40)),
            format!("{}. A second sentence.", "x".repeat(TITLE_MAX_CHARS)),
            format!("{}. A second sentence.", "y".repeat(TITLE_MAX_CHARS - 1)),
        ] {
            let short = short_form("", &content);
            assert!(
                short.chars().count() <= TITLE_MAX_CHARS,
                "{} chars: {short}",
                short.chars().count()
            );
            assert!(short.ends_with('…'), "{short}");
        }
    }

    #[test]
    fn short_form_prefers_a_stored_title_over_the_derived_one() {
        assert_eq!(
            short_form("ArgoCD owns deploys", "Deploys go through ArgoCD."),
            "ArgoCD owns deploys"
        );
    }

    #[test]
    fn importance_ranks_are_ordered_low_medium_high() {
        assert!(Importance::Low < Importance::Medium);
        assert!(Importance::Medium < Importance::High);
        // Signed DB integers: negatives clamp to the low edge without wraparound.
        assert_eq!(Importance::from_rank(-1), Importance::Low);
        assert_eq!(Importance::from_rank(0), Importance::Low);
        assert_eq!(Importance::from_rank(1), Importance::Medium);
        assert_eq!(Importance::from_rank(2), Importance::High);
        assert_eq!(Importance::from_rank(9), Importance::High);
    }
}
