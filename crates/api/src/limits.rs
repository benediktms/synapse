//! The input rules that need no embedding model, so a caller without a tokenizer can apply
//! them before it queues a write. The server layer wraps these into `ApiError::BadRequest`.

use domain::TITLE_MAX_CHARS;

/// Sized to sit inside the embedding model's token window with room for prose that tokenizes
/// badly. A fact that does not fit belongs in several linked memories, not one long one.
pub const CONTENT_MAX_BYTES: usize = 2 * 1024;
pub const QUERY_MAX_BYTES: usize = 1024;
pub const MAX_TAGS: usize = 16;
pub const TAG_MAX_BYTES: usize = 64;

pub fn content(content: &str) -> Result<(), String> {
    if content.is_empty() {
        return Err("content must not be empty".into());
    }
    if content.len() > CONTENT_MAX_BYTES {
        return Err(format!(
            "content is {} bytes, limit is {CONTENT_MAX_BYTES}: keep a memory lean — split a \
             long fact into separate memories and link them with `syn relate`",
            content.len()
        ));
    }
    Ok(())
}

/// An empty title is only ever valid on the way in from a dump: memories written before titles
/// existed keep deriving one. `allow_empty` is false everywhere a title is being authored.
pub fn title(title: &str, allow_empty: bool) -> Result<(), String> {
    if title.is_empty() && !allow_empty {
        return Err("title must not be empty: state the fact in one line".into());
    }
    if title.chars().count() > TITLE_MAX_CHARS {
        return Err(format!(
            "title is {} characters, limit is {TITLE_MAX_CHARS}",
            title.chars().count()
        ));
    }
    if title.chars().any(char::is_control) {
        return Err("title must be a single line without control characters".into());
    }
    if title.trim() != title {
        return Err("title must not have leading or trailing whitespace".into());
    }
    Ok(())
}

pub fn tags(tags: &[String]) -> Result<(), String> {
    if tags.len() > MAX_TAGS {
        return Err(format!("{} tags given, limit is {MAX_TAGS}", tags.len()));
    }
    for tag in tags {
        let valid = !tag.is_empty()
            && tag.len() <= TAG_MAX_BYTES
            && !tag.chars().any(|c| c.is_whitespace() || c.is_control());
        if !valid {
            return Err(format!(
                "invalid tag {tag:?}: tags must be 1-{TAG_MAX_BYTES} bytes without whitespace"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_over_the_byte_cap_names_the_remedy() {
        let err = content(&"x".repeat(CONTENT_MAX_BYTES + 1)).unwrap_err();
        assert!(err.contains("limit is 2048"), "{err}");
        assert!(err.contains("syn relate"), "{err}");
        assert!(content(&"x".repeat(CONTENT_MAX_BYTES)).is_ok());
        assert!(content("").is_err());
    }

    #[test]
    fn title_and_tag_rules_hold() {
        assert!(title("", false).is_err());
        assert!(title("", true).is_ok());
        assert!(title(&"x".repeat(TITLE_MAX_CHARS + 1), false).is_err());
        assert!(title(" padded", false).is_err());
        assert!(title("A stated fact", false).is_ok());
        assert!(tags(&["ok".to_string()]).is_ok());
        assert!(tags(&["bad tag".to_string()]).is_err());
        assert!(tags(&vec!["t".to_string(); MAX_TAGS + 1]).is_err());
    }
}
