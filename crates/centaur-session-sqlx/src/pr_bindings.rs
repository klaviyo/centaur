//! PR session bindings: the durable link from a GitHub pull request to the
//! exact Centaur session that opened it, and the per-binding FIFO of GitHub
//! follow-up work routed back into that session.
//!
//! A binding is `registered` by the session's own principal, then `verified`
//! (or `rejected`) by githubbot from a signed webhook plus a fresh GitHub API
//! fetch. Only a verified, managed binding queues follow-ups; dispatch drains
//! the queue one turn at a time on the bound thread key.

use std::str::FromStr;

use centaur_session_core::{ExecutionStatus, SessionStatus, ThreadKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, Postgres, Transaction};
use time::OffsetDateTime;

use crate::{PgSessionStore, SessionStoreError, parse_persisted, prefixed_id};

/// The only push authorization v1 dispatches with: prepare the fix, then ask
/// once for an authorized human push. Existing credential plumbing cannot
/// scope an unattended push to one repository, branch and grant.
pub const PR_FOLLOWUP_PUSH_AUTHORIZATION: &str = "prepare_and_ask";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrBindingState {
    Registered,
    Verified,
    Rejected,
}

impl PrBindingState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Verified => "verified",
            Self::Rejected => "rejected",
        }
    }
}

impl FromStr for PrBindingState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "registered" => Ok(Self::Registered),
            "verified" => Ok(Self::Verified),
            "rejected" => Ok(Self::Rejected),
            other => Err(format!("unknown PR binding state {other:?}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrFollowupKind {
    Review,
    ReviewComment,
    IssueComment,
    Mention,
    CiFailure,
    Conflict,
}

impl PrFollowupKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::ReviewComment => "review_comment",
            Self::IssueComment => "issue_comment",
            Self::Mention => "mention",
            Self::CiFailure => "ci_failure",
            Self::Conflict => "conflict",
        }
    }

    /// The `dedupe_key` prefix that names this kind's logical item.
    const fn dedupe_prefix(self) -> &'static str {
        match self {
            Self::Review => "review:",
            Self::ReviewComment | Self::IssueComment | Self::Mention => "comment:",
            Self::CiFailure => "ci:",
            Self::Conflict => "conflict:",
        }
    }

    /// Whether a queued item stays valid after the PR head moves. Reviews and
    /// direct requests are about the PR, not one commit; line comments, CI
    /// results and conflicts are about the head they were raised on.
    pub const fn survives_head_change(self) -> bool {
        matches!(self, Self::Review | Self::IssueComment | Self::Mention)
    }
}

impl FromStr for PrFollowupKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "review" => Ok(Self::Review),
            "review_comment" => Ok(Self::ReviewComment),
            "issue_comment" => Ok(Self::IssueComment),
            "mention" => Ok(Self::Mention),
            "ci_failure" => Ok(Self::CiFailure),
            "conflict" => Ok(Self::Conflict),
            other => Err(format!("unknown PR follow-up kind {other:?}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrFollowupStatus {
    Queued,
    Dispatched,
    Dropped,
}

impl PrFollowupStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Dispatched => "dispatched",
            Self::Dropped => "dropped",
        }
    }
}

impl FromStr for PrFollowupStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "dispatched" => Ok(Self::Dispatched),
            "dropped" => Ok(Self::Dropped),
            other => Err(format!("unknown PR follow-up status {other:?}")),
        }
    }
}

/// Why queued follow-ups were dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrFollowupDropReason {
    Unassigned,
    Closed,
    StaleHead,
}

impl PrFollowupDropReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unassigned => "unassigned",
            Self::Closed => "closed",
            Self::StaleHead => "stale_head",
        }
    }
}

/// A PR's lowercased `owner/repo` and number, validated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrRef {
    repository: String,
    number: i64,
}

impl PrRef {
    pub fn new(repository: &str, number: i64) -> Result<Self, SessionStoreError> {
        let repository = repository.trim().to_ascii_lowercase();
        let mut segments = repository.split('/');
        let valid_segment = |segment: Option<&str>| {
            segment.is_some_and(|segment| {
                !segment.is_empty()
                    && segment.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
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
        if number <= 0 {
            return Err(SessionStoreError::InvalidPrBinding(
                "number must be a positive integer".to_owned(),
            ));
        }
        Ok(Self { repository, number })
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub const fn number(&self) -> i64 {
        self.number
    }

    fn not_found(&self) -> SessionStoreError {
        SessionStoreError::PrBindingNotFound {
            repository: self.repository.clone(),
            number: self.number,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PrBinding {
    pub repository: String,
    pub number: i64,
    pub thread_key: ThreadKey,
    pub state: PrBindingState,
    pub pr_node_id: Option<String>,
    pub pr_author_login: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub pr_created_at: Option<OffsetDateTime>,
    pub head_ref: Option<String>,
    pub managed: bool,
    pub head_sha: Option<String>,
    pub automation_paused: bool,
    pub pause_reason: Option<String>,
    pub push_grant: Option<Value>,
    pub blocker: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PrFollowup {
    pub id: String,
    pub repository: String,
    pub number: i64,
    pub kind: PrFollowupKind,
    pub dedupe_key: String,
    pub head_sha: String,
    pub actor: Value,
    pub prompt: Value,
    pub reply_target: Option<Value>,
    pub status: PrFollowupStatus,
    pub drop_reason: Option<String>,
    pub execution_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl PrFollowup {
    /// The GitHub user id (digits) of the person whose event queued this item.
    pub fn actor_github_user_id(&self) -> Option<&str> {
        self.actor.get("github_user_id").and_then(Value::as_str)
    }

    pub fn actor_github_login(&self) -> Option<&str> {
        self.actor.get("github_login").and_then(Value::as_str)
    }
}

/// A follow-up as githubbot submits it, before it is queued.
#[derive(Clone, Debug, Deserialize)]
pub struct NewPrFollowup {
    pub kind: PrFollowupKind,
    pub dedupe_key: String,
    pub head_sha: String,
    pub actor: Value,
    pub prompt: Value,
    #[serde(default)]
    pub reply_target: Option<Value>,
}

impl NewPrFollowup {
    /// Checks the shape the dispatcher relies on: a kind-matching dedupe key,
    /// a numeric actor id from the signed webhook, text-only prompt parts and
    /// a GitHub reply target.
    pub fn validate(&self) -> Result<(), SessionStoreError> {
        let invalid = |message: &str| Err(SessionStoreError::InvalidPrBinding(message.to_owned()));
        let prefix = self.kind.dedupe_prefix();
        match self.dedupe_key.strip_prefix(prefix) {
            Some(rest) if !rest.trim().is_empty() => {}
            _ => {
                return Err(SessionStoreError::InvalidPrBinding(format!(
                    "dedupe_key for {} must look like {prefix}<id>",
                    self.kind.as_str()
                )));
            }
        }
        if self.head_sha.trim().is_empty() {
            return invalid("head_sha is required");
        }
        let actor_id = self.actor.get("github_user_id").and_then(Value::as_str);
        if !actor_id.is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())) {
            return invalid("actor.github_user_id must be a string of digits");
        }
        if !self
            .actor
            .get("github_login")
            .and_then(Value::as_str)
            .is_some_and(|login| !login.trim().is_empty())
        {
            return invalid("actor.github_login is required");
        }
        let Some(parts) = self.prompt.as_array().filter(|parts| !parts.is_empty()) else {
            return invalid("prompt must be a non-empty array of message parts");
        };
        let text_only = parts.iter().all(|part| {
            part.get("type").and_then(Value::as_str) == Some("text")
                && part.get("text").is_some_and(Value::is_string)
        });
        if !text_only {
            return invalid("prompt parts must be text parts");
        }
        match &self.reply_target {
            None | Some(Value::Null) => {}
            Some(target)
                if target.get("type").and_then(Value::as_str) == Some("github_comment") => {}
            Some(_) => return invalid("reply_target.type must be github_comment"),
        }
        Ok(())
    }
}

/// The GitHub facts githubbot fetched for a registered PR.
#[derive(Clone, Debug, Deserialize)]
pub struct PrVerification {
    pub node_id: String,
    pub author_login: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub head_ref: String,
    pub head_sha: String,
}

/// A partial binding update; an absent field is left unchanged, and a
/// present `null` clears it.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PrBindingStateUpdate {
    #[serde(default)]
    pub managed: Option<bool>,
    #[serde(default)]
    pub head_sha: Option<String>,
    #[serde(default)]
    pub closed: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    pub automation_paused: Option<Option<PauseRequest>>,
    #[serde(default, deserialize_with = "present")]
    pub push_grant: Option<Option<Value>>,
    #[serde(default, deserialize_with = "present")]
    pub blocker: Option<Option<String>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PauseRequest {
    pub reason: String,
}

/// Distinguishes a field sent as `null` (`Some(None)`) from an absent one
/// (`None`, through `#[serde(default)]`).
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A push grant as recorded: the instruction is kept for audit, but it never
/// authorizes anything in v1, so `active` is always false.
pub fn inactive_push_grant(grant: Value) -> Result<Value, SessionStoreError> {
    let Value::Object(mut fields) = grant else {
        return Err(SessionStoreError::InvalidPrBinding(
            "push_grant must be an object or null".to_owned(),
        ));
    };
    fields.insert("active".to_owned(), Value::Bool(false));
    Ok(Value::Object(fields))
}

/// The server-set execute metadata for a dispatched follow-up. The API strips
/// these keys from every external caller, so only dispatch can name a turn's
/// PR follow-up and its GitHub requester.
pub fn pr_followup_execute_metadata(followup: &PrFollowup) -> Value {
    json!({
        "source": "api-rs",
        "action": "pr_followup_dispatch",
        "pr_followup": {
            "repository": followup.repository,
            "number": followup.number,
            "followup_id": followup.id,
            "kind": followup.kind.as_str(),
            "reply_target": followup.reply_target,
        },
        "pr_followup_github_user_id": followup.actor_github_user_id(),
        "pr_followup_github_login": followup.actor_github_login(),
        "push_authorization": PR_FOLLOWUP_PUSH_AUTHORIZATION,
    })
}

/// A position in the listing of finished PR follow-up executions: the
/// execution's completion time, then its id. Executions are listed in that
/// order, and a completion time never changes once set, so the cursor is
/// stable and only moves forward.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrFollowupExecutionCursor {
    pub completed_at: OffsetDateTime,
    pub execution_id: String,
}

impl PrFollowupExecutionCursor {
    /// The opaque wire form, `<completed_at unix microseconds>:<execution id>`.
    pub fn encode(&self) -> String {
        let micros = self.completed_at.unix_timestamp_nanos() / 1_000;
        format!("{micros}:{}", self.execution_id)
    }

    pub fn parse(value: &str) -> Option<Self> {
        let (micros, execution_id) = value.split_once(':')?;
        let micros: i128 = micros.parse().ok()?;
        if execution_id.is_empty() {
            return None;
        }
        Some(Self {
            completed_at: OffsetDateTime::from_unix_timestamp_nanos(micros.checked_mul(1_000)?)
                .ok()?,
            execution_id: execution_id.to_owned(),
        })
    }
}

/// A finished execution that PR follow-up dispatch started. `pr_followup` is
/// only the server-set `pr_followup` object of its execute metadata, never
/// the requester or authorization fields beside it.
#[derive(Clone, Debug, PartialEq)]
pub struct PrFollowupExecution {
    pub execution_id: String,
    pub thread_key: ThreadKey,
    pub status: ExecutionStatus,
    pub pr_followup: Value,
    pub completed_at: OffsetDateTime,
}

impl PrFollowupExecution {
    pub fn cursor(&self) -> PrFollowupExecutionCursor {
        PrFollowupExecutionCursor {
            completed_at: self.completed_at,
            execution_id: self.execution_id.clone(),
        }
    }
}

const BINDING_COLUMNS: &str = "repository, number, thread_key, state, pr_node_id, pr_author_login, pr_created_at, head_ref, managed, head_sha, automation_paused, pause_reason, push_grant, blocker, created_at, updated_at";
const FOLLOWUP_COLUMNS: &str = "id, binding_repository, binding_number, kind, dedupe_key, head_sha, actor, prompt, reply_target, status, drop_reason, execution_id, created_at, updated_at";

impl PgSessionStore {
    /// Registers `pr` for `thread_key`. A new row, or a repeat by the same
    /// session while still `registered`, succeeds; anything else (another
    /// session, or a row githubbot already verified or rejected) is a
    /// conflict, so a binding is never silently moved.
    pub async fn register_pr_binding(
        &self,
        thread_key: &ThreadKey,
        pr: &PrRef,
    ) -> Result<PrBinding, SessionStoreError> {
        let row = sqlx::query_as::<_, PrBindingRow>(&format!(
            r#"
            insert into pr_session_bindings (repository, number, thread_key, state)
            values ($1, $2, $3, 'registered')
            on conflict (repository, number) do update
            set updated_at = now()
            where pr_session_bindings.state = 'registered'
              and pr_session_bindings.thread_key = excluded.thread_key
            returning {BINDING_COLUMNS}
            "#
        ))
        .bind(pr.repository())
        .bind(pr.number())
        .bind(thread_key.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| match foreign_key_violation(&error) {
            true => SessionStoreError::NotFound {
                thread_key: thread_key.as_str().to_owned(),
            },
            false => error.into(),
        })?;
        match row {
            Some(row) => row.try_into(),
            None => Err(SessionStoreError::PrBindingConflict {
                repository: pr.repository().to_owned(),
                number: pr.number(),
                reason: "the pull request is already bound".to_owned(),
            }),
        }
    }

    pub async fn pr_binding(&self, pr: &PrRef) -> Result<Option<PrBinding>, SessionStoreError> {
        sqlx::query_as::<_, PrBindingRow>(&format!(
            "select {BINDING_COLUMNS} from pr_session_bindings where repository = $1 and number = $2"
        ))
        .bind(pr.repository())
        .bind(pr.number())
        .fetch_optional(&self.pool)
        .await?
        .map(TryInto::try_into)
        .transpose()
    }

    /// Locks `pr`'s binding row until the returned guard is committed or
    /// dropped, so verification, state changes, enqueues and dispatch for one
    /// PR never interleave.
    pub async fn lock_pr_binding(&self, pr: &PrRef) -> Result<PrBindingLock, SessionStoreError> {
        let mut tx = self.pool.begin().await?;
        let binding = sqlx::query_as::<_, PrBindingRow>(&format!(
            "select {BINDING_COLUMNS} from pr_session_bindings where repository = $1 and number = $2 for update"
        ))
        .bind(pr.repository())
        .bind(pr.number())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| pr.not_found())?
        .try_into()?;
        Ok(PrBindingLock {
            tx,
            pr: pr.clone(),
            binding,
        })
    }

    /// Moves a `registered` binding to `verified` when the PR was created no
    /// earlier than its bound session, otherwise to `rejected`. Repeating the
    /// same verification is a no-op; a different `node_id` is a conflict.
    pub async fn verify_pr_binding(
        &self,
        pr: &PrRef,
        verification: &PrVerification,
    ) -> Result<PrBinding, SessionStoreError> {
        let mut lock = self.lock_pr_binding(pr).await?;
        let binding = lock.binding().clone();
        if binding.state != PrBindingState::Registered {
            if binding.pr_node_id.as_deref() == Some(verification.node_id.as_str()) {
                return Ok(binding);
            }
            return Err(SessionStoreError::PrNodeIdMismatch {
                repository: pr.repository().to_owned(),
                number: pr.number(),
            });
        }
        let session_created_at = lock.session_created_at().await?;
        let state = match session_created_at {
            Some(session_created_at) if verification.created_at >= session_created_at => {
                PrBindingState::Verified
            }
            _ => PrBindingState::Rejected,
        };
        let row = sqlx::query_as::<_, PrBindingRow>(&format!(
            r#"
            update pr_session_bindings
            set state = $3, pr_node_id = $4, pr_author_login = $5, pr_created_at = $6,
                head_ref = $7, head_sha = $8, updated_at = now()
            where repository = $1 and number = $2
            returning {BINDING_COLUMNS}
            "#
        ))
        .bind(pr.repository())
        .bind(pr.number())
        .bind(state.as_str())
        .bind(&verification.node_id)
        .bind(&verification.author_login)
        .bind(verification.created_at)
        .bind(&verification.head_ref)
        .bind(&verification.head_sha)
        .fetch_one(&mut *lock.tx)
        .await?;
        let binding = row.try_into()?;
        lock.commit().await?;
        Ok(binding)
    }

    /// Applies a partial state update. Unassigning (`managed=false`) or closing
    /// drops every queued follow-up and clears the push grant; a recorded grant
    /// is always stored inactive.
    pub async fn update_pr_binding_state(
        &self,
        pr: &PrRef,
        update: PrBindingStateUpdate,
    ) -> Result<PrBinding, SessionStoreError> {
        let push_grant = match update.push_grant {
            Some(Some(grant)) => Some(Some(inactive_push_grant(grant)?)),
            other => other,
        };
        let mut lock = self.lock_pr_binding(pr).await?;
        let binding = lock.binding().clone();
        let managed = update.managed.unwrap_or(binding.managed);
        let head_sha = update.head_sha.or(binding.head_sha);
        let (automation_paused, pause_reason) = match update.automation_paused {
            None => (binding.automation_paused, binding.pause_reason),
            Some(None) => (false, None),
            Some(Some(pause)) => (true, Some(pause.reason)),
        };
        let mut push_grant = push_grant.unwrap_or(binding.push_grant);
        let blocker = update.blocker.unwrap_or(binding.blocker);

        let drop_reason = if update.closed == Some(true) {
            Some(PrFollowupDropReason::Closed)
        } else if update.managed == Some(false) {
            Some(PrFollowupDropReason::Unassigned)
        } else {
            None
        };
        if let Some(reason) = drop_reason {
            lock.drop_queued(reason).await?;
            push_grant = None;
        }
        let row = sqlx::query_as::<_, PrBindingRow>(&format!(
            r#"
            update pr_session_bindings
            set managed = $3, head_sha = $4, automation_paused = $5, pause_reason = $6,
                push_grant = $7, blocker = $8, updated_at = now()
            where repository = $1 and number = $2
            returning {BINDING_COLUMNS}
            "#
        ))
        .bind(pr.repository())
        .bind(pr.number())
        .bind(managed)
        .bind(head_sha)
        .bind(automation_paused)
        .bind(pause_reason)
        .bind(push_grant)
        .bind(blocker)
        .fetch_one(&mut *lock.tx)
        .await?;
        let binding = row.try_into()?;
        lock.commit().await?;
        Ok(binding)
    }

    /// Queues a follow-up on a verified, managed binding. A repeat of the same
    /// logical item (`dedupe_key`) returns the existing row with
    /// `created = false`.
    pub async fn enqueue_pr_followup(
        &self,
        pr: &PrRef,
        followup: &NewPrFollowup,
    ) -> Result<(PrFollowup, bool), SessionStoreError> {
        followup.validate()?;
        let mut lock = self.lock_pr_binding(pr).await?;
        let binding = lock.binding();
        if binding.state != PrBindingState::Verified || !binding.managed {
            return Err(SessionStoreError::PrBindingInactive {
                repository: pr.repository().to_owned(),
                number: pr.number(),
            });
        }
        let inserted = sqlx::query_as::<_, PrFollowupRow>(&format!(
            r#"
            insert into pr_session_followups
                (id, binding_repository, binding_number, kind, dedupe_key, head_sha, actor, prompt, reply_target, status)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'queued')
            on conflict (binding_repository, binding_number, dedupe_key) do nothing
            returning {FOLLOWUP_COLUMNS}
            "#
        ))
        .bind(prefixed_id("prf"))
        .bind(pr.repository())
        .bind(pr.number())
        .bind(followup.kind.as_str())
        .bind(&followup.dedupe_key)
        .bind(&followup.head_sha)
        .bind(&followup.actor)
        .bind(&followup.prompt)
        .bind(followup.reply_target.clone().filter(|target| !target.is_null()))
        .fetch_optional(&mut *lock.tx)
        .await?;
        let (row, created) = match inserted {
            Some(row) => (row, true),
            None => (
                sqlx::query_as::<_, PrFollowupRow>(&format!(
                    r#"
                    select {FOLLOWUP_COLUMNS} from pr_session_followups
                    where binding_repository = $1 and binding_number = $2 and dedupe_key = $3
                    "#
                ))
                .bind(pr.repository())
                .bind(pr.number())
                .bind(&followup.dedupe_key)
                .fetch_one(&mut *lock.tx)
                .await?,
                false,
            ),
        };
        let followup = row.try_into()?;
        lock.commit().await?;
        Ok((followup, created))
    }

    /// The follow-ups queued on a binding, oldest first.
    pub async fn list_pr_followups(
        &self,
        pr: &PrRef,
    ) -> Result<Vec<PrFollowup>, SessionStoreError> {
        sqlx::query_as::<_, PrFollowupRow>(&format!(
            r#"
            select {FOLLOWUP_COLUMNS} from pr_session_followups
            where binding_repository = $1 and binding_number = $2
            order by created_at, id
            "#
        ))
        .bind(pr.repository())
        .bind(pr.number())
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
    }

    /// The bound session of `execution_id` when it is an execution `pr`'s own
    /// queue dispatched, or `None` for any other execution: one another
    /// binding dispatched, one never dispatched, or an unknown id.
    pub async fn pr_followup_execution_thread(
        &self,
        pr: &PrRef,
        execution_id: &str,
    ) -> Result<Option<ThreadKey>, SessionStoreError> {
        sqlx::query_scalar::<_, String>(
            r#"
            select e.thread_key
            from pr_session_followups f
            join pr_session_bindings b
              on b.repository = f.binding_repository and b.number = f.binding_number
            join session_executions e
              on e.execution_id = f.execution_id and e.thread_key = b.thread_key
            where f.binding_repository = $1
              and f.binding_number = $2
              and f.status = 'dispatched'
              and f.execution_id = $3
            limit 1
            "#,
        )
        .bind(pr.repository())
        .bind(pr.number())
        .bind(execution_id)
        .fetch_optional(&self.pool)
        .await?
        .map(parse_persisted)
        .transpose()
    }

    /// Finished executions that PR follow-up dispatch started on sessions
    /// whose thread key starts with one of `thread_prefixes`, ordered by
    /// completion and listed after `after`. An empty `thread_prefixes` lists
    /// nothing.
    ///
    /// Only executions finished at least `settle` ago are listed. A terminal
    /// update stamps `completed_at` before it commits, so without the margin
    /// an execution could become visible after a later-stamped one had
    /// already moved a caller's cursor past it.
    pub async fn list_terminal_pr_followup_executions(
        &self,
        thread_prefixes: &[String],
        after: Option<&PrFollowupExecutionCursor>,
        settle: std::time::Duration,
        limit: i64,
    ) -> Result<Vec<PrFollowupExecution>, SessionStoreError> {
        if thread_prefixes.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, PrFollowupExecutionRow>(
            r#"
            select e.execution_id, e.thread_key, e.status,
                   coalesce(e.metadata -> 'pr_followup', 'null'::jsonb) as pr_followup,
                   e.completed_at
            from pr_session_followups f
            join pr_session_bindings b
              on b.repository = f.binding_repository and b.number = f.binding_number
            join session_executions e
              on e.execution_id = f.execution_id and e.thread_key = b.thread_key
            where f.status = 'dispatched'
              and e.status in ('completed', 'failed', 'cancelled')
              and e.completed_at is not null
              and e.completed_at <= now() - make_interval(secs => $5)
              and exists (select 1 from unnest($1::text[]) p where starts_with(e.thread_key, p))
              and ($2::timestamptz is null or (e.completed_at, e.execution_id) > ($2, $3))
            order by e.completed_at, e.execution_id
            limit $4
            "#,
        )
        .bind(thread_prefixes)
        .bind(after.map(|cursor| cursor.completed_at))
        .bind(after.map(|cursor| cursor.execution_id.as_str()))
        .bind(limit)
        .bind(settle.as_secs_f64())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(PrFollowupExecution {
                    execution_id: row.execution_id,
                    thread_key: parse_persisted(row.thread_key)?,
                    status: parse_persisted(row.status)?,
                    pr_followup: row.pr_followup,
                    completed_at: row.completed_at,
                })
            })
            .collect()
    }
}

#[derive(Debug, FromRow)]
struct PrFollowupExecutionRow {
    execution_id: String,
    thread_key: String,
    status: String,
    pr_followup: Value,
    completed_at: OffsetDateTime,
}

/// A binding row held `for update` inside a transaction. Every write made
/// through it commits together; dropping it without [`Self::commit`] rolls
/// them back.
pub struct PrBindingLock {
    tx: Transaction<'static, Postgres>,
    pr: PrRef,
    binding: PrBinding,
}

impl PrBindingLock {
    pub fn binding(&self) -> &PrBinding {
        &self.binding
    }

    /// Drops every queued follow-up. Returns how many were dropped.
    pub async fn drop_queued(
        &mut self,
        reason: PrFollowupDropReason,
    ) -> Result<u64, SessionStoreError> {
        let result = sqlx::query(
            r#"
            update pr_session_followups
            set status = 'dropped', drop_reason = $3, updated_at = now()
            where binding_repository = $1 and binding_number = $2 and status = 'queued'
            "#,
        )
        .bind(self.pr.repository())
        .bind(self.pr.number())
        .bind(reason.as_str())
        .execute(&mut *self.tx)
        .await?;
        Ok(result.rows_affected())
    }

    /// Drops queued follow-ups raised on another head, except kinds that stay
    /// valid across heads. Returns how many were dropped.
    pub async fn drop_stale_head(
        &mut self,
        current_head_sha: &str,
    ) -> Result<u64, SessionStoreError> {
        let surviving = [
            PrFollowupKind::Review,
            PrFollowupKind::IssueComment,
            PrFollowupKind::Mention,
        ];
        debug_assert!(surviving.iter().all(|kind| kind.survives_head_change()));
        let surviving = surviving.map(PrFollowupKind::as_str);
        let result = sqlx::query(
            r#"
            update pr_session_followups
            set status = 'dropped', drop_reason = $4, updated_at = now()
            where binding_repository = $1 and binding_number = $2 and status = 'queued'
              and head_sha <> $3 and not (kind = any($5))
            "#,
        )
        .bind(self.pr.repository())
        .bind(self.pr.number())
        .bind(current_head_sha)
        .bind(PrFollowupDropReason::StaleHead.as_str())
        .bind(&surviving[..])
        .execute(&mut *self.tx)
        .await?;
        Ok(result.rows_affected())
    }

    /// Records the PR head githubbot just observed.
    pub async fn set_head_sha(&mut self, head_sha: &str) -> Result<(), SessionStoreError> {
        let row = sqlx::query_as::<_, PrBindingRow>(&format!(
            r#"
            update pr_session_bindings set head_sha = $3, updated_at = now()
            where repository = $1 and number = $2
            returning {BINDING_COLUMNS}
            "#
        ))
        .bind(self.pr.repository())
        .bind(self.pr.number())
        .bind(head_sha)
        .fetch_one(&mut *self.tx)
        .await?;
        self.binding = row.try_into()?;
        Ok(())
    }

    pub async fn set_blocker(&mut self, blocker: Option<&str>) -> Result<(), SessionStoreError> {
        let row = sqlx::query_as::<_, PrBindingRow>(&format!(
            r#"
            update pr_session_bindings set blocker = $3, updated_at = now()
            where repository = $1 and number = $2
            returning {BINDING_COLUMNS}
            "#
        ))
        .bind(self.pr.repository())
        .bind(self.pr.number())
        .bind(blocker)
        .fetch_one(&mut *self.tx)
        .await?;
        self.binding = row.try_into()?;
        Ok(())
    }

    /// Ends queued work after githubbot saw the bot unassigned or the PR
    /// closed: clears the push grant, and the managed flag when unassigned.
    pub async fn end_management(
        &mut self,
        reason: PrFollowupDropReason,
    ) -> Result<u64, SessionStoreError> {
        let dropped = self.drop_queued(reason).await?;
        let row = sqlx::query_as::<_, PrBindingRow>(&format!(
            r#"
            update pr_session_bindings
            set managed = managed and $3, push_grant = null, updated_at = now()
            where repository = $1 and number = $2
            returning {BINDING_COLUMNS}
            "#
        ))
        .bind(self.pr.repository())
        .bind(self.pr.number())
        .bind(reason != PrFollowupDropReason::Unassigned)
        .fetch_one(&mut *self.tx)
        .await?;
        self.binding = row.try_into()?;
        Ok(dropped)
    }

    /// Whether the bound session has a queued or running execution.
    pub async fn has_active_execution(&mut self) -> Result<bool, SessionStoreError> {
        Ok(sqlx::query_scalar::<_, bool>(
            r#"
            select exists (
                select 1 from session_executions
                where thread_key = $1 and status in ('queued', 'running')
            )
            "#,
        )
        .bind(self.binding.thread_key.as_str())
        .fetch_one(&mut *self.tx)
        .await?)
    }

    /// The bound session's status, or `None` when the session is gone.
    pub async fn session_status(&mut self) -> Result<Option<SessionStatus>, SessionStoreError> {
        sqlx::query_scalar::<_, String>("select status from sessions where thread_key = $1")
            .bind(self.binding.thread_key.as_str())
            .fetch_optional(&mut *self.tx)
            .await?
            .map(parse_persisted)
            .transpose()
    }

    async fn session_created_at(&mut self) -> Result<Option<OffsetDateTime>, SessionStoreError> {
        Ok(sqlx::query_scalar::<_, OffsetDateTime>(
            "select created_at from sessions where thread_key = $1",
        )
        .bind(self.binding.thread_key.as_str())
        .fetch_optional(&mut *self.tx)
        .await?)
    }

    /// The oldest queued follow-up, if any.
    pub async fn oldest_queued(&mut self) -> Result<Option<PrFollowup>, SessionStoreError> {
        sqlx::query_as::<_, PrFollowupRow>(&format!(
            r#"
            select {FOLLOWUP_COLUMNS} from pr_session_followups
            where binding_repository = $1 and binding_number = $2 and status = 'queued'
            order by created_at, id
            limit 1
            "#
        ))
        .bind(self.pr.repository())
        .bind(self.pr.number())
        .fetch_optional(&mut *self.tx)
        .await?
        .map(TryInto::try_into)
        .transpose()
    }

    pub async fn mark_dispatched(
        &mut self,
        followup_id: &str,
        execution_id: &str,
    ) -> Result<PrFollowup, SessionStoreError> {
        sqlx::query_as::<_, PrFollowupRow>(&format!(
            r#"
            update pr_session_followups
            set status = 'dispatched', execution_id = $4, updated_at = now()
            where binding_repository = $1 and binding_number = $2 and id = $3
            returning {FOLLOWUP_COLUMNS}
            "#
        ))
        .bind(self.pr.repository())
        .bind(self.pr.number())
        .bind(followup_id)
        .bind(execution_id)
        .fetch_one(&mut *self.tx)
        .await?
        .try_into()
    }

    pub async fn commit(self) -> Result<(), SessionStoreError> {
        self.tx.commit().await?;
        Ok(())
    }
}

fn foreign_key_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|error| error.code().as_deref() == Some("23503"))
}

#[derive(Debug, FromRow)]
struct PrBindingRow {
    repository: String,
    number: i64,
    thread_key: String,
    state: String,
    pr_node_id: Option<String>,
    pr_author_login: Option<String>,
    pr_created_at: Option<OffsetDateTime>,
    head_ref: Option<String>,
    managed: bool,
    head_sha: Option<String>,
    automation_paused: bool,
    pause_reason: Option<String>,
    push_grant: Option<Value>,
    blocker: Option<String>,
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
            state: parse_persisted(row.state)?,
            pr_node_id: row.pr_node_id,
            pr_author_login: row.pr_author_login,
            pr_created_at: row.pr_created_at,
            head_ref: row.head_ref,
            managed: row.managed,
            head_sha: row.head_sha,
            automation_paused: row.automation_paused,
            pause_reason: row.pause_reason,
            push_grant: row.push_grant,
            blocker: row.blocker,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct PrFollowupRow {
    id: String,
    binding_repository: String,
    binding_number: i64,
    kind: String,
    dedupe_key: String,
    head_sha: String,
    actor: Value,
    prompt: Value,
    reply_target: Option<Value>,
    status: String,
    drop_reason: Option<String>,
    execution_id: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<PrFollowupRow> for PrFollowup {
    type Error = SessionStoreError;

    fn try_from(row: PrFollowupRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            repository: row.binding_repository,
            number: row.binding_number,
            kind: parse_persisted(row.kind)?,
            dedupe_key: row.dedupe_key,
            head_sha: row.head_sha,
            actor: row.actor,
            prompt: row.prompt,
            reply_target: row.reply_target,
            status: parse_persisted(row.status)?,
            drop_reason: row.drop_reason,
            execution_id: row.execution_id,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn followup(kind: PrFollowupKind, dedupe_key: &str) -> NewPrFollowup {
        NewPrFollowup {
            kind,
            dedupe_key: dedupe_key.to_owned(),
            head_sha: "abc123".to_owned(),
            actor: json!({"github_user_id": "90210001", "github_login": "ada"}),
            prompt: json!([{"type": "text", "text": "Please address the review."}]),
            reply_target: Some(json!({
                "type": "github_comment", "repository": "acme/widgets", "number": 7, "review_id": 11
            })),
        }
    }

    #[test]
    fn execution_cursors_round_trip_at_microsecond_precision() {
        let cursor = PrFollowupExecutionCursor {
            completed_at: OffsetDateTime::from_unix_timestamp_nanos(1_779_000_000_123_456_000)
                .unwrap(),
            execution_id: "exe_abc".to_owned(),
        };
        assert_eq!(cursor.encode(), "1779000000123456:exe_abc");
        assert_eq!(
            PrFollowupExecutionCursor::parse(&cursor.encode()),
            Some(cursor)
        );
        for invalid in ["", "exe_abc", "12:", "x:exe_abc", ":exe_abc"] {
            assert_eq!(PrFollowupExecutionCursor::parse(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn pr_refs_are_lowercased_owner_repo_with_positive_numbers() {
        let pr = PrRef::new(" Acme/Widgets.rs ", 7).unwrap();
        assert_eq!(pr.repository(), "acme/widgets.rs");
        assert_eq!(pr.number(), 7);
        for (repository, number) in [
            ("acme", 7),
            ("acme/widgets/extra", 7),
            ("/widgets", 7),
            ("acme/", 7),
            ("acme/wid gets", 7),
            ("acme/widgets", 0),
            ("acme/widgets", -3),
        ] {
            assert!(
                PrRef::new(repository, number).is_err(),
                "{repository} {number}"
            );
        }
    }

    #[test]
    fn followups_validate_their_dedupe_key_actor_prompt_and_reply_target() {
        for (kind, key) in [
            (PrFollowupKind::Review, "review:11"),
            (PrFollowupKind::ReviewComment, "comment:12"),
            (PrFollowupKind::IssueComment, "comment:13"),
            (PrFollowupKind::Mention, "comment:14"),
            (PrFollowupKind::CiFailure, "ci:abc123:failure"),
            (PrFollowupKind::Conflict, "conflict:abc123"),
        ] {
            followup(kind, key).validate().unwrap();
        }
        assert!(
            followup(PrFollowupKind::Review, "comment:11")
                .validate()
                .is_err()
        );
        assert!(
            followup(PrFollowupKind::Review, "review:")
                .validate()
                .is_err()
        );

        let mut invalid = followup(PrFollowupKind::Review, "review:11");
        invalid.actor = json!({"github_user_id": "ada", "github_login": "ada"});
        assert!(invalid.validate().is_err());

        let mut invalid = followup(PrFollowupKind::Review, "review:11");
        invalid.prompt = json!([{"type": "attachment", "url": "https://example.com"}]);
        assert!(invalid.validate().is_err());

        let mut invalid = followup(PrFollowupKind::Review, "review:11");
        invalid.prompt = json!([]);
        assert!(invalid.validate().is_err());

        let mut invalid = followup(PrFollowupKind::Review, "review:11");
        invalid.reply_target = Some(json!({"type": "slack_message"}));
        assert!(invalid.validate().is_err());

        let mut ci = followup(PrFollowupKind::CiFailure, "ci:abc123:failure");
        ci.reply_target = None;
        ci.validate().unwrap();
    }

    #[test]
    fn only_reviews_and_direct_requests_survive_a_head_change() {
        assert!(PrFollowupKind::Review.survives_head_change());
        assert!(PrFollowupKind::IssueComment.survives_head_change());
        assert!(PrFollowupKind::Mention.survives_head_change());
        assert!(!PrFollowupKind::ReviewComment.survives_head_change());
        assert!(!PrFollowupKind::CiFailure.survives_head_change());
        assert!(!PrFollowupKind::Conflict.survives_head_change());
    }

    #[test]
    fn recorded_push_grants_are_always_inactive() {
        assert_eq!(
            inactive_push_grant(json!({
                "actor": {"platform": "slack", "id": "U1", "name": "Ada"},
                "branch": "fix/widgets",
                "recorded_at": "2026-11-23T00:00:00Z",
                "active": true
            }))
            .unwrap()["active"],
            json!(false)
        );
        assert_eq!(
            inactive_push_grant(json!({"branch": "fix/widgets"})).unwrap()["active"],
            json!(false)
        );
        assert!(inactive_push_grant(json!("grant")).is_err());
    }

    #[test]
    fn state_updates_distinguish_absent_from_null_fields() {
        let update: PrBindingStateUpdate =
            serde_json::from_value(json!({"managed": true})).unwrap();
        assert!(update.automation_paused.is_none());
        assert!(update.push_grant.is_none());
        assert!(update.blocker.is_none());

        let update: PrBindingStateUpdate = serde_json::from_value(json!({
            "automation_paused": null, "push_grant": null, "blocker": null
        }))
        .unwrap();
        assert!(matches!(update.automation_paused, Some(None)));
        assert!(matches!(update.push_grant, Some(None)));
        assert!(matches!(update.blocker, Some(None)));

        let update: PrBindingStateUpdate = serde_json::from_value(json!({
            "automation_paused": {"reason": "outside push"}, "blocker": "session archived"
        }))
        .unwrap();
        assert_eq!(
            update
                .automation_paused
                .and_then(|pause| pause)
                .map(|pause| pause.reason)
                .as_deref(),
            Some("outside push")
        );
        assert_eq!(
            update.blocker.flatten().as_deref(),
            Some("session archived")
        );
    }

    #[test]
    fn dispatch_metadata_carries_the_followup_and_its_github_actor() {
        let now = OffsetDateTime::now_utc();
        let followup = PrFollowup {
            id: "prf_1".to_owned(),
            repository: "acme/widgets".to_owned(),
            number: 7,
            kind: PrFollowupKind::Review,
            dedupe_key: "review:11".to_owned(),
            head_sha: "abc123".to_owned(),
            actor: json!({"github_user_id": "90210001", "github_login": "ada"}),
            prompt: json!([{"type": "text", "text": "hi"}]),
            reply_target: None,
            status: PrFollowupStatus::Queued,
            drop_reason: None,
            execution_id: None,
            created_at: now,
            updated_at: now,
        };
        let metadata = pr_followup_execute_metadata(&followup);
        assert_eq!(metadata["pr_followup"]["followup_id"], "prf_1");
        assert_eq!(metadata["pr_followup"]["repository"], "acme/widgets");
        assert_eq!(metadata["pr_followup"]["number"], 7);
        assert_eq!(metadata["pr_followup"]["kind"], "review");
        assert_eq!(metadata["pr_followup_github_user_id"], "90210001");
        assert_eq!(metadata["pr_followup_github_login"], "ada");
        assert_eq!(metadata["push_authorization"], "prepare_and_ask");
    }

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

    async fn session(store: &PgSessionStore, label: &str) -> ThreadKey {
        let thread_key = ThreadKey::parse(format!(
            "slack:T123:C123:{label}-{}",
            uuid::Uuid::new_v4().simple()
        ))
        .unwrap();
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

    fn unique_pr(number: i64) -> PrRef {
        PrRef::new(
            &format!("Acme/Widgets-{}", uuid::Uuid::new_v4().simple()),
            number,
        )
        .unwrap()
    }

    fn verification(node_id: &str, created_at: OffsetDateTime) -> PrVerification {
        PrVerification {
            node_id: node_id.to_owned(),
            author_login: "centaur-bot".to_owned(),
            created_at,
            head_ref: "fix/widgets".to_owned(),
            head_sha: "abc123".to_owned(),
        }
    }

    async fn managed_binding(store: &PgSessionStore, thread_key: &ThreadKey) -> PrRef {
        let pr = unique_pr(7);
        store.register_pr_binding(thread_key, &pr).await.unwrap();
        store
            .verify_pr_binding(
                &pr,
                &verification(
                    "PR_node",
                    OffsetDateTime::now_utc() + time::Duration::seconds(1),
                ),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registration_is_idempotent_for_its_session_and_never_rebinds() {
        let Some(store) = test_store().await else {
            return;
        };
        let owner = session(&store, "owner").await;
        let other = session(&store, "other").await;
        let pr = unique_pr(7);

        let binding = store.register_pr_binding(&owner, &pr).await.unwrap();
        assert_eq!(binding.state, PrBindingState::Registered);
        assert_eq!(binding.repository, pr.repository());
        assert_eq!(binding.thread_key, owner);
        assert!(!binding.managed);
        store.register_pr_binding(&owner, &pr).await.unwrap();

        assert!(matches!(
            store.register_pr_binding(&other, &pr).await,
            Err(SessionStoreError::PrBindingConflict { .. })
        ));
        store
            .verify_pr_binding(
                &pr,
                &verification(
                    "PR_node",
                    OffsetDateTime::now_utc() + time::Duration::seconds(1),
                ),
            )
            .await
            .unwrap();
        // A verified binding is final, even for the session that registered it.
        assert!(matches!(
            store.register_pr_binding(&owner, &pr).await,
            Err(SessionStoreError::PrBindingConflict { .. })
        ));
        assert_eq!(
            store.pr_binding(&pr).await.unwrap().unwrap().thread_key,
            owner
        );

        let missing = ThreadKey::parse(format!("slack:T1:C1:{}", uuid::Uuid::new_v4())).unwrap();
        assert!(matches!(
            store.register_pr_binding(&missing, &unique_pr(1)).await,
            Err(SessionStoreError::NotFound { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verification_rejects_prs_older_than_the_session_and_pins_the_node_id() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = session(&store, "verify").await;
        let session_created_at = store.get_session(&thread_key).await.unwrap().created_at;

        let older = unique_pr(3);
        store
            .register_pr_binding(&thread_key, &older)
            .await
            .unwrap();
        let rejected = store
            .verify_pr_binding(
                &older,
                &verification("PR_old", session_created_at - time::Duration::days(1)),
            )
            .await
            .unwrap();
        assert_eq!(rejected.state, PrBindingState::Rejected);
        assert!(matches!(
            store
                .update_pr_binding_state(
                    &older,
                    serde_json::from_value(json!({"managed": true})).unwrap()
                )
                .await
                .and(Ok(()))
                .and(
                    store
                        .enqueue_pr_followup(&older, &followup(PrFollowupKind::Review, "review:1"))
                        .await
                ),
            Err(SessionStoreError::PrBindingInactive { .. })
        ));

        let newer = unique_pr(4);
        store
            .register_pr_binding(&thread_key, &newer)
            .await
            .unwrap();
        let facts = verification("PR_new", session_created_at + time::Duration::seconds(5));
        let verified = store.verify_pr_binding(&newer, &facts).await.unwrap();
        assert_eq!(verified.state, PrBindingState::Verified);
        assert_eq!(verified.pr_node_id.as_deref(), Some("PR_new"));
        assert_eq!(verified.pr_author_login.as_deref(), Some("centaur-bot"));
        assert_eq!(verified.head_ref.as_deref(), Some("fix/widgets"));
        assert_eq!(verified.head_sha.as_deref(), Some("abc123"));
        // Idempotent for the same PR, a conflict for a different one.
        assert_eq!(
            store.verify_pr_binding(&newer, &facts).await.unwrap().state,
            PrBindingState::Verified
        );
        assert!(matches!(
            store
                .verify_pr_binding(
                    &newer,
                    &verification("PR_other", session_created_at + time::Duration::seconds(5))
                )
                .await,
            Err(SessionStoreError::PrNodeIdMismatch { .. })
        ));
        assert!(matches!(
            store.verify_pr_binding(&unique_pr(5), &facts).await,
            Err(SessionStoreError::PrBindingNotFound { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn followups_dedupe_repeat_deliveries_and_keep_distinct_reviews() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = session(&store, "dedupe").await;
        let pr = unique_pr(7);
        store.register_pr_binding(&thread_key, &pr).await.unwrap();
        store
            .verify_pr_binding(
                &pr,
                &verification(
                    "PR_node",
                    OffsetDateTime::now_utc() + time::Duration::seconds(1),
                ),
            )
            .await
            .unwrap();
        // Verified but not yet assigned: nothing queues.
        assert!(matches!(
            store
                .enqueue_pr_followup(&pr, &followup(PrFollowupKind::Review, "review:11"))
                .await,
            Err(SessionStoreError::PrBindingInactive { .. })
        ));
        store
            .update_pr_binding_state(
                &pr,
                serde_json::from_value(json!({"managed": true})).unwrap(),
            )
            .await
            .unwrap();

        let (first, created) = store
            .enqueue_pr_followup(&pr, &followup(PrFollowupKind::Review, "review:11"))
            .await
            .unwrap();
        assert!(created);
        assert_eq!(first.status, PrFollowupStatus::Queued);
        assert!(first.id.starts_with("prf_"));
        let (repeat, created) = store
            .enqueue_pr_followup(&pr, &followup(PrFollowupKind::Review, "review:11"))
            .await
            .unwrap();
        assert!(!created, "a repeat delivery is a no-op");
        assert_eq!(repeat.id, first.id);
        let (second, created) = store
            .enqueue_pr_followup(&pr, &followup(PrFollowupKind::Review, "review:12"))
            .await
            .unwrap();
        assert!(created, "a distinct review queues separately");
        assert_ne!(second.id, first.id);

        let mut ci = followup(PrFollowupKind::CiFailure, "ci:abc123:failure");
        ci.reply_target = None;
        assert!(store.enqueue_pr_followup(&pr, &ci).await.unwrap().1);
        assert!(
            !store.enqueue_pr_followup(&pr, &ci).await.unwrap().1,
            "redundant CI work for the same head collapses"
        );

        let queued = store.list_pr_followups(&pr).await.unwrap();
        assert_eq!(
            queued
                .iter()
                .map(|item| item.dedupe_key.as_str())
                .collect::<Vec<_>>(),
            ["review:11", "review:12", "ci:abc123:failure"]
        );
        assert_eq!(queued[2].reply_target, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unassignment_and_closure_drop_the_queue_and_end_the_grant() {
        let Some(store) = test_store().await else {
            return;
        };
        for (update, reason) in [
            (json!({"managed": false}), "unassigned"),
            (json!({"closed": true}), "closed"),
        ] {
            let thread_key = session(&store, "drop").await;
            let pr = managed_binding(&store, &thread_key).await;
            let binding = store
                .update_pr_binding_state(
                    &pr,
                    serde_json::from_value(json!({
                        "push_grant": {
                            "actor": {"platform": "slack", "id": "U1", "name": "Ada"},
                            "branch": "fix/widgets",
                            "recorded_at": "2026-11-23T00:00:00Z",
                            "active": true
                        }
                    }))
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(binding.push_grant.unwrap()["active"], json!(false));
            store
                .enqueue_pr_followup(&pr, &followup(PrFollowupKind::Review, "review:11"))
                .await
                .unwrap();

            let binding = store
                .update_pr_binding_state(&pr, serde_json::from_value(update).unwrap())
                .await
                .unwrap();
            assert_eq!(binding.push_grant, None, "{reason}");
            let items = store.list_pr_followups(&pr).await.unwrap();
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].status, PrFollowupStatus::Dropped);
            assert_eq!(items[0].drop_reason.as_deref(), Some(reason));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_and_blocker_round_trip_through_state_updates() {
        let Some(store) = test_store().await else {
            return;
        };
        let thread_key = session(&store, "pause").await;
        let pr = managed_binding(&store, &thread_key).await;
        let binding = store
            .update_pr_binding_state(
                &pr,
                serde_json::from_value(json!({
                    "automation_paused": {"reason": "outside push"},
                    "blocker": "session archived",
                    "head_sha": "def456"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(binding.automation_paused);
        assert_eq!(binding.pause_reason.as_deref(), Some("outside push"));
        assert_eq!(binding.blocker.as_deref(), Some("session archived"));
        assert_eq!(binding.head_sha.as_deref(), Some("def456"));
        assert!(binding.managed, "absent fields are unchanged");

        let binding = store
            .update_pr_binding_state(
                &pr,
                serde_json::from_value(json!({"automation_paused": null, "blocker": null}))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(!binding.automation_paused);
        assert_eq!(binding.pause_reason, None);
        assert_eq!(binding.blocker, None);
    }
}
