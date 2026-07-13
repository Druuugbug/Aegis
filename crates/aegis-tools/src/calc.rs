//! # CalcTool
//!
//! Deterministic arithmetic/expression evaluation so the agent doesn't have to
//! do math in its head (a common source of errors). Backed by `meval` — a
//! pure-Rust, MIT/Unlicense expression evaluator (no AGPL, no external runtime).

use crate::registry::{Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

/// Evaluates a math expression to a number.
pub struct CalcTool;

impl CalcTool {
    /// Create a new `CalcTool`.
    pub fn new() -> Self {
        CalcTool
    }
}

impl Default for CalcTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for CalcTool {
    fn name(&self) -> &str {
        "calc"
    }

    fn description(&self) -> &str {
        "Evaluate a mathematical expression and return the numeric result. Supports + - * / % ^, parentheses, and functions like sqrt, sin, cos, ln, exp, min, max, plus constants pi and e."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "expression": { "type": "string", "description": "The math expression, e.g. '2 * (3 + 4) ^ 2' or 'sqrt(2) + sin(pi/4)'" }
            },
            "required": ["expression"]
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let expr = args["expression"].as_str().unwrap_or("").trim();
        if expr.is_empty() {
            return Ok("Error: expression is required".to_string());
        }
        match meval::eval_str(expr) {
            Ok(value) => {
                // Present integers without a trailing .0 for readability.
                if value.fract() == 0.0 && value.abs() < 1e15 {
                    Ok(format!("{}", value as i64))
                } else {
                    Ok(format!("{value}"))
                }
            }
            Err(e) => Ok(format!("Error evaluating '{expr}': {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn eval(expr: &str) -> String {
        let tool = CalcTool::new();
        let ctx = crate::registry::ToolContext {
            cwd: std::path::PathBuf::from("."),
            session_id: "t".into(),
            approve_fn: &|_| true,
            yolo: true,
            identity: None,
            sandbox_enabled: false,
        };
        tool.execute(json!({ "expression": expr }), &ctx).await.unwrap()
    }

    #[tokio::test]
    async fn calc_basic_arithmetic() {
        assert_eq!(eval("2 * (3 + 4)").await, "14");
        assert_eq!(eval("10 / 4").await, "2.5");
        assert_eq!(eval("2 ^ 10").await, "1024");
    }

    #[tokio::test]
    async fn calc_empty_and_error() {
        assert!(eval("").await.contains("required"));
        assert!(eval("1 +").await.starts_with("Error"));
    }
}
