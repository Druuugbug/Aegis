//! Minimal, dependency-free Markdown → ANSI renderer for the assistant's reply.
//!
//! Renders the common elements (headings, bold/italic, inline code, fenced code
//! blocks, bullet/numbered lists, block quotes, horizontal rules). Tables are
//! left as-is for now (still readable). All scanning is char-based so it never
//! panics on multi-byte (CJK) input.

const BOLD: &str = "\x1b[1m";
const BOLD_OFF: &str = "\x1b[22m";
const ITALIC: &str = "\x1b[3m";
const ITALIC_OFF: &str = "\x1b[23m";
const DIM: &str = "\x1b[2m";
const DIM_OFF: &str = "\x1b[22m";
const CODE: &str = "\x1b[36m"; // cyan
const CODE_OFF: &str = "\x1b[39m";
const HEAD: &str = "\x1b[1;36m"; // bold cyan
const RESET: &str = "\x1b[0m";

/// Render Markdown text to an ANSI-styled string (no trailing newline).
pub fn render(md: &str) -> String {
    let mut out = String::with_capacity(md.len() + 64);
    let mut in_fence = false;
    for raw in md.lines() {
        let trimmed = raw.trim_start();

        // Fenced code block toggles.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            let lang = trimmed.trim_start_matches(['`', '~']).trim();
            out.push_str(DIM);
            out.push_str("```");
            out.push_str(lang);
            out.push_str(DIM_OFF);
            out.push('\n');
            continue;
        }
        if in_fence {
            // Verbatim, cyan, no inline processing.
            out.push_str(CODE);
            out.push_str(raw);
            out.push_str(CODE_OFF);
            out.push('\n');
            continue;
        }

        // Horizontal rule.
        if is_hr(trimmed) {
            out.push_str(DIM);
            out.push_str("────────────────────");
            out.push_str(DIM_OFF);
            out.push('\n');
            continue;
        }

        // Heading (#..######).
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && trimmed.chars().nth(hashes) == Some(' ') {
            let text: String = trimmed.chars().skip(hashes + 1).collect();
            out.push_str(HEAD);
            out.push_str(text.trim());
            out.push_str(RESET);
            out.push('\n');
            continue;
        }

        // Block quote.
        if let Some(rest) = trimmed.strip_prefix("> ") {
            out.push_str(DIM);
            out.push_str("│ ");
            out.push_str(&inline(rest));
            out.push_str(DIM_OFF);
            out.push('\n');
            continue;
        }

        // Bullet list (-, *, +) → •, preserving indentation.
        if let Some(rest) = bullet_rest(trimmed) {
            let indent_len = raw.len() - trimmed.len();
            out.push_str(&raw[..indent_len]);
            out.push_str("• ");
            out.push_str(&inline(rest));
            out.push('\n');
            continue;
        }

        // Default paragraph (incl. numbered lists, tables) with inline styling.
        out.push_str(&inline(raw));
        out.push('\n');
    }
    // Drop a single trailing newline for tidy printing.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

fn bullet_rest(trimmed: &str) -> Option<&str> {
    for p in ["- ", "* ", "+ "] {
        if let Some(r) = trimmed.strip_prefix(p) {
            return Some(r);
        }
    }
    None
}

fn is_hr(s: &str) -> bool {
    let s = s.trim();
    (s.len() >= 3)
        && (s.chars().all(|c| c == '-')
            || s.chars().all(|c| c == '*')
            || s.chars().all(|c| c == '_'))
}

/// Apply inline styling: `**bold**`, `*italic*`, `` `code` ``, `[text](url)`.
/// Non-nesting (keeps the inner text verbatim) to stay simple and panic-free.
fn inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // inline code
        if c == '`' {
            if let Some(end) = find(&chars, i + 1, |x| x == '`') {
                out.push_str(CODE);
                push_range(&mut out, &chars, i + 1, end);
                out.push_str(CODE_OFF);
                i = end + 1;
                continue;
            }
        }
        // bold ** .. **
        if c == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(end) = find_pair(&chars, i + 2, '*') {
                out.push_str(BOLD);
                push_range(&mut out, &chars, i + 2, end);
                out.push_str(BOLD_OFF);
                i = end + 2;
                continue;
            }
        }
        // bold __ .. __
        if c == '_' && chars.get(i + 1) == Some(&'_') {
            if let Some(end) = find_pair(&chars, i + 2, '_') {
                out.push_str(BOLD);
                push_range(&mut out, &chars, i + 2, end);
                out.push_str(BOLD_OFF);
                i = end + 2;
                continue;
            }
        }
        // italic * .. *
        if c == '*' {
            if let Some(end) = find(&chars, i + 1, |x| x == '*') {
                out.push_str(ITALIC);
                push_range(&mut out, &chars, i + 1, end);
                out.push_str(ITALIC_OFF);
                i = end + 1;
                continue;
            }
        }
        // link [text](url) → underlined text + dim url
        if c == '[' {
            if let Some(close) = find(&chars, i + 1, |x| x == ']') {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(paren) = find(&chars, close + 2, |x| x == ')') {
                        out.push_str("\x1b[4m");
                        push_range(&mut out, &chars, i + 1, close);
                        out.push_str("\x1b[24m ");
                        out.push_str(DIM);
                        out.push('(');
                        push_range(&mut out, &chars, close + 2, paren);
                        out.push(')');
                        out.push_str(DIM_OFF);
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn push_range(out: &mut String, chars: &[char], start: usize, end: usize) {
    for c in &chars[start..end] {
        out.push(*c);
    }
}

fn find(chars: &[char], from: usize, pred: impl Fn(char) -> bool) -> Option<usize> {
    (from..chars.len()).find(|&k| pred(chars[k]))
}

/// Find the next position of a doubled marker (e.g. `**`) starting at `from`.
fn find_pair(chars: &[char], from: usize, marker: char) -> Option<usize> {
    let mut k = from;
    while k + 1 <= chars.len() {
        if chars.get(k) == Some(&marker) && chars.get(k + 1) == Some(&marker) {
            return Some(k);
        }
        k += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_and_code() {
        let r = render("**hi** and `x`");
        assert!(r.contains("\x1b[1mhi\x1b[22m"));
        assert!(r.contains("\x1b[36mx\x1b[39m"));
    }

    #[test]
    fn heading_and_bullet() {
        let r = render("# Title\n- item");
        assert!(r.contains("Title"));
        assert!(r.contains("• item"));
    }

    #[test]
    fn fence_is_verbatim() {
        let r = render("```bash\ncat **a**\n```");
        // inside fence, ** is not turned into bold
        assert!(r.contains("cat **a**"));
    }

    #[test]
    fn cjk_does_not_panic() {
        let _ = render("**你好** `世界` 这是 *斜体*");
    }
}
