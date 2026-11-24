use axum::{
    Json,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use centaur_session_core::ThreadKeyError;
use centaur_session_runtime::SessionRuntimeError;
use centaur_session_sqlx::SessionStoreError;
use centaur_workflows::WorkflowRuntimeError;
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    MethodNotAllowed(String),
    #[error("{0}")]
    PayloadTooLarge(String),
    #[error("{0}")]
    ServiceUnavailable(String),
    /// A failure the caller can act on, carrying a stable machine-readable
    /// `code`. The message is written for the caller and is always returned,
    /// even for 5xx statuses, so it must never carry internals, headers or
    /// tokens.
    #[error("{message}")]
    Coded {
        status: StatusCode,
        code: &'static str,
        message: String,
    },
    /// Server-side misconfiguration or invariant failure. The message is
    /// logged but never returned to the client.
    #[error("{0}")]
    Internal(String),
    #[error(transparent)]
    Runtime(#[from] SessionRuntimeError),
    #[error(transparent)]
    Workflow(#[from] WorkflowRuntimeError),
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
}

impl ApiError {
    /// A caller-actionable failure with a stable `code` (see [`Self::Coded`]).
    pub(crate) fn coded(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self::Coded {
            status,
            code,
            message: message.into(),
        }
    }
}

impl From<ThreadKeyError> for ApiError {
    fn from(error: ThreadKeyError) -> Self {
        Self::BadRequest(error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed(_) => StatusCode::METHOD_NOT_ALLOWED,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Coded { status, .. } => *status,
            Self::Runtime(SessionRuntimeError::BadRequest(_)) => StatusCode::BAD_REQUEST,
            Self::Runtime(SessionRuntimeError::ShuttingDown) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Runtime(SessionRuntimeError::Store(SessionStoreError::NotFound { .. })) => {
                StatusCode::NOT_FOUND
            }
            Self::Runtime(SessionRuntimeError::Store(SessionStoreError::HarnessConflict {
                ..
            })) => StatusCode::CONFLICT,
            Self::Runtime(SessionRuntimeError::Store(SessionStoreError::PrincipalConflict {
                ..
            })) => StatusCode::CONFLICT,
            Self::Runtime(SessionRuntimeError::Store(
                SessionStoreError::ExecutionActive { .. }
                | SessionStoreError::WorkflowOwnedSandbox { .. }
                | SessionStoreError::PrBindingConflict { .. }
                | SessionStoreError::PrBindingStateConflict { .. },
            )) => StatusCode::CONFLICT,
            Self::Runtime(SessionRuntimeError::Store(SessionStoreError::InvalidPrBinding(_))) => {
                StatusCode::BAD_REQUEST
            }
            Self::Runtime(SessionRuntimeError::Store(SessionStoreError::PrBindingNotFound {
                ..
            })) => StatusCode::NOT_FOUND,
            Self::Runtime(SessionRuntimeError::IronControl(
                centaur_iron_control::IronControlError::PrincipalDerivation(_),
            )) => StatusCode::BAD_REQUEST,
            Self::Runtime(SessionRuntimeError::IronControl(
                centaur_iron_control::IronControlError::SessionPrincipalNotPreapproved { .. }
                | centaur_iron_control::IronControlError::RequesterNotLinked { .. },
            )) => StatusCode::FORBIDDEN,
            Self::Workflow(WorkflowRuntimeError::BadRequest(_)) => StatusCode::BAD_REQUEST,
            Self::Workflow(WorkflowRuntimeError::Disabled(_)) => StatusCode::FORBIDDEN,
            Self::Workflow(WorkflowRuntimeError::NotFound(_)) => StatusCode::NOT_FOUND,
            Self::Workflow(WorkflowRuntimeError::Upstream(_)) => StatusCode::BAD_GATEWAY,
            Self::Internal(_)
            | Self::Runtime(_)
            | Self::Workflow(_)
            | Self::Serialize(_)
            | Self::Sqlx(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        // 5xx error details are server-side faults: log them for operators but
        // never echo internals (SQL text, hostnames, config refs) to clients.
        let message = if status.is_server_error() && !matches!(self, Self::Coded { .. }) {
            tracing::error!(
                status = status.as_u16(),
                error = %error_chain(&self),
                "API request failed"
            );
            "internal server error".to_owned()
        } else {
            self.to_string()
        };
        let mut body = json!({
            "ok": false,
            "error": message,
        });
        // Structured conflict details let clients (e.g. the slackbot) recover by
        // retrying with the session's existing harness instead of parsing the
        // human-readable message.
        if let Self::Runtime(SessionRuntimeError::Store(SessionStoreError::HarnessConflict {
            existing,
            requested,
            ..
        })) = &self
        {
            body["code"] = json!("harness_conflict");
            body["existing_harness"] = json!(existing);
            body["requested_harness"] = json!(requested);
        }
        if let Self::Runtime(SessionRuntimeError::Store(SessionStoreError::PrincipalConflict {
            existing,
            requested,
            ..
        })) = &self
        {
            body["code"] = json!("principal_conflict");
            body["existing_principal"] = json!(existing);
            body["requested_principal"] = json!(requested);
        }
        if let Self::Runtime(SessionRuntimeError::IronControl(
            centaur_iron_control::IronControlError::SessionPrincipalNotPreapproved { .. },
        )) = &self
        {
            body["code"] = json!("session_principal_not_preapproved");
        }
        // Lets the ingress tell the person how to link their account instead of
        // surfacing a generic failure.
        if let Self::Runtime(SessionRuntimeError::IronControl(
            centaur_iron_control::IronControlError::RequesterNotLinked { provider, .. },
        )) = &self
        {
            body["code"] = json!("requester_not_linked");
            body["provider"] = json!(provider);
        }
        if let Self::Coded { code, .. } = &self {
            body["code"] = json!(code);
        }
        if let Self::Runtime(SessionRuntimeError::Store(SessionStoreError::PrBindingConflict {
            ..
        })) = &self
        {
            body["code"] = json!("binding_conflict");
        }
        if let Self::Runtime(SessionRuntimeError::Store(
            SessionStoreError::PrBindingStateConflict { .. },
        )) = &self
        {
            body["code"] = json!("binding_state_conflict");
        }
        let mut response = (status, Json(body)).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, "Bearer".parse().unwrap());
        }
        response
    }
}

/// Render an error and its full `source()` chain as a single string. Causes
/// already rendered into an ancestor's message are skipped.
pub(crate) fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let rendered = cause.to_string();
        if !message.contains(&rendered) {
            message.push_str(": ");
            message.push_str(&rendered);
        }
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use centaur_iron_control::{IronControlError, PrincipalDerivationError};

    #[test]
    fn principal_derivation_errors_are_bad_requests() {
        let response = ApiError::Runtime(SessionRuntimeError::IronControl(
            IronControlError::PrincipalDerivation(PrincipalDerivationError::MissingSlackTeamId),
        ))
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unapproved_session_principals_are_forbidden_with_a_stable_code() {
        let response = ApiError::Runtime(SessionRuntimeError::IronControl(
            IronControlError::SessionPrincipalNotPreapproved {
                foreign_id: "slack-channel-t123-c123".to_owned(),
            },
        ))
        .into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("decode response body");
        assert_eq!(body["code"], "session_principal_not_preapproved");
    }

    #[tokio::test]
    async fn unlinked_requesters_are_forbidden_with_a_stable_code() {
        let response = ApiError::Runtime(SessionRuntimeError::IronControl(
            IronControlError::RequesterNotLinked {
                provider: "linear",
                subject: "lin-user-1".to_owned(),
            },
        ))
        .into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("decode response body");
        assert_eq!(body["code"], "requester_not_linked");
        assert_eq!(body["provider"], "linear");
    }

    #[tokio::test]
    async fn principal_conflicts_include_structured_details() {
        let response = ApiError::Runtime(SessionRuntimeError::Store(
            SessionStoreError::PrincipalConflict {
                thread_key: "workflow:report".to_owned(),
                existing: "prn_finance".to_owned(),
                requested: "prn_support".to_owned(),
            },
        ))
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("decode response body");
        assert_eq!(body["code"], json!("principal_conflict"));
        assert_eq!(body["existing_principal"], json!("prn_finance"));
        assert_eq!(body["requested_principal"], json!("prn_support"));
    }

    #[tokio::test]
    async fn coded_errors_return_their_code_and_message_at_any_status() {
        for status in [StatusCode::CONFLICT, StatusCode::BAD_GATEWAY] {
            let response = ApiError::coded(status, "github_oauth_required", "connect GitHub first")
                .into_response();
            assert_eq!(response.status(), status);
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read response body");
            let body: serde_json::Value =
                serde_json::from_slice(&body).expect("decode response body");
            assert_eq!(body["code"], "github_oauth_required");
            assert_eq!(body["error"], "connect GitHub first");
        }
    }

    #[tokio::test]
    async fn pr_binding_store_errors_map_to_client_statuses() {
        for (error, status, code) in [
            (
                SessionStoreError::PrBindingNotFound {
                    repository: "acme/widgets".to_owned(),
                    number: 7,
                },
                StatusCode::NOT_FOUND,
                None,
            ),
            (
                SessionStoreError::InvalidPrBinding("repository must be owner/repo".to_owned()),
                StatusCode::BAD_REQUEST,
                None,
            ),
            (
                SessionStoreError::PrBindingConflict {
                    repository: "acme/widgets".to_owned(),
                    number: 7,
                    reason: "the pull request is already bound".to_owned(),
                },
                StatusCode::CONFLICT,
                Some("binding_conflict"),
            ),
            (
                SessionStoreError::PrBindingStateConflict {
                    repository: "acme/widgets".to_owned(),
                    number: 7,
                    field: "closed",
                },
                StatusCode::CONFLICT,
                Some("binding_state_conflict"),
            ),
        ] {
            let response = ApiError::Runtime(SessionRuntimeError::Store(error)).into_response();
            assert_eq!(response.status(), status);
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read response body");
            let body: serde_json::Value =
                serde_json::from_slice(&body).expect("decode response body");
            assert_eq!(body.get("code").and_then(|code| code.as_str()), code);
        }
    }
}
