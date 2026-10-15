//! `login`, `logout`, and `whoami`, plus the credential resolution every other
//! command runs with.

use std::{env, time::Duration};

use centaur_api_types::WhoamiResponse;
use eyre::{Result, WrapErr, bail, eyre};
use serde_json::Value;
use url::Url;

use crate::{
    oauth::{self, InvalidGrant},
    store::{Profile, Store},
};

/// Refresh an access token this long before it expires.
const REFRESH_MARGIN_SECONDS: i64 = 60;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub(crate) async fn login(
    http: &reqwest::Client,
    profile_name: &str,
    api_url: Option<Url>,
    authorization_server: Option<Url>,
    no_browser: bool,
) -> Result<()> {
    let previous = Store::load()?.profiles.remove(profile_name);
    let api_url = match (api_url, previous) {
        (Some(url), _) => url,
        (None, Some(profile)) => Url::parse(&profile.api_url)?,
        (None, None) => bail!("pass --api-url (or set CENTAUR_API_URL) for the first login"),
    };

    let server = oauth::discover(http, &api_url, authorization_server.as_ref()).await?;
    let client_id = server
        .centaur_cli_client_id
        .clone()
        .ok_or_else(|| eyre!("Console does not offer a client for this CLI; upgrade Console"))?;
    let tokens = oauth::authorize(http, &server, &client_id, !no_browser, LOGIN_TIMEOUT).await?;

    let profile = Profile {
        api_url: api_url.to_string(),
        client_id,
        token_endpoint: server.token_endpoint,
        revocation_endpoint: server.revocation_endpoint,
        refresh_token: tokens.refresh_token.clone(),
        access_token: tokens.access_token.clone(),
        access_token_expires_at: tokens.expires_at(),
    };
    let replaced = {
        // Re-read under the lock: another command may have refreshed the login
        // this one replaces while the browser was open.
        let _lock = Store::lock()?;
        let mut store = Store::load()?;
        let replaced = store
            .profiles
            .insert(profile_name.to_owned(), profile.clone());
        store.save()?;
        replaced
    };
    // Retire the login this one replaces; a failure only leaves a token that
    // expires on its own.
    if let Some(replaced) = replaced {
        revoke_quietly(http, &replaced).await;
    }

    let identity = fetch_whoami(http, &api_url, &profile.access_token).await?;
    eprintln!(
        "Logged in to {api_url} as {}.",
        display_name(&profile.access_token, &identity)
    );
    Ok(())
}

pub(crate) async fn logout(http: &reqwest::Client, profile_name: &str) -> Result<()> {
    let profile = {
        let _lock = Store::lock()?;
        let mut store = Store::load()?;
        let Some(profile) = store.profiles.remove(profile_name) else {
            eprintln!("Not logged in (profile {profile_name}).");
            return Ok(());
        };
        store.save()?;
        profile
    };
    revoke_quietly(http, &profile).await;
    eprintln!("Logged out of {}.", profile.api_url);
    Ok(())
}

async fn revoke_quietly(http: &reqwest::Client, profile: &Profile) {
    let Some(endpoint) = profile.revocation_endpoint.as_deref() else {
        return;
    };
    if let Err(error) =
        oauth::revoke(http, endpoint, &profile.client_id, &profile.refresh_token).await
    {
        eprintln!("warning: could not revoke the previous login: {error}");
    }
}

pub(crate) async fn whoami(
    http: &reqwest::Client,
    profile_name: &str,
    api_url: Option<Url>,
    json: bool,
) -> Result<()> {
    let target = resolve_credentials(http, profile_name, api_url).await?;
    let identity = fetch_whoami(http, &target.api_url, &target.access_token).await?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "api_url": target.api_url.as_str(),
                "source": target.source,
                "class": identity.class,
                "principal": identity.principal,
                "email": token_claim(&target.access_token, "email"),
                "capabilities": identity.capabilities,
            }))?
        );
        return Ok(());
    }
    println!("API:          {}", target.api_url);
    println!(
        "Signed in as: {}",
        display_name(&target.access_token, &identity)
    );
    println!("Caller class: {}", identity.class);
    if let Some(principal) = &identity.principal {
        println!("Principal:    {principal}");
    }
    println!(
        "Capabilities: {}",
        if identity.capabilities.is_empty() {
            "(none)".to_owned()
        } else {
            identity.capabilities.join(", ")
        }
    );
    println!("Credentials:  {}", target.source);
    Ok(())
}

async fn fetch_whoami(
    http: &reqwest::Client,
    api_url: &Url,
    token: &str,
) -> Result<WhoamiResponse> {
    let url = api_url.join("/api/auth/whoami")?;
    let response = http
        .get(url.clone())
        .bearer_auth(token)
        .send()
        .await
        .wrap_err_with(|| format!("GET {url} failed"))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        bail!("{api_url} rejected the credentials; run `centaur login`");
    }
    if !status.is_success() {
        bail!("GET {url} returned {status}");
    }
    response
        .json()
        .await
        .wrap_err_with(|| format!("GET {url} returned invalid JSON"))
}

/// The credentials a command runs with.
pub(crate) struct Credentials {
    pub(crate) api_url: Url,
    pub(crate) access_token: String,
    /// Where the token came from, for display. Never the token itself.
    pub(crate) source: &'static str,
}

/// `CENTAUR_API_TOKEN` wins so operators and tests can use static keys;
/// otherwise the stored login, refreshed when it is about to expire.
pub(crate) async fn resolve_credentials(
    http: &reqwest::Client,
    profile_name: &str,
    api_url: Option<Url>,
) -> Result<Credentials> {
    if let Some(token) = env::var("CENTAUR_API_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        let api_url =
            api_url.ok_or_else(|| eyre!("CENTAUR_API_TOKEN is set; also set CENTAUR_API_URL"))?;
        return Ok(Credentials {
            api_url,
            access_token: token.trim().to_owned(),
            source: "CENTAUR_API_TOKEN",
        });
    }

    let mut lock = None;
    loop {
        let mut store = Store::load()?;
        let profile = store
            .profiles
            .get_mut(profile_name)
            .ok_or_else(|| eyre!("not logged in (profile {profile_name}); run `centaur login`"))?;
        let profile_url = Url::parse(&profile.api_url)?;
        // A stored token belongs to the deployment it was issued for; never send
        // it anywhere else.
        if let Some(requested) = &api_url
            && *requested != profile_url
        {
            bail!(
                "profile {profile_name} is logged in to {profile_url}, not {requested}; \
                 run `centaur login --api-url {requested}` or pick another --profile"
            );
        }
        if profile.access_token_expires_at - REFRESH_MARGIN_SECONDS > oauth::now_unix() {
            return Ok(Credentials {
                api_url: profile_url,
                access_token: profile.access_token.clone(),
                source: "stored login",
            });
        }
        if lock.is_none() {
            // Refresh under the lock, re-reading first: a command that held it
            // may have just stored a fresh token.
            lock = Some(Store::lock()?);
            continue;
        }
        return match oauth::refresh(
            http,
            &profile.token_endpoint,
            &profile.client_id,
            &profile.refresh_token,
        )
        .await
        {
            Ok(tokens) => {
                profile.access_token_expires_at = tokens.expires_at();
                profile.access_token = tokens.access_token.clone();
                profile.refresh_token = tokens.refresh_token;
                store.save()?;
                Ok(Credentials {
                    api_url: profile_url,
                    access_token: tokens.access_token,
                    source: "stored login",
                })
            }
            // The profile stays so `centaur login` can reuse its API URL.
            Err(error) if error.is::<InvalidGrant>() => {
                bail!("your login has expired or was revoked; run `centaur login`")
            }
            Err(error) => Err(error),
        };
    }
}

/// The email Console put in the token, falling back to the principal. Read
/// for display only; api-rs is what verifies the token.
fn display_name(token: &str, identity: &WhoamiResponse) -> String {
    token_claim(token, "email")
        .or_else(|| identity.principal.clone())
        .unwrap_or_else(|| identity.class.clone())
}

fn token_claim(token: &str, claim: &str) -> Option<String> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    let payload = token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    claims.get(claim)?.as_str().map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_display_claims_without_trusting_them() {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let payload = URL_SAFE_NO_PAD.encode(r#"{"email":"user@example.com"}"#);
        let token = format!("e30.{payload}.sig");
        assert_eq!(
            token_claim(&token, "email").as_deref(),
            Some("user@example.com")
        );
        assert_eq!(token_claim("not-a-jwt", "email"), None);
        assert_eq!(token_claim(&token, "missing"), None);
    }
}
