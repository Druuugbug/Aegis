use aegis_types::message::{Message, Role};

/// Token-budget-aware context window manager.
pub struct ContextWindowManager {
    max_tokens: usize,
    reserve_for_output: usize,
    system_tokens: usize,
    messages: Vec<Message>,
}

impl ContextWindowManager {
    /// Create a context window manager with the given max token budget.
    pub fn new(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            reserve_for_output: 2048,
            system_tokens: 0,
            messages: Vec::new(),
        }
    }

    /// Estimate system prompt token usage (chars/4 rounded up).
    pub fn set_system_tokens(&mut self, prompt: &str) {
        self.system_tokens = prompt.len().div_ceil(4);
    }

    /// Tokens available for messages (excluding system prompt and output reserve).
    pub fn available_tokens(&self) -> usize {
        self.max_tokens
            .saturating_sub(self.reserve_for_output)
            .saturating_sub(self.system_tokens)
    }

    /// The total token budget (context window size), reflecting any learned /
    /// overridden value applied via `set_max_tokens`.
    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    /// Update the total token budget (e.g. after learning the model's real
    /// context window from a provider error).
    pub fn set_max_tokens(&mut self, max_tokens: usize) {
        self.max_tokens = max_tokens;
    }

    /// Estimate token count for a single message with CJK awareness.
    /// CJK characters average ~1 token each; ASCII averages ~4 chars per token.
    pub fn estimate_tokens(msg: &Message) -> usize {
        let text = msg.text();
        let mut tokens = 4; // role + framing overhead
        let mut ascii_chars = 0usize;
        for c in text.chars() {
            if is_cjk_char(c) {
                tokens += 1;
            } else {
                ascii_chars += 1;
            }
        }
        tokens += ascii_chars / 4;
        tokens
    }

    /// Push a message. Returns false if it would exceed available tokens.
    pub fn push(&mut self, msg: Message) -> bool {
        let msg_tokens = Self::estimate_tokens(&msg);
        let current_usage: usize = self.messages.iter().map(Self::estimate_tokens).sum();
        if current_usage + msg_tokens > self.available_tokens() {
            return false;
        }
        self.messages.push(msg);
        true
    }

    /// Remove earliest non-system messages until total fits within available tokens.
    pub fn trim_to_fit(&mut self) {
        let available = self.available_tokens();
        loop {
            let usage: usize = self.messages.iter().map(Self::estimate_tokens).sum();
            if usage <= available {
                break;
            }
            if let Some(pos) = self.messages.iter().position(|m| m.role != Role::System) {
                self.messages.remove(pos);
            } else {
                break;
            }
        }
    }

    /// Returns (used_tokens, available_tokens).
    pub fn token_usage(&self) -> (usize, usize) {
        let used: usize = self.messages.iter().map(Self::estimate_tokens).sum();
        (used, self.available_tokens())
    }

    /// Get a read-only view of the current messages in the window.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Replace internal messages (for syncing from external history).
    pub fn set_messages(&mut self, msgs: Vec<Message>) {
        self.messages = msgs;
    }

    /// Move the internal messages out, leaving the window empty. Lets callers
    /// reclaim the (possibly trimmed) buffer without cloning.
    pub fn take_messages(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.messages)
    }
}

fn is_cjk_char(c: char) -> bool {
    let cp = c as u32;
    (0x4E00..=0x9FFF).contains(&cp)    // CJK Unified Ideographs
        || (0x3400..=0x4DBF).contains(&cp)  // CJK Extension A
        || (0x3000..=0x303F).contains(&cp)  // CJK Symbols
        || (0x3040..=0x309F).contains(&cp)  // Hiragana
        || (0x30A0..=0x30FF).contains(&cp)  // Katakana
        || (0xAC00..=0xD7AF).contains(&cp)  // Hangul
        || (0xFF00..=0xFFEF).contains(&cp)  // Fullwidth forms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trim_to_fit() {
        let mut mgr = ContextWindowManager::new(4096);
        // available = 4096 - 2048 = 2048
        // Push many messages to exceed budget
        let mut msgs = Vec::new();
        for i in 0..500 {
            let text = format!("message number {i} with some extra padding text here");
            msgs.push(Message::user(&text));
        }
        mgr.set_messages(msgs);
        mgr.trim_to_fit();
        let (used, available) = mgr.token_usage();
        assert!(used <= available, "used {used} > available {available}");
        assert!(!mgr.messages().is_empty());
    }

    #[test]
    fn test_push_returns_false_when_full() {
        // max=2100, reserve=2048 → available=52 tokens
        let mut mgr = ContextWindowManager::new(2100);
        // First small message should fit: len("hi")/4+4 = 4+4? no, 2/4=0+4=4
        assert!(mgr.push(Message::user("hi")));
        // Now push a huge message that won't fit
        let big = "x".repeat(300); // 300/4+4 = 79 tokens, total would be 83 > 52
        assert!(!mgr.push(Message::user(&big)));
    }
}
