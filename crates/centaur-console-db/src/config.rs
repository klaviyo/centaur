use std::collections::HashMap;

use active_record_encryption::ActiveRecordEncryption;
use serde_json::{Map, Value, json};
use sqlx::PgPool;

use crate::{
    Error, Result,
    conflicts::{suppress, targets_host, writes_header_on_host},
    credentials::load_credentials,
    models::{
        Config, Credential, CredentialData, CredentialKind, PostgresSetting, RequestRule,
        SecretSource, StaticData,
    },
    oid::oid,
};

/// How stored secret values appear in a built configuration.
#[derive(Clone, Copy, Debug)]
pub enum SecretValues<'a> {
    /// Decrypted: the configuration a proxy receives.
    Live(&'a ActiveRecordEncryption),
    /// Every inline value replaced with `[redacted]`, decrypting nothing.
    Redacted,
}

/// Stand-in for an inline secret value, matching Console's redaction.
pub const REDACTED: &str = "[redacted]";

/// The principal a configuration is built for, with the attributes Postgres
/// session settings can reference.
#[derive(Clone, Copy, Debug)]
pub struct Subject<'a> {
    pub principal_id: i64,
    /// The principal's row as `to_jsonb(principals)`.
    pub principal: Option<&'a Value>,
    pub console_user_id: Option<i64>,
    pub console_user_email: Option<&'a str>,
    /// JSON array of the channels whose history the principal may read.
    pub slack_history_channel_ids: &'a Value,
    /// Labels of the proxy the configuration is for: `{}` without one.
    pub proxy_labels: &'a Value,
}

impl Subject<'_> {
    fn principal_field(&self, field: &str) -> &Value {
        self.principal
            .and_then(|principal| principal.get(field))
            .unwrap_or(&Value::Null)
    }
}

/// Builds the configuration served for `subject`'s credentials and, when
/// given, the always-available OAuth wrappers granted to `requester_id`.
/// Credentials a stronger grant would overwrite are withheld. Tokens the
/// caller mints are not included: append them to `secrets`.
pub async fn build_config(
    pool: &PgPool,
    values: SecretValues<'_>,
    subject: &Subject<'_>,
    requester_id: Option<i64>,
) -> Result<Config> {
    Ok(build(pool, values, subject, requester_id, false).await?.0)
}

/// [`build_config`], together with the GitHub account id
/// (`broker_credentials.provider_subject`) of the human whose OAuth token that
/// same configuration injects as `Authorization` on `api.github.com`, or
/// `None`.
///
/// The account is `Some` only under the rules of `github_user_id_of` below.
///
/// Both come from one resolved credential list, so the account named is
/// exactly the one behind the token the proxy is given. Deriving it from a
/// second load could name a token the delivered configuration no longer
/// contains (a grant, token or ownership change between the two reads), and
/// callers compare it with the author GitHub reports for a PR created with
/// that token.
///
/// Every configuration built here ends with one guard: a trailing `secrets`
/// transform, the very last element of `transforms`, holding a single secret.
/// In a managed proxy the control-plane `secrets` run first and then each
/// transform in list order, so a transform here runs after everything else,
/// including `oauth_token`, `aws_auth`, `gcp_*` and `hmac` transforms that
/// could overwrite `Authorization`. The guard is therefore the last writer of
/// `Authorization` for `POST api.github.com //repos/*/*/pulls` (see
/// [`PR_CREATE_GUARD_PATH`]): it replaces the
/// placeholder [`PR_OAUTH_PLACEHOLDER`] and is `require: true`, so the proxy
/// refuses the request outright if an earlier secret or transform has already
/// written that header. (Placed in `secrets` instead, a later transform would
/// overwrite it and the guard would fail open.) Callers must append nothing
/// to `transforms` after this build. api-rs's pull-request create call sends
/// that placeholder instead of the generic `GITHUB_TOKEN` one, so it
/// authenticates only with the attested OAuth credential in the configuration
/// in force at that moment: if the credential has since become a personal
/// access token, or something injects or rewrites `Authorization`, the call
/// fails closed instead of going out under another token.
///
/// The guard is in every configuration, not only attested ones, because a
/// guard that exists only while OAuth is attested could vanish in the very
/// change that adds a direct injector. Its source is the attested credential's
/// own delivered source; with no attested account it is a sentinel that is not
/// a credential, which GitHub rejects.
pub async fn build_config_with_github_user_id(
    pool: &PgPool,
    values: SecretValues<'_>,
    subject: &Subject<'_>,
    requester_id: Option<i64>,
) -> Result<(Config, Option<String>)> {
    build(pool, values, subject, requester_id, true).await
}

/// The placeholder api-rs's pull-request create call sends as its bearer
/// token, replaced by the proxy only with the attested OAuth credential's
/// token and only on that one operation.
pub const PR_OAUTH_PLACEHOLDER: &str = "CENTAUR_PR_OAUTH";

/// The path the guard rule matches: `//repos/*/*/pulls`, with a literal
/// leading double slash. api-rs's one create call sends to exactly that path,
/// and only that call does.
///
/// A guard on the ordinary `/repos/*/*/pulls` would also refuse a sandbox's
/// own REST PR creation with `GITHUB_TOKEN` (`require: true` fails any request
/// whose `Authorization` was already written). In the pinned iron-proxy
/// 0.56.0 rules match on the raw request path while the request forwarded to
/// GitHub is normalized to `/repos/{owner}/{repo}/pulls`, so the marker routes
/// api-rs's call through the guard and leaves ordinary creation untouched.
/// That depends on this proxy version's matcher and forwarder, so the offline
/// proof (`d1-oauth-placeholder-proof/`) must be rerun on any iron-proxy bump.
pub const PR_CREATE_GUARD_PATH: &str = "//repos/*/*/pulls";

/// The guard's source when no account is attested: a fixed non-credential
/// string, so the guard is present and fails closed without any token to leak.
static UNATTESTED_OAUTH_SOURCE: std::sync::LazyLock<Value> = std::sync::LazyLock::new(
    || json!({"type": "control_plane", "value": "centaur-unattested-oauth"}),
);

async fn build(
    pool: &PgPool,
    values: SecretValues<'_>,
    subject: &Subject<'_>,
    requester_id: Option<i64>,
    pr_oauth_guard: bool,
) -> Result<(Config, Option<String>)> {
    let credentials =
        effective_credentials(pool, values, subject.principal_id, requester_id).await?;
    let attested = attested_github_credential(&credentials);
    let github_user_id = attested.map(|(_, account)| account.to_owned());

    let mut config = Config::default();
    for credential in credentials
        .iter()
        .filter(|credential| credential.kind == CredentialKind::Static)
    {
        let CredentialData::Static(data) = &credential.data else {
            return Err(mismatched_data(credential));
        };
        if let Some(source) = source_value(&credential.sources[0], values)? {
            let mut entry = Map::new();
            entry.insert("source".to_owned(), source);
            entry.insert("rules".to_owned(), proxy_rules(&credential.rules));
            insert_present(&mut entry, "inject", data.inject_config.as_ref());
            insert_present(&mut entry, "replace", data.replace_config.as_ref());
            config.secrets.push(Value::Object(entry));
        }
    }

    for kind in [
        CredentialKind::GcpAuth,
        CredentialKind::GcpIdToken,
        CredentialKind::AwsAuth,
        CredentialKind::Hmac,
    ] {
        for credential in credentials
            .iter()
            .filter(|credential| credential.kind == kind)
        {
            config.transforms.push(transform(credential, values)?);
        }
    }
    let oauth: Vec<Value> = credentials
        .iter()
        .filter(|credential| credential.kind == CredentialKind::OauthToken)
        .map(|credential| oauth_entry(credential, values))
        .collect::<Result<_>>()?;
    if !oauth.is_empty() {
        config
            .transforms
            .push(json!({ "name": "oauth_token", "config": { "tokens": oauth } }));
    }
    append_postgres(subject, &credentials, values, &mut config)?;
    if pr_oauth_guard {
        // The attested credential's own delivered source, or, when none is
        // attested, a sentinel that is not a credential.
        let source = match attested {
            Some((credential, _)) => source_value(&credential.sources[0], values)?,
            None => None,
        }
        .unwrap_or_else(|| UNATTESTED_OAUTH_SOURCE.clone());
        let guard = json!({
            "source": source,
            "replace": {
                "proxy_value": PR_OAUTH_PLACEHOLDER,
                "match_headers": ["Authorization"],
                "require": true,
            },
            "rules": [{
                "host": GITHUB_API_HOST,
                "methods": ["POST"],
                "paths": [PR_CREATE_GUARD_PATH],
            }],
        });
        config
            .transforms
            .push(json!({ "name": "secrets", "config": { "secrets": [guard] } }));
    }
    Ok((config, github_user_id))
}

/// The credentials a proxy built for `principal_id` (and `requester_id`)
/// delivers: those with something to deliver, minus every one a stronger grant
/// would overwrite.
async fn effective_credentials(
    pool: &PgPool,
    values: SecretValues<'_>,
    principal_id: i64,
    requester_id: Option<i64>,
) -> Result<Vec<Credential>> {
    let loaded = load_credentials(pool, principal_id, requester_id).await?;
    let mut credentials = Vec::with_capacity(loaded.len());
    for credential in loaded {
        let deliverable = if credential.kind == CredentialKind::Static {
            match credential.sources.first() {
                Some(source) => source_value(source, values)?.is_some(),
                None => false,
            }
        } else {
            true
        };
        if deliverable {
            credentials.push(credential);
        }
    }
    suppress(&mut credentials);
    Ok(credentials)
}

/// The host whose `Authorization` header carries a person's GitHub token.
const GITHUB_API_HOST: &str = "api.github.com";

/// The GitHub account id (`broker_credentials.provider_subject`) of the human
/// whose OAuth token the proxy would inject as `Authorization` on
/// `api.github.com`, or `None`.
///
/// This is the identity a pull request created through that proxy is
/// authored as, so callers may compare it with the author GitHub reports.
/// It is `Some` only when exactly one delivered credential writes that header
/// on that host and it is a static secret fed by a token-broker credential of
/// a `github` OAuth app that:
///
/// - records a numeric subject, which is the GitHub user id Console stores for
///   an app that is neither shared nor owner-only (an owner-only connection
///   stores `owner-<console user>` instead, so it names no GitHub account);
/// - is not shared, so one connection does not serve many people;
/// - was connected by the acting human (the requester, else the principal),
///   so the token is theirs and not another person's that a channel or role
///   grant happens to deliver.
///
/// Any other shape (no credential, a non-OAuth token, a competing credential,
/// a missing or non-numeric subject) is `None`, never a guess.
#[cfg(test)]
fn github_user_id_of(credentials: &[Credential]) -> Option<String> {
    attested_github_credential(credentials).map(|(_, account)| account.to_owned())
}

/// The credential whose token the proxy would inject for the GitHub API, and
/// its GitHub account, when [`github_user_id_of`]'s rules attest one.
///
/// Beyond the owner, subject and sharing rules, the credential must be the
/// canonical `github_token` shape (a placeholder replacement of
/// `GITHUB_TOKEN` in `Authorization`, no direct injection) whose own rules
/// already cover `POST api.github.com /repos/*/*/pulls`, so attesting it never
/// widens what its token may do. And nothing else that rewrites headers may
/// target `api.github.com`: no `oauth_token`, `hmac`, `aws_auth`, `gcp_auth` or
/// `gcp_id_token` transform.
fn attested_github_credential(credentials: &[Credential]) -> Option<(&Credential, &str)> {
    let rewrites_headers = |credential: &Credential| {
        !matches!(
            credential.kind,
            CredentialKind::Static | CredentialKind::PgDsn
        ) && targets_host(credential, GITHUB_API_HOST)
    };
    if credentials.iter().any(rewrites_headers) {
        return None;
    }
    let mut serving = credentials
        .iter()
        .filter(|credential| writes_header_on_host(credential, "authorization", GITHUB_API_HOST));
    let credential = serving.next()?;
    if serving.next().is_some() {
        return None;
    }
    let CredentialData::Static(data) = &credential.data else {
        return None;
    };
    if !is_canonical_github_replace(data) || !rules_cover_pr_create(&credential.rules) {
        return None;
    }
    // A static credential delivers its first source, as `build_config` does.
    let source = credential.sources.first()?;
    if source.source_type != "token_broker"
        || source.broker_provider.as_deref() != Some("github")
        || source.broker_owner_only != Some(false)
        || source.broker_shared != Some(false)
        || source.broker_owned_by_actor != Some(true)
    {
        return None;
    }
    let account = source
        .broker_provider_subject
        .as_deref()
        .map(str::trim)
        .filter(|subject| !subject.is_empty() && subject.bytes().all(|b| b.is_ascii_digit()))?;
    Some((credential, account))
}

/// Console's canonical `github_token` replacement (`CredentialProfiles::
/// GithubToken::REPLACE_CONFIG`, also stored without `require`), and no direct
/// injection.
fn is_canonical_github_replace(data: &StaticData) -> bool {
    if data.inject_config.as_ref().is_some_and(present) {
        return false;
    }
    let Some(replace) = data.replace_config.as_ref() else {
        return false;
    };
    *replace
        == json!({
            "proxy_value": "GITHUB_TOKEN",
            "match_headers": ["Authorization"],
            "require": false,
        })
        || *replace
            == json!({
                "proxy_value": "GITHUB_TOKEN",
                "match_headers": ["Authorization"],
            })
}

/// Whether one of `rules` covers `POST https://api.github.com/repos/*/*/pulls`:
/// a rule for the API host that allows `POST` (or any method) and that path (or
/// any path). Path patterns are recognized only in the few forms known to
/// cover it; an unfamiliar pattern is not assumed to.
fn rules_cover_pr_create(rules: &[RequestRule]) -> bool {
    rules.iter().any(|rule| {
        rule.host.as_deref().is_some_and(|host| {
            host.trim()
                .trim_end_matches('.')
                .eq_ignore_ascii_case(GITHUB_API_HOST)
        }) && (rule.http_methods.is_empty()
            || rule
                .http_methods
                .iter()
                .any(|method| method.trim().eq_ignore_ascii_case("POST")))
            && (rule.paths.is_empty()
                || rule.paths.iter().any(|path| {
                    matches!(
                        path.trim(),
                        "/repos/*/*/pulls" | "/repos/*/*/*" | "/repos/**" | "/**" | "**"
                    )
                }))
    })
}

pub(crate) fn source_value(
    source: &SecretSource,
    values: SecretValues<'_>,
) -> Result<Option<Value>> {
    if source.source_type == "token_broker" {
        // A credential that has not minted an access token yet delivers nothing.
        let Some(raw) = source.broker_access_token.as_deref() else {
            return Ok(None);
        };
        let value = match values {
            SecretValues::Live(encryption) => encryption.decrypt(raw)?,
            // Without decrypting, any stored token counts as deliverable.
            SecretValues::Redacted => REDACTED.to_owned(),
        };
        if value.is_empty() {
            return Ok(None);
        }
        return Ok(Some(json!({ "type": "control_plane", "value": value })));
    }

    let mut result = source.config.clone();
    result.insert("type".to_owned(), Value::String(source.source_type.clone()));
    if source.source_type == "control_plane" {
        let raw = source
            .secret
            .as_deref()
            .ok_or_else(|| Error::InvalidData("control_plane source has no secret".to_owned()))?;
        let value = match values {
            SecretValues::Live(encryption) => encryption.decrypt(raw)?,
            SecretValues::Redacted => REDACTED.to_owned(),
        };
        result.insert("value".to_owned(), Value::String(value));
    }
    Ok(Some(Value::Object(result)))
}

fn proxy_rules(rules: &[RequestRule]) -> Value {
    Value::Array(
        rules
            .iter()
            .map(|rule| {
                let mut output = Map::new();
                insert_nonempty_string(&mut output, "host", rule.host.as_deref());
                insert_nonempty_string(&mut output, "cidr", rule.cidr.as_deref());
                insert_nonempty_vec(&mut output, "methods", &rule.http_methods);
                insert_nonempty_vec(&mut output, "paths", &rule.paths);
                Value::Object(output)
            })
            .collect(),
    )
}

fn transform(credential: &Credential, values: SecretValues<'_>) -> Result<Value> {
    let rules = proxy_rules(&credential.rules);
    match &credential.data {
        CredentialData::GcpAuth(data) => {
            let mut config = Map::new();
            if let Some(source) = credential.sources.first()
                && let Some(value) = source_value(source, values)?
            {
                config.insert("keyfile".to_owned(), value);
            }
            insert_present(
                &mut config,
                "credentials_provider",
                data.credentials_provider.as_ref(),
            );
            insert_nonempty_string(&mut config, "subject", data.subject.as_deref());
            config.insert("scopes".to_owned(), json!(data.scopes));
            config.insert("rules".to_owned(), rules);
            Ok(json!({ "name": "gcp_auth", "config": config }))
        }
        CredentialData::GcpIdToken(data) => {
            let source = credential
                .sources
                .first()
                .ok_or_else(|| Error::InvalidData("gcp_id_token source missing".to_owned()))?;
            let mut config = Map::new();
            config.insert(
                "keyfile".to_owned(),
                source_value(source, values)?.ok_or_else(|| {
                    Error::InvalidData("gcp_id_token source unavailable".to_owned())
                })?,
            );
            config.insert("audience".to_owned(), Value::String(data.audience.clone()));
            config.insert("rules".to_owned(), rules);
            insert_nonempty_string(&mut config, "header", data.header.as_deref());
            Ok(json!({ "name": "gcp_id_token", "config": config }))
        }
        CredentialData::AwsAuth(data) => {
            let mut config = Map::new();
            for source in &credential.sources {
                if let Some(role) = source.role.as_deref()
                    && let Some(value) = source_value(source, values)?
                {
                    config.insert(role.to_owned(), value);
                }
            }
            insert_nonempty_vec(&mut config, "allowed_regions", &data.allowed_regions);
            insert_nonempty_vec(&mut config, "allowed_services", &data.allowed_services);
            config.insert("rules".to_owned(), rules);
            Ok(json!({ "name": "aws_auth", "config": config }))
        }
        CredentialData::Hmac(data) => {
            let mut credentials = Map::new();
            for source in &credential.sources {
                if let Some(role) = source.role.as_deref()
                    && let Some(value) = source_value(source, values)?
                {
                    credentials.insert(role.to_owned(), value);
                }
            }
            let mut config = json!({
                "credentials": credentials,
                "timestamp": { "format": data.timestamp_format },
                "signature": {
                    "algorithm": data.signature_algorithm,
                    "key_encoding": data.signature_key_encoding,
                    "output_encoding": data.signature_output_encoding,
                    "message": data.signature_message
                },
                "headers": data.headers,
                "rules": rules
            });
            if data.allow_chunked_body {
                config["allow_chunked_body"] = Value::Bool(true);
            }
            Ok(json!({ "name": "hmac_sign", "config": config }))
        }
        _ => Err(mismatched_data(credential)),
    }
}

fn oauth_entry(credential: &Credential, values: SecretValues<'_>) -> Result<Value> {
    let CredentialData::OauthToken(data) = &credential.data else {
        return Err(mismatched_data(credential));
    };
    let mut entry = Map::new();
    entry.insert("grant".to_owned(), Value::String(data.grant.clone()));
    entry.insert(
        "token_endpoint".to_owned(),
        Value::String(data.token_endpoint.clone()),
    );
    let mut headers = Map::new();
    for source in &credential.sources {
        let Some(role) = source.role.as_deref() else {
            continue;
        };
        let Some(value) = source_value(source, values)? else {
            continue;
        };
        if source.role_kind.as_deref() == Some("endpoint_header") {
            headers.insert(role.to_owned(), value);
        } else {
            entry.insert(role.to_owned(), value);
        }
    }
    insert_nonempty_string(&mut entry, "audience", data.audience.as_deref());
    insert_nonempty_vec(&mut entry, "scopes", &data.scopes);
    insert_nonempty_string(&mut entry, "header", data.header.as_deref());
    insert_nonempty_string(&mut entry, "value_prefix", data.value_prefix.as_deref());
    if !headers.is_empty() {
        entry.insert("token_endpoint_headers".to_owned(), Value::Object(headers));
    }
    entry.insert("rules".to_owned(), proxy_rules(&credential.rules));
    Ok(Value::Object(entry))
}

fn append_postgres(
    subject: &Subject<'_>,
    credentials: &[Credential],
    values: SecretValues<'_>,
    config: &mut Config,
) -> Result<()> {
    let mut positions: HashMap<String, usize> = HashMap::new();
    for credential in credentials
        .iter()
        .filter(|credential| credential.kind == CredentialKind::PgDsn)
    {
        let CredentialData::PgDsn(data) = &credential.data else {
            return Err(mismatched_data(credential));
        };
        let Some(source) = credential.sources.first() else {
            continue;
        };
        let Some(dsn) = source_value(source, values)? else {
            continue;
        };
        let mut entry = Map::new();
        entry.insert("id".to_owned(), Value::String(oid("pgs", credential.id)));
        entry.insert(
            "foreign_id".to_owned(),
            Value::String(data.foreign_id.clone()),
        );
        entry.insert("database".to_owned(), Value::String(data.database.clone()));
        entry.insert("dsn".to_owned(), dsn);
        insert_nonempty_string(&mut entry, "role", data.role.as_deref());
        let settings = postgres_settings(subject, &data.settings);
        if !settings.is_empty() {
            entry.insert("settings".to_owned(), Value::Array(settings));
        }
        if let Some(index) = positions.get(&data.database).copied() {
            config.postgres[index] = Value::Object(entry);
        } else {
            positions.insert(data.database.clone(), config.postgres.len());
            config.postgres.push(Value::Object(entry));
        }
    }
    Ok(())
}

fn postgres_settings(subject: &Subject<'_>, settings: &[PostgresSetting]) -> Vec<Value> {
    settings
        .iter()
        .filter_map(|setting| {
            let name = setting.name.trim();
            if name.is_empty() {
                return None;
            }
            let value = if let Some(reference) = &setting.value_from {
                if let Some(label) = nonempty(reference.principal_label.as_deref()) {
                    subject
                        .principal_field("labels")
                        .get(label)
                        .map(value_to_string)
                        .unwrap_or_default()
                } else if let Some(label) = nonempty(reference.proxy_label.as_deref()) {
                    subject
                        .proxy_labels
                        .get(label)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned()
                } else {
                    reference
                        .principal_field
                        .as_deref()
                        .map(|field| principal_setting(subject, field))
                        .unwrap_or_default()
                }
            } else {
                setting
                    .value
                    .as_ref()
                    .map(value_to_string)
                    .unwrap_or_default()
            };
            Some(json!({ "name": name, "value": value }))
        })
        .collect()
}

fn principal_setting(subject: &Subject<'_>, field: &str) -> String {
    match field {
        "id" => oid("prn", subject.principal_id),
        "console_user_id" => subject
            .console_user_id
            .map(|id| oid("usr", id))
            .unwrap_or_default(),
        "console_user_email" => subject.console_user_email.unwrap_or_default().to_owned(),
        "slack_history_channel_ids" => subject.slack_history_channel_ids.to_string(),
        other => value_to_string(subject.principal_field(other)),
    }
}

fn insert_present(target: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(value) = value
        && present(value)
    {
        target.insert(key.to_owned(), value.clone());
    }
}

fn insert_nonempty_string(target: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = nonempty(value) {
        target.insert(key.to_owned(), Value::String(value.to_owned()));
    }
}

fn insert_nonempty_vec<T: serde::Serialize>(
    target: &mut Map<String, Value>,
    key: &str,
    value: &[T],
) {
    if !value.is_empty() {
        target.insert(key.to_owned(), json!(value));
    }
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

fn present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        _ => true,
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn mismatched_data(credential: &Credential) -> Error {
    Error::InvalidData(format!(
        "credential {} data does not match {:?}",
        credential.id, credential.kind
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Subject, github_user_id_of, postgres_settings, proxy_rules};
    use crate::{
        conflicts::suppress,
        models::{
            Credential, CredentialData, CredentialKind, PostgresSetting, RequestRule, SecretSource,
            StaticData,
        },
    };

    fn rule(host: &str) -> RequestRule {
        RequestRule {
            host: Some(host.to_owned()),
            cidr: None,
            http_methods: vec![],
            paths: vec![],
        }
    }

    fn source(source_type: &str, provider: Option<&str>, subject: Option<&str>) -> SecretSource {
        SecretSource {
            source_type: source_type.to_owned(),
            config: serde_json::Map::new(),
            secret: None,
            broker_access_token: Some("token".to_owned()),
            broker_provider: provider.map(str::to_owned),
            broker_provider_subject: subject.map(str::to_owned),
            broker_owner_only: Some(false),
            broker_shared: Some(false),
            broker_owned_by_actor: Some(true),
            role: None,
            role_kind: None,
        }
    }

    fn canonical_replace() -> serde_json::Value {
        json!({"proxy_value": "GITHUB_TOKEN", "match_headers": ["Authorization"], "require": false})
    }

    fn github_token(id: i64, priority: i32, host: &str) -> Credential {
        Credential {
            kind: CredentialKind::Static,
            id,
            priority,
            // Console's canonical `github_token` shape.
            data: CredentialData::Static(StaticData {
                inject_config: None,
                replace_config: Some(canonical_replace()),
            }),
            sources: vec![source("token_broker", Some("github"), Some("90210001"))],
            rules: vec![rule(host)],
        }
    }

    #[test]
    fn a_brokered_github_token_on_the_api_host_names_its_account() {
        assert_eq!(
            github_user_id_of(&[github_token(1, 100, "api.github.com")]),
            Some("90210001".to_owned())
        );
        // The canonical replacement is also stored without `require`.
        let mut without_require = github_token(1, 100, "api.github.com");
        without_require.data = CredentialData::Static(StaticData {
            inject_config: None,
            replace_config: Some(
                json!({"proxy_value": "GITHUB_TOKEN", "match_headers": ["Authorization"]}),
            ),
        });
        assert_eq!(
            github_user_id_of(&[without_require]),
            Some("90210001".to_owned())
        );
        // A wildcard host rule is not "a rule for api.github.com": not
        // attested, which errs toward no claim.
        assert_eq!(
            github_user_id_of(&[github_token(1, 100, "*.github.com")]),
            None
        );
    }

    fn static_with(id: i64, data: StaticData, rules: Vec<RequestRule>) -> Credential {
        Credential {
            data: CredentialData::Static(data),
            rules,
            ..github_token(id, 100, "api.github.com")
        }
    }

    fn api_rule(methods: &[&str], paths: &[&str]) -> RequestRule {
        RequestRule {
            host: Some("api.github.com".to_owned()),
            cidr: None,
            http_methods: methods.iter().map(|m| (*m).to_owned()).collect(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    #[test]
    fn a_direct_injector_or_a_non_canonical_replacement_is_never_attested() {
        let canonical = || StaticData {
            inject_config: None,
            replace_config: Some(canonical_replace()),
        };
        for (label, data) in [
            // Console lets a custom secret inject Authorization directly.
            (
                "injector",
                StaticData {
                    inject_config: Some(json!({"header": "Authorization"})),
                    replace_config: None,
                },
            ),
            (
                "injector and replacement",
                StaticData {
                    inject_config: Some(json!({"header": "Authorization"})),
                    ..canonical()
                },
            ),
            (
                "another placeholder",
                StaticData {
                    inject_config: None,
                    replace_config: Some(json!({
                        "proxy_value": "OTHER", "match_headers": ["Authorization"], "require": false
                    })),
                },
            ),
            (
                "another header",
                StaticData {
                    inject_config: None,
                    replace_config: Some(json!({
                        "proxy_value": "GITHUB_TOKEN", "match_headers": ["X-Token"], "require": false
                    })),
                },
            ),
            (
                "required",
                StaticData {
                    inject_config: None,
                    replace_config: Some(json!({
                        "proxy_value": "GITHUB_TOKEN", "match_headers": ["Authorization"], "require": true
                    })),
                },
            ),
            (
                "extra header",
                StaticData {
                    inject_config: None,
                    replace_config: Some(json!({
                        "proxy_value": "GITHUB_TOKEN",
                        "match_headers": ["Authorization", "X-Token"],
                        "require": false
                    })),
                },
            ),
        ] {
            let credential = static_with(1, data, vec![api_rule(&[], &[])]);
            assert_eq!(github_user_id_of(&[credential]), None, "{label}");
        }
        assert!(
            github_user_id_of(&[static_with(1, canonical(), vec![api_rule(&[], &[])])]).is_some()
        );
    }

    #[test]
    fn a_credential_whose_rules_do_not_cover_pr_create_is_never_attested() {
        let canonical = || StaticData {
            inject_config: None,
            replace_config: Some(canonical_replace()),
        };
        for (label, rules) in [
            ("read only", vec![api_rule(&["GET"], &[])]),
            ("issues only", vec![api_rule(&[], &["/repos/*/*/issues"])]),
            ("other paths", vec![api_rule(&["POST"], &["/graphql"])]),
            ("no rules", vec![]),
            ("other host", vec![rule("github.com")]),
        ] {
            let credential = static_with(1, canonical(), rules);
            assert_eq!(github_user_id_of(&[credential]), None, "{label}");
        }
        for (label, rules) in [
            ("any method and path", vec![api_rule(&[], &[])]),
            ("POST only", vec![api_rule(&["post"], &[])]),
            (
                "the exact path",
                vec![api_rule(&["POST"], &["/repos/*/*/pulls"])],
            ),
            ("a covering path", vec![api_rule(&[], &["/repos/**"])]),
            (
                "one covering rule among others",
                vec![
                    rule("github.com"),
                    api_rule(&["GET"], &[]),
                    api_rule(&["POST"], &[]),
                ],
            ),
        ] {
            let credential = static_with(1, canonical(), rules);
            assert_eq!(
                github_user_id_of(&[credential]),
                Some("90210001".to_owned()),
                "{label}"
            );
        }
    }

    #[test]
    fn a_header_rewriting_transform_on_the_api_host_withdraws_the_attestation() {
        let attested = github_token(1, 100, "api.github.com");
        assert!(github_user_id_of(std::slice::from_ref(&attested)).is_some());
        let transform = |kind: CredentialKind, data: CredentialData, host: &str| Credential {
            kind,
            id: 50,
            priority: 100,
            data,
            sources: vec![],
            rules: vec![rule(host)],
        };
        let hmac = |host: &str| {
            transform(
                CredentialKind::Hmac,
                CredentialData::Hmac(crate::models::HmacData {
                    timestamp_format: "unix".to_owned(),
                    signature_algorithm: "sha256".to_owned(),
                    signature_key_encoding: "raw".to_owned(),
                    signature_output_encoding: "hex".to_owned(),
                    signature_message: "{{ .Body }}".to_owned(),
                    // A header that does not collide with `Authorization`.
                    headers: vec![crate::models::HmacHeader {
                        name: "X-Signature".to_owned(),
                        value: "{{ .Signature }}".to_owned(),
                    }],
                    allow_chunked_body: false,
                }),
                host,
            )
        };
        let aws = |host: &str| {
            transform(
                CredentialKind::AwsAuth,
                CredentialData::AwsAuth(crate::models::AwsAuthData {
                    allowed_regions: vec![],
                    allowed_services: vec![],
                }),
                host,
            )
        };
        let gcp = |host: &str| {
            transform(
                CredentialKind::GcpAuth,
                CredentialData::GcpAuth(crate::models::GcpAuthData {
                    credentials_provider: None,
                    subject: None,
                    scopes: vec![],
                }),
                host,
            )
        };
        let gcp_id = |host: &str| {
            transform(
                CredentialKind::GcpIdToken,
                CredentialData::GcpIdToken(crate::models::GcpIdTokenData {
                    audience: "https://run.example".to_owned(),
                    header: Some("X-Serverless-Authorization".to_owned()),
                }),
                host,
            )
        };
        let oauth = |host: &str| {
            transform(
                CredentialKind::OauthToken,
                CredentialData::OauthToken(crate::models::OauthTokenData {
                    grant: "refresh_token".to_owned(),
                    token_endpoint: "https://idp.example/token".to_owned(),
                    audience: None,
                    scopes: vec![],
                    header: Some("X-Other".to_owned()),
                    value_prefix: None,
                }),
                host,
            )
        };
        for (label, other) in [
            ("hmac", hmac("api.github.com")),
            ("aws_auth", aws("api.github.com")),
            ("gcp_auth", gcp("api.github.com")),
            ("gcp_id_token", gcp_id("api.github.com")),
            ("oauth_token", oauth("api.github.com")),
            ("wildcard host", hmac("*.github.com")),
        ] {
            assert_eq!(
                github_user_id_of(&[github_token(1, 100, "api.github.com"), other]),
                None,
                "{label}"
            );
        }
        // The same transforms on other hosts leave it alone.
        assert!(
            github_user_id_of(&[
                github_token(1, 100, "api.github.com"),
                hmac("api.example.com"),
                aws("s3.example.com"),
                gcp("storage.example.com"),
                gcp_id("run.example.com"),
                oauth("mcp.example.com"),
            ])
            .is_some()
        );
    }

    #[test]
    fn only_an_unambiguous_brokered_github_credential_names_an_account() {
        let mut shared = github_token(1, 100, "api.github.com");
        shared.sources = vec![source("control_plane", None, None)];
        let mut other_provider = github_token(2, 100, "api.github.com");
        other_provider.sources = vec![source("token_broker", Some("slack"), Some("U1"))];
        let mut no_provider = github_token(3, 100, "api.github.com");
        no_provider.sources = vec![source("token_broker", None, Some("90210001"))];
        // A shared app's token can reach anyone it is granted to.
        let mut shared_app = github_token(9, 100, "api.github.com");
        shared_app.sources[0].broker_shared = Some(true);
        // An owner-only connection records `owner-<console user>`, not a
        // GitHub id, so it can never name an author.
        let mut owner_only_app = github_token(10, 100, "api.github.com");
        owner_only_app.sources[0].broker_owner_only = Some(true);
        owner_only_app.sources[0].broker_provider_subject = Some("owner-usr_abc123".to_owned());
        let mut owner_only_numeric = github_token(11, 100, "api.github.com");
        owner_only_numeric.sources[0].broker_owner_only = Some(true);
        // A token someone else connected, delivered by a channel or role grant.
        let mut not_the_actors = github_token(12, 100, "api.github.com");
        not_the_actors.sources[0].broker_owned_by_actor = Some(false);
        // Unknown ownership is no ownership.
        let mut unknown_owner = github_token(13, 100, "api.github.com");
        unknown_owner.sources[0].broker_owned_by_actor = None;
        let mut unknown_shared = github_token(14, 100, "api.github.com");
        unknown_shared.sources[0].broker_shared = None;
        let mut unknown_owner_only = github_token(15, 100, "api.github.com");
        unknown_owner_only.sources[0].broker_owner_only = None;
        let mut non_numeric = github_token(16, 100, "api.github.com");
        non_numeric.sources[0].broker_provider_subject = Some("ada-lovelace".to_owned());
        let mut no_subject = github_token(4, 100, "api.github.com");
        no_subject.sources = vec![source("token_broker", Some("github"), None)];
        let mut blank_subject = github_token(5, 100, "api.github.com");
        blank_subject.sources = vec![source("token_broker", Some("github"), Some(" "))];
        let mut other_header = github_token(6, 100, "api.github.com");
        other_header.data = CredentialData::Static(StaticData {
            inject_config: Some(json!({"header": "X-Api-Key"})),
            replace_config: None,
        });
        let other_host = github_token(7, 100, "github.com");
        let mut cidr_only = github_token(8, 100, "api.github.com");
        cidr_only.rules = vec![RequestRule {
            host: None,
            cidr: Some("140.82.112.0/20".to_owned()),
            http_methods: vec![],
            paths: vec![],
        }];
        for credential in [
            shared,
            shared_app,
            owner_only_app,
            owner_only_numeric,
            not_the_actors,
            unknown_owner,
            unknown_shared,
            unknown_owner_only,
            non_numeric,
            other_provider,
            no_provider,
            no_subject,
            blank_subject,
            other_header,
            other_host,
            cidr_only,
        ] {
            let id = credential.id;
            assert_eq!(github_user_id_of(&[credential]), None, "credential {id}");
        }
        assert_eq!(github_user_id_of(&[]), None);
    }

    #[test]
    fn competing_credentials_make_the_account_ambiguous_unless_suppressed() {
        // Two equal-priority grants both write Authorization on the host.
        let mut second = github_token(2, 100, "api.github.com");
        second.sources = vec![source("token_broker", Some("github"), Some("90210002"))];
        let mut credentials = vec![github_token(1, 100, "api.github.com"), second];
        suppress(&mut credentials);
        assert_eq!(github_user_id_of(&credentials), None);

        // A stronger grant withholds the weaker one, so the stronger one is
        // the credential the proxy would use.
        let mut weaker = github_token(3, 0, "api.github.com");
        weaker.sources = vec![source("token_broker", Some("github"), Some("90210003"))];
        let mut credentials = vec![weaker, github_token(1, 100, "api.github.com")];
        suppress(&mut credentials);
        assert_eq!(github_user_id_of(&credentials), Some("90210001".to_owned()));

        // A non-static credential writing the same header on the host also
        // makes the answer unknowable here.
        let mut credentials = vec![
            github_token(1, 100, "api.github.com"),
            Credential {
                kind: CredentialKind::OauthToken,
                id: 9,
                priority: 100,
                data: CredentialData::OauthToken(crate::models::OauthTokenData {
                    grant: "refresh_token".to_owned(),
                    token_endpoint: "https://idp.example/token".to_owned(),
                    audience: None,
                    scopes: vec![],
                    header: None,
                    value_prefix: None,
                }),
                sources: vec![],
                rules: vec![rule("api.github.com")],
            },
        ];
        suppress(&mut credentials);
        assert_eq!(github_user_id_of(&credentials), None);
    }

    #[test]
    fn postgres_principal_labels_stringify_scalars_and_missing_values() {
        let principal = json!({"labels": {"tenant": 123, "enabled": false, "empty": null}});
        let subject = Subject {
            principal_id: 1,
            principal: Some(&principal),
            console_user_id: None,
            console_user_email: None,
            slack_history_channel_ids: &json!([]),
            proxy_labels: &json!({}),
        };
        let settings: Vec<PostgresSetting> = serde_json::from_value(json!([
            {"name": "app.tenant", "value_from": {"principal_label": "tenant"}},
            {"name": "app.enabled", "value_from": {"principal_label": "enabled"}},
            {"name": "app.empty", "value_from": {"principal_label": "empty"}},
            {"name": "app.missing", "value_from": {"principal_label": "missing"}}
        ]))
        .unwrap();
        assert_eq!(
            postgres_settings(&subject, &settings),
            vec![
                json!({"name": "app.tenant", "value": "123"}),
                json!({"name": "app.enabled", "value": "false"}),
                json!({"name": "app.empty", "value": ""}),
                json!({"name": "app.missing", "value": ""}),
            ]
        );
    }

    #[test]
    fn rules_match_the_proxy_shape() {
        let rules = vec![RequestRule {
            host: Some("api.example.com".to_owned()),
            cidr: None,
            http_methods: vec!["POST".to_owned()],
            paths: vec!["/v1/*".to_owned()],
        }];
        assert_eq!(
            proxy_rules(&rules),
            json!([{"host":"api.example.com","methods":["POST"],"paths":["/v1/*"]}])
        );
    }
}
