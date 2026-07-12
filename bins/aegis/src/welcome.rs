use colored::Colorize;

/// Colour a string left-to-right across the same cool→warm palette used by the
/// thinking spinner (`tui::wave` / `status`), so the brand mark and the live
/// animation share one visual language.
fn gradient(s: &str) -> String {
    const PAL: [(u8, u8, u8); 6] = [
        (0x49, 0xE0, 0xE0),
        (0x4F, 0xC3, 0xF7),
        (0x7C, 0x9C, 0xFF),
        (0xB3, 0x88, 0xFF),
        (0xE0, 0x7C, 0xE0),
        (0x8A, 0xB4, 0xFF),
    ];
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len().max(1);
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        let (r, g, b) = PAL[(i * (PAL.len() - 1) / n).min(PAL.len() - 1)];
        out.push_str(&format!("{}", c.to_string().truecolor(r, g, b).bold()));
    }
    out
}

/// Print a compact Aegis welcome header.
///
/// Deliberately small (two lines) — the old full-screen ASCII banner took up
/// most of the viewport on every launch. Keeps the `🧿 aegis` brand mark and
/// magenta accent consistent with the input prompt.
pub fn print_welcome(version: &str, _model: &str, session_id: &str) {
    let dot = "·".dimmed();
    eprintln!();
    // Compact 3-row ASCII word-mark for aegis, in the same gradient as the
    // thinking animation. Three rows so the "E" gets a middle bar.
    eprintln!("  {}", gradient("▄▀▄ █▀▀ █▀▀ █ █▀▀"));
    eprintln!(
        "  {}   {}",
        gradient("█▀█ █▀▀ █ █ █ ▀▀▀"),
        format!("v{version}").dimmed(),
    );
    eprintln!("  {}", gradient("█ █ █▄▄ █▄█ █ ▄▄█"));
    eprintln!(
        "  {} {} {}",
        format!("session {session_id}").dimmed(),
        dot,
        "/help · /setup · Ctrl+C to interrupt".dimmed(),
    );
    // Running inside tmux? The REPL uses raw mode, so the mouse wheel is sent
    // as arrow keys (→ input history) and can't scroll scrollback. Tell the
    // user how to scroll long output via tmux itself.
    if std::env::var_os("TMUX").is_some() {
        eprintln!(
            "  {}",
            "tmux: 滚轮看历史请先 `tmux set -g mouse on`，或按 prefix+[ 进入复制模式滚动".dimmed()
        );
    }
    eprintln!();
}

/// Legacy full-screen ASCII welcome banner. **Preserved for later use** —
/// not called by default (replaced by the compact [`print_welcome`] above).
/// To restore it, call `print_welcome_banner` instead of `print_welcome`
/// from `run_chat`.
#[allow(dead_code)]
pub fn print_welcome_banner(version: &str, model: &str, session_id: &str) {
    use terminal_size::{terminal_size, Width};
    use unicode_width::UnicodeWidthStr;
    let term_width = terminal_size().map(|(Width(w), _)| w as usize).unwrap_or(80);

    let art_lines = [
        "         \u{2588}\u{2588}\u{2588}             \u{2588}\u{2588}\u{2588}             \u{2588}\u{2588}\u{2588}             \u{2588}\u{2588}\u{2588}             \u{2588}\u{2588}\u{2588} ",
        "       \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}  ",
        "     \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}    ",
        "   \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}      ",
        " \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}        ",
        "\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}          ",
        "\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            ",
        "            \u{2591}\u{2591}\u{2591}             \u{2591}\u{2591}\u{2591}             \u{2591}\u{2591}\u{2591}             \u{2591}\u{2591}\u{2591}             \u{2591}",
        "     \u{2588}\u{2588}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}  \u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}  \u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}  \u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}  \u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2588}\u{2588}     ",
        "   \u{2588}\u{2588}\u{2588}\u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}\u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}\u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}\u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}\u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}      ",
        " \u{2588}\u{2588}\u{2588}\u{2591} \u{2590}\u{2591}\u{2588}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2588}\u{2591}\u{258c}\u{2590}\u{2591}\u{2588}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580} \u{2590}\u{2591}\u{2588}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}  \u{2580}\u{2580}\u{2580}\u{2580}\u{2588}\u{2591}\u{2588}\u{2580}\u{2580}\u{2580}\u{2580} \u{2590}\u{2591}\u{2588}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}       ",
        "\u{2588}\u{2588}\u{2591}   \u{2590}\u{2591}\u{258c}       \u{2590}\u{2591}\u{258c}\u{2590}\u{2591}\u{258c}          \u{2590}\u{2591}\u{258c}               \u{2590}\u{2591}\u{258c}     \u{2590}\u{2591}\u{258c}  \u{2588}\u{2588}\u{2588}\u{2591}          ",
        "\u{2591}     \u{2590}\u{2591}\u{2588}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2588}\u{2591}\u{258c}\u{2590}\u{2591}\u{2588}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584} \u{2590}\u{2591}\u{258c} \u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}      \u{2590}\u{2591}\u{258c}     \u{2590}\u{2591}\u{2588}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}       ",
        "      \u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}\u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}\u{2590}\u{2591}\u{258c}\u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}     \u{2590}\u{2591}\u{258c}     \u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}    \u{2588}\u{2588}",
        "      \u{2590}\u{2591}\u{2588}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2588}\u{2591}\u{258c}\u{2590}\u{2591}\u{2588}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580} \u{2590}\u{2591}\u{258c} \u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2588}\u{2591}\u{258c}     \u{2590}\u{2591}\u{258c}      \u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2588}\u{2591}\u{258c}  \u{2588}\u{2588}\u{2588}\u{2591}",
        "      \u{2590}\u{2591}\u{258c}\u{2591}\u{2591}     \u{2590}\u{2591}\u{258c}\u{2590}\u{2591}\u{258c}          \u{2590}\u{2591}\u{258c}       \u{2590}\u{2591}\u{258c}     \u{2590}\u{2591}\u{258c}      \u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2588}\u{2591}\u{258c} \u{2591}\u{2591}\u{2591}  ",
        " \u{2588}\u{2588}\u{2588}  \u{2590}\u{2591}\u{258c}       \u{2590}\u{2591}\u{258c}\u{2590}\u{2591}\u{2588}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584} \u{2590}\u{2591}\u{2588}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2588}\u{2591}\u{258c} \u{2584}\u{2584}\u{2584}\u{2584}\u{2588}\u{2591}\u{2588}\u{2584}\u{2584}\u{2584}\u{2584}  \u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2584}\u{2588}\u{2591}\u{258c}      ",
        "\u{2588}\u{2588}\u{2591}   \u{2590}\u{2591}\u{258c}      \u{2588}\u{2590}\u{2591}\u{258c}\u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}\u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}\u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}\u{2590}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{258c}      ",
        "\u{2591}      \u{2580}     \u{2588}\u{2588}\u{2588}\u{2591}\u{2580}  \u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2588}\u{2591}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580} \u{2588}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}  \u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}\u{2580}       ",
        "           \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}",
        "         \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}",
        "       \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}  ",
        "     \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}    ",
        "    \u{2591}\u{2591}\u{2591}             \u{2591}\u{2591}\u{2591}             \u{2591}\u{2591}\u{2591}             \u{2591}\u{2591}\u{2591}             \u{2591}\u{2591}\u{2591}      ",
        "             \u{2588}\u{2588}\u{2588}             \u{2588}\u{2588}\u{2588}             \u{2588}\u{2588}\u{2588}             \u{2588}\u{2588}\u{2588}             ",
        "           \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}",
        "         \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}",
        "       \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}            \u{2588}\u{2588}\u{2588}\u{2591}  ",
    ];

    let noise_char = |row: usize, col: usize| -> Option<char> {
        let mut h = (row.wrapping_mul(2654435761) ^ col.wrapping_mul(1013904223)) as u32;
        h ^= h >> 13; h = h.wrapping_mul(1664525); h ^= h >> 17;
        match h % 60 {
            0       => Some('\u{2593}'),
            1 | 2   => Some('\u{2591}'),
            3..=8   => Some('\u{00b7}'),
            _       => None,
        }
    };
    let stripe_char = |row: usize, col: usize| -> char {
        if let Some(c) = noise_char(row, col) { return c; }
        let phase = ((col as isize) + (row as isize) * 2 - 9).rem_euclid(12) as usize;
        match phase {
            0       => '\u{2593}',
            1 | 2   => '\u{2591}',
            3       => '\u{00b7}',
            _       => ' ',
        }
    };

    let art_max_chars = art_lines.iter().map(|l| l.chars().count()).max().unwrap_or(77);
    let art_start = if term_width > art_max_chars { (term_width - art_max_chars) / 2 } else { 0 };

    const STRIPE_COLOR: &str = "\x1b[1;95m";
    const ART_COLOR:    &str = "\x1b[1;96m";
    const RESET:        &str = "\x1b[0m";

    eprintln!();
    for (row, line) in art_lines.iter().enumerate() {
        let art_chars: Vec<char> = line.chars().collect();
        let mut out = String::with_capacity(term_width * 8);
        let mut cur_is_art: Option<bool> = None;
        for col in 0..term_width {
            let bg = stripe_char(row, col);
            let idx = col.wrapping_sub(art_start);
            let (ch, is_art) = if idx < art_chars.len() {
                let c = art_chars[idx];
                match c {
                    ' ' | '\u{2588}' | '\u{2591}' => (bg, false),
                    _ => (c, true),
                }
            } else {
                (bg, false)
            };
            if cur_is_art != Some(is_art) {
                out.push_str(if is_art { ART_COLOR } else { STRIPE_COLOR });
                cur_is_art = Some(is_art);
            }
            out.push(ch);
        }
        out.push_str(RESET);
        eprintln!("{}", out);
    }

    let info_row1_plain = format!("  Aegis v{} \u{00b7} {} \u{00b7} session {}  ", version, model, session_id);
    let info_row2_plain = "  /help \u{00b7} /setup \u{00b7} Ctrl+C\u{00d7}2 exit  ";

    let print_info_row = |text_plain: &str, text_colored: &str, row: usize| {
        let text_display_len = text_plain.width();
        let col_start = term_width.saturating_sub(text_display_len) / 2;
        let col_end   = col_start + text_display_len;
        let mut full = String::with_capacity(term_width * 10);
        full.push_str(STRIPE_COLOR);
        for col in 0..col_start {
            full.push(stripe_char(row, col));
        }
        full.push_str("\x1b[0m");
        full.push_str(text_colored);
        full.push_str(STRIPE_COLOR);
        for col in col_end..term_width {
            full.push(stripe_char(row, col));
        }
        full.push_str(RESET);
        eprintln!("{}", full);
    };

    let row1_colored = format!("  Aegis v{} \u{00b7} {} \u{00b7} session {}  ",
        version.bright_white(),
        model.bright_white(),
        session_id.dimmed());
    let row2_colored = format!("{}", info_row2_plain.dimmed());

    let base_row = art_lines.len();
    print_info_row(&info_row1_plain, &row1_colored, base_row);
    print_info_row(info_row2_plain, &row2_colored, base_row + 1);

    {
        let mut out = String::with_capacity(term_width * 4);
        out.push_str(STRIPE_COLOR);
        for col in 0..term_width { out.push(stripe_char(base_row + 2, col)); }
        out.push_str(RESET);
        eprintln!("{}", out);
    }
    eprintln!();
}
