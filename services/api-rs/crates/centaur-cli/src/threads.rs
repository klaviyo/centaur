//! `centaur threads`: list, inspect, and drive threads through the session API.

use std::io::{IsTerminal, Read};

use centaur_api_client::{CentaurClient, ClientError};
use centaur_api_types::{
    AppendMessagesRequest, CreateSessionRequest, ExecuteSessionRequest, ExecuteSessionResponse,
    InterruptSessionExecutionRequest, ListSessionsQuery, SessionSummary,
};
use centaur_session_core::{HarnessType, MessageRole, SessionMessageInput, ThreadKey};
use eyre::{Result, WrapErr, bail, eyre};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::Url;
use uuid::Uuid;

use crate::{ThreadsCommand, TurnArgs, auth, oauth};

/// Identifies this client in session and message metadata.
const SOURCE: &str = "centaur-cli";
/// The session API otherwise records a reasonless interrupt as coming from Slack.
const DEFAULT_INTERRUPT_REASON: &str = "Interrupted from the centaur CLI";
const TITLE_WIDTH: usize = 48;

pub(crate) async fn run(
    http: &reqwest::Client,
    profile: &str,
    api_url: Option<Url>,
    command: ThreadsCommand,
) -> Result<()> {
    let credentials = auth::resolve_credentials(http, profile, api_url).await?;
    let writes = matches!(
        command,
        ThreadsCommand::New { .. } | ThreadsCommand::Send { .. } | ThreadsCommand::Interrupt { .. }
    );
    if writes && credentials.login_lacks_scope(oauth::WRITE_SCOPE) {
        bail!("your login can only read threads; run `centaur login` again to get write access");
    }
    let client =
        CentaurClient::with_bearer_token(credentials.api_url.as_str(), &credentials.access_token)
            .wrap_err("configure API client")?;
    match command {
        ThreadsCommand::List {
            prefix,
            limit,
            json,
        } => list(&client, prefix, limit, json).await,
        ThreadsCommand::Show { thread_key, json } => show(&client, &thread_key, json).await,
        ThreadsCommand::New { harness, turn } => new(&client, harness, turn).await,
        ThreadsCommand::Send { thread_key, turn } => {
            let message = turn_message(turn.message.clone(), true)?
                .ok_or_else(|| eyre!("pass a message or pipe one on stdin"))?;
            send(&client, &thread_key, &message, &turn).await
        }
        ThreadsCommand::Interrupt { thread_key, reason } => {
            let response = client
                .interrupt_session(
                    &thread_key,
                    InterruptSessionExecutionRequest {
                        reason: Some(reason.unwrap_or_else(|| DEFAULT_INTERRUPT_REASON.to_owned())),
                    },
                )
                .await
                .map_err(api_error)?;
            match response.execution_id {
                Some(execution_id) if response.interrupted => {
                    eprintln!("Interrupted {execution_id}.");
                }
                _ => eprintln!("Nothing was running on {thread_key}."),
            }
            Ok(())
        }
        ThreadsCommand::Pause { thread_key } => {
            let response = client.pause_session(&thread_key).await.map_err(api_error)?;
            if response.paused {
                eprintln!("Paused the sandbox for {thread_key}.");
            } else {
                eprintln!("{thread_key} has no idle sandbox to pause.");
            }
            Ok(())
        }
    }
}

async fn list(
    client: &CentaurClient,
    prefix: Option<String>,
    limit: u32,
    json: bool,
) -> Result<()> {
    let response = client
        .list_sessions(&ListSessionsQuery {
            limit: Some(limit),
            thread_key_prefix: prefix,
        })
        .await
        .map_err(api_error)?;
    if json {
        return print_json(&response);
    }
    if response.sessions.is_empty() {
        eprintln!("No threads.");
        return Ok(());
    }
    let now = OffsetDateTime::now_utc();
    let rows: Vec<[String; 6]> = response
        .sessions
        .iter()
        .map(|session| {
            [
                session.thread_key.to_string(),
                session.harness_type.to_string(),
                session.status.to_string(),
                session
                    .latest_execution
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |execution| execution.status.to_string()),
                ago(now, session.updated_at),
                truncate(session.title.as_deref().unwrap_or(""), TITLE_WIDTH),
            ]
        })
        .collect();
    print_table(
        [
            "THREAD", "HARNESS", "STATUS", "LAST RUN", "UPDATED", "TITLE",
        ],
        &rows,
    );
    Ok(())
}

async fn show(client: &CentaurClient, thread_key: &ThreadKey, json: bool) -> Result<()> {
    let summary = client
        .session_summary(thread_key)
        .await
        .map_err(api_error)?;
    if json {
        return print_json(&summary);
    }
    print_summary(&summary);
    Ok(())
}

fn print_summary(summary: &SessionSummary) {
    let now = OffsetDateTime::now_utc();
    println!("Thread:    {}", summary.thread_key);
    if let Some(title) = &summary.title {
        println!("Title:     {title}");
    }
    println!("Platform:  {}", summary.platform);
    println!("Harness:   {}", summary.harness_type);
    println!("Status:    {}", summary.status);
    println!("Created:   {}", timestamp(now, summary.created_at));
    println!("Updated:   {}", timestamp(now, summary.updated_at));
    match &summary.latest_execution {
        Some(execution) => {
            println!(
                "Last run:  {} ({}, {})",
                execution.execution_id,
                execution.status,
                ago(now, execution.completed_at.unwrap_or(execution.created_at))
            );
            if let Some(error) = &execution.error {
                println!("Error:     {error}");
            }
        }
        None => println!("Last run:  none"),
    }
}

async fn new(client: &CentaurClient, harness: HarnessType, turn: TurnArgs) -> Result<()> {
    // Resolve the message first so a bad one does not leave an empty thread.
    let message = turn_message(turn.message.clone(), false)?;
    if message.is_none() && turn.wait {
        bail!("--wait needs a message to start a turn");
    }
    let created = client
        .create_session_with_generated_key(CreateSessionRequest {
            harness_type: harness,
            persona_id: None,
            metadata: Some(json!({ "source": SOURCE })),
            on_harness_conflict: None,
        })
        .await
        .map_err(api_error)?;
    let Some(message) = message else {
        if turn.json {
            return print_json(&created);
        }
        // stdout carries only the thread key so scripts can capture it.
        println!("{}", created.session.thread_key);
        return Ok(());
    };
    let thread_key = created.session.thread_key;
    if turn.json || turn.wait {
        // stdout belongs to the execution JSON or the turn's NDJSON, both of
        // which identify the thread.
        eprintln!("Created {thread_key}.");
    } else {
        println!("{thread_key}");
    }
    send(client, &thread_key, &message, &turn).await
}

/// Appends the message, starts a turn, and with `--wait` follows that turn's
/// output until it ends.
async fn send(
    client: &CentaurClient,
    thread_key: &ThreadKey,
    message: &str,
    turn: &TurnArgs,
) -> Result<()> {
    // One ID makes a retried send idempotent for both the message and the turn.
    let request_id = Uuid::new_v4().to_string();
    client
        .append_messages(
            thread_key,
            AppendMessagesRequest {
                messages: vec![SessionMessageInput {
                    client_message_id: Some(request_id.clone()),
                    role: MessageRole::User,
                    parts: vec![json!({ "type": "text", "text": message })],
                    metadata: json!({ "source": SOURCE }),
                }],
            },
        )
        .await
        .map_err(api_error)?;
    let execution = client
        .execute_session(
            thread_key,
            ExecuteSessionRequest {
                idempotency_key: Some(request_id),
                metadata: Some(json!({ "source": SOURCE })),
                input_lines: vec![user_input_line(message)?],
                idle_timeout_ms: None,
                max_duration_ms: None,
            },
        )
        .await
        .map_err(api_error)?;
    if turn.json {
        print_json(&execution)?;
    } else {
        eprintln!("Started {} ({}).", execution.execution_id, execution.status);
    }
    if turn.wait {
        return follow_turn(client, &execution).await;
    }
    Ok(())
}

/// Prints the turn's raw harness output lines (NDJSON) to stdout until it ends.
/// Fails when the turn fails or is cancelled.
async fn follow_turn(client: &CentaurClient, execution: &ExecuteSessionResponse) -> Result<()> {
    // Replaying from the start of this execution's events cannot miss output
    // that arrived before the stream opened.
    let mut events = client
        .stream_events(&execution.thread_key, 0, Some(&execution.execution_id))
        .await
        .map_err(api_error)?;
    while let Some(event) = events.next().await {
        let event = event.wrap_err("read event stream")?;
        match event.event.as_str() {
            "session.output.line" => println!("{}", event.data),
            "session.execution_completed" => {
                eprintln!("Turn completed.");
                return Ok(());
            }
            "session.execution_failed" => bail!("turn failed: {}", failure_reason(&event.data)),
            "session.execution_cancelled" => bail!("turn was cancelled"),
            "session.stream_error" => bail!("event stream failed: {}", event.data),
            _ => {}
        }
    }
    bail!("event stream ended before the turn finished")
}

/// The `error` from an `execution_failed` payload, else the raw payload.
fn failure_reason(data: &str) -> String {
    serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|payload| payload.get("error")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| data.to_owned())
}

/// The harness input line for one user text turn.
fn user_input_line(text: &str) -> Result<String> {
    Ok(serde_json::to_string(&json!({
        "type": "user",
        "message": { "content": [{ "type": "text", "text": text }] },
    }))?)
}

/// The message from the argument, else from piped stdin. A terminal stdin is
/// never read: `required` callers fail instead of hanging on a prompt.
fn turn_message(argument: Option<String>, required: bool) -> Result<Option<String>> {
    if let Some(message) = argument {
        return non_empty(message).map(Some);
    }
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return if required {
            bail!("pass a message or pipe one on stdin")
        } else {
            Ok(None)
        };
    }
    let mut message = String::new();
    stdin
        .lock()
        .read_to_string(&mut message)
        .wrap_err("read message from stdin")?;
    // `echo text | centaur threads send ...` should not send the newline.
    let message = message.trim_end_matches(['\n', '\r']).to_owned();
    if message.trim().is_empty() && !required {
        return Ok(None);
    }
    non_empty(message).map(Some)
}

fn non_empty(message: String) -> Result<String> {
    if message.trim().is_empty() {
        bail!("message is empty");
    }
    Ok(message)
}

/// Turns API failures into the message api-rs sent, with a login hint for 401.
fn api_error(error: ClientError) -> eyre::Report {
    match error {
        ClientError::Api { status, body } => {
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or(body);
            match status.as_u16() {
                401 => eyre!("{message} (HTTP 401); run `centaur login`"),
                403 => eyre!("not allowed: {message} (HTTP 403)"),
                404 => eyre!("not found: {message} (HTTP 404)"),
                _ => eyre!("{message} (HTTP {status})"),
            }
        }
        error => eyre::Report::new(error),
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_table<const N: usize>(headers: [&str; N], rows: &[[String; N]]) {
    let mut widths = headers.map(str::len);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: [&str; N]| {
        let mut out = String::new();
        for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
            if index + 1 == N {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:<width$}  "));
            }
        }
        println!("{}", out.trim_end());
    };
    line(headers);
    for row in rows {
        line(std::array::from_fn(|index| row[index].as_str()));
    }
}

fn timestamp(now: OffsetDateTime, at: OffsetDateTime) -> String {
    let formatted = at.format(&Rfc3339).unwrap_or_else(|_| at.to_string());
    format!("{formatted} ({})", ago(now, at))
}

fn ago(now: OffsetDateTime, at: OffsetDateTime) -> String {
    let seconds = (now - at).whole_seconds();
    match seconds {
        ..60 => "just now".to_owned(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut truncated: String = value.chars().take(max_chars - 1).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use time::Duration;

    use super::*;

    #[test]
    fn user_input_line_is_one_ndjson_line() {
        let line = user_input_line("hi\nthere").unwrap();
        assert!(!line.contains('\n'));
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["message"]["content"][0]["text"], "hi\nthere");
    }

    #[test]
    fn ages_read_at_a_glance() {
        let now = OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap();
        assert_eq!(ago(now, now - Duration::seconds(5)), "just now");
        assert_eq!(ago(now, now - Duration::minutes(7)), "7m ago");
        assert_eq!(ago(now, now - Duration::hours(3)), "3h ago");
        assert_eq!(ago(now, now - Duration::days(2)), "2d ago");
        // Clock skew can put a timestamp slightly in the future.
        assert_eq!(ago(now, now + Duration::seconds(3)), "just now");
    }

    #[test]
    fn truncates_long_titles_on_character_boundaries() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("ééééééééééé", 5), "éééé…");
    }

    #[test]
    fn api_errors_surface_the_server_message() {
        let error = api_error(ClientError::Api {
            status: reqwest::StatusCode::FORBIDDEN,
            body: r#"{"ok":false,"error":"caller is not authorized for this session"}"#.to_owned(),
        });
        assert_eq!(
            error.to_string(),
            "not allowed: caller is not authorized for this session (HTTP 403)"
        );
        let error = api_error(ClientError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "nope".to_owned(),
        });
        assert!(error.to_string().ends_with("run `centaur login`"));
    }

    #[test]
    fn failure_reason_prefers_the_payload_error() {
        assert_eq!(
            failure_reason(r#"{"execution_id":"e","error":"sandbox died"}"#),
            "sandbox died"
        );
        assert_eq!(failure_reason("not json"), "not json");
    }
}
