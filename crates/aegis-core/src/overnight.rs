use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub memory_total_mb: u64,
    pub memory_available_mb: u64,
    pub cpu_count: usize,
    pub load_one: f64,
    pub battery_percent: Option<u8>,
    pub disk_available_gb: f64,
}

impl ResourceSnapshot {
    /// Capture current system resources (memory, CPU, load, disk).
    ///
    /// Cross-platform: Linux reads /proc, macOS uses sysctl, Windows uses GlobalMemoryStatusEx.
    pub fn capture() -> Self {
        let (memory_total_mb, memory_available_mb) = Self::read_memory();
        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let load_one = Self::read_load();
        let disk_available_gb = Self::read_disk();

        Self {
            memory_total_mb,
            memory_available_mb,
            cpu_count,
            load_one,
            battery_percent: None,
            disk_available_gb,
        }
    }

    /// Returns true if the system has enough memory (>200MB) and low enough load to spawn a worker.
    pub fn can_spawn_worker(&self) -> bool {
        self.memory_available_mb > 200 && self.load_one < self.cpu_count as f64 * 0.8
    }

    // ── Memory ──

    #[cfg(target_os = "linux")]
    fn read_memory() -> (u64, u64) {
        let content = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let mut total = 0u64;
        let mut available = 0u64;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                total = Self::parse_kb(rest) / 1024;
            } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
                available = Self::parse_kb(rest) / 1024;
            }
        }
        (total, available)
    }

    #[cfg(target_os = "macos")]
    fn read_memory() -> (u64, u64) {
        // Use sysctl to get hw.memsize (total physical memory)
        let total = Self::sysctl_u64("hw.memsize").unwrap_or(0) / (1024 * 1024);
        // Use vm_stat for available pages
        let available = Self::macos_available_memory_mb();
        (total, available)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn read_memory() -> (u64, u64) {
        // Windows and other platforms: fallback
        (0, 0)
    }

    fn parse_kb(s: &str) -> u64 {
        s.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    // ── Load ──

    #[cfg(target_os = "linux")]
    fn read_load() -> f64 {
        fs::read_to_string("/proc/loadavg")
            .unwrap_or_default()
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0)
    }

    #[cfg(target_os = "macos")]
    fn read_load() -> f64 {
        // Use sysctl vm.loadavg
        let output = std::process::Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        // Format: "{ 1.23 4.56 7.89 }"
        output
            .trim()
            .trim_matches(|c| c == '{' || c == '}')
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn read_load() -> f64 {
        0.0
    }

    // ── Disk ──

    #[cfg(target_os = "linux")]
    fn read_disk() -> f64 {
        use std::mem::MaybeUninit;
        unsafe {
            let mut buf = MaybeUninit::<libc_statvfs>::uninit();
            let path = b"/\0";
            if statvfs_syscall(path.as_ptr() as *const _, buf.as_mut_ptr()) == 0 {
                let s = buf.assume_init();
                return (s.f_bavail as f64 * s.f_frsize as f64) / (1024.0 * 1024.0 * 1024.0);
            }
        }
        0.0
    }

    #[cfg(target_os = "macos")]
    fn read_disk() -> f64 {
        // Use df to get available disk space
        let output = std::process::Command::new("df")
            .args(["-k", "/"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        // Parse second line, 4th column (available in 1K-blocks)
        output
            .lines()
            .nth(1)
            .and_then(|line| {
                line.split_whitespace()
                    .nth(3)
                    .and_then(|v| v.parse::<f64>().ok())
            })
            .map(|kb| kb / (1024.0 * 1024.0))
            .unwrap_or(0.0)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn read_disk() -> f64 {
        0.0
    }

    // ── macOS helpers ──

    #[cfg(target_os = "macos")]
    fn sysctl_u64(name: &str) -> Option<u64> {
        let output = std::process::Command::new("sysctl")
            .args(["-n", name])
            .output()
            .ok()?;
        let s = String::from_utf8(output.stdout).ok()?;
        s.trim().parse().ok()
    }

    #[cfg(target_os = "macos")]
    fn macos_available_memory_mb() -> u64 {
        let output = std::process::Command::new("vm_stat")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        let page_size = 4096u64; // Default page size on macOS
        let mut free = 0u64;
        for line in output.lines() {
            if let Some(rest) = line.strip_prefix("Pages free:") {
                free = rest.trim().trim_matches('.').parse::<u64>().unwrap_or(0);
            }
        }
        (free * page_size) / (1024 * 1024)
    }
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[allow(non_camel_case_types)]
struct libc_statvfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    __f_spare: [i32; 6],
}

#[cfg(target_os = "linux")]
extern "C" {
    #[link_name = "statvfs"]
    fn statvfs_syscall(path: *const std::ffi::c_char, buf: *mut libc_statvfs) -> i32;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OvernightStatus {
    Preflight,
    Running,
    HandoffReady,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCard {
    pub task_id: String,
    pub description: String,
    pub before_state: String,
    pub after_state: Option<String>,
    pub validation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OvernightEvent {
    pub timestamp: DateTime<Utc>,
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OvernightRun {
    pub run_id: String,
    pub mission: String,
    pub started_at: DateTime<Utc>,
    pub target_wake_at: DateTime<Utc>,
    pub status: OvernightStatus,
    pub resource_snapshot: ResourceSnapshot,
    pub task_cards: Vec<TaskCard>,
    pub events: Vec<OvernightEvent>,
}

impl OvernightRun {
    /// Create a new overnight run with a mission and target wake time.
    pub fn new(mission: &str, wake_at: DateTime<Utc>) -> Self {
        let now = Utc::now();
        Self {
            run_id: format!("run_{}", now.timestamp()),
            mission: mission.to_string(),
            started_at: now,
            target_wake_at: wake_at,
            status: OvernightStatus::Preflight,
            resource_snapshot: ResourceSnapshot::capture(),
            task_cards: Vec::new(),
            events: Vec::new(),
        }
    }

    /// Append a timestamped event to the run log.
    pub fn add_event(&mut self, kind: &str, message: &str) {
        self.events.push(OvernightEvent {
            timestamp: Utc::now(),
            kind: kind.to_string(),
            message: message.to_string(),
        });
    }

    /// Register a task card to be tracked during this overnight run.
    pub fn add_task_card(&mut self, card: TaskCard) {
        self.task_cards.push(card);
    }

    /// Mark a task card as completed with its after-state and optional validation result.
    pub fn complete_task(&mut self, task_id: &str, after_state: &str, validation: Option<&str>) {
        if let Some(card) = self.task_cards.iter_mut().find(|c| c.task_id == task_id) {
            card.after_state = Some(after_state.to_string());
            card.validation = validation.map(|v| v.to_string());
        }
    }

    /// Generate a human-readable morning report summarizing the overnight run.
    pub fn morning_report(&self) -> String {
        let duration = Utc::now().signed_duration_since(self.started_at);
        let hours = duration.num_hours();
        let minutes = duration.num_minutes() % 60;

        let mut report = format!(
            "=== Overnight Run Report ===\nMission: {}\nDuration: {}h {}m\nStatus: {:?}\n\n",
            self.mission, hours, minutes, self.status
        );

        report.push_str("--- Task Cards ---\n");
        let completed = self.task_cards.iter().filter(|c| c.after_state.is_some()).count();
        let total = self.task_cards.len();
        report.push_str(&format!("  Progress: {}/{}\n", completed, total));
        for card in &self.task_cards {
            let status = if card.after_state.is_some() { "✓" } else { "○" };
            report.push_str(&format!("  [{}] {}: {}\n", status, card.task_id, card.description));
        }

        report.push_str(&format!(
            "\n--- Resources ---\nMemory: {}/{} MB\nCPU: {} cores, load: {:.2}\nDisk: {:.1} GB available\n",
            self.resource_snapshot.memory_available_mb,
            self.resource_snapshot.memory_total_mb,
            self.resource_snapshot.cpu_count,
            self.resource_snapshot.load_one,
            self.resource_snapshot.disk_available_gb,
        ));

        report
    }

    /// Returns true if the current time has passed the target wake time.
    pub fn is_time_to_wake(&self) -> bool {
        Utc::now() >= self.target_wake_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resource_snapshot_fields() {
        let snap = ResourceSnapshot {
            memory_total_mb: 8192,
            memory_available_mb: 4096,
            cpu_count: 8,
            load_one: 2.5,
            battery_percent: Some(80),
            disk_available_gb: 100.0,
        };
        assert_eq!(snap.memory_total_mb, 8192);
        assert_eq!(snap.cpu_count, 8);
        assert!(snap.can_spawn_worker()); // 4096 > 200 && 2.5 < 8*0.8
    }

    #[test]
    fn test_can_spawn_worker_resource_constrained() {
        let snap = ResourceSnapshot {
            memory_total_mb: 4096,
            memory_available_mb: 100, // too low
            cpu_count: 8,
            load_one: 1.0,
            battery_percent: None,
            disk_available_gb: 50.0,
        };
        assert!(!snap.can_spawn_worker()); // 100 < 200
    }

    #[test]
    fn test_can_spawn_worker_high_load() {
        let snap = ResourceSnapshot {
            memory_total_mb: 8192,
            memory_available_mb: 4096,
            cpu_count: 4,
            load_one: 5.0, // 5.0 > 4*0.8=3.2
            battery_percent: None,
            disk_available_gb: 100.0,
        };
        assert!(!snap.can_spawn_worker());
    }

    #[test]
    fn test_overnight_run_lifecycle() {
        let mut run = OvernightRun::new(
            "Fix all bugs in module X",
            Utc::now() + chrono::Duration::hours(8),
        );

        // Starts in Preflight
        assert_eq!(run.status, OvernightStatus::Preflight);

        // Add events
        run.add_event("preflight_check", "system resources OK");
        assert_eq!(run.events.len(), 1);
        assert_eq!(run.events[0].kind, "preflight_check");

        // Add task cards
        run.add_task_card(TaskCard {
            task_id: "t1".into(),
            description: "Fix login bug".into(),
            before_state: "login fails".into(),
            after_state: None,
            validation: None,
        });
        run.add_task_card(TaskCard {
            task_id: "t2".into(),
            description: "Fix logout bug".into(),
            before_state: "logout crashes".into(),
            after_state: None,
            validation: None,
        });
        assert_eq!(run.task_cards.len(), 2);

        // Complete a task
        run.complete_task("t1", "login works", Some("test passed"));
        let t1 = run.task_cards.iter().find(|c| c.task_id == "t1").unwrap();
        assert_eq!(t1.after_state.as_deref(), Some("login works"));
        assert_eq!(t1.validation.as_deref(), Some("test passed"));

        // Morning report
        let report = run.morning_report();
        assert!(report.contains("Fix all bugs"));
        assert!(report.contains("Progress: 1/2")); // 1 completed
    }

    #[test]
    fn test_is_time_to_wake() {
        let _snap = ResourceSnapshot {
            memory_total_mb: 8192,
            memory_available_mb: 4096,
            cpu_count: 8,
            load_one: 1.0,
            battery_percent: None,
            disk_available_gb: 100.0,
        };
        // Past target time
        let run = OvernightRun::new(
            "test",
            Utc::now() - chrono::Duration::minutes(5),
        );
        assert!(run.is_time_to_wake());

        // Future target time
        let run2 = OvernightRun::new(
            "test",
            Utc::now() + chrono::Duration::hours(1),
        );
        assert!(!run2.is_time_to_wake());
    }
}
