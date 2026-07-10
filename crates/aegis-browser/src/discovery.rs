use anyhow::Result;
use serde::Deserialize;
use tracing::debug;

const COMMON_PORTS: &[u16] = &[9222, 9229, 9333];

#[derive(Debug, Deserialize)]
pub struct ChromeVersion {
    #[serde(rename = "Browser")]
    pub browser: Option<String>,
    #[serde(rename = "webSocketDebuggerUrl")]
    pub ws_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TargetInfo {
    pub id: String,
    pub title: String,
    pub url: String,
    #[serde(rename = "type")]
    pub target_type: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    pub ws_debugger_url: Option<String>,
}

pub async fn discover_chrome() -> Option<u16> {
    for &port in COMMON_PORTS {
        if check_port(port).await.is_ok() {
            debug!(port, "discovered Chrome debug port");
            return Some(port);
        }
    }
    None
}

async fn check_port(port: u16) -> Result<ChromeVersion> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await?;
    let ver: ChromeVersion = resp.json().await?;
    Ok(ver)
}

pub async fn get_version(port: u16) -> Result<ChromeVersion> {
    check_port(port).await
}

pub async fn list_targets(port: u16) -> Result<Vec<TargetInfo>> {
    let url = format!("http://127.0.0.1:{port}/json/list");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;
    let targets: Vec<TargetInfo> = resp.json().await?;
    Ok(targets)
}
