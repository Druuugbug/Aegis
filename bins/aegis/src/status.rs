//! Live status line for the chat REPL.
//!
//! The agent emits rich activity events (thinking, preparing/running tools,
//! reasoning, step counters). This module turns those events into a compact,
//! in-place status *region* at the bottom of the terminal:
//!
//! ```text
//!   ▸ 任务 3/7 ▰▰▰▱▱▱▱▱  当前: deploy nginx config   <- pinned todo bar (optional)
//!   ⠹ Running terminal… (2s)                          <- spinner / activity
//! ```
//!
//! The region is repainted in place every tick and erased cleanly when discrete
//! transcript lines (tool calls, results, errors) are printed above it, so the
//! user always sees both *what* the agent is doing and *how far* a long,
//! todo-tracked task has progressed.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use colored::Colorize;

/// Spinner frames — a rotating circle (distinct from the old braille dots).
const FRAMES: [&str; 8] = ["◜", "◠", "◝", "◞", "◡", "◟", "◞", "◝"];

/// Cool→warm colour cycle used to animate the spinner (truecolor).
const SPIN_PALETTE: [(u8, u8, u8); 6] = [
    (0x49, 0xE0, 0xE0), // cyan
    (0x4F, 0xC3, 0xF7), // sky blue
    (0x7C, 0x9C, 0xFF), // periwinkle
    (0xB3, 0x88, 0xFF), // violet
    (0xE0, 0x7C, 0xE0), // magenta
    (0x8A, 0xB4, 0xFF), // back toward blue
];

/// The animated, colour-cycling spinner glyph for the current tick.
fn spin(idx: usize) -> String {
    let frame = FRAMES[idx % FRAMES.len()];
    let (r, g, b) = SPIN_PALETTE[idx % SPIN_PALETTE.len()];
    format!("{}", frame.truecolor(r, g, b))
}

/// A 3-char scrolling block wave (the thinking indicator). A sliding window
/// over a smooth up/down pattern gives continuous motion; colour cycles too.
fn wave(idx: usize) -> String {
    const W: [char; 14] = [
        '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█', '▇', '▆', '▅', '▄', '▃', '▂',
    ];
    let n = W.len();
    let s: String = (0..3).map(|k| W[(idx + k) % n]).collect();
    let (r, g, b) = SPIN_PALETTE[idx % SPIN_PALETTE.len()];
    format!("{}", s.truecolor(r, g, b))
}

/// How often the render loop repaints the status line.
const TICK: Duration = Duration::from_millis(90);

struct Inner {
    /// Current activity description (e.g. "Thinking…", "Running terminal…").
    label: String,
    /// When true the spinner region is suppressed (e.g. while blocking on stdin
    /// for an approval prompt). Resumes on the next `set_label`.
    streaming: bool,
    /// True while reasoning (chain-of-thought) is streaming. Shown as a dim,
    /// in-place preview line that is folded away once real content arrives.
    reasoning: bool,
    /// Accumulated reasoning text (used to render the live dim preview tail).
    reasoning_buf: String,
    /// When the current reasoning block started, for the "thought for Ns" note.
    reasoning_started: Option<Instant>,
    /// Pinned todo progress: (completed, total, current_item). `None` hides the
    /// bar (no active task list).
    todo: Option<(usize, usize, String)>,
    /// Persistent widget lines rendered below the prompt between turns.
    widget_lines: Vec<String>,
    /// Number of terminal rows the last in-place paint occupied (0 = nothing
    /// painted). Used to erase the region before repainting / printing above it.
    painted: usize,
    /// Turn start, for the elapsed-time readout.
    started: Instant,
    spin_idx: usize,
}

/// A cloneable handle to a live status line.
///
/// Spawn the render loop with [`Status::spawn`], feed it activity via the
/// `set_label` / `line` / `reasoning` / `set_todo` methods (typically from
/// agent callbacks), then call [`Status::finish`] when the turn ends.
#[derive(Clone)]
pub struct Status {
    inner: Arc<Mutex<Inner>>,
    active: Arc<AtomicBool>,
}

impl Status {
    /// Create a new, idle status handle (timer starts now).
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                label: "Thinking…".to_string(),
                streaming: false,
                reasoning: false,
                reasoning_buf: String::new(),
                reasoning_started: None,
                todo: None,
                widget_lines: Vec::new(),
                painted: 0,
                started: Instant::now(),
                spin_idx: 0,
            })),
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Spawn the background render loop. It repaints in place until
    /// [`Status::finish`] is called, then clears the region.
    pub fn spawn(&self) -> JoinHandle<()> {
        let inner = self.inner.clone();
        let active = self.active.clone();
        std::thread::spawn(move || {
            while active.load(Ordering::Relaxed) {
                if let Ok(mut g) = inner.lock() {
                    if !g.streaming {
                        paint(&mut g);
                    }
                }
                std::thread::sleep(TICK);
            }
            // On exit, wipe the region (unless suppressed mid-approval).
            if let Ok(mut g) = inner.lock() {
                if !g.streaming {
                    erase_region(&mut g);
                }
            }
        })
    }

    /// Append a chunk of streamed reasoning (chain-of-thought). Shown as a dim,
    /// in-place preview line that is folded away once content arrives.
    pub fn reasoning(&self, chunk: &str) {
        if let Ok(mut g) = self.inner.lock() {
            if !g.reasoning {
                g.reasoning = true;
                g.reasoning_started = Some(Instant::now());
                g.reasoning_buf.clear();
            }
            g.reasoning_buf.push_str(chunk);
        }
    }

    /// Update the activity label. If reasoning was previewing, fold it into a
    /// compact breadcrumb first.
    pub fn set_label(&self, label: impl Into<String>) {
        if let Ok(mut g) = self.inner.lock() {
            if g.reasoning {
                erase_region(&mut g);
                fold_reasoning(&mut g);
            }
            g.streaming = false;
            g.label = label.into();
        }
    }

    /// Update the pinned todo progress bar. `None` removes it.
    pub fn set_todo(&self, progress: Option<(usize, usize, String)>) {
        if let Ok(mut g) = self.inner.lock() {
            g.todo = progress;
        }
    }

    /// Update the persistent widget lines displayed below the prompt.
    pub fn set_widgets(&self, lines: Vec<String>) {
        if let Ok(mut g) = self.inner.lock() {
            g.widget_lines = lines;
        }
    }

    /// No-op kept for call-site compatibility (the assistant answer is rendered
    /// at end of turn, not streamed, so there is no stream to close).
    pub fn close_stream(&self) {}

    /// Print a discrete transcript line above the status region (tool call,
    /// result, status note, error). May contain embedded newlines.
    pub fn line(&self, text: &str) {
        if let Ok(mut g) = self.inner.lock() {
            erase_region(&mut g);
            fold_reasoning(&mut g);
            eprintln!("{text}");
            let _ = std::io::stderr().flush();
        }
    }

    /// Clear the region and suppress the spinner (e.g. before reading stdin for
    /// an approval prompt). The spinner resumes on the next `set_label`.
    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            erase_region(&mut g);
            g.reasoning = false;
            g.reasoning_buf.clear();
            g.reasoning_started = None;
            g.streaming = true;
        }
    }

    /// Stop the render loop; the spawned thread clears the region and exits.
    pub fn finish(&self) {
        self.active.store(false, Ordering::Relaxed);
    }
}

impl Default for Status {
    fn default() -> Self {
        Self::new()
    }
}

/// Paint the in-place status region (optional pinned todo bar + spinner line).
/// Assumes the caller holds the lock and `!streaming`.
///
/// Flicker-free: when the number of rows is unchanged we overwrite the lines in
/// place (cursor up + rewrite + `\x1b[K` to trim each line) WITHOUT a
/// clear-to-end-of-screen first, so there is never a blank frame between erase
/// and redraw. Only a change in row count falls back to erase+redraw.
fn paint(g: &mut Inner) {
    let idx = g.spin_idx;
    g.spin_idx = g.spin_idx.wrapping_add(1);
    let frame = spin(idx);

    let mut lines: Vec<String> = Vec::with_capacity(4);
    for wl in &g.widget_lines {
        lines.push(format!("{}", wl.dimmed()));
    }
    if let Some((done, total, cur)) = &g.todo {
        lines.push(todo_bar(*done, *total, cur));
    }
    if g.reasoning {
        let dur = g
            .reasoning_started
            .map(|t| fmt_dur(t.elapsed()))
            .unwrap_or_else(|| "0ms".into());
        let tail = reasoning_tail(&g.reasoning_buf, 60);
        lines.push(format!("{} {} {}", wave(idx), tail.dimmed(), format!("({dur})").dimmed()));
    } else {
        let secs = g.started.elapsed().as_secs();
        if secs >= 1 {
            lines.push(format!(
                "{} {} {}",
                frame,
                g.label.as_str().dimmed(),
                format!("({secs}s)").dimmed()
            ));
        } else {
            lines.push(format!("{} {}", frame, g.label.as_str().dimmed()));
        }
    }
    let rows = lines.len();

    if g.painted == rows && rows > 0 {
        // In-place overwrite (no clear-first → no flicker).
        if rows > 1 {
            eprint!("\x1b[{}A", rows - 1);
        }
        eprint!("\r");
        for (i, line) in lines.iter().enumerate() {
            // Overwrite the line, then clear only any leftover tail of it.
            eprint!("{line}\x1b[K");
            if i + 1 < rows {
                eprint!("\n");
            }
        }
    } else {
        // Row count changed (e.g. todo bar appeared/disappeared): clear the old
        // region then draw fresh.
        erase_region(g);
        eprint!("{}", lines.join("\n"));
    }
    g.painted = rows;
    let _ = std::io::stderr().flush();
}

/// Erase the in-place region (move to its top row and clear to end of screen),
/// leaving the cursor at column 0 ready for fresh output. Resets `painted`.
fn erase_region(g: &mut Inner) {
    if g.painted > 1 {
        eprint!("\x1b[{}A", g.painted - 1);
    }
    eprint!("\r\x1b[0J");
    g.painted = 0;
}

/// Render the pinned todo progress bar. Width-bounded so it never wraps (which
/// would corrupt the in-place region accounting). Public so the input prompt
/// can show the same bar above itself between turns.
pub fn todo_bar(done: usize, total: usize, current: &str) -> String {
    // One cell per task for small lists (intuitive: 3/6 → ▰▰▰▱▱▱); a fixed
    // 10-cell percentage bar for large lists so it never gets too wide.
    let cells = if total == 0 {
        0
    } else if total <= 12 {
        total
    } else {
        10
    };
    let filled = if total == 0 {
        0
    } else {
        (done * cells / total).min(cells)
    };
    let bar = format!("{}{}", "▰".repeat(filled), "▱".repeat(cells - filled));

    // Budget the current-item text to the terminal width to avoid wrapping.
    let cols = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);
    let item_budget = cols.saturating_sub(28).min(60);
    let item = clip_cols(current, item_budget);
    let tail = if item.is_empty() {
        String::new()
    } else {
        format!("  {item}")
    };

    format!(
        "{} {} {}{}",
        "▸".bright_magenta(),
        format!("任务 {done}/{total}").bright_white(),
        bar.green(),
        tail.dimmed()
    )
}

/// Format a short duration: sub-second as "Nms", otherwise "Ns". Avoids the
/// confusing "0s" for fast (<1s) reasoning blocks.
fn fmt_dur(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{}s", d.as_secs())
    }
}

/// Fold the live reasoning preview into a compact dim breadcrumb. Assumes the
/// region was already erased. No-op if reasoning is not active.
fn fold_reasoning(g: &mut Inner) {
    if !g.reasoning {
        return;
    }
    let dur = g
        .reasoning_started
        .map(|t| fmt_dur(t.elapsed()))
        .unwrap_or_else(|| "0ms".into());
    eprintln!("{}", format!("∿ thought for {dur}").dimmed());
    let _ = std::io::stderr().flush();
    g.reasoning = false;
    g.reasoning_buf.clear();
    g.reasoning_started = None;
}

/// Clip a string to at most `max_cols` display columns, appending "…" when it
/// was truncated. Char-based, so it never panics on multi-byte input.
fn clip_cols(s: &str, max_cols: usize) -> String {
    let mut width = 0usize;
    let mut out = String::new();
    let mut truncated = false;
    for ch in s.chars() {
        let c = if ch.is_whitespace() { ' ' } else { ch };
        let w = char_width(c);
        if width + w > max_cols {
            truncated = true;
            break;
        }
        width += w;
        out.push(c);
    }
    if truncated {
        out.push('…');
    }
    out
}

/// Build a single-line, width-bounded dim preview of the *tail* of the
/// reasoning buffer. Whitespace (incl. newlines) is collapsed to single spaces;
/// the result is at most `max_cols` display columns, prefixed with "…" when the
/// buffer was longer. Iterates from the end so cost is O(max_cols), not O(buf).
fn reasoning_tail(s: &str, max_cols: usize) -> String {
    let mut width = 0usize;
    let mut rev: Vec<char> = Vec::new();
    let mut prev_space = false;
    let mut truncated = false;
    for ch in s.chars().rev() {
        let c = if ch.is_whitespace() { ' ' } else { ch };
        if c == ' ' {
            if prev_space {
                continue; // collapse runs of whitespace
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        let w = char_width(c);
        if width + w > max_cols {
            truncated = true;
            break;
        }
        width += w;
        rev.push(c);
    }
    let out: String = rev.iter().rev().collect();
    let out = out.trim().to_string();
    if truncated {
        format!("…{out}")
    } else {
        out
    }
}

/// Rough display width: CJK / fullwidth / emoji count as 2 columns, else 1.
/// Good enough to keep the in-place preview from wrapping; not a full
/// unicode-width implementation (avoids an extra dependency).
fn char_width(c: char) -> usize {
    let cp = c as u32;
    if (0x1100..=0x115F).contains(&cp)        // Hangul Jamo
        || (0x2E80..=0xA4CF).contains(&cp)     // CJK radicals … Yi
        || (0xAC00..=0xD7A3).contains(&cp)     // Hangul syllables
        || (0xF900..=0xFAFF).contains(&cp)     // CJK compatibility ideographs
        || (0xFE30..=0xFE4F).contains(&cp)     // CJK compatibility forms
        || (0xFF00..=0xFF60).contains(&cp)     // fullwidth forms
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x1F300..=0x1FAFF).contains(&cp)   // emoji / pictographs
    {
        2
    } else {
        1
    }
}
