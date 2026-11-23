//! PR session bindings: route a pull request's GitHub follow-ups back into
//! the session that opened it.
//!
//! Registration is the sandbox's own, principal-owned call on its session.
//! Every `/api/pr-bindings/*` route is githubbot's alone
//! (`Capability::PrBindingsManage`); they live outside `/api/session/*`, so
//! each handler resolves the bound session from the binding instead of from a
//! caller-chosen thread key. Listing finished follow-up executions is the
//! slackbot ingress's (`Capability::PrFollowupsRead`), scoped to its own
//! platform's sessions.

use std::convert::Infallible;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    response::{Sse, sse::Event},
};
use centaur_session_core::ThreadKey;
use centaur_session_runtime::{PrDispatchOutcome, PrDispatchRequest, SessionRuntimeError};
use centaur_session_sqlx::{
    NewPrFollowup, PrBinding, PrBindingStateUpdate, PrFollowup, PrFollowupExecutionCursor, PrRef,
    PrVerification,
};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    ApiError,
    auth::AuthenticatedCaller,
    routes::{AppState, principal_subject_owns_session, session_events_sse},
};

const DEFAULT_PR_FOLLOWUP_EXECUTIONS_LIMIT: i64 = 100;
const MAX_PR_FOLLOWUP_EXECUTIONS_LIMIT: i64 = 500;

#[derive(Debug, Deserialize)]
pub(crate) struct RegisterPrBindingRequest {
    repository: String,
    number: i64,
}

#[derive(Debug, Serialize)]
pub(crate) struct PrBindingStatusResponse {
    #[serde(flatten)]
    binding: PrBinding,
    session_exists: bool,
    active_execution: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct EnqueuePrFollowupResponse {
    followup: PrFollowup,
    created: bool,
}

fn pr_ref(owner: &str, repo: &str, number: i64) -> Result<PrRef, ApiError> {
    PrRef::new(&format!("{owner}/{repo}"), number)
        .map_err(|error| ApiError::from(SessionRuntimeError::from(error)))
}

/// `POST /api/session/{thread_key}/pr-bindings`: the session's own principal
/// (or an operator) records a PR the session opened. The binding stays
/// `registered`, and confers nothing, until githubbot verifies it.
pub(crate) async fn register_pr_binding(
    State(state): State<AppState>,
    Extension(caller): Extension<AuthenticatedCaller>,
    Path(raw_thread_key): Path<String>,
    Json(request): Json<RegisterPrBindingRequest>,
) -> Result<Json<PrBinding>, ApiError> {
    let thread_key = ThreadKey::try_from(raw_thread_key)?;
    let pr = PrRef::new(&request.repository, request.number).map_err(SessionRuntimeError::from)?;
    let runtime = state.runtime()?;
    let session = runtime.session(&thread_key).await?;
    if !principal_subject_owns_session(
        caller.principal_subject(),
        session.iron_control_principal.as_deref(),
    ) {
        return Err(ApiError::Forbidden(
            "caller is not authorized for this session".to_owned(),
        ));
    }
    Ok(Json(runtime.register_pr_binding(&thread_key, &pr).await?))
}

/// `GET /api/pr-bindings/{owner}/{repo}/{number}`.
pub(crate) async fn get_pr_binding(
    State(state): State<AppState>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
) -> Result<Json<PrBindingStatusResponse>, ApiError> {
    let pr = pr_ref(&owner, &repo, number)?;
    let status = state.runtime()?.pr_binding_status(&pr).await?;
    Ok(Json(PrBindingStatusResponse {
        binding: status.binding,
        session_exists: status.session_exists,
        active_execution: status.active_execution,
    }))
}

/// `POST /api/pr-bindings/{owner}/{repo}/{number}/verify`: githubbot's fresh
/// GitHub API facts after a signed webhook.
pub(crate) async fn verify_pr_binding(
    State(state): State<AppState>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(request): Json<PrVerification>,
) -> Result<Json<PrBinding>, ApiError> {
    let pr = pr_ref(&owner, &repo, number)?;
    Ok(Json(
        state.runtime()?.verify_pr_binding(&pr, &request).await?,
    ))
}

/// `POST /api/pr-bindings/{owner}/{repo}/{number}/state`.
pub(crate) async fn update_pr_binding_state(
    State(state): State<AppState>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(request): Json<PrBindingStateUpdate>,
) -> Result<Json<PrBinding>, ApiError> {
    let pr = pr_ref(&owner, &repo, number)?;
    Ok(Json(
        state
            .runtime()?
            .update_pr_binding_state(&pr, request)
            .await?,
    ))
}

/// `POST /api/pr-bindings/{owner}/{repo}/{number}/followups`.
pub(crate) async fn enqueue_pr_followup(
    State(state): State<AppState>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(request): Json<NewPrFollowup>,
) -> Result<Json<EnqueuePrFollowupResponse>, ApiError> {
    let pr = pr_ref(&owner, &repo, number)?;
    let (followup, created) = state.runtime()?.enqueue_pr_followup(&pr, &request).await?;
    Ok(Json(EnqueuePrFollowupResponse { followup, created }))
}

/// `POST /api/pr-bindings/{owner}/{repo}/{number}/dispatch`.
pub(crate) async fn dispatch_pr_followup(
    State(state): State<AppState>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(request): Json<PrDispatchRequest>,
) -> Result<Json<Value>, ApiError> {
    let pr = pr_ref(&owner, &repo, number)?;
    if request.current_head_sha.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "current_head_sha is required".to_owned(),
        ));
    }
    let outcome = state.runtime()?.dispatch_pr_followup(&pr, &request).await?;
    Ok(Json(match outcome {
        PrDispatchOutcome::Dispatched(dispatched) => json!({
            "dispatched": dispatched.followup,
            "execution_id": dispatched.execution.execution_id,
        }),
        PrDispatchOutcome::Skipped(skip) => json!({
            "dispatched": null,
            "reason": skip.as_str(),
        }),
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PrFollowupEventsQuery {
    after_event_id: Option<i64>,
}

/// `GET /api/pr-bindings/{owner}/{repo}/{number}/executions/{execution_id}/events`:
/// the replayable events of one execution this binding's queue dispatched,
/// in the `/api/session/{thread_key}/events` SSE format. Any other execution,
/// including one another binding dispatched, is a 404, so githubbot reads
/// only the turns it queued, never the rest of the bound session.
pub(crate) async fn stream_pr_followup_events(
    State(state): State<AppState>,
    Extension(caller): Extension<AuthenticatedCaller>,
    Path((owner, repo, number, execution_id)): Path<(String, String, i64, String)>,
    Query(query): Query<PrFollowupEventsQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let pr = pr_ref(&owner, &repo, number)?;
    let runtime = state.runtime()?;
    let Some(thread_key) = runtime
        .pr_followup_execution_thread(&pr, &execution_id)
        .await?
    else {
        return Err(ApiError::NotFound(format!(
            "no dispatched follow-up execution {execution_id} for {}#{}",
            pr.repository(),
            pr.number()
        )));
    };
    session_events_sse(
        &state,
        &runtime,
        &caller,
        thread_key,
        query.after_event_id.unwrap_or(0),
        Some(&execution_id),
    )
    .await
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListPrFollowupExecutionsQuery {
    thread_prefix: Option<String>,
    after: Option<String>,
    limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct PrFollowupExecutionItem {
    execution_id: String,
    thread_key: String,
    status: String,
    metadata: Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListPrFollowupExecutionsResponse {
    executions: Vec<PrFollowupExecutionItem>,
    next_cursor: Option<String>,
}

/// `GET /api/pr-followups/executions?thread_prefix=&after=&limit=`: finished
/// executions PR follow-up dispatch started, oldest completion first, on
/// sessions both under `thread_prefix` and on the caller's own platform. Each
/// item's `metadata` is only the server-set `pr_followup` object. Pass
/// `next_cursor` back as `after` for the next page; with nothing new it
/// repeats `after`.
pub(crate) async fn list_pr_followup_executions(
    State(state): State<AppState>,
    Extension(caller): Extension<AuthenticatedCaller>,
    Query(query): Query<ListPrFollowupExecutionsQuery>,
) -> Result<Json<ListPrFollowupExecutionsResponse>, ApiError> {
    let after = match query.after.as_deref().filter(|after| !after.is_empty()) {
        Some(after) => Some(
            PrFollowupExecutionCursor::parse(after)
                .ok_or_else(|| ApiError::BadRequest("invalid cursor".to_owned()))?,
        ),
        None => None,
    };
    let prefixes =
        scoped_thread_prefixes(query.thread_prefix.as_deref(), caller.platform_prefixes())?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_PR_FOLLOWUP_EXECUTIONS_LIMIT)
        .clamp(1, MAX_PR_FOLLOWUP_EXECUTIONS_LIMIT);
    let executions = state
        .runtime()?
        .list_terminal_pr_followup_executions(&prefixes, after.as_ref(), limit)
        .await?;
    let next_cursor = executions
        .last()
        .map(|execution| execution.cursor().encode())
        .or_else(|| after.map(|after| after.encode()));
    Ok(Json(ListPrFollowupExecutionsResponse {
        executions: executions
            .into_iter()
            .map(|execution| PrFollowupExecutionItem {
                execution_id: execution.execution_id,
                thread_key: execution.thread_key.as_str().to_owned(),
                status: execution.status.as_ref().to_owned(),
                metadata: json!({ "pr_followup": execution.pr_followup }),
            })
            .collect(),
        next_cursor,
    }))
}

/// The thread-key prefixes a listing may cover: the requested prefix
/// intersected with the caller's own platform prefixes, so an ingress never
/// sees another platform's sessions however it asks. An empty result lists
/// nothing. A caller without platform prefixes must name one.
fn scoped_thread_prefixes(
    requested: Option<&str>,
    platform: Option<&[&str]>,
) -> Result<Vec<String>, ApiError> {
    let requested = requested.filter(|prefix| !prefix.is_empty());
    let mut prefixes = match (requested, platform) {
        (requested, Some(platform)) => platform
            .iter()
            .filter_map(|own| match requested {
                None => Some((*own).to_owned()),
                Some(requested) if requested.starts_with(own) => Some(requested.to_owned()),
                Some(requested) if own.starts_with(requested) => Some((*own).to_owned()),
                Some(_) => None,
            })
            .collect::<Vec<_>>(),
        (Some(requested), None) => vec![requested.to_owned()],
        (None, None) => {
            return Err(ApiError::BadRequest("thread_prefix is required".to_owned()));
        }
    };
    prefixes.sort();
    prefixes.dedup();
    Ok(prefixes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_prefixes_never_leave_the_callers_platform() {
        let slack: &[&str] = &["slack:"];
        let github: &[&str] = &["github:", "github-issue:", "github-manage:"];
        let scoped = |requested, platform| scoped_thread_prefixes(requested, platform).unwrap();
        assert_eq!(scoped(Some("slack:"), Some(slack)), ["slack:"]);
        assert_eq!(scoped(None, Some(slack)), ["slack:"]);
        assert_eq!(scoped(Some(""), Some(slack)), ["slack:"]);
        // Narrower than the platform keeps the narrower prefix.
        assert_eq!(scoped(Some("slack:T1:"), Some(slack)), ["slack:T1:"]);
        // Wider than the platform shrinks to the platform.
        assert_eq!(scoped(Some("s"), Some(slack)), ["slack:"]);
        assert_eq!(scoped(Some(""), Some(github)).len(), 3);
        assert_eq!(scoped(Some("github"), Some(github)).len(), 3);
        // Another platform's prefix yields nothing.
        assert!(scoped(Some("github:"), Some(slack)).is_empty());
        assert!(scoped(Some("slackx:"), Some(slack)).is_empty());
        assert!(scoped(Some("discord:"), Some(slack)).is_empty());
        // Unscoped callers must name a prefix.
        assert_eq!(scoped(Some("slack:"), None), ["slack:"]);
        assert!(scoped_thread_prefixes(None, None).is_err());
    }
}
