//! The seeded dependency graph shared by both project styles.
//!
//! Units are split into layers. Layer 0 is the single root unit, `u0`; every unit in
//! layer `k` depends on `fanout` units of layer `k - 1`, so every unit reaches the root
//! and a dependency always has a lower index than its dependent. The top layer has no
//! dependents; `main` uses it.

use serde::{Deserialize, Serialize};

/// The generated graph: `deps[u]` lists the units `u` depends on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Graph {
    pub deps: Vec<Vec<usize>>,
}

impl Graph {
    /// `units` units in `depth` layers above the root, each depending on up to
    /// `fanout` units of the layer below.
    #[must_use]
    pub fn generate(units: usize, depth: usize, fanout: usize, seed: u64) -> Self {
        assert!(units >= 2, "need at least two units");
        let depth = depth.clamp(1, units - 1);
        let layer_of = |unit: usize| {
            if unit == 0 {
                0
            } else {
                1 + (unit - 1) * depth / (units - 1)
            }
        };
        let mut layers: Vec<Vec<usize>> = vec![Vec::new(); depth + 1];
        for unit in 0..units {
            layers[layer_of(unit)].push(unit);
        }

        let mut rng = SplitMix64(seed);
        let mut deps = vec![Vec::new(); units];
        for unit in 1..units {
            let below = &layers[layer_of(unit) - 1];
            let wanted = fanout.clamp(1, below.len());
            let mut chosen: Vec<usize> = Vec::with_capacity(wanted);
            while chosen.len() < wanted {
                let pick = below[rng.below(below.len())];
                if !chosen.contains(&pick) {
                    chosen.push(pick);
                }
            }
            chosen.sort_unstable();
            deps[unit] = chosen;
        }
        Self { deps }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.deps.len()
    }

    /// Units nothing depends on, in order.
    #[must_use]
    pub fn top(&self) -> Vec<usize> {
        let mut has_dependents = vec![false; self.len()];
        for deps in &self.deps {
            for &dep in deps {
                has_dependents[dep] = true;
            }
        }
        (0..self.len()).filter(|&u| !has_dependents[u]).collect()
    }

    /// For each unit, how many units depend on it directly or indirectly.
    #[must_use]
    pub fn dependent_counts(&self) -> Vec<usize> {
        let words = self.len().div_ceil(64);
        let mut sets = vec![vec![0u64; words]; self.len()];
        // Dependencies always have lower indices, so walking down visits every
        // dependent before its dependencies.
        for unit in (0..self.len()).rev() {
            let mut own = std::mem::take(&mut sets[unit]);
            for &dep in &self.deps[unit] {
                let set = &mut sets[dep];
                for (word, bits) in set.iter_mut().zip(&own) {
                    *word |= bits;
                }
                set[unit / 64] |= 1 << (unit % 64);
            }
            own.shrink_to_fit();
            sets[unit] = own;
        }
        sets.iter()
            .map(|set| set.iter().map(|w| w.count_ones() as usize).sum())
            .collect()
    }
}

/// A small, fast, seedable generator, so graphs are identical everywhere.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`.
    #[allow(clippy::cast_possible_truncation)] // The result is below `n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_unit_reaches_the_root_through_lower_indices() {
        let graph = Graph::generate(500, 6, 3, 7);
        for (unit, deps) in graph.deps.iter().enumerate().skip(1) {
            assert!(!deps.is_empty(), "u{unit} has no dependencies");
            assert!(deps.iter().all(|&dep| dep < unit));
            assert!(deps.len() <= 3);
        }
        let counts = graph.dependent_counts();
        assert_eq!(counts[0], 499, "everything depends on the root");
        for unit in graph.top() {
            assert_eq!(counts[unit], 0);
        }
    }

    #[test]
    fn is_deterministic_per_seed() {
        assert_eq!(Graph::generate(100, 4, 3, 1), Graph::generate(100, 4, 3, 1));
        assert_ne!(Graph::generate(100, 4, 3, 1), Graph::generate(100, 4, 3, 2));
    }

    #[test]
    fn counts_transitive_dependents() {
        // 0 <- 1 <- 3, 0 <- 2 <- 3
        let graph = Graph {
            deps: vec![vec![], vec![0], vec![0], vec![1, 2]],
        };
        assert_eq!(graph.dependent_counts(), [3, 1, 1, 0]);
        assert_eq!(graph.top(), [3]);
    }

    #[test]
    fn small_graphs_still_have_a_root_and_a_top() {
        let graph = Graph::generate(2, 8, 3, 0);
        assert_eq!(graph.deps, [vec![], vec![0]]);
        assert_eq!(graph.top(), [1]);
    }
}
