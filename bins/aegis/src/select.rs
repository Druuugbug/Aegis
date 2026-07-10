//! Arrow-key option selector for `clarify` questions — no TUI framework.
//!
//! Renders preset options for each question and lets the user navigate with the
//! arrow keys:
//! - ↑/↓ : move the highlight between options (last row is "manual input")
//! - ←/→ : switch between questions (when the model asks more than one)
//! - Enter: confirm the current question; when all are answered, finish
//! - digits 1-9: jump straight to an option
//!
//! Implemented with a tiny libc termios raw mode (ICANON/ECHO off) so we can
//! read individual keypresses. Degrades to a plain numbered stdin prompt when
//! stdin is not a TTY or on non-unix targets.

use std::io::Write;

use aegis_core::agent::ClarifyQuestion;
use colored::Colorize;

/// Label shown for the always-present "type my own answer" row.
const MANUAL_LABEL: &str = "其他（手动输入）";

/// Run the selector. Returns one answer per question (in order).
pub fn run(questions: &[ClarifyQuestion]) -> Vec<String> {
    if questions.is_empty() {
        return Vec::new();
    }
    #[cfg(unix)]
    {
        if is_tty() {
            if let Some(answers) = run_interactive(questions) {
                return answers;
            }
        }
    }
    run_fallback(questions)
}

/// Single-select arrow-key menu (with a scrolling viewport so long lists never
/// overflow the screen). Returns the chosen index, or `None` if cancelled.
/// Falls back to a numbered stdin prompt when not a TTY / non-unix.
pub fn pick(title: &str, items: &[String]) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    #[cfg(unix)]
    {
        if is_tty() {
            return pick_interactive(title, items);
        }
    }
    pick_fallback(title, items)
}

fn pick_fallback(title: &str, items: &[String]) -> Option<usize> {
    eprintln!("\n{} {}", "▸".bright_magenta(), title.bright_white());
    for (i, it) in items.iter().enumerate() {
        eprintln!("  {}. {}", i + 1, it);
    }
    eprint!("{} ", "选编号 ›".bright_magenta());
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok()?;
    let n: usize = input.trim().parse().ok()?;
    if n >= 1 && n <= items.len() {
        Some(n - 1)
    } else {
        None
    }
}

#[cfg(unix)]
fn pick_interactive(title: &str, items: &[String]) -> Option<usize> {
    let _guard = RawGuard::enable()?;
    let mut sel = 0usize;
    let mut prev = 0usize;
    let result;
    loop {
        prev = render_menu(title, items, sel, prev);
        match read_key() {
            Key::Up => sel = (sel + items.len() - 1) % items.len(),
            Key::Down => sel = (sel + 1) % items.len(),
            Key::Enter => {
                result = Some(sel);
                break;
            }
            Key::Abort => {
                result = None;
                break;
            }
            _ => {}
        }
    }
    erase(prev);
    result
}

/// Render a single-select menu with a scrolling viewport. Returns rows drawn.
#[cfg(unix)]
fn render_menu(title: &str, items: &[String], sel: usize, prev_lines: usize) -> usize {
    erase(prev_lines);
    const MAX_VIS: usize = 10;
    let cols = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);
    let n = items.len();
    let visible = n.min(MAX_VIS);
    let start = if sel < visible {
        0
    } else {
        (sel + 1).saturating_sub(visible).min(n.saturating_sub(visible))
    };

    let mut rows = 0usize;
    let counter = if n > visible {
        format!("  ({}/{})", sel + 1, n)
    } else {
        String::new()
    };
    eprintln!(
        "\n{} {}{}",
        "▸".bright_magenta(),
        clip(title, cols.saturating_sub(12)).bright_white(),
        counter.dimmed()
    );
    rows += 2;
    for i in start..start + visible {
        let label = clip(&items[i], cols.saturating_sub(8));
        if i == sel {
            eprintln!("  {} {}", "❯".bright_magenta(), label.bright_white().bold());
        } else {
            eprintln!("    {}", label.dimmed());
        }
        rows += 1;
    }
    eprint!("  {}", "↑/↓ 选择 · Enter 确认 · Esc 取消".dimmed());
    let _ = std::io::stderr().flush();
    rows += 1;
    rows
}

/// Plain numbered prompt used when interactive mode is unavailable.
fn run_fallback(questions: &[ClarifyQuestion]) -> Vec<String> {
    let mut answers = Vec::with_capacity(questions.len());
    for q in questions {
        eprintln!("\n{} {}", "❓".bright_yellow(), q.question.bright_white());
        for (i, opt) in q.options.iter().enumerate() {
            eprintln!("  {}. {}", i + 1, opt);
        }
        if q.options.is_empty() {
            eprint!("{} ", "›".bright_magenta());
        } else {
            eprint!("{} ", format!("选 1-{} 或直接输入 ›", q.options.len()).bright_magenta());
        }
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        let _ = std::io::stdin().read_line(&mut input);
        let input = input.trim();
        let ans = match input.parse::<usize>() {
            Ok(n) if n >= 1 && n <= q.options.len() => q.options[n - 1].clone(),
            _ => input.to_string(),
        };
        answers.push(ans);
    }
    answers
}

#[cfg(unix)]
fn is_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

#[cfg(unix)]
#[derive(PartialEq)]
enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Abort,
    Digit(usize),
    Other,
}

#[cfg(unix)]
fn run_interactive(questions: &[ClarifyQuestion]) -> Option<Vec<String>> {
    let _guard = RawGuard::enable()?;
    let n = questions.len();
    let mut sel: Vec<usize> = vec![0; n]; // highlighted row per question
    let mut answered: Vec<Option<String>> = vec![None; n];
    let mut cur = 0usize;
    let mut prev_lines = 0usize;

    loop {
        prev_lines = render(questions, &sel, &answered, cur, prev_lines);
        match read_key() {
            Key::Up => {
                let rows = questions[cur].options.len() + 1;
                sel[cur] = (sel[cur] + rows - 1) % rows;
            }
            Key::Down => {
                let rows = questions[cur].options.len() + 1;
                sel[cur] = (sel[cur] + 1) % rows;
            }
            Key::Left if n > 1 => cur = (cur + n - 1) % n,
            Key::Right if n > 1 => cur = (cur + 1) % n,
            Key::Digit(d) => {
                let rows = questions[cur].options.len() + 1;
                if d >= 1 && d <= rows {
                    sel[cur] = d - 1;
                }
            }
            Key::Enter => {
                let q = &questions[cur];
                let answer = if sel[cur] == q.options.len() {
                    // "manual input" row → read a free-text line in place.
                    prompt_manual(&q.question)
                } else {
                    q.options[sel[cur]].clone()
                };
                answered[cur] = Some(answer);
                // Force a fresh redraw (manual prompt printed extra lines).
                prev_lines = 0;
                if answered.iter().all(|a| a.is_some()) {
                    break;
                }
                for step in 1..=n {
                    let idx = (cur + step) % n;
                    if answered[idx].is_none() {
                        cur = idx;
                        break;
                    }
                }
            }
            Key::Abort => break,
            _ => {}
        }
    }

    // Wipe the menu region.
    erase(prev_lines);

    let answers: Vec<String> = (0..n)
        .map(|i| {
            answered[i].clone().unwrap_or_else(|| {
                let q = &questions[i];
                if sel[i] < q.options.len() {
                    q.options[sel[i]].clone()
                } else {
                    String::new()
                }
            })
        })
        .collect();
    // Leave a compact record of what was chosen.
    for ans in &answers {
        if !ans.is_empty() {
            eprintln!("  {} {}", "●".green(), ans.dimmed());
        }
    }
    Some(answers)
}

/// Repaint the menu in place. Returns the number of rows drawn.
#[cfg(unix)]
fn render(
    questions: &[ClarifyQuestion],
    sel: &[usize],
    answered: &[Option<String>],
    cur: usize,
    prev_lines: usize,
) -> usize {
    erase(prev_lines);

    let cols = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);
    let n = questions.len();
    let q = &questions[cur];
    let mut rows = 0usize;

    // Header.
    let stage = if n > 1 {
        format!("  ({}/{}  ←/→ 切换)", cur + 1, n)
    } else {
        String::new()
    };
    eprintln!(
        "\n{} {}{}",
        "❓".bright_yellow(),
        clip(&q.question, cols.saturating_sub(20)).bright_white(),
        stage.dimmed()
    );
    rows += 2; // blank line + header

    // Option rows + manual row.
    let total_rows = q.options.len() + 1;
    for i in 0..total_rows {
        let label = if i < q.options.len() {
            clip(&q.options[i], cols.saturating_sub(10))
        } else {
            MANUAL_LABEL.to_string()
        };
        let num = i + 1;
        if i == sel[cur] {
            eprintln!(
                "  {} {}",
                "▸".bright_magenta(),
                format!("{num}. {label}").bright_white().bold()
            );
        } else {
            eprintln!("    {}", format!("{num}. {label}").dimmed());
        }
        rows += 1;
    }

    // Footer hint + per-question answered ticks (multi only).
    let nav = if n > 1 {
        "↑/↓ 选择 · ←/→ 切换问题 · Enter 确认"
    } else {
        "↑/↓ 选择 · Enter 确认"
    };
    eprint!("  {}", nav.dimmed());
    if n > 1 {
        let ticks: String = (0..n)
            .map(|i| if answered[i].is_some() { '●' } else { '○' })
            .collect();
        eprint!("   {}", ticks.dimmed());
    }
    let _ = std::io::stderr().flush();
    rows += 1; // footer (no trailing newline)
    rows
}

/// Move to the top of a `lines`-row region and clear it.
#[cfg(unix)]
fn erase(lines: usize) {
    if lines == 0 {
        return;
    }
    if lines > 1 {
        eprint!("\x1b[{}A", lines - 1);
    }
    eprint!("\r\x1b[0J");
    let _ = std::io::stderr().flush();
}

/// Read a free-text line while in raw mode (echoes raw bytes so multi-byte
/// input still renders; basic backspace support).
#[cfg(unix)]
fn prompt_manual(question: &str) -> String {
    use std::io::Read;
    eprintln!("\n{} {}", "✎".bright_magenta(), question.dimmed());
    eprint!("{} ", "›".bright_magenta());
    let _ = std::io::stderr().flush();

    let mut bytes: Vec<u8> = Vec::new();
    let mut b = [0u8; 1];
    loop {
        match std::io::stdin().read(&mut b) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        match b[0] {
            b'\r' | b'\n' => break,
            0x03 => {
                bytes.clear();
                break;
            }
            0x7f | 0x08 => {
                if bytes.pop().is_some() {
                    eprint!("\x08 \x08");
                    let _ = std::io::stderr().flush();
                }
            }
            _ => {
                bytes.push(b[0]);
                let _ = std::io::stderr().write_all(&b);
                let _ = std::io::stderr().flush();
            }
        }
    }
    eprintln!();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

#[cfg(unix)]
fn read_key() -> Key {
    use std::io::Read;
    let mut buf = [0u8; 8];
    let n = match std::io::stdin().read(&mut buf) {
        Ok(0) | Err(_) => return Key::Abort,
        Ok(n) => n,
    };
    if n >= 3 && buf[0] == 0x1b && buf[1] == b'[' {
        return match buf[2] {
            b'A' => Key::Up,
            b'B' => Key::Down,
            b'C' => Key::Right,
            b'D' => Key::Left,
            _ => Key::Other,
        };
    }
    match buf[0] {
        b'\r' | b'\n' => Key::Enter,
        0x03 | 0x1b => Key::Abort,
        b'k' => Key::Up,
        b'j' => Key::Down,
        b'h' => Key::Left,
        b'l' => Key::Right,
        d @ b'1'..=b'9' => Key::Digit((d - b'0') as usize),
        _ => Key::Other,
    }
}

/// Clip a string to at most `max_cols` display columns (CJK counts as 2),
/// appending "…" when truncated. Char-based, never panics on multi-byte input.
#[cfg(unix)]
fn clip(s: &str, max_cols: usize) -> String {
    let mut width = 0usize;
    let mut out = String::new();
    let mut truncated = false;
    for ch in s.chars() {
        let c = if ch == '\n' || ch == '\r' { ' ' } else { ch };
        let w = char_cols(c);
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

#[cfg(unix)]
fn char_cols(c: char) -> usize {
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

#[cfg(unix)]
struct RawGuard {
    fd: i32,
    orig: libc::termios,
}

#[cfg(unix)]
impl RawGuard {
    fn enable() -> Option<Self> {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
            return None;
        }
        let mut raw = orig;
        // Unbuffered, no echo — but keep ISIG so Ctrl+C still raises SIGINT
        // (the chat turn's ctrl_c watcher then cancels), and keep OPOST so
        // normal "\n" output still works while we print the menu.
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(RawGuard { fd, orig })
    }
}

#[cfg(unix)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
        }
    }
}
