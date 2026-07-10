/// Tree of Thoughts (ToT) search engine for Aegis
///
/// Implements a Generate-Evaluate-Select pattern for multi-path reasoning.
/// Supports beam search (BFS) and greedy/sampling selection strategies.

use async_trait::async_trait;

/// State in the thought tree — a sequence of reasoning steps
#[derive(Clone, Debug)]
pub struct ThoughtState {
    /// The chain of thought steps so far
    pub steps: Vec<String>,
    /// Optional task/problem description
    pub problem: String,
}

impl ThoughtState {
    /// Create a new empty thought state for the given problem.
    pub fn new(problem: impl Into<String>) -> Self {
        Self { steps: vec![], problem: problem.into() }
    }

    /// Create a new state by extending this state with a new thought
    pub fn extend(&self, thought: impl Into<String>) -> Self {
        let mut steps = self.steps.clone();
        steps.push(thought.into());
        Self { steps, problem: self.problem.clone() }
    }

    /// Get the current chain as a single concatenated string
    pub fn chain_text(&self) -> String {
        self.steps.join("\n")
    }

    /// How many steps deep this state is
    pub fn depth(&self) -> usize {
        self.steps.len()
    }
}

/// Strategy for selecting candidates at each beam search step
#[derive(Clone, Debug)]
pub enum SelectionStrategy {
    /// Keep top-k by score (deterministic)
    Greedy(usize),
    /// Weighted random sampling of k candidates
    Sample(usize),
}

impl SelectionStrategy {
    /// Get the beam width (number of candidates kept per step).
    pub fn beam_width(&self) -> usize {
        match self {
            SelectionStrategy::Greedy(k) => *k,
            SelectionStrategy::Sample(k) => *k,
        }
    }
}

/// Generates candidate thoughts from the current state
#[async_trait]
pub trait ThoughtGenerator: Send + Sync {
    /// Generate `n` candidate next thoughts for the given state
    async fn generate(&self, state: &ThoughtState, n: usize) -> Vec<String>;
}

/// Evaluates a candidate thought given the current state
#[async_trait]
pub trait ThoughtEvaluator: Send + Sync {
    /// Return a score in [0.0, 1.0] for adding `thought` to `state`.
    /// Higher = more promising.
    async fn evaluate(&self, state: &ThoughtState, thought: &str) -> f64;
}

/// Result of a beam search
#[derive(Debug)]
pub struct BeamSearchResult {
    /// Final beam states sorted by best score
    pub states: Vec<ThoughtState>,
    /// The best (highest scored) state's final step score
    pub best_score: f64,
}

/// Run BFS / Beam Search over the thought space.
///
/// # Arguments
/// * `generator` - produces candidate thoughts
/// * `evaluator` - scores each candidate
/// * `initial` - starting state
/// * `steps` - how many reasoning steps (depth)
/// * `strategy` - beam selection strategy (Greedy(k) or Sample(k))
/// * `branch_factor` - how many candidates to generate per state per step
pub async fn beam_search(
    generator: &dyn ThoughtGenerator,
    evaluator: &dyn ThoughtEvaluator,
    initial: ThoughtState,
    steps: usize,
    strategy: SelectionStrategy,
    branch_factor: usize,
) -> BeamSearchResult {
    let beam_width = strategy.beam_width();
    let mut beam: Vec<(ThoughtState, f64)> = vec![(initial, 0.0)];

    for _step in 0..steps {
        let mut candidates: Vec<(ThoughtState, f64)> = vec![];

        for (state, _prev_score) in &beam {
            let thoughts = generator.generate(state, branch_factor).await;
            for thought in thoughts {
                let score = evaluator.evaluate(state, &thought).await;
                let next_state = state.extend(&thought);
                candidates.push((next_state, score));
            }
        }

        if candidates.is_empty() {
            break;
        }

        // Sort descending by score
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        beam = match &strategy {
            SelectionStrategy::Greedy(_) => {
                candidates.into_iter().take(beam_width).collect()
            }
            SelectionStrategy::Sample(k) => {
                // Weighted sampling without replacement
                sample_weighted(candidates, *k)
            }
        };
    }

    let best_score = beam.first().map(|(_, s)| *s).unwrap_or(0.0);
    let states = beam.into_iter().map(|(s, _)| s).collect();
    BeamSearchResult { states, best_score }
}

/// Weighted sampling using stochastic universal sampling.
/// candidates must be sorted descending by score.
fn sample_weighted(candidates: Vec<(ThoughtState, f64)>, k: usize) -> Vec<(ThoughtState, f64)> {
    let n = candidates.len().min(k);
    let total_score: f64 = candidates.iter().map(|(_, s)| s.max(0.0)).sum();
    if total_score <= 0.0 {
        return candidates.into_iter().take(n).collect();
    }

    let step = total_score / n as f64;
    let mut cumulative = 0.0f64;
    let mut pick_at = step * 0.5;
    let mut result: Vec<(ThoughtState, f64)> = Vec::with_capacity(n);

    for item in &candidates {
        if result.len() >= n { break; }
        cumulative += item.1.max(0.0);
        if cumulative >= pick_at {
            result.push(item.clone());
            pick_at += step;
        }
    }

    // Fill remaining from top if we didn't sample enough
    if result.len() < n {
        for item in candidates.into_iter().take(n) {
            if result.len() >= n { break; }
            // Avoid exact duplicates (by pointer comparison is not possible after clone,
            // so just fill remaining slots accepting possible overlap for simplicity)
            result.push(item);
        }
    }
    result.truncate(n);
    result
}

/// A simple static ThoughtGenerator that uses a fixed list of thoughts (for testing)
pub struct StaticGenerator {
    pub thoughts: Vec<String>,
}

#[async_trait]
impl ThoughtGenerator for StaticGenerator {
    async fn generate(&self, _state: &ThoughtState, n: usize) -> Vec<String> {
        self.thoughts.iter().take(n).cloned().collect()
    }
}

/// A simple ThoughtEvaluator that scores by thought length (for testing/demo)
pub struct LengthEvaluator;

#[async_trait]
impl ThoughtEvaluator for LengthEvaluator {
    async fn evaluate(&self, _state: &ThoughtState, thought: &str) -> f64 {
        // Longer thoughts score slightly higher, capped at 1.0
        (thought.len() as f64 / 200.0).min(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_beam_search_basic() {
        let gen = StaticGenerator {
            thoughts: vec![
                "Step A: analyze the problem".into(),
                "Step B: gather data".into(),
                "Step C: form hypothesis".into(),
            ],
        };
        let eval = LengthEvaluator;
        let initial = ThoughtState::new("Solve 2+2");
        let result = beam_search(&gen, &eval, initial, 2, SelectionStrategy::Greedy(2), 3).await;
        assert!(!result.states.is_empty());
        assert!(result.states[0].depth() <= 2);
    }

    #[tokio::test]
    async fn test_thought_state_extend() {
        let state = ThoughtState::new("problem");
        let next = state.extend("thought 1");
        assert_eq!(next.depth(), 1);
        let next2 = next.extend("thought 2");
        assert_eq!(next2.depth(), 2);
        assert!(next2.chain_text().contains("thought 1"));
    }
}
