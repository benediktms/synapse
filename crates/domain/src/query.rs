/// Closed-class English words: articles, pronouns, prepositions, conjunctions, auxiliaries and
/// question words. Every entry is a word no memory can be *about*, which is why stripping one can
/// never cost a hit. Negations are deliberately absent: dropping "not" from a query inverts what
/// the embedder is asked for.
const FILLER: &[&str] = &[
    "a", "about", "after", "again", "all", "also", "am", "an", "and", "any", "are", "as", "at",
    "be", "because", "been", "before", "being", "below", "between", "both", "but", "by", "can",
    "cannot", "could", "did", "do", "does", "doing", "down", "during", "each", "few", "for",
    "from", "further", "had", "has", "have", "having", "he", "her", "here", "hers", "him", "his",
    "how", "i", "if", "in", "into", "is", "it", "its", "itself", "just", "me", "more", "most",
    "my", "myself", "of", "off", "on", "once", "one", "only", "or", "other", "ought", "our",
    "ours", "out", "over", "she", "should", "so", "some", "such", "than", "that", "the", "their",
    "theirs", "them", "then", "there", "these", "they", "this", "those", "through", "to", "too",
    "under", "until", "up", "us", "very", "was", "we", "were", "what", "when", "where", "which",
    "while", "who", "whom", "why", "will", "with", "would", "you", "your", "yours",
];

/// The part of a query that can tell one memory from another.
///
/// Both lanes match on filler otherwise. The keyword lane joins its terms with `OR`, so a row
/// holding "and" ranks beside a row holding every content word; the vector lane embeds the filler
/// too, which pulls a short query toward a generic-English centroid that clears the cosine floor
/// against any corpus of technical prose.
///
/// `None` means the query was filler all the way down. A question built only from function words
/// asks for nothing, so recall answers with nothing rather than falling back to the raw query.
pub fn content_terms(query: &str) -> Option<String> {
    let kept: Vec<&str> = query
        .split_whitespace()
        .filter(|token| {
            let bare = token
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase();
            !bare.is_empty() && !FILLER.contains(&bare.as_str())
        })
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_of_content_words_survives_whole() {
        assert_eq!(
            content_terms("postgres autovacuum tuning").as_deref(),
            Some("postgres autovacuum tuning")
        );
    }

    #[test]
    fn padding_around_the_content_words_is_dropped() {
        assert_eq!(
            content_terms("how should we handle retries and backoff going forward").as_deref(),
            Some("handle retries backoff going forward")
        );
    }

    #[test]
    fn a_query_made_only_of_filler_asks_for_nothing() {
        assert_eq!(content_terms("can you do that for me?"), None);
        assert_eq!(content_terms("   "), None);
    }

    #[test]
    fn punctuation_and_case_do_not_hide_filler() {
        assert_eq!(
            content_terms("What, exactly, is HNSW?").as_deref(),
            Some("exactly, HNSW?")
        );
    }

    #[test]
    fn negation_survives_because_it_changes_what_is_asked_for() {
        assert_eq!(
            content_terms("do not use bare except").as_deref(),
            Some("not use bare except")
        );
    }
}
