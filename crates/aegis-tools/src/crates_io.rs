//! `crates` tool — read-only queries against the Rust package ecosystem.
//!
//! Lets the agent look up crate metadata + latest version, search crates.io by
//! keyword, and check known security advisories (RustSec, via the OSV API).
//! It is **read-only**: it never modifies `Cargo.toml` or dependencies.

use crate::registry::{Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

/// crates.io requires a User-Agent or it rejects the request (403).
const UA: &str = "aegis-agent (https://github.com/Druuugbug/Aegis)";

/// Read-only crates.io / RustSec lookup tool.
pub struct CratesTool;

impl CratesTool {
    /// Create a new `CratesTool`.
    pub fn new() -> Self {
        CratesTool
    }
}

impl Default for CratesTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Clip a string to at most `n` chars (char-safe), appending an ellipsis.
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    } else {
        s.to_string()
    }
}

/// Minimal percent-encoding for query/path segments (no extra deps).
fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[async_trait]
impl Tool for CratesTool {
    fn name(&self) -> &str {
        "crates"
    }

    fn description(&self) -> &str {
        "Look up Rust crates on crates.io (read-only): crate metadata + latest version (action=lookup), search by keyword (action=search), or known security advisories from RustSec via OSV (action=advisories). Does NOT modify Cargo.toml or dependencies."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["lookup", "search", "advisories"], "description": "What to do (optional; inferred from name/query)" },
                "name": { "type": "string", "description": "Crate name (for lookup/advisories)" },
                "query": { "type": "string", "description": "Search keywords (for search)" },
                "version": { "type": "string", "description": "Optional version to scope advisories" },
                "limit": { "type": "integer", "description": "Max search results (default 5)" }
            }
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let name = args["name"].as_str().unwrap_or("").trim().to_string();
        let query = args["query"].as_str().unwrap_or("").trim().to_string();
        let version = args["version"].as_str().unwrap_or("").trim().to_string();
        let limit = args["limit"].as_u64().unwrap_or(5).clamp(1, 20) as usize;
        let action = args["action"].as_str().unwrap_or("").trim().to_string();
        let action = if !action.is_empty() {
            action
        } else if !query.is_empty() {
            "search".to_string()
        } else {
            "lookup".to_string()
        };

        match action.as_str() {
            "search" => {
                let q = if !query.is_empty() { query } else { name };
                if q.is_empty() {
                    return Ok("Error: 'query' is required for search".to_string());
                }
                crates_search(&q, limit).await
            }
            "advisories" => {
                if name.is_empty() {
                    return Ok("Error: 'name' is required for advisories".to_string());
                }
                crates_advisories(&name, &version).await
            }
            _ => {
                if name.is_empty() {
                    return Ok("Error: 'name' is required for lookup".to_string());
                }
                crates_lookup(&name).await
            }
        }
    }
}

async fn http_get_json(url: &str) -> Result<Value> {
    use aegis_security::is_safe_url;
    is_safe_url(url).map_err(|e| anyhow::anyhow!("SSRF check failed: {e}"))?;
    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .header("User-Agent", UA)
        .header("Accept", "application/json")
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow::anyhow!("HTTP {status}"));
    }
    Ok(resp.json::<Value>().await?)
}

async fn crates_lookup(name: &str) -> Result<String> {
    let url = format!("https://crates.io/api/v1/crates/{}", urlenc(name));
    let v = match http_get_json(&url).await {
        Ok(v) => v,
        Err(e) => {
            return Ok(format!(
                "crates.io lookup for '{name}' failed: {e} (the crate may not exist)"
            ))
        }
    };
    let c = &v["crate"];
    if !c.is_object() {
        return Ok(format!("No crate named '{name}' on crates.io."));
    }
    let desc = c["description"].as_str().unwrap_or("");
    let stable = c["max_stable_version"]
        .as_str()
        .or_else(|| c["max_version"].as_str())
        .unwrap_or("?");
    let newest = c["newest_version"]
        .as_str()
        .or_else(|| c["max_version"].as_str())
        .unwrap_or("?");
    let downloads = c["downloads"].as_u64().unwrap_or(0);
    let repo = c["repository"].as_str().unwrap_or("");
    let docs = c["documentation"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("https://docs.rs/{name}"));
    let homepage = c["homepage"].as_str().unwrap_or("");

    let mut out = String::new();
    out.push_str(&format!("crate: {name}\n"));
    if !desc.is_empty() {
        out.push_str(&format!("description: {}\n", clip(desc, 300)));
    }
    out.push_str(&format!("latest stable: {stable}  (newest: {newest})\n"));
    out.push_str(&format!("downloads: {downloads}\n"));
    out.push_str(&format!("docs: {docs}\n"));
    if !repo.is_empty() {
        out.push_str(&format!("repository: {repo}\n"));
    }
    if !homepage.is_empty() {
        out.push_str(&format!("homepage: {homepage}\n"));
    }
    if let Some(vers) = v["versions"].as_array() {
        let recent: Vec<String> = vers
            .iter()
            .take(5)
            .filter_map(|x| {
                let num = x["num"].as_str()?;
                let yanked = x["yanked"].as_bool().unwrap_or(false);
                Some(if yanked {
                    format!("{num} (yanked)")
                } else {
                    num.to_string()
                })
            })
            .collect();
        if !recent.is_empty() {
            out.push_str(&format!("recent versions: {}\n", recent.join(", ")));
        }
    }
    out.push_str(
        "\nTip: add with a pinned version after confirming with the user; this tool does not modify Cargo.toml.",
    );
    Ok(out.trim_end().to_string())
}

async fn crates_search(query: &str, limit: usize) -> Result<String> {
    let url = format!(
        "https://crates.io/api/v1/crates?q={}&per_page={}",
        urlenc(query),
        limit
    );
    let v = match http_get_json(&url).await {
        Ok(v) => v,
        Err(e) => return Ok(format!("crates.io search failed: {e}")),
    };
    let mut out = String::new();
    if let Some(arr) = v["crates"].as_array() {
        for (i, c) in arr.iter().take(limit).enumerate() {
            let n = c["name"].as_str().unwrap_or("?");
            let ver = c["max_stable_version"]
                .as_str()
                .or_else(|| c["max_version"].as_str())
                .unwrap_or("?");
            let d = c["description"].as_str().unwrap_or("");
            let dl = c["downloads"].as_u64().unwrap_or(0);
            out.push_str(&format!(
                "[{}] {} v{}  ({} downloads)\n    {}\n",
                i + 1,
                n,
                ver,
                dl,
                clip(d, 160)
            ));
        }
    }
    if out.is_empty() {
        return Ok(format!("No crates found for '{query}'."));
    }
    Ok(out.trim_end().to_string())
}

async fn crates_advisories(name: &str, version: &str) -> Result<String> {
    use aegis_security::is_safe_url;
    let url = "https://api.osv.dev/v1/query";
    is_safe_url(url).map_err(|e| anyhow::anyhow!("SSRF check failed: {e}"))?;
    let pkg = if version.is_empty() {
        json!({ "package": { "ecosystem": "crates.io", "name": name } })
    } else {
        json!({ "package": { "ecosystem": "crates.io", "name": name }, "version": version })
    };
    let client = reqwest::Client::new();
    let resp = match client
        .post(url)
        .header("User-Agent", UA)
        .json(&pkg)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Ok(format!("advisory lookup failed: {e}")),
    };
    let v: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Ok(format!("advisory parse failed: {e}")),
    };
    let vulns = v["vulns"].as_array().cloned().unwrap_or_default();
    if vulns.is_empty() {
        let scope = if version.is_empty() {
            name.to_string()
        } else {
            format!("{name} {version}")
        };
        return Ok(format!(
            "No known advisories for {scope} (source: OSV/RustSec)."
        ));
    }
    let mut out = String::new();
    let scope = if version.is_empty() {
        String::new()
    } else {
        format!(" {version}")
    };
    out.push_str(&format!("known advisories for {name}{scope}:\n"));
    for vln in vulns.iter().take(10) {
        let id = vln["id"].as_str().unwrap_or("?");
        let summary = vln["summary"]
            .as_str()
            .or_else(|| vln["details"].as_str())
            .unwrap_or("");
        out.push_str(&format!("- {}: {}\n", id, clip(summary, 200)));
    }
    out.push_str(
        "\nReview these before depending; do NOT change dependencies without user confirmation.",
    );
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_urlenc_encodes_specials() {
        assert_eq!(urlenc("tokio"), "tokio");
        assert_eq!(urlenc("async runtime"), "async%20runtime");
        assert_eq!(urlenc("a/b"), "a%2Fb");
    }

    #[test]
    fn test_clip_is_char_safe() {
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("hello", 3), "hel…");
    }

    #[test]
    fn test_metadata() {
        let t = CratesTool::new();
        assert_eq!(t.name(), "crates");
        assert!(t.parameters().get("properties").is_some());
    }
}
