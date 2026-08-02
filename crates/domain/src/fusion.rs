use std::collections::HashMap;

use crate::memory::MemoryId;

pub const RRF_K: f64 = 60.0;

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
}
