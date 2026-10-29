//! User-scoped SSH grants for the opt-in ephemeral-environment pilot.
use axum::{Json, extract::State, http::HeaderMap};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;

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
    let assigned_principal = proxy.principal_id.ok_or_else(ApiError::unauthorized)?;
    // The ingress derives this binding from the verified actor on each execute,
    // before the sandbox receives input. Never take a requester from the tool.
    let (principal_id, binding) = match proxy.principal_field("kind").as_str() {
        Some("user" | "console_user" | "slack_dm") => (assigned_principal, None),
        Some("slack_channel") => {
            let requester = proxy
                .requester_principal_id
                .ok_or_else(ApiError::unauthorized)?;
            let assigned_at = proxy
                .requester_principal_assigned_at
                .ok_or_else(ApiError::unauthorized)?;
            let personal: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM principals WHERE id = $1 AND kind = 'slack_dm' \
                 AND NULLIF(slack_user_id, '') IS NOT NULL AND NULLIF(slack_team_id, '') IS NOT NULL)",
            )
            .bind(requester)
            .fetch_one(&state.pool)
            .await
            .map_err(centaur_console_db::Error::Database)?;
            if !personal {
                return Err(ApiError::unauthorized());
            }
            (
                requester,
                Some(format!("{}:{}", proxy.id, assigned_at.to_rfc3339())),
            )
        }
        _ => return Err(ApiError::unauthorized()),
    };
    validate_request(&request)?;
    let (grant, private_key) = match managed_grant(&state, principal_id, &request).await? {
        Some(grant) => grant,
        None => {
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
            let key = tokio::fs::read_to_string(&grant.private_key_file)
                .await
                .map_err(|_| ApiError::unauthorized())?;
            (grant, key)
        }
    };
    let expires_at = std::cmp::min(grant.expires_at, Utc::now() + chrono::Duration::seconds(30));
    // A lease from an earlier requester binding must not survive reassignment,
    // including switching away and back to the same user between renewals.
    let generation = match binding {
        Some(binding) => format!("{}:{binding}", grant.generation),
        None => grant.generation.clone(),
    };
    Ok(Json(json!({
        "id": format!("{}:{}", grant.route, grant.generation),
        "principal": grant.principal,
        "generation": generation,
        "destination": grant.destination,
        "username": grant.username,
        "host_key": grant.host_key,
        "private_key": {"type": "control_plane", "value": private_key},
        "expires_at": expires_at.to_rfc3339(),
    })))
}

// Resolve ownership through Console's authenticated account or its verified
// Slack SSO identity. Never match email, channel membership, or editable labels.
async fn managed_grant(
    state: &AppState,
    principal_id: i64,
    request: &Request,
) -> Result<Option<(Grant, String)>, ApiError> {
    let row = sqlx::query(
        "SELECT c.*, u.status AS owner_status FROM ssh_connections c \
         JOIN users u ON u.id = c.user_id JOIN principals p ON p.id = $1 \
         WHERE (p.kind = 'console_user' AND p.console_user_id = u.id) \
            OR (p.kind = 'slack_dm' AND EXISTS ( \
                SELECT 1 FROM user_identities i WHERE i.user_id = u.id AND i.provider = 'slack' \
                AND i.subject = p.slack_user_id AND i.team_id = p.slack_team_id \
                AND NULLIF(i.team_id, '') IS NOT NULL \
                AND (SELECT COUNT(DISTINCT (s.subject, s.team_id)) FROM user_identities s \
                     WHERE s.user_id = u.id AND s.provider = 'slack' \
                     AND NULLIF(s.subject, '') IS NOT NULL AND NULLIF(s.team_id, '') IS NOT NULL) = 1))",
    )
    .bind(principal_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(centaur_console_db::Error::Database)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let expiry: chrono::NaiveDateTime = row.get("expires_at");
    let expires_at = expiry.and_utc();
    // A saved but expired/disconnected connection must not resurrect a file grant.
    if request.route != "ee"
        || row.get::<String, _>("owner_status") != "active"
        || row
            .get::<Option<chrono::NaiveDateTime>, _>("revoked_at")
            .is_some()
        || expires_at <= Utc::now()
    {
        return Err(ApiError::unauthorized());
    }
    let encrypted: String = row
        .get::<Option<String>, _>("private_key")
        .ok_or_else(ApiError::unauthorized)?;
    let private_key = state
        .encryption
        .decrypt(&encrypted)
        .map_err(|_| ApiError::unauthorized())?;
    if private_key.is_empty() {
        return Err(ApiError::unauthorized());
    }
    let grant = Grant {
        principal: centaur_console_db::oid("prn", principal_id),
        route: "ee".to_owned(),
        generation: row.get("generation"),
        destination: row.get("destination"),
        username: row.get("username"),
        host_key: row.get("host_key"),
        private_key_file: String::new(),
        expires_at,
    };
    validate_destination(&grant.destination)?;
    Ok(Some((grant, private_key)))
}

fn validate_request(request: &Request) -> Result<(), ApiError> {
    if request.route.is_empty()
        || request.route.len() > 128
        || request.public_key.len() > 8192
        || !request.public_key.starts_with("ssh-ed25519 ")
    {
        return Err(ApiError::unauthorized());
    }
    Ok(())
}

fn select(
    data: &[u8],
    principal: &str,
    request: &Request,
    now: DateTime<Utc>,
) -> Result<Grant, ApiError> {
    let grants: Vec<Grant> = serde_json::from_slice(data).map_err(|_| ApiError::unauthorized())?;
    validate_request(request)?;
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
    validate_destination(&grant.destination)?;
    Ok(grant)
}

fn validate_destination(destination: &str) -> Result<(), ApiError> {
    let endpoint = url::Url::parse(destination).map_err(|_| ApiError::unauthorized())?;
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
    Ok(())
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
