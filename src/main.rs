//! `ats`: Agent Task Scheduler. Queues resource-heavy commands from AI agents
//! and admits them by machine capacity.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("ats supports macOS and Linux");

mod api;
mod channel;
mod cli;
mod client;
mod config;
mod control;
mod daemon;
mod executor;
mod hook;
mod ids;
mod install;
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

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// How install and uninstall confirm their changes.
#[derive(clap::Args)]
struct ConfirmArgs {
    /// Show the changes and write nothing.
    #[arg(long, conflicts_with = "yes")]
    dry_run: bool,
    /// Write the changes without a question, after the user agreed to the
    /// changes `--dry-run` showed.
    #[arg(long)]
    yes: bool,
}

impl ConfirmArgs {
    fn mode<'a>(&self, input: &'a mut dyn std::io::BufRead) -> install::Confirm<'a> {
        if self.dry_run {
            install::Confirm::DryRun
        } else if self.yes {
            install::Confirm::Yes
        } else {
            install::Confirm::Ask(input)
        }
    }
}

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
        /// The agent session; by default the one the agent sets in the
        /// environment.
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
    /// Leave a submission running in the background: the agent may stop
    /// without waiting for it, and still hears when it ends.
    Detach { submission: String },
    /// Add ats's hooks and instructions to Claude Code and Codex and start
    /// the daemon at login, after showing the changes and asking.
    Install {
        /// Only this agent; by default every agent set up on this machine.
        #[arg(long, conflicts_with = "service")]
        agent: Vec<hook::Agent>,
        /// Only start the daemon at login, for agents other than Claude
        /// Code and Codex; add `ats instructions` to their instructions.
        #[arg(long)]
        service: bool,
        #[command(flatten)]
        confirm: ConfirmArgs,
    },
    /// Remove what `ats install` added and put back the hooks it moved.
    /// Without `--agent`, the daemon no longer starts at login.
    Uninstall {
        #[arg(long)]
        agent: Vec<hook::Agent>,
        #[command(flatten)]
        confirm: ConfirmArgs,
    },
    /// Print the instructions that tell an agent which commands to send to
    /// ats, between markers, for its instructions file.
    Instructions,
    /// Answer an agent hook: the hook input on stdin, the answer on stdout.
    Hook {
        agent: hook::Agent,
        event: hook::Event,
    },
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
    // Commands that print stop quietly when their reader goes away, as in
    // `ats status | head`. The daemon and executors write to sockets and
    // must not be killed by a closed one.
    if !matches!(cli.command, Command::Daemon | Command::Executor { .. }) {
        // SAFETY: sets the default action before any thread starts.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
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
                // The login service sends stderr to a file or the journal.
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
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
            // Inside a job the command runs directly: queueing it would
            // wait on the job that is already holding its memory (design
            // 6.3). Other direct commands still queue when explicit
            // dependencies or locks need the scheduler.
            if std::env::var_os(executor::JOB_ENV).is_some()
                || (options.after.is_empty()
                    && options.lock.is_empty()
                    && runs_directly(&paths, &spec)?)
            {
                use std::os::unix::process::CommandExt;
                return Err(spec.command().exec().into());
            }
            let wait = if no_wait { Duration::ZERO } else { wait };
            let session = session.or_else(agent_session);
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
        Command::Install {
            agent,
            service,
            confirm,
        } => {
            let home = dirs::home_dir().context("no home directory")?;
            let agents = if service {
                Vec::new()
            } else {
                install::agents(&home, &agent)
            };
            if agents.is_empty() && !service {
                anyhow::bail!(
                    "found neither ~/.claude nor ~/.codex; name an agent with --agent, or use --service"
                );
            }
            let ats = std::env::current_exe().context("locating the ats binary")?;
            install::install(
                &paths,
                &home,
                &agents,
                &ats,
                true,
                confirm.mode(&mut std::io::stdin().lock()),
                &mut std::io::stdout(),
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Instructions => {
            print!("{}", install::instructions_block());
            Ok(ExitCode::SUCCESS)
        }
        Command::Uninstall { agent, confirm } => {
            let home = dirs::home_dir().context("no home directory")?;
            let agents = install::agents(&home, &agent);
            install::uninstall(
                &paths,
                &home,
                &agents,
                agent.is_empty(),
                true,
                confirm.mode(&mut std::io::stdin().lock()),
                &mut std::io::stdout(),
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Hook { event, .. } => {
            if let hook::Answer::Json(out) = hook::run(&paths, event).await? {
                println!("{out}");
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
            let error_log = executor::error_log(&paths, &job);
            executor::run(paths, job.clone(), spec, std::env::current_dir()?).await?;
            let _ = std::fs::remove_file(error_log);
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Whether the command is light, interactive or a service (design 8.2).
fn runs_directly(paths: &paths::Paths, spec: &runspec::RunSpec) -> Result<bool> {
    let config = config::Config::load(paths)?;
    let profiles = profile::Profiles::load(Some(&paths.profiles))?;
    let recognizer = recognize::Recognizer::new(&profiles, &config.recognize.wrappers);
    let cwd = std::env::current_dir()?;
    let analysis = match spec {
        runspec::RunSpec::Sh { source, .. } => recognizer.shell(source, &cwd),
        runspec::RunSpec::Argv { argv } => recognizer.argv(argv, &cwd),
    };
    Ok(analysis.light || analysis.interactive || analysis.segments.iter().any(|s| s.service))
}

/// The agent session the command comes from, as the agent sets it in the
/// environment of its shell commands (design 6.2). An agent started from
/// another agent's command also inherits the outer agent's variable;
/// Codex is checked first, which is right for Codex started from Claude
/// Code.
fn agent_session() -> Option<String> {
    ["CODEX_SESSION_ID", "CLAUDE_CODE_SESSION_ID"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
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
