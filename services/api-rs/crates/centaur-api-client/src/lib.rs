//! HTTP client for the api-rs session API, built on `centaur-api-types` so
//! callers do not depend on the server.

use std::pin::Pin;

use centaur_session_core::ThreadKey;
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use reqwest::{
    Client as HttpClient, StatusCode,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use thiserror::Error;

use centaur_api_types::{
    AppendMessagesRequest, AppendMessagesResponse, CreateSessionRequest, CreateSessionResponse,
    EventsQuery, ExecuteSessionRequest, ExecuteSessionResponse, InterruptSessionExecutionRequest,
    InterruptSessionExecutionResponse, ListSessionsQuery, ListSessionsResponse,
    PauseSessionResponse, SessionSummary,
};

#[derive(Clone, Debug)]
pub struct CentaurClient {
    client: HttpClient,
    base_url: String,
}

impl CentaurClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_client(HttpClient::new(), base_url)
    }

    pub fn with_bearer_token(
        base_url: impl Into<String>,
        token: &str,
    ) -> Result<Self, ClientError> {
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| ClientError::InvalidBearerToken)?;
        value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, value);
        let client = HttpClient::builder().default_headers(headers).build()?;
        Ok(Self::with_client(client, base_url))
    }

    pub fn with_client(client: HttpClient, base_url: impl Into<String>) -> Self {
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    pub async fn create_session(
        &self,
        thread_key: &ThreadKey,
        request: CreateSessionRequest,
    ) -> Result<CreateSessionResponse, ClientError> {
        self.post_json(&self.session_url(thread_key), &request)
            .await
    }

    pub async fn append_messages(
        &self,
        thread_key: &ThreadKey,
        request: AppendMessagesRequest,
    ) -> Result<AppendMessagesResponse, ClientError> {
        self.post_json(
            &format!("{}/messages", self.session_url(thread_key)),
            &request,
        )
        .await
    }

    pub async fn execute_session(
        &self,
        thread_key: &ThreadKey,
        request: ExecuteSessionRequest,
    ) -> Result<ExecuteSessionResponse, ClientError> {
        self.post_json(
            &format!("{}/execute", self.session_url(thread_key)),
            &request,
        )
        .await
    }

    /// Creates a session under a thread key the server generates; the key is
    /// in the response.
    pub async fn create_session_with_generated_key(
        &self,
        request: CreateSessionRequest,
    ) -> Result<CreateSessionResponse, ClientError> {
        self.post_json(&format!("{}/api/sessions", self.base_url), &request)
            .await
    }

    /// Sessions most recently updated first; see `GET /api/sessions`.
    pub async fn list_sessions(
        &self,
        query: &ListSessionsQuery,
    ) -> Result<ListSessionsResponse, ClientError> {
        self.get_json(&format!("{}/api/sessions", self.base_url), query)
            .await
    }

    pub async fn session_summary(
        &self,
        thread_key: &ThreadKey,
    ) -> Result<SessionSummary, ClientError> {
        self.get_json(&format!("{}/summary", self.session_url(thread_key)), &())
            .await
    }

    pub async fn interrupt_session(
        &self,
        thread_key: &ThreadKey,
        request: InterruptSessionExecutionRequest,
    ) -> Result<InterruptSessionExecutionResponse, ClientError> {
        self.post_json(
            &format!("{}/interrupt", self.session_url(thread_key)),
            &request,
        )
        .await
    }

    pub async fn pause_session(
        &self,
        thread_key: &ThreadKey,
    ) -> Result<PauseSessionResponse, ClientError> {
        self.post_json(
            &format!("{}/pause", self.session_url(thread_key)),
            &serde_json::json!({}),
        )
        .await
    }

    /// Replays the thread's events after `after_event_id`, then follows new
    /// ones. `execution_id` limits the stream to one execution's events.
    pub async fn stream_events(
        &self,
        thread_key: &ThreadKey,
        after_event_id: i64,
        execution_id: Option<&str>,
    ) -> Result<SseEventStream, ClientError> {
        let query = EventsQuery {
            after_event_id: Some(after_event_id),
            execution_id: execution_id.map(str::to_owned),
        };
        let response = self
            .client
            .get(format!("{}/events", self.session_url(thread_key)))
            .query(&query)
            .send()
            .await?;
        let response = ensure_response_success(response).await?;
        let stream = response
            .bytes_stream()
            .eventsource()
            .map(|event| event.map_err(|error| ClientError::EventStream(error.to_string())));
        Ok(Box::pin(stream))
    }

    async fn get_json<Q, R>(&self, url: &str, query: &Q) -> Result<R, ClientError>
    where
        Q: serde::Serialize + ?Sized,
        R: serde::de::DeserializeOwned,
    {
        let response = self.client.get(url).query(query).send().await?;
        let response = ensure_response_success(response).await?;
        Ok(response.json().await?)
    }

    async fn post_json<T, R>(&self, url: &str, payload: &T) -> Result<R, ClientError>
    where
        T: serde::Serialize + ?Sized,
        R: serde::de::DeserializeOwned,
    {
        let response = self.client.post(url).json(payload).send().await?;
        let response = ensure_response_success(response).await?;
        Ok(response.json().await?)
    }

    fn session_url(&self, thread_key: &ThreadKey) -> String {
        format!(
            "{}/api/session/{}",
            self.base_url,
            urlencoding::encode(thread_key.as_str())
        )
    }
}

pub type SseEventStream = Pin<Box<dyn Stream<Item = Result<SseEvent, ClientError>> + Send>>;
pub type SseEvent = eventsource_stream::Event;

async fn ensure_response_success(
    response: reqwest::Response,
) -> Result<reqwest::Response, ClientError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await?;
    Err(ClientError::Api { status, body })
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("HTTP {status}: {body}")]
    Api { status: StatusCode, body: String },
    #[error("event stream parse failed: {0}")]
    EventStream(String),
    #[error("bearer token cannot be represented as an HTTP header")]
    InvalidBearerToken,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query_string(query: &impl serde::Serialize) -> Option<String> {
        HttpClient::new()
            .get("http://api.test/")
            .query(query)
            .build()
            .unwrap()
            .url()
            .query()
            .map(str::to_owned)
    }

    /// Unset optional fields are left out, and values are percent-encoded.
    #[test]
    fn query_types_serialize_to_query_strings() {
        assert_eq!(query_string(&ListSessionsQuery::default()), None);
        assert_eq!(
            query_string(&ListSessionsQuery {
                limit: Some(5),
                thread_key_prefix: Some("slack:C1&x".to_owned()),
            })
            .as_deref(),
            Some("limit=5&thread_key_prefix=slack%3AC1%26x")
        );
        assert_eq!(
            query_string(&EventsQuery {
                after_event_id: Some(0),
                execution_id: None,
            })
            .as_deref(),
            Some("after_event_id=0")
        );
    }
}
