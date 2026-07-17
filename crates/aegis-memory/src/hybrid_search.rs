/// Hybrid search: vector-score + BM25 re-rank + closet boost + confidence weighting

#[derive(Debug, Clone)]
/// A single hybrid search result with combined vector and BM25 scores.
pub struct SearchResult {
    pub drawer_id: String,
    pub content: String,
    pub vector_score: f32,
    pub bm25_score: f32,
    pub final_score: f32,
    pub confidence: f32,
    pub wing: String,
    pub room: String,
}

impl SearchResult {
    /// Scale the final score by `factor` (used for tunnel weight decay).
    pub fn with_lower_weight(mut self, factor: f32) -> Self {
        self.final_score *= factor;
        self
    }
}

#[derive(Debug, Clone)]
/// Constrains a search to a specific wing and/or room.
pub struct SearchScope {
    pub wing: Option<String>,
    pub room: Option<String>,
    pub limit: usize,
    /// If true, include superseded (inactive) drawers in results.
    pub include_inactive: bool,
}

impl SearchScope {
    /// Create a scope that searches all wings and rooms.
    pub fn global(limit: usize) -> Self {
        Self {
            wing: None,
            room: None,
            limit,
            include_inactive: false,
        }
    }

    /// Create a scope that searches only the given wing.
    pub fn wing(wing: &str, limit: usize) -> Self {
        Self {
            wing: Some(wing.to_string()),
            room: None,
            limit,
            include_inactive: false,
        }
    }

    /// Create a scope that searches a specific wing and room.
    pub fn room(wing: &str, room: &str, limit: usize) -> Self {
        Self {
            wing: Some(wing.to_string()),
            room: Some(room.to_string()),
            limit,
            include_inactive: false,
        }
    }
}

/// Closet boost weights by rank position (0-indexed)
const CLOSET_BOOST: [f32; 5] = [0.40, 0.25, 0.15, 0.08, 0.04];

/// Weight of confidence in final score calculation (0.0 - 1.0).
/// The remaining weight goes to the semantic score (vector + BM25).
const CONFIDENCE_WEIGHT: f32 = 0.15;

/// Hybrid search engine combining vector similarity, BM25, closet boosting, and confidence.
pub struct HybridSearch {
    pub vector_weight: f32, // default 0.6
    pub bm25_weight: f32,   // default 0.4
}

impl Default for HybridSearch {
    fn default() -> Self {
        Self {
            vector_weight: 0.6,
            bm25_weight: 0.4,
        }
    }
}

impl HybridSearch {
    /// Create a new hybrid search with default weights (vector 0.6, BM25 0.4).
    pub fn new() -> Self {
        Self::default()
    }

    /// BM25-style term frequency scoring (simplified, no IDF since no global corpus here).
    /// Returns a 0..1 score for how well the query terms appear in the content.
    fn bm25_score(query: &str, content: &str) -> f32 {
        let k1 = 1.5f32;
        let b = 0.75f32;
        let avg_len = 400.0f32; // approximate average drawer length

        let query_terms: Vec<&str> = query.split_whitespace().collect();
        if query_terms.is_empty() {
            return 0.0;
        }

        let content_lower = content.to_lowercase();
        let words: Vec<&str> = content_lower.split_whitespace().collect();
        let doc_len = words.len() as f32;

        let mut score = 0.0f32;
        for term in &query_terms {
            let term_lower = term.to_lowercase();
            let tf = words.iter().filter(|&&w| w == term_lower.as_str()).count() as f32;
            let numerator = tf * (k1 + 1.0);
            let denominator = tf + k1 * (1.0 - b + b * doc_len / avg_len);
            score += numerator / denominator;
        }

        // Normalize to 0..1
        (score / query_terms.len() as f32).min(1.0)
    }

    /// Re-rank pre-scored candidates using BM25, closet boost, and confidence weighting.
    ///
    /// `candidates` is a tuple of `(drawer_id, content, wing, room, vector_score, confidence)`.
    pub fn rerank(
        &self,
        query: &str,
        candidates: Vec<(String, String, String, String, f32, f32)>,
        closet_boosted_ids: &[String],
        limit: usize,
    ) -> Vec<SearchResult> {
        let mut results: Vec<SearchResult> = candidates
            .into_iter()
            .map(|(id, content, wing, room, vscore, confidence)| {
                let bm25 = Self::bm25_score(query, &content);
                let semantic_score = vscore * self.vector_weight + bm25 * self.bm25_weight;
                let final_score =
                    semantic_score * (1.0 - CONFIDENCE_WEIGHT) + confidence * CONFIDENCE_WEIGHT;
                SearchResult {
                    drawer_id: id,
                    content,
                    vector_score: vscore,
                    bm25_score: bm25,
                    final_score,
                    confidence,
                    wing,
                    room,
                }
            })
            .collect();

        // Apply closet boost
        for (rank, drawer_id) in closet_boosted_ids.iter().enumerate() {
            let boost = CLOSET_BOOST.get(rank).copied().unwrap_or(0.0);
            if let Some(r) = results.iter_mut().find(|r| &r.drawer_id == drawer_id) {
                r.final_score += boost;
            }
        }

        results.sort_by(|a, b| {
            b.final_score
                .partial_cmp(&a.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        results
    }

    /// Search within a MemoryTaxonomy using keyword matching as a stand-in for vector search,
    /// plus BM25 re-rank, confidence weighting, and tunnel extension.
    /// Skips superseded (inactive) drawers unless scope includes inactive.
    pub fn search(
        &self,
        query: &str,
        taxonomy: &crate::taxonomy::MemoryTaxonomy,
        scope: &SearchScope,
    ) -> Vec<SearchResult> {
        // Collect candidate drawers
        let mut candidates: Vec<(String, String, String, String, f32, f32)> = Vec::new();
        let query_lower = query.to_lowercase();

        let wings_to_search: Vec<&str> = if let Some(w) = &scope.wing {
            vec![w.as_str()]
        } else {
            taxonomy.wings.keys().map(|s| s.as_str()).collect()
        };

        for wing_name in &wings_to_search {
            let wing = match taxonomy.wings.get(*wing_name) {
                Some(w) => w,
                None => continue,
            };

            let rooms_to_search: Vec<&str> = if let Some(r) = &scope.room {
                vec![r.as_str()]
            } else {
                wing.rooms.keys().map(|s| s.as_str()).collect()
            };

            for room_name in &rooms_to_search {
                let room = match wing.rooms.get(*room_name) {
                    Some(r) => r,
                    None => continue,
                };

                for drawer in &room.drawers {
                    // Skip superseded drawers unless explicitly requested
                    if !scope.include_inactive && !drawer.active {
                        continue;
                    }

                    // Simple keyword-based "vector" score
                    let vscore = if drawer.content.to_lowercase().contains(&query_lower) {
                        0.8
                    } else {
                        0.1
                    };
                    let confidence = drawer.effective_confidence();
                    candidates.push((
                        drawer.id.clone(),
                        drawer.content.clone(),
                        drawer.wing.clone(),
                        drawer.room.clone(),
                        vscore,
                        confidence,
                    ));
                }
            }
        }

        // Collect closet-boosted drawer ids
        let mut closet_boosted: Vec<String> = Vec::new();
        for wing_name in &wings_to_search {
            let wing = match taxonomy.wings.get(*wing_name) {
                Some(w) => w,
                None => continue,
            };
            for room in wing.rooms.values() {
                for closet in &room.closets {
                    if closet.topic.to_lowercase().contains(&query_lower)
                        || closet
                            .entities
                            .iter()
                            .any(|e| e.to_lowercase().contains(&query_lower))
                    {
                        closet_boosted.extend_from_slice(&closet.drawer_ids);
                    }
                }
            }
        }

        let mut results = self.rerank(query, candidates, &closet_boosted, scope.limit * 3);

        // Tunnel extension: follow tunnels from matching scopes (1 hop, weight 0.7)
        if let (Some(wing_name), Some(room_name)) = (&scope.wing, &scope.room) {
            for tunnel in taxonomy.tunnels_from(wing_name, room_name) {
                let target_scope =
                    SearchScope::room(&tunnel.target_wing, &tunnel.target_room, scope.limit);
                let extended = self.search(query, taxonomy, &target_scope);
                results.extend(extended.into_iter().map(|r| r.with_lower_weight(0.7)));
            }
        }

        results.sort_by(|a, b| {
            b.final_score
                .partial_cmp(&a.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(scope.limit);
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bm25_score_exact_match() {
        let score = HybridSearch::bm25_score("rust", "rust is a great language");
        assert!(score > 0.5, "exact match should score high, got {}", score);
    }

    #[test]
    fn bm25_score_no_match() {
        let score = HybridSearch::bm25_score("python", "rust is great");
        assert_eq!(score, 0.0);
    }

    #[test]
    fn bm25_score_empty_query() {
        let score = HybridSearch::bm25_score("", "some content");
        assert_eq!(score, 0.0);
    }

    #[test]
    fn bm25_score_partial_match() {
        let score = HybridSearch::bm25_score("rust language", "rust is a programming language");
        assert!(score > 0.0);
        assert!(score <= 1.0);
    }

    #[test]
    fn rerank_orders_by_score() {
        let hs = HybridSearch::new();
        let candidates = vec![
            ("d1".into(), "low".into(), "w".into(), "r".into(), 0.1, 0.5),
            ("d2".into(), "high".into(), "w".into(), "r".into(), 0.9, 0.5),
        ];
        let results = hs.rerank("test", candidates, &[], 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].drawer_id, "d2");
    }

    #[test]
    fn rerank_respects_limit() {
        let hs = HybridSearch::new();
        let candidates = vec![
            ("d1".into(), "a".into(), "w".into(), "r".into(), 0.5, 0.5),
            ("d2".into(), "b".into(), "w".into(), "r".into(), 0.5, 0.5),
            ("d3".into(), "c".into(), "w".into(), "r".into(), 0.5, 0.5),
        ];
        let results = hs.rerank("test", candidates, &[], 2);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn rerank_closet_boost() {
        let hs = HybridSearch::new();
        let candidates = vec![
            (
                "d1".into(),
                "boosted".into(),
                "w".into(),
                "r".into(),
                0.5,
                0.5,
            ),
            (
                "d2".into(),
                "normal".into(),
                "w".into(),
                "r".into(),
                0.5,
                0.5,
            ),
        ];
        let results = hs.rerank("test", candidates, &["d1".into()], 10);
        let d1_score = results
            .iter()
            .find(|r| r.drawer_id == "d1")
            .unwrap()
            .final_score;
        let d2_score = results
            .iter()
            .find(|r| r.drawer_id == "d2")
            .unwrap()
            .final_score;
        assert!(d1_score > d2_score, "boosted drawer should rank higher");
    }

    #[test]
    fn search_scope_constructions() {
        let global = SearchScope::global(10);
        assert!(global.wing.is_none());
        assert!(global.room.is_none());
        assert_eq!(global.limit, 10);

        let wing_scope = SearchScope::wing("dev", 5);
        assert_eq!(wing_scope.wing, Some("dev".into()));
        assert!(wing_scope.room.is_none());

        let room_scope = SearchScope::room("dev", "rust", 3);
        assert_eq!(room_scope.wing, Some("dev".into()));
        assert_eq!(room_scope.room, Some("rust".into()));
    }

    #[test]
    fn search_result_with_lower_weight() {
        let r = SearchResult {
            drawer_id: "d1".into(),
            content: "test".into(),
            vector_score: 0.8,
            bm25_score: 0.6,
            final_score: 1.0,
            confidence: 0.9,
            wing: "w".into(),
            room: "r".into(),
        };
        let r2 = r.with_lower_weight(0.5);
        assert!((r2.final_score - 0.5).abs() < 0.01);
    }

    #[test]
    fn rerank_empty_candidates() {
        let hs = HybridSearch::new();
        let results = hs.rerank("test", vec![], &[], 10);
        assert!(results.is_empty());
    }

    #[test]
    fn search_taxonomy_integration() {
        use crate::taxonomy::MemoryTaxonomy;
        let mut tax = MemoryTaxonomy::new();
        tax.ingest(
            "dev",
            "rust",
            "main.rs",
            "fn main() { println!(\"hello\"); }",
        );
        tax.ingest("dev", "python", "app.py", "print('hello')");

        let hs = HybridSearch::new();
        let scope = SearchScope::global(10);
        let results = hs.search("hello", &tax, &scope);
        assert!(results.len() >= 2, "both files contain 'hello'");
    }
}
