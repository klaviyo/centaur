use async_trait::async_trait;
use std::collections::BTreeMap;

use crate::{
    ObservedSandbox, SandboxError, SandboxHandle, SandboxId, SandboxIo, SandboxResult, SandboxSpec,
    SandboxStatus,
};

/// The only host [`SandboxBackend::create_github_pull_request`] reaches.
pub const GITHUB_API_HOST: &str = "api.github.com";

/// The prefix of every placeholder the PR-create guard understands.
pub const PR_OAUTH_PLACEHOLDER_PREFIX: &str = "CENTAUR_PR_OAUTH";

/// The guard's placeholder when no account is attested. It pairs with a
/// sentinel source that is not a credential, so a call using it can only be
/// rejected by GitHub.
pub const PR_OAUTH_UNATTESTED_PLACEHOLDER: &str = "CENTAUR_PR_OAUTH_UNATTESTED";

/// The bearer placeholder of api-rs's PR-create call, bound to the identity
/// api-rs validated for it: the attested GitHub account and the requester the
/// call acts for.
///
/// The proxy's guard (built by Console's config builder) requires exactly the
/// placeholder derived from the same two values as of the moment it was
/// built. If the proxy is rebound to another requester, or the account behind
/// the token changes, between api-rs's checks and its POST, the proxy's guard
/// expects a different string and refuses the call, so the request never
/// reaches GitHub under someone else's token.
///
/// Both ends call [`Self::derive`], so the encoding lives in one place. The
/// result contains only `A-Z`, `0-9` and `_`, and never `GITHUB_TOKEN`: it
/// starts with [`PR_OAUTH_PLACEHOLDER_PREFIX`], then `_U`, the account's
/// digits, `_`, and either `NONE` or the requester's principal id in
/// uppercase hexadecimal (an injective encoding that cannot spell `NONE`), so
/// it cannot match the generic `GITHUB_TOKEN` replacement either.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrOauthPlaceholder(String);

impl PrOauthPlaceholder {
    /// The placeholder for `github_user_id` (the account's numeric id) and
    /// `requester_principal_id` (the proxy's bound requester, `None` for
    /// none). Anything but 1 to 20 ASCII digits for the account, or an empty
    /// or over-long requester, is [`SandboxError::InvalidSpec`].
    pub fn derive(
        github_user_id: &str,
        requester_principal_id: Option<&str>,
    ) -> SandboxResult<Self> {
        if github_user_id.is_empty()
            || github_user_id.len() > 20
            || !github_user_id.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(SandboxError::InvalidSpec(
                "the GitHub account id must be 1 to 20 digits".to_owned(),
            ));
        }
        let requester = match requester_principal_id {
            None => "NONE".to_owned(),
            Some(requester) if !requester.is_empty() && requester.len() <= 128 => requester
                .bytes()
                .map(|byte| format!("{byte:02X}"))
                .collect(),
            Some(_) => {
                return Err(SandboxError::InvalidSpec(
                    "the requester principal id must be 1 to 128 bytes".to_owned(),
                ));
            }
        };
        Ok(Self(format!(
            "{PR_OAUTH_PLACEHOLDER_PREFIX}_U{github_user_id}_{requester}"
        )))
    }

    /// The placeholder used when no account is attested.
    pub fn unattested() -> Self {
        Self(PR_OAUTH_UNATTESTED_PLACEHOLDER.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What GitHub answered to a [`SandboxBackend::create_github_pull_request`]
/// call, as the sandbox's proxy relayed it. `body` is GitHub's raw response
/// body: the caller parses it and must not trust it beyond what GitHub says.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyHttpsResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The REST path of the pull-request create endpoint for `owner/repo`:
/// `/repos/{owner}/{repo}/pulls`.
///
/// `owner` and `repo` must each be one GitHub name segment (`A-Z a-z 0-9 . _
/// -`, non-empty, and neither `.` nor `..`), so neither can add path
/// separators, dot segments, queries or other characters. Anything else is
/// [`SandboxError::InvalidSpec`], which backends return before any network
/// call.
pub fn github_pull_requests_path(owner: &str, repo: &str) -> SandboxResult<String> {
    for (label, segment) in [("owner", owner), ("repo", repo)] {
        let safe = !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !safe {
            return Err(SandboxError::InvalidSpec(format!(
                "GitHub {label} must be a single name segment"
            )));
        }
    }
    Ok(format!("/repos/{owner}/{repo}/pulls"))
}

#[async_trait]
/// Backend-neutral lifecycle and byte-I/O operations for one sandbox runtime.
///
/// This trait intentionally models only the isolated workload primitive. Higher
/// layers decide why the sandbox exists and how stdin/stdout bytes should be
/// framed.
pub trait SandboxBackend: Send + Sync {
    /// Stable backend name used in handles, observations, and diagnostics.
    fn name(&self) -> &'static str;

    /// Create a sandbox from the supplied workload spec and return its handle.
    async fn create(&self, spec: SandboxSpec) -> SandboxResult<SandboxHandle>;

    /// Open owned stdin/stdout/stderr handles for a running sandbox.
    async fn open_io(&self, id: &SandboxId) -> SandboxResult<SandboxIo>;

    /// Read the sandbox workload's recorded stdout history since `since`.
    ///
    /// Live io streams only deliver output from attach time forward; output
    /// emitted while no reader was attached (for example across a control
    /// plane restart) is otherwise lost. Backends whose runtime records the
    /// workload's stdout (such as Kubernetes pod logs) can replay it here so
    /// orphaned executions can be adopted instead of failed.
    async fn read_output_since(
        &self,
        _id: &SandboxId,
        _since: Option<std::time::SystemTime>,
    ) -> SandboxResult<Vec<String>> {
        Err(crate::SandboxError::Unsupported {
            backend: self.name(),
            operation: "read_output_since",
        })
    }

    /// Return the network address (`<host>`, with IPv6 literals bracketed)
    /// where a running sandbox's workload accepts TCP connections.
    ///
    /// Backends that do not expose workload networking return
    /// [`crate::SandboxError::Unsupported`]. Callers add the port.
    async fn workload_address(&self, _id: &SandboxId) -> SandboxResult<String> {
        Err(crate::SandboxError::Unsupported {
            backend: self.name(),
            operation: "workload_address",
        })
    }

    /// Return the portable, cheap lifecycle status for a sandbox.
    async fn status(&self, id: &SandboxId) -> SandboxResult<SandboxStatus>;

    /// Return the full observed runtime snapshot for one sandbox.
    ///
    /// Unlike [`SandboxBackend::status`], this can include backend-owned
    /// diagnostic context used by reconcilers.
    async fn observe(&self, id: &SandboxId) -> SandboxResult<ObservedSandbox>;

    /// List all sandbox observations owned by this backend/control plane.
    async fn list_observed(&self) -> SandboxResult<Vec<ObservedSandbox>>;

    /// Stop the sandbox and clean up backend-owned runtime resources.
    async fn stop(&self, id: &SandboxId) -> SandboxResult<()>;

    /// Rebind a running sandbox's managed iron-proxy to a different
    /// iron-control principal, together with the requesting user's principal
    /// for the current turn (`None` clears a previous requester binding).
    async fn assign_iron_control_proxy_principal(
        &self,
        _id: &SandboxId,
        _principal_id: &str,
        _requester_principal_id: Option<&str>,
        _labels: &BTreeMap<String, String>,
    ) -> SandboxResult<()> {
        Err(crate::SandboxError::Unsupported {
            backend: self.name(),
            operation: "assign_iron_control_proxy_principal",
        })
    }

    /// Ensure a running sandbox's managed iron-proxy resources are present and
    /// usable for the supplied iron-control principal and requester without
    /// otherwise changing the sandbox lifecycle.
    async fn ensure_iron_control_proxy_resources(
        &self,
        _id: &SandboxId,
        _principal_id: &str,
        _requester_principal_id: Option<&str>,
        _labels: &BTreeMap<String, String>,
    ) -> SandboxResult<()> {
        Ok(())
    }

    /// Open the observability network path for a running sandbox that was
    /// created without it, without restarting the workload. Warm sandboxes
    /// boot without observability so one pool serves sessions with and
    /// without it.
    async fn enable_observability(&self, _id: &SandboxId) -> SandboxResult<()> {
        Err(crate::SandboxError::Unsupported {
            backend: self.name(),
            operation: "enable_observability",
        })
    }

    /// Create a pull request through the sandbox's own credential-injecting
    /// proxy (`placeholder` is the bearer placeholder, bound to the identity
    /// api-rs validated; see [`PrOauthPlaceholder`]): `POST https://api.github.com/repos/{owner}/{repo}/pulls` with
    /// `payload` as the JSON body.
    ///
    /// This is deliberately not a general proxied HTTPS request. The backend
    /// fixes the method, host, path (see [`github_pull_requests_path`]) and
    /// headers, so the caller cannot choose any of them. The request carries
    /// only the placeholder `Bearer CENTAUR_PR_OAUTH`; the sandbox's proxy swaps in
    /// the OAuth token of the human its principal is bound to, for this one
    /// operation and only while that credential is attested as OAuth. Otherwise
    /// the placeholder reaches GitHub unsubstituted and is rejected. The pull
    /// request is authored as that person and api-rs never holds the token.
    /// Callers must still verify the author in the response.
    async fn create_github_pull_request(
        &self,
        _id: &SandboxId,
        _owner: &str,
        _repo: &str,
        _placeholder: &PrOauthPlaceholder,
        _payload: serde_json::Value,
    ) -> SandboxResult<ProxyHttpsResponse> {
        Err(SandboxError::Unsupported {
            backend: self.name(),
            operation: "create_github_pull_request",
        })
    }

    /// Suspend the sandbox while preserving any backend-supported runtime state.
    async fn pause(&self, id: &SandboxId) -> SandboxResult<()>;

    /// Resume a previously suspended sandbox and wait until it can serve I/O.
    async fn resume(&self, id: &SandboxId) -> SandboxResult<()>;

    /// Delete iron-proxy resources that outlived their sandbox.
    ///
    /// A failed create, resume, or unwind can leave the proxy's pod, service,
    /// and network policies behind, and once the Sandbox CR is gone nothing
    /// keyed on an observed sandbox can reach them. Backends that manage no
    /// proxy resources report none. Returns the number of resources deleted
    /// per class.
    async fn reap_orphan_iron_proxy_resources(
        &self,
        _grace: std::time::Duration,
    ) -> SandboxResult<BTreeMap<String, u32>> {
        Ok(BTreeMap::new())
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::{PrOauthPlaceholder, github_pull_requests_path};
    use crate::{
        ObservedSandbox, SandboxBackend, SandboxError, SandboxHandle, SandboxId, SandboxIo,
        SandboxResult, SandboxSpec, SandboxStatus,
    };

    /// A backend that implements only the required methods.
    struct Minimal;

    #[async_trait]
    impl SandboxBackend for Minimal {
        fn name(&self) -> &'static str {
            "minimal"
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
    }

    #[tokio::test]
    async fn backends_do_not_create_pull_requests_unless_they_implement_it() {
        let error = Minimal
            .create_github_pull_request(
                &SandboxId::new("sbx-1"),
                "acme",
                "widgets",
                &PrOauthPlaceholder::unattested(),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            SandboxError::Unsupported {
                backend: "minimal",
                operation: "create_github_pull_request"
            }
        ));
    }

    #[test]
    fn pull_request_path_is_fixed_to_one_owner_and_repo() {
        assert_eq!(
            github_pull_requests_path("acme-co", "widgets.rs_2").unwrap(),
            "/repos/acme-co/widgets.rs_2/pulls"
        );
    }

    #[test]
    fn pull_request_path_refuses_anything_but_single_name_segments() {
        for (owner, repo) in [
            ("", "widgets"),
            ("acme", ""),
            (".", "widgets"),
            ("acme", ".."),
            ("..", ".."),
            ("acme/evil", "widgets"),
            ("acme", "widgets/pulls/../../../user"),
            ("acme", "widgets?x=1"),
            ("acme", "widgets#frag"),
            ("acme", "wid gets"),
            ("acme%2fevil", "widgets"),
            ("acme", "widgets\\evil"),
            ("acme", "widgets\n"),
            ("acme", "wïdgets"),
        ] {
            assert!(
                matches!(
                    github_pull_requests_path(owner, repo),
                    Err(SandboxError::InvalidSpec(_))
                ),
                "{owner:?}/{repo:?}"
            );
        }
    }

    #[test]
    fn the_pr_oauth_placeholder_is_bound_to_the_account_and_requester() {
        use super::PrOauthPlaceholder;

        let derive = |account: &str, requester: Option<&str>| {
            PrOauthPlaceholder::derive(account, requester)
                .unwrap()
                .as_str()
                .to_owned()
        };
        let alice = derive("90210001", Some("prn_Alice1"));
        assert_eq!(derive("90210001", Some("prn_Alice1")), alice);
        assert_eq!(alice, "CENTAUR_PR_OAUTH_U90210001_70726E5F416C69636531");
        assert_eq!(derive("90210001", None), "CENTAUR_PR_OAUTH_U90210001_NONE");

        // A different account, a different requester, or none at all: each
        // is a different placeholder.
        let all = [
            alice.clone(),
            derive("90210002", Some("prn_Alice1")),
            derive("90210001", Some("prn_Bob2")),
            derive("90210001", Some("prn_alice1")),
            derive("90210001", None),
            PrOauthPlaceholder::unattested().as_str().to_owned(),
        ];
        let distinct: std::collections::BTreeSet<_> = all.iter().collect();
        assert_eq!(distinct.len(), all.len(), "{all:?}");

        // Placeholder-safe, and never a match for the generic placeholder.
        for placeholder in &all {
            assert!(placeholder.starts_with("CENTAUR_PR_OAUTH"), "{placeholder}");
            assert!(
                placeholder
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "{placeholder}"
            );
            assert!(!placeholder.contains("GITHUB_TOKEN"), "{placeholder}");
        }
        // Even a requester that spells the generic placeholder cannot put it
        // in: it is encoded.
        let tricky = derive("1", Some("GITHUB_TOKEN"));
        assert!(!tricky.contains("GITHUB_TOKEN"), "{tricky}");
    }

    #[test]
    fn the_pr_oauth_placeholder_refuses_malformed_identities() {
        use super::PrOauthPlaceholder;

        for account in ["", "abc", "12 3", "-1", "123456789012345678901"] {
            assert!(
                matches!(
                    PrOauthPlaceholder::derive(account, None),
                    Err(SandboxError::InvalidSpec(_))
                ),
                "{account:?}"
            );
        }
        for requester in [Some(""), Some(&*"x".repeat(129))] {
            assert!(PrOauthPlaceholder::derive("1", requester).is_err());
        }
    }
}
