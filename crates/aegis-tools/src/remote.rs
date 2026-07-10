//! Remote server access over SSH (stage 1 of multi-machine support).
//!
//! Lets the agent operate a remote server given `host` + `user` + (`password`
//! or an SSH key/agent). Actions: `run` a command, `upload` a file, `check`
//! connectivity. Password auth uses the system `sshpass` with the password
//! passed via the `SSHPASS` env var — never on the command line (so it doesn't
//! leak into `ps`/argv) and never echoed back in output.
//!
//! High-risk by nature (remote code execution): every call goes through the
//! approval gate (`ctx.approve`), which also honours the permission policy.

use crate::registry::{Tool, ToolContext};
use aegis_security::sanitize_credentials;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;

pub struct RemoteTool;

impl RemoteTool {
    fn common_ssh_opts(port: u64) -> Vec<String> {
        vec![
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "BatchMode=no".into(),
            "-p".into(),
            port.to_string(),
        ]
    }
    fn common_scp_opts(port: u64) -> Vec<String> {
        vec![
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-P".into(), // scp uses uppercase -P for port
            port.to_string(),
        ]
    }
}

#[async_trait]
impl Tool for RemoteTool {
    fn name(&self) -> &str {
        "remote"
    }
    fn description(&self) -> &str {
        "Operate a remote server over SSH. Prefer a saved `server` handle (e.g. \
         server=\"srv1\") so host/user/password are resolved locally and never sent \
         to the model; otherwise pass host + user + (password OR SSH key). Actions: \
         run (command), upload (local→remote file), check (connectivity + OS). \
         Password auth needs `sshpass` locally. High-risk: each call is gated by \
         approval. Saved servers are added by the user via the `/server` command."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["run", "upload", "check"] },
                "server": { "type": "string", "description": "Saved server handle (resolved locally; preferred over host/user/password)." },
                "host": { "type": "string", "description": "Server hostname or IP (if no `server`)" },
                "user": { "type": "string", "description": "SSH username (if no `server`)" },
                "password": { "type": "string", "description": "SSH password (optional; needs sshpass)" },
                "port": { "type": "integer", "description": "SSH port (default 22)" },
                "command": { "type": "string", "description": "Shell command to run (action=run)" },
                "local": { "type": "string", "description": "Local file path (action=upload)" },
                "remote": { "type": "string", "description": "Remote destination path (action=upload)" }
            },
            "required": ["action"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let action = args["action"].as_str().unwrap_or("run");

        // A `server` handle resolves host/user/password locally from the saved
        // credential store — so the real values never appear in the model's
        // tool-call args (they never reach the provider). Otherwise fall back to
        // inline host/user/password.
        let (host, user, port, password_owned): (String, String, u64, Option<String>) =
            if let Some(name) = args["server"].as_str().filter(|s| !s.is_empty()) {
                match crate::remotes::get(name) {
                    Some(c) => (c.host, c.user, c.port, c.password),
                    None => {
                        return Ok(format!(
                            "No saved server '{name}'. Add it locally with the `/server add` command first."
                        ))
                    }
                }
            } else {
                (
                    args["host"].as_str().unwrap_or("").trim().to_string(),
                    args["user"].as_str().unwrap_or("").trim().to_string(),
                    args["port"].as_u64().unwrap_or(22),
                    args["password"].as_str().filter(|p| !p.is_empty()).map(|s| s.to_string()),
                )
            };
        let host = host.as_str();
        let user = user.as_str();
        if host.is_empty() || user.is_empty() {
            return Ok("Error: provide `server` (saved handle) or `host`+`user`.".to_string());
        }
        let password = password_owned.as_deref();
        let target = format!("{user}@{host}");

        // Approval (never include the password in the prompt).
        let what = match action {
            "upload" => format!(
                "upload {} → {host}:{}",
                args["local"].as_str().unwrap_or("?"),
                args["remote"].as_str().unwrap_or("?")
            ),
            "check" => format!("check connectivity to {target}"),
            _ => format!("run on {target}: {}", args["command"].as_str().unwrap_or("")),
        };
        if !ctx.approve(&format!("⚠️ REMOTE SSH ({host}): {what}")) {
            return Ok("Remote action blocked: user denied.".to_string());
        }

        let mut cmd = match action {
            "upload" => {
                let local = args["local"].as_str().unwrap_or("");
                let remote = args["remote"].as_str().unwrap_or("");
                if local.is_empty() || remote.is_empty() {
                    return Ok("Error: 'local' and 'remote' are required for upload.".to_string());
                }
                let mut c = if password.is_some() {
                    let mut c = tokio::process::Command::new("sshpass");
                    c.arg("-e").arg("scp");
                    c
                } else {
                    tokio::process::Command::new("scp")
                };
                c.args(Self::common_scp_opts(port))
                    .arg(local)
                    .arg(format!("{target}:{remote}"));
                c
            }
            _ => {
                let remote_cmd = if action == "check" {
                    "echo aegis-remote-ok && uname -a && (command -v aegis >/dev/null && echo 'aegis: present' || echo 'aegis: not installed')"
                } else {
                    args["command"].as_str().unwrap_or("")
                };
                if remote_cmd.is_empty() {
                    return Ok("Error: 'command' is required for action=run.".to_string());
                }
                let mut c = if password.is_some() {
                    let mut c = tokio::process::Command::new("sshpass");
                    c.arg("-e").arg("ssh");
                    c
                } else {
                    tokio::process::Command::new("ssh")
                };
                c.args(Self::common_ssh_opts(port))
                    .arg(&target)
                    .arg(remote_cmd);
                c
            }
        };

        // Password via env only — never argv.
        if let Some(pw) = password {
            cmd.env("SSHPASS", pw);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let output = tokio::time::timeout(Duration::from_secs(120), cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("remote action timed out after 120s"))?;
        let output = match output {
            Ok(o) => o,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    return Ok(format!(
                        "Error: required program not found ({e}). Password auth needs `sshpass` installed locally (e.g. `apt install sshpass`); or use an SSH key and omit `password`."
                    ));
                }
                return Ok(format!("Error launching remote command: {e}"));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);
        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("[stderr]\n");
            result.push_str(&stderr);
        }
        if code != 0 {
            result.push_str(&format!("\n[exit code: {code}]"));
        }
        if result.len() > 50_000 {
            result.truncate(result.floor_char_boundary(50_000));
            result.push_str("\n... [output truncated at 50KB]");
        }
        Ok(sanitize_credentials(&result))
    }
}
