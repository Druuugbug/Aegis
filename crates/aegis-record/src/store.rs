use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use tracing::{debug, warn};

pub struct SessionStore {
    conn: Connection,
}

// ── Schema migrations ──

const MIGRATIONS: &[(&str, &str)] = &[(
    "001_initial",
    r#"
CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY,
    title           TEXT,
    model           TEXT,
    started_at      TEXT NOT NULL,
    ended_at        TEXT,
    message_count   INTEGER DEFAULT 0,
    input_tokens    INTEGER DEFAULT 0,
    output_tokens   INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL REFERENCES sessions(id),
    role            TEXT NOT NULL,
    content         TEXT,
    tool_call_id    TEXT,
    tool_calls      TEXT,
    tool_name       TEXT,
    reasoning       TEXT,
    record_type     TEXT NOT NULL DEFAULT 'message',
    timestamp       TEXT NOT NULL,
    finish_reason   TEXT
);

CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC);
CREATE INDEX IF NOT EXISTS idx_messages_record_type ON messages(record_type);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    content, session_id UNINDEXED, role UNINDEXED,
    content=messages, content_rowid=id
);

CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content, session_id, role)
    VALUES (new.id, new.content, new.session_id, new.role);
END;

CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content, session_id, role)
    VALUES ('delete', old.id, old.content, old.session_id, old.role);
END;

CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content, session_id, role)
    VALUES ('delete', old.id, old.content, old.session_id, old.role);
    INSERT INTO messages_fts(rowid, content, session_id, role)
    VALUES (new.id, new.content, new.session_id, new.role);
END;
"#,
),
    (
        "002_usage_ledger",
        r#"
CREATE TABLE IF NOT EXISTS usage (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id    TEXT,
    model         TEXT,
    input_tokens  INTEGER DEFAULT 0,
    output_tokens INTEGER DEFAULT 0,
    cost_usd      REAL    DEFAULT 0,
    timestamp     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_usage_ts ON usage(timestamp);
"#,
    ),
];

fn run_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version TEXT PRIMARY KEY,
            applied_at TEXT NOT NULL
        );",
    )
    .context("creating schema_version table")?;

    for (version, sql) in MIGRATIONS {
        let applied: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM schema_version WHERE version = ?1",
            params![version],
            |row| row.get(0),
        )?;
        if !applied {
            debug!(version, "applying migration");
            conn.execute_batch(sql)
                .with_context(|| format!("migration {version}"))?;
            conn.execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                params![version, Utc::now().to_rfc3339()],
            )?;
        }
    }
    Ok(())
}

// ── Retry helper for SQLITE_BUSY ──

fn retry_busy<T>(mut f: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    for attempt in 0..5 {
        match f() {
            Ok(v) => return Ok(v),
            Err(ref e) if is_busy(e) && attempt < 4 => {
                let jitter = rand::random::<u64>() % 50;
                let delay = Duration::from_millis(50 * (1 << attempt) + jitter);
                warn!(
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    "SQLITE_BUSY, retrying"
                );
                std::thread::sleep(delay);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ffi::ErrorCode::DatabaseBusy)
}

// ── Record types ──

#[derive(Debug, Clone, Copy)]
pub enum RecordType {
    /// Raw user input
    Input,
    /// LLM API call (request + response)
    LlmCall,
    /// Tool invocation request
    ToolCall,
    /// Tool execution result
    ToolResult,
    /// Final assistant output shown to user
    Output,
    /// Feedback / success signal collected at task end
    Feedback,
    /// Strategy created, updated, or retired
    StrategyUpdate,
    /// Goal created, updated, or completed
    GoalUpdate,
    // Legacy alias kept for back-compat
    Message,
}

impl RecordType {
    /// Returns the string representation of this value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::LlmCall => "llm_call",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::Output => "output",
            Self::Feedback => "feedback",
            Self::StrategyUpdate => "strategy_update",
            Self::GoalUpdate => "goal_update",
            Self::Message => "message",
        }
    }

    /// Retention policy in days. None = permanent.
    pub fn retention_days(&self) -> Option<u32> {
        match self {
            Self::StrategyUpdate | Self::GoalUpdate => None, // permanent
            Self::Input | Self::Output | Self::Feedback | Self::Message => Some(90),
            Self::ToolCall | Self::ToolResult => Some(30),
            Self::LlmCall => Some(7),
        }
    }
}

// ── SessionStore ──

/// One aggregated row of token-usage history (a time bucket, a model, or a
/// grand total). `bucket` is the group label (date `YYYY-MM-DD`, model name, or
/// `"total"`).
#[derive(Debug, Clone)]
pub struct UsageRow {
    pub bucket: String,
    pub input: u64,
    pub output: u64,
    pub cost_usd: f64,
    pub calls: u64,
}

impl SessionStore {
    /// Opens or creates the store at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).context("opening session database")?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .context("setting pragmas")?;
        run_migrations(&conn)?;
        Ok(Self { conn })
    }

    /// Creates a new session record.
    pub fn create_session(&self, id: &str, model: &str) -> Result<()> {
        retry_busy(|| {
            self.conn.execute(
                "INSERT INTO sessions (id, model, started_at) VALUES (?1, ?2, ?3)",
                params![id, model, Utc::now().to_rfc3339()],
            )
        })?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_message(
        &self,
        session_id: &str,
        role: &str,
        content: Option<&str>,
        tool_call_id: Option<&str>,
        tool_calls_json: Option<&str>,
        tool_name: Option<&str>,
        reasoning: Option<&str>,
        finish_reason: Option<&str>,
        record_type: RecordType,
    ) -> Result<()> {
        retry_busy(|| {
            self.conn.execute(
                "INSERT INTO messages (session_id, role, content, tool_call_id, tool_calls, tool_name, reasoning, record_type, timestamp, finish_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    session_id, role, content, tool_call_id, tool_calls_json,
                    tool_name, reasoning, record_type.as_str(),
                    Utc::now().to_rfc3339(), finish_reason,
                ],
            )
        })?;
        retry_busy(|| {
            self.conn.execute(
                "UPDATE sessions SET message_count = message_count + 1 WHERE id = ?1",
                params![session_id],
            )
        })?;
        Ok(())
    }

    /// Retrieves all messages for a session.
    pub fn get_messages(&self, session_id: &str) -> Result<Vec<MessageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT role, content, tool_call_id, tool_calls, tool_name, reasoning, finish_reason, record_type
             FROM messages WHERE session_id = ?1 ORDER BY timestamp"
        )?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(MessageRow {
                    role: row.get(0)?,
                    content: row.get(1)?,
                    tool_call_id: row.get(2)?,
                    tool_calls: row.get(3)?,
                    tool_name: row.get(4)?,
                    reasoning: row.get(5)?,
                    finish_reason: row.get(6)?,
                    record_type: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Searches for entries matching the query.
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<SearchResult>> {
        let sanitized = sanitize_fts5(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.session_id, m.role, snippet(messages_fts, 0, '>>>', '<<<', '...', 32) as snip
             FROM messages_fts f
             JOIN messages m ON m.id = f.rowid
             WHERE messages_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![sanitized, limit], |row| {
                Ok(SearchResult {
                    session_id: row.get(0)?,
                    role: row.get(1)?,
                    snippet: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Lists recent sessions.
    pub fn list_sessions(&self, limit: u32) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model, started_at, message_count
             FROM sessions ORDER BY started_at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    model: row.get(2)?,
                    started_at: row.get(3)?,
                    message_count: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Marks a session as ended.
    pub fn end_session(&self, id: &str) -> Result<()> {
        retry_busy(|| {
            self.conn.execute(
                "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
                params![Utc::now().to_rfc3339(), id],
            )
        })?;
        Ok(())
    }

    /// Updates token usage counters for a session.
    pub fn update_tokens(&self, id: &str, input: u32, output: u32) -> Result<()> {
        retry_busy(|| {
            self.conn.execute(
                "UPDATE sessions SET input_tokens = input_tokens + ?1, output_tokens = output_tokens + ?2 WHERE id = ?3",
                params![input, output, id],
            )
        })?;
        Ok(())
    }

    /// Append one LLM-call usage record to the ledger (parsed from the
    /// response's `usage`). `cost_usd` is the estimate at call time, frozen so
    /// later price-table changes don't rewrite history.
    pub fn record_usage(
        &self,
        session_id: &str,
        model: &str,
        input: u32,
        output: u32,
        cost_usd: f64,
    ) -> Result<()> {
        let ts = Utc::now().to_rfc3339();
        retry_busy(|| {
            self.conn.execute(
                "INSERT INTO usage (session_id, model, input_tokens, output_tokens, cost_usd, timestamp) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![session_id, model, input, output, cost_usd, ts],
            )
        })?;
        Ok(())
    }

    /// Build the `WHERE timestamp …` clause + bound params from an optional
    /// half-open `[from, to)` range (RFC3339 strings).
    fn usage_where(from: Option<&str>, to: Option<&str>) -> (String, Vec<String>) {
        let mut clauses: Vec<String> = Vec::new();
        let mut binds: Vec<String> = Vec::new();
        if let Some(f) = from {
            binds.push(f.to_string());
            clauses.push(format!("timestamp >= ?{}", binds.len()));
        }
        if let Some(t) = to {
            binds.push(t.to_string());
            clauses.push(format!("timestamp < ?{}", binds.len()));
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        (where_sql, binds)
    }

    /// Grand total of token usage over an optional time range.
    pub fn usage_total(&self, from: Option<&str>, to: Option<&str>) -> Result<UsageRow> {
        let (where_sql, binds) = Self::usage_where(from, to);
        let sql = format!(
            "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), \
             COALESCE(SUM(cost_usd),0.0), COUNT(*) FROM usage {where_sql}"
        );
        let params: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let row = self.conn.query_row(&sql, params.as_slice(), |r| {
            Ok(UsageRow {
                bucket: "total".to_string(),
                input: r.get::<_, i64>(0)? as u64,
                output: r.get::<_, i64>(1)? as u64,
                cost_usd: r.get::<_, f64>(2)?,
                calls: r.get::<_, i64>(3)? as u64,
            })
        })?;
        Ok(row)
    }

    /// Per-day aggregation (bucket = `YYYY-MM-DD`, UTC), oldest first.
    pub fn usage_by_day(&self, from: Option<&str>, to: Option<&str>) -> Result<Vec<UsageRow>> {
        self.usage_grouped("substr(timestamp,1,10)", from, to)
    }

    /// Per-model aggregation (bucket = model name), highest cost first.
    pub fn usage_by_model(&self, from: Option<&str>, to: Option<&str>) -> Result<Vec<UsageRow>> {
        self.usage_grouped("COALESCE(model,'?')", from, to)
    }

    fn usage_grouped(
        &self,
        group_expr: &str,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Vec<UsageRow>> {
        let (where_sql, binds) = Self::usage_where(from, to);
        let sql = format!(
            "SELECT {group_expr} AS bucket, COALESCE(SUM(input_tokens),0), \
             COALESCE(SUM(output_tokens),0), COALESCE(SUM(cost_usd),0.0), COUNT(*) \
             FROM usage {where_sql} GROUP BY bucket ORDER BY bucket ASC"
        );
        let params: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params.as_slice(), |r| {
                Ok(UsageRow {
                    bucket: r.get::<_, String>(0)?,
                    input: r.get::<_, i64>(1)? as u64,
                    output: r.get::<_, i64>(2)? as u64,
                    cost_usd: r.get::<_, f64>(3)?,
                    calls: r.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Sets the display title for a session.
    pub fn set_title(&self, id: &str, title: &str) -> Result<()> {
        retry_busy(|| {
            self.conn.execute(
                "UPDATE sessions SET title = ?1 WHERE id = ?2",
                params![title, id],
            )
        })?;
        Ok(())
    }

    /// Delete ended sessions older than `retention_days`.
    pub fn prune_sessions(&self, retention_days: u32) -> Result<u32> {
        let cutoff = (Utc::now() - chrono::Duration::days(retention_days as i64)).to_rfc3339();
        // Delete messages first (FK)
        retry_busy(|| {
            self.conn.execute(
                "DELETE FROM messages WHERE session_id IN (
                    SELECT id FROM sessions WHERE ended_at IS NOT NULL AND ended_at < ?1
                )",
                params![cutoff],
            )
        })?;
        let deleted = retry_busy(|| {
            self.conn.execute(
                "DELETE FROM sessions WHERE ended_at IS NOT NULL AND ended_at < ?1",
                params![cutoff],
            )
        })? as u32;
        Ok(deleted)
    }

    /// Export a session and its messages as JSON.
    pub fn export_session(&self, id: &str) -> Result<serde_json::Value> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model, started_at, message_count, input_tokens, output_tokens
             FROM sessions WHERE id = ?1",
        )?;
        let session = stmt
            .query_row(params![id], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "title": row.get::<_, Option<String>>(1)?,
                    "model": row.get::<_, Option<String>>(2)?,
                    "started_at": row.get::<_, String>(3)?,
                    "message_count": row.get::<_, i64>(4)?,
                    "input_tokens": row.get::<_, i64>(5)?,
                    "output_tokens": row.get::<_, i64>(6)?,
                }))
            })
            .map_err(|_| anyhow::anyhow!("Session not found: {id}"))?;

        let messages = self.get_messages(id)?;

        let msgs_json: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| {
                serde_json::json!({
                    "role": m.role,
                    "content": m.content,
                    "record_type": m.record_type,
                })
            })
            .collect();

        let mut result = session;
        result["messages"] = serde_json::Value::Array(msgs_json);
        Ok(result)
    }
}

fn sanitize_fts5(query: &str) -> String {
    query
        .split_whitespace()
        .map(|w| {
            let clean: String = w
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if clean.is_empty() {
                String::new()
            } else {
                format!("\"{clean}\"")
            }
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

// ── Row types ──

#[derive(Debug)]
pub struct MessageRow {
    pub role: String,
    pub content: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_name: Option<String>,
    pub reasoning: Option<String>,
    pub finish_reason: Option<String>,
    pub record_type: String,
}

#[derive(Debug)]
pub struct SearchResult {
    pub session_id: String,
    pub role: String,
    pub snippet: String,
}

#[derive(Debug)]
pub struct SessionRow {
    pub id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub started_at: String,
    pub message_count: i64,
}

// ── Record & RecordStore ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: Option<String>,
    pub record_type: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordStats {
    pub total: u64,
    pub by_type: HashMap<String, u64>,
    pub oldest: Option<DateTime<Utc>>,
    pub newest: Option<DateTime<Utc>>,
}

pub struct RecordStore {
    conn: Connection,
}

impl RecordStore {
    /// Opens or creates the store at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).context("opening record database")?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .context("setting pragmas")?;
        run_migrations(&conn)?;
        Ok(Self { conn })
    }

    /// Inserts a new record into the store.
    pub fn insert(&self, session_id: &str, role: &str, content: Option<&str>, record_type: RecordType) -> Result<()> {
        retry_busy(|| {
            self.conn.execute(
                "INSERT INTO messages (session_id, role, content, record_type, timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session_id, role, content, record_type.as_str(), Utc::now().to_rfc3339()],
            )
        })?;
        Ok(())
    }

    /// Searches for entries matching the query.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Record>> {
        let pattern = format!("%{}%", query);
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, content, record_type, timestamp
             FROM messages WHERE content LIKE ?1 COLLATE NOCASE
             ORDER BY timestamp DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![pattern, limit as u32], |row| {
                Ok(Record {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    record_type: row.get(4)?,
                    timestamp: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Returns aggregate statistics for the store.
    pub fn stats(&self) -> Result<RecordStats> {
        let total: u64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages", [], |row| row.get(0),
        )?;

        let mut by_type = HashMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT record_type, COUNT(*) FROM messages GROUP BY record_type",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        for row in rows {
            let (rt, count) = row?;
            by_type.insert(rt, count);
        }

        let oldest: Option<DateTime<Utc>> = self.conn
            .query_row("SELECT MIN(timestamp) FROM messages", [], |row| row.get::<_, Option<String>>(0))
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());

        let newest: Option<DateTime<Utc>> = self.conn
            .query_row("SELECT MAX(timestamp) FROM messages", [], |row| row.get::<_, Option<String>>(0))
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());

        Ok(RecordStats { total, by_type, oldest, newest })
    }

    /// Exports all records to a JSONL file.
    pub fn export_jsonl(&self, path: &Path) -> Result<usize> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, content, record_type, timestamp
             FROM messages ORDER BY timestamp",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Record {
                id: row.get(0)?,
                session_id: row.get(1)?,
                role: row.get(2)?,
                content: row.get(3)?,
                record_type: row.get(4)?,
                timestamp: row.get(5)?,
            })
        })?;

        let mut file = std::fs::File::create(path).context("creating jsonl file")?;
        let mut count = 0;
        for row in rows {
            let record = row?;
            serde_json::to_writer(&mut file, &record)?;
            writeln!(file)?;
            count += 1;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_store() -> (SessionStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let store = SessionStore::open(&dir.path().join("test.db")).unwrap();
        (store, dir)
    }

    #[test]
    fn test_create_and_list_session() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        let sessions = store.list_sessions(10).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s1");
    }

    #[test]
    fn test_append_and_get_messages() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.append_message("s1", "user", Some("hello"), None, None, None, None, None, RecordType::Message).unwrap();
        store.append_message("s1", "assistant", Some("hi there"), None, None, None, None, Some("stop"), RecordType::Message).unwrap();
        let msgs = store.get_messages("s1").unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].content.as_deref(), Some("hi there"));
    }

    #[test]
    fn test_fts5_search() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.append_message("s1", "user", Some("deploy to kubernetes"), None, None, None, None, None, RecordType::Message).unwrap();
        store.append_message("s1", "assistant", Some("I will help with k8s deployment"), None, None, None, None, None, RecordType::Message).unwrap();
        let results = store.search("kubernetes", 10).unwrap();
        assert!(!results.is_empty());
        assert!(results[0].snippet.contains("kubernetes"));
    }

    #[test]
    fn test_search_empty_query() {
        let (store, _dir) = test_store();
        let results = store.search("", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_end_session() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.end_session("s1").unwrap();
    }

    #[test]
    fn test_update_tokens() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.update_tokens("s1", 100, 50).unwrap();
        store.update_tokens("s1", 200, 100).unwrap();
    }

    #[test]
    fn test_set_title() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.set_title("s1", "My Chat").unwrap();
        let sessions = store.list_sessions(10).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("My Chat"));
    }

    #[test]
    fn test_prune_sessions() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.end_session("s1").unwrap();
        // Prune with 0 days should delete it
        let deleted = store.prune_sessions(0).unwrap();
        assert_eq!(deleted, 1);
        let sessions = store.list_sessions(10).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_export_session() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.append_message("s1", "user", Some("hello"), None, None, None, None, None, RecordType::Message).unwrap();
        let json = store.export_session("s1").unwrap();
        assert_eq!(json["id"], "s1");
        assert_eq!(json["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_record_type_in_messages() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.append_message("s1", "tool", Some("result"), None, None, None, None, None, RecordType::ToolResult).unwrap();
        let msgs = store.get_messages("s1").unwrap();
        assert_eq!(msgs[0].record_type, "tool_result");
    }

    #[test]
    fn test_schema_migration_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        // Open twice — migrations should be idempotent
        let _s1 = SessionStore::open(&path).unwrap();
        let _s2 = SessionStore::open(&path).unwrap();
    }

    fn test_record_store() -> (RecordStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = RecordStore::open(&path).unwrap();
        // Create a session to satisfy FK constraint
        store.conn.execute(
            "INSERT INTO sessions (id, model, started_at) VALUES ('s1', 'test', '2026-01-01T00:00:00Z')",
            [],
        ).unwrap();
        (store, dir)
    }

    #[test]
    fn test_search() {
        let (store, _dir) = test_record_store();
        store.insert("s1", "user", Some("deploy to kubernetes"), RecordType::Input).unwrap();
        store.insert("s1", "assistant", Some("hello world"), RecordType::Output).unwrap();
        store.insert("s1", "user", Some("Kubernetes cluster setup"), RecordType::Input).unwrap();

        let results = store.search("kubernetes", 10).unwrap();
        assert_eq!(results.len(), 2);
        // case insensitive
        assert!(results.iter().all(|r| r.content.as_deref().unwrap().to_lowercase().contains("kubernetes")));
    }

    #[test]
    fn test_stats() {
        let (store, _dir) = test_record_store();
        store.insert("s1", "user", Some("hello"), RecordType::Input).unwrap();
        store.insert("s1", "assistant", Some("hi"), RecordType::Output).unwrap();
        store.insert("s1", "user", Some("bye"), RecordType::Input).unwrap();

        let stats = store.stats().unwrap();
        assert_eq!(stats.total, 3);
        assert_eq!(stats.by_type["input"], 2);
        assert_eq!(stats.by_type["output"], 1);
    }

    #[test]
    fn test_record_type_as_str() {
        assert_eq!(RecordType::Input.as_str(), "input");
        assert_eq!(RecordType::LlmCall.as_str(), "llm_call");
        assert_eq!(RecordType::ToolCall.as_str(), "tool_call");
        assert_eq!(RecordType::ToolResult.as_str(), "tool_result");
        assert_eq!(RecordType::Output.as_str(), "output");
        assert_eq!(RecordType::Feedback.as_str(), "feedback");
        assert_eq!(RecordType::StrategyUpdate.as_str(), "strategy_update");
        assert_eq!(RecordType::GoalUpdate.as_str(), "goal_update");
        assert_eq!(RecordType::Message.as_str(), "message");
    }

    #[test]
    fn test_record_type_retention_days() {
        // Permanent
        assert_eq!(RecordType::StrategyUpdate.retention_days(), None);
        assert_eq!(RecordType::GoalUpdate.retention_days(), None);
        // Temporary
        assert_eq!(RecordType::Input.retention_days(), Some(90));
        assert_eq!(RecordType::Output.retention_days(), Some(90));
        assert_eq!(RecordType::Feedback.retention_days(), Some(90));
        assert_eq!(RecordType::Message.retention_days(), Some(90));
        assert_eq!(RecordType::ToolCall.retention_days(), Some(30));
        assert_eq!(RecordType::ToolResult.retention_days(), Some(30));
        assert_eq!(RecordType::LlmCall.retention_days(), Some(7));
    }

    #[test]
    fn test_list_sessions_limit() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.create_session("s2", "claude").unwrap();
        store.create_session("s3", "gpt-4o-mini").unwrap();
        let all = store.list_sessions(100).unwrap();
        assert_eq!(all.len(), 3);
        let limited = store.list_sessions(2).unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn test_get_messages_ordering() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.append_message("s1", "user", Some("first"), None, None, None, None, None, RecordType::Message).unwrap();
        store.append_message("s1", "assistant", Some("second"), None, None, None, None, None, RecordType::Message).unwrap();
        store.append_message("s1", "user", Some("third"), None, None, None, None, None, RecordType::Message).unwrap();
        let msgs = store.get_messages("s1").unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].content.as_deref(), Some("first"));
        assert_eq!(msgs[1].content.as_deref(), Some("second"));
        assert_eq!(msgs[2].content.as_deref(), Some("third"));
    }

    #[test]
    fn test_append_with_tool_fields() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.append_message(
            "s1", "assistant", None,
            None, Some(r#"[{"id":"call_1","function":{"name":"terminal","arguments":"ls"}}]"#),
            None, Some("thinking..."), Some("tool_calls"),
            RecordType::ToolCall,
        ).unwrap();
        store.append_message(
            "s1", "tool", Some("file1.txt\nfile2.txt"),
            Some("call_1"), None, Some("terminal"), None, None,
            RecordType::ToolResult,
        ).unwrap();
        let msgs = store.get_messages("s1").unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "assistant");
        assert!(msgs[0].tool_calls.is_some());
        assert_eq!(msgs[0].reasoning.as_deref(), Some("thinking..."));
        assert_eq!(msgs[1].role, "tool");
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(msgs[1].tool_name.as_deref(), Some("terminal"));
    }

    #[test]
    fn test_record_store_insert_and_search_limit() {
        let (store, _dir) = test_record_store();
        for i in 0..5 {
            store.insert("s1", "user", Some(&format!("message {i}")), RecordType::Input).unwrap();
        }
        let all = store.search("message", 100).unwrap();
        assert_eq!(all.len(), 5);
        let limited = store.search("message", 2).unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn test_export_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = RecordStore::open(&path).unwrap();
        store.conn.execute(
            "INSERT INTO sessions (id, model, started_at) VALUES ('s1', 'test', '2026-01-01T00:00:00Z')",
            [],
        ).unwrap();
        store.insert("s1", "user", Some("hello"), RecordType::Input).unwrap();
        store.insert("s1", "assistant", Some("hi"), RecordType::Output).unwrap();

        let out_path = dir.path().join("export.jsonl");
        let count = store.export_jsonl(&out_path).unwrap();
        assert_eq!(count, 2);
        let content = std::fs::read_to_string(&out_path).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        // Each line is valid JSON
        for line in &lines {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn test_multiple_sessions_isolation() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.create_session("s2", "claude").unwrap();
        store.append_message("s1", "user", Some("msg from s1"), None, None, None, None, None, RecordType::Message).unwrap();
        store.append_message("s2", "user", Some("msg from s2"), None, None, None, None, None, RecordType::Message).unwrap();
        let msgs1 = store.get_messages("s1").unwrap();
        let msgs2 = store.get_messages("s2").unwrap();
        assert_eq!(msgs1.len(), 1);
        assert_eq!(msgs2.len(), 1);
        assert_eq!(msgs1[0].content.as_deref(), Some("msg from s1"));
        assert_eq!(msgs2[0].content.as_deref(), Some("msg from s2"));
    }

    #[test]
    fn test_search_no_results() {
        let (store, _dir) = test_store();
        store.create_session("s1", "gpt-4o").unwrap();
        store.append_message("s1", "user", Some("hello world"), None, None, None, None, None, RecordType::Message).unwrap();
        let results = store.search("kubernetes", 10).unwrap();
        assert!(results.is_empty());
    }
}
