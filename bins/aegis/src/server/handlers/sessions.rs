use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::server::error::ApiError;
use crate::server::state::AppState;

use aegis_core::config;
use aegis_record::SessionStore;

/// Opens the default session store (per-request, since Connection isn't Sync).
fn open_store() -> Result<SessionStore, ApiError> {
    let db_dir = config::config_dir();
    std::fs::create_dir_all(&db_dir).map_err(|e| ApiError::Internal(e.to_string()))?;
    SessionStore::open(&db_dir.join("sessions.db"))
        .map_err(|e| ApiError::Internal(e.to_string()))
}

/// Response body for a list of sessions.
#[derive(Serialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionSummary>,
}

/// Summary of a single session.
#[derive(Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub started_at: String,
    pub message_count: i64,
}

/// Response body for session detail (messages).
#[derive(Serialize)]
pub struct SessionDetailResponse {
    pub id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub started_at: String,
    pub messages: Vec<MessageInfo>,
}

/// A single message within a session.
#[derive(Serialize)]
pub struct MessageInfo {
    pub role: String,
    pub content: Option<String>,
    pub tool_name: Option<String>,
    pub record_type: String,
}

/// Query parameters for listing sessions.
#[derive(Deserialize)]
pub struct ListSessionsQuery {
    pub limit: Option<u32>,
}

/// Lists recent sessions from the store.
pub async fn list_sessions(
    State(_state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<ListSessionsQuery>,
) -> Result<Json<SessionListResponse>, ApiError> {
    let store = open_store()?;
    let limit = query.limit.unwrap_or(50);
    let rows = store.list_sessions(limit)?;
    let sessions = rows.into_iter().map(|s| SessionSummary {
        id: s.id,
        title: s.title,
        model: s.model,
        started_at: s.started_at,
        message_count: s.message_count,
    }).collect();
    Ok(Json(SessionListResponse { sessions }))
}

/// Gets session detail including messages.
pub async fn get_session(
    State(_state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<SessionDetailResponse>, ApiError> {
    let store = open_store()?;
    let all = store.list_sessions(1000)?;
    let session = all.into_iter().find(|s| s.id == session_id)
        .ok_or_else(|| ApiError::NotFound(format!("session {session_id}")))?;
    let message_rows = store.get_messages(&session_id)?;
    let messages = message_rows.into_iter().map(|m| MessageInfo {
        role: m.role,
        content: m.content,
        tool_name: m.tool_name,
        record_type: m.record_type,
    }).collect();
    Ok(Json(SessionDetailResponse {
        id: session.id,
        title: session.title,
        model: session.model,
        started_at: session.started_at,
        messages,
    }))
}

/// Exports a session as JSON.
pub async fn export_session(
    State(_state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let store = open_store()?;
    let exported = store.export_session(&session_id)?;
    Ok(Json(exported))
}

/// Deletes (ends) a session.
pub async fn delete_session(
    State(_state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let store = open_store()?;
    store.end_session(&session_id)?;
    Ok(Json(serde_json::json!({"deleted": session_id})))
}

/// Searches messages across all sessions.
#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub limit: Option<u32>,
}

/// Searches sessions and returns matching results.
pub async fn search_sessions(
    State(_state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<SearchQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let store = open_store()?;
    let limit = query.limit.unwrap_or(20);
    let results = store.search(&query.q, limit)?;
    Ok(Json(serde_json::json!({
        "query": query.q,
        "results": results.iter().map(|r| serde_json::json!({
            "session_id": r.session_id,
            "role": r.role,
            "snippet": r.snippet,
        })).collect::<Vec<_>>(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_summary_serde_roundtrip() {
        let s = SessionSummary {
            id: "s1".into(),
            title: Some("Test".into()),
            model: Some("gpt-4o".into()),
            started_at: "2025-01-01".into(),
            message_count: 5,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"id\":\"s1\""));
        assert!(json.contains("\"message_count\":5"));
    }

    #[test]
    fn message_info_serde() {
        let m = MessageInfo {
            role: "assistant".into(),
            content: Some("hello".into()),
            tool_name: None,
            record_type: "message".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"role\":\"assistant\""));
        assert!(json.contains("null"));
    }

    #[test]
    fn session_list_response_serde() {
        let resp = SessionListResponse { sessions: vec![] };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, "{\"sessions\":[]}");
    }

    #[test]
    fn search_query_deserialize() {
        let q: SearchQuery = serde_json::from_str(r#"{"q":"test","limit":10}"#).unwrap();
        assert_eq!(q.q, "test");
        assert_eq!(q.limit, Some(10));
    }

    #[test]
    fn search_query_default_limit() {
        let q: SearchQuery = serde_json::from_str(r#"{"q":"test"}"#).unwrap();
        assert_eq!(q.limit, None);
    }
}
