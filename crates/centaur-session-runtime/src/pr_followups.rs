//! Routes GitHub follow-up work for a bound PR back into the exact session
//! that opened it. Dispatch reuses the ordinary execute path: the follow-up
//! becomes a queued execution on the bound thread key, idempotent on the
//! follow-up id, carrying server-set metadata that names its GitHub actor.

use centaur_session_core::{
    MessageRole, SessionExecution, SessionMessageInput, SessionStatus, ThreadKey,
};
use centaur_session_sqlx::{
    NewPrFollowup, PrBinding, PrBindingState, PrBindingStateUpdate, PrFollowup,
    PrFollowupDropReason, PrFollowupExecution, PrFollowupExecutionCursor, PrRef, PrVerification,
    SessionStoreError, pr_followup_execute_metadata,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use crate::{COMPONENT_SESSION_RUNTIME, ExecuteSessionInput, SessionRuntime, SessionRuntimeError};

/// What githubbot rechecked from the GitHub API immediately before asking
/// for a dispatch.
#[derive(Clone, Debug, Deserialize)]
pub struct PrDispatchRequest {
    pub current_head_sha: String,
    pub assigned: bool,
    pub open: bool,
}

/// Why a dispatch started nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrDispatchSkip {
    /// Centaur is no longer assigned or managing the PR; the queue was dropped.
    Unassigned,
    /// The PR is closed; the queue was dropped.
    Closed,
    /// The binding was never verified; nothing can be queued on it.
    NotVerified,
    /// An outside push paused automation until reconfirmed.
    Paused,
    /// The bound session already has a queued or running turn.
    Busy,
    /// The bound session is gone or archived; the binding, its queue and a
    /// blocker are kept.
    SessionUnavailable,
    /// Nothing is queued.
    Empty,
}

impl PrDispatchSkip {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unassigned => "unassigned",
            Self::Closed => "closed",
            Self::NotVerified => "not_verified",
            Self::Paused => "paused",
            Self::Busy => "busy",
            Self::SessionUnavailable => "session_unavailable",
            Self::Empty => "empty",
        }
    }
}

/// A follow-up started as a turn on its bound session.
#[derive(Clone, Debug)]
pub struct PrDispatched {
    pub followup: PrFollowup,
    pub execution: SessionExecution,
}

#[derive(Clone, Debug)]
pub enum PrDispatchOutcome {
    Dispatched(Box<PrDispatched>),
    Skipped(PrDispatchSkip),
}

/// How long a finished PR follow-up execution waits before it is listed, so a
/// terminal update that commits slightly out of completion order is never
/// skipped by a cursor that already moved past it.
pub const PR_FOLLOWUP_EXECUTION_LIST_SETTLE: std::time::Duration =
    std::time::Duration::from_secs(5);

/// A binding as githubbot reads it before acting on an event.
#[derive(Clone, Debug)]
pub struct PrBindingStatus {
    pub binding: PrBinding,
    pub session_exists: bool,
    pub active_execution: bool,
}

impl SessionRuntime {
    /// Records `pr` as opened by the session `thread_key`. Authorization (the
    /// caller owns the session) is the API's job.
    pub async fn register_pr_binding(
        &self,
        thread_key: &ThreadKey,
        pr: &PrRef,
    ) -> Result<PrBinding, SessionRuntimeError> {
        self.store.get_session(thread_key).await?;
        let binding = self.store.register_pr_binding(thread_key, pr).await?;
        info!(
            component = COMPONENT_SESSION_RUNTIME,
            event = "pr_binding_registered",
            thread_key = %thread_key,
            repository = pr.repository(),
            number = pr.number(),
            "registered PR session binding"
        );
        Ok(binding)
    }

    pub async fn pr_binding_status(
        &self,
        pr: &PrRef,
    ) -> Result<PrBindingStatus, SessionRuntimeError> {
        let binding = self.store.pr_binding(pr).await?.ok_or_else(|| {
            SessionStoreError::PrBindingNotFound {
                repository: pr.repository().to_owned(),
                number: pr.number(),
            }
        })?;
        let session_exists = match self.store.get_session(&binding.thread_key).await {
            Ok(_) => true,
            Err(SessionStoreError::NotFound { .. }) => false,
            Err(error) => return Err(error.into()),
        };
        let active_execution = self
            .store
            .active_execution_for_thread(&binding.thread_key)
            .await?
            .is_some();
        Ok(PrBindingStatus {
            binding,
            session_exists,
            active_execution,
        })
    }

    pub async fn verify_pr_binding(
        &self,
        pr: &PrRef,
        verification: &PrVerification,
    ) -> Result<PrBinding, SessionRuntimeError> {
        let binding = self.store.verify_pr_binding(pr, verification).await?;
        info!(
            component = COMPONENT_SESSION_RUNTIME,
            event = "pr_binding_verified",
            thread_key = %binding.thread_key,
            repository = pr.repository(),
            number = pr.number(),
            state = binding.state.as_str(),
            "verified PR session binding"
        );
        Ok(binding)
    }

    pub async fn update_pr_binding_state(
        &self,
        pr: &PrRef,
        update: PrBindingStateUpdate,
    ) -> Result<PrBinding, SessionRuntimeError> {
        Ok(self.store.update_pr_binding_state(pr, update).await?)
    }

    /// The bound session of `execution_id` when `pr`'s own queue dispatched
    /// it; `None` for any other execution.
    pub async fn pr_followup_execution_thread(
        &self,
        pr: &PrRef,
        execution_id: &str,
    ) -> Result<Option<ThreadKey>, SessionRuntimeError> {
        Ok(self
            .store
            .pr_followup_execution_thread(pr, execution_id)
            .await?)
    }

    /// Finished PR follow-up executions on sessions under `thread_prefixes`,
    /// after `after`, oldest completion first. Scoping `thread_prefixes` to
    /// the caller is the API's job.
    pub async fn list_terminal_pr_followup_executions(
        &self,
        thread_prefixes: &[String],
        after: Option<&PrFollowupExecutionCursor>,
        limit: i64,
    ) -> Result<Vec<PrFollowupExecution>, SessionRuntimeError> {
        Ok(self
            .store
            .list_terminal_pr_followup_executions(
                thread_prefixes,
                after,
                PR_FOLLOWUP_EXECUTION_LIST_SETTLE,
                limit,
            )
            .await?)
    }

    pub async fn enqueue_pr_followup(
        &self,
        pr: &PrRef,
        followup: &NewPrFollowup,
    ) -> Result<(PrFollowup, bool), SessionRuntimeError> {
        Ok(self.store.enqueue_pr_followup(pr, followup).await?)
    }

    /// Starts the oldest queued follow-up as a turn on the bound session, when
    /// the PR is still assigned, open and unpaused and the session is idle.
    ///
    /// The binding row stays locked for the whole decision, so concurrent
    /// dispatches, enqueues and state changes for the PR serialize. The
    /// execution is created before the prompt is recorded as a session
    /// message: the one-active-execution index makes creation the atomic
    /// busy check, and appending first could steer the prompt into a turn
    /// another participant started meanwhile, under their identity. Retries
    /// are idempotent: the execution keys on the follow-up id and the message
    /// on it as its client message id.
    pub async fn dispatch_pr_followup(
        &self,
        pr: &PrRef,
        request: &PrDispatchRequest,
    ) -> Result<PrDispatchOutcome, SessionRuntimeError> {
        let mut lock = self.store.lock_pr_binding(pr).await?;
        let binding = lock.binding().clone();

        let inactive = if !request.open {
            Some((PrDispatchSkip::Closed, PrFollowupDropReason::Closed))
        } else if !request.assigned || !binding.managed {
            Some((PrDispatchSkip::Unassigned, PrFollowupDropReason::Unassigned))
        } else if binding.state != PrBindingState::Verified {
            Some((
                PrDispatchSkip::NotVerified,
                PrFollowupDropReason::Unassigned,
            ))
        } else {
            None
        };
        if let Some((skip, reason)) = inactive {
            let dropped = lock.end_management(reason).await?;
            lock.commit().await?;
            info!(
                component = COMPONENT_SESSION_RUNTIME,
                event = "pr_followup_queue_dropped",
                thread_key = %binding.thread_key,
                repository = pr.repository(),
                number = pr.number(),
                reason = reason.as_str(),
                dropped,
                "dropped queued PR follow-ups"
            );
            return Ok(PrDispatchOutcome::Skipped(skip));
        }
        if binding.automation_paused {
            return Ok(PrDispatchOutcome::Skipped(PrDispatchSkip::Paused));
        }

        if binding.head_sha.as_deref() != Some(request.current_head_sha.as_str()) {
            lock.set_head_sha(&request.current_head_sha).await?;
        }
        lock.drop_stale_head(&request.current_head_sha).await?;
        if lock.has_active_execution().await? {
            lock.commit().await?;
            return Ok(PrDispatchOutcome::Skipped(PrDispatchSkip::Busy));
        }
        let unavailable = match lock.session_status().await? {
            None => Some("missing"),
            Some(SessionStatus::Archived) => Some("archived"),
            Some(_) => None,
        };
        if let Some(state) = unavailable {
            return self.record_unavailable_session(lock, &binding, state).await;
        }
        let Some(followup) = lock.oldest_queued().await? else {
            lock.commit().await?;
            return Ok(PrDispatchOutcome::Skipped(PrDispatchSkip::Empty));
        };

        let input = ExecuteSessionInput {
            idempotency_key: Some(followup.id.clone()),
            metadata: Some(pr_followup_execute_metadata(&followup)),
            input_lines: vec![pr_followup_input_line(&binding.thread_key, &followup)?],
            idle_timeout_ms: None,
            max_duration_ms: None,
        };
        let execution = match self
            .enqueue_session_execution(&binding.thread_key, input)
            .await
        {
            Ok(execution) => execution,
            Err(SessionRuntimeError::Store(SessionStoreError::NotFound { .. })) => {
                return self
                    .record_unavailable_session(lock, &binding, "missing")
                    .await;
            }
            Err(error) if another_execution_is_active(&error) => {
                lock.commit().await?;
                return Ok(PrDispatchOutcome::Skipped(PrDispatchSkip::Busy));
            }
            Err(error) => return Err(error),
        };
        self.store
            .append_messages(
                &binding.thread_key,
                &[pr_followup_message(&followup, &execution.execution_id)?],
            )
            .await?;
        let followup = lock
            .mark_dispatched(&followup.id, &execution.execution_id)
            .await?;
        if binding.blocker.is_some() {
            lock.set_blocker(None).await?;
        }
        lock.commit().await?;
        info!(
            component = COMPONENT_SESSION_RUNTIME,
            event = "pr_followup_dispatched",
            thread_key = %binding.thread_key,
            repository = pr.repository(),
            number = pr.number(),
            followup_id = %followup.id,
            kind = followup.kind.as_str(),
            execution_id = %execution.execution_id,
            "dispatched PR follow-up into its bound session"
        );
        Ok(PrDispatchOutcome::Dispatched(Box::new(PrDispatched {
            followup,
            execution,
        })))
    }

    async fn record_unavailable_session(
        &self,
        mut lock: centaur_session_sqlx::PrBindingLock,
        binding: &PrBinding,
        state: &str,
    ) -> Result<PrDispatchOutcome, SessionRuntimeError> {
        let blocker = format!("bound session {} is {state}", binding.thread_key);
        lock.set_blocker(Some(&blocker)).await?;
        lock.commit().await?;
        info!(
            component = COMPONENT_SESSION_RUNTIME,
            event = "pr_followup_session_unavailable",
            thread_key = %binding.thread_key,
            repository = %binding.repository,
            number = binding.number,
            blocker = %blocker,
            "bound session is unavailable; keeping the binding and its queue"
        );
        Ok(PrDispatchOutcome::Skipped(
            PrDispatchSkip::SessionUnavailable,
        ))
    }
}

/// A concurrent turn won the session's one-active-execution slot.
fn another_execution_is_active(error: &SessionRuntimeError) -> bool {
    let SessionRuntimeError::Store(SessionStoreError::Sqlx(error)) = error else {
        return false;
    };
    error
        .as_database_error()
        .is_some_and(|error| error.constraint() == Some("session_executions_one_active_idx"))
}

/// The harness input line for a dispatched follow-up: its text prompt as one
/// user turn on the bound thread.
pub fn pr_followup_input_line(
    thread_key: &ThreadKey,
    followup: &PrFollowup,
) -> Result<String, SessionRuntimeError> {
    serde_json::to_string(&json!({
        "type": "user",
        "thread_key": thread_key.as_str(),
        "trace_metadata": {
            "source": "api-rs",
            "action": "pr_followup_dispatch",
            "followup_id": followup.id,
            "kind": followup.kind.as_str(),
        },
        "message": {
            "role": "user",
            "content": followup.prompt,
        },
    }))
    .map_err(|error| SessionRuntimeError::BadRequest(format!("invalid follow-up prompt: {error}")))
}

fn pr_followup_message(
    followup: &PrFollowup,
    execution_id: &str,
) -> Result<SessionMessageInput, SessionRuntimeError> {
    let Value::Array(parts) = followup.prompt.clone() else {
        return Err(SessionRuntimeError::BadRequest(
            "follow-up prompt must be an array of parts".to_owned(),
        ));
    };
    Ok(SessionMessageInput {
        client_message_id: Some(followup.id.clone()),
        role: MessageRole::User,
        parts,
        metadata: json!({
            "source": "api-rs",
            "action": "pr_followup_dispatch",
            "followup_id": followup.id,
            "execution_id": execution_id,
            "kind": followup.kind.as_str(),
            "repository": followup.repository,
            "number": followup.number,
            "github_user_id": followup.actor_github_user_id(),
            "github_login": followup.actor_github_login(),
        }),
    })
}
