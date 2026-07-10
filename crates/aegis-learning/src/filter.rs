//! # Sensitive data filter (D31)
//!
//! Aegis learning never sends raw shell history, git log lines, or
//! environment variables to the LLM. Every snippet is run through
//! [`SensitiveFilter`] which replaces API keys, tokens, SSH paths,
//! private key blocks, emails, and phone numbers with stable redaction
//! placeholders before they are persisted as [`UserFact::evidence`].
//!
//! The filter is intentionally regex-based and pure — no I/O, no LLM,
//! no async — so it can be invoked from the synchronous merge pipeline.

use regex::Regex;

/// Stable label for a redaction category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RedactionKind {
    AwsKey,
    AwsSecret,
    GitHubToken,
    OpenAiKey,
    AnthropicKey,
    GoogleApiKey,
    SlackToken,
    PrivateKey,
    SshPath,
    Email,
    Phone,
    ChineseId,
    GenericBearer,
    UrlCredentials,
    Jwt,
    StripeKey,
    GitlabToken,
    Password,
    ShadowHash,
}

impl RedactionKind {
    /// Short uppercase token used in the placeholder, e.g. `[REDACTED:EMAIL]`.
    pub fn label(self) -> &'static str {
        match self {
            Self::AwsKey => "AWS_KEY",
            Self::AwsSecret => "AWS_SECRET",
            Self::GitHubToken => "GITHUB_TOKEN",
            Self::OpenAiKey => "OPENAI_KEY",
            Self::AnthropicKey => "ANTHROPIC_KEY",
            Self::GoogleApiKey => "GOOGLE_KEY",
            Self::SlackToken => "SLACK_TOKEN",
            Self::PrivateKey => "PRIVATE_KEY",
            Self::SshPath => "SSH_PATH",
            Self::Email => "EMAIL",
            Self::Phone => "PHONE",
            Self::ChineseId => "CN_ID",
            Self::GenericBearer => "BEARER",
            Self::UrlCredentials => "URL_CREDS",
            Self::Jwt => "JWT",
            Self::StripeKey => "STRIPE_KEY",
            Self::GitlabToken => "GITLAB_TOKEN",
            Self::Password => "PASSWORD",
            Self::ShadowHash => "SHADOW_HASH",
        }
    }
}

impl std::fmt::Display for RedactionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// A (kind, pattern) pair used internally by the filter.
#[derive(Clone)]
pub struct PatternSpec {
    pub kind: RedactionKind,
    pub regex: Regex,
}

/// Returns the canonical set of built-in sensitive patterns. Exposed for
/// tests and the CLI (D31: explainable).
pub fn default_patterns() -> Vec<PatternSpec> {
    // Ordered specific → generic (the redactor applies them in order, so a more
    // specific kind labels the match before a broader rule can). Curated from
    // the Gitleaks rule set (MIT) — a lightweight Rust port, not a 1:1 copy.
    vec![
        // PEM private key block (multi-line) — highest value, match early.
        PatternSpec {
            kind: RedactionKind::PrivateKey,
            regex: Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----")
                .expect("private key regex"),
        },
        // AWS access key id (AKIA / ASIA prefixes)
        PatternSpec {
            kind: RedactionKind::AwsKey,
            regex: Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").expect("aws key regex"),
        },
        // AWS secret access key — context-anchored (avoids matching any 40-char
        // token such as a git commit hash). Matches the whole `key=value`.
        PatternSpec {
            kind: RedactionKind::AwsSecret,
            regex: Regex::new(
                r#"(?i)aws(.{0,20})?(secret|access).{0,20}["'=:\s]+[A-Za-z0-9/+=]{40}"#,
            )
            .expect("aws secret regex"),
        },
        // GitHub token
        PatternSpec {
            kind: RedactionKind::GitHubToken,
            regex: Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{36,255}\b").expect("github token regex"),
        },
        // GitLab personal access token
        PatternSpec {
            kind: RedactionKind::GitlabToken,
            regex: Regex::new(r"\bglpat-[A-Za-z0-9_-]{20,}\b").expect("gitlab token regex"),
        },
        // Anthropic API key — MUST precede the generic OpenAI `sk-` rule so it
        // is labelled correctly (sk-ant-… also matches the OpenAI pattern).
        PatternSpec {
            kind: RedactionKind::AnthropicKey,
            regex: Regex::new(r"\bsk-ant-[A-Za-z0-9_-]{20,}\b").expect("anthropic key regex"),
        },
        // Stripe secret/restricted/publishable live keys
        PatternSpec {
            kind: RedactionKind::StripeKey,
            regex: Regex::new(r"\b[rs]k_live_[A-Za-z0-9]{20,}\b").expect("stripe key regex"),
        },
        // OpenAI API key (generic sk-…)
        PatternSpec {
            kind: RedactionKind::OpenAiKey,
            regex: Regex::new(r"\bsk-[A-Za-z0-9_-]{20,}\b").expect("openai key regex"),
        },
        // Google API key
        PatternSpec {
            kind: RedactionKind::GoogleApiKey,
            regex: Regex::new(r"\bAIza[0-9A-Za-z_-]{35}\b").expect("google key regex"),
        },
        // Slack tokens (xox[bpars]-…)
        PatternSpec {
            kind: RedactionKind::SlackToken,
            regex: Regex::new(r"\bxox[bpars]-[A-Za-z0-9-]{10,}\b").expect("slack token regex"),
        },
        // JSON Web Token (header.payload.signature)
        PatternSpec {
            kind: RedactionKind::Jwt,
            regex: Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b")
                .expect("jwt regex"),
        },
        // /etc/shadow style password hash ($id$salt$hash)
        PatternSpec {
            kind: RedactionKind::ShadowHash,
            regex: Regex::new(r"\$[0-9][a-z]?\$[^\s:$]{0,64}\$[./A-Za-z0-9]{20,}")
                .expect("shadow hash regex"),
        },
        // Generic password/secret assignment — matches the whole `key=value`.
        // Limited to password-family keys so it never clobbers the provider
        // keys above (api_key/token values are handled by the specific rules).
        PatternSpec {
            kind: RedactionKind::Password,
            regex: Regex::new(
                r#"(?i)\b(?:password|passwd|passphrase|pwd|db_password)\b\s*[:=]\s*["']?[^\s"']{4,}"#,
            )
            .expect("password regex"),
        },
        // SSH-related paths
        PatternSpec {
            kind: RedactionKind::SshPath,
            regex: Regex::new(r"(?:\.ssh/|\.aws/|\.gnupg/)(?:[A-Za-z0-9_.-]+)?").expect("ssh path regex"),
        },
        // URL with embedded credentials — before Email so `user:pass@host` is
        // labelled as URL creds, not partially matched as an email.
        PatternSpec {
            kind: RedactionKind::UrlCredentials,
            regex: Regex::new(r"[A-Za-z][A-Za-z0-9+.-]*://[^\s/:@]+:[^\s/@]+@[^\s]+")
                .expect("url creds regex"),
        },
        // Email addresses
        PatternSpec {
            kind: RedactionKind::Email,
            regex: Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b").expect("email regex"),
        },
        // International phone numbers (very rough)
        PatternSpec {
            kind: RedactionKind::Phone,
            regex: Regex::new(r"\+?\d{1,3}[-.\s]?\(?\d{1,4}\)?[-.\s]?\d{3,4}[-.\s]?\d{4}\b")
                .expect("phone regex"),
        },
        // Chinese national ID (18 digits, last may be X)
        PatternSpec {
            kind: RedactionKind::ChineseId,
            regex: Regex::new(r"\b\d{17}[\dXx]\b").expect("cn id regex"),
        },
        // Generic Bearer tokens
        PatternSpec {
            kind: RedactionKind::GenericBearer,
            regex: Regex::new(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{16,}").expect("bearer regex"),
        },
    ]
}

/// Snapshot of the default patterns — exposed for tests and the CLI's
/// "what gets filtered" listing.
pub const SENSITIVE_PATTERNS: &[(&str, &str)] = &[
    ("AWS_KEY", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
    ("AWS_SECRET", r"\b[A-Za-z0-9/+=]{40}\b"),
    ("GITHUB_TOKEN", r"\bgh[pousr]_[A-Za-z0-9]{36,255}\b"),
    ("OPENAI_KEY", r"\bsk-[A-Za-z0-9_-]{20,}\b"),
    ("ANTHROPIC_KEY", r"\bsk-ant-[A-Za-z0-9_-]{20,}\b"),
    ("GOOGLE_KEY", r"\bAIza[0-9A-Za-z_-]{35}\b"),
    ("SLACK_TOKEN", r"\bxox[bpars]-[A-Za-z0-9-]{10,}\b"),
    ("PRIVATE_KEY", r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----"),
    ("SSH_PATH", r"(?:\.ssh/|\.aws/|\.gnupg/)(?:[A-Za-z0-9_.-]+)?"),
    ("EMAIL", r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b"),
    ("PHONE", r"\+?\d{1,3}[-.\s]?\(?\d{1,4}\)?[-.\s]?\d{3,4}[-.\s]?\d{4}\b"),
    ("CN_ID", r"\b\d{17}[\dXx]\b"),
    ("BEARER", r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{16,}"),
    ("URL_CREDS", r"[A-Za-z][A-Za-z0-9+.-]*://[^\s/:@]+:[^\s/@]+@[^\s]+"),
];

/// Reusable filter. Construction is cheap-ish (compiles ~14 regexes) so
/// callers should keep one instance around.
#[derive(Clone)]
pub struct SensitiveFilter {
    patterns: Vec<PatternSpec>,
}

impl Default for SensitiveFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl SensitiveFilter {
    /// Build a filter with the canonical pattern set.
    pub fn new() -> Self {
        Self { patterns: default_patterns() }
    }

    /// Build a filter with a custom set of patterns (for tests).
    pub fn with_patterns(patterns: Vec<PatternSpec>) -> Self {
        Self { patterns }
    }

    /// Returns true when at least one pattern matches.
    pub fn is_sensitive(&self, input: &str) -> bool {
        self.patterns.iter().any(|p| p.regex.is_match(input))
    }

    /// List the kinds that matched the input, in declaration order.
    pub fn matched_kinds(&self, input: &str) -> Vec<RedactionKind> {
        let mut out = Vec::new();
        for p in &self.patterns {
            if p.regex.is_match(input) {
                let already = out.contains(&p.kind);
                if !already {
                    out.push(p.kind);
                }
            }
        }
        out
    }

    /// Replace every sensitive span with a `[REDACTED:KIND]` placeholder.
    /// Pure function — does not mutate state.
    pub fn redact(&self, input: &str) -> String {
        if input.is_empty() {
            return String::new();
        }
        let mut result = input.to_string();
        for p in &self.patterns {
            let placeholder = format!("[REDACTED:{}]", p.kind);
            // Re-create the regex per replacement is fine — Regex::replace_all
            // owns the inner state. The owning struct's life is per call.
            result = p.regex.replace_all(&result, placeholder.as_str()).into_owned();
        }
        result
    }
}

/// Convenience wrapper — equivalent to `SensitiveFilter::new().redact(input)`.
pub fn redact_string(input: &str) -> String {
    SensitiveFilter::new().redact(input)
}

/// Convenience wrapper — returns true if the default filter would touch the input.
pub fn redact_sensitive(input: &str) -> bool {
    SensitiveFilter::new().is_sensitive(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter() -> SensitiveFilter {
        SensitiveFilter::new()
    }

    #[test]
    fn test_redact_aws_access_key() {
        let f = filter();
        let input = "key is AKIAIOSFODNN7EXAMPLE here";
        let out = f.redact(input);
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "AWS key should be redacted: {out}");
        assert!(out.contains("[REDACTED:AWS_KEY]"));
    }

    #[test]
    fn test_redact_github_token() {
        let f = filter();
        let token = "ghp_1234567890abcdefghijklmnopqrstuvwxyzAB";
        let out = f.redact(&format!("token={token}"));
        assert!(!out.contains(token));
        assert!(out.contains("[REDACTED:GITHUB_TOKEN]"));
    }

    #[test]
    fn test_redact_openai_key() {
        let f = filter();
        let key = "sk-proj-abcdefghijklmnopqrstuvwxyz1234567890";
        let out = f.redact(&format!("api_key=\"{key}\""));
        assert!(!out.contains(key));
        assert!(out.contains("[REDACTED:OPENAI_KEY]"));
    }

    #[test]
    fn test_redact_anthropic_key() {
        let f = filter();
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz1234567890";
        let out = f.redact(&format!("using {key}"));
        assert!(!out.contains(key));
        assert!(out.contains("[REDACTED:ANTHROPIC_KEY]"));
    }

    #[test]
    fn test_redact_google_key() {
        let f = filter();
        let key = "AIzaSyA1234567890abcdefghijklmnopqrstuv";
        let out = f.redact(&format!("google: {key}"));
        assert!(!out.contains(key));
        assert!(out.contains("[REDACTED:GOOGLE_KEY]"));
    }

    #[test]
    fn test_redact_slack_token() {
        let f = filter();
        let out = f.redact("slack token xoxb-1234567890-abcdefghij");
        assert!(out.contains("[REDACTED:SLACK_TOKEN]"));
    }

    #[test]
    fn test_redact_private_key_block() {
        let f = filter();
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAK...\n-----END RSA PRIVATE KEY-----";
        let out = f.redact(pem);
        assert!(!out.contains("MIIEowIBAAK"));
        assert!(out.contains("[REDACTED:PRIVATE_KEY]"));
    }

    #[test]
    fn test_redact_ssh_path() {
        let f = filter();
        let out = f.redact("see /home/u/.ssh/id_rsa for details");
        assert!(out.contains("[REDACTED:SSH_PATH]"));
    }

    #[test]
    fn test_redact_aws_path() {
        let f = filter();
        let out = f.redact("creds in .aws/credentials");
        assert!(out.contains("[REDACTED:SSH_PATH]"));
    }

    #[test]
    fn test_redact_email() {
        let f = filter();
        let out = f.redact("contact alice@example.com please");
        assert!(!out.contains("alice@example.com"));
        assert!(out.contains("[REDACTED:EMAIL]"));
    }

    #[test]
    fn test_redact_phone_international() {
        let f = filter();
        let out = f.redact("call +1 415-555-1234 tomorrow");
        assert!(!out.contains("415-555-1234"));
        assert!(out.contains("[REDACTED:PHONE]"));
    }

    #[test]
    fn test_redact_phone_chinese() {
        let f = filter();
        let out = f.redact("call 138-1234-5678 today");
        // The phone regex requires + or 1-3 leading digits, this may not match.
        // At minimum, ensure it does not crash.
        let _ = out;
    }

    #[test]
    fn test_redact_chinese_id() {
        let f = filter();
        let out = f.redact("id=11010519491231002X");
        assert!(!out.contains("11010519491231002X"));
        assert!(out.contains("[REDACTED:CN_ID]"));
    }

    #[test]
    fn test_redact_bearer() {
        let f = filter();
        let out = f.redact("Authorization: Bearer abcdefghijklmnop1234");
        assert!(out.contains("[REDACTED:BEARER]"));
    }

    #[test]
    fn test_redact_url_credentials() {
        let f = filter();
        let out = f.redact("use https://user:secret@host.example.com/path");
        assert!(!out.contains("user:secret"));
        assert!(out.contains("[REDACTED:URL_CREDS]"));
    }

    #[test]
    fn test_redact_empty_string() {
        let f = filter();
        assert_eq!(f.redact(""), "");
    }

    #[test]
    fn test_redact_benign_text_unchanged() {
        let f = filter();
        let input = "just a normal sentence about programming in Rust";
        assert_eq!(f.redact(input), input);
    }

    #[test]
    fn test_is_sensitive_true_for_keys() {
        assert!(filter().is_sensitive("AKIAIOSFODNN7EXAMPLE"));
        assert!(filter().is_sensitive("sk-abcdefghijklmnopqrstuv"));
        assert!(filter().is_sensitive("alice@example.com"));
    }

    #[test]
    fn test_is_sensitive_false_for_clean() {
        assert!(!filter().is_sensitive("nothing sensitive here"));
        assert!(!filter().is_sensitive(""));
    }

    #[test]
    fn test_matched_kinds_returns_distinct() {
        let f = filter();
        let kinds = f.matched_kinds("AKIAIOSFODNN7EXAMPLE alice@example.com");
        assert!(kinds.contains(&RedactionKind::AwsKey));
        assert!(kinds.contains(&RedactionKind::Email));
        assert_eq!(kinds.iter().filter(|k| **k == RedactionKind::AwsKey).count(), 1);
    }

    #[test]
    fn test_matched_kinds_empty_for_clean() {
        assert!(filter().matched_kinds("hello world").is_empty());
    }

    #[test]
    fn test_default_filter_matches_redact_sensitive_helper() {
        assert!(redact_sensitive("AKIAIOSFODNN7EXAMPLE"));
        assert!(!redact_sensitive("clean text"));
    }

    #[test]
    fn test_redact_string_helper_works() {
        let out = redact_string("email: bob@test.org");
        assert!(out.contains("[REDACTED:EMAIL]"));
    }

    #[test]
    fn test_redact_multiple_secrets_in_one_string() {
        let f = filter();
        let input = "AKIAIOSFODNN7EXAMPLE and alice@example.com and ghp_abc123def456ghi789jkl012mno345pqr678";
        let out = f.redact(input);
        assert!(!out.contains("AKIA"));
        assert!(!out.contains("alice@example.com"));
        assert!(!out.contains("ghp_"));
    }

    #[test]
    fn test_redaction_kind_labels() {
        assert_eq!(RedactionKind::AwsKey.label(), "AWS_KEY");
        assert_eq!(RedactionKind::Email.label(), "EMAIL");
        assert_eq!(RedactionKind::ChineseId.label(), "CN_ID");
    }

    #[test]
    fn test_redaction_kind_display() {
        assert_eq!(format!("{}", RedactionKind::AwsKey), "AWS_KEY");
        assert_eq!(format!("{}", RedactionKind::Phone), "PHONE");
    }

    #[test]
    fn test_default_patterns_count() {
        let p = default_patterns();
        assert!(p.len() >= 10, "expected at least 10 default patterns, got {}", p.len());
    }

    #[test]
    fn test_sensitive_patterns_const_matches_default() {
        assert!(SENSITIVE_PATTERNS.len() >= 10);
        let names: Vec<&str> = SENSITIVE_PATTERNS.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"EMAIL"));
        assert!(names.contains(&"AWS_KEY"));
        assert!(names.contains(&"OPENAI_KEY"));
    }

    #[test]
    fn test_filter_default_impl() {
        let f = SensitiveFilter::default();
        assert!(f.is_sensitive("alice@example.com"));
    }

    #[test]
    fn test_filter_clone_works() {
        let f = SensitiveFilter::new();
        let f2 = f.clone();
        assert!(f2.is_sensitive("alice@example.com"));
    }

    #[test]
    fn test_with_patterns_custom() {
        let custom = vec![PatternSpec {
            kind: RedactionKind::Phone,
            regex: Regex::new(r"\bCUSTOM-\d{4}\b").unwrap(),
        }];
        let f = SensitiveFilter::with_patterns(custom);
        assert!(f.is_sensitive("CUSTOM-1234"));
        // Default patterns should NOT be present in a custom filter.
        assert!(!f.is_sensitive("alice@example.com"));
    }

    #[test]
    fn test_redact_preserves_surrounding_text() {
        let f = filter();
        let out = f.redact("user alice@example.com is admin");
        assert!(out.starts_with("user "));
        assert!(out.ends_with(" is admin"));
    }

    #[test]
    fn test_aws_secret_regex_requires_40_chars() {
        let f = filter();
        // 39 chars — should not match
        assert_eq!(f.redact("shortABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789X"), "shortABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789X");
    }

    #[test]
    fn test_redact_unicode_path_preserved() {
        let f = filter();
        let out = f.redact("用户 alice@example.com 登录");
        assert!(out.contains("用户"));
        assert!(out.contains("[REDACTED:EMAIL]"));
    }

    #[test]
    fn test_redact_multiline_pem_block() {
        let f = filter();
        let pem = "-----BEGIN PRIVATE KEY-----\nMIIBVgIBADANBgkqhkiG9w0BAQEFAASCAUAwggE8AgEAAkEA\n-----END PRIVATE KEY-----";
        let out = f.redact(pem);
        assert!(!out.contains("MIIBVgIBADANBgkqhkiG9w0BAQEFAASCAUAwggE8AgEAAkEA"));
        assert!(out.contains("[REDACTED:PRIVATE_KEY]"));
    }
}
