//! Grouping speaker embeddings by voice: cosine similarity and average-linkage agglomerative
//! clustering. This is a port of `clusterSpeakers` in stt-sdk's `src/speakers.ts`: the two are
//! meant to be identical (same merge order, same tie-breaking, labels numbered from 0 in order of
//! first appearance), and both are checked against `tests/fixtures/cluster-golden.json`.

/// Cosine similarity of two vectors of one length, -1 to 1; a zero vector is similar to nothing.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    let (mut dot, mut norm_a, mut norm_b) = (0.0f64, 0.0f64, 0.0f64);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (f64::from(x), f64::from(y));
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b).sqrt()
    }
}

/// Labels one input per embedding: clusters merge (highest mean pairwise similarity first) while
/// that mean is at least `threshold` or more than `max_speakers` clusters remain. A `None`
/// embedding (a segment too short to fingerprint) gets `None`.
pub fn cluster_speakers(
    embeddings: &[Option<Vec<f32>>],
    threshold: f64,
    max_speakers: usize,
) -> Vec<Option<usize>> {
    // Clusters are named by their first member's index; `similarity[a][b]` (a < b) is the mean
    // pairwise similarity of clusters a and b, updated in place by a merge.
    let present: Vec<usize> = (0..embeddings.len())
        .filter(|&i| embeddings[i].is_some())
        .collect();
    let mut members: Vec<Option<Vec<usize>>> = vec![None; embeddings.len()];
    for &i in &present {
        members[i] = Some(vec![i]);
    }
    let mut similarity = vec![vec![0.0f64; embeddings.len()]; embeddings.len()];
    for (n, &a) in present.iter().enumerate() {
        for &b in &present[n + 1..] {
            similarity[a][b] = cosine_similarity(
                embeddings[a].as_deref().unwrap(),
                embeddings[b].as_deref().unwrap(),
            );
        }
    }

    let mut alive = present.clone();
    while alive.len() > 1 {
        let mut best: Option<(usize, usize, f64)> = None;
        for (n, &a) in alive.iter().enumerate() {
            for &b in &alive[n + 1..] {
                if best.is_none_or(|(_, _, value)| similarity[a][b] > value) {
                    best = Some((a, b, similarity[a][b]));
                }
            }
        }
        let Some((a, b, value)) = best else { break };
        if value < threshold && alive.len() <= max_speakers {
            break;
        }
        let size_a = members[a].as_ref().unwrap().len() as f64;
        let size_b = members[b].as_ref().unwrap().len() as f64;
        for &c in &alive {
            if c == a || c == b {
                continue;
            }
            let with_a = similarity[c.min(a)][c.max(a)];
            let with_b = similarity[c.min(b)][c.max(b)];
            similarity[c.min(a)][c.max(a)] =
                (size_a * with_a + size_b * with_b) / (size_a + size_b);
        }
        // Cluster b is absorbed into a (a < b, so labels stay in order of first appearance).
        let absorbed = members[b].take().unwrap();
        members[a].as_mut().unwrap().extend(absorbed);
        alive.retain(|&c| c != b);
    }

    let mut labels = vec![None; embeddings.len()];
    for (label, &id) in alive.iter().enumerate() {
        for &index in members[id].as_ref().unwrap() {
            labels[index] = Some(label);
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_sdk_on_the_shared_golden_cases() {
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/cluster-golden.json")).unwrap();
        let embeddings: Vec<Option<Vec<f32>>> = golden["embeddings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                e.as_array()
                    .map(|v| v.iter().map(|x| x.as_f64().unwrap() as f32).collect())
            })
            .collect();
        for case in golden["cases"].as_array().unwrap() {
            let threshold = case["threshold"].as_f64().unwrap();
            let max_speakers = case["max_speakers"]
                .as_u64()
                .map_or(usize::MAX, |n| n as usize);
            let expected: Vec<Option<usize>> = case["labels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l.as_i64().filter(|l| *l >= 0).map(|l| l as usize))
                .collect();
            assert_eq!(
                cluster_speakers(&embeddings, threshold, max_speakers),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn cosine_ignores_length_and_treats_zero_as_unlike_anything() {
        assert!((cosine_similarity(&[1.0, 0.0], &[3.0, 0.0]) - 1.0).abs() < 1e-12);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 2.0]).abs() < 1e-12);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn nothing_to_cluster_gives_no_labels() {
        assert_eq!(cluster_speakers(&[], 0.4, usize::MAX), vec![]);
        assert_eq!(cluster_speakers(&[None], 0.4, usize::MAX), vec![None]);
        assert_eq!(
            cluster_speakers(&[Some(vec![1.0])], 0.4, usize::MAX),
            vec![Some(0)]
        );
    }
}
