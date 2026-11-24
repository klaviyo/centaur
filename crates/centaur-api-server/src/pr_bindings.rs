//! PR session continuity: create a pull request as the authorized human from
//! inside a Slack session, bind it to exactly that session, and route one
//! failed-CI notice back to the session's Slack thread, posted synchronously
//! while githubbot's fresh read of the PR is still true.
//!
//! Trust model. A binding is established in one place only: the trusted
//! creation route below. It accepts a call only from the sandbox's own proxy
//! identity (the API token the proxy injects carries the sandbox id and the
//! GitHub account it would act as), checks that sandbox belongs to this
//! session, makes the GitHub call through that same proxy so GitHub authors the
//! PR as the human, and inserts a row only when GitHub's own 201 response
//! agrees on repository, user type and author. A webhook never creates or
//! rebinds anything. The `/api/pr-bindings/*` routes are githubbot's alone
//! (`Capability::PrBindingsManage`) and can only read and update the state of
//! a binding that already exists.

use std::future::Future;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use centaur_sandbox_core::{PrOauthPlaceholder, SandboxError};
use centaur_session_core::{ChatDestination, ExecutionStatus, ThreadKey};
use centaur_session_runtime::SessionRuntimeError;
use centaur_session_sqlx::{
    NewPrBinding, PgSessionStore, PrBinding, PrBindingStateUpdate, PrCiNoticeClaim, PrCreateLock,
    SessionStoreError, execution_requester, normalize_pr_repository,
};
use centaur_workflows::{WorkflowRuntimeError, send_slack_request};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::{
    ApiError,
    auth::AuthenticatedCaller,
    routes::{AppState, principal_subject_owns_session},
};

const MAX_TITLE_CHARS: usize = 256;
const MAX_BODY_CHARS: usize = 65_536;
const MAX_REF_CHARS: usize = 255;
const MAX_CHECK_NAME_CHARS: usize = 200;
const MAX_URL_CHARS: usize = 2_048;
const MAX_GITHUB_MESSAGE_CHARS: usize = 300;

/// `POST /api/session/{thread_key}/pull-requests` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePullRequestRequest {
    repository: String,
    head: String,
    base: String,
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
}

/// A validated create request: the repository as a lowercased `owner/repo`
/// and the exact JSON GitHub receives.
#[derive(Debug)]
struct ValidatedPullRequest {
    repository: String,
    owner: String,
    repo: String,
    payload: Value,
}

impl CreatePullRequestRequest {
    fn validate(self) -> Result<ValidatedPullRequest, ApiError> {
        let invalid = |message: &str| ApiError::BadRequest(message.to_owned());
        let repository = normalize_pr_repository(&self.repository)
            .map_err(|_| invalid("repository must be owner/repo"))?;
        let (owner, repo) = repository
            .split_once('/')
            .map(|(owner, repo)| (owner.to_owned(), repo.to_owned()))
            .ok_or_else(|| invalid("repository must be owner/repo"))?;
        let title = self.title.trim();
        if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
            return Err(invalid("title must be 1 to 256 characters"));
        }
        for (label, reference) in [("head", &self.head), ("base", &self.base)] {
            if !is_branch_ref(reference) {
                return Err(ApiError::BadRequest(format!(
                    "{label} must be a branch name of at most {MAX_REF_CHARS} characters"
                )));
            }
        }
        let body = self.body.unwrap_or_default();
        if body.chars().count() > MAX_BODY_CHARS {
            return Err(invalid("body is too long"));
        }
        let payload = json!({
            "title": title,
            "head": self.head,
            "base": self.base,
            "body": body,
            "draft": self.draft.unwrap_or(false),
        });
        Ok(ValidatedPullRequest {
            repository,
            owner,
            repo,
            payload,
        })
    }
}

/// A branch (or `owner:branch`) name: non-empty, no whitespace or control
/// characters.
fn is_branch_ref(reference: &str) -> bool {
    !reference.is_empty()
        && reference.chars().count() <= MAX_REF_CHARS
        && !reference
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

/// What GitHub's 201 body says about the PR it created.
#[derive(Debug, Deserialize)]
struct GithubPullRequest {
    id: i64,
    node_id: String,
    number: i64,
    html_url: String,
    user: GithubUser,
    head: GithubRef,
    base: GithubBaseRef,
}

#[derive(Debug, Deserialize)]
struct GithubUser {
    id: i64,
    login: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct GithubRef {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GithubBaseRef {
    #[serde(rename = "ref")]
    name: String,
    repo: GithubRepo,
}

#[derive(Debug, Deserialize)]
struct GithubRepo {
    full_name: String,
}

impl GithubPullRequest {
    /// Why this PR must not be bound, judged from GitHub's response alone:
    /// it must be in the requested repository and authored by a `User` whose
    /// GitHub id is the one the caller's proxy attests.
    fn rejection(&self, repository: &str, github_user_id: &str) -> Option<&'static str> {
        if self.base.repo.full_name.to_ascii_lowercase() != repository {
            return Some("repository_mismatch");
        }
        if self.user.kind != "User" {
            return Some("not_a_user");
        }
        if self.user.id.to_string() != github_user_id {
            return Some("author_mismatch");
        }
        None
    }

    fn summary(&self) -> Value {
        json!({
            "number": self.number,
            "html_url": self.html_url,
            "node_id": self.node_id,
        })
    }
}

/// `POST /api/session/{thread_key}/pull-requests`: creates a pull request
/// through the calling sandbox's proxy, as the human its proxy acts for, and
/// binds it to this session when GitHub's response proves the author.
///
/// Access (each failure has its own `code`): a principal caller that owns the
/// session, whose proxy-attested sandbox is the session's one sandbox, on a
/// Slack session that is not a DM and has a running execution whose requester
/// api-rs bound is the one its proxy attests, with a GitHub account attested
/// by its proxy.
///
/// The thread's proxy-binding lock, the one every rebind and unbind of that
/// thread's proxy takes, is held from before the sandbox, execution and
/// requester checks until the response is handled. A cheap ownership check
/// runs first, without the lock, so a caller that does not own the session is
/// refused without ever waiting on or holding it. A turn change cannot rebind the proxy to the next
/// speaker between the checks and the GitHub call, so the token GitHub sees is
/// the one the checks vouched for.
pub(crate) async fn create_session_pull_request(
    State(state): State<AppState>,
    Extension(caller): Extension<AuthenticatedCaller>,
    Path(raw_thread_key): Path<String>,
    Json(request): Json<CreatePullRequestRequest>,
) -> Result<Json<Value>, ApiError> {
    create_session_pull_request_with(&state, &caller, raw_thread_key, request, || async {})
        .await
        .map(Json)
}

/// Refuses a caller that is not a principal owning `session`.
fn authorize_session_owner(
    caller: &AuthenticatedCaller,
    session: &centaur_session_core::Session,
) -> Result<(), ApiError> {
    if caller.principal_subject().is_none()
        || !principal_subject_owns_session(
            caller.principal_subject(),
            session.iron_control_principal.as_deref(),
        )
    {
        return Err(ApiError::Forbidden(
            "caller is not authorized for this session".to_owned(),
        ));
    }
    Ok(())
}

/// [`create_session_pull_request`], with `before_github` run while the
/// binding lock is held, just before the GitHub call. Production passes a
/// no-op; tests use it to pause creation mid-flight.
async fn create_session_pull_request_with<B, BFut>(
    state: &AppState,
    caller: &AuthenticatedCaller,
    raw_thread_key: String,
    request: CreatePullRequestRequest,
    before_github: B,
) -> Result<Value, ApiError>
where
    B: FnOnce() -> BFut,
    BFut: Future<Output = ()>,
{
    // Off, nothing below runs: no session read, no lock and no sandbox call.
    // The SDK treats this code as "use the ordinary flow".
    if !state.trusted_pr_create_enabled() {
        return Err(ApiError::coded(
            StatusCode::NOT_IMPLEMENTED,
            "pull_requests_unsupported",
            "trusted pull request creation is not enabled in this deployment",
        ));
    }
    let thread_key = ThreadKey::try_from(raw_thread_key)?;
    let request = request.validate()?;
    let runtime = state.runtime()?;
    // A cheap read-only check first, so a caller that does not own the session
    // is refused before it can wait on, or hold, the thread's proxy-binding
    // lock. It is repeated under the lock, where the session is read afresh.
    let session = runtime.session(&thread_key).await?;
    authorize_session_owner(caller, &session)?;
    let _proxy_binding = runtime
        .store()
        .lock_proxy_binding(&thread_key)
        .await
        .map_err(SessionRuntimeError::from)?;
    let session = runtime.session(&thread_key).await?;
    authorize_session_owner(caller, &session)?;

    // The proxy mints the sandbox id from its own record; the session must
    // name that same sandbox and no other session may.
    let Some(sandbox_id) = caller.sandbox_id() else {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "sandbox_claim_required",
            "this token does not name a sandbox",
        ));
    };
    let sandbox_matches = session.sandbox_id.as_deref() == Some(sandbox_id)
        && runtime
            .store()
            .unique_session_for_sandbox(sandbox_id)
            .await
            .map_err(SessionRuntimeError::from)?
            .as_ref()
            == Some(&thread_key);
    if !sandbox_matches {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "sandbox_mismatch",
            "the calling sandbox does not belong to this session",
        ));
    }

    // `chat_destination` also reads unprefixed legacy keys; only `slack:`
    // sessions are Slack sessions here.
    match thread_key.chat_destination() {
        Some(ChatDestination::Slack { channel_id, .. })
            if thread_key.as_str().starts_with("slack:") =>
        {
            if channel_id.starts_with('D') {
                return Err(ApiError::coded(
                    StatusCode::FORBIDDEN,
                    "dm_not_supported",
                    "pull requests are not bound to direct-message sessions",
                ));
            }
        }
        _ => {
            return Err(ApiError::coded(
                StatusCode::FORBIDDEN,
                "slack_session_required",
                "pull requests can only be bound to Slack sessions",
            ));
        }
    }
    // The turn must be running, not merely queued, and the proxy must be
    // acting for the requester api-rs bound for it. A speaker change can leave
    // the proxy on the previous person's credentials for a moment; a PR
    // created then would be authored, and bound, as someone who is not
    // speaking. api-rs's own record is the reference, never the caller's word.
    let execution = runtime
        .store()
        .active_execution_for_thread(&thread_key)
        .await
        .map_err(SessionRuntimeError::from)?
        .filter(|execution| execution.status == ExecutionStatus::Running);
    let Some(execution) = execution else {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "no_active_execution",
            "the session has no running execution",
        ));
    };
    if execution_requester(&execution) != Some(caller.requester_principal_id()) {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "requester_not_applied",
            "the sandbox proxy is not yet acting for this turn's requester; try again",
        ));
    }
    let Some(github_user_id) = caller.github_user_id() else {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "github_oauth_required",
            "connect your GitHub account in Console so pull requests are created as you",
        ));
    };

    // From here to the binding insert, a lookup that misses must wait for this
    // creation in any repository (see `lock_pr_create`). It is shared, so
    // creations never wait on each other, and it is taken after the thread's
    // lock, always in that order.
    let pr_create = runtime
        .store()
        .lock_pr_create()
        .await
        .map_err(SessionRuntimeError::from)?;
    // The bearer placeholder is derived from the very claims validated above
    // (the attested account and the requester that equals api-rs's record), so
    // the proxy's guard honours it only while it is still configured for
    // exactly this account and requester.
    let placeholder = PrOauthPlaceholder::derive(github_user_id, caller.requester_principal_id())
        .map_err(sandbox_failure_spec)?;
    before_github().await;
    let response = runtime
        .create_github_pull_request(
            sandbox_id,
            &request.owner,
            &request.repo,
            &placeholder,
            request.payload,
        )
        .await
        .map_err(sandbox_failure)?;
    if response.status != StatusCode::CREATED.as_u16() {
        return Err(github_failure(response.status, &response.body));
    }
    let pull_request: GithubPullRequest = serde_json::from_slice(&response.body).map_err(|_| {
        ApiError::coded(
            StatusCode::BAD_GATEWAY,
            "github_response_invalid",
            "GitHub's response did not describe a pull request",
        )
    })?;

    // Insert inside the transaction that holds the repository lock, and commit
    // it (which releases the lock) before answering: a lookup waiting on the
    // lock then finds the binding. If that transaction has failed, nothing is
    // bound and the PR stays on the unbound flow.
    let mut pr_create = pr_create;
    let outcome = match pull_request.rejection(&request.repository, github_user_id) {
        Some(reason) => Err(reason),
        None => bind(
            &mut pr_create,
            &thread_key,
            sandbox_id,
            &request.repository,
            &pull_request,
        )
        .await
        .map(|_| ()),
    };
    let outcome = match outcome {
        Ok(()) => pr_create.commit().await.map_err(|error| {
            tracing::warn!(%error, "failed to commit the PR binding");
            "binding_unavailable"
        }),
        Err(reason) => {
            if let Err(error) = pr_create.release().await {
                tracing::warn!(%error, "failed to release the PR-create lock");
            }
            Err(reason)
        }
    };
    Ok(match outcome {
        Ok(()) => json!({"pull_request": pull_request.summary(), "bound": true}),
        Err(reason) => {
            // The PR exists on GitHub and stays on today's unbound flow.
            tracing::info!(
                event = "pr_binding_rejected",
                thread_key = %thread_key,
                repository = %request.repository,
                number = pull_request.number,
                reason,
                "created pull request was not bound"
            );
            json!({"pull_request": pull_request.summary(), "bound": false, "reason": reason})
        }
    })
}

/// Inserts the binding for a PR GitHub has proven authored by the caller.
async fn bind(
    lock: &mut PrCreateLock,
    thread_key: &ThreadKey,
    sandbox_id: &str,
    repository: &str,
    pull_request: &GithubPullRequest,
) -> Result<PrBinding, &'static str> {
    lock.insert_binding(&NewPrBinding {
        repository: repository.to_owned(),
        number: pull_request.number,
        thread_key: thread_key.clone(),
        pr_id: pull_request.id,
        pr_node_id: pull_request.node_id.clone(),
        author_github_id: pull_request.user.id,
        author_login: pull_request.user.login.clone(),
        sandbox_id: sandbox_id.to_owned(),
        head_ref: pull_request.head.name.clone(),
        base_ref: pull_request.base.name.clone(),
        head_sha: pull_request.head.sha.clone(),
    })
    .await
    .map_err(|error| match error {
        SessionStoreError::PrBindingConflict { .. } => "binding_conflict",
        other => {
            tracing::warn!(error = %other, "failed to store PR binding");
            "binding_unavailable"
        }
    })
}

/// An upstream failure carrying only GitHub's own `message`.
fn github_failure(status: u16, body: &[u8]) -> ApiError {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|body| body.get("message")?.as_str().map(str::to_owned))
        .map(|message| {
            message
                .chars()
                .filter(|character| !character.is_control())
                .take(MAX_GITHUB_MESSAGE_CHARS)
                .collect::<String>()
        })
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| format!("GitHub did not create the pull request (HTTP {status})"));
    ApiError::coded(StatusCode::BAD_GATEWAY, "github_error", message)
}

/// Maps a placeholder that cannot be derived from the validated claims (an
/// account id that is not numeric, say): the call is never made.
fn sandbox_failure_spec(error: SandboxError) -> ApiError {
    tracing::warn!(%error, "the PR-create placeholder could not be derived");
    ApiError::coded(
        StatusCode::CONFLICT,
        "github_oauth_required",
        "connect your GitHub account in Console so pull requests are created as you",
    )
}

/// Maps a failed sandbox call without echoing backend internals to the caller.
fn sandbox_failure(error: SessionRuntimeError) -> ApiError {
    match error {
        SessionRuntimeError::Sandbox(SandboxError::InvalidSpec(_)) => {
            ApiError::BadRequest("repository must be owner/repo".to_owned())
        }
        SessionRuntimeError::Sandbox(SandboxError::Unsupported { .. }) => ApiError::coded(
            StatusCode::NOT_IMPLEMENTED,
            "pull_requests_unsupported",
            "this deployment cannot create pull requests through the sandbox proxy",
        ),
        error => {
            tracing::warn!(
                error = %crate::error::error_chain(&error),
                "pull request create through the sandbox proxy failed"
            );
            ApiError::coded(
                StatusCode::BAD_GATEWAY,
                "sandbox_proxy_unavailable",
                "could not reach GitHub through the sandbox proxy",
            )
        }
    }
}

fn pr_path(owner: &str, repo: &str, number: i64) -> (String, i64) {
    (format!("{owner}/{repo}"), number)
}

/// `GET /api/pr-bindings/{owner}/{repo}/{number}` query.
#[derive(Debug, Deserialize)]
pub(crate) struct BindingLookupQuery {
    /// The PR's GraphQL node id. When given, the binding is found by it alone,
    /// wherever the path's repository and number point (which is how a renamed
    /// or transferred repository still finds its binding), and a different
    /// PR's older binding at the path's coordinates is never returned. The path
    /// is used only when no node id is given.
    #[serde(default)]
    node_id: Option<String>,
    /// Make a miss final. See [`get_pr_binding`].
    #[serde(default)]
    authoritative: bool,
}

/// `GET /api/pr-bindings/{owner}/{repo}/{number}[?node_id=…][&authoritative=true]`:
/// the binding (with its stored repository and number, for the caller's later
/// calls), or 404 with `code: "pr_binding_not_found"` (only a completed lookup
/// answers that; any other 404, such as a replica without the route, does not).
/// With `node_id` the lookup is by that node id alone; without
/// it, by the path.
///
/// A plain miss answers at once, without locking. With `authoritative=true`
/// a miss is final: before answering 404 the lookup takes the PR-create lock
/// exclusively, which waits for every PR creation in flight in any
/// repository (not just the path's, since GitHub may have renamed it by the
/// time the creation commits), reads again while holding it, and only then
/// lets go. A creation commits its binding before releasing its shared lock,
/// so that read is final for every creation that had started. Callers use it
/// where a wrong "unbound" would matter: a bot assignment, a red head on a
/// PR the bot owns.
pub(crate) async fn get_pr_binding(
    State(state): State<AppState>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Query(query): Query<BindingLookupQuery>,
) -> Result<Json<PrBinding>, ApiError> {
    let (repository, number) = pr_path(&owner, &repo, number);
    let runtime = state.runtime()?;
    let store = runtime.store();
    // With a node id, only that PR's own binding is a hit; without one, the
    // path names the PR. An older row at the same coordinates for a different
    // node (a deleted and recreated PR) is never this PR's binding.
    let lookup = || async {
        match query.node_id.as_deref() {
            Some(node_id) => store.pr_binding_by_node_id(node_id).await,
            None => store.pr_binding(&repository, number).await,
        }
    };
    let mut found = lookup().await.map_err(SessionRuntimeError::from)?;
    if found.is_none() && query.authoritative {
        let guard = store
            .lock_pr_create_exclusive()
            .await
            .map_err(SessionRuntimeError::from)?;
        found = lookup().await.map_err(SessionRuntimeError::from)?;
        guard.release().await.map_err(SessionRuntimeError::from)?;
    }
    // A completed lookup that found nothing carries a stable code: a bare 404
    // is also what a replica without this route answers mid-rollout, which a
    // caller must not read as "unbound".
    found.map(Json).ok_or_else(|| {
        ApiError::coded(
            StatusCode::NOT_FOUND,
            "pr_binding_not_found",
            format!("no PR binding for {repository}#{number}"),
        )
    })
}

/// `POST /api/pr-bindings/{owner}/{repo}/{number}/state` body. `pr_node_id`
/// names the pull request githubbot is talking about; a repository name and
/// number can be reused by a different PR, so the update applies only to a
/// binding for that exact PR. `observed_updated_at` is the fetched PR's
/// `updated_at`, which orders writes (see
/// [`PgSessionStore::update_pr_binding_state`]).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateUpdateRequest {
    pr_node_id: String,
    #[serde(with = "time::serde::rfc3339")]
    observed_updated_at: OffsetDateTime,
    #[serde(default)]
    managed: Option<bool>,
    #[serde(default)]
    closed: Option<bool>,
    #[serde(default)]
    head_sha: Option<String>,
    /// The binding `revision` the caller read (see the binding GET): lets a
    /// write that contradicts a field at the very same observation time apply
    /// as a compare-and-set. Without it, or with a stale one, such a write is a
    /// 409 `binding_state_conflict`.
    #[serde(default)]
    expected_revision: Option<i64>,
}

/// The binding for `repository#number`, which must be for the PR `pr_node_id`
/// names: 404 when unbound and 409 `pr_identity_mismatch` for another PR that
/// reuses the number.
async fn binding_for_pr(
    store: &PgSessionStore,
    repository: &str,
    number: i64,
    pr_node_id: &str,
) -> Result<PrBinding, ApiError> {
    let binding = store
        .pr_binding(repository, number)
        .await
        .map_err(SessionRuntimeError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("no PR binding for {repository}#{number}")))?;
    if binding.pr_node_id != pr_node_id {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "pr_identity_mismatch",
            "the binding is for a different pull request than the one named",
        ));
    }
    Ok(binding)
}

/// `POST /api/pr-bindings/{owner}/{repo}/{number}/state`: records `managed`,
/// `closed` and `head_sha` on an existing binding for the named PR. Each field
/// is ordered by its own observation time: a field a later observation already
/// set is left alone, and `applied` reports whether any field changed. It
/// cannot create or rebind a binding. Returns the binding and `applied`.
pub(crate) async fn update_pr_binding_state(
    State(state): State<AppState>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(request): Json<StateUpdateRequest>,
) -> Result<Json<Value>, ApiError> {
    let (repository, number) = pr_path(&owner, &repo, number);
    let runtime = state.runtime()?;
    let store = runtime.store();
    binding_for_pr(store, &repository, number, &request.pr_node_id).await?;
    let outcome = store
        .update_pr_binding_state(
            &repository,
            number,
            &request.pr_node_id,
            &PrBindingStateUpdate {
                managed: request.managed,
                closed: request.closed,
                head_sha: request.head_sha,
                expected_revision: request.expected_revision,
            },
            Some(request.observed_updated_at),
        )
        .await
        .map_err(SessionRuntimeError::from)?;
    let mut body = serde_json::to_value(&outcome.binding)?;
    body["applied"] = json!(outcome.applied);
    Ok(Json(body))
}

/// `POST /api/pr-bindings/{owner}/{repo}/{number}/ci-failures` body: what
/// githubbot saw in a fresh GitHub fetch just before calling.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CiFailureRequest {
    /// The pull request's GraphQL node id; must be the bound PR's.
    pr_node_id: String,
    /// The fetched PR's `updated_at`; orders the closure and unassignment this
    /// report records against other state writes.
    #[serde(with = "time::serde::rfc3339")]
    observed_updated_at: OffsetDateTime,
    /// The head the failing check ran on.
    head_sha: String,
    /// The PR's head now.
    current_head_sha: String,
    open: bool,
    assigned: bool,
    check_name: String,
    #[serde(default)]
    details_url: Option<String>,
    html_url: String,
    /// The repository and number GitHub reports for this PR now, from
    /// githubbot's fresh read; they differ from the binding's stored ones
    /// after a rename or transfer. Both or neither: older githubbot sends
    /// neither and the stored coordinates are used. They only decide where the
    /// notice links; the node id and the stored coordinates still govern
    /// state and dedupe.
    #[serde(default)]
    current_repository: Option<String>,
    #[serde(default)]
    current_number: Option<i64>,
}

impl CiFailureRequest {
    fn validate(&self) -> Result<(), ApiError> {
        let invalid = |message: &str| Err(ApiError::BadRequest(message.to_owned()));
        for (label, value) in [
            ("head_sha", &self.head_sha),
            ("current_head_sha", &self.current_head_sha),
        ] {
            if value.trim().is_empty() || value.chars().any(char::is_whitespace) {
                return invalid(&format!("{label} is required"));
            }
        }
        if self.check_name.trim().is_empty()
            || self.check_name.chars().count() > MAX_CHECK_NAME_CHARS
        {
            return invalid("check_name must be 1 to 200 characters");
        }
        if !is_usable_url(&self.html_url) {
            return invalid("html_url must be an https URL");
        }
        match (&self.current_repository, self.current_number) {
            (None, None) => {}
            (Some(repository), Some(number)) => {
                if normalize_pr_repository(repository).is_err() || number <= 0 {
                    return invalid("current_repository and current_number must name a PR");
                }
            }
            _ => return invalid("current_repository and current_number go together"),
        }
        Ok(())
    }

    /// The repository (lowercased) and number the notice names and links: the
    /// PR's current ones when reported, else the binding's stored ones.
    fn notice_coordinates(&self, binding: &PrBinding) -> (String, i64) {
        match (&self.current_repository, self.current_number) {
            (Some(repository), Some(number)) => (
                normalize_pr_repository(repository).unwrap_or_else(|_| binding.repository.clone()),
                number,
            ),
            _ => (binding.repository.clone(), binding.number),
        }
    }

    /// The failing check's own page, when it has a usable one. An unusable
    /// link only costs the notice its link; it never costs the notice.
    fn usable_details_url(&self) -> Option<&str> {
        self.details_url.as_deref().filter(|url| is_usable_url(url))
    }
}

fn is_usable_url(url: &str) -> bool {
    url.starts_with("https://") && url.len() <= MAX_URL_CHARS
}

/// `POST /api/pr-bindings/{owner}/{repo}/{number}/ci-failures`: posts the
/// failed-CI notice to the bound Slack thread, once per `(PR, head)`, before
/// the request returns.
pub(crate) async fn report_pr_ci_failure(
    State(state): State<AppState>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(request): Json<CiFailureRequest>,
) -> Result<Json<Value>, ApiError> {
    let (repository, number) = pr_path(&owner, &repo, number);
    let runtime = state.runtime()?;
    process_ci_failure(runtime.store(), &repository, number, &request, |message| {
        send_slack_request("chat.postMessage", message)
    })
    .await
    .map(Json)
}

/// Decides whether a failed check still deserves a notice and, if so, posts it.
///
/// Nothing is posted unless the request is for the bound PR and, in the fresh
/// read githubbot made just before calling, the PR is open and assigned, the
/// binding is managed and not closed, and the failing head is the binding's
/// head and the PR's current head. Closure and unassignment are recorded on
/// the binding (ordered by `observed_updated_at`); the head never is. The post
/// happens here, in the same request, so the checks cannot go stale in a
/// queue. It is claimed once per `(PR node id, head)` first; a failed post
/// releases the claim and fails the request, so a later event can try again.
async fn process_ci_failure<P, Fut>(
    store: &PgSessionStore,
    repository: &str,
    number: i64,
    request: &CiFailureRequest,
    post: P,
) -> Result<Value, ApiError>
where
    P: FnOnce(Value) -> Fut,
    Fut: Future<Output = Result<Value, WorkflowRuntimeError>>,
{
    process_ci_failure_with(store, repository, number, request, || async {}, post).await
}

/// Why a failed check on `binding` deserves no notice, if it doesn't: the
/// PR is closed or unassigned in the fresh read, the binding is not managed
/// or is closed, or the failing head is not the binding's head and the PR's
/// current head. The head is maintained only by the state route, from
/// githubbot's fresh fetch; a failure report never moves it.
fn skip_reason(binding: &PrBinding, request: &CiFailureRequest) -> Option<&'static str> {
    let current =
        binding.head_sha == request.head_sha && request.head_sha == request.current_head_sha;
    if !request.open {
        Some("not_open")
    } else if !request.assigned {
        Some("not_assigned")
    } else if !binding.managed {
        Some("not_managed")
    } else if binding.closed {
        Some("closed")
    } else if !current {
        Some("stale_head")
    } else {
        None
    }
}

/// [`process_ci_failure`], with `before_claim` run between the binding read
/// and the claim. That is the window in which the binding can change under
/// the request; production passes a no-op, and tests commit a change there.
async fn process_ci_failure_with<B, BFut, P, Fut>(
    store: &PgSessionStore,
    repository: &str,
    number: i64,
    request: &CiFailureRequest,
    before_claim: B,
    post: P,
) -> Result<Value, ApiError>
where
    B: FnOnce() -> BFut,
    BFut: Future<Output = ()>,
    P: FnOnce(Value) -> Fut,
    Fut: Future<Output = Result<Value, WorkflowRuntimeError>>,
{
    request.validate()?;
    let binding = binding_for_pr(store, repository, number, &request.pr_node_id).await?;

    if let Some(reason) = skip_reason(&binding, request) {
        // Closure and unassignment are facts about the PR, recorded so later
        // events for the same head stay dropped.
        let mut update = PrBindingStateUpdate::default();
        if !request.open && !binding.closed {
            update.closed = Some(true);
        }
        if !request.assigned && binding.managed {
            update.managed = Some(false);
        }
        if update != PrBindingStateUpdate::default() {
            store
                .update_pr_binding_state(
                    &binding.repository,
                    binding.number,
                    &binding.pr_node_id,
                    &update,
                    Some(request.observed_updated_at),
                )
                .await
                .map_err(SessionRuntimeError::from)?;
        }
        return Ok(json!({"notice": "skipped", "reason": reason}));
    }

    // Only a Slack thread outside a DM is ever a binding's destination, and
    // the link in the notice must be the pull request itself.
    let Some(message) = notice_message(&binding, request) else {
        let reason = match binding.thread_key.chat_destination() {
            Some(ChatDestination::Slack { channel_id, .. }) if !channel_id.starts_with('D') => {
                "invalid_html_url"
            }
            _ => "not_slack_thread",
        };
        return Ok(json!({"notice": "skipped", "reason": reason}));
    };

    before_claim().await;
    // One conditional insert: it claims only while the binding is still this
    // PR at this head, managed and open, whatever happened since the read.
    match store
        .claim_pr_ci_notice(
            &binding.repository,
            binding.number,
            &binding.pr_node_id,
            &request.head_sha,
        )
        .await
        .map_err(SessionRuntimeError::from)?
    {
        PrCiNoticeClaim::Claimed => {}
        PrCiNoticeClaim::Duplicate => return Ok(json!({"notice": "duplicate"})),
        PrCiNoticeClaim::NotEligible => {
            // Changed after the read: say why from the binding as it is now.
            let current = binding_for_pr(store, repository, number, &request.pr_node_id).await?;
            let reason = skip_reason(&current, request).unwrap_or("binding_changed");
            return Ok(json!({"notice": "skipped", "reason": reason}));
        }
    }
    if let Err(error) = post(message).await {
        tracing::warn!(
            event = "pr_ci_notice_post_failed",
            repository = %binding.repository,
            number = binding.number,
            error = %error,
            "failed to post the CI notice"
        );
        store
            .release_pr_ci_notice(&binding.pr_node_id, &request.head_sha)
            .await
            .map_err(SessionRuntimeError::from)?;
        return Err(ApiError::coded(
            StatusCode::BAD_GATEWAY,
            "slack_post_failed",
            "could not post the CI notice to Slack",
        ));
    }
    Ok(json!({"notice": "posted"}))
}

/// The `chat.postMessage` payload for the failed-CI notice of a binding, or
/// `None` when the notice must not be posted: the binding's thread is not a
/// Slack thread outside a DM, or `html_url` is not the pull request's own URL
/// at its current coordinates (compared without regard to the case of owner and
/// repo).
///
/// The channel and `thread_ts` come only from the bound thread key; the text
/// only from the check's own fields, escaped for Slack mrkdwn.
fn notice_message(binding: &PrBinding, request: &CiFailureRequest) -> Option<Value> {
    let Some(ChatDestination::Slack {
        channel_id,
        thread_ts,
    }) = binding.thread_key.chat_destination()
    else {
        return None;
    };
    if channel_id.starts_with('D') {
        return None;
    }
    // The PR's current coordinates (after a rename, say), not the stored ones.
    let (repository, number) = request.notice_coordinates(binding);
    let pull_request_url = format!("https://github.com/{repository}/pull/{number}");
    if !request.html_url.eq_ignore_ascii_case(&pull_request_url) {
        return None;
    }
    let sha7: String = request.head_sha.chars().take(7).collect();
    let pull_request = slack_link(Some(&request.html_url), &format!("{repository}#{number}"));
    let check = slack_link(request.usable_details_url(), &request.check_name);
    Some(json!({
        "channel": channel_id,
        "thread_ts": thread_ts,
        "text": format!(
            "CI failed on {pull_request} at `{sha7}`: {check}\n@-mention me in this thread if you'd like me to fix it."
        ),
        "unfurl_links": false,
        "unfurl_media": false,
        "client_msg_id": format!("pr-ci-notice:{}:{}", binding.pr_node_id, request.head_sha),
    }))
}

/// `<url|label>` for an https `url`, otherwise just the escaped label. A URL
/// that could break out of the link markup is never embedded.
fn slack_link(url: Option<&str>, label: &str) -> String {
    let label = escape_slack_mrkdwn(label);
    match url {
        Some(url)
            if url.starts_with("https://")
                && !url.chars().any(|c| {
                    c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '|')
                }) =>
        {
            format!("<{url}|{label}>")
        }
        _ => label,
    }
}

fn escape_slack_mrkdwn(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, header},
    };
    use centaur_sandbox_core::{
        ObservedSandbox, ProxyHttpsResponse, SandboxBackend, SandboxHandle, SandboxId, SandboxIo,
        SandboxResult, SandboxSpec, SandboxStatus,
    };
    use centaur_session_core::HarnessType;
    use centaur_session_runtime::{SandboxRuntime, SessionPrincipalRegistrar, SessionRuntime};
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::{ApiAuthConfig, build_router_with_app_state};

    // ---- fixtures -------------------------------------------------------

    #[derive(Clone, Copy)]
    struct Registrar;

    #[async_trait]
    impl SessionPrincipalRegistrar for Registrar {
        async fn register_session(
            &self,
            _thread_key: &str,
            _metadata: Option<&Value>,
            _create_if_missing: bool,
        ) -> Result<centaur_iron_control::Principal, centaur_iron_control::IronControlError>
        {
            unreachable!("PR routes never register sessions")
        }

        async fn register_requester(
            &self,
            _thread_key: &str,
            _metadata: Option<&Value>,
            _create_if_missing: bool,
        ) -> Result<Option<centaur_iron_control::Principal>, centaur_iron_control::IronControlError>
        {
            unreachable!("PR routes never register requesters")
        }

        async fn get_principal(
            &self,
            _principal: &str,
        ) -> Result<centaur_iron_control::Principal, centaur_iron_control::IronControlError>
        {
            unreachable!("PR routes never read principals")
        }
    }

    /// One recorded `create_github_pull_request` call.
    #[derive(Clone, Debug, PartialEq)]
    struct Call {
        sandbox_id: String,
        owner: String,
        repo: String,
        /// The bearer placeholder the call was given.
        placeholder: String,
        payload: Value,
    }

    /// A sandbox backend whose GitHub call is answered by the test, so no
    /// network is ever reached.
    #[derive(Default)]
    struct GithubBackend {
        calls: Mutex<Vec<Call>>,
        response: Mutex<Option<(u16, Value)>>,
    }

    impl GithubBackend {
        fn respond(&self, status: u16, body: Value) {
            *self.response.lock().unwrap() = Some((status, body));
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SandboxBackend for GithubBackend {
        fn name(&self) -> &'static str {
            "test"
        }
        async fn create(&self, _spec: SandboxSpec) -> SandboxResult<SandboxHandle> {
            unreachable!()
        }
        async fn open_io(&self, _id: &SandboxId) -> SandboxResult<SandboxIo> {
            unreachable!()
        }
        async fn status(&self, _id: &SandboxId) -> SandboxResult<SandboxStatus> {
            unreachable!()
        }
        async fn observe(&self, _id: &SandboxId) -> SandboxResult<ObservedSandbox> {
            unreachable!()
        }
        async fn list_observed(&self) -> SandboxResult<Vec<ObservedSandbox>> {
            unreachable!()
        }
        async fn stop(&self, _id: &SandboxId) -> SandboxResult<()> {
            unreachable!()
        }
        async fn pause(&self, _id: &SandboxId) -> SandboxResult<()> {
            unreachable!()
        }
        async fn resume(&self, _id: &SandboxId) -> SandboxResult<()> {
            unreachable!()
        }
        async fn create_github_pull_request(
            &self,
            id: &SandboxId,
            owner: &str,
            repo: &str,
            placeholder: &PrOauthPlaceholder,
            payload: Value,
        ) -> SandboxResult<ProxyHttpsResponse> {
            self.calls.lock().unwrap().push(Call {
                sandbox_id: id.as_str().to_owned(),
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                placeholder: placeholder.as_str().to_owned(),
                payload,
            });
            let (status, body) = self
                .response
                .lock()
                .unwrap()
                .clone()
                .expect("test must set a GitHub response before the call");
            Ok(ProxyHttpsResponse {
                status,
                body: serde_json::to_vec(&body).unwrap(),
            })
        }
    }

    fn proxy_token(
        subject: &str,
        sandbox_id: Option<&str>,
        github_user_id: Option<&str>,
    ) -> String {
        proxy_token_for(subject, sandbox_id, github_user_id, None)
    }

    /// A proxy token that also attests the requester its proxy is bound to.
    fn proxy_token_for(
        subject: &str,
        sandbox_id: Option<&str>,
        github_user_id: Option<&str>,
        requester: Option<&str>,
    ) -> String {
        let mut claims = json!({
            "iss": "centaur-console",
            "sub": subject,
            "aud": "centaur-api",
            "iat": 1_700_000_000i64,
            "exp": 4_102_444_800i64,
            "capabilities": {
                "sessions_read": false,
                "workflows_read": false,
                "workflows_write": false
            },
        });
        if let Some(sandbox_id) = sandbox_id {
            claims["sandbox_id"] = json!(sandbox_id);
        }
        if let Some(github_user_id) = github_user_id {
            claims["github_user_id"] = json!(github_user_id);
        }
        if let Some(requester) = requester {
            claims["requester_principal_id"] = json!(requester);
        }
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap()
    }

    const GITHUBBOT_KEY: &str = "test-githubbot-key";
    const AUTHOR_ID: i64 = 90_210_001;

    /// A ready api-rs over the test database with a recording GitHub backend.
    struct Harness {
        store: PgSessionStore,
        backend: Arc<GithubBackend>,
        router: axum::Router,
        state: AppState,
        auth: ApiAuthConfig,
        principal: String,
        suffix: String,
        database_url: String,
        _gate: Gate,
    }

    /// A test that kills the PR-create lock's connection must not disturb
    /// another test's pool. It scopes the kill to its own pool by application
    /// name and to this database, and also holds this gate exclusively
    /// (every harness holds it shared), so no other test in the process has a
    /// creation in flight beside it.
    fn application_name(suffix: &str) -> String {
        format!("c10-pr-bindings-{suffix}")
    }

    static GATE: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

    enum Gate {
        Shared(#[allow(dead_code)] tokio::sync::RwLockReadGuard<'static, ()>),
        Exclusive(#[allow(dead_code)] tokio::sync::RwLockWriteGuard<'static, ()>),
    }

    /// The tests' sessions hold running executions in a database other crates'
    /// tests share, and the runtime's adoption scan would pick them up. Delete
    /// them (and their bindings, notices and executions, by cascade) when the
    /// harness goes away, from a fresh connection on a thread of its own.
    impl Drop for Harness {
        fn drop(&mut self) {
            let (url, principal) = (self.database_url.clone(), self.principal.clone());
            let _ = std::thread::spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(async {
                    if let Ok(store) = PgSessionStore::connect(&url).await {
                        let _ =
                            sqlx::query("delete from sessions where iron_control_principal = $1")
                                .bind(&principal)
                                .execute(store.pool())
                                .await;
                    }
                });
            })
            .join();
        }
    }

    impl Harness {
        async fn new() -> Option<Self> {
            Self::with_gate(false).await
        }

        /// A harness no other harness runs beside, for a test that disturbs
        /// connections the others share or that parks creations holding the
        /// PR-create lock (a queued exclusive taker in another test would
        /// otherwise hold up the shared takers behind it).
        async fn exclusive() -> Option<Self> {
            Self::with_gate(true).await
        }

        async fn with_gate(exclusive: bool) -> Option<Self> {
            let gate = if exclusive {
                Gate::Exclusive(GATE.write().await)
            } else {
                Gate::Shared(GATE.read().await)
            };
            let Ok(url) = std::env::var("SESSION_RUNTIME_TEST_DATABASE_URL") else {
                eprintln!("skipping: SESSION_RUNTIME_TEST_DATABASE_URL not set");
                return None;
            };
            let suffix = Uuid::new_v4().simple().to_string();
            // This harness's pool names itself, so a test that kills
            // connections can scope the kill to this pool alone.
            let separator = if url.contains('?') { '&' } else { '?' };
            let store = PgSessionStore::connect(&format!(
                "{url}{separator}application_name={}",
                application_name(&suffix)
            ))
            .await
            .unwrap();
            store
                .run_migrations(centaur_session_sqlx::TextSearchBackend::Postgres)
                .await
                .unwrap();
            let backend = Arc::new(GithubBackend::default());
            let runtime = SessionRuntime::new(
                store.clone(),
                SandboxRuntime::backend(backend.clone(), SandboxSpec::new("test")),
                Registrar,
            );
            let auth = ApiAuthConfig::testing_with_ingresses("test-secret");
            // Trusted creation is off in a real deployment unless enabled; the
            // harness enables it so the create routes can be exercised.
            let state =
                AppState::ready_with_pool(runtime, None, Some(store.pool().clone()), auth.clone())
                    .with_trusted_pr_create(true);
            let router = build_router_with_app_state(state.clone());
            Some(Self {
                store,
                backend,
                router,
                state,
                auth,
                principal: format!("prn_{suffix}"),
                suffix,
                database_url: url,
                _gate: gate,
            })
        }

        /// A Slack session owned by this harness's principal, assigned
        /// `sandbox_id`, with a running execution api-rs recorded as having
        /// no requester.
        async fn session(&self, channel: &str, sandbox_id: &str) -> ThreadKey {
            self.session_for(channel, sandbox_id, Some(None)).await
        }

        /// Like [`Self::session`], but the running execution's recorded
        /// requester is `requester`: `None` records nothing yet, `Some(None)`
        /// no requester, `Some(Some(id))` that requester.
        async fn session_for(
            &self,
            channel: &str,
            sandbox_id: &str,
            requester: Option<Option<&str>>,
        ) -> ThreadKey {
            let thread_key = ThreadKey::parse(format!(
                "slack:T0TEST:{channel}:{}.000100",
                Uuid::new_v4().simple()
            ))
            .unwrap();
            self.store
                .create_or_get_session(
                    &thread_key,
                    &HarnessType::Codex,
                    None,
                    json!({}),
                    Default::default(),
                )
                .await
                .unwrap();
            self.store
                .set_iron_control_principal(&thread_key, Some(&self.principal))
                .await
                .unwrap();
            self.store
                .update_sandbox_id(&thread_key, Some(sandbox_id))
                .await
                .unwrap();
            let execution = self
                .store
                .create_execution(&thread_key, None, json!({}))
                .await
                .unwrap()
                .execution;
            if let Some(requester) = requester {
                self.store
                    .set_execution_requester(&execution.execution_id, requester)
                    .await
                    .unwrap();
            }
            self.store
                .mark_execution_running(&execution.execution_id)
                .await
                .unwrap();
            thread_key
        }

        /// The node id GitHub reports for this harness's PR number 7.
        fn node_id(&self) -> String {
            format!("PR_{}_7", self.suffix)
        }

        /// A ci-failures report for this harness's PR.
        fn ci(
            &self,
            head_sha: &str,
            current_head_sha: &str,
            open: bool,
            assigned: bool,
        ) -> CiFailureRequest {
            CiFailureRequest {
                pr_node_id: self.node_id(),
                observed_updated_at: observed_at(0),
                head_sha: head_sha.to_owned(),
                current_head_sha: current_head_sha.to_owned(),
                open,
                assigned,
                check_name: "unit tests".to_owned(),
                details_url: Some("https://github.com/acme/widgets/actions/runs/1".to_owned()),
                html_url: format!("https://github.com/{}/pull/7", self.repository()),
                current_repository: None,
                current_number: None,
            }
        }

        fn sandbox_id(&self, label: &str) -> String {
            format!("sbx-{label}-{}", self.suffix)
        }

        fn repository(&self) -> String {
            format!("acme/widgets-{}", self.suffix)
        }

        /// What GitHub answers for a PR created in `repository` as `user`.
        fn github_pull_request(
            &self,
            number: i64,
            user_id: i64,
            kind: &str,
            repository: &str,
        ) -> Value {
            json!({
                "id": 1_000_000 + number,
                "node_id": format!("PR_{}_{number}", self.suffix),
                "number": number,
                "html_url": format!("https://github.com/{repository}/pull/{number}"),
                "user": {"id": user_id, "login": "ada", "type": kind},
                "head": {"ref": "fix/widgets", "sha": "abcdef1234567890"},
                "base": {"ref": "main", "repo": {"full_name": repository}},
            })
        }

        async fn create(
            &self,
            thread_key: &ThreadKey,
            token: &str,
            body: Value,
        ) -> (StatusCode, Value) {
            self.send(
                Method::POST,
                &format!(
                    "/api/session/{}/pull-requests",
                    urlencoding::encode(thread_key.as_str())
                ),
                token,
                Some(body),
            )
            .await
        }

        async fn send(
            &self,
            method: Method,
            uri: &str,
            token: &str,
            body: Option<Value>,
        ) -> (StatusCode, Value) {
            let mut request = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"));
            let body = match body {
                Some(body) => {
                    request = request.header(header::CONTENT_TYPE, "application/json");
                    Body::from(body.to_string())
                }
                None => Body::empty(),
            };
            let response = self
                .router
                .clone()
                .oneshot(request.body(body).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            )
        }

        fn create_body(&self) -> Value {
            json!({
                "repository": self.repository(),
                "head": "fix/widgets",
                "base": "main",
                "title": "Fix the widgets",
                "body": "Details",
                "draft": true,
            })
        }

        fn token(&self, sandbox_id: &str) -> String {
            proxy_token(
                &self.principal,
                Some(sandbox_id),
                Some(&AUTHOR_ID.to_string()),
            )
        }
    }

    // ---- trusted creation: access ----------------------------------------

    #[tokio::test]
    async fn creation_succeeds_and_binds_the_session_that_asked() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("a");
        let thread_key = h.session("C0TEST", &sandbox).await;
        h.backend.respond(
            201,
            h.github_pull_request(7, AUTHOR_ID, "User", &h.repository()),
        );

        let (status, body) = h
            .create(&thread_key, &h.token(&sandbox), h.create_body())
            .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["bound"], true);
        assert_eq!(body["pull_request"]["number"], 7);
        assert_eq!(
            body["pull_request"]["html_url"],
            format!("https://github.com/{}/pull/7", h.repository())
        );
        assert!(body["pull_request"]["node_id"].is_string());
        // GitHub sees exactly the fixed request, for the sandbox's own proxy.
        assert_eq!(
            h.backend.calls(),
            vec![Call {
                sandbox_id: sandbox.clone(),
                owner: "acme".to_owned(),
                repo: format!("widgets-{}", h.suffix),
                // Bound to the attested account; this turn has no requester.
                placeholder: "CENTAUR_PR_OAUTH_U90210001_NONE".to_owned(),
                payload: json!({
                    "title": "Fix the widgets",
                    "head": "fix/widgets",
                    "base": "main",
                    "body": "Details",
                    "draft": true,
                }),
            }]
        );
        let binding = h
            .store
            .pr_binding(&h.repository(), 7)
            .await
            .unwrap()
            .expect("binding stored");
        assert_eq!(binding.thread_key, thread_key);
        assert_eq!(binding.sandbox_id, sandbox);
        assert_eq!(binding.author_github_id, AUTHOR_ID);
        assert_eq!(binding.author_login, "ada");
        assert_eq!(binding.head_ref, "fix/widgets");
        assert_eq!(binding.base_ref, "main");
        assert_eq!(binding.head_sha, "abcdef1234567890");
        assert!(!binding.managed);
        assert!(!binding.closed);
    }

    #[tokio::test]
    async fn a_caller_without_or_with_the_wrong_sandbox_cannot_create_or_bind() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("a");
        let thread_key = h.session("C0TEST", &sandbox).await;
        h.backend.respond(
            201,
            h.github_pull_request(7, AUTHOR_ID, "User", &h.repository()),
        );
        let author = AUTHOR_ID.to_string();

        // No sandbox claim at all (an older token).
        let (status, body) = h
            .create(
                &thread_key,
                &proxy_token(&h.principal, None, Some(&author)),
                h.create_body(),
            )
            .await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::FORBIDDEN, Some("sandbox_claim_required"))
        );

        // Another sandbox's proxy, even for the same principal.
        let other = h.sandbox_id("other");
        let (status, body) = h
            .create(
                &thread_key,
                &proxy_token(&h.principal, Some(&other), Some(&author)),
                h.create_body(),
            )
            .await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::FORBIDDEN, Some("sandbox_mismatch"))
        );

        // Two sessions claim one sandbox: the assignment is ambiguous, so even
        // the right sandbox id is refused for both.
        let twin = h.session("C0TEST", &sandbox).await;
        for key in [&thread_key, &twin] {
            let (status, body) = h.create(key, &h.token(&sandbox), h.create_body()).await;
            assert_eq!(
                (status, body["code"].as_str()),
                (StatusCode::FORBIDDEN, Some("sandbox_mismatch"))
            );
        }

        assert!(h.backend.calls().is_empty());
        assert_eq!(h.store.pr_binding(&h.repository(), 7).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_principal_that_does_not_own_the_session_is_refused() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("a");
        let thread_key = h.session("C0TEST", &sandbox).await;
        let stranger = proxy_token(
            "prn_someone_else",
            Some(&sandbox),
            Some(&AUTHOR_ID.to_string()),
        );

        let (status, _) = h.create(&thread_key, &stranger, h.create_body()).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(h.backend.calls().is_empty());
    }

    #[tokio::test]
    async fn a_missing_github_account_is_a_conflict_with_a_stable_code() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("a");
        let thread_key = h.session("C0TEST", &sandbox).await;

        for token in [
            proxy_token(&h.principal, Some(&sandbox), None),
            proxy_token(&h.principal, Some(&sandbox), Some("  ")),
        ] {
            let (status, body) = h.create(&thread_key, &token, h.create_body()).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["code"], "github_oauth_required");
        }
        assert!(h.backend.calls().is_empty());
        assert_eq!(h.store.pr_binding(&h.repository(), 7).await.unwrap(), None);
    }

    #[tokio::test]
    async fn only_active_non_dm_slack_sessions_can_create() {
        let Some(h) = Harness::new().await else {
            return;
        };
        // A direct-message session.
        let sandbox = h.sandbox_id("dm");
        let dm = h.session("D0TEST", &sandbox).await;
        let (status, body) = h.create(&dm, &h.token(&sandbox), h.create_body()).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::FORBIDDEN, Some("dm_not_supported"))
        );

        // A non-Slack session.
        let sandbox = h.sandbox_id("api");
        let api = ThreadKey::parse(format!("api:{}", h.suffix)).unwrap();
        h.store
            .create_or_get_session(
                &api,
                &HarnessType::Codex,
                None,
                json!({}),
                Default::default(),
            )
            .await
            .unwrap();
        h.store
            .set_iron_control_principal(&api, Some(&h.principal))
            .await
            .unwrap();
        h.store
            .update_sandbox_id(&api, Some(&sandbox))
            .await
            .unwrap();
        h.store
            .create_execution(&api, None, json!({}))
            .await
            .unwrap();
        let (status, body) = h.create(&api, &h.token(&sandbox), h.create_body()).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::FORBIDDEN, Some("slack_session_required"))
        );

        // A Slack session whose execution is over.
        let sandbox = h.sandbox_id("idle");
        let idle = h.session("C0TEST", &sandbox).await;
        let execution = h
            .store
            .active_execution_for_thread(&idle)
            .await
            .unwrap()
            .unwrap();
        h.store
            .complete_execution(&execution.execution_id)
            .await
            .unwrap();
        let (status, body) = h.create(&idle, &h.token(&sandbox), h.create_body()).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::CONFLICT, Some("no_active_execution"))
        );

        assert!(h.backend.calls().is_empty());
    }

    #[tokio::test]
    async fn a_rebind_waits_for_an_in_flight_creation_and_a_refusal_releases_the_lock() {
        use std::time::Duration;

        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("lock");
        let thread_key = h.session("C0TEST", &sandbox).await;
        h.backend.respond(
            201,
            h.github_pull_request(7, AUTHOR_ID, "User", &h.repository()),
        );
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", h.token(&sandbox)).parse().unwrap(),
        );
        let caller = h.auth.authenticate(&headers).unwrap();
        let request: CreatePullRequestRequest = serde_json::from_value(h.create_body()).unwrap();

        // Creation pauses just before the GitHub call, holding the lock.
        let (paused_tx, paused_rx) = tokio::sync::oneshot::channel::<()>();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel::<()>();
        let creation = tokio::spawn({
            let state = h.state.clone();
            let raw = thread_key.as_str().to_owned();
            async move {
                create_session_pull_request_with(&state, &caller, raw, request, || async move {
                    let _ = paused_tx.send(());
                    let _ = resume_rx.await;
                })
                .await
            }
        });
        paused_rx.await.expect("creation reached the GitHub call");
        assert!(h.backend.calls().is_empty(), "paused before the call");

        // A rebind or unbind of this thread's proxy takes the same lock, so it
        // waits while the creation is in flight.
        let waiting = tokio::time::timeout(
            Duration::from_millis(400),
            h.store.lock_proxy_binding(&thread_key),
        )
        .await;
        assert!(waiting.is_err(), "the rebind did not wait for the creation");

        // Another thread's rebind is unaffected.
        let other = ThreadKey::parse(format!("slack:T0TEST:C0OTHER:{}.1", h.suffix)).unwrap();
        tokio::time::timeout(Duration::from_secs(5), h.store.lock_proxy_binding(&other))
            .await
            .expect("another thread's lock is free")
            .unwrap();

        // Creation finishes (and binds), then the rebind gets the lock.
        resume_tx.send(()).unwrap();
        let body = creation.await.unwrap().unwrap();
        assert_eq!(body["bound"], true);
        assert_eq!(h.backend.calls().len(), 1);
        tokio::time::timeout(
            Duration::from_secs(5),
            h.store.lock_proxy_binding(&thread_key),
        )
        .await
        .expect("the lock is released once creation finishes")
        .unwrap();

        // A refused request holds the lock only while it is checked.
        let (status, _) = h
            .create(
                &thread_key,
                &proxy_token(&h.principal, Some("sbx-wrong"), Some("1")),
                h.create_body(),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        tokio::time::timeout(
            Duration::from_secs(5),
            h.store.lock_proxy_binding(&thread_key),
        )
        .await
        .expect("a refusal releases the lock")
        .unwrap();
    }

    #[tokio::test]
    async fn an_authoritative_miss_waits_for_an_in_flight_creation_in_any_repository() {
        use std::time::Duration;

        let Some(h) = Harness::exclusive().await else {
            return;
        };
        let sandbox = h.sandbox_id("lookup");
        let thread_key = h.session("C0TEST", &sandbox).await;
        h.backend.respond(
            201,
            h.github_pull_request(7, AUTHOR_ID, "User", &h.repository()),
        );
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", h.token(&sandbox)).parse().unwrap(),
        );
        let caller = h.auth.authenticate(&headers).unwrap();
        let request: CreatePullRequestRequest = serde_json::from_value(h.create_body()).unwrap();

        // Creation pauses just before its GitHub call, holding the repository's
        // PR-create lock.
        let (paused_tx, paused_rx) = tokio::sync::oneshot::channel::<()>();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel::<()>();
        let creation = tokio::spawn({
            let state = h.state.clone();
            let raw = thread_key.as_str().to_owned();
            async move {
                create_session_pull_request_with(&state, &caller, raw, request, || async move {
                    let _ = paused_tx.send(());
                    let _ = resume_rx.await;
                })
                .await
            }
        });
        paused_rx.await.expect("creation reached the GitHub call");

        // An authoritative lookup that misses waits for the creation, whatever
        // repository the path names: GitHub may have renamed it before the
        // creation commits. A plain lookup does not lock and answers at once.
        let get = |uri: String, router: axum::Router| async move {
            let response = router
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(header::AUTHORIZATION, format!("Bearer {GITHUBBOT_KEY}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        };
        let same = format!("/api/pr-bindings/{}/7?authoritative=true", h.repository());
        let other = format!(
            "/api/pr-bindings/acme/renamed-{}/7?authoritative=true",
            h.suffix
        );
        let mut same_lookup = tokio::spawn(get(same, h.router.clone()));
        let mut other_lookup = tokio::spawn(get(other, h.router.clone()));
        for (label, lookup) in [("same", &mut same_lookup), ("other", &mut other_lookup)] {
            assert!(
                tokio::time::timeout(Duration::from_millis(400), lookup)
                    .await
                    .is_err(),
                "the {label}-repository authoritative miss answered while a creation was in flight"
            );
        }
        let plain = format!("/api/pr-bindings/acme/other-{}/7", h.suffix);
        let (status, body) = tokio::time::timeout(
            Duration::from_secs(3),
            h.send(Method::GET, &plain, GITHUBBOT_KEY, None),
        )
        .await
        .expect("a plain lookup waited");
        assert_binding_miss(status, &body);

        // The creation commits its binding; the waiting lookups then read.
        resume_tx.send(()).unwrap();
        assert_eq!(creation.await.unwrap().unwrap()["bound"], true);
        let (status, body) = tokio::time::timeout(Duration::from_secs(5), same_lookup)
            .await
            .expect("the lookup finishes once the creation does")
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["thread_key"], thread_key.as_str());
        assert_eq!(body["number"], 7);
        // The renamed path alone still finds nothing (a lookup by node id
        // would), but only after the wait.
        let (status, body) = tokio::time::timeout(Duration::from_secs(5), other_lookup)
            .await
            .expect("the other lookup finishes once the creation does")
            .unwrap();
        assert_binding_miss(status, &body);
    }

    #[tokio::test]
    async fn a_stalled_kubernetes_preflight_is_reported_as_the_proxy_being_unavailable() {
        // What the k8s backend returns when its Kubernetes preflight times out.
        let error = sandbox_failure(SessionRuntimeError::Sandbox(SandboxError::NotReady(
            "the Kubernetes API did not answer in time".to_owned(),
        )));
        let response = axum::response::IntoResponse::into_response(error);
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "sandbox_proxy_unavailable");
        // Nothing of the internal cause reaches the caller.
        assert_eq!(
            body["error"],
            "could not reach GitHub through the sandbox proxy"
        );
        assert!(!body.to_string().contains("Kubernetes"));
    }

    #[tokio::test]
    async fn a_lookup_by_node_id_never_returns_another_prs_older_row_at_the_same_path() {
        use std::time::Duration;

        let Some(h) = Harness::exclusive().await else {
            return;
        };
        // An older PR (node A) is bound at the path the lookup will name.
        // `(repository, number)` is the table's primary key, so a second PR
        // (node B) cannot be bound at those very coordinates: B's creation is
        // in a repository GitHub has since renamed, which is also the case
        // where a caller still holds the old path. The older row is seeded
        // through the store.
        let old_sandbox = h.sandbox_id("old");
        let old_thread = h.session("C0TEST", &old_sandbox).await;
        let node_a = format!("PR_{}_A", h.suffix);
        h.store
            .insert_pr_binding(&NewPrBinding {
                repository: h.repository(),
                number: 7,
                thread_key: old_thread,
                pr_id: 1,
                pr_node_id: node_a.clone(),
                author_github_id: AUTHOR_ID,
                author_login: "ada".to_owned(),
                sandbox_id: old_sandbox,
                head_ref: "old".to_owned(),
                base_ref: "main".to_owned(),
                head_sha: "abcdef1".to_owned(),
            })
            .await
            .unwrap();

        // B's creation pauses before its GitHub call, holding the shared lock.
        let renamed = format!("acme/renamed-{}", h.suffix);
        let sandbox = h.sandbox_id("new");
        let thread_key = h.session("C0TEST", &sandbox).await;
        h.backend
            .respond(201, h.github_pull_request(7, AUTHOR_ID, "User", &renamed));
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", h.token(&sandbox)).parse().unwrap(),
        );
        let caller = h.auth.authenticate(&headers).unwrap();
        let mut body = h.create_body();
        body["repository"] = json!(renamed);
        let request: CreatePullRequestRequest = serde_json::from_value(body).unwrap();
        let (paused_tx, paused_rx) = tokio::sync::oneshot::channel::<()>();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel::<()>();
        let creation = tokio::spawn({
            let state = h.state.clone();
            let raw = thread_key.as_str().to_owned();
            async move {
                create_session_pull_request_with(&state, &caller, raw, request, || async move {
                    let _ = paused_tx.send(());
                    let _ = resume_rx.await;
                })
                .await
            }
        });
        paused_rx.await.expect("creation reached the GitHub call");

        // The lookup names B's node id on the old path. It must not answer
        // with A's row while B's creation is in flight.
        let uri = format!(
            "/api/pr-bindings/{}/7?node_id={}&authoritative=true",
            h.repository(),
            h.node_id()
        );
        let router = h.router.clone();
        let mut lookup = tokio::spawn(async move {
            let response = router
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(header::AUTHORIZATION, format!("Bearer {GITHUBBOT_KEY}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(400), &mut lookup)
                .await
                .is_err(),
            "the lookup answered while B's creation was in flight"
        );

        resume_tx.send(()).unwrap();
        assert_eq!(creation.await.unwrap().unwrap()["bound"], true);
        let (status, body) = tokio::time::timeout(Duration::from_secs(5), lookup)
            .await
            .expect("the lookup finishes once the creation does")
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["pr_node_id"], h.node_id(), "{body}");
        assert_ne!(body["pr_node_id"], node_a.as_str());
        assert_eq!(body["repository"], renamed);
        assert_eq!(body["thread_key"], thread_key.as_str());
    }

    #[tokio::test]
    async fn creations_never_wait_on_each_other() {
        use std::time::Duration;

        let Some(h) = Harness::exclusive().await else {
            return;
        };
        // Two creations (two sessions, two repositories) both parked at the
        // GitHub call at once, each holding the PR-create lock shared.
        let arrived = Arc::new(tokio::sync::Barrier::new(2));
        let mut creations = Vec::new();
        for label in ["first", "second"] {
            let sandbox = h.sandbox_id(label);
            let thread_key = h.session("C0TEST", &sandbox).await;
            h.backend.respond(
                201,
                h.github_pull_request(7, AUTHOR_ID, "User", &h.repository()),
            );
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {}", h.token(&sandbox)).parse().unwrap(),
            );
            let caller = h.auth.authenticate(&headers).unwrap();
            let mut body = h.create_body();
            body["repository"] = json!(format!("{}-{label}", h.repository()));
            let request: CreatePullRequestRequest = serde_json::from_value(body).unwrap();
            let arrived = arrived.clone();
            creations.push(tokio::spawn({
                let state = h.state.clone();
                let raw = thread_key.as_str().to_owned();
                async move {
                    create_session_pull_request_with(&state, &caller, raw, request, || async move {
                        // Returns only once the other creation is here too.
                        arrived.wait().await;
                    })
                    .await
                }
            }));
        }
        for creation in creations {
            tokio::time::timeout(Duration::from_secs(10), creation)
                .await
                .expect("one creation waited on the other")
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_refused_creation_releases_the_repository_lock() {
        use std::time::Duration;

        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("refused");
        let thread_key = h.session("C0TEST", &sandbox).await;
        // GitHub refuses after the lock is taken.
        h.backend
            .respond(422, json!({"message": "Validation Failed"}));
        let (status, _) = h
            .create(&thread_key, &h.token(&sandbox), h.create_body())
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        tokio::time::timeout(Duration::from_secs(5), h.store.lock_pr_create_exclusive())
            .await
            .expect("the refusal left the repository lock held")
            .unwrap();
        // And a lookup miss answers at once.
        let uri = format!("/api/pr-bindings/{}/7", h.repository());
        let (status, body) = tokio::time::timeout(
            Duration::from_secs(5),
            h.send(Method::GET, &uri, GITHUBBOT_KEY, None),
        )
        .await
        .expect("the miss waited on a lock a refusal should have released");
        assert_binding_miss(status, &body);
    }

    #[tokio::test]
    async fn a_binding_is_found_by_node_id_after_a_repository_rename() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, bound) = bound_pr(&h, true).await;

        // GitHub now calls the repository something else, and the PR a
        // different number in the caller's mind. By path alone: no binding.
        let renamed = format!("/api/pr-bindings/acme/renamed-{}/7", h.suffix);
        let (status, body) = h.send(Method::GET, &renamed, GITHUBBOT_KEY, None).await;
        assert_binding_miss(status, &body);

        // By node id: found, and the response carries the stored repository
        // and number for the caller's later calls.
        let (status, body) = h
            .send(
                Method::GET,
                &format!("{renamed}?node_id={}", h.node_id()),
                GITHUBBOT_KEY,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["repository"], repository);
        assert_eq!(body["number"], 7);
        assert_eq!(body["pr_node_id"], bound.pr_node_id);
        assert_eq!(body["thread_key"], bound.thread_key.as_str());

        // An unknown node id on an unknown path is still a (waited-for) miss.
        let (status, body) = h
            .send(
                Method::GET,
                &format!("{renamed}?node_id=PR_unknown"),
                GITHUBBOT_KEY,
                None,
            )
            .await;
        assert_binding_miss(status, &body);

        // An unknown node id on the right path does not fall back to the path:
        // the row there is another PR's, never this one's.
        let (status, body) = h
            .send(
                Method::GET,
                &format!("/api/pr-bindings/{repository}/7?node_id=PR_unknown"),
                GITHUBBOT_KEY,
                None,
            )
            .await;
        assert_binding_miss(status, &body);
        // With no node id the path alone still finds it.
        let (status, body) = h
            .send(
                Method::GET,
                &format!("/api/pr-bindings/{repository}/7"),
                GITHUBBOT_KEY,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["pr_node_id"], bound.pr_node_id);
    }

    #[tokio::test]
    async fn a_lost_lock_transaction_before_the_insert_leaves_nothing_bound() {
        let Some(h) = Harness::exclusive().await else {
            return;
        };
        let sandbox = h.sandbox_id("lost-lock");
        let thread_key = h.session("C0TEST", &sandbox).await;
        h.backend.respond(
            201,
            h.github_pull_request(7, AUTHOR_ID, "User", &h.repository()),
        );
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", h.token(&sandbox)).parse().unwrap(),
        );
        let caller = h.auth.authenticate(&headers).unwrap();
        let request: CreatePullRequestRequest = serde_json::from_value(h.create_body()).unwrap();

        // The connection holding the global PR-create lock dies after the GitHub
        // call is about to be made and before the binding is inserted.
        let body = create_session_pull_request_with(
            &h.state,
            &caller,
            thread_key.as_str().to_owned(),
            request,
            || async {
                // Only a backend that holds the (global) lock, in this
                // database, and belongs to this harness's own pool.
                let killed: i64 = sqlx::query_scalar(
                    "select count(*) from (
                         select pg_terminate_backend(l.pid)
                         from pg_locks l
                         join pg_stat_activity a on a.pid = l.pid
                         where l.locktype = 'advisory' and l.granted
                           and l.database = (select oid from pg_database
                                             where datname = current_database())
                           and a.datname = current_database()
                           and a.application_name = $1
                           and ((l.classid::bigint << 32) | l.objid::bigint)
                               = hashtextextended('centaur:pr-create', 0)
                     ) killed",
                )
                .bind(application_name(&h.suffix))
                .fetch_one(h.store.pool())
                .await
                .unwrap();
                assert_eq!(killed, 1, "exactly the lock's connection is killed");
            },
        )
        .await
        .unwrap();

        // The PR exists on GitHub and is reported, unbound, with a reason.
        assert_eq!(body["pull_request"]["number"], 7);
        assert_eq!(body["bound"], false, "{body}");
        assert_eq!(body["reason"], "binding_unavailable");
        assert_eq!(h.store.pr_binding(&h.repository(), 7).await.unwrap(), None);
        assert_eq!(h.backend.calls().len(), 1);
        // And the lock is gone with the connection.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            h.store.lock_pr_create_exclusive(),
        )
        .await
        .expect("the dead lock was never released")
        .unwrap();
    }

    #[tokio::test]
    async fn the_binding_is_committed_with_the_lock_and_never_before() {
        use std::time::Duration;

        let Some(h) = Harness::exclusive().await else {
            return;
        };
        // The binding is inserted inside the shared PR-create lock's transaction: invisible to
        // other connections until the commit, then visible with the lock free.
        let thread_key = h.session("C0TEST", &h.sandbox_id("commit")).await;
        let mut lock = h.store.lock_pr_create().await.unwrap();
        lock.insert_binding(&NewPrBinding {
            repository: h.repository(),
            number: 7,
            thread_key: thread_key.clone(),
            pr_id: 1,
            pr_node_id: h.node_id(),
            author_github_id: AUTHOR_ID,
            author_login: "ada".to_owned(),
            sandbox_id: "sbx".to_owned(),
            head_ref: "fix".to_owned(),
            base_ref: "main".to_owned(),
            head_sha: "abcdef1".to_owned(),
        })
        .await
        .unwrap();
        assert_eq!(h.store.pr_binding(&h.repository(), 7).await.unwrap(), None);
        // A conflicting second insert fails without breaking the transaction.
        let conflict = lock
            .insert_binding(&NewPrBinding {
                repository: h.repository(),
                number: 7,
                thread_key: thread_key.clone(),
                pr_id: 2,
                pr_node_id: "PR_other".to_owned(),
                author_github_id: AUTHOR_ID,
                author_login: "ada".to_owned(),
                sandbox_id: "sbx".to_owned(),
                head_ref: "fix".to_owned(),
                base_ref: "main".to_owned(),
                head_sha: "abcdef1".to_owned(),
            })
            .await;
        assert!(matches!(
            conflict,
            Err(SessionStoreError::PrBindingConflict { .. })
        ));
        lock.commit().await.unwrap();
        let bound = h
            .store
            .pr_binding(&h.repository(), 7)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bound.pr_node_id, h.node_id());
        tokio::time::timeout(Duration::from_secs(5), h.store.lock_pr_create_exclusive())
            .await
            .expect("commit releases the lock")
            .unwrap();

        // A rolled-back lock binds nothing.
        let mut lock = h.store.lock_pr_create().await.unwrap();
        lock.insert_binding(&NewPrBinding {
            repository: "acme/rolled-back".to_owned(),
            number: 9,
            thread_key,
            pr_id: 3,
            pr_node_id: format!("PR_rb_{}", h.suffix),
            author_github_id: AUTHOR_ID,
            author_login: "ada".to_owned(),
            sandbox_id: "sbx".to_owned(),
            head_ref: "fix".to_owned(),
            base_ref: "main".to_owned(),
            head_sha: "abcdef1".to_owned(),
        })
        .await
        .unwrap();
        lock.release().await.unwrap();
        assert_eq!(
            h.store.pr_binding("acme/rolled-back", 9).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_caller_that_does_not_own_the_session_never_waits_on_the_lock() {
        use std::time::Duration;

        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("owner");
        let thread_key = h.session("C0TEST", &sandbox).await;
        // Someone holds the thread's lock, as an in-flight rebind would.
        let held = h.store.lock_proxy_binding(&thread_key).await.unwrap();

        // A non-owner, and a request for a session that does not exist, are
        // refused at once instead of queueing behind the lock.
        let stranger = proxy_token("prn_someone_else", Some(&sandbox), Some("1"));
        let missing = ThreadKey::parse(format!("slack:T0TEST:C0TEST:{}.9", h.suffix)).unwrap();
        for (thread, token, expected) in [
            (&thread_key, stranger.as_str(), StatusCode::FORBIDDEN),
            (&missing, h.token(&sandbox).as_str(), StatusCode::NOT_FOUND),
        ] {
            let (status, _) = tokio::time::timeout(
                Duration::from_secs(3),
                h.create(thread, token, h.create_body()),
            )
            .await
            .expect("the refusal did not wait for the lock");
            assert_eq!(status, expected);
        }

        // The owner does wait for it: the lock is what the pre-check avoids.
        let owner = tokio::time::timeout(
            Duration::from_millis(400),
            h.create(&thread_key, &h.token(&sandbox), h.create_body()),
        )
        .await;
        assert!(owner.is_err(), "the owner's request ignored the lock");
        assert!(h.backend.calls().is_empty());
        drop(held);
    }

    #[tokio::test]
    async fn a_rejected_placeholder_leaves_nothing_bound() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("unsubstituted");
        let thread_key = h.session("C0TEST", &sandbox).await;
        // The credential stopped being the attested OAuth one, so the proxy
        // forwarded `CENTAUR_PR_OAUTH` as is and GitHub refused it.
        h.backend
            .respond(401, json!({"message": "Bad credentials"}));

        let (status, body) = h
            .create(&thread_key, &h.token(&sandbox), h.create_body())
            .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["code"], "github_error");
        assert_eq!(body["error"], "Bad credentials");
        assert_eq!(h.backend.calls().len(), 1);
        for number in 1..=10 {
            assert_eq!(
                h.store.pr_binding(&h.repository(), number).await.unwrap(),
                None
            );
        }
    }

    #[tokio::test]
    async fn a_tie_converges_over_http_when_the_revision_read_still_matches() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let base = format!("/api/pr-bindings/{repository}/7");
        let write = |closed: bool, expected: Option<i64>| {
            let mut body = json!({
                "pr_node_id": h.node_id(),
                // GitHub's timestamp does not move across any of these.
                "observed_updated_at": "2026-11-20T10:00:10Z",
                "closed": closed,
            });
            if let Some(expected) = expected {
                body["expected_revision"] = json!(expected);
            }
            let (uri, h) = (format!("{base}/state"), &h);
            async move { h.send(Method::POST, &uri, GITHUBBOT_KEY, Some(body)).await }
        };
        let revision = || async {
            let (status, body) = h.send(Method::GET, &base, GITHUBBOT_KEY, None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["revision"].as_i64().unwrap()
        };

        let before = revision().await;
        let (status, body) = write(true, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["revision"], before + 1);

        // A contradicting write without the revision is refused; reading the
        // binding and retrying with its revision converges.
        let (status, body) = write(false, None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "binding_state_conflict");
        let read = revision().await;
        assert_eq!(read, before + 1);
        let (status, body) = write(false, Some(read)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["closed"], false);
        assert_eq!(body["revision"], read + 1);

        // The same retry again is now stale.
        let (status, body) = write(true, Some(read)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "binding_state_conflict");
    }

    #[tokio::test]
    async fn contradictory_state_writes_at_one_observation_are_a_conflict_over_http() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let uri = format!("/api/pr-bindings/{repository}/7/state");
        let write = |closed: bool, observed: &str| {
            let body = json!({
                "pr_node_id": h.node_id(),
                "observed_updated_at": observed,
                "closed": closed,
            });
            let (uri, h) = (uri.clone(), &h);
            async move { h.send(Method::POST, &uri, GITHUBBOT_KEY, Some(body)).await }
        };

        let (status, _) = write(true, "2026-11-20T10:00:10Z").await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = write(false, "2026-11-20T10:00:10Z").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "binding_state_conflict");
        assert!(
            h.store
                .pr_binding(&repository, 7)
                .await
                .unwrap()
                .unwrap()
                .closed
        );
        // The same value again, and a newer observation, go through.
        assert_eq!(write(true, "2026-11-20T10:00:10Z").await.0, StatusCode::OK);
        let (status, body) = write(false, "2026-11-20T10:00:11Z").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["closed"], false);
    }

    #[tokio::test]
    async fn the_proxys_requester_must_be_the_one_api_rs_bound_for_the_running_turn() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let author = AUTHOR_ID.to_string();
        let token = |sandbox: &str, requester: Option<&str>| {
            proxy_token_for(&h.principal, Some(sandbox), Some(&author), requester)
        };
        h.backend.respond(
            201,
            h.github_pull_request(7, AUTHOR_ID, "User", &h.repository()),
        );

        // Alice's turn: api-rs bound Alice and the proxy attests Alice.
        let sandbox = h.sandbox_id("alice");
        let alice = h
            .session_for("C0TEST", &sandbox, Some(Some("prn_alice")))
            .await;
        let (status, body) = h
            .create(&alice, &token(&sandbox, Some("prn_alice")), h.create_body())
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["bound"], true);
        let calls = h.backend.calls().len();
        assert_eq!(calls, 1);

        // Bob's turn: api-rs bound Bob, but the proxy still acts for the
        // previous speaker. The refusal is before any GitHub call.
        let sandbox = h.sandbox_id("bob");
        let bob = h
            .session_for("C0TEST", &sandbox, Some(Some("prn_bob")))
            .await;
        for (label, requester) in [
            ("previous speaker's claim", Some("prn_alice")),
            ("missing claim while a requester is bound", None),
        ] {
            let (status, body) = h
                .create(&bob, &token(&sandbox, requester), h.create_body())
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{label}: {body}");
            assert_eq!(body["code"], "requester_not_applied", "{label}");
        }
        // Once the proxy has caught up, the same request goes through.
        let (status, _) = h
            .create(&bob, &token(&sandbox, Some("prn_bob")), h.create_body())
            .await;
        assert_eq!(status, StatusCode::OK);
        let calls = h.backend.calls();
        assert_eq!(calls.len(), 2);
        // Each call carried a placeholder derived from its own account and
        // requester, so a proxy configured for one cannot honour the other.
        let derive = |requester: &str| {
            PrOauthPlaceholder::derive(&AUTHOR_ID.to_string(), Some(requester))
                .unwrap()
                .as_str()
                .to_owned()
        };
        assert_eq!(calls[0].placeholder, derive("prn_alice"));
        assert_eq!(calls[1].placeholder, derive("prn_bob"));
        assert_ne!(calls[0].placeholder, calls[1].placeholder);
        assert!(
            calls
                .iter()
                .all(|call| !call.placeholder.contains("GITHUB_TOKEN"))
        );
    }

    #[tokio::test]
    async fn a_requester_claim_is_refused_when_api_rs_bound_none_or_nothing_is_recorded() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let author = AUTHOR_ID.to_string();
        // api-rs ran this turn without a requester, yet the proxy names one.
        let sandbox = h.sandbox_id("none");
        let thread_key = h.session("C0TEST", &sandbox).await;
        let (status, body) = h
            .create(
                &thread_key,
                &proxy_token_for(
                    &h.principal,
                    Some(&sandbox),
                    Some(&author),
                    Some("prn_alice"),
                ),
                h.create_body(),
            )
            .await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::CONFLICT, Some("requester_not_applied"))
        );

        // The turn is running but api-rs has not recorded a requester yet, so
        // even a token with no requester cannot be shown to match.
        let sandbox = h.sandbox_id("unrecorded");
        let thread_key = h.session_for("C0TEST", &sandbox, None).await;
        let (status, body) = h
            .create(&thread_key, &h.token(&sandbox), h.create_body())
            .await;
        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::CONFLICT, Some("requester_not_applied"))
        );
        assert!(h.backend.calls().is_empty());
    }

    #[tokio::test]
    async fn a_queued_execution_is_not_enough() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("queued");
        let thread_key = h.session("C0TEST", &sandbox).await;
        // A session whose only execution has not started.
        sqlx::query("update session_executions set status = 'queued' where thread_key = $1")
            .bind(thread_key.as_str())
            .execute(h.store.pool())
            .await
            .unwrap();
        h.backend.respond(
            201,
            h.github_pull_request(7, AUTHOR_ID, "User", &h.repository()),
        );

        let (status, body) = h
            .create(&thread_key, &h.token(&sandbox), h.create_body())
            .await;

        assert_eq!(
            (status, body["code"].as_str()),
            (StatusCode::CONFLICT, Some("no_active_execution"))
        );
        assert!(h.backend.calls().is_empty());
    }

    #[tokio::test]
    async fn malformed_requests_are_rejected_before_the_sandbox_is_called() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("a");
        let thread_key = h.session("C0TEST", &sandbox).await;
        for patch in [
            json!({"repository": "acme"}),
            json!({"repository": "acme/../user"}),
            json!({"repository": "acme/widgets/extra"}),
            json!({"repository": "acme/widgets?x=1"}),
            json!({"title": "  "}),
            json!({"head": ""}),
            json!({"base": "main branch"}),
            json!({"title": "x".repeat(257)}),
            json!({"body": "x".repeat(MAX_BODY_CHARS + 1)}),
        ] {
            let mut body = h.create_body();
            for (key, value) in patch.as_object().unwrap() {
                body[key] = value.clone();
            }
            let (status, _) = h.create(&thread_key, &h.token(&sandbox), body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{patch}");
        }
        let mut body = h.create_body();
        body["requester"] = json!("someone");
        let (status, _) = h.create(&thread_key, &h.token(&sandbox), body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(h.backend.calls().is_empty());
    }

    // ---- trusted creation: what GitHub says -------------------------------

    #[tokio::test]
    async fn a_pr_that_fails_verification_is_reported_unbound_and_writes_no_row() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("a");
        let thread_key = h.session("C0TEST", &sandbox).await;
        let repository = h.repository();
        for (response, reason) in [
            // Authored by a different GitHub user than the proxy attests.
            (
                h.github_pull_request(7, AUTHOR_ID + 1, "User", &repository),
                "author_mismatch",
            ),
            // Authored by a bot or app, whatever its id.
            (
                h.github_pull_request(7, AUTHOR_ID, "Bot", &repository),
                "not_a_user",
            ),
            // Created in a different repository than requested.
            (
                h.github_pull_request(7, AUTHOR_ID, "User", "acme/elsewhere"),
                "repository_mismatch",
            ),
        ] {
            h.backend.respond(201, response);
            let (status, body) = h
                .create(&thread_key, &h.token(&sandbox), h.create_body())
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["bound"], false, "{reason}");
            assert_eq!(body["reason"], reason);
            // The PR is still reported so the agent can use it.
            assert_eq!(body["pull_request"]["number"], 7);
            assert_eq!(h.store.pr_binding(&repository, 7).await.unwrap(), None);
            assert_eq!(h.store.pr_binding("acme/elsewhere", 7).await.unwrap(), None);
        }
    }

    #[tokio::test]
    async fn the_repository_check_ignores_case_but_nothing_else() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("a");
        let thread_key = h.session("C0TEST", &sandbox).await;
        let mut body = h.create_body();
        body["repository"] = json!(h.repository().to_ascii_uppercase());
        h.backend.respond(
            201,
            h.github_pull_request(
                7,
                AUTHOR_ID,
                "User",
                &h.repository().replace("acme", "Acme"),
            ),
        );

        let (status, body) = h.create(&thread_key, &h.token(&sandbox), body).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["bound"], true);
        assert!(
            h.store
                .pr_binding(&h.repository(), 7)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_binding_is_never_rebound_to_another_session() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let first_sandbox = h.sandbox_id("first");
        let second_sandbox = h.sandbox_id("second");
        let first = h.session("C0TEST", &first_sandbox).await;
        let second = h.session("C0TEST", &second_sandbox).await;
        h.backend.respond(
            201,
            h.github_pull_request(7, AUTHOR_ID, "User", &h.repository()),
        );
        let (_, body) = h
            .create(&first, &h.token(&first_sandbox), h.create_body())
            .await;
        assert_eq!(body["bound"], true);

        // The same PR shows up in a second session's create response.
        let (status, body) = h
            .create(&second, &h.token(&second_sandbox), h.create_body())
            .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["bound"], false);
        assert_eq!(body["reason"], "binding_conflict");
        let binding = h
            .store
            .pr_binding(&h.repository(), 7)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.thread_key, first);
        assert_eq!(binding.sandbox_id, first_sandbox);
    }

    #[tokio::test]
    async fn github_errors_surface_only_githubs_message() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("a");
        let thread_key = h.session("C0TEST", &sandbox).await;
        h.backend.respond(
            422,
            json!({
                "message": "Validation Failed",
                "errors": [{"resource": "PullRequest", "code": "custom", "message": "No commits between main and fix"}],
                "documentation_url": "https://docs.github.com/rest",
            }),
        );

        let (status, body) = h
            .create(&thread_key, &h.token(&sandbox), h.create_body())
            .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["code"], "github_error");
        assert_eq!(body["error"], "Validation Failed");
        assert!(!body.to_string().contains("No commits"));
        assert_eq!(h.store.pr_binding(&h.repository(), 7).await.unwrap(), None);

        // A 2xx that is not 201, or an unparseable error, never binds.
        h.backend.respond(200, json!({"message": "OK"}));
        let (status, body) = h
            .create(&thread_key, &h.token(&sandbox), h.create_body())
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["code"], "github_error");
        h.backend.respond(201, json!({"not": "a pull request"}));
        let (status, body) = h
            .create(&thread_key, &h.token(&sandbox), h.create_body())
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["code"], "github_response_invalid");
    }

    #[test]
    fn github_messages_are_trimmed_to_text() {
        let error = github_failure(401, br#"{"message":"Bad\u0000 credentials"}"#);
        assert_eq!(error.to_string(), "Bad credentials");
        let error = github_failure(502, b"<html>Bad gateway</html>");
        assert_eq!(
            error.to_string(),
            "GitHub did not create the pull request (HTTP 502)"
        );
        let long = json!({"message": "x".repeat(1000)}).to_string();
        assert_eq!(
            github_failure(500, long.as_bytes())
                .to_string()
                .chars()
                .count(),
            MAX_GITHUB_MESSAGE_CHARS
        );
    }

    // ---- githubbot-only routes ---------------------------------------------

    fn caller_tokens(h: &Harness) -> Vec<(&'static str, String)> {
        let console = encode(
            &Header::new(Algorithm::HS256),
            &json!({
                "iss": "centaur-console", "sub": "centaur-console", "aud": "centaur-api",
                "iat": 1_700_000_000i64, "exp": 4_102_444_800i64, "token_use": "console_service",
            }),
            &EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap();
        vec![
            ("slackbot", "test-slackbot-key".to_owned()),
            ("discordbot", "test-discordbot-key".to_owned()),
            ("linearbot", "test-linearbot-key".to_owned()),
            ("teamsbot", "test-teamsbot-key".to_owned()),
            ("console", console),
            ("principal", h.token("sbx-any")),
        ]
    }

    #[tokio::test]
    async fn the_githubbot_routes_reject_every_other_caller_class() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("a");
        let thread_key = h.session("C0TEST", &sandbox).await;
        h.backend.respond(
            201,
            h.github_pull_request(7, AUTHOR_ID, "User", &h.repository()),
        );
        let (_, body) = h
            .create(&thread_key, &h.token(&sandbox), h.create_body())
            .await;
        assert_eq!(body["bound"], true);
        let base = format!("/api/pr-bindings/acme/widgets-{}/7", h.suffix);
        let ci = json!({
            "pr_node_id": h.node_id(),
            "head_sha": "abcdef1234567890", "current_head_sha": "abcdef1234567890",
            "open": true, "assigned": true, "check_name": "unit tests",
            "html_url": "https://github.com/acme/widgets/pull/7",
        });

        for (caller, token) in caller_tokens(&h) {
            for (method, uri, body) in [
                (Method::GET, base.clone(), None),
                (
                    Method::POST,
                    format!("{base}/state"),
                    Some(
                        json!({"pr_node_id": h.node_id(), "observed_updated_at": "2026-11-20T10:00:10Z", "managed": true}),
                    ),
                ),
                (
                    Method::POST,
                    format!("{base}/ci-failures"),
                    Some(ci.clone()),
                ),
            ] {
                let (status, _) = h.send(method.clone(), &uri, &token, body).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{caller} {method} {uri}");
            }
        }
        // None of those attempts changed the binding.
        let binding = h
            .store
            .pr_binding(&h.repository(), 7)
            .await
            .unwrap()
            .unwrap();
        assert!(!binding.managed);

        // The githubbot ingress itself is admitted.
        let (status, body) = h.send(Method::GET, &base, "test-githubbot-key", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["thread_key"], thread_key.as_str());
    }

    #[tokio::test]
    async fn only_the_session_creation_route_is_open_to_sandbox_principals() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("a");
        let thread_key = h.session("C0TEST", &sandbox).await;
        let uri = format!(
            "/api/session/{}/pull-requests",
            urlencoding::encode(thread_key.as_str())
        );
        for (caller, token) in caller_tokens(&h) {
            if caller == "principal" {
                continue;
            }
            let (status, _) = h
                .send(Method::POST, &uri, &token, Some(h.create_body()))
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{caller}");
        }
        let (status, _) = h
            .send(Method::POST, &uri, GITHUBBOT_KEY, Some(h.create_body()))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(h.backend.calls().is_empty());
    }

    #[tokio::test]
    async fn state_updates_apply_to_existing_bindings_and_never_create_or_rebind() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let sandbox = h.sandbox_id("a");
        let thread_key = h.session("C0TEST", &sandbox).await;
        h.backend.respond(
            201,
            h.github_pull_request(7, AUTHOR_ID, "User", &h.repository()),
        );
        h.create(&thread_key, &h.token(&sandbox), h.create_body())
            .await;
        let base = format!("/api/pr-bindings/{}/7", h.repository());

        let (status, body) = h
            .send(
                Method::POST,
                &format!("{base}/state"),
                GITHUBBOT_KEY,
                Some(json!({"pr_node_id": h.node_id(), "observed_updated_at": "2026-11-20T10:00:10Z", "managed": true, "head_sha": "fedcba9876"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            (body["managed"].clone(), body["head_sha"].clone()),
            (json!(true), json!("fedcba9876"))
        );

        // Unknown fields (a thread key, say) are refused outright.
        let (status, _) = h
            .send(
                Method::POST,
                &format!("{base}/state"),
                GITHUBBOT_KEY,
                Some(json!({"pr_node_id": h.node_id(), "observed_updated_at": "2026-11-20T10:00:10Z", "thread_key": "slack:T0TEST:C0TEST:1.1"})),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        // Unbound PRs are not created by an update, and read as 404.
        let unbound = format!("/api/pr-bindings/{}/99", h.repository());
        let (status, _) = h
            .send(
                Method::POST,
                &format!("{unbound}/state"),
                GITHUBBOT_KEY,
                Some(json!({"pr_node_id": h.node_id(), "observed_updated_at": "2026-11-20T10:00:10Z", "managed": true})),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = h.send(Method::GET, &unbound, GITHUBBOT_KEY, None).await;
        assert_binding_miss(status, &body);
        let binding = h
            .store
            .pr_binding(&h.repository(), 7)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.thread_key, thread_key);
    }

    // ---- ci-failures ---------------------------------------------------------

    /// A fixed observation time, `seconds` after a base.
    fn observed_at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + seconds).unwrap()
    }

    /// A Slack double: records every post, and fails the next `fail` of them.
    #[derive(Default, Clone)]
    struct Slack {
        posts: Arc<Mutex<Vec<Value>>>,
        fail: Arc<Mutex<usize>>,
    }

    impl Slack {
        async fn post(&self, message: Value) -> Result<Value, WorkflowRuntimeError> {
            let mut fail = self.fail.lock().unwrap();
            if *fail > 0 {
                *fail -= 1;
                return Err(WorkflowRuntimeError::Upstream("Slack is down".to_owned()));
            }
            self.posts.lock().unwrap().push(message);
            Ok(json!({"ok": true}))
        }

        fn fail_next(&self, count: usize) {
            *self.fail.lock().unwrap() = count;
        }

        fn posts(&self) -> Vec<Value> {
            self.posts.lock().unwrap().clone()
        }
    }

    /// The final answer of a completed binding lookup that found nothing: a
    /// 404 carrying the stable code githubbot keys on.
    fn assert_binding_miss(status: StatusCode, body: &Value) {
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["code"], "pr_binding_not_found", "{body}");
        assert_eq!(body["ok"], false, "{body}");
    }

    #[tokio::test]
    async fn trusted_creation_is_off_by_default_and_answers_unsupported() {
        use std::time::Duration;

        // Exclusive, so this test may hold the PR-create lock without holding
        // up another test's create.
        let Some(h) = Harness::exclusive().await else {
            return;
        };
        // The deployment default: the setting off, on the same runtime.
        assert!(!AppState::unready(h.auth.clone()).trusted_pr_create_enabled());
        let off = build_router_with_app_state(h.state.clone().with_trusted_pr_create(false));
        let sandbox = h.sandbox_id("off");
        let thread_key = h.session("C0TEST", &sandbox).await;
        h.backend.respond(
            201,
            h.github_pull_request(7, AUTHOR_ID, "User", &h.repository()),
        );

        // Both locks a create would take are held elsewhere, so a create that
        // reached either would wait instead of answering.
        let _thread_lock = h.store.lock_proxy_binding(&thread_key).await.unwrap();
        let _create_lock = h.store.lock_pr_create_exclusive().await.unwrap();

        let request = Request::builder()
            .method(Method::POST)
            .uri(format!(
                "/api/session/{}/pull-requests",
                urlencoding::encode(thread_key.as_str())
            ))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", h.token(&sandbox)),
            )
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(h.create_body().to_string()))
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(3), off.clone().oneshot(request))
            .await
            .expect("a disabled create waited on a lock")
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "pull_requests_unsupported", "{body}");
        assert_eq!(
            body["error"],
            "trusted pull request creation is not enabled in this deployment"
        );

        // Nothing happened: no sandbox call, no binding.
        assert!(h.backend.calls().is_empty());
        assert_eq!(h.store.pr_binding(&h.repository(), 7).await.unwrap(), None);

        // The binding routes are unaffected: a lookup still answers, as a
        // coded miss.
        let response = off
            .oneshot(
                Request::builder()
                    .uri(format!("/api/pr-bindings/{}/7", h.repository()))
                    .header(header::AUTHORIZATION, format!("Bearer {GITHUBBOT_KEY}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_binding_miss(status, &serde_json::from_slice(&body).unwrap());
    }

    #[tokio::test]
    async fn only_a_completed_lookup_miss_carries_the_binding_not_found_code() {
        let Some(h) = Harness::new().await else {
            return;
        };
        // A miss on the binding route is coded, with its message.
        let (status, body) = h
            .send(
                Method::GET,
                &format!("/api/pr-bindings/{}/7", h.repository()),
                GITHUBBOT_KEY,
                None,
            )
            .await;
        assert_binding_miss(status, &body);
        assert_eq!(
            body["error"],
            format!("no PR binding for {}#7", h.repository())
        );
        // A route this api-rs does not have (what an older replica answers for
        // the binding route itself during a rollout) is a bare 404 without it,
        // so a caller can tell the two apart.
        for uri in [
            "/api/pr-bindings/acme/widgets/7/unknown",
            "/api/pr-bindings/acme/widgets",
            "/api/pr-bindings-unknown/acme/widgets/7",
        ] {
            let (status, body) = h.send(Method::GET, uri, GITHUBBOT_KEY, None).await;
            assert_ne!(body["code"], "pr_binding_not_found", "{uri}: {body}");
            assert!(
                status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN,
                "{uri}: {status}"
            );
        }
        // A malformed path keeps its own shape too.
        let (status, body) = h
            .send(
                Method::GET,
                "/api/pr-bindings/acme/widgets/notanumber",
                GITHUBBOT_KEY,
                None,
            )
            .await;
        assert!(status.is_client_error(), "{status}");
        assert_ne!(body["code"], "pr_binding_not_found", "{body}");
    }

    /// A store with one bound PR at head `abcdef1` in a Slack channel,
    /// managed as asked.
    async fn bound_pr_in(h: &Harness, channel: &str, managed: bool) -> (String, PrBinding) {
        let sandbox = h.sandbox_id("ci");
        let thread_key = h.session(channel, &sandbox).await;
        let repository = h.repository();
        h.store
            .insert_pr_binding(&NewPrBinding {
                repository: repository.clone(),
                number: 7,
                thread_key,
                pr_id: 1,
                pr_node_id: h.node_id(),
                author_github_id: AUTHOR_ID,
                author_login: "ada".to_owned(),
                sandbox_id: sandbox,
                head_ref: "fix".to_owned(),
                base_ref: "main".to_owned(),
                head_sha: "abcdef1".to_owned(),
            })
            .await
            .unwrap();
        let binding = h
            .store
            .update_pr_binding_state(
                &repository,
                7,
                &h.node_id(),
                &PrBindingStateUpdate {
                    managed: Some(managed),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap()
            .binding;
        (repository, binding)
    }

    async fn bound_pr(h: &Harness, managed: bool) -> (String, PrBinding) {
        bound_pr_in(h, "C0TEST", managed).await
    }

    async fn report(
        h: &Harness,
        repository: &str,
        request: &CiFailureRequest,
        slack: &Slack,
    ) -> Result<Value, ApiError> {
        process_ci_failure(&h.store, repository, 7, request, |message| {
            slack.post(message)
        })
        .await
    }

    async fn set_state(h: &Harness, repository: &str, update: PrBindingStateUpdate) {
        h.store
            .update_pr_binding_state(repository, 7, &h.node_id(), &update, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_notice_after_a_rename_links_the_current_url_and_keeps_the_stored_identity() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (stored, binding) = bound_pr(&h, true).await;
        let slack = Slack::default();
        let new_name = format!("acme/new-name-{}", h.suffix);
        let request = CiFailureRequest {
            current_repository: Some(new_name.to_ascii_uppercase()),
            current_number: Some(7),
            html_url: format!("https://github.com/{new_name}/pull/7"),
            ..h.ci("abcdef1", "abcdef1", true, true)
        };

        // The route is called at the binding's stored coordinates; the report
        // carries the PR's current ones.
        let outcome = report(&h, &stored, &request, &slack).await.unwrap();

        assert_eq!(outcome, json!({"notice": "posted"}));
        let posts = slack.posts();
        assert_eq!(posts.len(), 1);
        let text = posts[0]["text"].as_str().unwrap();
        assert!(
            text.starts_with(&format!(
                "CI failed on <https://github.com/{new_name}/pull/7|{new_name}#7> at `abcdef1`"
            )),
            "{text}"
        );
        assert!(!text.contains(&stored), "{text}");
        // Dedupe and state still follow the node id: a repeat is a duplicate.
        assert_eq!(
            report(&h, &stored, &request, &slack).await.unwrap(),
            json!({"notice": "duplicate"})
        );
        assert_eq!(
            h.store
                .pr_binding(&stored, 7)
                .await
                .unwrap()
                .unwrap()
                .repository,
            binding.repository
        );

        // A URL for the old name no longer matches the current coordinates.
        let stale = CiFailureRequest {
            html_url: format!("https://github.com/{stored}/pull/7"),
            ..request
        };
        assert_eq!(
            report(&h, &stored, &stale, &slack).await.unwrap(),
            json!({"notice": "skipped", "reason": "invalid_html_url"})
        );
        assert_eq!(slack.posts().len(), 1);
        set_state(
            &h,
            &stored,
            PrBindingStateUpdate {
                head_sha: Some("fedcba9".to_owned()),
                ..Default::default()
            },
        )
        .await;

        // Without the current coordinates (older githubbot) the stored ones
        // are used.
        let legacy = h.ci("fedcba9", "fedcba9", true, true);
        assert_eq!(
            report(&h, &stored, &legacy, &slack).await.unwrap(),
            json!({"notice": "posted"})
        );
        assert!(
            slack.posts()[1]["text"]
                .as_str()
                .unwrap()
                .contains(&format!("https://github.com/{stored}/pull/7"))
        );

        // Only one of the pair, or nonsense, is refused.
        for (repository, number) in [
            (Some("acme/x".to_owned()), None),
            (None, Some(7)),
            (Some("acme".to_owned()), Some(7)),
            (Some("acme/x".to_owned()), Some(0)),
        ] {
            let bad = CiFailureRequest {
                current_repository: repository,
                current_number: number,
                ..h.ci("fedcba9", "fedcba9", true, true)
            };
            let error = report(&h, &stored, &bad, &slack).await.unwrap_err();
            assert!(matches!(error, ApiError::BadRequest(_)), "{error:?}");
        }
    }

    #[tokio::test]
    async fn a_failure_at_the_current_head_of_a_managed_open_pr_posts_once_to_its_thread() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, binding) = bound_pr(&h, true).await;
        let slack = Slack::default();

        let outcome = report(
            &h,
            &repository,
            &h.ci("abcdef1", "abcdef1", true, true),
            &slack,
        )
        .await
        .unwrap();

        assert_eq!(outcome, json!({"notice": "posted"}));
        let posts = slack.posts();
        assert_eq!(posts.len(), 1, "{posts:?}");
        let ChatDestination::Slack {
            channel_id,
            thread_ts,
        } = binding.thread_key.chat_destination().unwrap()
        else {
            unreachable!()
        };
        // The destination is the bound thread's, and nothing else.
        assert_eq!(posts[0]["channel"], channel_id);
        assert_eq!(posts[0]["thread_ts"], thread_ts);
        assert_eq!(posts[0]["unfurl_links"], false);
        assert_eq!(posts[0]["unfurl_media"], false);
        assert_eq!(
            posts[0]["text"],
            format!(
                "CI failed on <https://github.com/{repository}/pull/7|{repository}#7> at `abcdef1`: <https://github.com/acme/widgets/actions/runs/1|unit tests>\n@-mention me in this thread if you'd like me to fix it."
            )
        );
    }

    #[tokio::test]
    async fn a_change_committed_between_the_read_and_the_claim_posts_nothing() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let slack = Slack::default();
        let request = h.ci("abcdef1", "abcdef1", true, true);

        // Each of these commits after the route read an eligible binding and
        // before it claims. The claim is one conditional insert, so it finds
        // the row changed and nothing is posted.
        for (label, change, restore, reason) in [
            (
                "closed",
                PrBindingStateUpdate {
                    closed: Some(true),
                    ..Default::default()
                },
                PrBindingStateUpdate {
                    closed: Some(false),
                    ..Default::default()
                },
                "closed",
            ),
            (
                "unassigned",
                PrBindingStateUpdate {
                    managed: Some(false),
                    ..Default::default()
                },
                PrBindingStateUpdate {
                    managed: Some(true),
                    ..Default::default()
                },
                "not_managed",
            ),
            (
                "new head",
                PrBindingStateUpdate {
                    head_sha: Some("fedcba9".to_owned()),
                    ..Default::default()
                },
                PrBindingStateUpdate {
                    head_sha: Some("abcdef1".to_owned()),
                    ..Default::default()
                },
                "stale_head",
            ),
        ] {
            let outcome = process_ci_failure_with(
                &h.store,
                &repository,
                7,
                &request,
                || async { set_state(&h, &repository, change).await },
                |message| slack.post(message),
            )
            .await
            .unwrap();
            assert_eq!(
                outcome,
                json!({"notice": "skipped", "reason": reason}),
                "{label}"
            );
            assert!(slack.posts().is_empty(), "{label}: {:?}", slack.posts());
            set_state(&h, &repository, restore).await;
        }

        // None of those left a claim behind: with the binding restored, the
        // same report posts.
        assert_eq!(
            report(&h, &repository, &request, &slack).await.unwrap(),
            json!({"notice": "posted"})
        );
        assert_eq!(slack.posts().len(), 1);
    }

    #[tokio::test]
    async fn a_repeat_for_the_same_pr_and_head_is_a_duplicate_and_posts_nothing() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let slack = Slack::default();
        let request = h.ci("abcdef1", "abcdef1", true, true);

        let first = report(&h, &repository, &request, &slack).await.unwrap();
        let second = report(&h, &repository, &request, &slack).await.unwrap();

        assert_eq!(first, json!({"notice": "posted"}));
        assert_eq!(second, json!({"notice": "duplicate"}));
        assert_eq!(slack.posts().len(), 1);

        // A new head, once githubbot's state update records it, is a new notice.
        set_state(
            &h,
            &repository,
            PrBindingStateUpdate {
                head_sha: Some("fedcba9".to_owned()),
                ..Default::default()
            },
        )
        .await;
        let newer = h.ci("fedcba9", "fedcba9", true, true);
        assert_eq!(
            report(&h, &repository, &newer, &slack).await.unwrap(),
            json!({"notice": "posted"})
        );
        assert_eq!(slack.posts().len(), 2);
    }

    #[tokio::test]
    async fn a_failed_post_releases_the_claim_so_a_retry_posts() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let slack = Slack::default();
        let request = h.ci("abcdef1", "abcdef1", true, true);
        slack.fail_next(1);

        let error = report(&h, &repository, &request, &slack).await.unwrap_err();

        assert!(
            matches!(&error, ApiError::Coded { code: "slack_post_failed", status, .. } if *status == StatusCode::BAD_GATEWAY),
            "{error:?}"
        );
        assert!(slack.posts().is_empty());
        // The failure leaves nothing claimed: the next event posts.
        assert_eq!(
            report(&h, &repository, &request, &slack).await.unwrap(),
            json!({"notice": "posted"})
        );
        assert_eq!(slack.posts().len(), 1);
        assert_eq!(
            report(&h, &repository, &request, &slack).await.unwrap(),
            json!({"notice": "duplicate"})
        );
    }

    #[tokio::test]
    async fn nothing_is_posted_unless_open_assigned_managed_and_current() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let slack = Slack::default();
        let binding = || async { h.store.pr_binding(&repository, 7).await.unwrap().unwrap() };

        // Not open: recorded as closed.
        let outcome = report(
            &h,
            &repository,
            &h.ci("abcdef1", "abcdef1", false, true),
            &slack,
        )
        .await
        .unwrap();
        assert_eq!(outcome, json!({"notice": "skipped", "reason": "not_open"}));
        assert!(binding().await.closed);

        // Closed stays skipped even if githubbot calls again as open (a
        // reopen event has to clear it first).
        let outcome = report(
            &h,
            &repository,
            &h.ci("abcdef1", "abcdef1", true, true),
            &slack,
        )
        .await
        .unwrap();
        assert_eq!(outcome, json!({"notice": "skipped", "reason": "closed"}));

        // Reopened, but no longer assigned: recorded as unmanaged.
        set_state(
            &h,
            &repository,
            PrBindingStateUpdate {
                closed: Some(false),
                ..Default::default()
            },
        )
        .await;
        let outcome = report(
            &h,
            &repository,
            &h.ci("abcdef1", "abcdef1", true, false),
            &slack,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            json!({"notice": "skipped", "reason": "not_assigned"})
        );
        assert!(!binding().await.managed);

        // Assigned in the fresh fetch, but the binding never recorded the
        // bot's explicit assignment.
        let outcome = report(
            &h,
            &repository,
            &h.ci("abcdef1", "abcdef1", true, true),
            &slack,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            json!({"notice": "skipped", "reason": "not_managed"})
        );

        // Managed again, but the failing head is stale, in each way it can be.
        set_state(
            &h,
            &repository,
            PrBindingStateUpdate {
                managed: Some(true),
                ..Default::default()
            },
        )
        .await;
        for request in [
            h.ci("abcdef1", "fedcba9", true, true),
            h.ci("fedcba9", "fedcba9", true, true),
            h.ci("fedcba9", "abcdef1", true, true),
        ] {
            let outcome = report(&h, &repository, &request, &slack).await.unwrap();
            assert_eq!(
                outcome,
                json!({"notice": "skipped", "reason": "stale_head"})
            );
            // A report never moves the binding's head.
            assert_eq!(binding().await.head_sha, "abcdef1");
        }

        assert!(slack.posts().is_empty());
    }

    #[tokio::test]
    async fn recorded_closure_and_unassignment_follow_the_observation_order() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let slack = Slack::default();
        // The binding already reflects an observation at +30.
        h.store
            .update_pr_binding_state(
                &repository,
                7,
                &h.node_id(),
                &PrBindingStateUpdate {
                    closed: Some(false),
                    managed: Some(true),
                    ..Default::default()
                },
                Some(observed_at(30)),
            )
            .await
            .unwrap();

        // A report from an older fetch (+10) saying "closed, unassigned" is
        // skipped but does not change the binding.
        let old = CiFailureRequest {
            observed_updated_at: observed_at(10),
            ..h.ci("abcdef1", "abcdef1", false, false)
        };
        let outcome = report(&h, &repository, &old, &slack).await.unwrap();
        assert_eq!(outcome, json!({"notice": "skipped", "reason": "not_open"}));
        let binding = h.store.pr_binding(&repository, 7).await.unwrap().unwrap();
        assert!(!binding.closed && binding.managed);

        // The same report from a newer fetch is recorded.
        let new = CiFailureRequest {
            observed_updated_at: observed_at(40),
            ..old
        };
        report(&h, &repository, &new, &slack).await.unwrap();
        let binding = h.store.pr_binding(&repository, 7).await.unwrap().unwrap();
        assert!(binding.closed && !binding.managed);
        assert!(slack.posts().is_empty());
    }

    #[tokio::test]
    async fn a_report_for_another_pr_reusing_the_number_changes_nothing() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, before) = bound_pr(&h, true).await;
        let slack = Slack::default();

        // ci-failures: a different PR node id (the number was reused).
        let request = CiFailureRequest {
            pr_node_id: "PR_someone_elses".to_owned(),
            open: false,
            ..h.ci("abcdef1", "abcdef1", true, true)
        };
        let error = report(&h, &repository, &request, &slack).await.unwrap_err();
        assert!(
            matches!(&error, ApiError::Coded { code: "pr_identity_mismatch", status, .. } if *status == StatusCode::CONFLICT),
            "{error:?}"
        );
        assert!(slack.posts().is_empty());
        // Not even the closure it reported was recorded.
        assert_eq!(
            h.store.pr_binding(&repository, 7).await.unwrap().unwrap(),
            before
        );

        // state: same refusal over HTTP, binding untouched.
        let base = format!("/api/pr-bindings/{repository}/7");
        let (status, body) = h
            .send(
                Method::POST,
                &format!("{base}/state"),
                GITHUBBOT_KEY,
                Some(json!({"pr_node_id": "PR_someone_elses", "observed_updated_at": "2026-11-20T10:00:10Z", "managed": false, "closed": true, "head_sha": "ffff"})),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "pr_identity_mismatch");
        assert_eq!(
            h.store.pr_binding(&repository, 7).await.unwrap().unwrap(),
            before
        );

        // The node id and the observation time are required on both routes.
        for (suffix, body) in [
            (
                "state",
                json!({"observed_updated_at": "2026-11-20T10:00:10Z", "managed": false}),
            ),
            (
                "state",
                json!({"pr_node_id": h.node_id(), "managed": false}),
            ),
            (
                "ci-failures",
                json!({
                    "observed_updated_at": "2026-11-20T10:00:10Z",
                    "head_sha": "abcdef1", "current_head_sha": "abcdef1", "open": true,
                    "assigned": true, "check_name": "unit tests",
                    "html_url": "https://github.com/acme/widgets/pull/7",
                }),
            ),
            (
                "ci-failures",
                json!({
                    "pr_node_id": h.node_id(),
                    "head_sha": "abcdef1", "current_head_sha": "abcdef1", "open": true,
                    "assigned": true, "check_name": "unit tests",
                    "html_url": "https://github.com/acme/widgets/pull/7",
                }),
            ),
        ] {
            let (status, _) = h
                .send(
                    Method::POST,
                    &format!("{base}/{suffix}"),
                    GITHUBBOT_KEY,
                    Some(body),
                )
                .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{suffix}");
        }
        assert_eq!(
            h.store.pr_binding(&repository, 7).await.unwrap().unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn reordered_state_writes_over_http_converge_on_the_newest_fetch() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let uri = format!("/api/pr-bindings/{repository}/7/state");
        let write = |closed: bool, observed: &str| {
            let body = json!({
                "pr_node_id": h.node_id(),
                "observed_updated_at": observed,
                "closed": closed,
            });
            let uri = uri.clone();
            let h = &h;
            async move {
                let (status, body) = h.send(Method::POST, &uri, GITHUBBOT_KEY, Some(body)).await;
                assert_eq!(status, StatusCode::OK, "{body}");
                body
            }
        };

        let body = write(true, "2026-11-20T10:00:10Z").await;
        assert_eq!(
            (body["applied"].clone(), body["closed"].clone()),
            (json!(true), json!(true))
        );
        let body = write(false, "2026-11-20T10:00:30Z").await;
        assert_eq!(
            (body["applied"].clone(), body["closed"].clone()),
            (json!(true), json!(false))
        );
        // The earlier close is handled last and must not win.
        let body = write(true, "2026-11-20T10:00:10Z").await;
        assert_eq!(
            (body["applied"].clone(), body["closed"].clone()),
            (json!(false), json!(false))
        );
        assert_eq!(body["closed_observed_at"], "2026-11-20T10:00:30Z");
        let binding = h.store.pr_binding(&repository, 7).await.unwrap().unwrap();
        assert!(!binding.closed);
    }

    #[tokio::test]
    async fn a_pr_rebound_with_a_new_node_id_gets_its_own_notice_and_never_the_old_ones() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, first) = bound_pr(&h, true).await;
        let slack = Slack::default();
        let request = h.ci("abcdef1", "abcdef1", true, true);
        assert_eq!(
            report(&h, &repository, &request, &slack).await.unwrap(),
            json!({"notice": "posted"})
        );

        // The repository's PR number is reused: the old binding is deleted and
        // a new PR with the same repository, number and head is bound to
        // another session under a different node id.
        sqlx::query("delete from pr_session_bindings where repository = $1 and number = 7")
            .bind(&repository)
            .execute(h.store.pool())
            .await
            .unwrap();
        let sandbox = h.sandbox_id("rebound");
        let second_thread = h.session("C0OTHER", &sandbox).await;
        let new_node = format!("PR_rebound_{}", h.suffix);
        h.store
            .insert_pr_binding(&NewPrBinding {
                repository: repository.clone(),
                number: 7,
                thread_key: second_thread.clone(),
                pr_id: 2,
                pr_node_id: new_node.clone(),
                author_github_id: AUTHOR_ID,
                author_login: "ada".to_owned(),
                sandbox_id: sandbox,
                head_ref: "fix".to_owned(),
                base_ref: "main".to_owned(),
                head_sha: "abcdef1".to_owned(),
            })
            .await
            .unwrap();
        h.store
            .update_pr_binding_state(
                &repository,
                7,
                &new_node,
                &PrBindingStateUpdate {
                    managed: Some(true),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();

        // A report still naming the old PR cannot drive the new binding.
        let error = report(&h, &repository, &request, &slack).await.unwrap_err();
        assert!(
            matches!(
                &error,
                ApiError::Coded {
                    code: "pr_identity_mismatch",
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(slack.posts().len(), 1);

        // The new PR's report is not a duplicate of the old one's, and goes to
        // the new binding's thread.
        let new_request = CiFailureRequest {
            pr_node_id: new_node,
            ..request
        };
        assert_eq!(
            report(&h, &repository, &new_request, &slack).await.unwrap(),
            json!({"notice": "posted"})
        );
        let posts = slack.posts();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0]["channel"], "C0TEST");
        assert_eq!(posts[1]["channel"], "C0OTHER");
        assert_ne!(first.thread_key, second_thread);
    }

    #[tokio::test]
    async fn an_unusable_details_url_is_dropped_and_the_notice_is_kept() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let too_long = format!("https://github.com/{}", "a".repeat(MAX_URL_CHARS));
        for (index, details_url) in [
            Some("http://github.com/acme/widgets/actions/runs/1".to_owned()),
            Some("javascript:alert(1)".to_owned()),
            Some("https://x.example/a|b".to_owned()),
            Some(too_long),
            None,
        ]
        .into_iter()
        .enumerate()
        {
            // Each attempt is a new head so each posts.
            let head = format!("abcdef{index}");
            set_state(
                &h,
                &repository,
                PrBindingStateUpdate {
                    head_sha: Some(head.clone()),
                    ..Default::default()
                },
            )
            .await;
            let slack = Slack::default();
            let request = CiFailureRequest {
                details_url,
                ..h.ci(&head, &head, true, true)
            };
            let outcome = report(&h, &repository, &request, &slack).await.unwrap();
            assert_eq!(outcome, json!({"notice": "posted"}));
            let text = slack.posts()[0]["text"].as_str().unwrap().to_owned();
            assert!(
                text.contains(&format!("`{}`: unit tests\n", &head[..7])),
                "{text}"
            );
        }
    }

    #[tokio::test]
    async fn the_notice_is_never_posted_for_a_foreign_link_or_a_dm_thread() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let slack = Slack::default();
        for url in [
            "https://evil.example/acme/widgets/pull/7".to_owned(),
            format!("https://github.com/{repository}/pull/8"),
            format!("https://github.com/{repository}/pull/7/files"),
            format!("https://github.com/{repository}/issues/7"),
        ] {
            let request = CiFailureRequest {
                html_url: url.clone(),
                ..h.ci("abcdef1", "abcdef1", true, true)
            };
            assert_eq!(
                report(&h, &repository, &request, &slack).await.unwrap(),
                json!({"notice": "skipped", "reason": "invalid_html_url"}),
                "{url}"
            );
        }
        assert!(slack.posts().is_empty());
        // Owner and repository compare without regard to case.
        let request = CiFailureRequest {
            html_url: format!(
                "https://github.com/{}/pull/7",
                repository.to_ascii_uppercase()
            ),
            ..h.ci("abcdef1", "abcdef1", true, true)
        };
        assert_eq!(
            report(&h, &repository, &request, &slack).await.unwrap(),
            json!({"notice": "posted"})
        );
    }

    #[tokio::test]
    async fn a_binding_on_a_dm_thread_is_never_posted_into() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr_in(&h, "D0TEST", true).await;
        let slack = Slack::default();

        let outcome = report(
            &h,
            &repository,
            &h.ci("abcdef1", "abcdef1", true, true),
            &slack,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            json!({"notice": "skipped", "reason": "not_slack_thread"})
        );
        assert!(slack.posts().is_empty());
    }

    #[tokio::test]
    async fn an_unbound_pr_and_a_malformed_report_post_nothing() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let slack = Slack::default();
        let error = report(
            &h,
            &format!("acme/unbound-{}", h.suffix),
            &h.ci("abcdef1", "abcdef1", true, true),
            &slack,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::NotFound(_)), "{error:?}");

        let (repository, _) = bound_pr(&h, true).await;
        let malformed = [
            h.ci("", "abcdef1", true, true),
            h.ci("abcdef1", "", true, true),
            h.ci("abc def", "abcdef1", true, true),
            CiFailureRequest {
                check_name: " ".to_owned(),
                ..h.ci("abcdef1", "abcdef1", true, true)
            },
            CiFailureRequest {
                html_url: "http://github.com/acme/widgets/pull/7".to_owned(),
                ..h.ci("abcdef1", "abcdef1", true, true)
            },
        ];
        for request in malformed {
            let error = report(&h, &repository, &request, &slack).await.unwrap_err();
            assert!(matches!(error, ApiError::BadRequest(_)), "{error:?}");
        }
        assert!(slack.posts().is_empty());
    }

    // ---- validation helpers (no database) -------------------------------------

    fn github_pr(user_id: i64, kind: &str, repository: &str) -> GithubPullRequest {
        serde_json::from_value(json!({
            "id": 1, "node_id": "PR_node", "number": 7,
            "html_url": "https://github.com/acme/widgets/pull/7",
            "user": {"id": user_id, "login": "ada", "type": kind},
            "head": {"ref": "fix", "sha": "abc"},
            "base": {"ref": "main", "repo": {"full_name": repository}},
        }))
        .unwrap()
    }

    #[test]
    fn verification_needs_the_right_repository_a_user_and_the_attested_author() {
        let pr = github_pr(90_210_001, "User", "Acme/Widgets");
        assert_eq!(pr.rejection("acme/widgets", "90210001"), None);
        assert_eq!(
            pr.rejection("acme/other", "90210001"),
            Some("repository_mismatch")
        );
        assert_eq!(pr.rejection("acme/widgets", "1"), Some("author_mismatch"));
        // Ids are compared as strings, exactly.
        assert_eq!(
            pr.rejection("acme/widgets", "090210001"),
            Some("author_mismatch")
        );
        assert_eq!(
            github_pr(90_210_001, "Bot", "acme/widgets").rejection("acme/widgets", "90210001"),
            Some("not_a_user")
        );
        assert_eq!(
            github_pr(90_210_001, "Organization", "acme/widgets")
                .rejection("acme/widgets", "90210001"),
            Some("not_a_user")
        );
    }

    #[test]
    fn create_requests_become_the_fixed_github_payload() {
        let request: CreatePullRequestRequest = serde_json::from_value(json!({
            "repository": " Acme/Widgets ",
            "head": "fix/widgets",
            "base": "main",
            "title": "  Fix  ",
        }))
        .unwrap();
        let validated = request.validate().unwrap();
        assert_eq!(validated.repository, "acme/widgets");
        assert_eq!(
            (validated.owner.as_str(), validated.repo.as_str()),
            ("acme", "widgets")
        );
        assert_eq!(
            validated.payload,
            json!({"title": "Fix", "head": "fix/widgets", "base": "main", "body": "", "draft": false})
        );
    }

    // ---- access gate without a database ---------------------------------------

    #[tokio::test]
    async fn the_capability_gate_runs_before_any_handler() {
        let uris = [
            (Method::GET, "/api/pr-bindings/acme/widgets/7"),
            (Method::POST, "/api/pr-bindings/acme/widgets/7/state"),
            (Method::POST, "/api/pr-bindings/acme/widgets/7/ci-failures"),
        ];
        for (method, uri) in uris {
            // githubbot passes the gate and reaches the (not yet ready) handler.
            let router = build_router_with_app_state(AppState::unready(
                ApiAuthConfig::testing_with_ingresses("test-secret"),
            ));
            let response = router
                .oneshot(
                    Request::builder()
                        .method(method.clone())
                        .uri(uri)
                        .header(header::AUTHORIZATION, format!("Bearer {GITHUBBOT_KEY}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(response.status(), StatusCode::FORBIDDEN, "{method} {uri}");
            assert_ne!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri}"
            );

            // Anonymous callers never do.
            let router = build_router_with_app_state(AppState::unready(
                ApiAuthConfig::testing_with_ingresses("test-secret"),
            ));
            let response = router
                .oneshot(
                    Request::builder()
                        .method(method.clone())
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri}"
            );
        }
    }
}
