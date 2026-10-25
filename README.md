# Agent Task Scheduler (ats)

Several AI agent sessions on one Mac can build, test and run models at the same time. Each session works as if it has the machine for itself. Then memory runs out, the system swaps, and all commands become slow until they time out.

`ats` is a local queue for heavy shell commands. An agent sends a heavy command to `ats run`. A daemon starts the command when the machine has memory for it, and tells the command how many parallel jobs to use. The command runs in the environment of the agent. Its output and exit code come back unchanged.

- **Admission by memory.** A command starts when its predicted peak memory fits in the free memory. CPU stays with the operating system. `ats` only sets the parallelism of each job.
- **Predictions from use.** Each run records its measured memory peak and duration. The next run of the same command uses these values.
- **Less load under pressure.** When the system starts to swap, `ats` starts no new commands and gives running jobs background priority. It does not pause or stop jobs.
- **The agent decides.** Instructions tell the model which commands to send to `ats`. Light commands run directly, so they cost nothing more.

`ats` runs on macOS with Apple Silicon. It works with any agent that can run shell commands. The reminder hooks are for Claude Code and Codex.

## Install

Give this prompt to your AI coding agent, for example Claude Code, Codex, OpenCode or Gemini CLI:

```text
Install Agent Task Scheduler (ats) for me. Read https://raw.githubusercontent.com/Natsuki-Kaede/agent-task-scheduler/main/INSTALL.md and do the steps. Ask me before you change files outside this project.
```

The agent installs the binary and shows you the changes. It writes them when you agree.

To install by hand, run these commands:

```sh
brew install natsuki-kaede/tap/ats
ats install
```

To build from source, install a Rust toolchain. Then use `cargo install --locked --git https://github.com/Natsuki-Kaede/agent-task-scheduler` in place of `brew install`.

`ats install` shows each change as a diff. It writes nothing before you agree:

- instructions for the model: `~/.claude/ats.md` with an `@ats.md` line in `~/.claude/CLAUDE.md`, and a section in `~/.codex/AGENTS.md`;
- two reminder hooks in `~/.claude/settings.json` and `~/.codex/hooks.json`, which tell the agent about commands that still run or that ended;
- a LaunchAgent that starts the daemon at login.

For other agents, `ats install --service` adds only the LaunchAgent. `ats instructions` prints the instructions, which you add to the instructions file of the agent.

Codex asks you to trust the changed hooks when it starts the next time.

`ats uninstall` removes these items after the same diff and question.

## Use

The model writes a heavy command as `ats run -- cargo build` or `ats run --sh 'npm run build'`. When the command ends in 90 seconds or less, it gives its output as usual. When it runs longer, `ats` prints a line such as

```
[ats] submission s_x job j_y is running; log: ...; check with: ats wait s_x
```

and exits with code 75. The command continues in the background. Other exit codes of `ats`: 76 when a predecessor given with `--after` failed, 77 when `ats` did not accept the submission, 78 when `ats` failed.

| Command | |
|---|---|
| `ats run [options] -- <program> [args...]` | Run a program when the machine has memory for it |
| `ats run [options] --sh '<shell source>'` | Run shell source with your shell |
| `ats wait <id>` | Wait for a submission to end, then give its output and exit code |
| `ats status [<id>]` | Show jobs, or one submission or job |
| `ats logs <id> [-f]` | Show the output of a job, or follow it |
| `ats why <id>` | Show why a job waits |
| `ats cancel <id>` | Stop a submission |
| `ats detach <id>` | Let the agent stop without a wait for the submission; the agent is told when it ends |
| `ats unblock <id>` | Run a job whose predecessor failed |

Options of `ats run`: `--wait <duration>` (default 90s) or `--no-wait`; `--after <id,...>` to run after other jobs succeed; `--lock <path or name>` for a resource that only one job can use at a time; `--hint mem=6G` when the command needs more memory than the prediction; `--now` to start without the queue.

In Claude Code permission modes that ask before shell commands, each `ats run` asks too. Do not add an allow rule such as `Bash(ats run:*)`, because it allows any command.

## Configuration

The files are in `~/Library/Application Support/ats/`.

`config.toml` lists programs that run the rest of their arguments as a command. With this list, `ats` recognizes the command in them:

```toml
[recognize]
wrappers = ["mytool"]
```

A tool profile tells how to recognize a tool, how much memory it usually needs, which paths it writes, and how to set its parallelism. Files in `profiles.d/` add profiles or replace built-in profiles of the same name. For the format, see `profiles/builtin.toml`.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
