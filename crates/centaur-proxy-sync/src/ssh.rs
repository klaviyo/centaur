//! User-scoped SSH grants for the opt-in ephemeral-environment pilot.
use axum::{Json, extract::State, http::HeaderMap};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ApiError, bearer_token, database::load_proxy, models::AppState};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    route: String,
    public_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    principal: String,
    route: String,
    generation: String,
    destination: String,
    username: String,
    host_key: String,
    private_key_file: String,
    expires_at: DateTime<Utc>,
}

pub(crate) async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Request>,
) -> Result<Json<Value>, ApiError> {
    let token = bearer_token(&headers).ok_or_else(ApiError::unauthorized)?;
    let proxy = load_proxy(&state.pool, token)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    let principal_id = proxy.principal_id.ok_or_else(ApiError::unauthorized)?;
    // Channel/workflow requester attribution cannot select a personal SSH key.
    if !matches!(
        proxy.principal_field("kind").as_str(),
        Some("user" | "console_user" | "slack_dm")
    ) {
        return Err(ApiError::unauthorized());
    }
    let path = state
        .ssh_grants_path
        .as_ref()
        .ok_or_else(ApiError::unauthorized)?;
    let data = tokio::fs::read(path)
        .await
        .map_err(|_| ApiError::unauthorized())?;
    let grant = select(
        &data,
        &centaur_console_db::oid("prn", principal_id),
        &request,
        Utc::now(),
    )?;
    let private_key = tokio::fs::read_to_string(&grant.private_key_file)
        .await
        .map_err(|_| ApiError::unauthorized())?;
    let expires_at = std::cmp::min(grant.expires_at, Utc::now() + chrono::Duration::seconds(30));
    Ok(Json(json!({
        "id": format!("{}:{}", grant.route, grant.generation),
        "principal": grant.principal,
        "generation": grant.generation,
        "destination": grant.destination,
        "username": grant.username,
        "host_key": grant.host_key,
        "private_key": {"type": "control_plane", "value": private_key},
        "expires_at": expires_at.to_rfc3339(),
    })))
}

fn select(
    data: &[u8],
    principal: &str,
    request: &Request,
    now: DateTime<Utc>,
) -> Result<Grant, ApiError> {
    let grants: Vec<Grant> = serde_json::from_slice(data).map_err(|_| ApiError::unauthorized())?;
    if request.route.is_empty()
        || request.route.len() > 128
        || request.public_key.len() > 8192
        || !request.public_key.starts_with("ssh-ed25519 ")
    {
        return Err(ApiError::unauthorized());
    }
    let mut matched = grants
        .into_iter()
        .filter(|g| g.principal == principal && g.route == request.route && g.expires_at > now);
    let grant = matched.next().ok_or_else(ApiError::unauthorized)?;
    if matched.next().is_some()
        || grant.generation.is_empty()
        || grant.username.is_empty()
        || grant.host_key.is_empty()
        || grant.private_key_file.is_empty()
    {
        return Err(ApiError::unauthorized());
    }
    let endpoint = url::Url::parse(&grant.destination).map_err(|_| ApiError::unauthorized())?;
    if endpoint.scheme() != "wss"
        || endpoint.host_str().is_none()
        || endpoint.port_or_known_default() != Some(443)
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(ApiError::unauthorized());
    }
    Ok(grant)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record() -> Value {
        json!({"principal":"prn_alice","route":"ee","generation":"one", "destination":"wss://ee.example.com/events/events", "username":"dev","host_key":"ssh-ed25519 fixture","private_key_file":"/run/keys/alice","expires_at":"2099-01-01T00:00:00Z"})
    }
    #[test]
    fn only_exact_personal_grant_is_selected() {
        let request = Request {
            route: "ee".into(),
            public_key: "ssh-ed25519 sandbox-key".into(),
        };
        let bytes = serde_json::to_vec(&vec![record()]).unwrap();
        assert!(select(&bytes, "prn_alice", &request, Utc::now()).is_ok());
        assert!(select(&bytes, "prn_bob", &request, Utc::now()).is_err());
        let other = Request {
            route: "arbitrary-host".into(),
            ..request
        };
        assert!(select(&bytes, "prn_alice", &other, Utc::now()).is_err());
    }
    #[test]
    fn invalid_expired_and_ambiguous_grants_fail_closed() {
        let request = Request {
            route: "ee".into(),
            public_key: "ssh-ed25519 sandbox-key".into(),
        };
        for (field, value) in [
            ("expires_at", "2000-01-01T00:00:00Z"),
            ("destination", "tcp://example.com:22"),
            ("destination", "wss://user@example.com/events"),
            ("generation", ""),
        ] {
            let mut row = record();
            row[field] = json!(value);
            assert!(
                select(
                    &serde_json::to_vec(&vec![row]).unwrap(),
                    "prn_alice",
                    &request,
                    Utc::now()
                )
                .is_err()
            );
        }
        assert!(
            select(
                &serde_json::to_vec(&vec![record(), record()]).unwrap(),
                "prn_alice",
                &request,
                Utc::now()
            )
            .is_err()
        );
    }
}
