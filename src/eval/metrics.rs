//! Scoring a plan against ground truth (EVAL.md §3): BCubed and pairwise F1 for grouping,
//! debug-leftover precision and recall, and a seeded bootstrap for confidence intervals.

use serde::Serialize;

use super::sha256::{Rng, sha256};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Prf {
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
}

impl Prf {
    pub fn from_counts(tp: usize, fp: usize, fn_: usize) -> Self {
        let p = if tp + fp == 0 {
            1.0
        } else {
            tp as f64 / (tp + fp) as f64
        };
        let r = if tp + fn_ == 0 {
            1.0
        } else {
            tp as f64 / (tp + fn_) as f64
        };
        Self::from_pr(p, r)
    }

    fn from_pr(p: f64, r: f64) -> Self {
        let f1 = if p + r == 0.0 {
            0.0
        } else {
            2.0 * p * r / (p + r)
        };
        Prf {
            precision: p,
            recall: r,
            f1,
        }
    }
}

/// Pair counts for the "same commit" relation: (tp, fp, fn) over all unordered pairs of
/// `(truth, group)` items.
pub fn pair_counts(items: &[(u8, usize)]) -> (usize, usize, usize) {
    let (mut tp, mut fp, mut fn_) = (0, 0, 0);
    for (i, a) in items.iter().enumerate() {
        for b in &items[i + 1..] {
            match (a.0 == b.0, a.1 == b.1) {
                (true, true) => tp += 1,
                (false, true) => fp += 1,
                (true, false) => fn_ += 1,
                (false, false) => {}
            }
        }
    }
    (tp, fp, fn_)
}

/// BCubed precision, recall, and F1 over `(truth, group)` items; `None` when empty.
pub fn bcubed(items: &[(u8, usize)]) -> Option<Prf> {
    if items.is_empty() {
        return None;
    }
    let n = items.len() as f64;
    let (mut p, mut r) = (0.0, 0.0);
    for a in items {
        let both = items.iter().filter(|b| b.0 == a.0 && b.1 == a.1).count() as f64;
        p += both / items.iter().filter(|b| b.1 == a.1).count() as f64;
        r += both / items.iter().filter(|b| b.0 == a.0).count() as f64;
    }
    Some(Prf::from_pr(p / n, r / n))
}

/// Adjusted Rand index of the grouping against the truth (ROADMAP item 5): 1 for the same
/// partition, about 0 for chance agreement, negative below chance. Comparable with published
/// commit-untangling results (AtomicCommitBench reports ARI). `None` with fewer than 2 items.
/// When both partitions are trivial (all apart or all together) the index is undefined; it
/// is 1 if they are the same partition and 0 otherwise.
pub fn ari(items: &[(u8, usize)]) -> Option<f64> {
    let n = items.len();
    if n < 2 {
        return None;
    }
    let (tp, fp, fn_) = pair_counts(items);
    let all = (n * (n - 1) / 2) as f64;
    let (truth, plan) = ((tp + fn_) as f64, (tp + fp) as f64);
    let expected = truth * plan / all;
    let max = (truth + plan) / 2.0;
    if max == expected {
        return Some(if fp == 0 && fn_ == 0 { 1.0 } else { 0.0 });
    }
    Some((tp as f64 - expected) / (max - expected))
}

pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        f64::NAN
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

/// 95% percentile bootstrap interval of the mean: 10,000 resamples, seeded (EVAL.md §5).
pub fn bootstrap_ci(values: &[f64], seed: u64) -> Option<(f64, f64)> {
    if values.len() < 2 {
        return None;
    }
    let mut rng = Rng::from_digest(&sha256(format!("bootstrap:{seed}").as_bytes()));
    let mut means: Vec<f64> = (0..10_000)
        .map(|_| {
            (0..values.len())
                .map(|_| values[rng.below(values.len())])
                .sum::<f64>()
                / values.len() as f64
        })
        .collect();
    means.sort_by(f64::total_cmp);
    Some((means[249], means[9749]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ari_is_one_for_the_same_partition_and_near_zero_for_chance() {
        // Labels may differ; only the partition counts.
        let same = [(0, 5), (0, 5), (1, 9), (1, 9), (2, 1)];
        assert_eq!(ari(&same), Some(1.0));
        // Truth {a,b,c}{d,e,f}; plan {a,d}{b,e}{c,f}: no pair agrees.
        let crossed = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2)];
        assert!(ari(&crossed).unwrap() < 0.0);
        // A worked example: truth {1,2,3}{4,5}, plan {1,2}{3,4,5}: tp 2, truth pairs 4,
        // plan pairs 4, all 10 -> (2 - 1.6) / (4 - 1.6) = 1/6.
        let worked = [(0, 0), (0, 0), (0, 1), (1, 1), (1, 1)];
        assert!((ari(&worked).unwrap() - 1.0 / 6.0).abs() < 1e-12);
        // Trivial partitions: all apart vs all apart is the same partition.
        assert_eq!(ari(&[(0, 0), (1, 1)]), Some(1.0));
        assert_eq!(ari(&[(0, 0), (0, 1)]), Some(0.0));
        assert_eq!(ari(&[(0, 0)]), None);
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn perfect_and_trivial_groupings() {
        let perfect = [(0, 0), (0, 0), (1, 1)];
        assert_eq!(bcubed(&perfect).unwrap().f1, 1.0);
        assert_eq!(pair_counts(&perfect), (1, 0, 0));
        // Everything in one group: pairwise precision 1/3, recall 1.
        let merged = [(0, 0), (0, 0), (1, 0)];
        assert_eq!(pair_counts(&merged), (1, 2, 0));
        let b = bcubed(&merged).unwrap();
        // Precision: items 1 and 2 see 2 of 3 correct, item 3 sees 1 of 3 -> 5/9. Recall 1.
        assert!(
            close(b.precision, 5.0 / 9.0) && close(b.recall, 1.0),
            "{b:?}"
        );
    }

    #[test]
    fn singletons_are_defined() {
        let alone = [(0, 0), (0, 1)];
        let b = bcubed(&alone).unwrap();
        assert!(close(b.precision, 1.0) && close(b.recall, 0.5));
        assert!(bcubed(&[]).is_none());
    }

    #[test]
    fn bootstrap_is_deterministic_and_brackets_the_mean() {
        let values = [0.2, 0.4, 0.6, 0.8, 1.0, 0.5, 0.7];
        let a = bootstrap_ci(&values, 7).unwrap();
        assert_eq!(a, bootstrap_ci(&values, 7).unwrap());
        assert!(a.0 < mean(&values) && mean(&values) < a.1);
    }
}
