//! `ats`: Agent Task Scheduler. Queues resource-heavy commands from AI agents
//! and admits them by machine capacity.

#[cfg(not(target_os = "macos"))]
compile_error!("ats supports macOS only");

mod api;
mod channel;
mod cli;
mod client;
mod config;
mod control;
mod daemon;
mod executor;
mod ids;
mod paths;
mod procinfo;
mod profile;
mod recognize;
mod redact;
mod runspec;
mod sample;
mod sched;
mod store;
mod timing;

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ats", version, about = "Agent Task Scheduler")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// Parsed once per process, so the size of `Run` does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground.
    Daemon,
    /// Queue a command and run it when admitted.
    ///
    /// Returns the command's own output and exit code when its main process
    /// finishes within the wait; otherwise prints one status line and exits
    /// with 75 while the job continues. 76: a predecessor failed;
    /// 77: rejected; 78: ats error.
    Run {
        /// Shell source, run by the submitter's shell.
        #[arg(long, conflicts_with = "argv")]
        sh: Option<String>,
        /// How long to wait for the result, including system sleep.
        #[arg(long, value_parser = humantime::parse_duration, default_value = "90s", conflicts_with = "no_wait")]
        wait: Duration,
        /// Return at once with the status line.
        #[arg(long)]
        no_wait: bool,
        /// The agent session, added by its hook.
        #[arg(long, hide = true)]
        session: Option<String>,
        #[command(flatten)]
        options: cli::RunOptions,
        /// The program and its arguments, run without a shell.
        #[arg(last = true, required_unless_present = "sh")]
        argv: Vec<String>,
    },
    /// Wait for a submission's job to end and report its state.
    Wait {
        submission: String,
        /// How long to wait, including system sleep.
        #[arg(long, value_parser = humantime::parse_duration, default_value = "90s")]
        wait: Duration,
    },
    /// Show jobs, or one submission or job.
    Status {
        /// A submission or job id.
        id: Option<String>,
        /// Only jobs in these states, comma-separated.
        #[arg(long, conflicts_with = "id")]
        state: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print a submission's or job's output.
    Logs {
        id: String,
        /// Keep printing until the job ends.
        #[arg(short, long)]
        follow: bool,
    },
    /// Explain why a job is still waiting.
    Why { id: String },
    /// Run a job whose predecessor failed.
    Unblock { id: String },
    /// Cancel a submission; the job stops when no other submission wants it.
    Cancel { submission: String },
    /// Mark a submission as left to run in the background.
    Detach { submission: String },
    /// Show how a command line is recognized, as JSON.
    #[command(name = "__analyze", hide = true)]
    Analyze {
        #[arg(long, conflicts_with = "argv")]
        sh: Option<String>,
        #[arg(last = true, required_unless_present = "sh")]
        argv: Vec<String>,
    },
    /// Run one job's executor (started by `ats run`).
    #[command(name = "__executor", hide = true)]
    Executor {
        #[arg(long)]
        job: String,
        #[arg(long)]
        spec: String,
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
        Command::Run {
            sh,
            wait,
            no_wait,
            session,
            options,
            argv,
        } => {
            let spec = match sh {
                Some(source) => runspec::RunSpec::Sh {
                    source,
                    shell: runspec::RunSpec::default_shell(),
                },
                None => runspec::RunSpec::Argv { argv },
            };
            let wait = if no_wait { Duration::ZERO } else { wait };
            cli::run(&paths, spec, Some(wait), session, options).await
        }
        Command::Wait { submission, wait } => cli::wait(&paths, &submission, Some(wait)).await,
        Command::Status {
            id: Some(id), json, ..
        } => cli::status_one(&paths, &id, json).await,
        Command::Logs { id, follow } => cli::logs(&paths, &id, follow).await,
        Command::Why { id } => cli::why(&paths, &id).await,
        Command::Unblock { id } => cli::unblock(&paths, &id).await,
        Command::Cancel { submission } => cli::mark(&paths, &submission, "cancel").await,
        Command::Detach { submission } => cli::mark(&paths, &submission, "detach").await,
        Command::Status { state, json, .. } => {
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
        Command::Analyze { sh, argv } => {
            let config = config::Config::load(&paths)?;
            let profiles = profile::Profiles::load(Some(&paths.profiles))?;
            let recognizer = recognize::Recognizer::new(&profiles, &config.recognize.wrappers);
            let cwd = std::env::current_dir()?;
            let analysis = match sh {
                Some(source) => recognizer.shell(&source, &cwd),
                None => recognizer.argv(&argv, &cwd),
            };
            println!("{}", serde_json::to_string_pretty(&analysis)?);
            Ok(ExitCode::SUCCESS)
        }
        Command::Executor { job, spec } => {
            let spec: runspec::RunSpec = serde_json::from_str(&spec)?;
            executor::run(paths, job.clone(), spec, std::env::current_dir()?).await?;
            let _ = std::fs::remove_file(executor::error_log(&job));
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
