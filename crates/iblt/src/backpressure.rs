//! # Backpressure
//!
//! Backpressure-aware streaming for IBLT bulk operations.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackpressureConfig { pub max_buffer_size: usize, pub batch_size: usize, pub low_watermark: f64, pub high_watermark: f64 }
impl Default for BackpressureConfig { fn default() -> Self { Self { max_buffer_size: 10_000, batch_size: 128, low_watermark: 0.2, high_watermark: 0.8 } } }

#[derive(Debug, Clone)]
pub enum IbltOperation { Insert { key: Vec<u8>, value: Vec<u8> }, Delete { key: Vec<u8>, value: Vec<u8> } }

pub struct BackpressureQueue { config: BackpressureConfig, buffer: Arc<Mutex<VecDeque<IbltOperation>>>, semaphore: Arc<Semaphore> }
impl BackpressureQueue {
    pub fn new(config: BackpressureConfig) -> Self { let cap = config.max_buffer_size; Self { config, buffer: Arc::new(Mutex::new(VecDeque::with_capacity(cap))), semaphore: Arc::new(Semaphore::new(cap)) } }
    pub async fn push(&self, op: IbltOperation) -> anyhow::Result<()> { let permit = self.semaphore.acquire().await?; let mut buf = self.buffer.lock().await; buf.push_back(op); drop(permit); Ok(()) }
    pub async fn drain_batch(&self) -> Vec<IbltOperation> { let mut buf = self.buffer.lock().await; let n = buf.len().min(self.config.batch_size); buf.drain(..n).collect() }
    pub async fn len(&self) -> usize { self.buffer.lock().await.len() }
    pub async fn is_empty(&self) -> bool { self.buffer.lock().await.is_empty() }
}
impl Default for BackpressureQueue { fn default() -> Self { Self::new(BackpressureConfig::default()) } }

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn test_queue_push_drain() {
        let queue = BackpressureQueue::new(BackpressureConfig { max_buffer_size: 100, batch_size: 10, ..Default::default() });
        for i in 0..25 { queue.push(IbltOperation::Insert { key: format!("k{i}").into_bytes(), value: format!("v{i}").into_bytes() }).await.unwrap(); }
        assert_eq!(queue.len().await, 25);
        let batch = queue.drain_batch().await;
        assert_eq!(batch.len(), 10);
    }
}
