//! # CalcTool
//!
//! Deterministic arithmetic/expression evaluation so the agent doesn't have to
//! do math in its head (a common source of errors). The evaluator is kept
//! in-tree to avoid pulling old parser dependencies into the build graph.

use crate::registry::{Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

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
        match eval_expression(expr) {
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

fn eval_expression(expr: &str) -> std::result::Result<f64, String> {
    let mut parser = Parser::new(expr);
    let value = parser.parse_expression()?;
    parser.skip_ws();
    if parser.is_eof() {
        Ok(value)
    } else {
        Err(format!(
            "unexpected input at '{}'; expected end of expression",
            parser.remaining()
        ))
    }
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn parse_expression(&mut self) -> std::result::Result<f64, String> {
        self.parse_add_sub()
    }

    fn parse_add_sub(&mut self) -> std::result::Result<f64, String> {
        let mut value = self.parse_mul_div()?;
        loop {
            self.skip_ws();
            if self.consume('+') {
                value += self.parse_mul_div()?;
            } else if self.consume('-') {
                value -= self.parse_mul_div()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_mul_div(&mut self) -> std::result::Result<f64, String> {
        let mut value = self.parse_power()?;
        loop {
            self.skip_ws();
            if self.consume('*') {
                value *= self.parse_power()?;
            } else if self.consume('/') {
                value /= self.parse_power()?;
            } else if self.consume('%') {
                value %= self.parse_power()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_power(&mut self) -> std::result::Result<f64, String> {
        let base = self.parse_unary()?;
        self.skip_ws();
        if self.consume('^') {
            let exponent = self.parse_power()?;
            Ok(base.powf(exponent))
        } else {
            Ok(base)
        }
    }

    fn parse_unary(&mut self) -> std::result::Result<f64, String> {
        self.skip_ws();
        if self.consume('+') {
            self.parse_unary()
        } else if self.consume('-') {
            Ok(-self.parse_unary()?)
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> std::result::Result<f64, String> {
        self.skip_ws();
        if self.consume('(') {
            let value = self.parse_expression()?;
            self.expect(')')?;
            return Ok(value);
        }
        if self
            .peek()
            .map(|ch| ch.is_ascii_digit() || ch == '.')
            .unwrap_or(false)
        {
            return self.parse_number();
        }
        if self
            .peek()
            .map(|ch| ch.is_ascii_alphabetic() || ch == '_')
            .unwrap_or(false)
        {
            return self.parse_identifier();
        }
        Err(format!(
            "expected number, identifier or '(' at '{}'; got end or invalid token",
            self.remaining()
        ))
    }

    fn parse_identifier(&mut self) -> std::result::Result<f64, String> {
        let ident = self.take_while(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        self.skip_ws();
        if self.consume('(') {
            let mut args = Vec::new();
            self.skip_ws();
            if !self.consume(')') {
                loop {
                    args.push(self.parse_expression()?);
                    self.skip_ws();
                    if self.consume(')') {
                        break;
                    }
                    self.expect(',')?;
                }
            }
            return apply_function(ident, &args);
        }
        match ident {
            "pi" => Ok(std::f64::consts::PI),
            "e" => Ok(std::f64::consts::E),
            _ => Err(format!("unknown identifier '{ident}'")),
        }
    }

    fn parse_number(&mut self) -> std::result::Result<f64, String> {
        let start = self.pos;
        let mut saw_digit = false;
        while self.peek().map(|ch| ch.is_ascii_digit()).unwrap_or(false) {
            saw_digit = true;
            self.bump();
        }
        if self.consume('.') {
            while self.peek().map(|ch| ch.is_ascii_digit()).unwrap_or(false) {
                saw_digit = true;
                self.bump();
            }
        }
        if !saw_digit {
            return Err(format!("invalid number at '{}'", &self.input[start..]));
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let exp_start = self.pos;
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            let mut exp_digits = 0usize;
            while self.peek().map(|ch| ch.is_ascii_digit()).unwrap_or(false) {
                exp_digits += 1;
                self.bump();
            }
            if exp_digits == 0 {
                return Err(format!(
                    "invalid exponent at '{}'",
                    &self.input[exp_start..]
                ));
            }
        }
        self.input[start..self.pos]
            .parse::<f64>()
            .map_err(|e| format!("invalid number '{}': {e}", &self.input[start..self.pos]))
    }

    fn expect(&mut self, expected: char) -> std::result::Result<(), String> {
        self.skip_ws();
        if self.consume(expected) {
            Ok(())
        } else {
            Err(format!("expected '{expected}' at '{}'", self.remaining()))
        }
    }

    fn consume(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn take_while(&mut self, mut pred: impl FnMut(char) -> bool) -> &'a str {
        let start = self.pos;
        while self.peek().map(&mut pred).unwrap_or(false) {
            self.bump();
        }
        &self.input[start..self.pos]
    }

    fn skip_ws(&mut self) {
        while self.peek().map(|ch| ch.is_whitespace()).unwrap_or(false) {
            self.bump();
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn remaining(&self) -> &str {
        &self.input[self.pos..]
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.input.len()
    }
}

fn apply_function(name: &str, args: &[f64]) -> std::result::Result<f64, String> {
    match name {
        "sqrt" => expect_arity(name, args, 1).map(|_| args[0].sqrt()),
        "sin" => expect_arity(name, args, 1).map(|_| args[0].sin()),
        "cos" => expect_arity(name, args, 1).map(|_| args[0].cos()),
        "tan" => expect_arity(name, args, 1).map(|_| args[0].tan()),
        "ln" => expect_arity(name, args, 1).map(|_| args[0].ln()),
        "log" => expect_arity(name, args, 1).map(|_| args[0].log10()),
        "exp" => expect_arity(name, args, 1).map(|_| args[0].exp()),
        "abs" => expect_arity(name, args, 1).map(|_| args[0].abs()),
        "floor" => expect_arity(name, args, 1).map(|_| args[0].floor()),
        "ceil" => expect_arity(name, args, 1).map(|_| args[0].ceil()),
        "round" => expect_arity(name, args, 1).map(|_| args[0].round()),
        "min" => {
            if args.is_empty() {
                Err("min expects at least 1 argument".to_string())
            } else {
                Ok(args.iter().copied().fold(f64::INFINITY, f64::min))
            }
        }
        "max" => {
            if args.is_empty() {
                Err("max expects at least 1 argument".to_string())
            } else {
                Ok(args.iter().copied().fold(f64::NEG_INFINITY, f64::max))
            }
        }
        _ => Err(format!("unknown function '{name}'")),
    }
}

fn expect_arity(name: &str, args: &[f64], expected: usize) -> std::result::Result<(), String> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(format!(
            "{name} expects {expected} argument(s), got {}",
            args.len()
        ))
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
        tool.execute(json!({ "expression": expr }), &ctx)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn calc_basic_arithmetic() {
        assert_eq!(eval("2 * (3 + 4)").await, "14");
        assert_eq!(eval("10 / 4").await, "2.5");
        assert_eq!(eval("2 ^ 10").await, "1024");
        assert_eq!(eval("2 ^ 3 ^ 2").await, "512");
    }

    #[tokio::test]
    async fn calc_functions_and_constants() {
        assert_eq!(eval("sqrt(9) + max(1, 4, 2)").await, "7");
        assert_eq!(eval("sin(pi / 2)").await, "1");
        assert_eq!(eval("ln(e)").await, "1");
    }

    #[tokio::test]
    async fn calc_empty_and_error() {
        assert!(eval("").await.contains("required"));
        assert!(eval("1 +").await.starts_with("Error"));
        assert!(eval("unknown(1)").await.starts_with("Error"));
    }
}
