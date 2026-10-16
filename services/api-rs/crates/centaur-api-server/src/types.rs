use axum::response::sse::Event;
pub use centaur_api_types::*;
use centaur_session_core::SessionEvent;
use thiserror::Error;

pub struct SessionSseEvent(Event);

impl TryFrom<SessionEvent> for SessionSseEvent {
    type Error = SessionEventConversionError;

    fn try_from(event: SessionEvent) -> Result<Self, Self::Error> {
        let event_id = event.event_id;
        let event_name = SessionEventName::from(event.event_type);
        let sse = Event::default()
            .id(event_id.to_string())
            .event(event_name.as_str());

        let sse = match event_name {
            SessionEventName::OutputLine => {
                let Some(line) = event.payload.as_str() else {
                    return Err(SessionEventConversionError::OutputLinePayload { event_id });
                };
                sse.data(line)
            }
            _ => sse
                .json_data(event.payload)
                .map_err(|source| SessionEventConversionError::JsonData { event_id, source })?,
        };

        Ok(Self(sse))
    }
}

impl From<SessionSseEvent> for Event {
    fn from(value: SessionSseEvent) -> Self {
        value.0
    }
}

pub fn stream_error_sse(message: impl Into<String>) -> Event {
    Event::default()
        .event(SessionEventName::StreamError.as_str())
        .json_data(serde_json::json!({ "error": message.into() }))
        .unwrap_or_else(|_| {
            Event::default()
                .event(SessionEventName::StreamError.as_str())
                .data("{}")
        })
}

#[derive(Debug, Error)]
pub enum SessionEventConversionError {
    #[error("session.output.line event {event_id} payload must be a string")]
    OutputLinePayload { event_id: i64 },
    #[error("failed to serialize session event {event_id} payload as SSE JSON: {source}")]
    JsonData { event_id: i64, source: axum::Error },
}
