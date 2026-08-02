use domain::Timestamp;

use crate::backend::Backend;
use crate::error::ApiError;

pub const CONTENT_MAX_BYTES: usize = 8 * 1024;
pub const QUERY_MAX_BYTES: usize = 1024;
pub const MAX_TAGS: usize = 16;
pub const TAG_MAX_BYTES: usize = 64;

pub(crate) fn validate_content<B: Backend>(backend: &B, content: &str) -> Result<(), ApiError> {
    if content.is_empty() {
        return Err(ApiError::BadRequest("content must not be empty".into()));
    }
    if content.len() > CONTENT_MAX_BYTES {
        return Err(ApiError::BadRequest(format!(
            "content is {} bytes, limit is {CONTENT_MAX_BYTES}",
            content.len()
        )));
    }
    check_token_window(backend, content, "content")
}

pub(crate) fn validate_query<B: Backend>(backend: &B, query: &str) -> Result<(), ApiError> {
    if query.trim().is_empty() {
        return Err(ApiError::BadRequest("query must not be empty".into()));
    }
    if query.len() > QUERY_MAX_BYTES {
        return Err(ApiError::BadRequest(format!(
            "query is {} bytes, limit is {QUERY_MAX_BYTES}",
            query.len()
        )));
    }
    check_token_window(backend, query, "query")
}

pub(crate) fn validate_tags(tags: &[String]) -> Result<(), ApiError> {
    if tags.len() > MAX_TAGS {
        return Err(ApiError::BadRequest(format!(
            "{} tags given, limit is {MAX_TAGS}",
            tags.len()
        )));
    }
    for tag in tags {
        let valid = !tag.is_empty()
            && tag.len() <= TAG_MAX_BYTES
            && !tag.chars().any(|c| c.is_whitespace() || c.is_control());
        if !valid {
            return Err(ApiError::BadRequest(format!(
                "invalid tag {tag:?}: tags must be 1-{TAG_MAX_BYTES} bytes without whitespace"
            )));
        }
    }
    Ok(())
}

pub(crate) fn normalize_timestamp(value: &str) -> Result<Timestamp, ApiError> {
    let parsed = humantime::parse_rfc3339(value).map_err(|_| {
        ApiError::BadRequest(format!(
            "invalid timestamp {value:?}: expected RFC3339 UTC (e.g. 2026-08-02T10:00:00Z)"
        ))
    })?;
    Ok(Timestamp::new(
        humantime::format_rfc3339_seconds(parsed).to_string(),
    ))
}

fn check_token_window<B: Backend>(backend: &B, text: &str, field: &str) -> Result<(), ApiError> {
    let tokens = backend
        .token_count(text)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let window = backend.token_window();
    if tokens > window {
        return Err(ApiError::BadRequest(format!(
            "{field} is {tokens} tokens, model window is {window}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_every_accepted_form_to_canonical_seconds() {
        for value in [
            "2026-08-02T10:00:00Z",
            "2026-08-02T10:00:00.000Z",
            "2026-08-02T10:00:00.999999999Z",
        ] {
            assert_eq!(
                normalize_timestamp(value).unwrap().as_str(),
                "2026-08-02T10:00:00Z",
                "{value}"
            );
        }
    }

    #[test]
    fn rejects_impossible_and_non_utc_timestamps() {
        for value in [
            "",
            "2026-99-99T99:99:99Z",
            "2026-13-01T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-08-02T24:00:00Z",
            "2026-08-02T10:00:00+02:00",
            "2026-08-02 10:00:00Z",
            "2026-08-02T10:00:00",
            "not a timestamp",
        ] {
            assert!(normalize_timestamp(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn normalized_timestamps_sort_chronologically() {
        let earlier = normalize_timestamp("2026-08-02T10:00:00.900Z").unwrap();
        let later = normalize_timestamp("2026-08-02T10:00:01Z").unwrap();
        assert!(earlier < later);
    }
}
