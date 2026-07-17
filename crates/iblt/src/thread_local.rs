//! # Thread-Local IBLT
//!
//! Per-thread IBLT instances that merge into a shared global table.
use crate::compact::CompactIblt;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

thread_local! { static LOCAL_IBLT: RefCell<Option<CompactIblt>> = const { RefCell::new(None) }; }

#[derive(Debug, Clone)]
pub struct ThreadLocalConfig {
    pub cells_per_thread: usize,
    pub flush_threshold: usize,
}
impl Default for ThreadLocalConfig {
    fn default() -> Self {
        Self {
            cells_per_thread: 128,
            flush_threshold: 64,
        }
    }
}

pub struct SharedIblt {
    inner: Mutex<CompactIblt>,
}
impl SharedIblt {
    pub fn new(num_cells: usize) -> Self {
        Self {
            inner: Mutex::new(CompactIblt::new(num_cells)),
        }
    }
    pub fn merge(&self, local: &CompactIblt) {
        let mut g = self.inner.lock().unwrap();
        for (k, v) in local.dump_entries() {
            g.insert(&k, &v);
        }
    }
    pub fn snapshot(&self) -> CompactIblt {
        self.inner.lock().unwrap().clone()
    }
}

pub struct ThreadLocalIblt {
    config: ThreadLocalConfig,
    shared: Arc<SharedIblt>,
    local_count: usize,
}
impl ThreadLocalIblt {
    pub fn new(config: ThreadLocalConfig, shared: Arc<SharedIblt>) -> Self {
        Self {
            config,
            shared,
            local_count: 0,
        }
    }
    pub fn insert(&mut self, key: &[u8], value: &[u8]) {
        LOCAL_IBLT.with(|l| {
            let mut b = l.borrow_mut();
            if b.is_none() {
                *b = Some(CompactIblt::new(self.config.cells_per_thread));
            }
            b.as_mut().unwrap().insert(key, value);
        });
        self.local_count += 1;
        if self.local_count >= self.config.flush_threshold {
            self.flush();
        }
    }
    pub fn flush(&mut self) {
        LOCAL_IBLT.with(|l| {
            let mut b = l.borrow_mut();
            if let Some(iblt) = b.take() {
                self.shared.merge(&iblt);
            }
        });
        self.local_count = 0;
    }
    pub fn local_count(&self) -> usize {
        self.local_count
    }
}
impl Drop for ThreadLocalIblt {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_shared_merge() {
        let s = SharedIblt::new(128);
        let mut l = CompactIblt::new(64);
        l.insert(b"x", b"1");
        s.merge(&l);
        assert!(s.snapshot().occupied_count() > 0);
    }
    #[test]
    fn test_thread_local_flush() {
        let s = Arc::new(SharedIblt::new(256));
        let cfg = ThreadLocalConfig {
            cells_per_thread: 64,
            flush_threshold: 3,
        };
        let mut tl = ThreadLocalIblt::new(cfg, s.clone());
        tl.insert(b"a", b"1");
        tl.insert(b"b", b"2");
        tl.insert(b"c", b"3");
        assert_eq!(tl.local_count(), 0);
    }
}
