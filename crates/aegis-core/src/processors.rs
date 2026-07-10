use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::pipeline::{AgentContext, RequestProcessor, ResponseProcessor};

/// 从 metadata["memory_context"] 注入记忆到 system_prompt_parts
pub struct MemoryInjector;

#[async_trait]
impl RequestProcessor for MemoryInjector {
    async fn process(&self, ctx: &mut AgentContext) -> Result<()> {
        if let Some(Value::String(mem)) = ctx.metadata.get("memory_context") {
            let text = format!("## Memory Context\n{}", mem);
            ctx.system_prompt_parts.push(text);
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "memory_injector"
    }
}

/// 从 metadata["strategy"] 注入策略到 system_prompt_parts
pub struct StrategyInjector;

#[async_trait]
impl RequestProcessor for StrategyInjector {
    async fn process(&self, ctx: &mut AgentContext) -> Result<()> {
        if let Some(Value::String(strat)) = ctx.metadata.get("strategy") {
            let text = format!("## Strategy\n{}", strat);
            ctx.system_prompt_parts.push(text);
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "strategy_injector"
    }
}

/// 从 metadata["goal"] 注入目标到 system_prompt_parts
pub struct GoalContextInjector;

#[async_trait]
impl RequestProcessor for GoalContextInjector {
    async fn process(&self, ctx: &mut AgentContext) -> Result<()> {
        if let Some(Value::String(goal)) = ctx.metadata.get("goal") {
            let text = format!("## Current Goal\n{}", goal);
            ctx.system_prompt_parts.push(text);
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "goal_context"
    }
}

/// 从 metadata["steer"] 注入引导指令到 system_prompt_parts
pub struct SteerInjector;

#[async_trait]
impl RequestProcessor for SteerInjector {
    async fn process(&self, ctx: &mut AgentContext) -> Result<()> {
        if let Some(Value::String(steer)) = ctx.metadata.get("steer") {
            let text = format!("## Steering Instructions\n{}", steer);
            ctx.system_prompt_parts.push(text);
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "steer_injector"
    }
}

/// 从 response["usage"] 累计 token 用量到 ctx.metadata["total_tokens"]
pub struct CostAccumulator;

#[async_trait]
impl ResponseProcessor for CostAccumulator {
    async fn process(&self, ctx: &mut AgentContext, response: &mut Value) -> Result<()> {
        if let Some(usage) = response.get("usage") {
            let new_tokens = usage
                .get("total_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            let current = ctx
                .metadata
                .entry("total_tokens".to_string())
                .or_insert(Value::Number(0.into()));

            let prev = current.as_u64().unwrap_or(0);
            *current = Value::Number((prev + new_tokens).into());
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "cost_accumulator"
    }
}

/// 记录响应预览到 ctx.metadata["last_response_preview"]（截取前100字符）
pub struct FeedbackCollector;

#[async_trait]
impl ResponseProcessor for FeedbackCollector {
    async fn process(&self, ctx: &mut AgentContext, response: &mut Value) -> Result<()> {
        let preview = response
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or_else(|| response.as_str().unwrap_or(""));

        let truncated: String = preview.chars().take(100).collect();
        ctx.metadata.insert(
            "last_response_preview".to_string(),
            Value::String(truncated),
        );
        Ok(())
    }

    fn name(&self) -> &str {
        "feedback_collector"
    }
}

/// 检查 ctx.messages 长度，超过50条时保留前2条和后20条
pub struct ContextOverflowGuard;

#[async_trait]
impl ResponseProcessor for ContextOverflowGuard {
    async fn process(&self, ctx: &mut AgentContext, _response: &mut Value) -> Result<()> {
        if ctx.messages.len() > 50 {
            let head: Vec<Value> = ctx.messages.iter().take(2).cloned().collect();
            let tail: Vec<Value> = ctx.messages.iter().rev().take(20).cloned().collect::<Vec<_>>().into_iter().rev().collect();
            ctx.messages = head.into_iter().chain(tail).collect();
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "overflow_guard"
    }
}
