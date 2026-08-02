use std::sync::Arc;

use adapters_fastembed::{DIMENSION, FastEmbedder, MODEL_NAME};
use domain::{Embedder, cosine_similarity};

#[tokio::test(flavor = "multi_thread")]
async fn embedder_integration() {
    let embedder = Arc::new(FastEmbedder::new().expect("model init"));
    assert_eq!(embedder.model_name(), MODEL_NAME);
    assert_eq!(embedder.dimension(), DIMENSION);

    let paraphrase_a = embedder
        .embed("prefer rebasing feature branches instead of merging main into them")
        .await
        .unwrap();
    let paraphrase_b = embedder
        .embed("feature branches should be rebased onto main rather than merged")
        .await
        .unwrap();
    let unrelated = embedder
        .embed("the pasta needs eleven minutes of boiling before draining")
        .await
        .unwrap();

    assert_eq!(paraphrase_a.len(), DIMENSION);

    let sim_paraphrase = cosine_similarity(&paraphrase_a, &paraphrase_b);
    let sim_unrelated = cosine_similarity(&paraphrase_a, &unrelated);
    println!("paraphrase similarity: {sim_paraphrase:.4}");
    println!("unrelated similarity:  {sim_unrelated:.4}");
    assert!(sim_paraphrase > sim_unrelated);
    assert!(sim_paraphrase > 0.8);

    let unrelated_pairs = [
        (
            "use ripgrep instead of grep for searching code",
            "her flight to lisbon departs at seven tomorrow",
        ),
        (
            "the sqlite connection pool holds four connections",
            "add more basil to the tomato sauce",
        ),
        (
            "cargo clippy must pass before every commit",
            "the marathon route follows the river north",
        ),
        (
            "bearer tokens rotate by updating the server env",
            "penguins huddle together to conserve warmth",
        ),
    ];
    for (left, right) in unrelated_pairs {
        let a = embedder.embed(left).await.unwrap();
        let b = embedder.embed(right).await.unwrap();
        let sim = cosine_similarity(&a, &b);
        println!("unrelated baseline: {sim:.4}  ({left} | {right})");
        assert!(sim < sim_paraphrase);
    }

    let mut handles = Vec::new();
    for i in 0..16 {
        let embedder = Arc::clone(&embedder);
        handles.push(tokio::spawn(async move {
            embedder.embed(&format!("burst request number {i}")).await
        }));
    }
    for handle in handles {
        let embedding = handle.await.unwrap().unwrap();
        assert_eq!(embedding.len(), DIMENSION);
    }
}
