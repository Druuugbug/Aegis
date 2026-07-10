use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum BreakerState {
    Closed,
    Open(Instant),
    HalfOpen,
}

#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    state: Arc<Mutex<BreakerState>>,
    failure_count: Arc<Mutex<u32>>,
    threshold: u32,
    timeout: Duration,
}

impl CircuitBreaker {
    /// Creates a new `instance`.
    pub fn new(threshold: u32, timeout_secs: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(BreakerState::Closed)),
            failure_count: Arc::new(Mutex::new(0)),
            threshold,
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    /// 是否允许请求通过
    pub fn allow_request(&self) -> bool {
        let mut state = self.state.lock().expect("circuit breaker state lock poisoned");
        match *state {
            BreakerState::Closed => true,
            BreakerState::Open(opened_at) => {
                if opened_at.elapsed() >= self.timeout {
                    // 超时，进入 HalfOpen 试探
                    *state = BreakerState::HalfOpen;
                    true
                } else {
                    false
                }
            }
            BreakerState::HalfOpen => {
                // HalfOpen 只放一个请求，再次调用 allow_request 时返回 false
                // 防止并发时多个请求同时通过，将状态切换回 Open 暂时阻断
                *state = BreakerState::Open(
                    Instant::now() - self.timeout + Duration::from_secs(1),
                );
                true
            }
        }
    }

    /// 记录成功
    pub fn record_success(&self) {
        let mut state = self.state.lock().expect("circuit breaker state lock poisoned");
        *state = BreakerState::Closed;
        let mut count = self.failure_count.lock().expect("circuit breaker count lock poisoned");
        *count = 0;
    }

    /// 记录失败
    pub fn record_failure(&self) {
        let mut count = self.failure_count.lock().expect("circuit breaker count lock poisoned");
        *count += 1;
        if *count >= self.threshold {
            let mut state = self.state.lock().expect("circuit breaker state lock poisoned");
            *state = BreakerState::Open(Instant::now());
            *count = 0;
            tracing::warn!("CircuitBreaker tripped: moving to Open state");
        }
    }

    /// 当前状态（用于日志）
    pub fn state_name(&self) -> &'static str {
        let state = self.state.lock().expect("circuit breaker state lock poisoned");
        match *state {
            BreakerState::Closed => "Closed",
            BreakerState::Open(_) => "Open",
            BreakerState::HalfOpen => "HalfOpen",
        }
    }
}
