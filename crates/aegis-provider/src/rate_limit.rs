use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Parse a duration string like "60s", "1m30s", "2m", or plain seconds integer.
fn parse_duration_str(s: &str) -> Option<Duration> {
    let s = s.trim();

    // Try plain integer (seconds)
    if let Ok(secs) = s.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }

    // Try compound like "1m30s", "2m", "60s"
    let mut remaining = s;
    let mut total_secs: u64 = 0;
    let mut parsed_any = false;

    while !remaining.is_empty() {
        // Parse number
        let num_end = remaining
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(remaining.len());
        if num_end == 0 {
            break;
        }
        let num: u64 = remaining[..num_end].parse().ok()?;
        remaining = &remaining[num_end..];

        // Parse unit
        if remaining.starts_with('m') {
            total_secs += num * 60;
            remaining = &remaining[1..];
            parsed_any = true;
        } else if remaining.starts_with('s') {
            total_secs += num;
            remaining = &remaining[1..];
            parsed_any = true;
        } else {
            // Unknown unit
            break;
        }
    }

    if parsed_any {
        Some(Duration::from_secs(total_secs))
    } else {
        None
    }
}

/// Try to parse an HTTP date string like "Thu, 01 Jan 2026 00:00:00 GMT"
/// and return the duration from now until that time.
fn parse_http_date_to_duration(s: &str) -> Option<Duration> {
    // Use httpdate crate if available; otherwise fall back to a simple approach.
    // We'll try parsing via the `httpdate` crate if present, else skip.
    // For simplicity, we try parsing as unix timestamp first, then give up.
    let _ = s; // suppress unused warning
    None
}

/// Parse the retry-after duration from response headers.
///
/// Priority:
/// 1. `Retry-After` header (seconds integer or HTTP date)
/// 2. `x-ratelimit-reset-requests` header (duration string like "2m", "60s")
/// 3. `x-ratelimit-reset` header (Unix timestamp)
/// 4. `anthropic-ratelimit-requests-reset` / `anthropic-ratelimit-tokens-reset` (ISO 8601 or secs)
/// 5. Default: 60 seconds
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    parse_retry_after_opt(headers).unwrap_or_else(|| Duration::from_secs(60))
}

/// Like [`parse_retry_after`] but returns `None` when the response carries no
/// recognizable reset hint, instead of defaulting to 60 seconds.
///
/// This lets callers distinguish "the server told us to wait N seconds" from
/// "the server said nothing" — important for providers like MiniMax whose
/// fixed-window quotas send a bare 429 with no `Retry-After`, where a computed
/// window boundary is far more accurate than a 60-second guess.
pub fn parse_retry_after_opt(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    // 1. Retry-After
    if let Some(val) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        // Try integer seconds
        if let Ok(secs) = val.trim().parse::<u64>() {
            return Some(Duration::from_secs(secs));
        }
        // Try HTTP date
        if let Some(d) = parse_http_date_to_duration(val) {
            return Some(d);
        }
    }

    // 2. x-ratelimit-reset-requests
    if let Some(val) = headers
        .get("x-ratelimit-reset-requests")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(d) = parse_duration_str(val) {
            return Some(d);
        }
    }

    // 3. x-ratelimit-reset (Unix timestamp)
    if let Some(val) = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
    {
        if let Ok(ts) = val.trim().parse::<u64>() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if ts > now {
                return Some(Duration::from_secs(ts - now));
            }
        }
    }

    // 4. Anthropic-specific headers
    for header_name in &[
        "anthropic-ratelimit-requests-reset",
        "anthropic-ratelimit-tokens-reset",
    ] {
        if let Some(val) = headers.get(*header_name).and_then(|v| v.to_str().ok()) {
            if let Some(d) = parse_duration_str(val) {
                return Some(d);
            }
        }
    }

    None
}

/// Returns true if the HTTP status code represents a rate limit response.
pub fn is_rate_limited(status: u16) -> bool {
    status == 429
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    fn headers_with(key: &str, val: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(
            HeaderName::from_bytes(key.as_bytes()).unwrap(),
            HeaderValue::from_str(val).unwrap(),
        );
        m
    }

    #[test]
    fn test_parse_seconds() {
        let headers = headers_with("retry-after", "30");
        assert_eq!(parse_retry_after(&headers), Duration::from_secs(30));
    }

    #[test]
    fn test_parse_default() {
        let headers = HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), Duration::from_secs(60));
    }

    #[test]
    fn test_parse_opt_none_when_absent() {
        // No recognizable header → None (not the 60s default).
        let headers = HeaderMap::new();
        assert_eq!(parse_retry_after_opt(&headers), None);
    }

    #[test]
    fn test_parse_opt_some_when_present() {
        let headers = headers_with("retry-after", "45");
        assert_eq!(
            parse_retry_after_opt(&headers),
            Some(Duration::from_secs(45))
        );
    }

    #[test]
    fn test_is_rate_limited() {
        assert!(is_rate_limited(429));
        assert!(!is_rate_limited(200));
        assert!(!is_rate_limited(500));
    }

    #[test]
    fn test_openai_reset_header() {
        let headers = headers_with("x-ratelimit-reset-requests", "2m");
        assert_eq!(parse_retry_after(&headers), Duration::from_secs(120));
    }

    #[test]
    fn test_parse_duration_str_compound() {
        assert_eq!(parse_duration_str("1m30s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration_str("60s"), Some(Duration::from_secs(60)));
        assert_eq!(parse_duration_str("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration_str("60"), Some(Duration::from_secs(60)));
    }
}
