//! PR session continuity: create a pull request as the authorized human from
//! inside a Slack session, bind it to exactly that session, and route one
//! failed-CI notice back to the session's Slack thread.
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
    extract::{Path, State},
    http::StatusCode,
};
use centaur_sandbox_core::SandboxError;
use centaur_session_core::{ChatDestination, ThreadKey};
use centaur_session_runtime::SessionRuntimeError;
use centaur_session_sqlx::{
    NewPrBinding, PgSessionStore, PrBinding, PrBindingStateUpdate, SessionStoreError,
    normalize_pr_repository,
};
use centaur_workflows::{
    CreateWorkflowRunRequest, CreateWorkflowRunResponse, WorkflowRuntimeError,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    ApiError,
    auth::AuthenticatedCaller,
    routes::{AppState, principal_subject_owns_session},
};

/// The built-in workflow that posts the failed-CI notice.
const PR_CI_NOTICE_WORKFLOW: &str = "pr_ci_notice";
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
/// Slack session that is not a DM and has a queued or running execution, with
/// a GitHub account attested by its proxy.
pub(crate) async fn create_session_pull_request(
    State(state): State<AppState>,
    Extension(caller): Extension<AuthenticatedCaller>,
    Path(raw_thread_key): Path<String>,
    Json(request): Json<CreatePullRequestRequest>,
) -> Result<Json<Value>, ApiError> {
    let thread_key = ThreadKey::try_from(raw_thread_key)?;
    let request = request.validate()?;
    let runtime = state.runtime()?;
    let session = runtime.session(&thread_key).await?;
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
    if runtime
        .store()
        .active_execution_for_thread(&thread_key)
        .await
        .map_err(SessionRuntimeError::from)?
        .is_none()
    {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "no_active_execution",
            "the session has no queued or running execution",
        ));
    }
    let Some(github_user_id) = caller.github_user_id() else {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "github_oauth_required",
            "connect your GitHub account in Console so pull requests are created as you",
        ));
    };

    let response = runtime
        .create_github_pull_request(sandbox_id, &request.owner, &request.repo, request.payload)
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

    let outcome = match pull_request.rejection(&request.repository, github_user_id) {
        Some(reason) => Err(reason),
        None => bind(
            runtime.store(),
            &thread_key,
            sandbox_id,
            &request.repository,
            &pull_request,
        )
        .await
        .map(|_| ()),
    };
    Ok(Json(match outcome {
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
    }))
}

/// Inserts the binding for a PR GitHub has proven authored by the caller.
async fn bind(
    store: &PgSessionStore,
    thread_key: &ThreadKey,
    sandbox_id: &str,
    repository: &str,
    pull_request: &GithubPullRequest,
) -> Result<PrBinding, &'static str> {
    store
        .insert_pr_binding(&NewPrBinding {
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

/// `GET /api/pr-bindings/{owner}/{repo}/{number}`: the binding, or 404.
pub(crate) async fn get_pr_binding(
    State(state): State<AppState>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
) -> Result<Json<PrBinding>, ApiError> {
    let (repository, number) = pr_path(&owner, &repo, number);
    state
        .runtime()?
        .store()
        .pr_binding(&repository, number)
        .await
        .map_err(SessionRuntimeError::from)?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("no PR binding for {repository}#{number}")))
}

/// `POST /api/pr-bindings/{owner}/{repo}/{number}/state`: records `managed`,
/// `closed` and `head_sha` on an existing binding. It cannot create or rebind
/// one.
pub(crate) async fn update_pr_binding_state(
    State(state): State<AppState>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(update): Json<PrBindingStateUpdate>,
) -> Result<Json<PrBinding>, ApiError> {
    let (repository, number) = pr_path(&owner, &repo, number);
    Ok(Json(
        state
            .runtime()?
            .store()
            .update_pr_binding_state(&repository, number, &update)
            .await
            .map_err(SessionRuntimeError::from)?,
    ))
}

/// `POST /api/pr-bindings/{owner}/{repo}/{number}/ci-failures` body: what
/// githubbot saw in a fresh GitHub fetch just before calling.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CiFailureRequest {
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
        let https_url = |url: &str| url.starts_with("https://") && url.len() <= MAX_URL_CHARS;
        if !https_url(&self.html_url) {
            return invalid("html_url must be an https URL");
        }
        if self
            .details_url
            .as_deref()
            .is_some_and(|url| !https_url(url))
        {
            return invalid("details_url must be an https URL");
        }
        Ok(())
    }
}

/// `POST /api/pr-bindings/{owner}/{repo}/{number}/ci-failures`.
pub(crate) async fn report_pr_ci_failure(
    State(state): State<AppState>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(request): Json<CiFailureRequest>,
) -> Result<Json<Value>, ApiError> {
    let (repository, number) = pr_path(&owner, &repo, number);
    let runtime = state.runtime()?;
    let workflows = state.workflows()?;
    process_ci_failure(
        runtime.store(),
        &repository,
        number,
        &request,
        |run| async move { workflows.create_run(run).await },
    )
    .await
    .map(Json)
}

/// Decides whether a failed check still deserves a notice and, if so, spawns
/// the notice workflow once per `(PR, head)`.
///
/// Nothing is spawned unless the PR is open, assigned, managed, not closed,
/// and the failing head is still the PR's head. Each reason that applies is
/// also recorded on the binding first, so a later event for the same head
/// stays dropped. The workflow rechecks the binding when it runs.
async fn process_ci_failure<S, Fut>(
    store: &PgSessionStore,
    repository: &str,
    number: i64,
    request: &CiFailureRequest,
    spawn: S,
) -> Result<Value, ApiError>
where
    S: FnOnce(CreateWorkflowRunRequest) -> Fut,
    Fut: Future<Output = Result<CreateWorkflowRunResponse, WorkflowRuntimeError>>,
{
    request.validate()?;
    let binding = store
        .pr_binding(repository, number)
        .await
        .map_err(SessionRuntimeError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("no PR binding for {repository}#{number}")))?;

    let stale = request.head_sha != request.current_head_sha;
    let reason = if !request.open {
        Some("not_open")
    } else if !request.assigned {
        Some("not_assigned")
    } else if !binding.managed {
        Some("not_managed")
    } else if binding.closed {
        Some("closed")
    } else if stale {
        Some("stale_head")
    } else {
        None
    };

    let mut update = PrBindingStateUpdate::default();
    if !request.open && !binding.closed {
        update.closed = Some(true);
    }
    if !request.assigned && binding.managed {
        update.managed = Some(false);
    }
    if stale && binding.head_sha != request.current_head_sha {
        update.head_sha = Some(request.current_head_sha.clone());
    }
    if let Some(reason) = reason {
        if update != PrBindingStateUpdate::default() {
            store
                .update_pr_binding_state(&binding.repository, binding.number, &update)
                .await
                .map_err(SessionRuntimeError::from)?;
        }
        return Ok(json!({"notice": "skipped", "reason": reason}));
    }

    if binding.head_sha != request.current_head_sha {
        store
            .update_pr_binding_state(
                &binding.repository,
                binding.number,
                &PrBindingStateUpdate {
                    head_sha: Some(request.current_head_sha.clone()),
                    ..Default::default()
                },
            )
            .await
            .map_err(SessionRuntimeError::from)?;
    }
    let run = spawn(CreateWorkflowRunRequest {
        workflow_name: PR_CI_NOTICE_WORKFLOW.to_owned(),
        input: json!({
            "repository": binding.repository,
            "number": binding.number,
            "head_sha": request.head_sha,
            "check_name": request.check_name,
            "details_url": request.details_url,
            "html_url": request.html_url,
        }),
        idempotency_key: Some(format!(
            "pr-ci-notice:{}#{}:{}",
            binding.repository, binding.number, request.head_sha
        )),
        harness_type: None,
        max_attempts: None,
    })
    .await?;
    Ok(json!({"notice": if run.created { "queued" } else { "duplicate" }}))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{Arc, Mutex},
    };

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
            payload: Value,
        ) -> SandboxResult<ProxyHttpsResponse> {
            self.calls.lock().unwrap().push(Call {
                sandbox_id: id.as_str().to_owned(),
                owner: owner.to_owned(),
                repo: repo.to_owned(),
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
        principal: String,
        suffix: String,
    }

    impl Harness {
        async fn new() -> Option<Self> {
            let Ok(url) = std::env::var("SESSION_RUNTIME_TEST_DATABASE_URL") else {
                eprintln!("skipping: SESSION_RUNTIME_TEST_DATABASE_URL not set");
                return None;
            };
            let store = PgSessionStore::connect(&url).await.unwrap();
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
            let router = build_router_with_app_state(AppState::ready_with_pool(
                runtime,
                None,
                Some(store.pool().clone()),
                ApiAuthConfig::testing_with_ingresses("test-secret"),
            ));
            let suffix = Uuid::new_v4().simple().to_string();
            Some(Self {
                store,
                backend,
                router,
                principal: format!("prn_{suffix}"),
                suffix,
            })
        }

        /// A Slack session owned by this harness's principal, assigned
        /// `sandbox_id`, with a running execution.
        async fn session(&self, channel: &str, sandbox_id: &str) -> ThreadKey {
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
            self.store
                .create_execution(&thread_key, None, json!({}))
                .await
                .unwrap();
            thread_key
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
                    Some(json!({"managed": true})),
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
                Some(json!({"managed": true, "head_sha": "fedcba9876"})),
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
                Some(json!({"thread_key": "slack:T0TEST:C0TEST:1.1"})),
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
                Some(json!({"managed": true})),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = h.send(Method::GET, &unbound, GITHUBBOT_KEY, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let binding = h
            .store
            .pr_binding(&h.repository(), 7)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.thread_key, thread_key);
    }

    // ---- ci-failures ---------------------------------------------------------

    fn ci_request(
        head_sha: &str,
        current_head_sha: &str,
        open: bool,
        assigned: bool,
    ) -> CiFailureRequest {
        CiFailureRequest {
            head_sha: head_sha.to_owned(),
            current_head_sha: current_head_sha.to_owned(),
            open,
            assigned,
            check_name: "unit tests".to_owned(),
            details_url: Some("https://github.com/acme/widgets/actions/runs/1".to_owned()),
            html_url: "https://github.com/acme/widgets/pull/7".to_owned(),
        }
    }

    /// Records every spawn and answers `created` once per idempotency key,
    /// as the workflow runtime does.
    #[derive(Default, Clone)]
    struct Spawns {
        runs: Arc<Mutex<Vec<CreateWorkflowRunRequest>>>,
        keys: Arc<Mutex<BTreeSet<String>>>,
    }

    impl Spawns {
        async fn spawn(
            &self,
            run: CreateWorkflowRunRequest,
        ) -> Result<CreateWorkflowRunResponse, WorkflowRuntimeError> {
            let created = self
                .keys
                .lock()
                .unwrap()
                .insert(run.idempotency_key.clone().unwrap_or_default());
            if created {
                self.runs.lock().unwrap().push(run);
            }
            Ok(CreateWorkflowRunResponse {
                ok: true,
                run_id: "run".to_owned(),
                initial_run_id: "run".to_owned(),
                task_id: "task".to_owned(),
                status: "queued".to_owned(),
                created,
            })
        }

        fn runs(&self) -> Vec<CreateWorkflowRunRequest> {
            self.runs.lock().unwrap().clone()
        }
    }

    /// A store with one bound PR at head `abcdef1`, managed as asked.
    async fn bound_pr(h: &Harness, managed: bool) -> (String, PrBinding) {
        let sandbox = h.sandbox_id("ci");
        let thread_key = h.session("C0TEST", &sandbox).await;
        let repository = h.repository();
        h.store
            .insert_pr_binding(&NewPrBinding {
                repository: repository.clone(),
                number: 7,
                thread_key,
                pr_id: 1,
                pr_node_id: format!("PR_{}", Uuid::new_v4().simple()),
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
                &PrBindingStateUpdate {
                    managed: Some(managed),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        (repository, binding)
    }

    async fn report(
        h: &Harness,
        repository: &str,
        request: &CiFailureRequest,
        spawns: &Spawns,
    ) -> Result<Value, ApiError> {
        process_ci_failure(&h.store, repository, 7, request, |run| spawns.spawn(run)).await
    }

    #[tokio::test]
    async fn a_failure_at_the_current_head_of_a_managed_open_pr_queues_one_notice() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let spawns = Spawns::default();

        let outcome = report(
            &h,
            &repository,
            &ci_request("abcdef1", "abcdef1", true, true),
            &spawns,
        )
        .await
        .unwrap();

        assert_eq!(outcome, json!({"notice": "queued"}));
        let runs = spawns.runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].workflow_name, "pr_ci_notice");
        assert_eq!(
            runs[0].idempotency_key.as_deref(),
            Some(format!("pr-ci-notice:{repository}#7:abcdef1").as_str())
        );
        assert_eq!(
            runs[0].input,
            json!({
                "repository": repository,
                "number": 7,
                "head_sha": "abcdef1",
                "check_name": "unit tests",
                "details_url": "https://github.com/acme/widgets/actions/runs/1",
                "html_url": "https://github.com/acme/widgets/pull/7",
            })
        );
    }

    #[tokio::test]
    async fn the_binding_head_follows_the_current_head_when_a_notice_is_queued() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let spawns = Spawns::default();

        // githubbot missed the synchronize event: the binding still says
        // `abcdef1`, but the fresh fetch says the failing head is current.
        let outcome = report(
            &h,
            &repository,
            &ci_request("fedcba9", "fedcba9", true, true),
            &spawns,
        )
        .await
        .unwrap();

        assert_eq!(outcome, json!({"notice": "queued"}));
        let binding = h.store.pr_binding(&repository, 7).await.unwrap().unwrap();
        assert_eq!(binding.head_sha, "fedcba9");
    }

    #[tokio::test]
    async fn a_duplicate_head_reports_duplicate_and_does_not_spawn_twice() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let spawns = Spawns::default();
        let request = ci_request("abcdef1", "abcdef1", true, true);

        let first = report(&h, &repository, &request, &spawns).await.unwrap();
        let second = report(&h, &repository, &request, &spawns).await.unwrap();

        assert_eq!(first, json!({"notice": "queued"}));
        assert_eq!(second, json!({"notice": "duplicate"}));
        assert_eq!(spawns.runs().len(), 1);

        // A new head is a new notice.
        let newer = ci_request("fedcba9", "fedcba9", true, true);
        assert_eq!(
            report(&h, &repository, &newer, &spawns).await.unwrap(),
            json!({"notice": "queued"})
        );
        assert_eq!(spawns.runs().len(), 2);
    }

    #[tokio::test]
    async fn work_is_skipped_without_a_spawn_unless_open_assigned_managed_and_current() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let (repository, _) = bound_pr(&h, true).await;
        let spawns = Spawns::default();

        // Not open: recorded as closed.
        let outcome = report(
            &h,
            &repository,
            &ci_request("abcdef1", "abcdef1", false, true),
            &spawns,
        )
        .await
        .unwrap();
        assert_eq!(outcome, json!({"notice": "skipped", "reason": "not_open"}));
        assert!(
            h.store
                .pr_binding(&repository, 7)
                .await
                .unwrap()
                .unwrap()
                .closed
        );

        // Closed stays skipped even if githubbot calls again as open (a
        // reopen event has to clear it first).
        let outcome = report(
            &h,
            &repository,
            &ci_request("abcdef1", "abcdef1", true, true),
            &spawns,
        )
        .await
        .unwrap();
        assert_eq!(outcome, json!({"notice": "skipped", "reason": "closed"}));

        // Reopened, but no longer assigned: recorded as unmanaged.
        h.store
            .update_pr_binding_state(
                &repository,
                7,
                &PrBindingStateUpdate {
                    closed: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let outcome = report(
            &h,
            &repository,
            &ci_request("abcdef1", "abcdef1", true, false),
            &spawns,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            json!({"notice": "skipped", "reason": "not_assigned"})
        );
        assert!(
            !h.store
                .pr_binding(&repository, 7)
                .await
                .unwrap()
                .unwrap()
                .managed
        );

        // Assigned in the fresh fetch, but the binding never recorded the
        // bot's explicit assignment.
        let outcome = report(
            &h,
            &repository,
            &ci_request("abcdef1", "abcdef1", true, true),
            &spawns,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            json!({"notice": "skipped", "reason": "not_managed"})
        );

        // Managed again, but the failing head is stale: the newer head is
        // recorded.
        h.store
            .update_pr_binding_state(
                &repository,
                7,
                &PrBindingStateUpdate {
                    managed: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let outcome = report(
            &h,
            &repository,
            &ci_request("abcdef1", "fedcba9", true, true),
            &spawns,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            json!({"notice": "skipped", "reason": "stale_head"})
        );
        assert_eq!(
            h.store
                .pr_binding(&repository, 7)
                .await
                .unwrap()
                .unwrap()
                .head_sha,
            "fedcba9"
        );

        assert!(spawns.runs().is_empty());
    }

    #[tokio::test]
    async fn an_unbound_pr_and_a_malformed_report_spawn_nothing() {
        let Some(h) = Harness::new().await else {
            return;
        };
        let spawns = Spawns::default();
        let error = report(
            &h,
            &format!("acme/unbound-{}", h.suffix),
            &ci_request("abcdef1", "abcdef1", true, true),
            &spawns,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::NotFound(_)), "{error:?}");

        let (repository, _) = bound_pr(&h, true).await;
        let malformed = [
            ci_request("", "abcdef1", true, true),
            ci_request("abcdef1", "", true, true),
            ci_request("abc def", "abcdef1", true, true),
            CiFailureRequest {
                check_name: " ".to_owned(),
                ..ci_request("abcdef1", "abcdef1", true, true)
            },
            CiFailureRequest {
                html_url: "http://github.com/acme/widgets/pull/7".to_owned(),
                ..ci_request("abcdef1", "abcdef1", true, true)
            },
            CiFailureRequest {
                details_url: Some("javascript:alert(1)".to_owned()),
                ..ci_request("abcdef1", "abcdef1", true, true)
            },
        ];
        for request in malformed {
            let error = report(&h, &repository, &request, &spawns)
                .await
                .unwrap_err();
            assert!(matches!(error, ApiError::BadRequest(_)), "{error:?}");
        }
        assert!(spawns.runs().is_empty());
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
