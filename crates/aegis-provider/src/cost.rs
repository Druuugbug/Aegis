use serde::{Deserialize, Serialize};

// ── Model route cost estimation ──

/// How the model is billed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BillingKind {
    Metered,
    Subscription,
    IncludedQuota,
    Unknown,
}

/// Where the cost data came from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    PublicApiPricing,
    PublicPlanPricing,
    RuntimePlan,
    OpenRouterEndpoint,
    Heuristic,
}

/// Confidence level of the cost estimate.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CostConfidence {
    Exact,
    High,
    Medium,
    Low,
    Unknown,
}

/// Cost tier preference for automatic routing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CostTier {
    /// Always pick the cheapest option.
    Cheap,
    /// Balance cost and quality.
    Balanced,
    /// Prefer quality over cost.
    Quality,
}

/// Reference token counts for cost estimation.
pub const REFERENCE_INPUT_TOKENS: u64 = 25_000;
pub const REFERENCE_OUTPUT_TOKENS: u64 = 5_000;

/// Detailed cost estimate for a model route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteCheapnessEstimate {
    pub billing_kind: BillingKind,
    pub source: CostSource,
    pub confidence: CostConfidence,
    /// Monthly price in micro-dollars (for subscription models).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly_price_micros: Option<u64>,
    /// Input price per million tokens in micro-dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_price_per_mtok_micros: Option<u64>,
    /// Output price per million tokens in micro-dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_price_per_mtok_micros: Option<u64>,
    /// Cache read price per million tokens in micro-dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_price_per_mtok_micros: Option<u64>,
    /// Estimated cost of a reference request (25k in / 5k out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_reference_cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl RouteCheapnessEstimate {
    /// Convenience constructor for metered (pay-per-token) models.
    pub fn metered(
        source: CostSource,
        confidence: CostConfidence,
        input_price_per_mtok_micros: u64,
        output_price_per_mtok_micros: u64,
        cache_read_price_per_mtok_micros: Option<u64>,
    ) -> Self {
        Self {
            billing_kind: BillingKind::Metered,
            source,
            confidence,
            monthly_price_micros: None,
            input_price_per_mtok_micros: Some(input_price_per_mtok_micros),
            output_price_per_mtok_micros: Some(output_price_per_mtok_micros),
            cache_read_price_per_mtok_micros,
            estimated_reference_cost_micros: Some(reference_request_cost_micros(
                input_price_per_mtok_micros,
                output_price_per_mtok_micros,
            )),
            note: None,
        }
    }

    /// Convenience constructor for subscription models.
    pub fn subscription(
        source: CostSource,
        confidence: CostConfidence,
        monthly_price_micros: u64,
        included_requests_per_month: Option<u64>,
    ) -> Self {
        let estimated = included_requests_per_month
            .map(|reqs| monthly_price_micros.checked_div(reqs).unwrap_or(0));
        Self {
            billing_kind: BillingKind::Subscription,
            source,
            confidence,
            monthly_price_micros: Some(monthly_price_micros),
            input_price_per_mtok_micros: None,
            output_price_per_mtok_micros: None,
            cache_read_price_per_mtok_micros: None,
            estimated_reference_cost_micros: estimated,
            note: None,
        }
    }
}

/// Calculate the estimated cost of a reference request in micro-dollars.
pub fn reference_request_cost_micros(
    input_price_per_mtok_micros: u64,
    output_price_per_mtok_micros: u64,
) -> u64 {
    (input_price_per_mtok_micros * REFERENCE_INPUT_TOKENS
        + output_price_per_mtok_micros * REFERENCE_OUTPUT_TOKENS)
        / 1_000_000
}

// ── Model route ──

/// A single route to access a model: model + provider + method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelRoute {
    pub model: String,
    pub provider: String,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cheapness: Option<RouteCheapnessEstimate>,
}

impl ModelRoute {
    /// Estimates the cost in micro-currency units.
    pub fn estimated_cost_micros(&self) -> Option<u64> {
        self.cheapness
            .as_ref()
            .and_then(|c| c.estimated_reference_cost_micros)
    }
}

// ── Cost estimation for actual requests ──

/// Estimate the cost of an actual request given token counts.
pub fn estimate_request_cost(
    route: &ModelRoute,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
) -> Option<CostEstimate> {
    let cheapness = route.cheapness.as_ref()?;
    match cheapness.billing_kind {
        BillingKind::Metered => {
            let input_price = cheapness.input_price_per_mtok_micros? as f64;
            let output_price = cheapness.output_price_per_mtok_micros? as f64;
            let cache_price = cheapness.cache_read_price_per_mtok_micros.unwrap_or(0) as f64;
            let normal_input = input_tokens.saturating_sub(cache_read_tokens) as f64;
            let cost = (normal_input * input_price
                + cache_read_tokens as f64 * cache_price
                + output_tokens as f64 * output_price)
                / 1_000_000.0;
            Some(CostEstimate {
                cost_micros: cost as u64,
                billing_kind: BillingKind::Metered,
            })
        }
        BillingKind::Subscription => {
            // Subscription models: cost is 0 per request (included in plan)
            Some(CostEstimate {
                cost_micros: 0,
                billing_kind: BillingKind::Subscription,
            })
        }
        _ => None,
    }
}

/// Result of a cost estimation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostEstimate {
    pub cost_micros: u64,
    pub billing_kind: BillingKind,
}

impl CostEstimate {
    /// Display as human-readable dollar amount.
    pub fn display_dollars(&self) -> String {
        if self.cost_micros == 0 {
            return "included".to_string();
        }
        let dollars = self.cost_micros as f64 / 1_000_000.0;
        if dollars < 0.01 {
            format!("${:.4}", dollars)
        } else {
            format!("${:.2}", dollars)
        }
    }
}

// ── Router ──

/// Automatic route selection based on requirements.
pub struct Router;

impl Router {
    /// Select the best route from available options.
    pub fn select(routes: &[ModelRoute], tier: CostTier) -> Option<&ModelRoute> {
        let available: Vec<&ModelRoute> = routes.iter().filter(|r| r.available).collect();
        if available.is_empty() {
            return None;
        }

        match tier {
            CostTier::Cheap => available
                .iter()
                .min_by_key(|r| r.estimated_cost_micros().unwrap_or(u64::MAX))
                .copied(),
            CostTier::Quality => {
                // Without quality signals, fall back to most expensive (likely best)
                available
                    .iter()
                    .max_by_key(|r| r.estimated_cost_micros().unwrap_or(0))
                    .copied()
            }
            CostTier::Balanced => {
                // Pick median cost
                let mut sorted = available;
                sorted.sort_by_key(|r| r.estimated_cost_micros().unwrap_or(u64::MAX));
                sorted.get(sorted.len() / 2).copied()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_route(model: &str, provider: &str, cost: u64) -> ModelRoute {
        ModelRoute {
            model: model.to_string(),
            provider: provider.to_string(),
            available: true,
            cheapness: Some(RouteCheapnessEstimate::metered(
                CostSource::PublicApiPricing,
                CostConfidence::High,
                cost,     // input price per mtok
                cost * 3, // output price
                None,
            )),
        }
    }

    #[test]
    fn test_router_selects_cheapest() {
        let routes = vec![
            make_route("gpt-4o", "openai", 5_000_000),
            make_route("gpt-4o-mini", "openai", 150_000),
            make_route("claude-sonnet", "anthropic", 3_000_000),
        ];
        let selected = Router::select(&routes, CostTier::Cheap).unwrap();
        assert_eq!(selected.model, "gpt-4o-mini");
    }

    #[test]
    fn test_router_skips_unavailable() {
        let mut cheap = make_route("gpt-4o-mini", "openai", 150_000);
        cheap.available = false;
        let expensive = make_route("gpt-4o", "openai", 5_000_000);
        let routes = vec![cheap, expensive];
        let selected = Router::select(&routes, CostTier::Cheap).unwrap();
        assert_eq!(selected.model, "gpt-4o");
    }

    #[test]
    fn test_cost_estimate_display() {
        let estimate = CostEstimate {
            cost_micros: 1_500_000,
            billing_kind: BillingKind::Metered,
        };
        assert_eq!(estimate.display_dollars(), "$1.50");

        let free = CostEstimate {
            cost_micros: 0,
            billing_kind: BillingKind::Subscription,
        };
        assert_eq!(free.display_dollars(), "included");

        let tiny = CostEstimate {
            cost_micros: 123,
            billing_kind: BillingKind::Metered,
        };
        assert_eq!(tiny.display_dollars(), "$0.0001");
    }

    #[test]
    fn test_reference_cost_calculation() {
        // $5/M input, $15/M output → reference request (25k in + 5k out)
        let cost = reference_request_cost_micros(5_000_000, 15_000_000);
        // 25k * 5M / 1M = 125k + 5k * 15M / 1M = 75k = 200k micro-dollars
        assert_eq!(cost, 200_000);
    }

    #[test]
    fn test_estimate_request_cost_with_cache() {
        let route = make_route("claude", "anthropic", 3_000_000);
        let mut route = route;
        route
            .cheapness
            .as_mut()
            .unwrap()
            .cache_read_price_per_mtok_micros = Some(300_000);

        let estimate = estimate_request_cost(&route, 25_000, 5_000, 20_000).unwrap();
        // normal input: 5k * 3M / 1M = 15k
        // cache: 20k * 300k / 1M = 6k
        // output: 5k * 9M / 1M = 45k
        // total: 66k
        assert_eq!(estimate.cost_micros, 66_000);
    }

    #[test]
    fn test_subscription_cost_is_zero() {
        let route = ModelRoute {
            model: "copilot".into(),
            provider: "github".into(),
            available: true,
            cheapness: Some(RouteCheapnessEstimate::subscription(
                CostSource::PublicPlanPricing,
                CostConfidence::Exact,
                19_000_000, // $19/month
                Some(500),
            )),
        };
        let estimate = estimate_request_cost(&route, 50_000, 10_000, 0).unwrap();
        assert_eq!(estimate.cost_micros, 0);
        assert_eq!(estimate.display_dollars(), "included");
    }
}
