//! Size-rotating log file for the resident daemon.
//!
//! The daemon is started with stdout+stderr = /dev/null (setsid), so its
//! `tracing` output would otherwise be lost. This writes it to
//! `~/.aegis/logs/agent.log`, rotating to `agent.log.1` once the file exceeds a
//! size cap and keeping a single backup — so a long-lived resident never fills
//! the disk on a 1c1g box (total ≤ ~2× the cap), consistent with the trash /
//! snapshot / memory "bound your own disk" policy.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

struct Inner {
    path: PathBuf,
    file: File,
    size: u64,
    /// Rotate once the file exceeds this. `0` = never rotate (unbounded).
    max_bytes: u64,
}

/// A `MakeWriter` that appends to a single log file and rotates by size.
#[derive(Clone)]
pub struct RotatingLog(Arc<Mutex<Inner>>);

impl RotatingLog {
    /// Open (creating dirs) the log at `path`, continuing an existing file.
    /// `max_bytes` = rotation threshold (`0` disables rotation).
    pub fn new(path: PathBuf, max_bytes: u64) -> io::Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self(Arc::new(Mutex::new(Inner {
            path,
            file,
            size,
            max_bytes,
        }))))
    }
}

/// Per-write handle (locks the shared file on each write — log volume is low).
pub struct Handle(Arc<Mutex<Inner>>);

impl Write for Handle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Poison-safe: keep logging even if a previous writer panicked.
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if g.max_bytes > 0 && g.size + buf.len() as u64 > g.max_bytes {
            // Rotate: agent.log -> agent.log.1 (overwrite old backup), reopen.
            let bak = g.path.with_extension("log.1");
            let _ = std::fs::rename(&g.path, &bak);
            match OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&g.path)
            {
                Ok(f) => {
                    g.file = f;
                    g.size = 0;
                }
                Err(_) => { /* keep writing to the old handle on failure */ }
            }
        }
        let n = g.file.write(buf)?;
        g.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .file
            .flush()
    }
}

impl<'a> MakeWriter<'a> for RotatingLog {
    type Writer = Handle;
    fn make_writer(&'a self) -> Handle {
        Handle(self.0.clone())
    }
}
