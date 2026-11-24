use std::env;

use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;
use serde_json::json;

use centaur_console_db::{
    SecretValues, SlackChannels as ConsoleSlackChannels, github_user_id, models::Config, oid,
    slack_channels,
};

use crate::{
    ApiError,
    models::{AppState, ProxyRecord},
};

#[derive(Serialize)]
struct ApiJwtClaims {
    iss: &'static str,
    aud: &'static str,
    iat: i64,
    exp: i64,
    sub: String,
    capabilities: Capabilities,
    slack: SlackChannels,
    /// The sandbox this proxy serves (the proxy's name). The proxy mints it
    /// from its own record, so a sandbox cannot present another's id; api-rs
    /// compares it with the session that owns that sandbox.
    sandbox_id: String,
    /// The GitHub account id of the human whose OAuth token this proxy would
    /// inject for `api.github.com`, set only when that is unambiguous (see
    /// [`centaur_console_db::github_user_id`]). api-rs compares it with the
    /// author GitHub reports for a pull request created with that token.
    #[serde(skip_serializing_if = "Option::is_none")]
    github_user_id: Option<String>,
}

#[derive(Serialize)]
struct Capabilities {
    sessions_read: bool,
    workflows_read: bool,
    workflows_write: bool,
}

#[derive(Serialize)]
struct SlackChannels {
    upload_channels: Vec<String>,
    download_channels: Vec<String>,
    history_channels: Vec<String>,
}

#[derive(Serialize)]
struct SandboxJwtClaims {
    iss: String,
    aud: String,
    iat: i64,
    exp: i64,
    sub: String,
    sandbox_id: String,
    proxy_id: String,
    principal_id: String,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct TokenWindows {
    pub(crate) api: Option<i64>,
    pub(crate) sandbox: Option<i64>,
}

pub(crate) fn token_windows(state: &AppState, proxy: &ProxyRecord, now: i64) -> TokenWindows {
    let api = proxy
        .principal_id
        .filter(|_| state.jwt_secret.is_some() && !state.api_hosts.is_empty())
        .map(|principal_id| window_start(&oid("prn", principal_id), now, 900));
    let sandbox = proxy
        .principal_id
        .filter(|_| state.jwt_secret.is_some() && state.console_host.is_some())
        .map(|_| window_start(&oid("prx", proxy.id), now, 86_400));
    TokenWindows { api, sandbox }
}

pub(crate) async fn append_api_jwt(
    state: &AppState,
    proxy: &ProxyRecord,
    config: &mut Config,
    now: i64,
) -> Result<(), ApiError> {
    let (Some(secret), Some(principal_id)) = (&state.jwt_secret, proxy.principal_id) else {
        return Ok(());
    };
    if state.api_hosts.is_empty() {
        return Ok(());
    }
    let channels = slack_channels(&state.pool, principal_id).await?;
    let github_user_id = github_user_id(
        &state.pool,
        SecretValues::Live(&state.encryption),
        principal_id,
        proxy.requester_principal_id,
    )
    .await?;
    let principal_oid = oid("prn", principal_id);
    let iat = window_start(&principal_oid, now, 900);
    let claims = api_jwt_claims(proxy, principal_oid, iat, channels, github_user_id);
    let token = jwt(&claims, secret)?;
    config.secrets.push(json!({
        "source": { "type": "control_plane", "value": token },
        "inject": { "header": "Authorization", "formatter": "Bearer {{ .Value }}" },
        "rules": state.api_hosts.iter().map(|host| json!({ "host": host })).collect::<Vec<_>>()
    }));
    Ok(())
}

fn api_jwt_claims(
    proxy: &ProxyRecord,
    principal_oid: String,
    iat: i64,
    channels: ConsoleSlackChannels,
    github_user_id: Option<String>,
) -> ApiJwtClaims {
    ApiJwtClaims {
        iss: "centaur-console",
        aud: "centaur-api",
        iat,
        exp: iat + 3600,
        sub: principal_oid,
        capabilities: Capabilities {
            sessions_read: proxy
                .principal_field("sandbox_sessions_read_enabled")
                .as_bool()
                .unwrap_or(false),
            workflows_read: proxy
                .principal_field("sandbox_workflows_read_enabled")
                .as_bool()
                .unwrap_or(false),
            workflows_write: proxy
                .principal_field("sandbox_workflows_write_enabled")
                .as_bool()
                .unwrap_or(false),
        },
        slack: SlackChannels {
            upload_channels: channels.upload,
            download_channels: channels.download,
            history_channels: channels.history,
        },
        sandbox_id: proxy.name.clone(),
        github_user_id,
    }
}

pub(crate) fn append_sandbox_jwt(
    state: &AppState,
    proxy: &ProxyRecord,
    config: &mut Config,
    now: i64,
) -> Result<(), ApiError> {
    let (Some(secret), Some(host), Some(principal_id)) =
        (&state.jwt_secret, &state.console_host, proxy.principal_id)
    else {
        return Ok(());
    };
    let proxy_oid = oid("prx", proxy.id);
    let iat = window_start(&proxy_oid, now, 86_400);
    let claims = SandboxJwtClaims {
        iss: env_default("CENTAUR_SANDBOX_ENTITLEMENTS_JWT_ISSUER", "centaur-console"),
        aud: env_default(
            "CENTAUR_SANDBOX_ENTITLEMENTS_JWT_AUDIENCE",
            "centaur-console-sandbox-entitlements",
        ),
        iat,
        exp: iat + 259_200,
        sub: proxy.name.clone(),
        sandbox_id: proxy.name.clone(),
        proxy_id: proxy_oid,
        principal_id: oid("prn", principal_id),
    };
    config.secrets.push(json!({
        "source": { "type": "control_plane", "value": jwt(&claims, secret)? },
        "inject": { "header": "Authorization", "formatter": "Bearer {{ .Value }}" },
        "rules": [{ "host": host, "methods": ["GET", "POST", "PUT", "PATCH", "DELETE"], "paths": ["/api/v1/sandbox/*"] }]
    }));
    Ok(())
}

fn jwt<T: Serialize>(claims: &T, secret: &str) -> Result<String, ApiError> {
    let mut header = Header::new(Algorithm::HS256);
    header.typ = Some("JWT".to_owned());
    encode(
        &header,
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|_| ApiError::internal("JWT encoding failed"))
}

fn window_start(subject: &str, timestamp: i64, window: i64) -> i64 {
    let offset = (crc32fast::hash(subject.as_bytes()) as i64) % window;
    timestamp - (timestamp - offset).rem_euclid(window)
}

fn env_default(name: &str, default: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

#[cfg(test)]
mod tests {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
    use serde_json::{Value, json};

    use super::{ConsoleSlackChannels, api_jwt_claims, jwt, window_start};
    use crate::models::ProxyRecord;

    fn proxy(name: &str) -> ProxyRecord {
        ProxyRecord {
            id: 7,
            name: name.to_owned(),
            labels: json!({}),
            principal_id: Some(11),
            requester_principal_id: None,
            principal_assigned_at: None,
            requester_principal_assigned_at: None,
            principal_cache_version: None,
            principal: Some(json!({"sandbox_sessions_read_enabled": true})),
            console_user_email: None,
            console_user_id: None,
            slack_history_channel_ids: json!([]),
            has_owner_only_grant: false,
        }
    }

    fn decoded(proxy: &ProxyRecord, github_user_id: Option<&str>) -> Value {
        let claims = api_jwt_claims(
            proxy,
            "prn_test".to_owned(),
            1_700_000_000,
            ConsoleSlackChannels::default(),
            github_user_id.map(str::to_owned),
        );
        let token = jwt(&claims, "test-secret").unwrap();
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_audience(&["centaur-api"]);
        validation.validate_exp = false;
        decode::<Value>(
            &token,
            &DecodingKey::from_secret(b"test-secret"),
            &validation,
        )
        .unwrap()
        .claims
    }

    #[test]
    fn api_jwt_names_the_proxys_own_sandbox() {
        let claims = decoded(&proxy("sandbox-abc"), None);
        assert_eq!(claims["sandbox_id"], "sandbox-abc");
        assert_eq!(claims["sub"], "prn_test");
        assert_eq!(claims["capabilities"]["sessions_read"], true);
    }

    #[test]
    fn api_jwt_carries_the_github_account_only_when_known() {
        let claims = decoded(&proxy("sandbox-abc"), Some("90210001"));
        assert_eq!(claims["github_user_id"], "90210001");
        let claims = decoded(&proxy("sandbox-abc"), None);
        assert!(claims.get("github_user_id").is_none());
    }

    #[test]
    fn windowed_tokens_are_stable_within_a_window() {
        let start = window_start("prn_example", 1_700_000_001, 900);
        assert_eq!(start, window_start("prn_example", start + 899, 900));
    }
}
