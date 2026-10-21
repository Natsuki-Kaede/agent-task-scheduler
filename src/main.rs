//! `ats`: Agent Task Scheduler. Queues resource-heavy commands from AI agents
//! and admits them by machine capacity.

mod api;
mod channel;
mod client;
mod daemon;
mod ids;
mod paths;
mod redact;
mod store;

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ats", version, about = "Agent Task Scheduler")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground.
    Daemon,
    /// Show jobs.
    Status {
        /// Only jobs in these states, comma-separated.
        #[arg(long)]
        state: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Runtime::new().expect("starting the async runtime");
    match runtime.block_on(dispatch(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ats: {e:#}");
            ExitCode::from(78)
        }
    }
}

async fn dispatch(cli: Cli) -> Result<ExitCode> {
    let paths = paths::Paths::from_env()?;
    match cli.command {
        Command::Daemon => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .init();
            daemon::run(paths, daemon::shutdown_signal()).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Status { state, json } => {
            let path = match state {
                Some(s) => format!("/v1/jobs?state={s}"),
                None => "/v1/jobs".to_string(),
            };
            let (answer, _) = client::call(&paths, "GET", &path, None).await?;
            if answer.status != 200 {
                anyhow::bail!("daemon answered {}: {}", answer.status, answer.body);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&answer.body)?);
            } else {
                print_jobs(&answer.body);
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn print_jobs(jobs: &serde_json::Value) {
    let Some(list) = jobs.as_array() else {
        println!("{jobs}");
        return;
    };
    if list.is_empty() {
        println!("no jobs");
    }
    for job in list {
        println!(
            "{:<24} {:<12} {}",
            job["id"].as_str().unwrap_or("?"),
            job["state"].as_str().unwrap_or("?"),
            job["command"].as_str().unwrap_or("")
        );
    }
}
