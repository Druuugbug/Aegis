use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A2A Protocol v0.2.5 core types

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub url: String,
    pub version: String,
    pub protocol_version: String,
    pub capabilities: AgentCapabilities,
    pub skills: Vec<AgentSkill>,
    pub security_schemes: Vec<SecurityScheme>,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapabilities {
    pub streaming: bool,
    pub push_notifications: bool,
    pub state_transition_history: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityScheme {
    /// OpenAPI scheme type: "http" | "apiKey" | "oauth2" | "openIdConnect".
    #[serde(rename = "type")]
    pub scheme_type: String,
    /// For http: the scheme name, e.g. "bearer".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub status: TaskStatusInfo,
    /// Conversation history (A2A wire field is `history`; kept as `messages`
    /// internally for back-compat).
    #[serde(rename = "history")]
    pub messages: Vec<Message>,
    pub artifacts: Vec<Artifact>,
    #[serde(default = "task_kind")]
    pub kind: String,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

fn task_kind() -> String {
    "task".to_string()
}

fn msg_kind() -> String {
    "message".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusInfo {
    pub state: TaskState,
    pub message: Option<Message>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Submitted,
    Working,
    /// The agent needs more input from the user to proceed (A2A spec).
    #[serde(rename = "input-required")]
    InputRequired,
    /// The agent needs authentication to proceed (A2A spec).
    #[serde(rename = "auth-required")]
    AuthRequired,
    Completed,
    Failed,
    Canceled,
    Rejected,
    /// Unknown / unrecognised state (A2A spec catch-all).
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub role: MessageRole,
    pub parts: Vec<Part>,
    #[serde(default = "msg_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Part {
    Text { text: String },
    File { file: FileContent },
    Data { data: serde_json::Value },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    pub name: Option<String>,
    pub mime_type: Option<String>,
    pub bytes: Option<String>,
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub artifact_id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub parts: Vec<Part>,
    pub index: u32,
    pub append: Option<bool>,
    pub last_chunk: Option<bool>,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSendParams {
    pub id: Option<String>,
    /// A2A `message/send` sends a single `message`; we also accept the legacy
    /// `messages` array. `on_send` normalises: if `messages` is empty and
    /// `message` is present, the single message is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default)]
    pub messages: Vec<Message>,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskGetParams {
    pub id: String,
    pub history_length: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCancelParams {
    pub id: String,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TaskEvent {
    StatusUpdate(TaskStatusUpdateEvent),
    ArtifactUpdate(TaskArtifactUpdateEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusUpdateEvent {
    /// A2A wire field is `taskId`.
    #[serde(rename = "taskId")]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub status: TaskStatusInfo,
    /// A2A wire field is `final`.
    #[serde(rename = "final")]
    pub final_event: bool,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskArtifactUpdateEvent {
    /// A2A wire field is `taskId`.
    #[serde(rename = "taskId")]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub artifact: Artifact,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

impl JsonRpcResponse {
    /// Creates a successful response with the given result.
    pub fn ok(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }
    /// Creates an error response with the given code and message.
    pub fn err(id: Option<serde_json::Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jsonrpc_response_ok() {
        let resp = JsonRpcResponse::ok(
            Some(serde_json::json!(1)),
            serde_json::json!({"status": "ok"}),
        );
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        assert_eq!(resp.id, Some(serde_json::json!(1)));
    }

    #[test]
    fn test_jsonrpc_response_err() {
        let resp = JsonRpcResponse::err(Some(serde_json::json!(42)), -32601, "Method not found");
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.result.is_none());
        let err = resp.error.as_ref().unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
        assert!(err.data.is_none());
    }

    #[test]
    fn test_jsonrpc_response_serde_roundtrip() {
        let original = JsonRpcResponse::ok(
            Some(serde_json::json!(1)),
            serde_json::json!({"hello": "world"}),
        );
        let json = serde_json::to_string(&original).unwrap();
        let decoded: JsonRpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.result.unwrap(),
            serde_json::json!({"hello": "world"})
        );
    }

    #[test]
    fn test_task_state_serde_roundtrip() {
        let states = vec![
            TaskState::Submitted,
            TaskState::Working,
            TaskState::InputRequired,
            TaskState::AuthRequired,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::Rejected,
            TaskState::Unknown,
        ];
        for state in states {
            let json = serde_json::to_string(&state).unwrap();
            let decoded: TaskState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, decoded);
        }
    }

    #[test]
    fn test_task_state_serialized_form() {
        assert_eq!(
            serde_json::to_string(&TaskState::Submitted).unwrap(),
            "\"submitted\""
        );
        assert_eq!(
            serde_json::to_string(&TaskState::Working).unwrap(),
            "\"working\""
        );
        assert_eq!(
            serde_json::to_string(&TaskState::Completed).unwrap(),
            "\"completed\""
        );
        assert_eq!(
            serde_json::to_string(&TaskState::InputRequired).unwrap(),
            "\"input-required\""
        );
        assert_eq!(
            serde_json::to_string(&TaskState::AuthRequired).unwrap(),
            "\"auth-required\""
        );
    }

    #[test]
    fn test_part_uses_kind_discriminator() {
        let p = Part::Text { text: "hi".into() };
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            json.contains("\"kind\""),
            "A2A Part must use `kind`: {json}"
        );
        assert!(json.contains("\"text\""));
        // round-trips
        let back: Part = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Part::Text { .. }));
    }

    #[test]
    fn test_task_wire_shape_history_and_kind() {
        let now = chrono::Utc::now();
        let task = Task {
            id: "t1".into(),
            context_id: Some("c1".into()),
            status: TaskStatusInfo {
                state: TaskState::Working,
                message: None,
                timestamp: now,
            },
            messages: vec![Message {
                role: MessageRole::User,
                parts: vec![Part::Text { text: "hi".into() }],
                kind: "message".into(),
                message_id: Some("m1".into()),
                context_id: None,
                task_id: None,
                metadata: None,
            }],
            artifacts: vec![],
            kind: "task".into(),
            metadata: None,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&task).unwrap();
        assert!(
            json.contains("\"history\""),
            "Task must use `history`: {json}"
        );
        assert!(json.contains("\"contextId\":\"c1\""));
        assert!(json.contains("\"messageId\":\"m1\""));
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(back.messages.len(), 1);
        assert_eq!(back.context_id.as_deref(), Some("c1"));
    }

    #[test]
    fn test_task_event_wire_shape() {
        let now = chrono::Utc::now();
        let ev = TaskEvent::StatusUpdate(TaskStatusUpdateEvent {
            id: "t1".into(),
            context_id: Some("c1".into()),
            status: TaskStatusInfo {
                state: TaskState::Working,
                message: None,
                timestamp: now,
            },
            final_event: false,
            metadata: None,
        });
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"kind\":\"status-update\""), "{json}");
        assert!(json.contains("\"taskId\":\"t1\""));
        assert!(json.contains("\"final\":false"));
        assert!(json.contains("\"contextId\":\"c1\""));
    }

    #[test]
    fn test_agent_card_serde_roundtrip() {
        let card = AgentCard {
            name: "test-agent".into(),
            description: "A test agent".into(),
            url: "https://example.com".into(),
            version: "1.0.0".into(),
            protocol_version: "0.2.5".into(),
            capabilities: AgentCapabilities {
                streaming: true,
                push_notifications: false,
                state_transition_history: true,
            },
            skills: vec![AgentSkill {
                id: "summarize".into(),
                name: "Summarize".into(),
                description: "Summarizes text".into(),
                tags: vec!["nlp".into()],
            }],
            security_schemes: vec![SecurityScheme {
                scheme_type: "http".into(),
                scheme: Some("bearer".into()),
                description: Some("Bearer token auth".into()),
            }],
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
        };
        let json = serde_json::to_string(&card).unwrap();
        let decoded: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.name, "test-agent");
        assert_eq!(decoded.skills.len(), 1);
        assert!(decoded.capabilities.streaming);
    }

    #[test]
    fn test_agent_card_camel_case_json() {
        let card = AgentCard {
            name: "a".into(),
            description: "b".into(),
            url: "c".into(),
            version: "1".into(),
            protocol_version: "0.2.5".into(),
            capabilities: AgentCapabilities {
                streaming: false,
                push_notifications: false,
                state_transition_history: false,
            },
            skills: vec![],
            security_schemes: vec![],
            default_input_modes: vec![],
            default_output_modes: vec![],
        };
        let json = serde_json::to_string(&card).unwrap();
        assert!(
            json.contains("protocolVersion"),
            "should use camelCase: {json}"
        );
        assert!(
            json.contains("defaultInputModes"),
            "should use camelCase: {json}"
        );
    }
}
