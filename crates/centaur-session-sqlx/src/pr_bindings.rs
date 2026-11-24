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
     managed, closed, managed_observed_at, closed_observed_at, head_observed_at, \
     created_at, updated_at";

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
    /// The fetched PR's `updated_at` behind each field's latest applied write.
    #[serde(with = "time::serde::rfc3339::option")]
    pub managed_observed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub closed_observed_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub head_observed_at: Option<OffsetDateTime>,
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

/// The result of [`PgSessionStore::update_pr_binding_state`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrBindingStateOutcome {
    /// The binding after the call: the updated row, or the unchanged one when
    /// every provided field was older than what the binding already reflects.
    pub binding: PrBinding,
    /// Whether any provided field was applied.
    pub applied: bool,
}

/// The outcome of [`PgSessionStore::claim_pr_ci_notice`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrCiNoticeClaim {
    /// This caller owns the notice and should post it.
    Claimed,
    /// A notice for this PR and head was already claimed.
    Duplicate,
    /// The binding is not, any longer, an open, managed binding of this PR at
    /// this head: nothing was claimed.
    NotEligible,
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

    /// Applies `update` to an existing binding of the PR `pr_node_id`. Fails
    /// with [`SessionStoreError::PrBindingNotFound`] when none exists and with
    /// [`SessionStoreError::PrBindingConflict`] when the binding is for a
    /// different PR (a reused number): state updates never create or rebind a
    /// row.
    ///
    /// `observed_updated_at` is the `updated_at` of the PR as fetched from
    /// GitHub when the caller formed this write. Each provided field is
    /// ordered on its own: it is applied only when the field has no stored
    /// observation or its stored one is not later, and then records this one.
    /// An older write to a field changes nothing in that field, and never
    /// disturbs another field's newer value. `applied` is true when any
    /// provided field was applied. Webhook handlers run concurrently and
    /// arrive out of order, so this is what makes close, reopen, assignment
    /// and head writes converge on the newest fetch. `None` writes
    /// unconditionally and leaves the stored observations alone; only
    /// server-side callers that did not fetch the PR use it.
    pub async fn update_pr_binding_state(
        &self,
        repository: &str,
        number: i64,
        pr_node_id: &str,
        update: &PrBindingStateUpdate,
        observed_updated_at: Option<OffsetDateTime>,
    ) -> Result<PrBindingStateOutcome, SessionStoreError> {
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
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query_as::<_, PrBindingRow>(&format!(
            "select {BINDING_COLUMNS} from pr_session_bindings \
             where repository = $1 and number = $2 for update"
        ))
        .bind(&repository)
        .bind(number)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| SessionStoreError::PrBindingNotFound {
            repository: repository.clone(),
            number,
        })?;
        if current.pr_node_id != pr_node_id {
            return Err(SessionStoreError::PrBindingConflict {
                repository,
                number,
                reason: "the binding is for a different pull request".to_owned(),
            });
        }
        // A field applies when it is provided and no later observation of it
        // is already stored.
        let applies = |provided: bool, stored: Option<OffsetDateTime>| {
            provided
                && match (observed_updated_at, stored) {
                    (Some(observed), Some(stored)) => stored <= observed,
                    _ => true,
                }
        };
        let apply_managed = applies(update.managed.is_some(), current.managed_observed_at);
        let apply_closed = applies(update.closed.is_some(), current.closed_observed_at);
        let apply_head = applies(update.head_sha.is_some(), current.head_observed_at);
        if !(apply_managed || apply_closed || apply_head) {
            return Ok(PrBindingStateOutcome {
                binding: current.try_into()?,
                applied: false,
            });
        }
        let stamp = |apply: bool, stored: Option<OffsetDateTime>| {
            if apply {
                observed_updated_at.or(stored)
            } else {
                stored
            }
        };
        let row = sqlx::query_as::<_, PrBindingRow>(&format!(
            r#"
            update pr_session_bindings
            set managed = $3, managed_observed_at = $4,
                closed = $5, closed_observed_at = $6,
                head_sha = $7, head_observed_at = $8,
                updated_at = now()
            where repository = $1 and number = $2 and pr_node_id = $9
            returning {BINDING_COLUMNS}
            "#
        ))
        .bind(&repository)
        .bind(number)
        .bind(if apply_managed {
            update.managed.unwrap_or(current.managed)
        } else {
            current.managed
        })
        .bind(stamp(apply_managed, current.managed_observed_at))
        .bind(if apply_closed {
            update.closed.unwrap_or(current.closed)
        } else {
            current.closed
        })
        .bind(stamp(apply_closed, current.closed_observed_at))
        .bind(if apply_head {
            update.head_sha.as_deref().unwrap_or(&current.head_sha)
        } else {
            &current.head_sha
        })
        .bind(stamp(apply_head, current.head_observed_at))
        .bind(pr_node_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(PrBindingStateOutcome {
            binding: row.try_into()?,
            applied: true,
        })
    }

    /// Claims the failed-CI notice for `repository#number` at `head_sha`, in
    /// one conditional insert that succeeds only while the binding row is still
    /// this PR (`pr_node_id`), at this head, managed and not closed. A close,
    /// unassignment or head change committed after the caller read the binding
    /// therefore cannot be missed: the insert finds the row changed and claims
    /// nothing. The binding row is read `FOR SHARE`, so a close, unassignment
    /// or head change still in flight (holding the row's lock) is waited for
    /// and its result decides, instead of the claim reading the old row. The notice is keyed by `(pr_node_id, head_sha)`, so a second
    /// claim for the same PR and head is [`PrCiNoticeClaim::Duplicate`].
    /// Release the claim if the post fails, so a later event can retry.
    pub async fn claim_pr_ci_notice(
        &self,
        repository: &str,
        number: i64,
        pr_node_id: &str,
        head_sha: &str,
    ) -> Result<PrCiNoticeClaim, SessionStoreError> {
        let repository = normalize_pr_repository(repository)?;
        validate_number(number)?;
        let inserted = sqlx::query(
            r#"
            insert into pr_ci_notices (pr_node_id, head_sha, thread_key)
            select b.pr_node_id, b.head_sha, b.thread_key
            from pr_session_bindings b
            where b.repository = $1 and b.number = $2
              and b.pr_node_id = $3 and b.head_sha = $4
              and b.managed and not b.closed
            for share of b
            on conflict (pr_node_id, head_sha) do nothing
            "#,
        )
        .bind(&repository)
        .bind(number)
        .bind(pr_node_id)
        .bind(head_sha)
        .execute(&self.pool)
        .await?;
        if inserted.rows_affected() == 1 {
            return Ok(PrCiNoticeClaim::Claimed);
        }
        let exists: bool = sqlx::query_scalar(
            "select exists (select 1 from pr_ci_notices where pr_node_id = $1 and head_sha = $2)",
        )
        .bind(pr_node_id)
        .bind(head_sha)
        .fetch_one(&self.pool)
        .await?;
        Ok(if exists {
            PrCiNoticeClaim::Duplicate
        } else {
            PrCiNoticeClaim::NotEligible
        })
    }

    /// Releases a claim from [`Self::claim_pr_ci_notice`] whose post failed.
    pub async fn release_pr_ci_notice(
        &self,
        pr_node_id: &str,
        head_sha: &str,
    ) -> Result<(), SessionStoreError> {
        sqlx::query("delete from pr_ci_notices where pr_node_id = $1 and head_sha = $2")
            .bind(pr_node_id)
            .bind(head_sha)
            .execute(&self.pool)
            .await?;
        Ok(())
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
    managed_observed_at: Option<OffsetDateTime>,
    closed_observed_at: Option<OffsetDateTime>,
    head_observed_at: Option<OffsetDateTime>,
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
            managed_observed_at: row.managed_observed_at,
            closed_observed_at: row.closed_observed_at,
            head_observed_at: row.head_observed_at,
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
                &new.pr_node_id,
                &PrBindingStateUpdate {
                    managed: Some(true),
                    head_sha: Some("def4567890".to_owned()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap()
            .binding;
        assert!(managed.managed);
        assert!(!managed.closed);
        assert_eq!(managed.head_sha, "def4567890");
        assert_eq!(managed.thread_key, thread_key);

        let closed = store
            .update_pr_binding_state(
                &new.repository,
                new.number,
                &new.pr_node_id,
                &PrBindingStateUpdate {
                    closed: Some(true),
                    managed: Some(false),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap()
            .binding;
        assert!(closed.closed);
        assert!(!closed.managed);
        assert_eq!(closed.head_sha, "def4567890");

        assert!(matches!(
            store
                .update_pr_binding_state(
                    &new.repository,
                    99,
                    &new.pr_node_id,
                    &PrBindingStateUpdate::default(),
                    None
                )
                .await,
            Err(SessionStoreError::PrBindingNotFound { .. })
        ));
        // A different PR that reuses the number cannot drive this binding.
        assert!(matches!(
            store
                .update_pr_binding_state(
                    &new.repository,
                    new.number,
                    "PR_someone_elses",
                    &PrBindingStateUpdate {
                        managed: Some(true),
                        closed: Some(false),
                        ..Default::default()
                    },
                    None,
                )
                .await,
            Err(SessionStoreError::PrBindingConflict { .. })
        ));
        let after = store
            .pr_binding(&new.repository, new.number)
            .await
            .unwrap()
            .unwrap();
        assert!(after.closed && !after.managed);
        assert!(matches!(
            store
                .update_pr_binding_state(
                    &new.repository,
                    new.number,
                    &new.pr_node_id,
                    &PrBindingStateUpdate {
                        head_sha: Some(" ".to_owned()),
                        ..Default::default()
                    },
                    None,
                )
                .await,
            Err(SessionStoreError::InvalidPrBinding(_))
        ));
    }

    fn at(seconds: i64) -> Option<OffsetDateTime> {
        Some(OffsetDateTime::from_unix_timestamp(1_800_000_000 + seconds).unwrap())
    }

    async fn write(
        store: &PgSessionStore,
        new: &NewPrBinding,
        update: PrBindingStateUpdate,
        observed: Option<OffsetDateTime>,
    ) -> PrBindingStateOutcome {
        store
            .update_pr_binding_state(
                &new.repository,
                new.number,
                &new.pr_node_id,
                &update,
                observed,
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn reordered_writes_to_one_field_converge_on_the_newest_observation() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = session(&store).await;
        let new = new_binding(&thread_key);
        store.insert_pr_binding(&new).await.unwrap();
        let reopen = PrBindingStateUpdate {
            closed: Some(false),
            ..Default::default()
        };
        let close = PrBindingStateUpdate {
            closed: Some(true),
            ..Default::default()
        };

        // The reopen (observed later) is handled before the close it follows.
        let outcome = write(&store, &new, close.clone(), at(10)).await;
        assert!(outcome.applied && outcome.binding.closed);
        let outcome = write(&store, &new, reopen.clone(), at(30)).await;
        assert!(outcome.applied && !outcome.binding.closed);
        // The older close arrives last and changes nothing.
        let outcome = write(&store, &new, close.clone(), at(10)).await;
        assert!(!outcome.applied);
        assert!(!outcome.binding.closed);
        assert_eq!(outcome.binding.closed_observed_at, at(30));
        // The same observation, or a newer one, applies.
        assert!(write(&store, &new, close, at(30)).await.applied);
        // An unobserved server-side write applies and keeps the observation.
        let outcome = write(&store, &new, reopen, None).await;
        assert!(outcome.applied && !outcome.binding.closed);
        assert_eq!(outcome.binding.closed_observed_at, at(30));
    }

    #[tokio::test]
    async fn each_field_is_ordered_independently_of_the_others() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = session(&store).await;
        let new = new_binding(&thread_key);
        store.insert_pr_binding(&new).await.unwrap();

        // A head write observed at T2 lands first.
        let outcome = write(
            &store,
            &new,
            PrBindingStateUpdate {
                head_sha: Some("fedcba9".to_owned()),
                ..Default::default()
            },
            at(20),
        )
        .await;
        assert!(outcome.applied);

        // The assignment observed earlier, at T1, arrives afterwards. It is
        // still the newest word on `managed`, so it applies; the head stays.
        let outcome = write(
            &store,
            &new,
            PrBindingStateUpdate {
                managed: Some(true),
                ..Default::default()
            },
            at(10),
        )
        .await;
        assert!(outcome.applied);
        assert!(outcome.binding.managed);
        assert_eq!(outcome.binding.head_sha, "fedcba9");
        assert_eq!(outcome.binding.managed_observed_at, at(10));
        assert_eq!(outcome.binding.head_observed_at, at(20));
        assert_eq!(outcome.binding.closed_observed_at, None);

        // An older head write cannot move the head, and `applied` is false.
        let outcome = write(
            &store,
            &new,
            PrBindingStateUpdate {
                head_sha: Some("old0000".to_owned()),
                ..Default::default()
            },
            at(15),
        )
        .await;
        assert!(!outcome.applied);
        assert_eq!(outcome.binding.head_sha, "fedcba9");

        // A write that mixes fields applies the fresh ones and skips the
        // stale ones, and reports `applied` for the former.
        let outcome = write(
            &store,
            &new,
            PrBindingStateUpdate {
                managed: Some(false),
                head_sha: Some("old0001".to_owned()),
                ..Default::default()
            },
            at(15),
        )
        .await;
        assert!(outcome.applied);
        assert!(!outcome.binding.managed);
        assert_eq!(outcome.binding.head_sha, "fedcba9");
        assert_eq!(outcome.binding.managed_observed_at, at(15));
        assert_eq!(outcome.binding.head_observed_at, at(20));
    }

    #[tokio::test]
    async fn the_first_observation_applies_whatever_its_time() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = session(&store).await;
        let new = new_binding(&thread_key);
        store.insert_pr_binding(&new).await.unwrap();
        let before = store.pr_binding(&new.repository, 7).await.unwrap().unwrap();
        assert_eq!(
            (
                before.managed_observed_at,
                before.closed_observed_at,
                before.head_observed_at
            ),
            (None, None, None)
        );
        let outcome = write(
            &store,
            &new,
            PrBindingStateUpdate {
                managed: Some(true),
                ..Default::default()
            },
            Some(OffsetDateTime::from_unix_timestamp(1).unwrap()),
        )
        .await;
        assert!(outcome.applied);
    }

    /// A managed, open binding at head `abc1234567` and its notice claims.
    async fn managed_binding(store: &PgSessionStore) -> NewPrBinding {
        let thread_key = session(store).await;
        let new = new_binding(&thread_key);
        store.insert_pr_binding(&new).await.unwrap();
        write(
            store,
            &new,
            PrBindingStateUpdate {
                managed: Some(true),
                ..Default::default()
            },
            None,
        )
        .await;
        new
    }

    async fn claim(store: &PgSessionStore, new: &NewPrBinding, head: &str) -> PrCiNoticeClaim {
        store
            .claim_pr_ci_notice(&new.repository, new.number, &new.pr_node_id, head)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_ci_notice_is_claimed_once_per_pr_and_head_until_released() {
        let Some(store) = test_store().await else {
            return;
        };
        let new = managed_binding(&store).await;

        assert_eq!(
            claim(&store, &new, "abc1234567").await,
            PrCiNoticeClaim::Claimed
        );
        assert_eq!(
            claim(&store, &new, "abc1234567").await,
            PrCiNoticeClaim::Duplicate
        );

        // Another head is a separate notice, once the binding is at it.
        write(
            &store,
            &new,
            PrBindingStateUpdate {
                head_sha: Some("def4567890".to_owned()),
                ..Default::default()
            },
            None,
        )
        .await;
        assert_eq!(
            claim(&store, &new, "def4567890").await,
            PrCiNoticeClaim::Claimed
        );

        store
            .release_pr_ci_notice(&new.pr_node_id, "def4567890")
            .await
            .unwrap();
        assert_eq!(
            claim(&store, &new, "def4567890").await,
            PrCiNoticeClaim::Claimed
        );
        // Releasing what was never claimed is harmless.
        store.release_pr_ci_notice("PR_never", "abc").await.unwrap();
    }

    #[tokio::test]
    async fn a_claim_waits_for_an_in_flight_invalidation_and_then_loses_to_it() {
        use std::time::Duration;

        let Some(store) = test_store().await else {
            return;
        };
        let new = managed_binding(&store).await;

        // Connection A closes the PR and has not committed yet.
        let mut invalidation = store.pool().begin().await.unwrap();
        let closed = sqlx::query(
            "update pr_session_bindings set closed = true where repository = $1 and number = $2",
        )
        .bind(new.repository.to_ascii_lowercase())
        .bind(new.number)
        .execute(&mut *invalidation)
        .await
        .unwrap();
        assert_eq!(closed.rows_affected(), 1);

        // Connection B claims the notice. It must block on A's row lock
        // rather than read the still-open row and claim.
        let mut claiming = tokio::spawn({
            let store = store.clone();
            let new = new.clone();
            async move { claim(&store, &new, "abc1234567").await }
        });
        let early = tokio::time::timeout(Duration::from_millis(500), &mut claiming).await;
        assert!(
            early.is_err(),
            "the claim did not wait for the in-flight invalidation: {early:?}"
        );

        // A commits: the claim re-evaluates against the closed row.
        invalidation.commit().await.unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(10), claiming)
            .await
            .expect("the claim finishes once the invalidation commits")
            .unwrap();
        assert_eq!(outcome, PrCiNoticeClaim::NotEligible);

        // Nothing was claimed: reopened, the notice can still be claimed.
        write(
            &store,
            &new,
            PrBindingStateUpdate {
                closed: Some(false),
                ..Default::default()
            },
            None,
        )
        .await;
        assert_eq!(
            claim(&store, &new, "abc1234567").await,
            PrCiNoticeClaim::Claimed
        );
    }

    #[tokio::test]
    async fn a_claim_needs_the_binding_to_still_be_this_pr_at_this_head_managed_and_open() {
        let Some(store) = test_store().await else {
            return;
        };
        let new = managed_binding(&store).await;
        let head = "abc1234567";

        // A different PR node id, a different head, an unknown number.
        assert_eq!(
            store
                .claim_pr_ci_notice(&new.repository, new.number, "PR_someone_elses", head)
                .await
                .unwrap(),
            PrCiNoticeClaim::NotEligible
        );
        assert_eq!(
            claim(&store, &new, "other00").await,
            PrCiNoticeClaim::NotEligible
        );
        assert_eq!(
            store
                .claim_pr_ci_notice(&new.repository, 99, &new.pr_node_id, head)
                .await
                .unwrap(),
            PrCiNoticeClaim::NotEligible
        );

        // Unassigned, then closed.
        write(
            &store,
            &new,
            PrBindingStateUpdate {
                managed: Some(false),
                ..Default::default()
            },
            None,
        )
        .await;
        assert_eq!(
            claim(&store, &new, head).await,
            PrCiNoticeClaim::NotEligible
        );
        write(
            &store,
            &new,
            PrBindingStateUpdate {
                managed: Some(true),
                closed: Some(true),
                ..Default::default()
            },
            None,
        )
        .await;
        assert_eq!(
            claim(&store, &new, head).await,
            PrCiNoticeClaim::NotEligible
        );

        // None of the refusals left a claim behind.
        write(
            &store,
            &new,
            PrBindingStateUpdate {
                closed: Some(false),
                ..Default::default()
            },
            None,
        )
        .await;
        assert_eq!(claim(&store, &new, head).await, PrCiNoticeClaim::Claimed);
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
    async fn an_execution_records_the_requester_api_rs_intended_for_it() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = session(&store).await;
        let execution = store
            .create_execution(&thread_key, None, json!({}))
            .await
            .unwrap()
            .execution;
        // Nothing recorded yet is not the same as "no requester".
        assert_eq!(crate::execution_requester(&execution), None);

        store
            .set_execution_requester(&execution.execution_id, Some("prn_alice"))
            .await
            .unwrap();
        let recorded = store
            .latest_execution_for_thread(&thread_key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            crate::execution_requester(&recorded),
            Some(Some("prn_alice"))
        );

        store
            .set_execution_requester(&execution.execution_id, None)
            .await
            .unwrap();
        let recorded = store
            .latest_execution_for_thread(&thread_key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(crate::execution_requester(&recorded), Some(None));

        assert!(matches!(
            store.set_execution_requester("exe_missing", None).await,
            Err(SessionStoreError::ExecutionNotFound { .. })
        ));

        // Other crates' tests share this database and adopt active executions.
        sqlx::query("delete from sessions where thread_key = $1")
            .bind(thread_key.as_str())
            .execute(store.pool())
            .await
            .unwrap();
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
