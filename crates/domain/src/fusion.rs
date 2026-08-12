use std::collections::HashMap;

use crate::memory::MemoryId;

pub const RRF_K: f64 = 60.0;

/// How far behind the best keyword hit a row may rank and still count as a hit.
pub const KEYWORD_RANK_RATIO: f64 = 0.4;

/// Cut the tail off a keyword result ordered best-first by bm25.
///
/// The FTS query joins the terms with `OR`, so a row matching one common term lands in the
/// result beside a row matching every term. bm25 is negative and stronger matches are more
/// negative, so the cutoff is a fraction of the best row's score. The cutoff is relative because
/// no absolute one exists: bm25 grows with corpus size and with query length, and a measured
/// sweep put every absolute floor that silenced a contentless query below the floor that kept a
/// rare identifier findable. A single-term query has no partial matches to drop, and its
/// exact-token recall is left alone.
pub fn trim_keyword_tail<T>(hits: &mut Vec<(T, f64)>, query: &str) {
    if keyword_term_count(query) < 2 {
        return;
    }
    let Some((_, best)) = hits.first() else {
        return;
    };
    let cutoff = best * KEYWORD_RANK_RATIO;
    if let Some(tail) = hits.iter().position(|(_, rank)| *rank > cutoff) {
        hits.truncate(tail);
    }
}

fn keyword_term_count(query: &str) -> usize {
    query
        .split_whitespace()
        .filter(|token| token.chars().any(char::is_alphanumeric))
        .count()
}

pub fn rrf_scores(lists: &[Vec<MemoryId>]) -> HashMap<MemoryId, f64> {
    let mut scores = HashMap::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    scores
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(n: u32) -> MemoryId {
        MemoryId::parse(&format!("m_{n:022}")).unwrap()
    }

    #[test]
    fn scores_sum_across_lists_and_dedup_by_id() {
        let lists = vec![vec![mid(1), mid(2)], vec![mid(2), mid(3)]];
        let scores = rrf_scores(&lists);
        assert_eq!(scores.len(), 3);
        assert!((scores[&mid(1)] - 1.0 / 61.0).abs() < 1e-12);
        assert!((scores[&mid(2)] - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-12);
        assert!((scores[&mid(3)] - 1.0 / 62.0).abs() < 1e-12);
    }

    #[test]
    fn item_in_two_lists_beats_single_list_items_of_same_rank() {
        let lists = vec![vec![mid(1)], vec![mid(1)], vec![mid(2)]];
        let scores = rrf_scores(&lists);
        assert!(scores[&mid(1)] > scores[&mid(2)]);
    }

    #[test]
    fn trim_drops_rows_far_behind_the_best_multi_term_hit() {
        let mut hits = vec![
            (mid(1), -8.0),
            (mid(2), -5.0),
            (mid(3), -3.9),
            (mid(4), -0.4),
        ];
        trim_keyword_tail(&mut hits, "zero downtime migrations");
        assert_eq!(
            hits.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            vec![mid(1), mid(2), mid(3)]
        );
    }

    #[test]
    fn trim_keeps_every_row_for_a_single_term_query() {
        let mut hits = vec![(mid(1), -8.0), (mid(2), -0.4)];
        trim_keyword_tail(&mut hits, "  HashMap  ");
        assert_eq!(hits.len(), 2, "exact-token recall must not regress");
    }

    #[test]
    fn trim_keeps_a_weak_scoring_query_that_still_has_a_clear_winner() {
        let mut hits = vec![(mid(1), -1.65), (mid(2), -1.36), (mid(3), -0.30)];
        trim_keyword_tail(&mut hits, "synapse routing");
        assert_eq!(hits.len(), 2, "a low absolute score is not a miss");
    }

    #[test]
    fn trim_handles_an_empty_result() {
        let mut hits: Vec<(MemoryId, f64)> = Vec::new();
        trim_keyword_tail(&mut hits, "zero downtime");
        assert!(hits.is_empty());
    }
}
