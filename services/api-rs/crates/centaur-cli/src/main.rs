//! `centaur`: the command line for managing Centaur threads.
//!
//! Commands are declared here ahead of their implementations so the command
//! surface can be reviewed with `centaur --help`. Unimplemented commands exit
//! with an error.

use std::{ffi::OsString, time::Duration};

use clap::{Args, Parser, Subcommand};
use eyre::{Result, bail};
use url::Url;

mod auth;
mod oauth;
mod store;

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
    /// List threads you can see.
    List {
        /// Only threads from this platform (slack, github, linear, ...).
        #[arg(long)]
        platform: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Show a thread's session, sandbox, and latest execution.
    Show {
        thread_key: String,
        #[arg(long)]
        json: bool,
    },
    /// Start a new thread.
    New {
        /// Harness to run the thread on (codex, claude-code, pi, ...).
        #[arg(long)]
        harness: Option<String>,
        /// First message; read from stdin when omitted.
        #[arg(long)]
        message: Option<String>,
    },
    /// Send a message to a thread and run a turn.
    Send(SendArgs),
    /// Interrupt a thread's running execution.
    Interrupt {
        thread_key: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Pause a thread's idle sandbox.
    Pause { thread_key: String },
}

#[derive(Debug, Args)]
struct SendArgs {
    thread_key: String,
    /// Message text; read from stdin when omitted.
    message: Option<String>,
    /// Wait for the turn to finish and print the final reply.
    #[arg(long)]
    wait: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
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
        Command::Connect { .. } => "connect",
        Command::Threads(threads) => match threads {
            ThreadsCommand::List { .. } => "threads list",
            ThreadsCommand::Show { .. } => "threads show",
            ThreadsCommand::New { .. } => "threads new",
            ThreadsCommand::Send(_) => "threads send",
            ThreadsCommand::Interrupt { .. } => "threads interrupt",
            ThreadsCommand::Pause { .. } => "threads pause",
        },
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
