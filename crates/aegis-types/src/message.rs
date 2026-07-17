use serde::{Deserialize, Serialize};
use std::fmt;

// ── Roles ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System => write!(f, "system"),
            Self::User => write!(f, "user"),
            Self::Assistant => write!(f, "assistant"),
            Self::Tool => write!(f, "tool"),
        }
    }
}

// ── Content blocks (multi-modal, Anthropic-compatible) ──

/// Media type for binary content (RFC 6838).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaType(pub String);

impl MediaType {
    /// Png.
    pub fn png() -> Self {
        Self("image/png".into())
    }
    /// Jpeg.
    pub fn jpeg() -> Self {
        Self("image/jpeg".into())
    }
    /// Gif.
    pub fn gif() -> Self {
        Self("image/gif".into())
    }
    /// Webp.
    pub fn webp() -> Self {
        Self("image/webp".into())
    }
    /// Pdf.
    pub fn pdf() -> Self {
        Self("application/pdf".into())
    }
    /// Mp3.
    pub fn mp3() -> Self {
        Self("audio/mpeg".into())
    }
    /// Wav.
    pub fn wav() -> Self {
        Self("audio/wav".into())
    }
    /// Mp4.
    pub fn mp4() -> Self {
        Self("video/mp4".into())
    }
    /// Webm.
    pub fn webm() -> Self {
        Self("video/webm".into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        /// Tool result content: can be a plain string or nested multi-modal blocks.
        content: ToolResultContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// Claude extended thinking / reasoning blocks.
    Thinking {
        thinking: String,
    },
    /// Document upload (PDF, plain text, etc.).
    Document {
        source: DocumentSource,
    },
    /// Audio content (for future STT/TTS integration).
    Audio {
        source: AudioSource,
    },
    /// Video content (for future video analysis).
    Video {
        source: VideoSource,
    },
}

/// Source for image content (Anthropic base64 format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String, // "base64"
    pub media_type: String, // "image/png"
    pub data: String,       // base64-encoded
}

/// Source for document content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSource {
    #[serde(rename = "type")]
    pub source_type: String, // "base64"
    pub media_type: String, // "application/pdf"
    pub data: String,       // base64-encoded
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>, // filename
}

/// Source for audio content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioSource {
    #[serde(rename = "type")]
    pub source_type: String, // "base64"
    pub media_type: String, // "audio/mpeg"
    pub data: String,       // base64-encoded
}

/// Source for video content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoSource {
    #[serde(rename = "type")]
    pub source_type: String, // "base64"
    pub media_type: String, // "video/mp4"
    pub data: String,       // base64-encoded
}

/// Tool result content: plain text or multi-modal blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl ToolResultContent {
    /// Extracts the text content from this message.
    pub fn text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

impl From<String> for ToolResultContent {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

impl From<&str> for ToolResultContent {
    fn from(s: &str) -> Self {
        Self::Text(s.to_string())
    }
}

/// Content can be a plain string or an array of blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl Content {
    /// Extract the concatenated text from content.
    pub fn text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    ContentBlock::ToolResult { content, .. } => Some(match content {
                        ToolResultContent::Text(s) => s.as_str(),
                        ToolResultContent::Blocks(_) => "[multi-modal result]",
                    }),
                    ContentBlock::Thinking { thinking } => Some(thinking.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }

    /// Check if content contains any non-text blocks (images, audio, etc.).
    pub fn has_multimodal(&self) -> bool {
        match self {
            Self::Text(_) => false,
            Self::Blocks(blocks) => blocks.iter().any(|b| {
                !matches!(
                    b,
                    ContentBlock::Text { .. }
                        | ContentBlock::ToolUse { .. }
                        | ContentBlock::ToolResult { .. }
                )
            }),
        }
    }
}

impl fmt::Display for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.text())
    }
}

// ── Tool calls ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String, // JSON string
}

// ── Message ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

impl Message {
    /// Creates a system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(Content::Text(content.into())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    /// Creates a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(Content::Text(content.into())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    /// Creates an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(Content::Text(content.into())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    /// Creates a tool result message.
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(Content::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: call_id.into(),
                content: ToolResultContent::Text(content.into()),
                is_error: None,
            }])),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    /// Construct a tool result containing multimodal content blocks.
    pub fn tool_result_blocks(call_id: impl Into<String>, blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(Content::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: call_id.into(),
                content: ToolResultContent::Blocks(blocks),
                is_error: None,
            }])),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    /// Build an assistant message carrying tool calls (no text content).
    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(calls),
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    /// Get the plain-text content, empty string if absent.
    pub fn text(&self) -> String {
        match &self.content {
            Some(c) => c.text(),
            None => String::new(),
        }
    }

    /// Returns whether this message contains tool calls.
    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty())
    }

    /// The tool_call id a tool-result message responds to. Prefers the
    /// message-level `tool_call_id`; otherwise reads the embedded `ToolResult`
    /// content block's `tool_use_id`. Providers need this to pair tool results
    /// with the assistant's tool call.
    pub fn tool_result_id(&self) -> Option<String> {
        if let Some(id) = &self.tool_call_id {
            return Some(id.clone());
        }
        if let Some(Content::Blocks(blocks)) = &self.content {
            for b in blocks {
                if let ContentBlock::ToolResult { tool_use_id, .. } = b {
                    return Some(tool_use_id.clone());
                }
            }
        }
        None
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.role, self.text())
    }
}

// ── Usage / LlmResponse ──

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
    pub reasoning_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub message: Message,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_constructors() {
        let m = Message::system("hello");
        assert_eq!(m.role, Role::System);
        assert_eq!(m.text(), "hello");

        let m = Message::user("input");
        assert_eq!(m.role, Role::User);

        let m = Message::assistant("reply");
        assert_eq!(m.role, Role::Assistant);

        let m = Message::tool_result("call-1", "result");
        assert_eq!(m.role, Role::Tool);
    }

    #[test]
    fn test_message_display() {
        let m = Message::user("hello world");
        let s = format!("{m}");
        assert!(s.contains("[user]"));
        assert!(s.contains("hello world"));
    }

    #[test]
    fn test_has_tool_calls() {
        let m = Message::assistant("no tools");
        assert!(!m.has_tool_calls());

        let m = Message::assistant_tool_calls(vec![ToolCall {
            id: "1".into(),
            name: "test".into(),
            arguments: "{}".into(),
        }]);
        assert!(m.has_tool_calls());
    }

    #[test]
    fn test_content_text_from_blocks() {
        let c = Content::Blocks(vec![
            ContentBlock::Text {
                text: "hello ".into(),
            },
            ContentBlock::Text {
                text: "world".into(),
            },
        ]);
        assert_eq!(c.text(), "hello world");
    }

    #[test]
    fn test_role_display() {
        assert_eq!(format!("{}", Role::System), "system");
        assert_eq!(format!("{}", Role::User), "user");
        assert_eq!(format!("{}", Role::Assistant), "assistant");
        assert_eq!(format!("{}", Role::Tool), "tool");
    }

    #[test]
    fn test_message_serialize_json() {
        let m = Message::user("test");
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"role\":\"user\""));
    }

    #[test]
    fn test_empty_message_text() {
        let m = Message {
            role: Role::Assistant,
            content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        };
        assert_eq!(m.text(), "");
    }

    #[test]
    fn test_thinking_block() {
        let c = Content::Blocks(vec![
            ContentBlock::Thinking {
                thinking: "reasoning...".into(),
            },
            ContentBlock::Text {
                text: "answer".into(),
            },
        ]);
        assert_eq!(c.text(), "reasoning...answer");
    }

    #[test]
    fn test_tool_result_content_variants() {
        let plain = ToolResultContent::from("hello");
        assert_eq!(plain.text(), "hello");

        let multi = ToolResultContent::Blocks(vec![
            ContentBlock::Text {
                text: "see ".into(),
            },
            ContentBlock::Image {
                source: ImageSource {
                    source_type: "base64".into(),
                    media_type: "image/png".into(),
                    data: "abc123".into(),
                },
            },
        ]);
        // Only text should be extracted
        assert_eq!(multi.text(), "see ");
    }

    #[test]
    fn test_content_multimodal_detection() {
        let text_only = Content::Blocks(vec![ContentBlock::Text {
            text: "hello".into(),
        }]);
        assert!(!text_only.has_multimodal());

        let with_image = Content::Blocks(vec![
            ContentBlock::Text {
                text: "look: ".into(),
            },
            ContentBlock::Image {
                source: ImageSource {
                    source_type: "base64".into(),
                    media_type: "image/png".into(),
                    data: "abc".into(),
                },
            },
        ]);
        assert!(with_image.has_multimodal());
    }

    #[test]
    fn test_document_block() {
        let c = Content::Blocks(vec![ContentBlock::Document {
            source: DocumentSource {
                source_type: "base64".into(),
                media_type: "application/pdf".into(),
                data: "pdfdata".into(),
                name: Some("report.pdf".into()),
            },
        }]);
        assert!(c.has_multimodal()); // Document is multimodal
    }

    #[test]
    fn test_tool_result_with_error() {
        let m = Message::tool_result("call-1", "error: file not found");
        match &m.content {
            Some(Content::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    ContentBlock::ToolResult { is_error, .. } => {
                        // is_error defaults to None
                        assert!(is_error.is_none());
                    }
                    _ => panic!("Expected ToolResult block"),
                }
            }
            _ => panic!("Expected Blocks content"),
        }
    }

    #[test]
    fn test_media_type_constructors() {
        assert_eq!(MediaType::png().0, "image/png");
        assert_eq!(MediaType::jpeg().0, "image/jpeg");
        assert_eq!(MediaType::gif().0, "image/gif");
        assert_eq!(MediaType::webp().0, "image/webp");
        assert_eq!(MediaType::pdf().0, "application/pdf");
        assert_eq!(MediaType::mp3().0, "audio/mpeg");
        assert_eq!(MediaType::wav().0, "audio/wav");
        assert_eq!(MediaType::mp4().0, "video/mp4");
        assert_eq!(MediaType::webm().0, "video/webm");
    }

    #[test]
    fn test_content_from_string() {
        let c = Content::Text("hello".to_string());
        assert_eq!(c.text(), "hello");
        match c {
            Content::Text(s) => assert_eq!(s, "hello"),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn test_message_assistant_tool_calls() {
        let calls = vec![ToolCall {
            id: "call_1".into(),
            name: "terminal".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        }];
        let m = Message::assistant_tool_calls(calls);
        assert_eq!(m.role, Role::Assistant);
        assert!(m.content.is_none());
        assert!(m.has_tool_calls());
        let tc = m.tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].name, "terminal");
    }

    #[test]
    fn test_message_serde_roundtrip() {
        let m = Message::system("you are helpful");
        let json = serde_json::to_string(&m).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.role, Role::System);
        assert_eq!(deserialized.text(), "you are helpful");
    }

    #[test]
    fn test_content_has_multimodal_audio() {
        let c = Content::Blocks(vec![ContentBlock::Audio {
            source: AudioSource {
                source_type: "base64".into(),
                media_type: "audio/mp3".into(),
                data: "audiodata".into(),
            },
        }]);
        assert!(c.has_multimodal());
    }

    #[test]
    fn test_content_has_multimodal_video() {
        let c = Content::Blocks(vec![ContentBlock::Video {
            source: VideoSource {
                source_type: "base64".into(),
                media_type: "video/mp4".into(),
                data: "videodata".into(),
            },
        }]);
        assert!(c.has_multimodal());
    }

    #[test]
    fn test_tool_call_serde_roundtrip() {
        let tc = ToolCall {
            id: "call_abc".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"/tmp/test.rs"}"#.into(),
        };
        let json = serde_json::to_string(&tc).unwrap();
        let deserialized: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "call_abc");
        assert_eq!(deserialized.name, "read_file");
    }

    #[test]
    fn test_role_serde() {
        let json = serde_json::to_string(&Role::User).unwrap();
        assert_eq!(json, "\"user\"");
        let role: Role = serde_json::from_str("\"assistant\"").unwrap();
        assert_eq!(role, Role::Assistant);
    }
}
