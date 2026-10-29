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
    #[serde(default)]
    destination: Option<String>,
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
    #[serde(default)]
    tls_identity: bool,
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
        "tls_identity": grant.tls_identity,
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
    let (destination, username, host_key, tls_identity) =
        if let Some(target) = request.destination.as_deref() {
            let (destination, policy) = target_policy(&state.ssh_targets, target)?;
            (destination, policy.username.clone(), String::new(), true)
        } else {
            // Retain existing fixed-target records until the owner converts to a
            // reusable key. New key-only records require a URL on each call.
            (
                row.get::<Option<String>, _>("destination")
                    .ok_or_else(ApiError::unauthorized)?,
                row.get::<Option<String>, _>("username")
                    .ok_or_else(ApiError::unauthorized)?,
                row.get::<Option<String>, _>("host_key")
                    .ok_or_else(ApiError::unauthorized)?,
                false,
            )
        };
    let grant = Grant {
        principal: centaur_console_db::oid("prn", principal_id),
        route: "ee".to_owned(),
        generation: row.get("generation"),
        destination,
        username,
        host_key,
        tls_identity,
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
    if request
        .destination
        .as_deref()
        .is_some_and(|target| target != grant.destination)
    {
        return Err(ApiError::unauthorized());
    }
    Ok(grant)
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetPolicy {
    host: String,
    path: String,
    username: String,
}

pub(crate) fn target_policies(raw: Option<&str>) -> Result<Vec<TargetPolicy>, String> {
    let policies: Vec<TargetPolicy> = serde_json::from_str(raw.unwrap_or("[]"))
        .map_err(|_| "CENTAUR_EE_SSH_TARGETS must be a JSON array".to_owned())?;
    for p in &policies {
        let host = p.host.strip_prefix("*.").unwrap_or(&p.host);
        if host.is_empty()
            || host.starts_with('.')
            || host.ends_with('.')
            || !host
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
            || !p.path.starts_with('/')
            || p.path.contains(['?', '#', '*'])
            || p.username.is_empty()
            || p.username.len() > 64
            || !p
                .username
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        {
            return Err("invalid SSH target policy: use an exact/lowercase wildcard host, exact path, and SSH username".to_owned());
        }
    }
    Ok(policies)
}

fn target_policy<'a>(
    policies: &'a [TargetPolicy],
    destination: &str,
) -> Result<(String, &'a TargetPolicy), ApiError> {
    if destination.len() > 2048 {
        return Err(ApiError::unauthorized());
    }
    validate_destination(destination)?;
    let url = url::Url::parse(destination).map_err(|_| ApiError::unauthorized())?;
    let host = url.host_str().ok_or_else(ApiError::unauthorized)?;
    let mut matches = policies.iter().filter(|p| {
        let allowed = match p.host.strip_prefix("*.") {
            Some(suffix) => host
                .strip_suffix(suffix)
                .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1),
            None => host == p.host,
        };
        allowed && url.path() == p.path
    });
    let policy = matches.next().ok_or_else(ApiError::unauthorized)?;
    if matches.next().is_some() {
        return Err(ApiError::unauthorized());
    }
    Ok((url.to_string(), policy))
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
    #[test]
    fn dynamic_targets_are_restricted_by_deployment_policy() {
        let policies = target_policies(Some(
            r#"[{"host":"*.dev.example.com","path":"/events/events","username":"dev"}]"#,
        ))
        .unwrap();
        for target in [
            "wss://one.dev.example.com/events/events",
            "wss://two.dev.example.com/events/events",
        ] {
            assert!(target_policy(&policies, target).is_ok());
        }
        for target in [
            "wss://dev.example.com/events/events",
            "wss://one.dev.example.com.evil.test/events/events",
            "wss://evildev.example.com/events/events",
            "wss://one.dev.example.com/other",
            "wss://one.dev.example.com:8443/events/events",
            "wss://user:secret@one.dev.example.com/events/events",
            "wss://one.dev.example.com/events/events?token=secret",
            "tcp://one.dev.example.com:22",
            "wss://169.254.169.254/events/events",
        ] {
            assert!(target_policy(&policies, target).is_err(), "{target}");
        }
        assert!(target_policy(&[], "wss://one.dev.example.com/events/events").is_err());
        let ambiguous = vec![policies[0].clone(), policies[0].clone()];
        assert!(target_policy(&ambiguous, "wss://one.dev.example.com/events/events").is_err());
        assert!(
            target_policies(Some(
                r#"[{"host":"*","path":"/events/events","username":"dev"}]"#
            ))
            .is_err()
        );
        assert!(target_policies(Some("not-json")).is_err());
    }
    fn record() -> Value {
        json!({"principal":"prn_alice","route":"ee","generation":"one", "destination":"wss://ee.example.com/events/events", "username":"dev","host_key":"ssh-ed25519 fixture","private_key_file":"/run/keys/alice","expires_at":"2099-01-01T00:00:00Z"})
    }
    #[test]
    fn only_exact_personal_grant_is_selected() {
        let request = Request {
            route: "ee".into(),
            public_key: "ssh-ed25519 sandbox-key".into(),
            destination: None,
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
            destination: None,
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
