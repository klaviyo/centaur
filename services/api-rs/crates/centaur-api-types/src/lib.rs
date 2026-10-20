//! Wire types for the api-rs HTTP API, shared by the server and its clients.
//!
//! Keep this crate free of server dependencies (axum, sqlx, the runtime) so
//! clients such as the `centaur` CLI can depend on it cheaply.

use centaur_session_core::{
    ExecutionStatus, HarnessType, SESSION_OUTPUT_LINE_EVENT, Session, SessionMessageInput,
    SessionStatus, ThreadKey,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CreateSessionRequest {
    pub harness_type: HarnessType,
    /// Used only when creating the session. The first persisted persona stays
    /// pinned for the lifetime of the thread. An ID absent from the deployment
    /// falls back to the eligible deployment default or no persona.
    pub persona_id: Option<String>,
    pub metadata: Option<Value>,
    /// What to do when the session already exists on a different harness.
    /// Omitted or `reject`: fail with 409. `restart`: stop the old sandbox and
    /// restart the thread on the requested harness (the new harness starts
    /// with no conversational memory).
    #[serde(default)]
    pub on_harness_conflict: Option<OnHarnessConflict>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OnHarnessConflict {
    Reject,
    Restart,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CreateSessionResponse {
    #[serde(flatten)]
    pub session: Session,
    /// True when this request restarted the thread onto a different harness.
    pub harness_switched: bool,
    /// Present only when a new-session request named an unavailable persona
    /// and the returned session uses the resolved fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_requested_persona_id: Option<String>,
    /// Model the session's persona pins for its harness. Every turn runs on
    /// it; requested models and providers are ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persona_model: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionContextResponse {
    pub thread_key: ThreadKey,
    /// The chat surface the agent is operating on: `slack`, `discord`, `linear`,
    /// `github`, or `unknown` for threads that are not platform-addressable.
    pub platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackThreadContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordThreadContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linear: Option<LinearThreadContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github: Option<GithubThreadContext>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SlackThreadContext {
    pub channel_id: String,
    pub thread_ts: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiscordThreadContext {
    pub guild_id: String,
    pub channel_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LinearThreadContext {
    pub issue_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GithubThreadContext {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    /// `issue` or `pr`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_comment_id: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AppendMessagesRequest {
    pub messages: Vec<SessionMessageInput>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AppendMessagesResponse {
    pub ok: bool,
    pub message_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecuteSessionRequest {
    pub idempotency_key: Option<String>,
    pub metadata: Option<Value>,
    #[serde(default)]
    pub input_lines: Vec<String>,
    pub idle_timeout_ms: Option<u64>,
    pub max_duration_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecuteSessionResponse {
    pub ok: bool,
    pub execution_id: String,
    pub thread_key: ThreadKey,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InterruptSessionExecutionRequest {
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InterruptSessionExecutionResponse {
    pub ok: bool,
    pub interrupted: bool,
    pub execution_id: Option<String>,
    pub thread_key: ThreadKey,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventsQuery {
    pub after_event_id: Option<i64>,
    pub execution_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ListWorkflowRunsQuery {
    pub limit: Option<i64>,
    pub workflow_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EmitWorkflowEventRequest {
    pub event_name: Option<String>,
    pub event_type: Option<String>,
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEventName {
    OutputLine,
    ExecutionStarted,
    ExecutionCompleted,
    ExecutionFailed,
    ExecutionCancelled,
    StreamError,
    Other(String),
}

impl SessionEventName {
    pub fn as_str(&self) -> &str {
        match self {
            Self::OutputLine => SESSION_OUTPUT_LINE_EVENT,
            Self::ExecutionStarted => "session.execution_started",
            Self::ExecutionCompleted => "session.execution_completed",
            Self::ExecutionFailed => "session.execution_failed",
            Self::ExecutionCancelled => "session.execution_cancelled",
            Self::StreamError => "session.stream_error",
            Self::Other(value) => value.as_str(),
        }
    }
}

impl From<String> for SessionEventName {
    fn from(value: String) -> Self {
        match value.as_str() {
            SESSION_OUTPUT_LINE_EVENT => Self::OutputLine,
            "session.execution_started" => Self::ExecutionStarted,
            "session.execution_completed" => Self::ExecutionCompleted,
            "session.execution_failed" => Self::ExecutionFailed,
            "session.execution_cancelled" => Self::ExecutionCancelled,
            "session.stream_error" => Self::StreamError,
            _ => Self::Other(value),
        }
    }
}

impl From<&str> for SessionEventName {
    fn from(value: &str) -> Self {
        Self::from(value.to_owned())
    }
}

/// `GET /api/auth/whoami`. Class and capability names are strings rather than
/// enums so a client keeps working when the server adds new ones.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WhoamiResponse {
    pub class: String,
    /// The principal ID ownership checks compare against, when the caller has
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// A console admin's user token, which reaches every session.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub console_admin: bool,
}

/// `POST /api/session/{thread_key}/pause`. `paused` is false when the session
/// had no idle sandbox to pause.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PauseSessionResponse {
    pub ok: bool,
    pub paused: bool,
}

/// `GET /api/sessions` query. The server clamps `limit` to its maximum.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ListSessionsQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Only sessions whose thread key starts with this, e.g. `slack:` or `cli:`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_key_prefix: Option<String>,
}

/// `GET /api/sessions`: most recently updated first. Admin and Console callers
/// see every session; user callers see the sessions bound to their principal.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionSummary>,
}

/// One session as `GET /api/sessions` lists it, and the body of
/// `GET /api/session/{thread_key}/summary`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionSummary {
    pub thread_key: ThreadKey,
    /// `slack`, `discord`, `linear`, `github`, or `unknown`.
    pub platform: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub harness_type: HarnessType,
    pub status: SessionStatus,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_execution: Option<ExecutionSummary>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionSummary {
    pub execution_id: String,
    pub status: ExecutionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub started_at: Option<OffsetDateTime>,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub completed_at: Option<OffsetDateTime>,
}

/// `GET /.well-known/oauth-protected-resource` (RFC 9728).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProtectedResourceMetadata {
    pub resource: String,
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    #[serde(default)]
    pub bearer_methods_supported: Vec<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn whoami_tolerates_missing_optionals_and_new_fields() {
        let whoami: WhoamiResponse = serde_json::from_value(json!({
            "class": "admin",
            "capabilities": ["sessions_read", "capability_from_the_future"],
            "field_from_the_future": true,
        }))
        .unwrap();
        assert_eq!(whoami.class, "admin");
        assert_eq!(whoami.principal, None);
        assert_eq!(whoami.capabilities.len(), 2);
        assert!(
            serde_json::to_value(&whoami)
                .unwrap()
                .get("principal")
                .is_none()
        );
    }

    #[test]
    fn protected_resource_metadata_defaults_missing_lists() {
        let metadata: ProtectedResourceMetadata =
            serde_json::from_value(json!({"resource": "https://api.example.com/mcp"})).unwrap();
        assert!(metadata.authorization_servers.is_empty());
        assert!(metadata.scopes_supported.is_empty());
    }
}
