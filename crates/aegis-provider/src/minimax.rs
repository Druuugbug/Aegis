//! MiniMax dialect adapter for quota / rate-limit detection.
//!
//! MiniMax's OpenAI-compatible endpoint differs from standard OpenAI in ways
//! that matter for long-running, quota-aware execution:
//!
//! 1. **Errors live in the body, not the HTTP status.** MiniMax frequently
//!    returns `HTTP 200` with the real result code in `base_resp.status_code`
//!    (`0` == success). Inspecting only the HTTP status silently misses quota
//!    exhaustion and would parse an error response into an empty message.
//! 2. **Quota exhaustion carries no reset time.** The token-plan limit surfaces
//!    as `base_resp.status_code = 2056` ("usage limit exceeded / please wait for
//!    the resource release in the next 5-hour window") with no `Retry-After`.
//! 3. **Fixed 5-hour windows.** MiniMax resets at fixed local-time boundaries
//!    (`00:00 / 05:00 / 10:00 / 15:00 / 20:00`), not rolling windows, plus a
//!    weekly cap. We therefore compute the wait ourselves.
//!
//! References: MiniMax API error-code table and rate-limit docs (fixed 5-hour
//! windows). See `devdocs/design-quota-aware-endurance.md` §8.

use serde_json::Value;
use std::time::Duration;

/// Seconds in a day.
const DAY_SECS: u64 = 86_400;

/// Fixed reset-window boundaries within a local day, in seconds since local
/// midnight: 00:00, 05:00, 10:00, 15:00, 20:00.
const WINDOW_BOUNDARIES_SECS: [u64; 5] = [0, 5 * 3600, 10 * 3600, 15 * 3600, 20 * 3600];

/// Default assumed timezone offset for MiniMax reset windows (UTC+8, Beijing).
/// Configurable on the provider; if wrong, the endurance escalation ladder
/// compensates by extending the wait after an immediate re-hit.
pub const DEFAULT_TZ_OFFSET_SECS: i64 = 8 * 3600;

/// Outcome of classifying a MiniMax `base_resp.status_code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MiniMaxOutcome {
    /// `status_code == 0`: success, parse normally.
    Ok,
    /// `2056`: 5-hour window or weekly usage cap exhausted. Wait until the next
    /// fixed window boundary (self-computed; MiniMax gives no reset time).
    UsageWindowExhausted,
    /// Per-minute rate/token/connection limits (`1002` RPM, `1039` TPM,
    /// `1041` conn, `2045` rate-growth): short exponential backoff.
    TransientRateLimit { code: i64 },
    /// `1008`: insufficient balance. Hard stop — waiting does not help.
    InsufficientBalance,
    /// Transient server-side errors (`1000` unknown, `1001` timeout,
    /// `1024` internal, `1033` system): retry with backoff.
    TransientServer { code: i64 },
    /// Any other non-zero code we do not special-case.
    Other { code: i64, msg: String },
}

/// Classify a MiniMax response body via its `base_resp` field.
///
/// Returns `None` when the body has no `base_resp` (a standard OpenAI-shaped
/// response, or unparseable text) — callers should then fall back to normal
/// OpenAI handling, so this is safe to run against real OpenAI responses.
pub fn classify_body(body: &str) -> Option<MiniMaxOutcome> {
    let v: Value = serde_json::from_str(body).ok()?;
    classify_value(&v)
}

/// Classify an already-parsed MiniMax response value via its `base_resp` field.
///
/// Useful on the streaming path where each SSE `data:` chunk is parsed to a
/// [`Value`] before inspection. Returns `None` when there is no `base_resp`.
pub fn classify_value(v: &Value) -> Option<MiniMaxOutcome> {
    let base = v.get("base_resp")?;
    let code = base.get("status_code").and_then(Value::as_i64)?;
    let msg = base
        .get("status_msg")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    Some(match code {
        0 => MiniMaxOutcome::Ok,
        2056 => MiniMaxOutcome::UsageWindowExhausted,
        1002 | 1039 | 1041 | 2045 => MiniMaxOutcome::TransientRateLimit { code },
        1008 => MiniMaxOutcome::InsufficientBalance,
        1000 | 1001 | 1024 | 1033 => MiniMaxOutcome::TransientServer { code },
        other => MiniMaxOutcome::Other { code: other, msg },
    })
}

/// Compute the duration until the next fixed 5-hour reset window boundary.
///
/// * `now_unix_secs` — current time as UTC unix seconds.
/// * `tz_offset_secs` — account timezone offset from UTC (e.g. `+8h` = 28800).
/// * `margin` — safety margin added to the wait so we do not wake a hair early
///   and immediately re-trip the limit.
///
/// Boundaries are `00:00 / 05:00 / 10:00 / 15:00 / 20:00` in local time. Past
/// `20:00`, the next boundary is the following local midnight.
pub fn wait_until_next_window(
    now_unix_secs: u64,
    tz_offset_secs: i64,
    margin: Duration,
) -> Duration {
    // Seconds since local midnight (rem_euclid keeps this in [0, DAY_SECS)).
    let local = (now_unix_secs as i64 + tz_offset_secs).rem_euclid(DAY_SECS as i64) as u64;

    let next = WINDOW_BOUNDARIES_SECS
        .iter()
        .copied()
        .find(|&b| b > local)
        .unwrap_or(DAY_SECS); // after 20:00 → next local midnight

    Duration::from_secs(next - local) + margin
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_body ──

    #[test]
    fn success_code_is_ok() {
        let body = r#"{"choices":[],"base_resp":{"status_code":0,"status_msg":"success"}}"#;
        assert_eq!(classify_body(body), Some(MiniMaxOutcome::Ok));
    }

    #[test]
    fn usage_limit_2056_is_window_exhausted() {
        let body = r#"{"base_resp":{"status_code":2056,"status_msg":"usage limit exceeded"}}"#;
        assert_eq!(
            classify_body(body),
            Some(MiniMaxOutcome::UsageWindowExhausted)
        );
    }

    #[test]
    fn rpm_and_tpm_limits_are_transient_rate_limits() {
        for code in [1002, 1039, 1041, 2045] {
            let body = format!(r#"{{"base_resp":{{"status_code":{code},"status_msg":"x"}}}}"#);
            assert_eq!(
                classify_body(&body),
                Some(MiniMaxOutcome::TransientRateLimit { code }),
                "code {code} should be a transient rate limit"
            );
        }
    }

    #[test]
    fn insufficient_balance_1008() {
        let body = r#"{"base_resp":{"status_code":1008,"status_msg":"insufficient balance"}}"#;
        assert_eq!(classify_body(body), Some(MiniMaxOutcome::InsufficientBalance));
    }

    #[test]
    fn transient_server_errors() {
        for code in [1000, 1001, 1024, 1033] {
            let body = format!(r#"{{"base_resp":{{"status_code":{code},"status_msg":"x"}}}}"#);
            assert_eq!(
                classify_body(&body),
                Some(MiniMaxOutcome::TransientServer { code })
            );
        }
    }

    #[test]
    fn unknown_code_is_other() {
        let body = r#"{"base_resp":{"status_code":2013,"status_msg":"invalid params"}}"#;
        assert_eq!(
            classify_body(body),
            Some(MiniMaxOutcome::Other {
                code: 2013,
                msg: "invalid params".to_string()
            })
        );
    }

    #[test]
    fn standard_openai_body_has_no_base_resp() {
        // A normal OpenAI response must NOT be misclassified.
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}]}"#;
        assert_eq!(classify_body(body), None);
    }

    #[test]
    fn non_json_body_returns_none() {
        assert_eq!(classify_body("not json at all"), None);
        assert_eq!(classify_body(""), None);
    }

    // ── wait_until_next_window ──

    // Use UTC (offset 0) in these tests so the arithmetic is easy to reason about.
    const NO_MARGIN: Duration = Duration::from_secs(0);

    /// Build a unix timestamp for a given hour:minute on an arbitrary UTC day.
    fn ts(hour: u64, min: u64) -> u64 {
        // 2026-01-01 was a reference; only the intra-day part matters here since
        // the function reduces modulo DAY_SECS.
        let day0 = 1_767_225_600; // 2026-01-01T00:00:00Z (00:00 boundary aligned)
        day0 + hour * 3600 + min * 60
    }

    #[test]
    fn before_first_boundary_waits_to_0500() {
        // 03:00 → next boundary 05:00 → 2h.
        let w = wait_until_next_window(ts(3, 0), 0, NO_MARGIN);
        assert_eq!(w, Duration::from_secs(2 * 3600));
    }

    #[test]
    fn just_before_1000_waits_short() {
        // 09:45 → next boundary 10:00 → 15m. (Matches the blogged real case.)
        let w = wait_until_next_window(ts(9, 45), 0, NO_MARGIN);
        assert_eq!(w, Duration::from_secs(15 * 60));
    }

    #[test]
    fn exactly_on_boundary_waits_to_next_boundary() {
        // 10:00 exactly → boundary must be strictly greater → 15:00 → 5h.
        let w = wait_until_next_window(ts(10, 0), 0, NO_MARGIN);
        assert_eq!(w, Duration::from_secs(5 * 3600));
    }

    #[test]
    fn after_last_boundary_rolls_to_next_midnight() {
        // 22:00 → past 20:00 → next local midnight → 2h.
        let w = wait_until_next_window(ts(22, 0), 0, NO_MARGIN);
        assert_eq!(w, Duration::from_secs(2 * 3600));
    }

    #[test]
    fn margin_is_added() {
        let w = wait_until_next_window(ts(3, 0), 0, Duration::from_secs(30));
        assert_eq!(w, Duration::from_secs(2 * 3600 + 30));
    }

    #[test]
    fn tz_offset_shifts_boundary() {
        // now = 01:00 UTC, offset +8h → local 09:00 → next boundary 10:00 → 1h.
        let w = wait_until_next_window(ts(1, 0), 8 * 3600, NO_MARGIN);
        assert_eq!(w, Duration::from_secs(3600));
    }

    #[test]
    fn negative_tz_offset_wraps_correctly() {
        // now = 02:00 UTC, offset -8h → local 18:00 (prev day) → next boundary 20:00 → 2h.
        let w = wait_until_next_window(ts(2, 0), -8 * 3600, NO_MARGIN);
        assert_eq!(w, Duration::from_secs(2 * 3600));
    }
}
