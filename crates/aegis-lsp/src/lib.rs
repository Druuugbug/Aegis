//! Minimal Language Server Protocol (LSP) client for Aegis.
//!
//! Purpose: after the agent writes/patches a source file, spin up (or reuse) the
//! matching language server, open the document, collect `publishDiagnostics`, and
//! feed a concise summary back to the model — closing the "edit → see compile/type
//! errors → fix" loop without the model having to manually run a compiler.
//!
//! Design constraints:
//! - **No external crates** beyond `serde`/`serde_json`/`tokio` (already in the
//!   workspace) — we hand-roll the tiny slice of LSP we need (JSON-RPC over stdio
//!   with `Content-Length` framing, `initialize`/`didOpen`/`publishDiagnostics`).
//! - **Graceful degradation**: a missing/broken language server never breaks the
//!   write — callers get an empty diagnostic set and a logged warning.
//! - **Lazy + cached**: one server process per (language, root), reused across calls.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, Notify};

/// How to launch a language server for a set of file extensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSpec {
    /// Executable, e.g. "rust-analyzer".
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extensions (without dot) this server handles, e.g. ["rs"].
    pub extensions: Vec<String>,
}

/// Diagnostic severity (mirrors the LSP numeric codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Severity {
    fn from_lsp(n: i64) -> Self {
        match n {
            1 => Severity::Error,
            2 => Severity::Warning,
            3 => Severity::Information,
            _ => Severity::Hint,
        }
    }
    fn label(&self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Information => "info",
            Severity::Hint => "hint",
        }
    }
    /// Sort weight: errors first.
    fn rank(&self) -> u8 {
        match self {
            Severity::Error => 0,
            Severity::Warning => 1,
            Severity::Information => 2,
            Severity::Hint => 3,
        }
    }
}

/// A normalized diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    /// 1-based line.
    pub line: u64,
    /// 1-based column.
    pub col: u64,
    pub message: String,
    pub code: Option<String>,
}

// ── Pure helpers (unit-testable without a running server) ──

/// Convert a filesystem path to a `file://` URI (best-effort, POSIX-style).
pub fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// Frame a JSON-RPC payload with the LSP `Content-Length` header.
pub fn encode_message(payload: &str) -> Vec<u8> {
    let mut buf = format!("Content-Length: {}\r\n\r\n", payload.len()).into_bytes();
    buf.extend_from_slice(payload.as_bytes());
    buf
}

/// Parse the `Content-Length` out of a header block (text up to the blank line).
pub fn parse_content_length(header: &str) -> Option<usize> {
    for line in header.lines() {
        if let Some(v) = line
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
            .map(|s| s.trim().to_string())
        {
            return v.parse().ok();
        }
    }
    None
}

/// Extract `(uri, diagnostics)` from a `textDocument/publishDiagnostics` params.
pub fn parse_publish_diagnostics(params: &Value) -> Option<(String, Vec<Diagnostic>)> {
    let uri = params.get("uri")?.as_str()?.to_string();
    let arr = params.get("diagnostics").and_then(|d| d.as_array());
    let mut out = Vec::new();
    if let Some(arr) = arr {
        for d in arr {
            let sev = d
                .get("severity")
                .and_then(|s| s.as_i64())
                .map(Severity::from_lsp)
                .unwrap_or(Severity::Error);
            let start = d.get("range").and_then(|r| r.get("start"));
            let line = start
                .and_then(|s| s.get("line"))
                .and_then(|l| l.as_u64())
                .unwrap_or(0)
                + 1;
            let col = start
                .and_then(|s| s.get("character"))
                .and_then(|c| c.as_u64())
                .unwrap_or(0)
                + 1;
            let message = d
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            let code = d.get("code").map(|c| match c {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
            out.push(Diagnostic {
                severity: sev,
                line,
                col,
                message,
                code,
            });
        }
    }
    Some((uri, out))
}

/// Render a compact diagnostics section for injection into a tool result.
/// Keeps only errors + warnings, sorts errors first, truncates to `max`.
pub fn format_diagnostics(lang: &str, path: &Path, diags: &[Diagnostic], max: usize) -> String {
    let mut kept: Vec<&Diagnostic> = diags
        .iter()
        .filter(|d| matches!(d.severity, Severity::Error | Severity::Warning))
        .collect();
    if kept.is_empty() {
        return String::new();
    }
    kept.sort_by_key(|d| (d.severity.rank(), d.line, d.col));
    let errs = kept
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warns = kept.len() - errs;
    let total = kept.len();
    let file = path.display();
    let mut s = format!("\n── LSP diagnostics ({lang}, {errs} error(s), {warns} warning(s)) ──\n");
    for d in kept.iter().take(max) {
        let code = d
            .code
            .as_deref()
            .map(|c| format!(" [{c}]"))
            .unwrap_or_default();
        s.push_str(&format!(
            "{}{} {}:{}:{} {}\n",
            d.severity.label(),
            code,
            file,
            d.line,
            d.col,
            d.message.replace('\n', " ")
        ));
    }
    if total > max {
        s.push_str(&format!(
            "… ({} more; fix the above and retry)\n",
            total - max
        ));
    }
    s
}

/// Convert a `file://` URI back to a filesystem path (best-effort).
pub fn uri_to_path(uri: &str) -> String {
    uri.strip_prefix("file://")
        .map(|s| {
            // "file:///a/b" → "/a/b"; keep a leading slash.
            if let Some(rest) = s.strip_prefix('/') {
                if rest.chars().nth(1) == Some(':') {
                    // Windows drive: file:///C:/x → C:/x
                    rest.to_string()
                } else {
                    format!("/{rest}")
                }
            } else {
                s.to_string()
            }
        })
        .unwrap_or_else(|| uri.to_string())
}

/// Extract `"path:line:col"` entries from a `definition`/`references` result,
/// which may be `null`, a single `Location`/`LocationLink`, or an array.
pub fn format_locations(result: &Value) -> String {
    fn one(v: &Value) -> Option<String> {
        // Location: { uri, range:{start:{line,character}} }
        // LocationLink: { targetUri, targetSelectionRange:{start} }
        let uri = v
            .get("uri")
            .or_else(|| v.get("targetUri"))
            .and_then(|u| u.as_str())?;
        let range = v
            .get("range")
            .or_else(|| v.get("targetSelectionRange"))
            .or_else(|| v.get("targetRange"))?;
        let start = range.get("start")?;
        let line = start.get("line").and_then(|l| l.as_u64()).unwrap_or(0) + 1;
        let col = start.get("character").and_then(|c| c.as_u64()).unwrap_or(0) + 1;
        Some(format!("{}:{}:{}", uri_to_path(uri), line, col))
    }

    let mut out: Vec<String> = Vec::new();
    match result {
        Value::Null => {}
        Value::Array(arr) => {
            for v in arr {
                if let Some(s) = one(v) {
                    out.push(s);
                }
            }
        }
        v => {
            if let Some(s) = one(v) {
                out.push(s);
            }
        }
    }
    if out.is_empty() {
        "No results.".to_string()
    } else {
        out.join("\n")
    }
}

/// Extract readable text from a `hover` result (MarkupContent / MarkedString /
/// arrays thereof).
pub fn format_hover(result: &Value) -> String {
    fn marked(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o
                .get("value")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            _ => None,
        }
    }
    let contents = match result.get("contents") {
        Some(c) => c,
        None => return "No hover information.".to_string(),
    };
    let text = match contents {
        Value::Array(arr) => arr.iter().filter_map(marked).collect::<Vec<_>>().join("\n"),
        v => marked(v).unwrap_or_default(),
    };
    if text.trim().is_empty() {
        "No hover information.".to_string()
    } else {
        text
    }
}

/// Format a `documentSymbol` result: either `DocumentSymbol[]` (hierarchical,
/// has `range`) or `SymbolInformation[]` (flat, has `location`).
pub fn format_symbols(result: &Value) -> String {
    fn kind_name(n: i64) -> &'static str {
        // A useful subset of the LSP SymbolKind enum.
        match n {
            2 => "module",
            5 => "class",
            6 => "method",
            8 => "field",
            9 => "constructor",
            10 => "enum",
            11 => "interface",
            12 => "function",
            13 => "variable",
            14 => "constant",
            23 => "struct",
            _ => "symbol",
        }
    }
    fn walk(v: &Value, depth: usize, out: &mut Vec<String>) {
        let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let kind = v
            .get("kind")
            .and_then(|k| k.as_i64())
            .map(kind_name)
            .unwrap_or("symbol");
        // DocumentSymbol.range.start OR SymbolInformation.location.range.start
        let start = v
            .get("range")
            .or_else(|| v.get("selectionRange"))
            .or_else(|| v.get("location").and_then(|l| l.get("range")))
            .and_then(|r| r.get("start"));
        let line = start
            .and_then(|s| s.get("line"))
            .and_then(|l| l.as_u64())
            .map(|l| l + 1);
        let loc = line.map(|l| format!(":{l}")).unwrap_or_default();
        out.push(format!("{}{} {}{}", "  ".repeat(depth), kind, name, loc));
        if let Some(children) = v.get("children").and_then(|c| c.as_array()) {
            for c in children {
                walk(c, depth + 1, out);
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    if let Some(arr) = result.as_array() {
        for v in arr {
            walk(v, 0, &mut out);
        }
    }
    if out.is_empty() {
        "No symbols.".to_string()
    } else {
        out.join("\n")
    }
}

// ── Live client ──

/// A single language-server subprocess with a background reader that keeps the
/// latest diagnostics per document URI.
pub struct LspClient {
    child: Child,
    stdin: ChildStdin,
    next_id: AtomicI64,
    diagnostics: Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>,
    /// Responses to client→server requests, keyed by request id.
    responses: Arc<Mutex<HashMap<i64, Value>>>,
    /// URIs for which we have already sent `didOpen`.
    open_docs: Arc<Mutex<std::collections::HashSet<String>>>,
    notify: Arc<Notify>,
}

impl LspClient {
    /// Spawn `spec.command` and perform the `initialize`/`initialized` handshake
    /// rooted at `root`.
    pub async fn start(spec: &ServerSpec, root: &Path) -> anyhow::Result<Self> {
        let mut child = tokio::process::Command::new(&spec.command)
            .args(&spec.args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;

        let diagnostics: Arc<Mutex<HashMap<String, Vec<Diagnostic>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let responses: Arc<Mutex<HashMap<i64, Value>>> = Arc::new(Mutex::new(HashMap::new()));
        let open_docs: Arc<Mutex<std::collections::HashSet<String>>> =
            Arc::new(Mutex::new(std::collections::HashSet::new()));
        let notify = Arc::new(Notify::new());

        // Background reader: parse framed messages, capture publishDiagnostics
        // notifications and request responses (keyed by id).
        {
            let diagnostics = diagnostics.clone();
            let responses = responses.clone();
            let notify = notify.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_message(&mut reader).await {
                        Ok(Some(msg)) => {
                            if msg.get("method").and_then(|m| m.as_str())
                                == Some("textDocument/publishDiagnostics")
                            {
                                if let Some(params) = msg.get("params") {
                                    if let Some((uri, diags)) = parse_publish_diagnostics(params) {
                                        diagnostics.lock().await.insert(uri, diags);
                                        notify.notify_waiters();
                                    }
                                }
                            } else if let Some(id) = msg.get("id").and_then(|i| i.as_i64()) {
                                // A response to one of our requests.
                                if msg.get("result").is_some() || msg.get("error").is_some() {
                                    let val = msg.get("result").cloned().unwrap_or_else(|| {
                                        msg.get("error").cloned().unwrap_or(Value::Null)
                                    });
                                    responses.lock().await.insert(id, val);
                                    notify.notify_waiters();
                                }
                            }
                        }
                        Ok(None) => break, // EOF
                        Err(e) => {
                            tracing::debug!(target: "aegis::lsp", "reader stopped: {e}");
                            break;
                        }
                    }
                }
            });
        }

        let mut client = Self {
            child,
            stdin,
            next_id: AtomicI64::new(1),
            diagnostics,
            responses,
            open_docs,
            notify,
        };

        let root_uri = path_to_uri(root);
        let init = json!({
            "jsonrpc": "2.0",
            "id": client.next_id.fetch_add(1, Ordering::SeqCst),
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": { "relatedInformation": false },
                        "definition": { "dynamicRegistration": false, "linkSupport": true },
                        "references": { "dynamicRegistration": false },
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true }
                    }
                }
            }
        });
        client.send(&init).await?;
        client
            .send(&json!({"jsonrpc":"2.0","method":"initialized","params":{}}))
            .await?;
        Ok(client)
    }

    async fn send(&mut self, msg: &Value) -> anyhow::Result<()> {
        let payload = serde_json::to_string(msg)?;
        self.stdin.write_all(&encode_message(&payload)).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Open (or re-open) a document and wait up to `timeout` for its diagnostics.
    pub async fn diagnostics_for(
        &mut self,
        path: &Path,
        language_id: &str,
        timeout: Duration,
    ) -> Vec<Diagnostic> {
        let uri = path_to_uri(path);
        let text = std::fs::read_to_string(path).unwrap_or_default();
        // Clear any stale entry so we wait for a fresh publish.
        self.diagnostics.lock().await.remove(&uri);
        let did_open = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": self.next_id.fetch_add(1, Ordering::SeqCst),
                    "text": text
                }
            }
        });
        if self.send(&did_open).await.is_err() {
            return Vec::new();
        }
        // Wait for the reader to record diagnostics for our uri (or time out).
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(d) = self.diagnostics.lock().await.get(&uri) {
                return d.clone();
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Vec::new();
            }
            let _ = tokio::time::timeout(remaining, self.notify.notified()).await;
            if tokio::time::Instant::now() >= deadline {
                return self
                    .diagnostics
                    .lock()
                    .await
                    .get(&uri)
                    .cloned()
                    .unwrap_or_default();
            }
        }
    }

    /// Send a client→server request and wait up to `timeout` for its response.
    /// Returns the `result` value (or the `error` object) verbatim.
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&msg).await?;

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(resp) = self.responses.lock().await.remove(&id) {
                return Ok(resp);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("LSP request '{method}' timed out");
            }
            // Poll at most every 50ms so we never miss a `notify_waiters` that
            // fired between our map check and awaiting the notification.
            let slice = remaining.min(Duration::from_millis(50));
            let _ = tokio::time::timeout(slice, self.notify.notified()).await;
        }
    }

    /// Ensure the server has the document open (send `didOpen` once, then
    /// `didChange` with the current file contents on subsequent calls).
    async fn ensure_open(&mut self, path: &Path, language_id: &str) -> anyhow::Result<()> {
        let uri = path_to_uri(path);
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let version = self.next_id.fetch_add(1, Ordering::SeqCst);
        let already = self.open_docs.lock().await.contains(&uri);
        let msg = if already {
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri.clone(), "version": version },
                    "contentChanges": [ { "text": text } ]
                }
            })
        } else {
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri.clone(), "languageId": language_id,
                        "version": version, "text": text
                    }
                }
            })
        };
        self.send(&msg).await?;
        if !already {
            self.open_docs.lock().await.insert(uri);
        }
        Ok(())
    }

    /// `textDocument/definition` at a 0-based (line, character).
    pub async fn goto_definition(
        &mut self,
        path: &Path,
        language_id: &str,
        line0: u64,
        col0: u64,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        self.ensure_open(path, language_id).await?;
        let params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line0, "character": col0 }
        });
        self.request("textDocument/definition", params, timeout)
            .await
    }

    /// `textDocument/references` at a 0-based (line, character).
    pub async fn find_references(
        &mut self,
        path: &Path,
        language_id: &str,
        line0: u64,
        col0: u64,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        self.ensure_open(path, language_id).await?;
        let params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line0, "character": col0 },
            "context": { "includeDeclaration": true }
        });
        self.request("textDocument/references", params, timeout)
            .await
    }

    /// `textDocument/hover` at a 0-based (line, character).
    pub async fn hover(
        &mut self,
        path: &Path,
        language_id: &str,
        line0: u64,
        col0: u64,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        self.ensure_open(path, language_id).await?;
        let params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line0, "character": col0 }
        });
        self.request("textDocument/hover", params, timeout).await
    }

    /// `textDocument/documentSymbol` for the whole file.
    pub async fn document_symbols(
        &mut self,
        path: &Path,
        language_id: &str,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        self.ensure_open(path, language_id).await?;
        let params = json!({ "textDocument": { "uri": path_to_uri(path) } });
        self.request("textDocument/documentSymbol", params, timeout)
            .await
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // kill_on_drop is set, but be explicit.
        let _ = self.child.start_kill();
    }
}

/// Read one `Content-Length`-framed JSON-RPC message. Returns `Ok(None)` on EOF.
async fn read_message<R: AsyncReadExt + Unpin>(reader: &mut R) -> anyhow::Result<Option<Value>> {
    // Read header bytes until CRLFCRLF.
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Ok(None);
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() > 8192 {
            anyhow::bail!("header too large");
        }
    }
    let header_str = String::from_utf8_lossy(&header);
    let len =
        parse_content_length(&header_str).ok_or_else(|| anyhow::anyhow!("no content-length"))?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    let val: Value = serde_json::from_slice(&body)?;
    Ok(Some(val))
}

// ── Manager: route by extension, cache one client per (language, root) ──

/// Configuration for the manager: language name → server spec, plus timeouts.
#[derive(Debug, Clone, Default)]
pub struct LspSettings {
    pub servers: HashMap<String, ServerSpec>,
    pub timeout_ms: u64,
    pub max_diagnostics: usize,
}

/// Manages lazily-started language servers and routes files to them by extension.
pub struct LspManager {
    settings: LspSettings,
    /// key = "lang\u{0}root"
    clients: Mutex<HashMap<String, LspClient>>,
}

impl LspManager {
    pub fn new(settings: LspSettings) -> Self {
        Self {
            settings,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Find the (language, spec) that handles `path`'s extension.
    fn route(&self, path: &Path) -> Option<(String, ServerSpec)> {
        let ext = path.extension()?.to_string_lossy().to_string();
        self.settings
            .servers
            .iter()
            .find(|(_, spec)| spec.extensions.iter().any(|e| e == &ext))
            .map(|(lang, spec)| (lang.clone(), spec.clone()))
    }

    /// Collect + format diagnostics for a freshly-written file. Returns an empty
    /// string when LSP is not applicable or the server is unavailable (graceful).
    pub async fn diagnostics_summary(&self, path: &Path, root: &Path) -> String {
        let Some((lang, spec)) = self.route(path) else {
            return String::new();
        };
        let key = format!("{lang}\u{0}{}", root.display());
        let mut clients = self.clients.lock().await;
        if !clients.contains_key(&key) {
            match LspClient::start(&spec, root).await {
                Ok(c) => {
                    clients.insert(key.clone(), c);
                }
                Err(e) => {
                    tracing::warn!(target: "aegis::lsp", "failed to start {}: {e}", spec.command);
                    return String::new();
                }
            }
        }
        let client = clients.get_mut(&key).unwrap();
        let timeout = Duration::from_millis(self.settings.timeout_ms.max(200));
        let diags = client.diagnostics_for(path, &lang, timeout).await;
        format_diagnostics(&lang, path, &diags, self.settings.max_diagnostics.max(1))
    }

    /// Whether any configured server handles this path (cheap check for callers).
    pub fn handles(&self, path: &Path) -> bool {
        self.route(path).is_some()
    }

    /// Perform a navigation request (`definition`/`references`/`hover`/`symbols`)
    /// and return a formatted, human-readable result. `line`/`col` are 0-based.
    /// Returns a friendly message when LSP is not applicable/available.
    pub async fn navigate(
        &self,
        action: &str,
        path: &Path,
        root: &Path,
        line0: u64,
        col0: u64,
    ) -> String {
        let Some((lang, spec)) = self.route(path) else {
            return format!(
                "No language server configured for {} (add one under [lsp.servers]).",
                path.display()
            );
        };
        let key = format!("{lang}\u{0}{}", root.display());
        let mut clients = self.clients.lock().await;
        if !clients.contains_key(&key) {
            match LspClient::start(&spec, root).await {
                Ok(c) => {
                    clients.insert(key.clone(), c);
                }
                Err(e) => {
                    return format!("Failed to start language server '{}': {e}", spec.command);
                }
            }
        }
        let client = clients.get_mut(&key).unwrap();
        let timeout = Duration::from_millis(self.settings.timeout_ms.max(1000));

        let result = match action {
            "definition" => {
                client
                    .goto_definition(path, &lang, line0, col0, timeout)
                    .await
            }
            "references" => {
                client
                    .find_references(path, &lang, line0, col0, timeout)
                    .await
            }
            "hover" => client.hover(path, &lang, line0, col0, timeout).await,
            "symbols" => client.document_symbols(path, &lang, timeout).await,
            other => return format!("Unknown navigation action: '{other}'"),
        };

        match result {
            Ok(v) => match action {
                "definition" | "references" => format_locations(&v),
                "hover" => format_hover(&v),
                "symbols" => format_symbols(&v),
                _ => v.to_string(),
            },
            Err(e) => format!("LSP {action} failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_and_parse_header() {
        let framed = encode_message("{\"a\":1}");
        let text = String::from_utf8(framed).unwrap();
        assert!(text.starts_with("Content-Length: 7\r\n\r\n"));
        assert_eq!(parse_content_length("Content-Length: 42\r\n"), Some(42));
        assert_eq!(
            parse_content_length("content-length: 7\r\nX: y\r\n"),
            Some(7)
        );
        assert_eq!(parse_content_length("X: y\r\n"), None);
    }

    #[test]
    fn test_path_to_uri() {
        assert_eq!(path_to_uri(Path::new("/a/b.rs")), "file:///a/b.rs");
    }

    #[test]
    fn test_parse_publish_diagnostics() {
        let params = json!({
            "uri": "file:///a/b.rs",
            "diagnostics": [
                {
                    "severity": 1,
                    "range": {"start": {"line": 11, "character": 8}, "end": {"line": 11, "character": 9}},
                    "message": "mismatched types",
                    "code": "E0308"
                },
                {
                    "severity": 2,
                    "range": {"start": {"line": 19, "character": 4}, "end": {"line": 19, "character": 5}},
                    "message": "unused variable: x"
                }
            ]
        });
        let (uri, diags) = parse_publish_diagnostics(&params).unwrap();
        assert_eq!(uri, "file:///a/b.rs");
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].line, 12); // 0-based 11 → 1-based 12
        assert_eq!(diags[0].col, 9);
        assert_eq!(diags[0].code.as_deref(), Some("E0308"));
        assert_eq!(diags[1].severity, Severity::Warning);
    }

    #[test]
    fn test_format_diagnostics_orders_errors_first_and_truncates() {
        let diags = vec![
            Diagnostic {
                severity: Severity::Warning,
                line: 20,
                col: 5,
                message: "unused".into(),
                code: None,
            },
            Diagnostic {
                severity: Severity::Error,
                line: 12,
                col: 9,
                message: "type error".into(),
                code: Some("E0308".into()),
            },
            Diagnostic {
                severity: Severity::Hint,
                line: 1,
                col: 1,
                message: "hint".into(),
                code: None,
            },
        ];
        let out = format_diagnostics("rust", Path::new("src/foo.rs"), &diags, 10);
        assert!(out.contains("1 error(s), 1 warning(s)"));
        // error must appear before warning
        assert!(out.find("type error").unwrap() < out.find("unused").unwrap());
        // hint filtered out
        assert!(!out.contains("hint"));

        // truncation
        let out2 = format_diagnostics("rust", Path::new("src/foo.rs"), &diags, 1);
        assert!(out2.contains("more; fix the above"));
    }

    #[test]
    fn test_format_empty_when_no_errors_or_warnings() {
        let diags = vec![Diagnostic {
            severity: Severity::Hint,
            line: 1,
            col: 1,
            message: "h".into(),
            code: None,
        }];
        assert!(format_diagnostics("rust", Path::new("x.rs"), &diags, 5).is_empty());
    }

    #[test]
    fn test_manager_routes_by_extension() {
        let mut servers = HashMap::new();
        servers.insert(
            "rust".to_string(),
            ServerSpec {
                command: "rust-analyzer".into(),
                args: vec![],
                extensions: vec!["rs".into()],
            },
        );
        let mgr = LspManager::new(LspSettings {
            servers,
            timeout_ms: 1000,
            max_diagnostics: 20,
        });
        assert!(mgr.handles(Path::new("src/main.rs")));
        assert!(!mgr.handles(Path::new("README.md")));
    }

    #[test]
    fn test_uri_to_path() {
        assert_eq!(uri_to_path("file:///a/b.rs"), "/a/b.rs");
        assert_eq!(uri_to_path("file:///C:/x/y.rs"), "C:/x/y.rs");
        assert_eq!(uri_to_path("/already/path"), "/already/path");
    }

    #[test]
    fn test_format_locations_variants() {
        // Single Location
        let single = json!({
            "uri": "file:///a/b.rs",
            "range": {"start": {"line": 9, "character": 4}, "end": {"line": 9, "character": 8}}
        });
        assert_eq!(format_locations(&single), "/a/b.rs:10:5");

        // Array of LocationLink
        let arr = json!([
            {"targetUri": "file:///a/b.rs", "targetSelectionRange": {"start": {"line": 0, "character": 0}}},
            {"targetUri": "file:///c/d.rs", "targetSelectionRange": {"start": {"line": 41, "character": 2}}}
        ]);
        assert_eq!(format_locations(&arr), "/a/b.rs:1:1\n/c/d.rs:42:3");

        // Null → no results
        assert_eq!(format_locations(&Value::Null), "No results.");
    }

    #[test]
    fn test_format_hover_markup_and_marked() {
        let markup = json!({ "contents": { "kind": "markdown", "value": "fn foo()" } });
        assert_eq!(format_hover(&markup), "fn foo()");

        let marked_array = json!({ "contents": [ "line1", { "value": "line2" } ] });
        assert_eq!(format_hover(&marked_array), "line1\nline2");

        assert_eq!(format_hover(&json!({})), "No hover information.");
    }

    #[test]
    fn test_format_symbols_hierarchical_and_flat() {
        // DocumentSymbol (hierarchical)
        let doc = json!([
            {
                "name": "MyStruct", "kind": 23,
                "range": {"start": {"line": 2, "character": 0}},
                "children": [
                    {"name": "field", "kind": 8, "range": {"start": {"line": 3, "character": 4}}}
                ]
            }
        ]);
        let out = format_symbols(&doc);
        assert!(out.contains("struct MyStruct:3"));
        assert!(out.contains("  field field:4"));

        // SymbolInformation (flat, uses location)
        let flat = json!([
            {"name": "do_it", "kind": 12, "location": {"uri": "file:///a.rs", "range": {"start": {"line": 0, "character": 0}}}}
        ]);
        assert!(format_symbols(&flat).contains("function do_it:1"));

        assert_eq!(format_symbols(&json!([])), "No symbols.");
    }
}
