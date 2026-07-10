//! Reactive model context-window learning.
//!
//! Provider chat responses report only *used* tokens (`usage`), not the model's
//! context-window limit. But when a request is too large the provider returns an
//! error that states the real limit (e.g. OpenAI: "maximum context length is
//! 128000 tokens"; Anthropic: "... 250000 tokens > 200000 maximum"). We parse
//! that number and cache it per model in `~/.aegis/model_ctx.json`, so future
//! sessions budget against the true window without any extra config or network.

use std::collections::HashMap;
use std::path::PathBuf;

fn cache_path() -> PathBuf {
    dirs_next::home_dir()
        .unwrap_or_default()
        .join(".aegis")
        .join("model_ctx.json")
}

/// Plausible bounds for a learned context window (tokens) — reject garbage.
const MIN_CTX: u32 = 1_000;
const MAX_CTX: u32 = 20_000_000;

/// Learned context window (tokens) for a model, if previously observed.
pub fn load_learned(model: &str) -> Option<u32> {
    let content = std::fs::read_to_string(cache_path()).ok()?;
    let map: HashMap<String, u32> = serde_json::from_str(&content).ok()?;
    map.get(model).copied()
}

/// Record a learned context window for a model (merged into the cache).
pub fn record_learned(model: &str, tokens: u32) {
    if !(MIN_CTX..=MAX_CTX).contains(&tokens) {
        return;
    }
    let path = cache_path();
    let mut map: HashMap<String, u32> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default();
    if map.get(model) == Some(&tokens) {
        return; // unchanged
    }
    map.insert(model.to_string(), tokens);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(&path, s);
    }
}

/// Parse a provider error message for the model's context-window limit (tokens).
pub fn parse_context_limit(err: &str) -> Option<u32> {
    let lower = err.to_ascii_lowercase();

    // OpenAI / Azure: "maximum context length is 128000 tokens"
    const MARKER: &str = "maximum context length is ";
    if let Some(idx) = lower.find(MARKER) {
        if let Some(n) = first_number(&lower[idx + MARKER.len()..]) {
            if (MIN_CTX..=MAX_CTX).contains(&n) {
                return Some(n);
            }
        }
    }

    // Anthropic & others: "... N tokens > M maximum" → the number before
    // "maximum" is the limit.
    if let Some(idx) = lower.find("maximum") {
        if let Some(n) = last_number(&lower[..idx]) {
            if (MIN_CTX..=MAX_CTX).contains(&n) {
                return Some(n);
            }
        }
    }

    // Generic: "context window of 32768" / "context window is 32768".
    for marker in ["context window of ", "context window is ", "context length of "] {
        if let Some(idx) = lower.find(marker) {
            if let Some(n) = first_number(&lower[idx + marker.len()..]) {
                if (MIN_CTX..=MAX_CTX).contains(&n) {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// First run of digits in `s`, parsed.
fn first_number(s: &str) -> Option<u32> {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Last run of digits in `s`, parsed.
fn last_number(s: &str) -> Option<u32> {
    let chars: Vec<char> = s.chars().collect();
    let mut end = chars.len();
    while end > 0 && !chars[end - 1].is_ascii_digit() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && chars[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if end > start {
        chars[start..end].iter().collect::<String>().parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_format() {
        let e = "This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens.";
        assert_eq!(parse_context_limit(e), Some(128_000));
    }

    #[test]
    fn anthropic_format() {
        let e = "prompt is too long: 250000 tokens > 200000 maximum";
        assert_eq!(parse_context_limit(e), Some(200_000));
    }

    #[test]
    fn no_match() {
        assert_eq!(parse_context_limit("rate limit exceeded"), None);
    }
}
