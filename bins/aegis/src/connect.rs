//! `aegis connect` — interactive onboarding for reaching Aegis from anywhere.
//!
//! Charter: users solve any aegis problem via natural language. This command is
//! the guided front door for "I want to conveniently talk to Aegis": it lists
//! the available channels, collects what each needs (tokens read hidden), writes
//! the `[gateway.*]` config for the user, and prints how to start.

use anyhow::{anyhow, Result};

use crate::chat::read_secret;
use crate::select;

/// Entry point for `aegis connect`.
pub fn run() -> Result<()> {
    let routes = vec![
        "Telegram   — bot token, outbound long-poll (simplest)".to_string(),
        "Feishu(ws) — 飞书长连接，出站，无需公网端口".to_string(),
        "Discord    — bot token + channel".to_string(),
        "Slack      — bot token + channel (Socket Mode)".to_string(),
        "SimpleX    — 端到端加密/无账号/NAT友好（需先装 simplex-chat CLI）".to_string(),
        "A2A        — 让另一个 Agent 连过来".to_string(),
    ];
    let choice = match select::pick("想怎么和 Aegis 通信？", &routes) {
        Some(i) => i,
        None => {
            println!("已取消。");
            return Ok(());
        }
    };

    match choice {
        0 => connect_telegram(),
        1 => connect_feishu(),
        2 => connect_discord(),
        3 => connect_slack(),
        4 => connect_simplex(),
        5 => connect_a2a(),
        _ => Ok(()),
    }
}

fn prompt(label: &str) -> Option<String> {
    use std::io::{BufRead, Write};
    eprint!("  {label}: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    Some(line.trim().to_string())
}

fn connect_telegram() -> Result<()> {
    let token = read_secret("  Bot token").unwrap_or_default();
    if token.is_empty() {
        return Err(anyhow!("bot token 不能为空"));
    }
    set_gateway_keys("telegram", &[("enabled", "true"), ("bot_token", &token)])?;
    started_hint("telegram", "出站长轮询，无需公网端口");
    Ok(())
}

fn connect_feishu() -> Result<()> {
    let app_id = prompt("App ID").unwrap_or_default();
    let app_secret = read_secret("  App Secret").unwrap_or_default();
    if app_id.is_empty() || app_secret.is_empty() {
        return Err(anyhow!("app_id / app_secret 不能为空"));
    }
    set_gateway_keys(
        "feishu",
        &[
            ("enabled", "true"),
            ("app_id", &app_id),
            ("app_secret", &app_secret),
            ("mode", "ws"),
        ],
    )?;
    started_hint("feishu", "mode=ws 长连接出站，无需公网端口");
    Ok(())
}

fn connect_discord() -> Result<()> {
    let token = read_secret("  Bot token").unwrap_or_default();
    let channel = prompt("Channel ID").unwrap_or_default();
    if token.is_empty() || channel.is_empty() {
        return Err(anyhow!("bot token / channel id 不能为空"));
    }
    set_gateway_keys(
        "discord",
        &[
            ("enabled", "true"),
            ("bot_token", &token),
            ("channel_id", &channel),
            ("mode", "gateway"),
        ],
    )?;
    started_hint("discord", "mode=gateway 实时 WebSocket");
    Ok(())
}

fn connect_slack() -> Result<()> {
    let token = read_secret("  Bot token (xoxb-…)").unwrap_or_default();
    let app_token = read_secret("  App token (xapp-…, Socket Mode)").unwrap_or_default();
    let channel = prompt("Channel ID").unwrap_or_default();
    if token.is_empty() || channel.is_empty() {
        return Err(anyhow!("bot token / channel id 不能为空"));
    }
    set_gateway_keys(
        "slack",
        &[
            ("enabled", "true"),
            ("bot_token", &token),
            ("app_token", &app_token),
            ("channel_id", &channel),
            ("mode", "socket"),
        ],
    )?;
    started_hint("slack", "mode=socket，需 app_token 才能用 Socket Mode");
    Ok(())
}

fn connect_simplex() -> Result<()> {
    println!(
        "\nSimpleX 需要本机运行官方 `simplex-chat` CLI（我们不打包它以保持 MIT）。\n\
         1) 安装 CLI： curl -o- https://raw.githubusercontent.com/simplex-chat/simplex-chat/stable/install.sh | bash\n\
         2) 建 bot 身份并起本地 WS 服务： simplex-chat --create-bot-display-name AegisBot -p 5225\n\
         3) 本命令下面会把 [gateway.simplex] 打开（默认连 127.0.0.1:5225）。\n"
    );
    let port = prompt("WS 端口 [5225]").unwrap_or_default();
    let port = if port.is_empty() {
        "5225".to_string()
    } else {
        port
    };
    set_gateway_keys(
        "simplex",
        &[
            ("enabled", "true"),
            ("host", "127.0.0.1"),
            ("port", &port),
            ("bot_name", "AegisBot"),
        ],
    )?;
    started_hint(
        "simplex",
        "出站连接本地 CLI（端口须保持 127.0.0.1，不可暴露公网）",
    );
    Ok(())
}

fn connect_a2a() -> Result<()> {
    let token = read_secret("  Bearer token（留空=不鉴权，强烈建议设置）").unwrap_or_default();
    let mut kv: Vec<(&str, &str)> = vec![
        ("enabled", "true"),
        ("host", "127.0.0.1"),
        ("port", "41241"),
    ];
    if !token.is_empty() {
        kv.push(("token", &token));
    }
    set_gateway_keys("a2a", &kv)?;
    if token.is_empty() {
        println!("⚠ 未设置 token —— A2A 端点无鉴权，任何能连到该端口的人都能让 aegis 执行任务。建议只绑 127.0.0.1 + SSH 隧道。");
    }
    started_hint("a2a", "另一个 Agent 通过 A2A 协议连入");
    Ok(())
}

/// Print how to (re)start the gateway to pick up the new channel.
fn started_hint(channel: &str, note: &str) {
    println!(
        "\n✓ 已写入 [gateway.{channel}]（{note}）。\n\
         启动/重启网关使其生效： aegis gateway stop  然后  aegis\n\
         配置文件： {}",
        aegis_core::config::config_path().display()
    );
}

/// Merge `[gateway.<channel>]` keys into the existing config.toml, preserving
/// everything else. Across every channel the only non-string fields are
/// `enabled` (bool) and `port` (int); everything else — including numeric-looking
/// ids like `channel_id` — is a TOML string. We type each key accordingly rather
/// than guessing from the value's shape (which used to turn a numeric Discord
/// `channel_id` into an integer and break `Config::load`).
fn set_gateway_keys(channel: &str, kv: &[(&str, &str)]) -> Result<()> {
    let path = aegis_core::config::config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml::Value = if existing.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        existing
            .parse()
            .map_err(|e| anyhow!("解析现有 config.toml 失败: {e}"))?
    };

    let root = doc
        .as_table_mut()
        .ok_or_else(|| anyhow!("config.toml 顶层不是 table"))?;
    let gateway = root
        .entry("gateway".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let gw_tbl = gateway
        .as_table_mut()
        .ok_or_else(|| anyhow!("[gateway] 不是 table"))?;
    let ch = gw_tbl
        .entry(channel.to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let ch_tbl = ch
        .as_table_mut()
        .ok_or_else(|| anyhow!("[gateway.{channel}] 不是 table"))?;

    for (k, v) in kv {
        let value = match *k {
            "enabled" => toml::Value::Boolean(*v == "true"),
            "port" => v
                .parse::<i64>()
                .map(toml::Value::Integer)
                .unwrap_or_else(|_| toml::Value::String(v.to_string())),
            // Everything else (bot_token, channel_id, app_token, app_id,
            // app_secret, host, mode, bot_name, token, …) is a string, even when
            // it looks numeric.
            _ => toml::Value::String(v.to_string()),
        };
        ch_tbl.insert(k.to_string(), value);
    }

    let out = toml::to_string_pretty(&doc).map_err(|e| anyhow!("序列化 config.toml 失败: {e}"))?;
    // Write guard: never persist a config the gateway couldn't load back.
    aegis_core::config::Config::validate_toml_str(&out)
        .map_err(|e| anyhow!("写入会生成无法解析的 config.toml，已中止（未改动）：{e}"))?;
    std::fs::write(&path, out).map_err(|e| anyhow!("写入 config.toml 失败: {e}"))?;
    Ok(())
}
