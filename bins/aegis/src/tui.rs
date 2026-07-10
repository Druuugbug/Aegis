//! Raw-mode single-owner TUI for the gateway interactive client.
//!
//! One thread reads raw keystrokes; the async client loop owns all terminal
//! output, drawing a bottom **live region** (animated status line + editable
//! input line) and printing scrollback above it — so the spinner animates AND
//! the user can type (queue/stop/answer) at the same time, with no flood.
//!
//! Zero new deps: raw mode via `libc` termios (already a unix dep), ANSI by
//! hand (as `status.rs` already does). Falls back to plain mode on non-TTY.

use std::io::Write;

use colored::Colorize;

/// A parsed key event from raw stdin.
#[derive(Debug, Clone)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Left,
    Right,
    Up,
    Down,
    CtrlC,
    CtrlD,
    CtrlU,
    CtrlL,
    Esc,
    Tab,
    /// Bracketed paste: all pasted text (may contain newlines) as one event.
    Paste(String),
}

// ─────────────────────────── raw mode ───────────────────────────

/// Puts the terminal in raw mode for its lifetime; restores on drop.
#[cfg(unix)]
pub struct RawGuard {
    fd: i32,
    orig: libc::termios,
}

#[cfg(unix)]
impl RawGuard {
    /// Enable raw mode (no canonical/echo/signal-gen; keep OPOST so `\n` still
    /// renders as a newline). Returns `None` if stdin isn't a TTY.
    pub fn enable() -> Option<Self> {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: isatty/tcgetattr/tcsetattr on the stdin fd.
        unsafe {
            if libc::isatty(fd) != 1 {
                return None;
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return None;
            }
            let orig = t;
            t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
            t.c_iflag &= !(libc::IXON | libc::ICRNL);
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                return None;
            }
            Some(Self { fd, orig })
        }
    }
}

#[cfg(unix)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        // Disable bracketed paste before restoring terminal.
        let _ = std::io::Write::write_all(&mut std::io::stderr(), b"\x1b[?2004l");
        // SAFETY: restore the saved termios.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
        }
    }
}

#[cfg(not(unix))]
pub struct RawGuard;
#[cfg(not(unix))]
impl RawGuard {
    pub fn enable() -> Option<Self> {
        None
    }
}

/// Whether stdin has a byte ready within `timeout_ms` (used to tell a lone ESC
/// from the start of an escape sequence). Always false on non-unix.
fn byte_ready(timeout_ms: i32) -> bool {
    #[cfg(unix)]
    {
        let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        // SAFETY: poll on a single fd with a timeout.
        unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 && (pfd.revents & libc::POLLIN) != 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = timeout_ms;
        false
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// Read one raw byte from stdin (fd 0), bypassing `std::io::Stdin`'s ~8KB
/// BufReader. Buffering would swallow the rest of an escape sequence into
/// userspace, defeating `byte_ready`'s `poll` and breaking arrow keys.
fn read1() -> Option<u8> {
    #[cfg(unix)]
    {
        let mut b = [0u8; 1];
        // SAFETY: blocking read of 1 byte from stdin (raw mode: VMIN=1).
        let n = unsafe { libc::read(0, b.as_mut_ptr() as *mut libc::c_void, 1) };
        if n == 1 {
            Some(b[0])
        } else {
            None
        }
    }
    #[cfg(not(unix))]
    {
        use std::io::Read;
        let mut b = [0u8; 1];
        match std::io::stdin().read(&mut b) {
            Ok(1) => Some(b[0]),
            _ => None,
        }
    }
}

/// Read bytes until the bracketed paste end sequence `\x1b[201~` is found.
/// Returns the pasted content as a String (newlines preserved).
fn read_bracketed_paste() -> String {
    let mut buf = Vec::new();
    loop {
        match read1() {
            None => break,
            Some(0x1b) => {
                // Check for paste-end: ESC [ 2 0 1 ~
                if byte_ready(10) {
                    if let Some(b'[') = read1() {
                        let mut seq = Vec::new();
                        loop {
                            match read1() {
                                Some(c @ b'0'..=b'9') => seq.push(c),
                                Some(b'~') => break,
                                _ => break,
                            }
                        }
                        if seq == b"201" {
                            break; // end of paste
                        }
                        // Not the end marker — push the consumed bytes as content
                        buf.push(0x1b);
                        buf.push(b'[');
                        buf.extend_from_slice(&seq);
                        buf.push(b'~');
                    } else {
                        buf.push(0x1b);
                    }
                } else {
                    buf.push(0x1b);
                }
            }
            Some(b) => buf.push(b),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Spawn a blocking thread that reads raw stdin and forwards parsed [`Key`]s.
pub fn spawn_key_reader(tx: tokio::sync::mpsc::UnboundedSender<Key>) {
    std::thread::spawn(move || {
        loop {
            let b = match read1() {
                Some(b) => b,
                None => break,
            };
            let key = match b {
                0x0d | 0x0a => Key::Enter,
                0x09 => Key::Tab,
                0x7f | 0x08 => Key::Backspace,
                0x03 => Key::CtrlC,
                0x04 => Key::CtrlD,
                0x15 => Key::CtrlU,
                0x0c => Key::CtrlL,
                0x1b => {
                    // Distinguish a lone ESC from an escape sequence (arrows) by
                    // polling briefly for a follow-up byte. No follow-up → ESC.
                    if !byte_ready(50) {
                        Key::Esc
                    } else {
                        match read1() {
                            Some(b'[') => {
                                match read1() {
                                    Some(b'A') => Key::Up,
                                    Some(b'B') => Key::Down,
                                    Some(b'C') => Key::Right,
                                    Some(b'D') => Key::Left,
                                    Some(b'2') => {
                                        // Could be bracketed paste start: 2 0 0 ~
                                        let mut seq = vec![b'2'];
                                        loop {
                                            match read1() {
                                                Some(c @ b'0'..=b'9') => seq.push(c),
                                                Some(b'~') => break,
                                                _ => break,
                                            }
                                        }
                                        if seq == b"200" {
                                            Key::Paste(read_bracketed_paste())
                                        } else {
                                            continue; // other CSI sequence
                                        }
                                    }
                                    Some(b'0'..=b'9') => {
                                        // e.g. ESC [ 3 ~ — consume until ~.
                                        loop {
                                            match read1() {
                                                Some(b'~') | None => break,
                                                Some(b'0'..=b'9') | Some(b';') => continue,
                                                Some(_) => break,
                                            }
                                        }
                                        continue;
                                    }
                                    _ => continue,
                                }
                            }
                            Some(b'O') => {
                                match read1() {
                                    Some(b'A') => Key::Up,
                                    Some(b'B') => Key::Down,
                                    Some(b'C') => Key::Right,
                                    Some(b'D') => Key::Left,
                                    _ => continue,
                                }
                            }
                            None => Key::Esc,
                            _ => continue,
                        }
                    }
                }
                0x00..=0x1f => continue, // other control bytes
                _ => {
                    let len = utf8_len(b);
                    let mut buf = vec![b];
                    for _ in 1..len {
                        match read1() {
                            Some(c) => buf.push(c),
                            None => break,
                        }
                    }
                    match std::str::from_utf8(&buf).ok().and_then(|s| s.chars().next()) {
                        Some(ch) => Key::Char(ch),
                        None => continue,
                    }
                }
            };
            if tx.send(key).is_err() {
                break;
            }
        }
    });
}

// ─────────────────────────── live region ───────────────────────────

/// Owns the bottom live region (status lines + input line) and prints
/// scrollback above it. All terminal writes go through one of these from the
/// single async loop, so there's never concurrent/torn output.
pub struct LiveRegion {
    painted: usize,
}

impl Default for LiveRegion {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveRegion {
    pub fn new() -> Self {
        Self { painted: 0 }
    }

    fn erase(&mut self) {
        if self.painted > 1 {
            eprint!("\x1b[{}A", self.painted - 1);
        }
        eprint!("\r\x1b[0J");
        self.painted = 0;
    }

    /// Repaint the live region in place. `lines` = status line(s) + input line
    /// (last). `cursor_col` = 0-based display column of the cursor in the last
    /// line. Flicker-free when the row count is unchanged.
    pub fn render(&mut self, lines: &[String], cursor_col: usize) {
        let n = lines.len();
        if n == 0 {
            return;
        }
        if self.painted == n {
            if n > 1 {
                eprint!("\x1b[{}A", n - 1);
            }
            eprint!("\r");
            for (i, l) in lines.iter().enumerate() {
                eprint!("{l}\x1b[K");
                if i + 1 < n {
                    eprint!("\n");
                }
            }
        } else {
            self.erase();
            for (i, l) in lines.iter().enumerate() {
                eprint!("{l}");
                if i + 1 < n {
                    eprint!("\n");
                }
            }
        }
        self.painted = n;
        eprint!("\r");
        if cursor_col > 0 {
            eprint!("\x1b[{cursor_col}C");
        }
        let _ = std::io::stderr().flush();
    }

    /// Print a scrollback block above the region, then repaint the region.
    pub fn print_above(&mut self, text: &str, lines: &[String], cursor_col: usize) {
        self.erase();
        eprint!("{text}\n");
        self.render(lines, cursor_col);
    }

    /// Erase the region entirely (e.g. on exit), leaving the cursor at col 0.
    pub fn clear(&mut self) {
        self.erase();
        let _ = std::io::stderr().flush();
    }
}

// ─────────────────────────── input line ───────────────────────────

/// Rough display width (CJK/emoji = 2). Mirrors `status::char_width`.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE30..=0xFE4F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x1F300..=0x1FAFF).contains(&cp)
    {
        2
    } else {
        1
    }
}

fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Clip plain text to at most `max` display columns (CJK/emoji = 2 cols),
/// collapsing newlines/tabs to spaces. NOTE: input must be PLAIN (no ANSI
/// escapes) — region lines are clipped *before* colouring so they never wrap
/// (a wrapped region line corrupts the in-place cursor math → flood).
pub fn clip_cols(s: &str, max: usize) -> String {
    let mut w = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let c = if ch == '\n' || ch == '\r' || ch == '\t' { ' ' } else { ch };
        let cw = char_width(c);
        if w + cw > max {
            break;
        }
        w += cw;
        out.push(c);
    }
    out
}

/// Build the visible input line (prompt + horizontally-scrolled input keeping
/// the cursor in view) and the cursor's display column. `cursor` is a char index.
pub fn input_line(prompt: &str, input: &str, cursor: usize, term_cols: usize) -> (String, usize) {
    let pw = str_width(prompt);
    // Reserve the last column: filling the final cell makes many terminals wrap
    // to a new row, which corrupts the in-place region accounting.
    let avail = term_cols.saturating_sub(pw + 1).max(8);
    let chars: Vec<char> = input.chars().collect();
    let cur = cursor.min(chars.len());
    let total: usize = chars.iter().map(|&c| char_width(c)).sum();

    if total <= avail {
        let cw: usize = chars[..cur].iter().map(|&c| char_width(c)).sum();
        return (format!("{prompt}{input}"), pw + cw);
    }
    // Scroll: choose a start so the cursor sits near the right edge.
    let mut start = cur;
    let mut acc = 0usize;
    while start > 0 {
        let w = char_width(chars[start - 1]);
        if acc + w > avail.saturating_sub(1) {
            break;
        }
        acc += w;
        start -= 1;
    }
    let mut vis = String::new();
    let mut vw = 0usize;
    for &c in &chars[start..] {
        let w = char_width(c);
        if vw + w > avail {
            break;
        }
        vis.push(c);
        vw += w;
    }
    let cur_col = pw + chars[start..cur].iter().map(|&c| char_width(c)).sum::<usize>();
    (format!("{prompt}{vis}"), cur_col)
}

// ─────────────────────────── spinner ───────────────────────────

const WAVE: [char; 14] = [
    '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█', '▇', '▆', '▅', '▄', '▃', '▂',
];

/// A 3-cell animated wave glyph for the status line (cool→warm cycle).
pub fn wave(idx: usize) -> String {
    let n = WAVE.len();
    let s: String = (0..3).map(|k| WAVE[(idx + k) % n]).collect();
    const PAL: [(u8, u8, u8); 6] = [
        (0x49, 0xE0, 0xE0),
        (0x4F, 0xC3, 0xF7),
        (0x7C, 0x9C, 0xFF),
        (0xB3, 0x88, 0xFF),
        (0xE0, 0x7C, 0xE0),
        (0x8A, 0xB4, 0xFF),
    ];
    let (r, g, b) = PAL[idx % PAL.len()];
    format!("{}", s.truecolor(r, g, b))
}

/// Current terminal width (columns), default 80.
pub fn term_cols() -> usize {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80)
}

/// Current terminal height (rows), default 24. Used to bound the live region's
/// activity window so the in-place repaint never scrolls past the screen.
pub fn term_rows() -> usize {
    terminal_size::terminal_size()
        .map(|(_, h)| h.0 as usize)
        .unwrap_or(24)
}
