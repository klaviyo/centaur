//! `centaur`: the command line for managing Centaur threads.
//!
//! Commands are declared here ahead of their implementations so the command
//! surface can be reviewed with `centaur --help`. Unimplemented commands exit
//! with an error.

use std::{ffi::OsString, process::ExitCode, time::Duration};

use centaur_session_core::{HarnessType, ThreadKey};
use clap::{Args, Parser, Subcommand};
use eyre::{Result, bail};
use url::Url;

mod auth;
mod oauth;
mod store;
mod threads;

#[derive(Debug, Parser)]
#[command(name = "centaur", version, about = "Manage Centaur threads")]
struct Cli {
    /// Named login to use, so one machine can hold several deployments.
    #[arg(
        long,
        global = true,
        env = "CENTAUR_PROFILE",
        default_value = "default"
    )]
    profile: String,

    /// api-rs base URL. Defaults to the profile's URL.
    #[arg(long, global = true, env = "CENTAUR_API_URL")]
    api_url: Option<Url>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Sign in through Console in a browser.
    Login {
        /// Console base URL, when api-rs does not advertise one.
        #[arg(long)]
        authorization_server: Option<Url>,
        /// Print the sign-in URL without opening a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Revoke the stored login and forget it.
    Logout,
    /// Show who api-rs thinks you are.
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// List, inspect, and drive threads.
    #[command(subcommand)]
    Threads(ThreadsCommand),
    /// Open the harness's native UI on an existing thread.
    Connect {
        thread_key: String,
        /// Extra native harness options, after `--`.
        #[arg(last = true, allow_hyphen_values = true)]
        harness_args: Vec<OsString>,
    },
}

#[derive(Debug, Subcommand)]
enum ThreadsCommand {
    /// List threads you can see, most recently updated first.
    List {
        /// Only threads whose key starts with this (e.g. `slack:` or `api:`).
        #[arg(long)]
        prefix: Option<String>,
        /// How many threads to show (the server caps this at 200).
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Show a thread's harness, status, and latest run.
    Show {
        thread_key: ThreadKey,
        #[arg(long)]
        json: bool,
    },
    /// Start a new thread and, given a message, run its first turn. Prints the
    /// thread key, which the server assigns.
    New {
        /// Harness to run the thread on: codex, claudecode, amp, nanocodex,
        /// hermes, pi, or omp.
        #[arg(long, default_value = "codex")]
        harness: HarnessType,
        #[command(flatten)]
        turn: TurnArgs,
    },
    /// Send a message to a thread and run a turn.
    Send {
        thread_key: ThreadKey,
        #[command(flatten)]
        turn: TurnArgs,
    },
    /// Interrupt a thread's running turn.
    Interrupt {
        thread_key: ThreadKey,
        /// Recorded with the interrupt; defaults to naming this CLI.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Pause a thread's idle sandbox. Needs an operator credential.
    Pause { thread_key: ThreadKey },
}

/// A user message and how to wait for the turn it starts.
#[derive(Debug, Args)]
struct TurnArgs {
    /// Message text; read from stdin when omitted and stdin is piped.
    message: Option<String>,
    /// Follow the turn, printing the harness's raw output lines (NDJSON) to
    /// stdout, and exit when it finishes. Exits non-zero if it fails.
    #[arg(long)]
    wait: bool,
    /// Print the started execution as JSON instead of a status line. With
    /// `new` and no message, print the created thread as JSON.
    #[arg(long, conflicts_with = "wait")]
    json: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The cause chain on one line; eyre's default report adds source
            // locations, which mean nothing to someone running the CLI.
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("centaur-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()?;
    match cli.command {
        Command::Login {
            authorization_server,
            no_browser,
        } => {
            auth::login(
                &http,
                &cli.profile,
                cli.api_url,
                authorization_server,
                no_browser,
            )
            .await
        }
        Command::Logout => auth::logout(&http, &cli.profile).await,
        Command::Whoami { json } => auth::whoami(&http, &cli.profile, cli.api_url, json).await,
        Command::Threads(command) => threads::run(&http, &cli.profile, cli.api_url, command).await,
        command => bail!(
            "`centaur {}` is not implemented yet",
            command_name(&command)
        ),
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Login { .. } => "login",
        Command::Logout => "logout",
        Command::Whoami { .. } => "whoami",
        Command::Threads(_) => "threads",
        Command::Connect { .. } => "connect",
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_every_auth_command() {
        for args in [
            vec!["centaur", "login", "--api-url", "https://api.example.com"],
            vec!["centaur", "login", "--no-browser"],
            vec!["centaur", "logout"],
            vec!["centaur", "--profile", "staging", "whoami", "--json"],
        ] {
            Cli::try_parse_from(args).unwrap();
        }
    }

    #[test]
    fn parses_every_threads_command() {
        for args in [
            vec![
                "centaur", "threads", "list", "--prefix", "cli:", "--limit", "5", "--json",
            ],
            vec!["centaur", "threads", "show", "slack:C123:123.456"],
            vec!["centaur", "threads", "new"],
            vec![
                "centaur",
                "threads",
                "new",
                "--harness",
                "claudecode",
                "hello",
                "--wait",
            ],
            vec!["centaur", "threads", "send", "cli:abc", "hello", "--json"],
            vec![
                "centaur",
                "threads",
                "interrupt",
                "cli:abc",
                "--reason",
                "stop",
            ],
            vec!["centaur", "threads", "pause", "cli:abc"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|error| panic!("{args:?}: {error}"));
        }
    }

    #[test]
    fn rejects_unknown_harnesses_and_invalid_thread_keys() {
        assert!(Cli::try_parse_from(["centaur", "threads", "new", "--harness", "gpt"]).is_err());
        assert!(Cli::try_parse_from(["centaur", "threads", "show", ""]).is_err());
        // One stdout format per run: a JSON document or the turn's NDJSON.
        assert!(
            Cli::try_parse_from([
                "centaur", "threads", "send", "cli:a", "hi", "--json", "--wait"
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_connect_passthrough_args() {
        let cli = Cli::try_parse_from([
            "centaur",
            "connect",
            "slack:C123:123.456",
            "--",
            "--no-alt-screen",
        ])
        .unwrap();
        let Command::Connect {
            thread_key,
            harness_args,
        } = cli.command
        else {
            panic!("expected connect");
        };
        assert_eq!(thread_key, "slack:C123:123.456");
        assert_eq!(harness_args, vec![OsString::from("--no-alt-screen")]);
    }
}
