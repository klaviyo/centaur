use centaur_console_db::{SecretValues, Subject, models::Config, oid};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    ApiError,
    models::{AppState, ProxyRecord},
    tokens::{append_api_jwt, append_sandbox_jwt},
};

pub(crate) async fn build_config(
    state: &AppState,
    proxy: &ProxyRecord,
    principal_id: i64,
    now: i64,
) -> Result<Config, ApiError> {
    let subject = Subject {
        principal_id,
        principal: proxy.principal.as_ref(),
        console_user_id: proxy.console_user_id,
        console_user_email: proxy.console_user_email.as_deref(),
        slack_history_channel_ids: &proxy.slack_history_channel_ids,
        proxy_labels: &proxy.labels,
    };
    let mut config = centaur_console_db::build_config(
        &state.pool,
        SecretValues::Live(&state.encryption),
        &subject,
        proxy.requester_principal_id,
        state.runlayer_resource_url.as_deref(),
    )
    .await?;
    append_api_jwt(state, proxy, &mut config, now).await?;
    append_sandbox_jwt(state, proxy, &mut config, now)?;
    append_runlayer_user_agent(
        &mut config,
        state.runlayer_resource_url.as_deref(),
        state.runlayer_user_agent.as_deref(),
    );
    Ok(config)
}

pub(crate) fn parse_runlayer_user_agent(raw: &str) -> Result<Option<String>, &'static str> {
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.trim().is_empty() || !raw.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
        return Err("RUNLAYER_USER_AGENT must be a non-empty printable ASCII header value");
    }
    Ok(Some(raw.to_owned()))
}

fn append_runlayer_user_agent(config: &mut Config, resource: Option<&str>, value: Option<&str>) {
    let (Some(resource), Some(value)) = (resource, value) else {
        return;
    };
    // RUNLAYER_MCP_URL is validated at startup. Match only its HTTPS endpoint,
    // leaving model-provider headers and other paths on the tenant untouched.
    let url = url::Url::parse(resource).expect("validated Runlayer resource URL");
    config.secrets.push(json!({
        "source": {"type": "control_plane", "value": value},
        "inject": {"header": "User-Agent"},
        "rules": [{
            "host": url.host_str().expect("validated Runlayer host"),
            "methods": ["GET", "POST", "DELETE"],
            "paths": [url.path()],
        }],
    }));
}

pub(crate) fn config_hash(proxy: &ProxyRecord, config: &Value) -> Result<String, ApiError> {
    let mut payload = config
        .as_object()
        .cloned()
        .ok_or_else(|| ApiError::internal("config was not an object"))?;
    payload.insert(
        "principal".to_owned(),
        proxy
            .principal_id
            .map(|id| Value::String(oid("prn", id)))
            .unwrap_or(Value::Null),
    );
    payload.insert(
        "principal_assigned_at".to_owned(),
        timestamp(proxy.principal_assigned_at),
    );
    payload.insert("proxy_labels".to_owned(), proxy.labels.clone());
    if proxy.requester_principal_id.is_some() {
        payload.insert(
            "requester_principal".to_owned(),
            proxy
                .requester_principal_id
                .map(|id| Value::String(oid("prn", id)))
                .unwrap_or(Value::Null),
        );
        payload.insert(
            "requester_principal_assigned_at".to_owned(),
            timestamp(proxy.requester_principal_assigned_at),
        );
    }
    let canonical = serde_json::to_vec(&Value::Object(payload))
        .map_err(|_| ApiError::internal("config serialization failed"))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
}

fn timestamp(value: Option<DateTime<Utc>>) -> Value {
    value
        .map(|value| Value::String(value.format("%Y-%m-%dT%H:%M:%SZ").to_string()))
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod runlayer_tests {
    use super::*;

    #[test]
    fn user_agent_is_optional_and_rejects_invalid_header_values() {
        assert_eq!(parse_runlayer_user_agent(""), Ok(None));
        assert_eq!(
            parse_runlayer_user_agent("centaur/0.1.0"),
            Ok(Some("centaur/0.1.0".into()))
        );
        for raw in [" ", "centaur\r\nX-Test: injected", "centaur\0", "centaur/é"] {
            assert!(parse_runlayer_user_agent(raw).is_err());
        }
    }

    #[test]
    fn user_agent_injection_is_scoped_and_preserves_existing_credentials() {
        let auth = json!({"inject": {"header": "Authorization"}});
        let mut config = Config::default();
        config.secrets.push(auth.clone());
        append_runlayer_user_agent(
            &mut config,
            Some("https://tenant.example.test/plugins/one/mcp"),
            Some("centaur/0.1.0"),
        );
        assert_eq!(config.secrets[0], auth);
        assert_eq!(
            config.secrets[1],
            json!({
                "source": {"type": "control_plane", "value": "centaur/0.1.0"},
                "inject": {"header": "User-Agent"},
                "rules": [{"host": "tenant.example.test", "methods": ["GET", "POST", "DELETE"], "paths": ["/plugins/one/mcp"]}],
            })
        );
        for (resource, value) in [
            (None, Some("centaur/0.1.0")),
            (Some("https://tenant.example.test/mcp"), None),
        ] {
            let mut disabled = Config::default();
            append_runlayer_user_agent(&mut disabled, resource, value);
            assert!(disabled.secrets.is_empty());
        }
    }
}
