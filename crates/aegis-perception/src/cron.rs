use anyhow::Result;
use std::time::Duration;

/// 简化的 cron 触发器（只支持固定间隔和每天某时）
pub struct CronTrigger {
    /// 间隔秒数
    pub interval_secs: u64,
    /// 描述信息
    pub description: String,
}

impl CronTrigger {
    /// 从 cron 表达式解析（仅支持少数常用格式）
    /// "*/5 * * * *" → 5分钟
    /// "0 9 * * *" → 大约每 86400s（一天）
    /// "@hourly" → 3600s
    /// "@daily" → 86400s
    pub fn from_cron(expr: &str) -> Result<Self> {
        let expr = expr.trim();
        let interval_secs = match expr {
            "@hourly" => 3600,
            "@daily" => 86400,
            "@weekly" => 604800,
            s if s.starts_with("*/") => {
                // "*/5 * * * *" → 5分钟
                let parts: Vec<&str> = s.split_whitespace().collect();
                if !parts.is_empty() {
                    let mins: u64 = parts[0][2..].parse().unwrap_or(1);
                    mins * 60
                } else {
                    60
                }
            }
            _ => {
                // 其他格式默认 1 天
                86400
            }
        };
        Ok(Self {
            interval_secs,
            description: format!("cron: {expr}"),
        })
    }

    /// 启动定时循环，每次触发时调用 callback
    pub async fn run<F, Fut>(&self, callback: F) -> Result<()>
    where
        F: Fn() -> Fut + Send + Sync,
        Fut: std::future::Future<Output = ()> + Send,
    {
        loop {
            tokio::time::sleep(Duration::from_secs(self.interval_secs)).await;
            callback().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hourly() {
        let trigger = CronTrigger::from_cron("@hourly").unwrap();
        assert_eq!(trigger.interval_secs, 3600);
        assert_eq!(trigger.description, "cron: @hourly");
    }

    #[test]
    fn test_daily() {
        let trigger = CronTrigger::from_cron("@daily").unwrap();
        assert_eq!(trigger.interval_secs, 86400);
    }

    #[test]
    fn test_weekly() {
        let trigger = CronTrigger::from_cron("@weekly").unwrap();
        assert_eq!(trigger.interval_secs, 604800);
    }

    #[test]
    fn test_every_5_minutes() {
        let trigger = CronTrigger::from_cron("*/5 * * * *").unwrap();
        assert_eq!(trigger.interval_secs, 300); // 5 * 60
    }

    #[test]
    fn test_every_15_minutes() {
        let trigger = CronTrigger::from_cron("*/15 * * * *").unwrap();
        assert_eq!(trigger.interval_secs, 900);
    }

    #[test]
    fn test_fallback_unknown_expression() {
        let trigger = CronTrigger::from_cron("0 9 * * *").unwrap();
        assert_eq!(trigger.interval_secs, 86400);
        assert_eq!(trigger.description, "cron: 0 9 * * *");
    }

    #[test]
    fn test_whitespace_trimmed() {
        let trigger = CronTrigger::from_cron("  @daily  ").unwrap();
        assert_eq!(trigger.interval_secs, 86400);
    }
}
