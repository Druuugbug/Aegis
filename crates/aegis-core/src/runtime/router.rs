use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// A model specification in a category route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    pub model_id: String,
    pub provider: String,
    /// Whether this model is currently available (circuit breaker open = false)
    pub available: bool,
}

impl ModelSpec {
    /// Create a model spec that is available by default.
    pub fn new(provider: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            provider: provider.into(),
            available: true,
        }
    }

    /// Returns true if the model is currently available (circuit breaker closed).
    pub fn is_available(&self) -> bool {
        self.available
    }
}

/// Routes semantic categories to ordered model fallback chains.
///
/// Categories are semantic, not model-name bound:
///   "deep"               -> [claude-opus, gpt-4o, gemini-ultra]
///   "quick"              -> [claude-haiku, gpt-4o-mini, gemini-flash]
///   "visual-engineering" -> [claude-opus (vision), gpt-4o]
///   "writing"            -> [claude-sonnet, gpt-4o]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CategoryRouter {
    routes: HashMap<String, Vec<ModelSpec>>,
}

impl CategoryRouter {
    /// Create an empty category router with no routes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a category with an ordered list of model specs.
    pub fn register(&mut self, category: impl Into<String>, models: Vec<ModelSpec>) {
        self.routes.insert(category.into(), models);
    }

    /// Resolve a category to the first available model spec.
    pub fn resolve<'a>(&'a self, category: &str) -> Option<&'a ModelSpec> {
        self.routes
            .get(category)
            .and_then(|models| models.iter().find(|m| m.is_available()))
    }

    /// Mark a model unavailable (circuit breaker open).
    pub fn mark_unavailable(&mut self, category: &str, model_id: &str) {
        if let Some(models) = self.routes.get_mut(category) {
            for m in models.iter_mut() {
                if m.model_id == model_id {
                    m.available = false;
                }
            }
        }
    }

    /// Mark a model available again (circuit breaker closed).
    pub fn mark_available(&mut self, category: &str, model_id: &str) {
        if let Some(models) = self.routes.get_mut(category) {
            for m in models.iter_mut() {
                if m.model_id == model_id {
                    m.available = true;
                }
            }
        }
    }

    /// Iterate over all registered category names.
    pub fn categories(&self) -> impl Iterator<Item = &str> {
        self.routes.keys().map(|k| k.as_str())
    }
}
