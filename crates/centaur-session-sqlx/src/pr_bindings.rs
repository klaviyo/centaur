//! PR session bindings: the durable link from a GitHub pull request to the
//! exact Slack session whose sandbox created it.
//!
//! A row exists only because api-rs created the PR through the authorized
//! human's OAuth identity and checked GitHub's own 201 response (repository,
//! user type, author id) before inserting it. Nothing here trusts a webhook
//! to establish a binding: githubbot may only drive `managed`, `closed` and
//! `head_sha` on a row that already exists.

use centaur_session_core::ThreadKey;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::OffsetDateTime;

use crate::{PgSessionStore, SessionStoreError, parse_persisted};

const BINDING_COLUMNS: &str = "repository, number, thread_key, pr_id, pr_node_id, \
     author_github_id, author_login, sandbox_id, head_ref, base_ref, head_sha, \
     managed, closed, created_at, updated_at";

/// A binding as api-rs verified it from GitHub's create response.
///
/// Every field comes from that response or from the caller's authenticated
/// claims, never from a webhook, so a delayed or rerouted delivery cannot
/// choose what a binding says.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewPrBinding {
    /// Lowercased `owner/repo`.
    pub repository: String,
    pub number: i64,
    pub thread_key: ThreadKey,
    pub pr_id: i64,
    pub pr_node_id: String,
    pub author_github_id: i64,
    pub author_login: String,
    /// The sandbox whose proxy made the create call.
    pub sandbox_id: String,
    pub head_ref: String,
    pub base_ref: String,
    /// The head the PR was created at.
    pub head_sha: String,
}

/// A stored binding. `managed` starts false and only an explicit assignment
/// of the bot (recorded by githubbot) turns it on.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PrBinding {
    pub repository: String,
    pub number: i64,
    pub thread_key: ThreadKey,
    pub pr_id: i64,
    pub pr_node_id: String,
    pub author_github_id: i64,
    pub author_login: String,
    pub sandbox_id: String,
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: String,
    pub managed: bool,
    pub closed: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// The mutable state githubbot records for a bound PR. Absent fields are left
/// unchanged; nothing here can move a binding to another session.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PrBindingStateUpdate {
    #[serde(default)]
    pub managed: Option<bool>,
    #[serde(default)]
    pub closed: Option<bool>,
    #[serde(default)]
    pub head_sha: Option<String>,
}

/// Lowercases and validates `owner/repo`, the shape the table's check
/// constraint enforces.
pub fn normalize_pr_repository(repository: &str) -> Result<String, SessionStoreError> {
    let repository = repository.trim().to_ascii_lowercase();
    let mut segments = repository.split('/');
    let valid_segment = |segment: Option<&str>| {
        segment.is_some_and(|segment| {
            !segment.is_empty()
                && !matches!(segment, "." | "..")
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    };
    if !(valid_segment(segments.next())
        && valid_segment(segments.next())
        && segments.next().is_none())
    {
        return Err(SessionStoreError::InvalidPrBinding(
            "repository must be owner/repo".to_owned(),
        ));
    }
    Ok(repository)
}

fn validate_number(number: i64) -> Result<(), SessionStoreError> {
    if number <= 0 {
        return Err(SessionStoreError::InvalidPrBinding(
            "number must be a positive integer".to_owned(),
        ));
    }
    Ok(())
}

impl PgSessionStore {
    /// Records `new` as the binding for its pull request.
    ///
    /// Re-inserting the same `(repository, number)` with the same `pr_node_id`
    /// and `thread_key` returns the stored row untouched, so a retried create
    /// response is harmless. Anything else that collides, such as another
    /// session, another node id, or a node id already bound to a different
    /// PR, is [`SessionStoreError::PrBindingConflict`]: a binding is never
    /// rebound to another session.
    pub async fn insert_pr_binding(
        &self,
        new: &NewPrBinding,
    ) -> Result<PrBinding, SessionStoreError> {
        let repository = normalize_pr_repository(&new.repository)?;
        validate_number(new.number)?;
        let conflict = |reason: &str| SessionStoreError::PrBindingConflict {
            repository: repository.clone(),
            number: new.number,
            reason: reason.to_owned(),
        };
        let inserted = sqlx::query_as::<_, PrBindingRow>(&format!(
            r#"
            insert into pr_session_bindings (
                repository, number, thread_key, pr_id, pr_node_id, author_github_id,
                author_login, sandbox_id, head_ref, base_ref, head_sha
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            on conflict (repository, number) do nothing
            returning {BINDING_COLUMNS}
            "#
        ))
        .bind(&repository)
        .bind(new.number)
        .bind(new.thread_key.as_str())
        .bind(new.pr_id)
        .bind(&new.pr_node_id)
        .bind(new.author_github_id)
        .bind(&new.author_login)
        .bind(&new.sandbox_id)
        .bind(&new.head_ref)
        .bind(&new.base_ref)
        .bind(&new.head_sha)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| match database_error_code(&error).as_deref() {
            Some("23503") => SessionStoreError::NotFound {
                thread_key: new.thread_key.as_str().to_owned(),
            },
            Some("23505") => conflict("the pull request node id is already bound"),
            Some("23514") => SessionStoreError::InvalidPrBinding(
                "binding violates the table constraints".to_owned(),
            ),
            _ => error.into(),
        })?;
        if let Some(row) = inserted {
            return row.try_into();
        }
        let existing = self
            .pr_binding(&repository, new.number)
            .await?
            .ok_or_else(|| conflict("the binding changed while it was being inserted"))?;
        if existing.pr_node_id == new.pr_node_id && existing.thread_key == new.thread_key {
            return Ok(existing);
        }
        Err(conflict("the pull request is already bound"))
    }

    /// The binding for `repository#number`, if any.
    pub async fn pr_binding(
        &self,
        repository: &str,
        number: i64,
    ) -> Result<Option<PrBinding>, SessionStoreError> {
        let repository = normalize_pr_repository(repository)?;
        validate_number(number)?;
        sqlx::query_as::<_, PrBindingRow>(&format!(
            "select {BINDING_COLUMNS} from pr_session_bindings where repository = $1 and number = $2"
        ))
        .bind(&repository)
        .bind(number)
        .fetch_optional(&self.pool)
        .await?
        .map(TryInto::try_into)
        .transpose()
    }

    /// Applies `update` to an existing binding and returns it. Fails with
    /// [`SessionStoreError::PrBindingNotFound`] when none exists: state
    /// updates never create or rebind a row.
    pub async fn update_pr_binding_state(
        &self,
        repository: &str,
        number: i64,
        update: &PrBindingStateUpdate,
    ) -> Result<PrBinding, SessionStoreError> {
        let repository = normalize_pr_repository(repository)?;
        validate_number(number)?;
        if update
            .head_sha
            .as_deref()
            .is_some_and(|head_sha| head_sha.trim().is_empty())
        {
            return Err(SessionStoreError::InvalidPrBinding(
                "head_sha must not be empty".to_owned(),
            ));
        }
        sqlx::query_as::<_, PrBindingRow>(&format!(
            r#"
            update pr_session_bindings
            set managed = coalesce($3, managed),
                closed = coalesce($4, closed),
                head_sha = coalesce($5, head_sha),
                updated_at = now()
            where repository = $1 and number = $2
            returning {BINDING_COLUMNS}
            "#
        ))
        .bind(&repository)
        .bind(number)
        .bind(update.managed)
        .bind(update.closed)
        .bind(update.head_sha.as_deref())
        .fetch_optional(&self.pool)
        .await?
        .map(TryInto::try_into)
        .transpose()?
        .ok_or(SessionStoreError::PrBindingNotFound { repository, number })
    }

    /// The thread key of the one session assigned `sandbox_id`, or `None` when
    /// no session or more than one session claims it. A caller whose proxy
    /// names a sandbox is bound to a session only through an unambiguous
    /// assignment.
    pub async fn unique_session_for_sandbox(
        &self,
        sandbox_id: &str,
    ) -> Result<Option<ThreadKey>, SessionStoreError> {
        let rows: Vec<String> =
            sqlx::query_scalar("select thread_key from sessions where sandbox_id = $1 limit 2")
                .bind(sandbox_id)
                .fetch_all(&self.pool)
                .await?;
        match <[String; 1]>::try_from(rows) {
            Ok([thread_key]) => Ok(Some(parse_persisted(thread_key)?)),
            Err(_) => Ok(None),
        }
    }
}

fn database_error_code(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .map(std::borrow::Cow::into_owned)
}

#[derive(Debug, FromRow)]
struct PrBindingRow {
    repository: String,
    number: i64,
    thread_key: String,
    pr_id: i64,
    pr_node_id: String,
    author_github_id: i64,
    author_login: String,
    sandbox_id: String,
    head_ref: String,
    base_ref: String,
    head_sha: String,
    managed: bool,
    closed: bool,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<PrBindingRow> for PrBinding {
    type Error = SessionStoreError;

    fn try_from(row: PrBindingRow) -> Result<Self, Self::Error> {
        Ok(Self {
            repository: row.repository,
            number: row.number,
            thread_key: parse_persisted(row.thread_key)?,
            pr_id: row.pr_id,
            pr_node_id: row.pr_node_id,
            author_github_id: row.author_github_id,
            author_login: row.author_login,
            sandbox_id: row.sandbox_id,
            head_ref: row.head_ref,
            base_ref: row.base_ref,
            head_sha: row.head_sha,
            managed: row.managed,
            closed: row.closed,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use centaur_session_core::HarnessType;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    async fn test_store() -> Option<PgSessionStore> {
        let Ok(url) = std::env::var("SESSION_RUNTIME_TEST_DATABASE_URL") else {
            eprintln!("skipping: SESSION_RUNTIME_TEST_DATABASE_URL not set");
            return None;
        };
        let store = PgSessionStore::connect(&url)
            .await
            .expect("connect test db");
        store
            .run_migrations(crate::TextSearchBackend::Postgres)
            .await
            .expect("run migrations");
        Some(store)
    }

    async fn session(store: &PgSessionStore) -> ThreadKey {
        let thread_key = ThreadKey::parse(format!(
            "slack:T0TEST:C0TEST:{}.000100",
            Uuid::new_v4().simple()
        ))
        .unwrap();
        store
            .create_or_get_session(
                &thread_key,
                &HarnessType::Codex,
                None,
                json!({}),
                BTreeMap::new(),
            )
            .await
            .unwrap();
        thread_key
    }

    fn new_binding(thread_key: &ThreadKey) -> NewPrBinding {
        let unique = Uuid::new_v4().simple().to_string();
        NewPrBinding {
            repository: format!("Acme/widgets-{unique}"),
            number: 7,
            thread_key: thread_key.clone(),
            pr_id: 1_000_007,
            pr_node_id: format!("PR_node_{unique}"),
            author_github_id: 90_210_001,
            author_login: "ada".to_owned(),
            sandbox_id: format!("sbx-{unique}"),
            head_ref: "fix/widgets".to_owned(),
            base_ref: "main".to_owned(),
            head_sha: "abc1234567".to_owned(),
        }
    }

    #[test]
    fn repositories_are_lowercased_owner_repo() {
        assert_eq!(
            normalize_pr_repository(" Acme/Widgets.rs ").unwrap(),
            "acme/widgets.rs"
        );
        for invalid in [
            "acme",
            "acme/widgets/extra",
            "/widgets",
            "acme/",
            "acme/wid gets",
            "./widgets",
            "acme/..",
            "",
        ] {
            assert!(normalize_pr_repository(invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn insert_stores_an_unmanaged_open_binding_and_is_idempotent() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = session(&store).await;
        let new = new_binding(&thread_key);

        let binding = store.insert_pr_binding(&new).await.unwrap();
        assert_eq!(binding.repository, new.repository.to_ascii_lowercase());
        assert_eq!(binding.thread_key, thread_key);
        assert_eq!(binding.pr_node_id, new.pr_node_id);
        assert_eq!(binding.head_sha, "abc1234567");
        assert!(!binding.managed);
        assert!(!binding.closed);

        // A retry of the same create result returns the stored row.
        let again = store.insert_pr_binding(&new).await.unwrap();
        assert_eq!(again, binding);

        // Lookups are case-insensitive on the repository.
        let found = store
            .pr_binding(&new.repository.to_ascii_uppercase(), new.number)
            .await
            .unwrap();
        assert_eq!(found, Some(binding));
        assert_eq!(store.pr_binding("acme/missing", 1).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_binding_is_never_rebound_to_another_session_or_node() {
        let Some(store) = test_store().await else {
            return;
        };
        let first = session(&store).await;
        let second = session(&store).await;
        let new = new_binding(&first);
        let original = store.insert_pr_binding(&new).await.unwrap();

        let other_session = NewPrBinding {
            thread_key: second.clone(),
            ..new.clone()
        };
        let other_node = NewPrBinding {
            pr_node_id: "PR_node_other".to_owned(),
            ..new.clone()
        };
        for attempt in [other_session, other_node] {
            assert!(matches!(
                store.insert_pr_binding(&attempt).await,
                Err(SessionStoreError::PrBindingConflict { .. })
            ));
        }
        // The same node id cannot also be bound under a different PR number.
        let same_node_other_number = NewPrBinding {
            number: 8,
            thread_key: second,
            ..new.clone()
        };
        assert!(matches!(
            store.insert_pr_binding(&same_node_other_number).await,
            Err(SessionStoreError::PrBindingConflict { .. })
        ));
        assert_eq!(
            store.pr_binding(&new.repository, 7).await.unwrap(),
            Some(original)
        );
        assert_eq!(store.pr_binding(&new.repository, 8).await.unwrap(), None);
    }

    #[tokio::test]
    async fn insert_requires_an_existing_session_and_valid_identity() {
        let Some(store) = test_store().await else {
            return;
        };
        let missing =
            ThreadKey::parse(format!("slack:T0TEST:C0TEST:{}.1", Uuid::new_v4().simple())).unwrap();
        assert!(matches!(
            store.insert_pr_binding(&new_binding(&missing)).await,
            Err(SessionStoreError::NotFound { .. })
        ));
        let thread_key = session(&store).await;
        for invalid in [
            NewPrBinding {
                repository: "acme".to_owned(),
                ..new_binding(&thread_key)
            },
            NewPrBinding {
                number: 0,
                ..new_binding(&thread_key)
            },
        ] {
            assert!(matches!(
                store.insert_pr_binding(&invalid).await,
                Err(SessionStoreError::InvalidPrBinding(_))
            ));
        }
    }

    #[tokio::test]
    async fn state_updates_change_only_the_named_fields_and_never_create_rows() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = session(&store).await;
        let new = new_binding(&thread_key);
        store.insert_pr_binding(&new).await.unwrap();

        let managed = store
            .update_pr_binding_state(
                &new.repository,
                new.number,
                &PrBindingStateUpdate {
                    managed: Some(true),
                    head_sha: Some("def4567890".to_owned()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(managed.managed);
        assert!(!managed.closed);
        assert_eq!(managed.head_sha, "def4567890");
        assert_eq!(managed.thread_key, thread_key);

        let closed = store
            .update_pr_binding_state(
                &new.repository,
                new.number,
                &PrBindingStateUpdate {
                    closed: Some(true),
                    managed: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(closed.closed);
        assert!(!closed.managed);
        assert_eq!(closed.head_sha, "def4567890");

        assert!(matches!(
            store
                .update_pr_binding_state(&new.repository, 99, &PrBindingStateUpdate::default())
                .await,
            Err(SessionStoreError::PrBindingNotFound { .. })
        ));
        assert!(matches!(
            store
                .update_pr_binding_state(
                    &new.repository,
                    new.number,
                    &PrBindingStateUpdate {
                        head_sha: Some(" ".to_owned()),
                        ..Default::default()
                    },
                )
                .await,
            Err(SessionStoreError::InvalidPrBinding(_))
        ));
    }

    #[test]
    fn state_updates_reject_unknown_fields() {
        assert!(
            serde_json::from_value::<PrBindingStateUpdate>(json!({"thread_key": "slack:T:C:1"}))
                .is_err()
        );
        assert_eq!(
            serde_json::from_value::<PrBindingStateUpdate>(json!({"managed": true})).unwrap(),
            PrBindingStateUpdate {
                managed: Some(true),
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn a_sandbox_maps_to_a_session_only_when_exactly_one_claims_it() {
        let Some(store) = test_store().await else {
            return;
        };
        let sandbox_id = format!("sbx-{}", Uuid::new_v4().simple());
        assert_eq!(
            store.unique_session_for_sandbox(&sandbox_id).await.unwrap(),
            None
        );

        let first = session(&store).await;
        store
            .update_sandbox_id(&first, Some(&sandbox_id))
            .await
            .unwrap();
        assert_eq!(
            store.unique_session_for_sandbox(&sandbox_id).await.unwrap(),
            Some(first)
        );

        let second = session(&store).await;
        store
            .update_sandbox_id(&second, Some(&sandbox_id))
            .await
            .unwrap();
        assert_eq!(
            store.unique_session_for_sandbox(&sandbox_id).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn deleting_a_session_removes_its_bindings() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = session(&store).await;
        let new = new_binding(&thread_key);
        store.insert_pr_binding(&new).await.unwrap();

        sqlx::query("delete from sessions where thread_key = $1")
            .bind(thread_key.as_str())
            .execute(store.pool())
            .await
            .unwrap();
        assert_eq!(
            store.pr_binding(&new.repository, new.number).await.unwrap(),
            None
        );
    }
}
