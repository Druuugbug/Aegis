use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Strategy for selecting the next available API key.
#[derive(Debug, Clone, Default)]
pub enum RotationStrategy {
    /// Use the first key that is not in cooldown.
    FillFirst,
    /// Rotate keys in round-robin order.
    #[default]
    RoundRobin,
    /// Pick a random available key.
    Random,
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeyStatus {
    Ok,
    Cooldown,
}

#[derive(Debug)]
struct KeyState {
    key: String,
    status: KeyStatus,
    cooldown_until: Option<Instant>,
}

impl KeyState {
    fn is_available(&self) -> bool {
        match self.status {
            KeyStatus::Ok => true,
            KeyStatus::Cooldown => {
                if let Some(until) = self.cooldown_until {
                    Instant::now() >= until
                } else {
                    false
                }
            }
        }
    }
}

/// A pool of API keys with rotation and cooldown support.
pub struct CredentialPool {
    keys: Mutex<Vec<KeyState>>,
    strategy: RotationStrategy,
    current: Mutex<usize>,
}

impl CredentialPool {
    /// Creates a new `instance`.
    pub fn new(keys: Vec<String>, strategy: RotationStrategy) -> Self {
        let key_states = keys
            .into_iter()
            .map(|k| KeyState {
                key: k,
                status: KeyStatus::Ok,
                cooldown_until: None,
            })
            .collect();
        Self {
            keys: Mutex::new(key_states),
            strategy,
            current: Mutex::new(0),
        }
    }

    /// Get the next available key based on the rotation strategy.
    pub fn next_key(&self) -> Option<String> {
        let mut keys = self.keys.lock().expect("credential pool lock poisoned");
        // Refresh cooldown status
        for k in keys.iter_mut() {
            if k.status == KeyStatus::Cooldown {
                if let Some(until) = k.cooldown_until {
                    if Instant::now() >= until {
                        k.status = KeyStatus::Ok;
                        k.cooldown_until = None;
                    }
                }
            }
        }

        let len = keys.len();
        if len == 0 {
            return None;
        }

        match self.strategy {
            RotationStrategy::FillFirst => keys
                .iter()
                .find(|k| k.is_available())
                .map(|k| k.key.clone()),
            RotationStrategy::RoundRobin => {
                let mut current = self.current.lock().expect("current lock poisoned");
                let start = *current;
                for i in 0..len {
                    let idx = (start + i) % len;
                    if keys[idx].is_available() {
                        *current = (idx + 1) % len;
                        return Some(keys[idx].key.clone());
                    }
                }
                None
            }
            RotationStrategy::Random => {
                use rand::seq::SliceRandom;
                let available: Vec<usize> = (0..len).filter(|&i| keys[i].is_available()).collect();
                available
                    .choose(&mut rand::thread_rng())
                    .map(|&i| keys[i].key.clone())
            }
        }
    }

    /// Mark a key as rate-limited for `duration`.
    pub fn mark_rate_limited(&self, key: &str, duration: Duration) {
        let mut keys = self.keys.lock().expect("credential pool lock poisoned");
        if let Some(k) = keys.iter_mut().find(|k| k.key == key) {
            k.status = KeyStatus::Cooldown;
            k.cooldown_until = Some(Instant::now() + duration);
        }
    }

    /// Restore a key to Ok status.
    pub fn mark_ok(&self, key: &str) {
        let mut keys = self.keys.lock().expect("credential pool lock poisoned");
        if let Some(k) = keys.iter_mut().find(|k| k.key == key) {
            k.status = KeyStatus::Ok;
            k.cooldown_until = None;
        }
    }

    /// Number of keys in the pool.
    pub fn len(&self) -> usize {
        self.keys
            .lock()
            .expect("credential pool lock poisoned")
            .len()
    }

    /// Returns whether this value empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_robin_rotation() {
        let pool = CredentialPool::new(
            vec!["key-a".into(), "key-b".into(), "key-c".into()],
            RotationStrategy::RoundRobin,
        );
        assert_eq!(pool.next_key(), Some("key-a".into()));
        assert_eq!(pool.next_key(), Some("key-b".into()));
        assert_eq!(pool.next_key(), Some("key-c".into()));
        // Wraps around
        assert_eq!(pool.next_key(), Some("key-a".into()));
    }

    #[test]
    fn test_mark_rate_limited_skips_key() {
        let pool = CredentialPool::new(
            vec!["key-a".into(), "key-b".into()],
            RotationStrategy::RoundRobin,
        );
        // Take key-a first
        assert_eq!(pool.next_key(), Some("key-a".into()));
        // Rate limit key-b
        pool.mark_rate_limited("key-b", Duration::from_secs(60));
        // Should skip key-b and go back to key-a
        assert_eq!(pool.next_key(), Some("key-a".into()));
        // Restore key-b
        pool.mark_ok("key-b");
        assert_eq!(pool.next_key(), Some("key-b".into()));
    }

    #[test]
    fn test_all_keys_rate_limited_returns_none() {
        let pool = CredentialPool::new(vec!["key-a".into()], RotationStrategy::RoundRobin);
        pool.mark_rate_limited("key-a", Duration::from_secs(60));
        assert_eq!(pool.next_key(), None);
    }

    #[test]
    fn test_fill_first_strategy() {
        let pool = CredentialPool::new(
            vec!["key-a".into(), "key-b".into()],
            RotationStrategy::FillFirst,
        );
        assert_eq!(pool.next_key(), Some("key-a".into()));
        assert_eq!(pool.next_key(), Some("key-a".into())); // always first available
        pool.mark_rate_limited("key-a", Duration::from_secs(60));
        assert_eq!(pool.next_key(), Some("key-b".into()));
    }

    #[test]
    fn test_random_strategy_returns_available() {
        let pool = CredentialPool::new(
            vec!["key-a".into(), "key-b".into()],
            RotationStrategy::Random,
        );
        pool.mark_rate_limited("key-a", Duration::from_secs(60));
        // Only key-b available
        assert_eq!(pool.next_key(), Some("key-b".into()));
    }

    #[test]
    fn test_cooldown_expires() {
        let pool = CredentialPool::new(vec!["key-a".into()], RotationStrategy::RoundRobin);
        // Set cooldown that's already expired
        pool.mark_rate_limited("key-a", Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        // Should be available again after sleep
        assert_eq!(pool.next_key(), Some("key-a".into()));
    }
}
