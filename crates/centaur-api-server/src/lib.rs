mod api_clients;
mod api_jwt;
mod auth;
pub mod client;
mod error;
mod mcp;
mod pr_bindings;
mod routes;
mod slack_external_teams;
mod slack_proxy;
mod tool_discovery;
pub mod types;

pub use auth::{ApiAuthConfig, ApiAuthConfigError};
pub use centaur_session_runtime::{SandboxRuntime, SessionRuntime};
pub use error::ApiError;
pub use mcp::mcp_codemode_enabled;
pub use routes::{
    AppState, build_router_with_app_state, build_router_with_runtime,
    build_router_with_session_and_workflow_runtime, build_router_with_session_runtime,
};
pub use slack_external_teams::partition_slack_external_team_ids;
pub use tool_discovery::{
    DiscoveredToolProxyFragment, ToolDiscoveryConfig, ToolDiscoveryError,
    discover_persona_registry, discover_tool_proxy_fragment,
};

#[doc(hidden)]
pub fn warm_slack_public_channel_cache() {
    slack_proxy::warm_slack_public_channel_cache();
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use async_trait::async_trait;
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, StatusCode, header},
    };
    use centaur_sandbox_core::{
        ObservedSandbox, SandboxBackend, SandboxError, SandboxHandle, SandboxId, SandboxIo,
        SandboxResult, SandboxSpec, SandboxStatus,
    };
    use centaur_session_core::ThreadKey;
    use centaur_session_runtime::{SandboxRuntime, SessionPrincipalRegistrar};
    use centaur_session_sqlx::{PgSessionStore, PrRef};
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;

    use super::{
        ApiAuthConfig, AppState, build_router_with_app_state,
        build_router_with_runtime as build_router_with_iron_control,
    };

    fn test_auth() -> ApiAuthConfig {
        ApiAuthConfig::testing("test-secret")
    }

    fn test_auth_with_slack() -> ApiAuthConfig {
        ApiAuthConfig::testing_with_slack_ingress("test-slackbot-key", "test-secret")
    }

    fn console_token() -> String {
        encode(
            &Header::new(Algorithm::HS256),
            &json!({
                "iss": "centaur-console",
                "sub": "centaur-console",
                "aud": "centaur-api",
                "iat": 1_700_000_000i64,
                "exp": 4_102_444_800i64,
                "token_use": "console_service",
            }),
            &EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap()
    }

    fn principal_token(subject: &str) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            &json!({
                "iss": "centaur-console",
                "sub": subject,
                "aud": "centaur-api",
                "iat": 1_700_000_000i64,
                "exp": 4_102_444_800i64,
                "capabilities": {
                    "sessions_read": false,
                    "workflows_read": false,
                    "workflows_write": false
                },
                "slack": {
                    "upload_channels": [],
                    "download_channels": [],
                    "history_channels": []
                }
            }),
            &EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap()
    }

    #[derive(Clone, Copy)]
    struct TestSessionPrincipalRegistrar;

    #[async_trait]
    impl SessionPrincipalRegistrar for TestSessionPrincipalRegistrar {
        async fn register_session(
            &self,
            _thread_key: &str,
            _metadata: Option<&Value>,
            _create_if_missing: bool,
        ) -> Result<centaur_iron_control::Principal, centaur_iron_control::IronControlError>
        {
            Ok(test_principal("prn_test"))
        }

        async fn register_requester(
            &self,
            _thread_key: &str,
            _metadata: Option<&Value>,
            _create_if_missing: bool,
        ) -> Result<Option<centaur_iron_control::Principal>, centaur_iron_control::IronControlError>
        {
            Ok(None)
        }

        async fn get_principal(
            &self,
            principal: &str,
        ) -> Result<centaur_iron_control::Principal, centaur_iron_control::IronControlError>
        {
            Ok(test_principal(principal))
        }
    }

    fn test_principal(id: &str) -> centaur_iron_control::Principal {
        centaur_iron_control::Principal {
            id: id.to_owned(),
            foreign_id: Some("test".to_owned()),
            name: "Test".to_owned(),
            labels: Default::default(),
            sandbox_observability_enabled: true,
        }
    }

    fn build_router_with_runtime(
        store: PgSessionStore,
        sandbox_runtime: SandboxRuntime,
    ) -> axum::Router {
        build_router_with_iron_control(
            store,
            sandbox_runtime,
            TestSessionPrincipalRegistrar,
            test_auth(),
        )
    }

    #[tokio::test]
    async fn metrics_endpoint_renders_http_request_metrics() {
        let pool =
            PgPool::connect_lazy("postgres://postgres:postgres@localhost/centaur_test").unwrap();
        let app = build_router_with_runtime(
            PgSessionStore::new(pool),
            SandboxRuntime::backend(Arc::new(TestBackend::default()), SandboxSpec::new("test")),
        );

        let app = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(app.status(), StatusCode::OK);

        let pool =
            PgPool::connect_lazy("postgres://postgres:postgres@localhost/centaur_test").unwrap();
        let app = build_router_with_runtime(
            PgSessionStore::new(pool),
            SandboxRuntime::backend(Arc::new(TestBackend::default()), SandboxSpec::new("test")),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body.contains(
                r#"http_server_requests_total{method="GET",route="/healthz",status="200"}"#
            )
        );
    }

    #[tokio::test]
    async fn healthz_is_available_before_runtime_is_ready() {
        let app = build_router_with_app_state(AppState::unready(test_auth()));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn protected_routes_reject_anonymous_requests_before_handlers() {
        for request in [
            Request::builder()
                .method(Method::POST)
                .uri("/api/session/not-a-valid-thread-key")
                .body(Body::from("not-json"))
                .unwrap(),
            Request::builder()
                .uri("/api/slack/channels")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/sandboxes/drain")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/session/slack%3AC123%3A123.456/pause")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri("/api/workflows/runs")
                .body(Body::empty())
                .unwrap(),
        ] {
            let response = build_router_with_app_state(AppState::unready(test_auth()))
                .oneshot(request)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response.headers().get(header::WWW_AUTHENTICATE),
                Some(&"Bearer".parse().unwrap())
            );
        }
    }

    #[tokio::test]
    async fn console_service_jwt_passes_every_capability_route_family() {
        for request in [
            Request::builder()
                .uri("/api/session/slack%3AC123%3A123.456")
                .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/session/slack%3AC123%3A123.456/interrupt")
                .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/sandboxes/drain")
                .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/session/slack%3AC123%3A123.456/pause")
                .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/session/slack%3AC123%3A123.456/sandbox")
                .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri("/api/workflows/runs")
                .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/workflows/events")
                .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"event_name":"test.event","payload":{}}"#))
                .unwrap(),
            Request::builder()
                .uri("/api/admin/slack/dm-sync/checkpoints")
                .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri("/api/admin/google/docs-sync/checkpoint")
                .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/admin/google/docs-sync/content-status")
                .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"files":[]}"#))
                .unwrap(),
            Request::builder()
                .uri("/api/admin/granola/sync/checkpoint")
                .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                .body(Body::empty())
                .unwrap(),
        ] {
            let response = build_router_with_app_state(AppState::unready(test_auth()))
                .oneshot(request)
                .await
                .unwrap();
            assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
            assert_ne!(response.status(), StatusCode::FORBIDDEN);
        }

        let slack_proxy = build_router_with_app_state(AppState::unready(test_auth()))
            .oneshot(
                Request::builder()
                    .uri("/api/slack/channels")
                    .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(slack_proxy.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn public_and_protocol_routes_are_outside_the_general_auth_gate() {
        for uri in [
            "/healthz",
            "/readyz",
            "/metrics",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
        ] {
            let response = build_router_with_app_state(AppState::unready(test_auth()))
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_ne!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }

        let response = build_router_with_app_state(AppState::unready(test_auth()))
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/webhooks/example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ingress_key_is_platform_scoped_and_capability_limited() {
        let authorized = format!("Bearer {}", "test-slackbot-key");
        for (request, expected) in [
            (
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/session/slack%3AC123%3A123.456")
                    .header(header::AUTHORIZATION, &authorized)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"harness_type":"codex"}"#))
                    .unwrap(),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/session/discord%3A123%3A456")
                    .header(header::AUTHORIZATION, &authorized)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"harness_type":"codex"}"#))
                    .unwrap(),
                StatusCode::FORBIDDEN,
            ),
            (
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/api/session/slack%3AC123%3A123.456/sandbox")
                    .header(header::AUTHORIZATION, &authorized)
                    .body(Body::empty())
                    .unwrap(),
                StatusCode::FORBIDDEN,
            ),
            (
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/sandboxes/drain")
                    .header(header::AUTHORIZATION, &authorized)
                    .body(Body::empty())
                    .unwrap(),
                StatusCode::FORBIDDEN,
            ),
            (
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/session/slack%3AC123%3A123.456/pause")
                    .header(header::AUTHORIZATION, &authorized)
                    .body(Body::empty())
                    .unwrap(),
                StatusCode::FORBIDDEN,
            ),
            (
                Request::builder()
                    .uri("/api/workflows/runs")
                    .header(header::AUTHORIZATION, &authorized)
                    .body(Body::empty())
                    .unwrap(),
                StatusCode::FORBIDDEN,
            ),
            (
                Request::builder()
                    .uri("/api/slack/channels")
                    .header(header::AUTHORIZATION, &authorized)
                    .body(Body::empty())
                    .unwrap(),
                StatusCode::FORBIDDEN,
            ),
            (
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/workflows/events")
                    .header(header::AUTHORIZATION, &authorized)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"event_name":"test.event","payload":{}}"#))
                    .unwrap(),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            let response = build_router_with_app_state(AppState::unready(test_auth_with_slack()))
                .oneshot(request)
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }

    #[tokio::test]
    async fn workflow_actions_require_trusted_ingress_capability() {
        let mut message = json!({"channel": "C1", "blocks": [{"type": "actions", "elements": [{
            "type": "button", "action_id": "centaur.workflow.action:00000000-0000-0000-0000-000000000001:approve",
            "value": json!({"workflow_name": "review", "input": {"release_id": "r1"}}).to_string(),
        }]}]});
        centaur_workflows::slack_buttons::sign_message(&mut message, b"test-secret").unwrap();
        let signed = message["blocks"][0]["elements"][0]["value"]
            .as_str()
            .unwrap();
        for (token, button, expected) in [
            (
                "test-slackbot-key".to_owned(),
                signed,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "test-slackbot-key".to_owned(),
                r#"{"workflow_name":"review","input":{}}"#,
                StatusCode::FORBIDDEN,
            ),
            (
                principal_token("prn_sandbox"),
                signed,
                StatusCode::FORBIDDEN,
            ),
        ] {
            let request = json!({"button": button, "idempotency_key": "click-1", "click": {
                "id": "00000000-0000-0000-0000-000000000001", "action": "approve", "channel_id": "C1", "user_id": "U1",
            }});
            let response = build_router_with_app_state(AppState::unready(test_auth_with_slack()))
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/workflows/actions/invoke")
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(request.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }

    #[tokio::test]
    async fn slack_external_team_ids_are_readable_by_slackbot_and_writable_by_console() {
        let slackbot = "test-slackbot-key".to_owned();
        let body = r#"{"slack_external_team_ids":["T0PARTNER"]}"#;
        for (method, token, expected) in [
            (
                Method::GET,
                slackbot.clone(),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                Method::GET,
                principal_token("prn_sandbox"),
                StatusCode::FORBIDDEN,
            ),
            (Method::PUT, slackbot, StatusCode::FORBIDDEN),
            (
                Method::PUT,
                console_token(),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            let response = build_router_with_app_state(AppState::unready(test_auth_with_slack()))
                .oneshot(
                    Request::builder()
                        .method(&method)
                        .uri("/api/slack/external-team-ids")
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{method}");
        }
    }

    #[tokio::test]
    async fn principal_jwt_is_capability_scoped() {
        let principal = principal_token("prn_sandbox");
        let write_response = build_router_with_app_state(AppState::unready(test_auth()))
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/session/slack%3AC123%3A123.456")
                    .header(header::AUTHORIZATION, format!("Bearer {principal}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"harness_type":"codex"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(write_response.status(), StatusCode::FORBIDDEN);

        let release_response = build_router_with_app_state(AppState::unready(test_auth()))
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/api/session/slack%3AC123%3A123.456/sandbox")
                    .header(header::AUTHORIZATION, format!("Bearer {principal}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(release_response.status(), StatusCode::FORBIDDEN);

        let slack_response = build_router_with_app_state(AppState::unready(test_auth()))
            .oneshot(
                Request::builder()
                    .uri("/api/slack/channels")
                    .header(header::AUTHORIZATION, format!("Bearer {principal}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(slack_response.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(slack_response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn slack_proxy_user_message_and_reaction_routes_are_principal_only() {
        let routes = [
            (Method::GET, "/api/slack/users"),
            (Method::GET, "/api/slack/users?limit=10&cursor=next"),
            (Method::GET, "/api/slack/users/U123456789"),
            (
                Method::GET,
                "/api/slack/channels/C123456789/messages/1700000000.000001/permalink",
            ),
            (
                Method::POST,
                "/api/slack/channels/C123456789/messages/1700000000.000001/reactions/eyes",
            ),
            (
                Method::DELETE,
                "/api/slack/channels/C123456789/messages/1700000000.000001/reactions/eyes",
            ),
            (
                Method::POST,
                "/api/slack/channels/C123456789/messages/1700000000.000001/reactions/%2B1",
            ),
        ];
        let forged = encode(
            &Header::new(Algorithm::HS256),
            &json!({
                "iss": "centaur-console",
                "sub": "prn_sandbox",
                "aud": "centaur-api",
                "iat": 1_700_000_000i64,
                "exp": 4_102_444_800i64,
            }),
            &EncodingKey::from_secret(b"wrong-secret"),
        )
        .unwrap();
        let principal = principal_token("prn_sandbox");

        for (method, uri) in routes {
            for (authorization, expected) in [
                (None, Some(StatusCode::UNAUTHORIZED)),
                (Some(forged.clone()), Some(StatusCode::UNAUTHORIZED)),
                (
                    Some("test-slackbot-key".to_owned()),
                    Some(StatusCode::FORBIDDEN),
                ),
                (Some(console_token()), Some(StatusCode::FORBIDDEN)),
                // Principals pass the gate; the handler then needs Slack config.
                (Some(principal.clone()), None),
            ] {
                let mut request = Request::builder().method(&method).uri(uri);
                if let Some(token) = &authorization {
                    request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
                }
                let response =
                    build_router_with_app_state(AppState::unready(test_auth_with_slack()))
                        .oneshot(request.body(Body::empty()).unwrap())
                        .await
                        .unwrap();
                let status = response.status();
                match expected {
                    Some(expected) => assert_eq!(status, expected, "{method} {uri}"),
                    None => {
                        assert_ne!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
                        assert_ne!(status, StatusCode::FORBIDDEN, "{method} {uri}");
                        assert_ne!(status, StatusCode::NOT_FOUND, "{method} {uri}");
                        assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {uri}");
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn healthz_decodes_slack_client_bearer_jwt_when_present() {
        let app = build_router_with_app_state(AppState::unready(test_auth()));
        let token = encode(
            &Header::new(Algorithm::HS256),
            &json!({
                "iss": "centaur-console",
                "sub": "principal_123",
                "aud": "centaur-api",
                "iat": 1_700_000_000i64,
                "exp": 4_102_444_800i64,
                "slack": {
                    "upload_channels": ["C123456789"],
                    "download_channels": ["C987654321"],
                    "history_channels": ["C111111111"]
                }
            }),
            &EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body.get("ok").and_then(Value::as_bool), Some(true));
        assert_eq!(
            body.pointer("/slack_client_jwt/claims/sub")
                .and_then(Value::as_str),
            Some("principal_123")
        );
        assert_eq!(
            body.pointer("/slack_client_jwt/claims/slack/upload_channels/0")
                .and_then(Value::as_str),
            Some("C123456789")
        );
        assert_eq!(
            body.pointer("/slack_client_jwt/claims/slack/download_channels/0")
                .and_then(Value::as_str),
            Some("C987654321")
        );
        assert_eq!(
            body.pointer("/slack_client_jwt/claims/slack/history_channels/0")
                .and_then(Value::as_str),
            Some("C111111111")
        );
    }

    #[tokio::test]
    async fn readyz_reports_starting_until_runtime_is_ready() {
        let state = AppState::unready(test_auth());
        let app = build_router_with_app_state(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let pool =
            PgPool::connect_lazy("postgres://postgres:postgres@localhost/centaur_test").unwrap();
        state.mark_ready(
            centaur_session_runtime::SessionRuntime::new(
                PgSessionStore::new(pool),
                SandboxRuntime::backend(Arc::new(TestBackend::default()), SandboxSpec::new("test")),
                TestSessionPrincipalRegistrar,
            ),
            None,
            None,
        );
        let app = build_router_with_app_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn token_endpoint_is_routed_only_while_api_clients_are_enabled() {
        for (enabled, status) in [
            (true, StatusCode::BAD_REQUEST),
            (false, StatusCode::NOT_FOUND),
        ] {
            let request = Request::builder()
                .method(Method::POST)
                .uri("/oauth/token")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("grant_type=password"))
                .unwrap();
            let response = build_router_with_app_state(AppState::unready(
                test_auth().with_api_clients_enabled(enabled),
            ))
            .oneshot(request)
            .await
            .unwrap();
            assert_eq!(response.status(), status, "enabled={enabled}");
        }
    }

    #[tokio::test]
    async fn runtime_routes_report_unavailable_until_runtime_is_ready() {
        for mut request in [
            Request::builder()
                .method(Method::GET)
                .uri("/api/session/slack%3AC123%3A123.456")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/session/slack%3AC123%3A123.456")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"harness_type":"codex"}"#))
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/session/slack%3AC123%3A123.456/messages")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"messages":[]}"#))
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/session/slack%3AC123%3A123.456/execute")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"input_lines":[]}"#))
                .unwrap(),
            Request::builder()
                .method(Method::GET)
                .uri("/api/session/slack%3AC123%3A123.456/events")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/sandboxes/drain")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::GET)
                .uri("/api/workflows/schedules")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::GET)
                .uri("/api/workflows/runs")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/workflows/runs")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"workflow_name":"echo","input":{}}"#))
                .unwrap(),
            Request::builder()
                .method(Method::GET)
                .uri("/api/workflows/runs/run-1")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/workflows/runs/run-1/cancel")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/workflows/events")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"event_name":"test.event","payload":{}}"#))
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/webhooks/test")
                .body(Body::empty())
                .unwrap(),
        ] {
            request.headers_mut().insert(
                header::AUTHORIZATION,
                format!("Bearer {}", console_token()).parse().unwrap(),
            );
            let app = build_router_with_app_state(AppState::unready(test_auth()));
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        }
    }

    #[test]
    fn mcp_requires_bearer_before_runtime_is_ready() {
        // MCP tests vary public URL configuration through process environment.
        let _lock = crate::mcp::MCP_ENV_LOCK.lock().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let app = build_router_with_app_state(AppState::unready(test_auth()));

                let response = app
                    .oneshot(
                        Request::builder()
                            .method(Method::POST)
                            .uri("/mcp")
                            .header(header::HOST, "centaur.local")
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                            ))
                            .unwrap(),
                    )
                    .await
                    .unwrap();

                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                let challenge = response
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap();
                assert!(challenge.contains("Bearer"));
                assert!(challenge.contains(
                    "resource_metadata=\"http://centaur.local/.well-known/oauth-protected-resource/mcp\""
                ));
            });
    }

    /// Messages and executes carry attachments and long transcripts, so these
    /// routes must read bodies past axum's 2 MiB default limit.
    #[tokio::test]
    async fn session_writes_accept_bodies_over_the_default_limit() {
        let pool =
            PgPool::connect_lazy("postgres://postgres:postgres@localhost/centaur_test").unwrap();
        let app = build_router_with_runtime(
            PgSessionStore::new(pool),
            SandboxRuntime::backend(Arc::new(TestBackend::default()), SandboxSpec::new("test")),
        );
        let padding = "x".repeat(3 * 1024 * 1024);

        for (route, field) in [("messages", "messages"), ("execute", "input_lines")] {
            // A well-formed body with the wrong shape: rejecting it as 422 proves
            // the whole body was read and parsed rather than refused as too large.
            let body = json!({ field: padding }).to_string();
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/api/session/slack%3AC123%3A123.456/{route}"))
                        .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{route}"
            );
        }
    }

    #[tokio::test]
    async fn session_context_exposes_slack_channel_and_thread_ts() {
        let pool =
            PgPool::connect_lazy("postgres://postgres:postgres@localhost/centaur_test").unwrap();
        let app = build_router_with_runtime(
            PgSessionStore::new(pool),
            SandboxRuntime::backend(Arc::new(TestBackend::default()), SandboxSpec::new("test")),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/session/slack%3AC123%3A123.456")
                    .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["thread_key"], "slack:C123:123.456");
        assert_eq!(body["platform"], "slack");
        assert_eq!(body["slack"]["channel_id"], "C123");
        assert_eq!(body["slack"]["thread_ts"], "123.456");
        assert!(body.get("discord").is_none());
        assert!(body.get("linear").is_none());
    }

    #[tokio::test]
    async fn session_context_exposes_discord_guild_channel_and_thread() {
        let pool =
            PgPool::connect_lazy("postgres://postgres:postgres@localhost/centaur_test").unwrap();
        let app = build_router_with_runtime(
            PgSessionStore::new(pool),
            SandboxRuntime::backend(Arc::new(TestBackend::default()), SandboxSpec::new("test")),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/session/discord%3A111%3A222%3A333")
                    .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["thread_key"], "discord:111:222:333");
        assert_eq!(body["platform"], "discord");
        assert_eq!(body["discord"]["guild_id"], "111");
        assert_eq!(body["discord"]["channel_id"], "222");
        assert_eq!(body["discord"]["thread_id"], "333");
        assert!(body.get("slack").is_none());
        assert!(body.get("linear").is_none());
    }

    #[tokio::test]
    async fn session_context_exposes_linear_issue_comment_and_session() {
        let pool =
            PgPool::connect_lazy("postgres://postgres:postgres@localhost/centaur_test").unwrap();
        let app = build_router_with_runtime(
            PgSessionStore::new(pool),
            SandboxRuntime::backend(Arc::new(TestBackend::default()), SandboxSpec::new("test")),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/session/linear%3AISSUE%3Ac%3ACMT%3As%3ASESS")
                    .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["thread_key"], "linear:ISSUE:c:CMT:s:SESS");
        assert_eq!(body["platform"], "linear");
        assert_eq!(body["linear"]["issue_id"], "ISSUE");
        assert_eq!(body["linear"]["comment_id"], "CMT");
        assert_eq!(body["linear"]["agent_session_id"], "SESS");
        assert!(body.get("slack").is_none());
        assert!(body.get("discord").is_none());
    }

    #[tokio::test]
    async fn session_context_exposes_github_repo_number_and_review_comment() {
        let pool =
            PgPool::connect_lazy("postgres://postgres:postgres@localhost/centaur_test").unwrap();
        let app = build_router_with_runtime(
            PgSessionStore::new(pool),
            SandboxRuntime::backend(Arc::new(TestBackend::default()), SandboxSpec::new("test")),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/session/github%3A0xSplits%2Fcentaur%3A704%3Arc%3A99")
                    .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["thread_key"], "github:0xSplits/centaur:704:rc:99");
        assert_eq!(body["platform"], "github");
        assert_eq!(body["github"]["owner"], "0xSplits");
        assert_eq!(body["github"]["repo"], "centaur");
        assert_eq!(body["github"]["number"], 704);
        assert_eq!(body["github"]["kind"], "pr");
        assert_eq!(body["github"]["review_comment_id"], 99);
        assert!(body.get("slack").is_none());
        assert!(body.get("discord").is_none());
        assert!(body.get("linear").is_none());
    }

    #[tokio::test]
    async fn session_context_omits_slack_for_non_slack_thread_key() {
        let pool =
            PgPool::connect_lazy("postgres://postgres:postgres@localhost/centaur_test").unwrap();
        let app = build_router_with_runtime(
            PgSessionStore::new(pool),
            SandboxRuntime::backend(Arc::new(TestBackend::default()), SandboxSpec::new("test")),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/session/cli%3Atest")
                    .header(header::AUTHORIZATION, format!("Bearer {}", console_token()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["thread_key"], "cli:test");
        assert_eq!(body["platform"], "unknown");
        assert!(body.get("slack").is_none());
        assert!(body.get("discord").is_none());
        assert!(body.get("linear").is_none());
    }

    fn test_auth_with_ingresses() -> ApiAuthConfig {
        ApiAuthConfig::testing_with_ingresses("test-secret")
    }

    async fn send(
        app: &axum::Router,
        method: Method,
        uri: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    #[tokio::test]
    async fn only_the_githubbot_ingress_may_call_pr_binding_routes() {
        let routes = [
            (Method::GET, "/api/pr-bindings/acme/widgets/7", None),
            (
                Method::POST,
                "/api/pr-bindings/acme/widgets/7/verify",
                Some(json!({
                    "node_id": "PR_1", "author_login": "bot", "created_at": "2026-11-23T00:00:00Z",
                    "head_ref": "fix", "head_sha": "abc123"
                })),
            ),
            (
                Method::POST,
                "/api/pr-bindings/acme/widgets/7/state",
                Some(json!({"managed": true})),
            ),
            (
                Method::POST,
                "/api/pr-bindings/acme/widgets/7/followups",
                Some(json!({
                    "kind": "review", "dedupe_key": "review:1", "head_sha": "abc123",
                    "actor": {"github_user_id": "1", "github_login": "ada"},
                    "prompt": [{"type": "text", "text": "hi"}]
                })),
            ),
            (
                Method::POST,
                "/api/pr-bindings/acme/widgets/7/dispatch",
                Some(json!({"current_head_sha": "abc123", "assigned": true, "open": true})),
            ),
            (
                Method::GET,
                "/api/pr-bindings/acme/widgets/7/executions/exe_1/events?after_event_id=0",
                None,
            ),
        ];
        let app = build_router_with_app_state(AppState::unready(test_auth_with_ingresses()));
        for (method, uri, body) in routes {
            for (token, expected) in [
                // githubbot passes the gate; the unready runtime answers.
                (
                    "test-githubbot-key".to_owned(),
                    StatusCode::SERVICE_UNAVAILABLE,
                ),
                ("test-slackbot-key".to_owned(), StatusCode::FORBIDDEN),
                (principal_token("prn_sandbox"), StatusCode::FORBIDDEN),
                (console_token(), StatusCode::FORBIDDEN),
            ] {
                let (status, _) = send(&app, method.clone(), uri, &token, body.clone()).await;
                assert_eq!(status, expected, "{method} {uri}");
            }
        }

        // Registration is the session principal's (or an operator's), never
        // an ingress key's.
        for (token, expected) in [
            (
                principal_token("prn_sandbox"),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (console_token(), StatusCode::SERVICE_UNAVAILABLE),
            ("test-githubbot-key".to_owned(), StatusCode::FORBIDDEN),
            ("test-slackbot-key".to_owned(), StatusCode::FORBIDDEN),
        ] {
            let (status, _) = send(
                &app,
                Method::POST,
                "/api/session/slack%3AT1%3AC1%3A1.2/pr-bindings",
                &token,
                Some(json!({"repository": "acme/widgets", "number": 7})),
            )
            .await;
            assert_eq!(status, expected);
        }
    }

    async fn pr_binding_test_app() -> Option<(axum::Router, PgSessionStore)> {
        let Ok(url) = std::env::var("SESSION_RUNTIME_TEST_DATABASE_URL") else {
            eprintln!("skipping: SESSION_RUNTIME_TEST_DATABASE_URL not set");
            return None;
        };
        let store = PgSessionStore::connect(&url)
            .await
            .expect("connect test db");
        store
            .run_migrations(centaur_session_sqlx::TextSearchBackend::Postgres)
            .await
            .expect("run migrations");
        let app = build_router_with_iron_control(
            store.clone(),
            SandboxRuntime::backend(Arc::new(TestBackend::default()), SandboxSpec::new("test")),
            TestSessionPrincipalRegistrar,
            test_auth_with_ingresses(),
        );
        Some((app, store))
    }

    async fn create_test_session(app: &axum::Router) -> String {
        let thread_key = format!("slack:T1:C1:{}.0001", uuid::Uuid::new_v4().simple());
        let (status, _) = send(
            app,
            Method::POST,
            &format!("/api/session/{}", urlencoding::encode(&thread_key)),
            &console_token(),
            Some(json!({"harness_type": "codex"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        thread_key
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr_binding_registration_requires_session_ownership_and_never_rebinds() {
        let Some((app, _store)) = pr_binding_test_app().await else {
            return;
        };
        let owner = create_test_session(&app).await;
        let other = create_test_session(&app).await;
        let repository = format!("Acme/Widgets-{}", uuid::Uuid::new_v4().simple());
        let register = |thread_key: &str| {
            format!(
                "/api/session/{}/pr-bindings",
                urlencoding::encode(thread_key)
            )
        };
        let body = json!({"repository": repository, "number": 7});

        // TestSessionPrincipalRegistrar binds every session to prn_test.
        let (status, _) = send(
            &app,
            Method::POST,
            &register(&owner),
            &principal_token("prn_other"),
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, binding) = send(
            &app,
            Method::POST,
            &register(&owner),
            &principal_token("prn_test"),
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{binding}");
        assert_eq!(binding["thread_key"], owner);
        assert_eq!(binding["state"], "registered");
        assert_eq!(binding["repository"], repository.to_lowercase());
        assert_eq!(binding["managed"], false);

        // The operator may register too, and a repeat is idempotent.
        let (status, _) = send(
            &app,
            Method::POST,
            &register(&owner),
            &console_token(),
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, conflict) = send(
            &app,
            Method::POST,
            &register(&other),
            &console_token(),
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(conflict["code"], "binding_conflict");

        let (status, _) = send(
            &app,
            Method::POST,
            &register("slack:T1:C1:missing"),
            &console_token(),
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = send(
            &app,
            Method::POST,
            &register(&owner),
            &console_token(),
            Some(json!({"repository": "not-a-repo", "number": 7})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn githubbot_verifies_activates_and_queues_pr_followups() {
        let Some((app, store)) = pr_binding_test_app().await else {
            return;
        };
        let githubbot = "test-githubbot-key";
        let thread_key = create_test_session(&app).await;
        let session_created_at = store
            .get_session(&centaur_session_core::ThreadKey::parse(thread_key.clone()).unwrap())
            .await
            .unwrap()
            .created_at;
        let rfc3339 = |at: time::OffsetDateTime| {
            at.format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        let repo = format!("widgets-{}", uuid::Uuid::new_v4().simple());
        let base = |number: i64| format!("/api/pr-bindings/acme/{repo}/{number}");
        for number in [7, 8] {
            let (status, _) = send(
                &app,
                Method::POST,
                &format!(
                    "/api/session/{}/pr-bindings",
                    urlencoding::encode(&thread_key)
                ),
                &principal_token("prn_test"),
                Some(json!({"repository": format!("acme/{repo}"), "number": number})),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }

        let (status, binding) = send(&app, Method::GET, &base(7), githubbot, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(binding["state"], "registered");
        assert_eq!(binding["session_exists"], true);
        assert_eq!(binding["active_execution"], false);
        let (status, _) = send(&app, Method::GET, &base(9), githubbot, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // A PR created before the session cannot be the session's PR.
        let (status, rejected) = send(
            &app,
            Method::POST,
            &format!("{}/verify", base(8)),
            githubbot,
            Some(json!({
                "node_id": "PR_old", "author_login": "centaur-bot",
                "created_at": rfc3339(session_created_at - time::Duration::days(1)),
                "head_ref": "fix", "head_sha": "abc123"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rejected["state"], "rejected");

        let verification = json!({
            "node_id": "PR_new", "author_login": "centaur-bot",
            "created_at": rfc3339(session_created_at + time::Duration::seconds(1)),
            "head_ref": "fix", "head_sha": "abc123"
        });
        let (status, verified) = send(
            &app,
            Method::POST,
            &format!("{}/verify", base(7)),
            githubbot,
            Some(verification.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(verified["state"], "verified");
        let mut other_node = verification.clone();
        other_node["node_id"] = json!("PR_other");
        let (status, mismatch) = send(
            &app,
            Method::POST,
            &format!("{}/verify", base(7)),
            githubbot,
            Some(other_node),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(mismatch["code"], "node_id_mismatch");

        let review = |review_id: u64| {
            json!({
                "kind": "review", "dedupe_key": format!("review:{review_id}"), "head_sha": "abc123",
                "actor": {"github_user_id": "90210001", "github_login": "ada"},
                "prompt": [{"type": "text", "text": "Address the review."}],
                "reply_target": {"type": "github_comment", "repository": format!("acme/{repo}"), "number": 7, "review_id": review_id}
            })
        };
        let (status, inactive) = send(
            &app,
            Method::POST,
            &format!("{}/followups", base(7)),
            githubbot,
            Some(review(11)),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(inactive["code"], "binding_inactive");

        let (status, state) = send(
            &app,
            Method::POST,
            &format!("{}/state", base(7)),
            githubbot,
            Some(json!({
                "managed": true, "head_sha": "abc123",
                "push_grant": {"actor": {"platform": "slack", "id": "U1", "name": "Ada"}, "branch": "fix", "active": true}
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(state["managed"], true);
        assert_eq!(state["push_grant"]["active"], false);

        let (status, first) = send(
            &app,
            Method::POST,
            &format!("{}/followups", base(7)),
            githubbot,
            Some(review(11)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["created"], true);
        assert_eq!(first["followup"]["status"], "queued");
        let (_, repeat) = send(
            &app,
            Method::POST,
            &format!("{}/followups", base(7)),
            githubbot,
            Some(review(11)),
        )
        .await;
        assert_eq!(repeat["created"], false);
        assert_eq!(repeat["followup"]["id"], first["followup"]["id"]);
        let (_, second) = send(
            &app,
            Method::POST,
            &format!("{}/followups", base(7)),
            githubbot,
            Some(review(12)),
        )
        .await;
        assert_eq!(second["created"], true);
        let mut bad = review(13);
        bad["dedupe_key"] = json!("comment:13");
        let (status, _) = send(
            &app,
            Method::POST,
            &format!("{}/followups", base(7)),
            githubbot,
            Some(bad),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // A turn already runs on the bound session, so dispatch reports busy.
        store
            .create_execution(
                &centaur_session_core::ThreadKey::parse(thread_key.clone()).unwrap(),
                Some("slack-turn"),
                json!({}),
            )
            .await
            .unwrap();
        let (status, outcome) = send(
            &app,
            Method::POST,
            &format!("{}/dispatch", base(7)),
            githubbot,
            Some(json!({"current_head_sha": "abc123", "assigned": true, "open": true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(outcome, json!({"dispatched": null, "reason": "busy"}));
        let (_, binding) = send(&app, Method::GET, &base(7), githubbot, None).await;
        assert_eq!(binding["active_execution"], true);

        let (status, outcome) = send(
            &app,
            Method::POST,
            &format!("{}/dispatch", base(7)),
            githubbot,
            Some(json!({"current_head_sha": "abc123", "assigned": false, "open": true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(outcome, json!({"dispatched": null, "reason": "unassigned"}));
        let (_, binding) = send(&app, Method::GET, &base(7), githubbot, None).await;
        assert_eq!(binding["managed"], false);
        assert_eq!(binding["push_grant"], Value::Null);
    }

    #[tokio::test]
    async fn only_the_slackbot_ingress_may_list_pr_followup_executions() {
        let app = build_router_with_app_state(AppState::unready(test_auth_with_ingresses()));
        for (token, expected) in [
            // slackbot passes the gate; the unready runtime answers.
            (
                "test-slackbot-key".to_owned(),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            ("test-githubbot-key".to_owned(), StatusCode::FORBIDDEN),
            (principal_token("prn_sandbox"), StatusCode::FORBIDDEN),
            (console_token(), StatusCode::FORBIDDEN),
        ] {
            let (status, _) = send(
                &app,
                Method::GET,
                "/api/pr-followups/executions?thread_prefix=slack:",
                &token,
                None,
            )
            .await;
            assert_eq!(status, expected);
        }
    }

    async fn send_raw(app: &axum::Router, uri: &str, token: &str) -> (StatusCode, String) {
        let request = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .expect("execution-scoped stream ends at the terminal event")
        .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn store_session(store: &PgSessionStore, thread_key: &str) -> ThreadKey {
        let thread_key = ThreadKey::parse(thread_key.to_owned()).unwrap();
        store
            .create_or_get_session(
                &thread_key,
                &centaur_session_core::HarnessType::Codex,
                None,
                json!({}),
                Default::default(),
            )
            .await
            .expect("create session");
        thread_key
    }

    /// A verified binding Centaur manages, registered by `thread_key`.
    async fn store_managed_binding(
        store: &PgSessionStore,
        thread_key: &ThreadKey,
        repository: &str,
        number: i64,
    ) -> PrRef {
        let pr = PrRef::new(repository, number).unwrap();
        store.register_pr_binding(thread_key, &pr).await.unwrap();
        let created_at = store.get_session(thread_key).await.unwrap().created_at;
        store
            .verify_pr_binding(
                &pr,
                &serde_json::from_value(json!({
                    "node_id": format!("PR_{number}"), "author_login": "centaur-bot",
                    "created_at": (created_at + time::Duration::seconds(1))
                        .format(&time::format_description::well_known::Rfc3339).unwrap(),
                    "head_ref": "fix", "head_sha": "abc123"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        store
            .update_pr_binding_state(
                &pr,
                serde_json::from_value(json!({"managed": true, "head_sha": "abc123"})).unwrap(),
            )
            .await
            .unwrap();
        pr
    }

    /// An execution `pr`'s queue dispatched, as dispatch records it: the
    /// follow-up's server-set metadata and a `dispatched` queue row. Left
    /// queued; the caller finishes it.
    async fn store_dispatched_execution(
        store: &PgSessionStore,
        thread_key: &ThreadKey,
        pr: &PrRef,
        review_id: u64,
    ) -> String {
        let (followup, _) = store
            .enqueue_pr_followup(
                pr,
                &serde_json::from_value(json!({
                    "kind": "review", "dedupe_key": format!("review:{review_id}"), "head_sha": "abc123",
                    "actor": {"github_user_id": "90210001", "github_login": "ada"},
                    "prompt": [{"type": "text", "text": "Address the review."}],
                    "reply_target": {
                        "type": "github_comment", "repository": pr.repository(),
                        "number": pr.number(), "review_id": review_id
                    }
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let execution = store
            .create_execution(
                thread_key,
                Some(&followup.id),
                centaur_session_sqlx::pr_followup_execute_metadata(&followup),
            )
            .await
            .unwrap()
            .execution;
        let mut lock = store.lock_pr_binding(pr).await.unwrap();
        lock.mark_dispatched(&followup.id, &execution.execution_id)
            .await
            .unwrap();
        lock.commit().await.unwrap();
        execution.execution_id
    }

    /// Ends an execution with `status`, as if it finished at `completed_at`.
    async fn finish_execution(
        store: &PgSessionStore,
        execution_id: &str,
        status: &str,
        completed_at: time::OffsetDateTime,
    ) {
        sqlx::query(
            "update session_executions set status = $2, completed_at = $3, updated_at = now() where execution_id = $1",
        )
        .bind(execution_id)
        .bind(status)
        .bind(completed_at)
        .execute(store.pool())
        .await
        .unwrap();
    }

    async fn turn_events(
        store: &PgSessionStore,
        thread_key: &ThreadKey,
        execution_id: &str,
        text: &str,
    ) -> Vec<i64> {
        let mut ids = Vec::new();
        for (event_type, payload) in [
            ("session.output.line", json!(text)),
            ("session.execution_completed", json!({})),
        ] {
            ids.push(
                store
                    .append_event(thread_key, Some(execution_id), event_type, payload)
                    .await
                    .unwrap()
                    .event_id,
            );
        }
        ids
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr_followup_event_streams_serve_only_the_bindings_dispatched_executions() {
        let Some((app, store)) = pr_binding_test_app().await else {
            return;
        };
        let githubbot = "test-githubbot-key";
        let tenant = uuid::Uuid::new_v4().simple().to_string();
        let thread_key = store_session(&store, &format!("slack:T{tenant}:C1:1.0001")).await;
        let pr7 = store_managed_binding(&store, &thread_key, &format!("acme/a-{tenant}"), 7).await;
        let pr8 = store_managed_binding(&store, &thread_key, &format!("acme/b-{tenant}"), 8).await;
        let now = time::OffsetDateTime::now_utc();

        let e7 = store_dispatched_execution(&store, &thread_key, &pr7, 1).await;
        let e7_events = turn_events(&store, &thread_key, &e7, "answer-for-pr7").await;
        finish_execution(&store, &e7, "completed", now).await;
        let e8 = store_dispatched_execution(&store, &thread_key, &pr8, 2).await;
        turn_events(&store, &thread_key, &e8, "answer-for-pr8").await;
        finish_execution(&store, &e8, "completed", now).await;
        // An ordinary turn on the same session, never dispatched.
        let plain = store
            .create_execution(&thread_key, Some("slack-turn"), json!({}))
            .await
            .unwrap()
            .execution
            .execution_id;
        turn_events(&store, &thread_key, &plain, "slack-only-answer").await;
        finish_execution(&store, &plain, "completed", now).await;
        // A follow-up still queued has no execution to read.
        store
            .enqueue_pr_followup(
                &pr7,
                &serde_json::from_value(json!({
                    "kind": "ci_failure", "dedupe_key": "ci:abc123:failure", "head_sha": "abc123",
                    "actor": {"github_user_id": "90210001", "github_login": "ada"},
                    "prompt": [{"type": "text", "text": "Fix CI."}]
                }))
                .unwrap(),
            )
            .await
            .unwrap();

        let events_uri = |pr: &PrRef, execution_id: &str, after: i64| {
            format!(
                "/api/pr-bindings/{}/{}/executions/{execution_id}/events?after_event_id={after}",
                pr.repository(),
                pr.number()
            )
        };
        let (status, body) = send_raw(&app, &events_uri(&pr7, &e7, 0), githubbot).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("answer-for-pr7"), "{body}");
        assert!(
            body.contains("event: session.execution_completed"),
            "{body}"
        );
        assert!(body.contains(&format!("id: {}", e7_events[0])), "{body}");
        assert!(!body.contains("answer-for-pr8"), "{body}");
        assert!(!body.contains("slack-only-answer"), "{body}");

        let (status, body) = send_raw(&app, &events_uri(&pr7, &e7, e7_events[0]), githubbot).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("answer-for-pr7"), "{body}");
        assert!(body.contains(&format!("id: {}", e7_events[1])), "{body}");

        for (pr, execution_id) in [
            // Another binding's dispatched execution, on the same session.
            (&pr7, e8.as_str()),
            (&pr8, e7.as_str()),
            // A turn no binding dispatched.
            (&pr7, plain.as_str()),
            (&pr7, "exe_unknown"),
        ] {
            let (status, _) = send(
                &app,
                Method::GET,
                &events_uri(pr, execution_id, 0),
                githubbot,
                None,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{} {execution_id}",
                pr.number()
            );
        }
        let (status, _) = send(
            &app,
            Method::GET,
            &format!("/api/pr-bindings/acme/none-{tenant}/7/executions/{e7}/events"),
            githubbot,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pr_followup_execution_listing_is_scoped_paged_and_trimmed() {
        let Some((app, store)) = pr_binding_test_app().await else {
            return;
        };
        let slackbot = "test-slackbot-key";
        let tenant = uuid::Uuid::new_v4().simple().to_string();
        let slack = store_session(&store, &format!("slack:T{tenant}:C1:1.0001")).await;
        let github = store_session(&store, &format!("github:acme/g-{tenant}:9")).await;
        let slack_pr = store_managed_binding(&store, &slack, &format!("acme/s-{tenant}"), 7).await;
        let github_pr =
            store_managed_binding(&store, &github, &format!("acme/g-{tenant}"), 9).await;
        let base = time::OffsetDateTime::now_utc() - time::Duration::hours(1);

        let mut finished = Vec::new();
        for (review_id, status) in [(1, "completed"), (2, "failed"), (3, "cancelled")] {
            let execution_id =
                store_dispatched_execution(&store, &slack, &slack_pr, review_id).await;
            // Completion order, not dispatch order, decides the listing.
            let completed_at =
                base + time::Duration::seconds(10 - i64::try_from(review_id).unwrap());
            finish_execution(&store, &execution_id, status, completed_at).await;
            finished.push((execution_id, status));
        }
        finished.reverse();
        // An ordinary turn that even carries `pr_followup` metadata is not
        // listed: only dispatch through the follow-up queue counts.
        let plain = store
            .create_execution(
                &slack,
                Some("slack-turn"),
                json!({"pr_followup": {"repository": "acme/x", "number": 1, "followup_id": "prf_x", "kind": "review"}}),
            )
            .await
            .unwrap()
            .execution
            .execution_id;
        finish_execution(&store, &plain, "completed", base).await;
        // Finished too recently to be listed yet.
        let recent = store_dispatched_execution(&store, &slack, &slack_pr, 4).await;
        finish_execution(
            &store,
            &recent,
            "completed",
            time::OffsetDateTime::now_utc(),
        )
        .await;
        // Still running.
        let running = store_dispatched_execution(&store, &slack, &slack_pr, 5).await;
        // Another platform's finished follow-up.
        let other = store_dispatched_execution(&store, &github, &github_pr, 6).await;
        finish_execution(&store, &other, "completed", base).await;

        let list = |query: String| {
            let app = app.clone();
            async move {
                send(
                    &app,
                    Method::GET,
                    &format!("/api/pr-followups/executions?{query}"),
                    slackbot,
                    None,
                )
                .await
            }
        };
        let prefix = urlencoding::encode(&format!("slack:T{tenant}:")).into_owned();
        let (status, page) = list(format!("thread_prefix={prefix}&limit=2")).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let ids = |page: &Value| {
            page["executions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["execution_id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&page), [finished[0].0.clone(), finished[1].0.clone()]);
        for (item, (execution_id, status)) in
            page["executions"].as_array().unwrap().iter().zip(&finished)
        {
            assert_eq!(item["execution_id"], execution_id.as_str());
            assert_eq!(item["thread_key"], slack.as_str());
            assert_eq!(item["status"], *status);
            // Only the `pr_followup` object, never the requester or push
            // authorization beside it.
            assert_eq!(item["metadata"].as_object().unwrap().len(), 1, "{item}");
            let followup = &item["metadata"]["pr_followup"];
            assert_eq!(followup["repository"], slack_pr.repository());
            assert_eq!(followup["number"], 7);
            assert_eq!(followup["kind"], "review");
            assert!(
                followup["followup_id"]
                    .as_str()
                    .unwrap()
                    .starts_with("prf_")
            );
            assert_eq!(followup["reply_target"]["type"], "github_comment");
        }
        assert!(!page.to_string().contains("90210001"), "{page}");
        assert!(!page.to_string().contains("prepare_and_ask"), "{page}");

        let cursor = page["next_cursor"].as_str().unwrap().to_owned();
        let after = |cursor: &str| urlencoding::encode(cursor).into_owned();
        let (status, page) = list(format!(
            "thread_prefix={prefix}&limit=2&after={}",
            after(&cursor)
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ids(&page), [finished[2].0.clone()]);
        let last = page["next_cursor"].as_str().unwrap().to_owned();
        assert_ne!(last, cursor);
        // Caught up: nothing new, and the cursor holds still.
        let (_, page) = list(format!("thread_prefix={prefix}&after={}", after(&last))).await;
        assert_eq!(ids(&page), Vec::<String>::new());
        assert_eq!(page["next_cursor"], last.as_str());

        // Asking for another platform's sessions lists nothing.
        for requested in [
            format!("github:acme/g-{tenant}"),
            "github:".to_owned(),
            "g".to_owned(),
        ] {
            let (status, page) =
                list(format!("thread_prefix={}", urlencoding::encode(&requested))).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(ids(&page), Vec::<String>::new(), "{requested}");
            assert_eq!(page["next_cursor"], Value::Null);
        }
        // Even an unnarrowed walk of every page stays on Slack.
        let mut cursor: Option<String> = None;
        let mut seen = Vec::new();
        loop {
            let query = match &cursor {
                Some(cursor) => format!("limit=500&after={}", after(cursor)),
                None => "limit=500".to_owned(),
            };
            let (status, page) = list(query).await;
            assert_eq!(status, StatusCode::OK);
            for item in page["executions"].as_array().unwrap() {
                assert!(
                    item["thread_key"].as_str().unwrap().starts_with("slack:"),
                    "{item}"
                );
            }
            let page_ids = ids(&page);
            if page_ids.is_empty() {
                break;
            }
            seen.extend(page_ids);
            cursor = page["next_cursor"].as_str().map(ToOwned::to_owned);
        }
        for (execution_id, _) in &finished {
            assert!(seen.contains(execution_id));
        }
        for execution_id in [&plain, &recent, &running, &other] {
            assert!(!seen.contains(execution_id), "{execution_id}");
        }

        let (status, _) = list("after=not-a-cursor".to_owned()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[derive(Default)]
    struct TestBackend {
        next_id: AtomicU64,
    }

    #[async_trait]
    impl SandboxBackend for TestBackend {
        fn name(&self) -> &'static str {
            "test"
        }

        async fn create(&self, _spec: SandboxSpec) -> SandboxResult<SandboxHandle> {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(SandboxHandle::new(
                SandboxId::new(format!("test-{id}")),
                self.name(),
            ))
        }

        async fn open_io(&self, _id: &SandboxId) -> SandboxResult<SandboxIo> {
            unreachable!("router construction should not open sandbox I/O")
        }

        async fn status(&self, _id: &SandboxId) -> SandboxResult<SandboxStatus> {
            Ok(SandboxStatus::Running)
        }

        async fn observe(&self, id: &SandboxId) -> SandboxResult<ObservedSandbox> {
            Ok(ObservedSandbox::new(
                id.clone(),
                self.name(),
                SandboxStatus::Running,
            ))
        }

        async fn list_observed(&self) -> SandboxResult<Vec<ObservedSandbox>> {
            Ok(Vec::new())
        }

        async fn stop(&self, _id: &SandboxId) -> SandboxResult<()> {
            Ok(())
        }

        async fn pause(&self, _id: &SandboxId) -> SandboxResult<()> {
            Err(SandboxError::Unsupported {
                backend: self.name(),
                operation: "pause",
            })
        }

        async fn resume(&self, _id: &SandboxId) -> SandboxResult<()> {
            Err(SandboxError::Unsupported {
                backend: self.name(),
                operation: "resume",
            })
        }
    }
}
