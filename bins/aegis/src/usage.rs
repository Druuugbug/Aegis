//! `aegis usage` — historical token consumption, parsed from each LLM
//! response's `usage` and persisted in the per-call ledger (aegis-record). The
//! agent's `track_tokens` appends a row per call; this command aggregates them
//! over a time period and optionally breaks them down by day or by model.

use anyhow::Result;
use chrono::{Duration, Local, NaiveDate, TimeZone, Utc};

use aegis_record::{SessionStore, UsageRow};

/// Convert a local calendar date at 00:00 to a UTC RFC3339 instant string.
fn local_date_midnight_utc(d: NaiveDate) -> Option<String> {
    let ndt = d.and_hms_opt(0, 0, 0)?;
    let local = Local.from_local_datetime(&ndt).earliest()?;
    Some(local.with_timezone(&Utc).to_rfc3339())
}

/// Resolve the `[from, to)` UTC bounds from the time-period flags.
/// Precedence: explicit since/until > today > days > week > month > all-time.
fn resolve_range(
    today: bool,
    week: bool,
    month: bool,
    days: Option<i64>,
    since: Option<String>,
    until: Option<String>,
) -> Result<(Option<String>, Option<String>, String)> {
    // Explicit date range wins.
    if since.is_some() || until.is_some() {
        let from = match &since {
            Some(s) => {
                let d = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| {
                    anyhow::anyhow!("invalid --since date '{s}', expected YYYY-MM-DD")
                })?;
                local_date_midnight_utc(d)
            }
            None => None,
        };
        let to = match &until {
            Some(u) => {
                let d = NaiveDate::parse_from_str(u.trim(), "%Y-%m-%d").map_err(|_| {
                    anyhow::anyhow!("invalid --until date '{u}', expected YYYY-MM-DD")
                })?;
                // Inclusive of the until day: upper bound = next day 00:00.
                local_date_midnight_utc(d + Duration::days(1))
            }
            None => None,
        };
        let label = format!(
            "{} .. {}",
            since.as_deref().unwrap_or("beginning"),
            until.as_deref().unwrap_or("now")
        );
        return Ok((from, to, label));
    }

    let n_days = if today {
        Some(0)
    } else if let Some(d) = days {
        Some(d.max(0))
    } else if week {
        Some(6)
    } else if month {
        Some(29)
    } else {
        None
    };

    match n_days {
        None => Ok((None, None, "all time".to_string())),
        Some(n) => {
            let start_date = (Local::now() - Duration::days(n)).date_naive();
            let from = local_date_midnight_utc(start_date);
            let label = if n == 0 {
                "today".to_string()
            } else {
                format!("last {} days", n + 1)
            };
            Ok((from, None, label))
        }
    }
}

fn fmt_row(label_width: usize, r: &UsageRow) -> String {
    format!(
        "  {:<width$}  in {:>10}  out {:>10}  total {:>11}  calls {:>5}  ~${:.4}",
        r.bucket,
        r.input,
        r.output,
        r.input + r.output,
        r.calls,
        r.cost_usd,
        width = label_width
    )
}

/// Entry point for `aegis usage`.
#[allow(clippy::too_many_arguments)]
pub fn run_usage(
    today: bool,
    week: bool,
    month: bool,
    days: Option<i64>,
    since: Option<String>,
    until: Option<String>,
    by_day: bool,
    by_model: bool,
) -> Result<()> {
    let (from, to, label) = resolve_range(today, week, month, days, since, until)?;
    let store = crate::provider::open_store()?;
    print!(
        "{}",
        build_report(&store, &from, &to, &label, by_day, by_model)?
    );
    Ok(())
}

/// Build the textual usage report for a resolved range. Shared by the CLI
/// command and the REPL `/usage <period>`.
pub fn build_report(
    store: &SessionStore,
    from: &Option<String>,
    to: &Option<String>,
    label: &str,
    by_day: bool,
    by_model: bool,
) -> Result<String> {
    use std::fmt::Write as _;
    let (f, t) = (from.as_deref(), to.as_deref());
    let mut out = String::new();
    let _ = writeln!(out, "Token usage — {label}");

    if by_day {
        let rows = store.usage_by_day(f, t)?;
        if rows.is_empty() {
            let _ = writeln!(out, "  (no usage recorded in this period)");
        } else {
            for r in &rows {
                let _ = writeln!(out, "{}", fmt_row(10, r));
            }
        }
    }
    if by_model {
        let rows = store.usage_by_model(f, t)?;
        if rows.is_empty() {
            if !by_day {
                let _ = writeln!(out, "  (no usage recorded in this period)");
            }
        } else {
            let w = rows
                .iter()
                .map(|r| r.bucket.len())
                .max()
                .unwrap_or(10)
                .clamp(10, 40);
            for r in &rows {
                let _ = writeln!(out, "{}", fmt_row(w, r));
            }
        }
    }

    let total = store.usage_total(f, t)?;
    let sep_w = if by_model { 40 } else { 10 } + 60;
    let _ = writeln!(out, "  {:-<1$}", "", sep_w);
    let _ = writeln!(
        out,
        "  TOTAL: {} input + {} output = {} tokens over {} call(s), estimated ${:.4}",
        total.input,
        total.output,
        total.input + total.output,
        total.calls,
        total.cost_usd
    );
    Ok(out)
}

/// REPL entry: `/usage <args>` where args are space-separated words from
/// {today, week, month, all, by-day, by-model}. Returns the report text.
pub fn repl_report(store: &SessionStore, args: &str) -> Result<String> {
    let mut today = false;
    let mut week = false;
    let mut month = false;
    let mut by_day = false;
    let mut by_model = false;
    for w in args.split_whitespace() {
        match w.to_ascii_lowercase().as_str() {
            "today" => today = true,
            "week" => week = true,
            "month" => month = true,
            "all" => {} // all-time is the default range
            "by-day" | "byday" | "daily" => by_day = true,
            "by-model" | "bymodel" => by_model = true,
            _ => {}
        }
    }
    let (from, to, label) = resolve_range(today, week, month, None, None, None)?;
    build_report(store, &from, &to, &label, by_day, by_model)
}
