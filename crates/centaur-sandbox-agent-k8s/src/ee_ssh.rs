//! Disposable sandbox identities for the opt-in Iron SSH companion.
use centaur_sandbox_core::{SandboxError, SandboxResult};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ssh_key::{
    LineEnding, PrivateKey,
    private::{Ed25519Keypair, KeypairData},
};

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct Identity {
    client_private: String,
    client_public: String,
    host_private: String,
    host_public: String,
    route: String,
    workspace: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Identity([redacted])")
    }
}

impl Identity {
    pub(crate) fn derive(scope: &str, route: &str, workspace: &str) -> SandboxResult<Self> {
        let seed = std::env::var("CENTAUR_EE_SSH_SEED").map_err(|_| {
            SandboxError::InvalidSpec("SSH enabled without CENTAUR_EE_SSH_SEED".into())
        })?;
        Self::from_seed(&seed, scope, route, workspace)
    }
    fn from_seed(seed: &str, scope: &str, route: &str, workspace: &str) -> SandboxResult<Self> {
        if seed.len() < 32 {
            return Err(SandboxError::InvalidSpec(
                "SSH seed must contain at least 32 bytes".into(),
            ));
        }
        let pair = |purpose: &str| -> SandboxResult<(String, String)> {
            let mut hash = Sha256::new();
            for value in ["centaur-ee-ssh-v1", seed, scope, purpose] {
                hash.update((value.len() as u64).to_be_bytes());
                hash.update(value.as_bytes());
            }
            let bytes: [u8; 32] = hash.finalize().into();
            let key = PrivateKey::new(KeypairData::Ed25519(Ed25519Keypair::from_seed(&bytes)), "")
                .map_err(|_| SandboxError::InvalidSpec("SSH key generation failed".into()))?;
            let private = key
                .to_openssh(LineEnding::LF)
                .map_err(|_| SandboxError::InvalidSpec("SSH key encoding failed".into()))?
                .to_string();
            let public = key
                .public_key()
                .to_openssh()
                .map_err(|_| SandboxError::InvalidSpec("SSH public key encoding failed".into()))?;
            Ok((private, public))
        };
        let (client_private, client_public) = pair("client")?;
        let (host_private, host_public) = pair("host")?;
        Ok(Self {
            client_private,
            client_public,
            host_private,
            host_public,
            route: route.into(),
            workspace: workspace.into(),
        })
    }
    #[cfg(test)]
    pub(crate) fn fixture(scope: &str) -> Self {
        Self::from_seed(
            "test-only-seed-with-at-least-32-bytes",
            scope,
            "ee",
            "/workspace",
        )
        .unwrap()
    }
    pub(crate) fn sandbox(&self, host: &str) -> Value {
        json!({"host":host,"port":2222,"private_key":self.client_private,"host_key":self.host_public,"route":self.route,"workspace":self.workspace})
    }
    pub(crate) fn proxy(&self, control_url: &str, deny_cidrs: &[String]) -> Value {
        let mut config = json!({"listen":"0.0.0.0:2222","host_key":{"type":"control_plane","value":self.host_private},"client_public_key":self.client_public,"authorize_url":format!("{}/api/v1/proxy/ssh/authorize",control_url.trim_end_matches('/')),"token_env":"IRON_PROXY_TOKEN"});
        if !deny_cidrs.is_empty() {
            config["upstream_deny_cidrs"] = json!(deny_cidrs);
        }
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_are_stable_and_isolated() {
        let seed = "test-only-seed-with-at-least-32-bytes";
        let a = Identity::from_seed(seed, "sandbox-a", "ee", "/workspace").unwrap();
        let again = Identity::from_seed(seed, "sandbox-a", "ee", "/workspace").unwrap();
        let b = Identity::from_seed(seed, "sandbox-b", "ee", "/workspace").unwrap();
        assert_eq!(a, again);
        assert_ne!(a.client_public, b.client_public);
        assert_ne!(a.client_public, a.host_public);
        let client = a.sandbox("proxy").to_string();
        assert!(!client.contains(&a.host_private));
        assert!(!client.contains(seed));
        assert!(!format!("{a:?}").contains(&a.client_private));
        assert!(Identity::from_seed("short", "a", "ee", "/").is_err());
    }
}
