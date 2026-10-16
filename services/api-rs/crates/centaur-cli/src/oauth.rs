//! OAuth 2.1 public-client flow against Console's built-in CLI client:
//! discovery, authorization code + PKCE over a loopback redirect, refresh, and
//! revocation.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use centaur_api_types::ProtectedResourceMetadata;
use eyre::{Result, WrapErr, bail, eyre};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use url::Url;
use uuid::Uuid;

const READ_SCOPE: &str = "sessions:read";
/// Requested whenever the console offers it; a console without it still
/// grants a read-only login.
pub(crate) const WRITE_SCOPE: &str = "sessions:write";
/// Console matches loopback redirects on host and path and lets the port vary
/// (RFC 8252 §7.3), so its CLI client covers every ephemeral port.
const REDIRECT_PATH: &str = "/callback";
const MAX_CALLBACK_REQUEST_BYTES: usize = 16 * 1024;
/// How long one loopback connection may take to send its request line, so a
/// silent connection cannot hold the server while the browser waits.
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct AuthorizationServer {
    pub(crate) authorization_endpoint: String,
    pub(crate) token_endpoint: String,
    pub(crate) revocation_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    /// Console's built-in public client for this CLI, shared by every login.
    pub(crate) centaur_cli_client_id: Option<String>,
}

impl AuthorizationServer {
    /// The space-delimited scopes this CLI asks for at this server.
    fn requested_scope(&self) -> String {
        let mut scopes = vec![READ_SCOPE];
        if self
            .scopes_supported
            .iter()
            .any(|scope| scope == WRITE_SCOPE)
        {
            scopes.push(WRITE_SCOPE);
        }
        scopes.join(" ")
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct TokenResponse {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_in: i64,
}

impl TokenResponse {
    pub(crate) fn expires_at(&self) -> i64 {
        now_unix() + self.expires_in
    }
}

#[derive(Debug, Deserialize)]
struct OAuthErrorBody {
    error: String,
    error_description: Option<String>,
}

/// An OAuth error response from Console.
#[derive(Debug)]
struct OAuthError {
    error: String,
    description: Option<String>,
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.description {
            Some(description) => write!(f, "{}: {description}", self.error),
            None => f.write_str(&self.error),
        }
    }
}

impl std::error::Error for OAuthError {}

/// A refresh token Console no longer honors. The only fix is a new login.
#[derive(Debug)]
pub(crate) struct InvalidGrant;

impl std::fmt::Display for InvalidGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the stored login is no longer valid")
    }
}

impl std::error::Error for InvalidGrant {}

/// Finds Console through api-rs' protected-resource metadata unless the caller
/// names the authorization server directly.
pub(crate) async fn discover(
    http: &reqwest::Client,
    api_url: &Url,
    authorization_server: Option<&Url>,
) -> Result<AuthorizationServer> {
    let issuer =
        match authorization_server {
            Some(url) => url.clone(),
            None => {
                let metadata_url = api_url.join("/.well-known/oauth-protected-resource")?;
                let metadata: ProtectedResourceMetadata = get_json(http, &metadata_url).await?;
                let issuer = metadata.authorization_servers.into_iter().next().ok_or_else(|| {
                eyre!(
                    "{metadata_url} lists no authorization server; pass --authorization-server"
                )
            })?;
                Url::parse(&issuer)
                    .wrap_err_with(|| format!("invalid authorization server {issuer}"))?
            }
        };
    let metadata_url = issuer.join("/.well-known/oauth-authorization-server")?;
    let server: AuthorizationServer = get_json(http, &metadata_url).await?;
    if !server
        .scopes_supported
        .iter()
        .any(|scope| scope == READ_SCOPE)
    {
        bail!("{issuer} does not offer the {READ_SCOPE} scope; upgrade Console");
    }
    Ok(server)
}

async fn get_json<T: for<'de> Deserialize<'de>>(http: &reqwest::Client, url: &Url) -> Result<T> {
    let response = http
        .get(url.clone())
        .send()
        .await
        .wrap_err_with(|| format!("GET {url} failed"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("GET {url} returned {status}");
    }
    response
        .json()
        .await
        .wrap_err_with(|| format!("GET {url} returned invalid JSON"))
}

/// Runs the browser half of the flow and returns the token response.
pub(crate) async fn authorize(
    http: &reqwest::Client,
    server: &AuthorizationServer,
    client_id: &str,
    open_browser: bool,
    timeout: Duration,
) -> Result<TokenResponse> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .wrap_err("failed to bind a loopback port for the login callback")?;
    let redirect_uri = format!(
        "http://127.0.0.1:{}{REDIRECT_PATH}",
        listener.local_addr()?.port()
    );
    let verifier = code_verifier();
    let state = Uuid::new_v4().simple().to_string();

    let mut authorize_url = Url::parse(&server.authorization_endpoint)
        .wrap_err("authorization server returned an invalid authorization_endpoint")?;
    authorize_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", &server.requested_scope())
        .append_pair("state", &state)
        .append_pair("code_challenge", &code_challenge(&verifier))
        .append_pair("code_challenge_method", "S256");

    eprintln!("Open this URL to sign in:\n\n  {authorize_url}\n");
    if open_browser && open_in_browser(authorize_url.as_str()).is_err() {
        eprintln!("(could not open a browser automatically)");
    }
    eprintln!("Waiting for the browser to finish...");

    let callback = tokio::time::timeout(timeout, wait_for_callback(&listener, &state))
        .await
        .map_err(|_| eyre!("timed out waiting for the browser to finish signing in"))??;
    if let Some(error) = callback.error {
        match callback.error_description {
            Some(description) => bail!("authorization failed: {error}: {description}"),
            None => bail!("authorization failed: {error}"),
        }
    }
    let code = callback
        .code
        .ok_or_else(|| eyre!("login callback did not include an authorization code"))?;

    let response = http
        .post(&server.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .wrap_err("token request failed")?;
    oauth_json(response, "token exchange").await
}

pub(crate) async fn refresh(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let response = http
        .post(token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .wrap_err("token refresh request failed")?;
    oauth_json(response, "token refresh")
        .await
        .map_err(refresh_error)
}

/// Only for a refresh does `invalid_grant` mean the stored login is dead;
/// during login it means this sign-in's code was rejected.
fn refresh_error(error: eyre::Report) -> eyre::Report {
    match error.downcast_ref::<OAuthError>() {
        Some(oauth) if oauth.error == "invalid_grant" => InvalidGrant.into(),
        _ => error,
    }
}

pub(crate) async fn revoke(
    http: &reqwest::Client,
    revocation_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<()> {
    let response = http
        .post(revocation_endpoint)
        .form(&[
            ("client_id", client_id),
            ("token", refresh_token),
            ("token_type_hint", "refresh_token"),
        ])
        .send()
        .await
        .wrap_err("revocation request failed")?;
    let status = response.status();
    if !status.is_success() {
        bail!("revocation returned {status}");
    }
    Ok(())
}

async fn oauth_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    action: &str,
) -> Result<T> {
    let status = response.status();
    let body = response
        .bytes()
        .await
        .wrap_err_with(|| format!("{action} response was unreadable"))?;
    if status.is_success() {
        return serde_json::from_slice(&body)
            .wrap_err_with(|| format!("{action} returned an unexpected response"));
    }
    Err(error_response(action, status, &body))
}

fn error_response(action: &str, status: reqwest::StatusCode, body: &[u8]) -> eyre::Report {
    match serde_json::from_slice::<OAuthErrorBody>(body) {
        Ok(body) => eyre::Report::new(OAuthError {
            error: body.error,
            description: body.error_description,
        })
        .wrap_err(format!("{action} failed")),
        Err(_) => eyre!("{action} returned {status}"),
    }
}

#[derive(Debug, Default, PartialEq)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Serves the loopback redirect until the browser delivers the callback for
/// this login's `state`. Anything else that connects (favicon requests, probes,
/// a stale tab from an earlier attempt) is answered and the wait continues.
async fn wait_for_callback(listener: &TcpListener, state: &str) -> Result<Callback> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let Ok(Ok(target)) =
            tokio::time::timeout(CALLBACK_READ_TIMEOUT, read_request_target(&mut stream)).await
        else {
            continue;
        };
        match parse_callback(&target) {
            Some(callback) if callback.state.as_deref() != Some(state) => {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "This sign-in response is not for the login in progress.",
                )
                .await;
            }
            Some(callback) => {
                let message = if callback.error.is_some() {
                    "Sign-in was not completed. You can close this tab."
                } else {
                    "Signed in to Centaur. You can close this tab."
                };
                respond(&mut stream, "200 OK", message).await;
                return Ok(callback);
            }
            None => respond(&mut stream, "404 Not Found", "Not found.").await,
        }
    }
}

async fn read_request_target(stream: &mut TcpStream) -> Result<String> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        if buffer.len() >= MAX_CALLBACK_REQUEST_BYTES {
            bail!("callback request is too large");
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            bail!("callback connection closed early");
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let request = String::from_utf8_lossy(&buffer);
    let request_line = request.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => Ok(target.to_owned()),
        _ => bail!("unexpected callback request"),
    }
}

fn parse_callback(target: &str) -> Option<Callback> {
    let url = Url::parse("http://127.0.0.1").ok()?.join(target).ok()?;
    if url.path() != REDIRECT_PATH {
        return None;
    }
    let mut callback = Callback::default();
    for (key, value) in url.query_pairs() {
        let value = Some(value.into_owned());
        match key.as_ref() {
            "code" => callback.code = value,
            "state" => callback.state = value,
            "error" => callback.error = value,
            "error_description" => callback.error_description = value,
            _ => {}
        }
    }
    Some(callback)
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Centaur</title>\
         <body style=\"font-family:system-ui;margin:4rem\"><p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// 64 hex characters (256 bits from two v4 UUIDs, 244 of them random), within
/// RFC 7636's 43–128 character range.
fn code_verifier() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn open_in_browser(url: &str) -> std::io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
}

pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_write_access_only_where_offered() {
        let server = |scopes: &[&str]| AuthorizationServer {
            authorization_endpoint: String::new(),
            token_endpoint: String::new(),
            centaur_cli_client_id: None,
            revocation_endpoint: None,
            scopes_supported: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        };
        assert_eq!(
            server(&["mcp:tools", "sessions:read", "sessions:write"]).requested_scope(),
            "sessions:read sessions:write"
        );
        assert_eq!(
            server(&["sessions:read"]).requested_scope(),
            "sessions:read"
        );
    }

    #[test]
    fn code_challenge_matches_rfc_7636_appendix_b() {
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn code_verifier_has_a_valid_length() {
        let verifier = code_verifier();
        assert!((43..=128).contains(&verifier.len()));
        assert_ne!(verifier, code_verifier());
    }

    #[test]
    fn parses_the_callback_and_ignores_other_paths() {
        assert_eq!(
            parse_callback("/callback?code=abc&state=xyz"),
            Some(Callback {
                code: Some("abc".to_owned()),
                state: Some("xyz".to_owned()),
                ..Callback::default()
            })
        );
        assert_eq!(
            parse_callback(
                "/callback?error=access_denied&error_description=The+user+denied&state=s"
            ),
            Some(Callback {
                state: Some("s".to_owned()),
                error: Some("access_denied".to_owned()),
                error_description: Some("The user denied".to_owned()),
                ..Callback::default()
            })
        );
        assert_eq!(parse_callback("/favicon.ico"), None);
    }

    #[test]
    fn invalid_grant_means_a_dead_login_only_on_refresh() {
        let rejected = || {
            error_response(
                "token exchange",
                reqwest::StatusCode::BAD_REQUEST,
                br#"{"error":"invalid_grant","error_description":"PKCE verification failed."}"#,
            )
        };
        let exchange = rejected();
        assert!(!exchange.is::<InvalidGrant>());
        assert_eq!(
            format!("{exchange:#}"),
            "token exchange failed: invalid_grant: PKCE verification failed."
        );
        assert!(refresh_error(rejected()).is::<InvalidGrant>());

        let other = error_response(
            "token refresh",
            reqwest::StatusCode::UNAUTHORIZED,
            br#"{"error":"invalid_client"}"#,
        );
        assert!(!refresh_error(other).is::<InvalidGrant>());
    }

    #[tokio::test]
    async fn loopback_server_skips_strays_and_returns_the_callback() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = tokio::spawn(async move {
            for target in [
                "/favicon.ico",
                "/callback",
                "/callback?code=stale&state=old",
                "/callback?code=abc&state=xyz",
            ] {
                let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                stream
                    .write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                let mut response = String::new();
                stream.read_to_string(&mut response).await.unwrap();
                assert!(response.starts_with("HTTP/1.1 "));
            }
        });

        let callback = wait_for_callback(&listener, "xyz").await.unwrap();
        client.await.unwrap();
        assert_eq!(callback.code.as_deref(), Some("abc"));
        assert_eq!(callback.state.as_deref(), Some("xyz"));
    }
}
