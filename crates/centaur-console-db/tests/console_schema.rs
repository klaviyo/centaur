//! Tests against a Postgres database holding Console's schema.
//!
//! Set `CONSOLE_DB_TEST_DATABASE_URL` to a database prepared with Console's
//! `bin/rails db:prepare`. Each test inserts uniquely named rows, so tests can
//! share one database and run in parallel.

use std::env;

use active_record_encryption::ActiveRecordEncryption;
use centaur_console_db::{
    ApiClient, ApiClientPrincipal, PR_OAUTH_PLACEHOLDER, REDACTED, SecretValues,
    SlackChannelPermission, Subject, api_client_principals, build_config,
    build_config_with_github_user_id, find_api_client, find_direct_slack_channel_permission,
    find_principal, find_principal_by_foreign_id, find_user_id_by_email, models::Config,
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};

const PRIMARY_KEY: &str = "dev_ar_encryption_primary_key_0000000000000000";
const SALT: &str = "dev_ar_encryption_key_derivation_salt_000000000";
/// "known secret" encrypted by Active Record with the keys above.
const ENCRYPTED: &str =
    r#"{"p":"xOiyro9XYBkfABUF","h":{"iv":"pCjJC5SxH78ZN0Nm","at":"s9DIUyHAkF8ZAthKO6ROtw=="}}"#;
const PLAINTEXT: &str = "known secret";

async fn database() -> Option<PgPool> {
    let Ok(url) = env::var("CONSOLE_DB_TEST_DATABASE_URL") else {
        assert!(
            env::var_os("CI").is_none(),
            "CONSOLE_DB_TEST_DATABASE_URL must be set in CI"
        );
        eprintln!("skipping: set CONSOLE_DB_TEST_DATABASE_URL to a database with Console's schema");
        return None;
    };
    Some(
        PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect to the test database"),
    )
}

/// How a brokered test credential reaches the proxy.
#[derive(Clone, Copy)]
enum Wrapper {
    /// Console's canonical `github_token`: a `GITHUB_TOKEN` placeholder
    /// replacement in `Authorization`, on the whole host.
    Canonical,
    /// A custom secret that injects `Authorization` directly.
    Injected,
    /// The canonical replacement, but its rule allows only `GET`.
    ReadOnly,
}

/// The OAuth app and connection behind a brokered test credential.
#[derive(Clone, Copy)]
struct App {
    owner_only: bool,
    shared: bool,
    /// The Console user who connected the credential.
    connected_by: i64,
    wrapper: Wrapper,
}

struct Fixture {
    pool: PgPool,
    admin: i64,
    unique: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let pool = database().await?;
        let unique = format!("{:032x}", rand::random::<u128>());
        let admin = sqlx::query_scalar(
            "INSERT INTO users (email, admin, status, created_at, updated_at) \
             VALUES ($1, true, 'active', now(), now()) RETURNING id",
        )
        .bind(format!("admin-{unique}@example.com"))
        .fetch_one(&pool)
        .await
        .unwrap();
        Some(Self {
            pool,
            admin,
            unique,
        })
    }

    fn name(&self, label: &str) -> String {
        format!("{label}-{}", self.unique)
    }

    async fn principal(&self, label: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO principals (foreign_id, name, kind, labels, slack_user_id, created_by_id, created_at, updated_at) \
             VALUES ($1, 'Example', 'slack_dm', '{\"team\": \"core\"}', 'U12345678', $2, now(), now()) RETURNING id",
        )
        .bind(self.name(label))
        .bind(self.admin)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn grant(&self, principal: i64, column: &str, secret: i64) {
        sqlx::query(&format!(
            "INSERT INTO grants (principal_id, {column}, priority, created_by_id, created_at, updated_at) \
             VALUES ($1, $2, 100, $3, now(), now())"
        ))
        .bind(principal)
        .bind(secret)
        .bind(self.admin)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn rule(&self, column: &str, secret: i64, host: &str) {
        sqlx::query(&format!(
            "INSERT INTO request_rules ({column}, host, position, created_at, updated_at) \
             VALUES ($1, $2, 0, now(), now())"
        ))
        .bind(secret)
        .bind(host)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn static_secret(&self, label: &str, inject: Value, replace: Option<Value>) -> i64 {
        let (inject, replace) = match replace {
            Some(replace) => (None, Some(replace)),
            None => (Some(inject), None),
        };
        sqlx::query_scalar(
            "INSERT INTO static_secrets (foreign_id, inject_config, replace_config, created_by_id, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, now(), now()) RETURNING id",
        )
        .bind(self.name(label))
        .bind(inject)
        .bind(replace)
        .bind(self.admin)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn broker_credential(&self, label: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO broker_credentials (foreign_id, token_endpoint, access_token, created_by_id, created_at, updated_at) \
             VALUES ($1, 'https://idp.example/token', $2, $3, now(), now()) RETURNING id",
        )
        .bind(self.name(label))
        .bind(ENCRYPTED)
        .bind(self.admin)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// A personal GitHub-style app (neither shared nor owner-only) whose
    /// connection the admin user made.
    fn personal(&self) -> App {
        App {
            owner_only: false,
            shared: false,
            connected_by: self.admin,
            wrapper: Wrapper::Canonical,
        }
    }

    /// Another Console user, who can connect credentials of their own.
    async fn other_user(&self, label: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO users (email, admin, status, created_at, updated_at) \
             VALUES ($1, false, 'active', now(), now()) RETURNING id",
        )
        .bind(format!("{}@example.com", self.name(label)))
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// A `console_user` principal for the admin user, the owner of the
    /// credentials `brokered_token` creates.
    async fn owner_principal(&self, label: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO principals (foreign_id, name, kind, labels, console_user_id, created_by_id, created_at, updated_at) \
             VALUES ($1, 'Owner', 'console_user', '{}', $2, $2, now(), now()) RETURNING id",
        )
        .bind(self.name(label))
        .bind(self.admin)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// A brokered token for `provider` account `subject`, wrapped in a static
    /// secret that injects `Authorization` on `host`, and granted directly to
    /// `principal` at `priority`. The app is `owner_only` as given; Console
    /// delivers an owner-only token only to the owner's own principal
    /// (`owner_principal`). Returns the static secret's id.
    #[allow(clippy::too_many_arguments)]
    async fn brokered_token(
        &self,
        principal: i64,
        label: &str,
        provider: &str,
        subject: Option<&str>,
        host: &str,
        priority: i32,
        app_shape: App,
    ) -> i64 {
        let app: i64 = sqlx::query_scalar(
            "INSERT INTO oauth_apps (slug, provider, client_id, owner_only, shared, created_by_id, created_at, updated_at) \
             VALUES ($1, $2, 'client', $3, $4, $5, now(), now()) RETURNING id",
        )
        .bind(self.name(label))
        .bind(provider)
        .bind(app_shape.owner_only)
        .bind(app_shape.shared)
        .bind(self.admin)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        let broker: i64 = sqlx::query_scalar(
            "INSERT INTO broker_credentials (foreign_id, token_endpoint, access_token, oauth_app_id, provider_subject, created_by_id, created_at, updated_at) \
             VALUES ($1, 'https://idp.example/token', $2, $3, $4, $5, now(), now()) RETURNING id",
        )
        .bind(self.name(label))
        .bind(ENCRYPTED)
        .bind(app)
        .bind(subject)
        .bind(app_shape.connected_by)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        let secret: i64 = sqlx::query_scalar(
            "INSERT INTO static_secrets (foreign_id, inject_config, replace_config, broker_credential_id, created_by_id, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, now(), now()) RETURNING id",
        )
        .bind(self.name(label))
        .bind(match app_shape.wrapper {
            Wrapper::Injected => Some(json!({ "header": "Authorization" })),
            Wrapper::Canonical | Wrapper::ReadOnly => None,
        })
        .bind(match app_shape.wrapper {
            Wrapper::Injected => None,
            Wrapper::Canonical | Wrapper::ReadOnly => Some(json!({
                "proxy_value": "GITHUB_TOKEN",
                "match_headers": ["Authorization"],
                "require": false,
            })),
        })
        .bind(broker)
        .bind(self.admin)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        self.source(
            "static_secret_id",
            secret,
            "token_broker",
            None,
            Some(broker),
        )
        .await;
        self.rule("static_secret_id", secret, host).await;
        if matches!(app_shape.wrapper, Wrapper::ReadOnly) {
            sqlx::query(
                "UPDATE request_rules SET http_methods = '[\"GET\"]' WHERE static_secret_id = $1",
            )
            .bind(secret)
            .execute(&self.pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO grants (principal_id, static_secret_id, priority, created_by_id, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, now(), now())",
        )
        .bind(principal)
        .bind(secret)
        .bind(priority)
        .bind(self.admin)
        .execute(&self.pool)
        .await
        .unwrap();
        secret
    }

    async fn source(
        &self,
        column: &str,
        owner: i64,
        source_type: &str,
        secret: Option<&str>,
        broker: Option<i64>,
    ) {
        sqlx::query(&format!(
            "INSERT INTO secret_sources ({column}, source_type, config, secret, broker_credential_id, created_at, updated_at) \
             VALUES ($1, $2, '{{}}', $3, $4, now(), now())"
        ))
        .bind(owner)
        .bind(source_type)
        .bind(secret)
        .bind(broker)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// Grants `principal` a stored inject secret, a brokered secret, a replace
    /// secret, and a GCP keyfile transform, all with encrypted values.
    async fn grant_secret_values(&self, principal: i64) {
        let inject = self
            .static_secret("inject", json!({ "header": "X-Api-Key" }), None)
            .await;
        self.source(
            "static_secret_id",
            inject,
            "control_plane",
            Some(ENCRYPTED),
            None,
        )
        .await;
        self.rule("static_secret_id", inject, "inject.example")
            .await;
        self.grant(principal, "static_secret_id", inject).await;

        let broker = self.broker_credential("broker").await;
        let brokered = self
            .static_secret("brokered", json!({ "header": "Authorization" }), None)
            .await;
        self.source(
            "static_secret_id",
            brokered,
            "token_broker",
            None,
            Some(broker),
        )
        .await;
        self.rule("static_secret_id", brokered, "brokered.example")
            .await;
        self.grant(principal, "static_secret_id", brokered).await;

        let replace = self
            .static_secret(
                "replace",
                Value::Null,
                Some(json!({ "proxy_value": "PLACEHOLDER", "match_headers": ["X-Token"] })),
            )
            .await;
        self.source(
            "static_secret_id",
            replace,
            "control_plane",
            Some(ENCRYPTED),
            None,
        )
        .await;
        self.rule("static_secret_id", replace, "replace.example")
            .await;
        self.grant(principal, "static_secret_id", replace).await;

        let gcp: i64 = sqlx::query_scalar(
            "INSERT INTO gcp_auth_secrets (foreign_id, scopes, created_by_id, created_at, updated_at) \
             VALUES ($1, '[]', $2, now(), now()) RETURNING id",
        )
        .bind(self.name("gcp"))
        .bind(self.admin)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        self.source(
            "gcp_auth_secret_id",
            gcp,
            "control_plane",
            Some(ENCRYPTED),
            None,
        )
        .await;
        self.rule("gcp_auth_secret_id", gcp, "gcp.example").await;
        self.grant(principal, "gcp_auth_secret_id", gcp).await;
    }
}

fn to_json(config: &Config) -> Value {
    json!({
        "secrets": config.secrets,
        "transforms": config.transforms,
        "postgres": config.postgres,
    })
}

/// Console's redaction: every inline control_plane value becomes `[redacted]`.
fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut redacted: serde_json::Map<String, Value> = map
                .iter()
                .map(|(key, value)| (key.clone(), redact(value)))
                .collect();
            if map.get("type") == Some(&json!("control_plane")) && map.contains_key("value") {
                redacted.insert("value".to_owned(), json!(REDACTED));
            }
            Value::Object(redacted)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        other => other.clone(),
    }
}

#[tokio::test]
async fn redacted_config_is_the_live_config_without_secret_values() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let principal = fixture.principal("principal").await;
    fixture.grant_secret_values(principal).await;
    let empty = json!([]);
    let labels = json!({});
    let subject = Subject {
        principal_id: principal,
        principal: None,
        console_user_id: None,
        console_user_email: None,
        slack_history_channel_ids: &empty,
        proxy_labels: &labels,
    };
    let encryption = ActiveRecordEncryption::new(PRIMARY_KEY, SALT);

    let live = build_config(
        &fixture.pool,
        SecretValues::Live(&encryption),
        &subject,
        None,
    )
    .await
    .unwrap();
    let redacted = build_config(&fixture.pool, SecretValues::Redacted, &subject, None)
        .await
        .unwrap();

    let live = to_json(&live);
    assert_eq!(live["secrets"].as_array().unwrap().len(), 3);
    assert!(live.to_string().contains(PLAINTEXT));
    let redacted = to_json(&redacted);
    assert!(!redacted.to_string().contains(PLAINTEXT));
    assert_eq!(redacted, redact(&live));
}

#[tokio::test]
async fn effective_config_is_redacted_and_absent_for_missing_principals() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let principal = fixture.principal("principal").await;
    fixture.grant_secret_values(principal).await;

    let config = centaur_console_db::effective_config(&fixture.pool, principal)
        .await
        .unwrap()
        .expect("principal exists");
    let config = to_json(&config);
    assert!(!config.to_string().contains(PLAINTEXT));
    assert_eq!(
        config["secrets"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|secret| secret["replace"]["proxy_value"].as_str())
            .collect::<Vec<_>>(),
        vec!["PLACEHOLDER"]
    );

    assert!(
        centaur_console_db::effective_config(&fixture.pool, i64::MAX)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn reads_principals_users_and_direct_slack_permissions() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let principal = fixture.principal("principal").await;
    sqlx::query(
        "INSERT INTO slack_channel_permissions (principal_id, channel_id, upload_enabled, download_enabled, history_enabled, created_at, updated_at) \
         VALUES ($1, 'D12345678', true, false, true, now(), now())",
    )
    .bind(principal)
    .execute(&fixture.pool)
    .await
    .unwrap();

    let by_id = find_principal(&fixture.pool, principal)
        .await
        .unwrap()
        .expect("principal exists");
    assert_eq!(
        by_id.foreign_id.as_deref(),
        Some(fixture.name("principal").as_str())
    );
    assert_eq!(by_id.name.as_deref(), Some("Example"));
    assert_eq!(by_id.kind, "slack_dm");
    assert_eq!(by_id.labels, json!({ "team": "core" }));
    assert_eq!(by_id.slack_user_id.as_deref(), Some("U12345678"));
    assert_eq!(by_id.sandbox_repo_cache, "all");
    assert!(by_id.sandbox_observability_enabled);
    assert_eq!(
        find_principal_by_foreign_id(&fixture.pool, &fixture.name("principal"))
            .await
            .unwrap(),
        Some(by_id)
    );
    assert_eq!(
        find_principal_by_foreign_id(&fixture.pool, &fixture.name("missing"))
            .await
            .unwrap(),
        None
    );

    assert_eq!(
        find_user_id_by_email(
            &fixture.pool,
            &format!("admin-{}@example.com", fixture.unique)
        )
        .await
        .unwrap(),
        Some(fixture.admin)
    );
    assert_eq!(
        find_direct_slack_channel_permission(&fixture.pool, principal, "D12345678")
            .await
            .unwrap(),
        Some(SlackChannelPermission {
            upload_enabled: true,
            download_enabled: false,
            history_enabled: true,
        })
    );
    assert_eq!(
        find_direct_slack_channel_permission(&fixture.pool, principal, "D99999999")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn reads_api_clients_and_their_principals() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let default = fixture.principal("default").await;
    let other = fixture.principal("other").await;
    let client: i64 = sqlx::query_scalar(
        "INSERT INTO api_clients (name, secret_digest, scopes, disabled_at, created_by_id, created_at, updated_at) \
         VALUES ($1, $2, '[\"sessions:read\", \"sessions:write\"]', now(), $3, now(), now()) RETURNING id",
    )
    .bind(fixture.name("client"))
    .bind(fixture.name("digest"))
    .bind(fixture.admin)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    for (principal, is_default) in [(default, true), (other, false)] {
        sqlx::query(
            "INSERT INTO api_client_principals (api_client_id, principal_id, \"default\", created_at, updated_at) \
             VALUES ($1, $2, $3, now(), now())",
        )
        .bind(client)
        .bind(principal)
        .bind(is_default)
        .execute(&fixture.pool)
        .await
        .unwrap();
    }

    assert_eq!(
        find_api_client(&fixture.pool, client).await.unwrap(),
        Some(ApiClient {
            id: client,
            name: fixture.name("client"),
            secret_digest: fixture.name("digest"),
            scopes: vec!["sessions:read".to_owned(), "sessions:write".to_owned()],
            disabled: true,
        })
    );
    assert_eq!(
        find_api_client(&fixture.pool, i64::MAX).await.unwrap(),
        None
    );
    assert_eq!(
        api_client_principals(&fixture.pool, client).await.unwrap(),
        vec![
            ApiClientPrincipal {
                principal_id: default,
                foreign_id: Some(fixture.name("default")),
                default: true,
            },
            ApiClientPrincipal {
                principal_id: other,
                foreign_id: Some(fixture.name("other")),
                default: false,
            },
        ]
    );
}

/// The delivered config and the GitHub account derived with it, in one call.
async fn github_config(fixture: &Fixture, principal: i64) -> (Config, Option<String>) {
    let encryption = ActiveRecordEncryption::new(PRIMARY_KEY, SALT);
    let empty = json!([]);
    let labels = json!({});
    let subject = Subject {
        principal_id: principal,
        principal: None,
        console_user_id: None,
        console_user_email: None,
        slack_history_channel_ids: &empty,
        proxy_labels: &labels,
    };
    build_config_with_github_user_id(
        &fixture.pool,
        SecretValues::Live(&encryption),
        &subject,
        None,
    )
    .await
    .unwrap()
}

async fn github_account(fixture: &Fixture, principal: i64) -> Option<String> {
    github_config(fixture, principal).await.1
}

/// The `Authorization` secrets the config delivers for `api.github.com`.
fn github_secrets(config: &Config) -> Vec<&Value> {
    config
        .secrets
        .iter()
        .filter(|secret| {
            secret["replace"]["proxy_value"] == "GITHUB_TOKEN"
                && secret["rules"] == json!([{ "host": "api.github.com" }])
        })
        .collect()
}

/// The secrets that carry the PR-create OAuth placeholder.
fn pr_oauth_secrets(config: &Config) -> Vec<&Value> {
    config
        .secrets
        .iter()
        .filter(|secret| secret["replace"]["proxy_value"] == PR_OAUTH_PLACEHOLDER)
        .collect()
}

/// The sentinel the guard carries when no account is attested.
fn sentinel() -> Value {
    json!({"type": "control_plane", "value": "centaur-unattested-oauth"})
}

/// Checks that `config` ends with exactly one PR-create guard, `require: true`,
/// for the one create operation, and returns its source.
fn assert_guard_last(config: &Config) -> Value {
    let guards = pr_oauth_secrets(config);
    assert_eq!(guards.len(), 1, "{guards:?}");
    assert_eq!(
        config.secrets.last().map(|secret| &secret["replace"]),
        Some(&json!({
            "proxy_value": "CENTAUR_PR_OAUTH",
            "match_headers": ["Authorization"],
            "require": true,
        })),
        "the guard must be the last secret"
    );
    assert_eq!(
        guards[0]["rules"],
        json!([{
            "host": "api.github.com",
            "methods": ["POST"],
            "paths": ["/repos/*/*/pulls"],
        }])
    );
    assert!(guards[0].get("inject").is_none());
    guards[0]["source"].clone()
}

#[tokio::test]
async fn the_pr_create_guard_is_in_every_build_and_names_the_attested_credential() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // Nothing granted: the guard is still there, with the sentinel.
    let actor = fixture.owner_principal("pr-oauth").await;
    let (config, account) = github_config(&fixture, actor).await;
    assert_eq!(account, None);
    assert_eq!(assert_guard_last(&config), sentinel());

    fixture
        .brokered_token(
            actor,
            "github-pr-oauth",
            "github",
            Some("90210001"),
            "api.github.com",
            100,
            fixture.personal(),
        )
        .await;
    let (config, account) = github_config(&fixture, actor).await;
    assert_eq!(account.as_deref(), Some("90210001"));

    // Attested: the guard carries the credential's own delivered source.
    let delivered = github_secrets(&config);
    assert_eq!(delivered.len(), 1);
    let source = assert_guard_last(&config);
    assert_eq!(source, delivered[0]["source"]);
    assert_eq!(source["type"], "control_plane");
    assert_ne!(source, sentinel());

    // The plain config (Console's effective-config preview, for one) never
    // carries it.
    let encryption = ActiveRecordEncryption::new(PRIMARY_KEY, SALT);
    let empty = json!([]);
    let labels = json!({});
    let subject = Subject {
        principal_id: actor,
        principal: None,
        console_user_id: None,
        console_user_email: None,
        slack_history_channel_ids: &empty,
        proxy_labels: &labels,
    };
    let plain = build_config(
        &fixture.pool,
        SecretValues::Live(&encryption),
        &subject,
        None,
    )
    .await
    .unwrap();
    assert!(pr_oauth_secrets(&plain).is_empty());

    // An equal competitor leaves the account unattested: the guard stays, now
    // with the sentinel instead of either token.
    fixture
        .brokered_token(
            actor,
            "github-pr-oauth-competing",
            "github",
            Some("90210002"),
            "api.github.com",
            100,
            fixture.personal(),
        )
        .await;
    let (config, account) = github_config(&fixture, actor).await;
    assert_eq!(account, None);
    assert_eq!(assert_guard_last(&config), sentinel());
}

#[tokio::test]
async fn a_direct_injector_or_a_narrow_rule_gives_no_claim_and_a_sentinel_guard() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // A custom secret injecting Authorization directly on api.github.com.
    let injected = fixture.owner_principal("injected").await;
    fixture
        .brokered_token(
            injected,
            "github-injected",
            "github",
            Some("90210001"),
            "api.github.com",
            100,
            App {
                wrapper: Wrapper::Injected,
                ..fixture.personal()
            },
        )
        .await;
    let (config, account) = github_config(&fixture, injected).await;
    assert_eq!(account, None);
    assert_eq!(assert_guard_last(&config), sentinel());

    // The canonical replacement, but only for GET.
    let read_only = fixture.owner_principal("read-only").await;
    fixture
        .brokered_token(
            read_only,
            "github-read-only",
            "github",
            Some("90210001"),
            "api.github.com",
            100,
            App {
                wrapper: Wrapper::ReadOnly,
                ..fixture.personal()
            },
        )
        .await;
    let (config, account) = github_config(&fixture, read_only).await;
    assert_eq!(account, None);
    assert_eq!(assert_guard_last(&config), sentinel());
}

#[tokio::test]
async fn a_transform_targeting_the_api_host_withdraws_the_claim() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let actor = fixture.owner_principal("transform").await;
    fixture
        .brokered_token(
            actor,
            "github-transform",
            "github",
            Some("90210001"),
            "api.github.com",
            100,
            fixture.personal(),
        )
        .await;
    assert_eq!(
        github_account(&fixture, actor).await.as_deref(),
        Some("90210001")
    );

    // A GCP auth transform on another host leaves the claim alone ...
    let gcp: i64 = sqlx::query_scalar(
        "INSERT INTO gcp_auth_secrets (foreign_id, scopes, created_by_id, created_at, updated_at) \
         VALUES ($1, '[]', $2, now(), now()) RETURNING id",
    )
    .bind(fixture.name("gcp-elsewhere"))
    .bind(fixture.admin)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    fixture
        .source(
            "gcp_auth_secret_id",
            gcp,
            "control_plane",
            Some(ENCRYPTED),
            None,
        )
        .await;
    fixture
        .rule("gcp_auth_secret_id", gcp, "storage.example.com")
        .await;
    fixture.grant(actor, "gcp_auth_secret_id", gcp).await;
    assert_eq!(
        github_account(&fixture, actor).await.as_deref(),
        Some("90210001")
    );

    // ... and one that targets api.github.com withdraws it, leaving the guard
    // with the sentinel.
    fixture
        .rule("gcp_auth_secret_id", gcp, "api.github.com")
        .await;
    let (config, account) = github_config(&fixture, actor).await;
    assert_eq!(account, None);
    assert_eq!(assert_guard_last(&config), sentinel());
}

#[tokio::test]
async fn the_account_is_derived_from_the_credentials_the_config_delivers() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let actor = fixture.owner_principal("one-load").await;
    fixture
        .brokered_token(
            actor,
            "github-delivered",
            "github",
            Some("90210001"),
            "api.github.com",
            100,
            fixture.personal(),
        )
        .await;

    // The account is the one behind the single GitHub secret in the config.
    let (config, account) = github_config(&fixture, actor).await;
    assert_eq!(account.as_deref(), Some("90210001"));
    assert_eq!(github_secrets(&config).len(), 1);

    // A weaker competitor is withheld from the config, so it neither changes
    // the account nor appears.
    fixture
        .brokered_token(
            actor,
            "github-withheld",
            "github",
            Some("90210002"),
            "api.github.com",
            0,
            fixture.personal(),
        )
        .await;
    let (config, account) = github_config(&fixture, actor).await;
    assert_eq!(account.as_deref(), Some("90210001"));
    assert_eq!(github_secrets(&config).len(), 1);

    // An equal competitor is delivered too, so the config carries two and no
    // account is claimed.
    fixture
        .brokered_token(
            actor,
            "github-competing",
            "github",
            Some("90210003"),
            "api.github.com",
            100,
            fixture.personal(),
        )
        .await;
    let (config, account) = github_config(&fixture, actor).await;
    assert_eq!(account, None);
    assert_eq!(github_secrets(&config).len(), 2);

    // A token that cannot be delivered (none minted yet) is in neither.
    let undelivered = fixture.owner_principal("one-load-undelivered").await;
    let secret = fixture
        .brokered_token(
            undelivered,
            "github-undelivered",
            "github",
            Some("90210004"),
            "api.github.com",
            100,
            fixture.personal(),
        )
        .await;
    sqlx::query(
        "UPDATE broker_credentials SET access_token = NULL \
         WHERE id = (SELECT broker_credential_id FROM static_secrets WHERE id = $1)",
    )
    .bind(secret)
    .execute(&fixture.pool)
    .await
    .unwrap();
    let (config, account) = github_config(&fixture, undelivered).await;
    assert_eq!(account, None);
    assert!(github_secrets(&config).is_empty());
}

#[tokio::test]
async fn names_the_github_account_behind_the_token_injected_for_the_api_host() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let principal = fixture.owner_principal("principal").await;
    assert_eq!(github_account(&fixture, principal).await, None);

    fixture
        .brokered_token(
            principal,
            "github",
            "github",
            Some("90210001"),
            "api.github.com",
            100,
            fixture.personal(),
        )
        .await;
    assert_eq!(
        github_account(&fixture, principal).await.as_deref(),
        Some("90210001")
    );

    // A weaker competing grant is withheld by the proxy, so it neither
    // changes the answer nor makes it ambiguous.
    fixture
        .brokered_token(
            principal,
            "weaker",
            "github",
            Some("90210002"),
            "api.github.com",
            0,
            fixture.personal(),
        )
        .await;
    assert_eq!(
        github_account(&fixture, principal).await.as_deref(),
        Some("90210001")
    );

    // An equal-strength competitor leaves two credentials writing the header.
    fixture
        .brokered_token(
            principal,
            "equal",
            "github",
            Some("90210003"),
            "api.github.com",
            100,
            fixture.personal(),
        )
        .await;
    assert_eq!(github_account(&fixture, principal).await, None);
}

#[tokio::test]
async fn only_a_github_app_with_a_subject_names_an_account() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let other_provider = fixture.owner_principal("other-provider").await;
    fixture
        .brokered_token(
            other_provider,
            "slack",
            "slack",
            Some("90210001"),
            "api.github.com",
            100,
            fixture.personal(),
        )
        .await;
    assert_eq!(github_account(&fixture, other_provider).await, None);

    let no_subject = fixture.owner_principal("no-subject").await;
    fixture
        .brokered_token(
            no_subject,
            "github-no-subject",
            "github",
            None,
            "api.github.com",
            100,
            fixture.personal(),
        )
        .await;
    assert_eq!(github_account(&fixture, no_subject).await, None);

    let other_host = fixture.owner_principal("other-host").await;
    fixture
        .brokered_token(
            other_host,
            "github-other-host",
            "github",
            Some("90210001"),
            "github.com",
            100,
            fixture.personal(),
        )
        .await;
    assert_eq!(github_account(&fixture, other_host).await, None);

    // A static token stored in Console, not brokered from an OAuth account,
    // proves no human identity.
    let shared = fixture.principal("shared").await;
    let secret = fixture
        .static_secret("shared", json!({ "header": "Authorization" }), None)
        .await;
    fixture
        .source(
            "static_secret_id",
            secret,
            "control_plane",
            Some(ENCRYPTED),
            None,
        )
        .await;
    fixture
        .rule("static_secret_id", secret, "api.github.com")
        .await;
    fixture.grant(shared, "static_secret_id", secret).await;
    assert_eq!(github_account(&fixture, shared).await, None);
}

#[tokio::test]
async fn only_the_actors_own_unshared_connection_names_an_account() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // A connection the actor made, from an app that is neither shared nor
    // owner-only, with the numeric GitHub id Console records.
    let actor = fixture.owner_principal("actor").await;
    fixture
        .brokered_token(
            actor,
            "github-mine",
            "github",
            Some("90210001"),
            "api.github.com",
            100,
            fixture.personal(),
        )
        .await;
    assert_eq!(
        github_account(&fixture, actor).await.as_deref(),
        Some("90210001")
    );

    // Someone else's connection delivered to the actor by a grant (a channel
    // or role grant, say) is not the actor's token.
    let channel_actor = fixture.owner_principal("channel-actor").await;
    let someone_else = fixture.other_user("someone-else").await;
    fixture
        .brokered_token(
            channel_actor,
            "github-theirs",
            "github",
            Some("90210002"),
            "api.github.com",
            100,
            App {
                connected_by: someone_else,
                ..fixture.personal()
            },
        )
        .await;
    assert_eq!(github_account(&fixture, channel_actor).await, None);

    // A shared app serves one connection to many people.
    let shared = fixture.owner_principal("shared-app").await;
    fixture
        .brokered_token(
            shared,
            "github-shared-app",
            "github",
            Some("90210003"),
            "api.github.com",
            100,
            App {
                shared: true,
                ..fixture.personal()
            },
        )
        .await;
    assert_eq!(github_account(&fixture, shared).await, None);

    // An owner-only connection, shaped like Console's real callback, records
    // `owner-<console user oid>` and no GitHub id.
    let owner_only = fixture.owner_principal("owner-only-app").await;
    fixture
        .brokered_token(
            owner_only,
            "github-owner-only",
            "github",
            Some("owner-usr_Xk3D9a"),
            "api.github.com",
            100,
            App {
                owner_only: true,
                ..fixture.personal()
            },
        )
        .await;
    assert_eq!(github_account(&fixture, owner_only).await, None);
    // ... even if its subject were numeric.
    let owner_only_numeric = fixture.owner_principal("owner-only-numeric").await;
    fixture
        .brokered_token(
            owner_only_numeric,
            "github-owner-only-numeric",
            "github",
            Some("90210004"),
            "api.github.com",
            100,
            App {
                owner_only: true,
                ..fixture.personal()
            },
        )
        .await;
    assert_eq!(github_account(&fixture, owner_only_numeric).await, None);
}
