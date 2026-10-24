# Agent Task Scheduler (ats)

When several AI agent sessions work on one Mac, each one builds, tests and runs models as if it had the machine to itself. Memory runs out, the system swaps, and everything slows down together until commands time out.

`ats` sits between the agents and the machine. Agent hooks hand each shell command with real load to `ats run`; a local daemon starts it when the machine has memory for it and tells it how many parallel jobs to use. The command runs in the agent's own environment, and its output and exit code come back unchanged.

- **Admission by memory.** A command starts when its predicted peak memory fits the headroom left by the system and by jobs already running. CPU is left to the operating system; `ats` only sets each job's parallelism.
- **Learns from use.** Each run records its measured memory peak and duration, and the next run of the same command is predicted from that history.
- **Steps back under pressure.** When the system starts swapping, nothing new starts and running jobs move to background priority until it calms down. Jobs are never paused or killed.
- **Light commands pass through.** `ls`, `git rev-parse`, editors and other interactive or quick commands run directly.

macOS on Apple Silicon. Claude Code and Codex are supported.

## Install

```sh
brew install natsuki-kaede/tap/ats
ats install
```

Or from source, with a Rust toolchain:

```sh
cargo install --locked --git https://github.com/Natsuki-Kaede/agent-task-scheduler
ats install
```

`ats install` shows every change as a diff and asks before writing:

- hooks in `~/.claude/settings.json` and `~/.codex/hooks.json` that rewrite shell commands into `ats run` and remind the agent of commands still running;
- a short section in `~/.claude/CLAUDE.md` and `~/.codex/AGENTS.md` that explains `ats` to the agent;
- a LaunchAgent that starts the daemon at login.

Other hooks that rewrite shell commands can run inside `ats`'s hook; `ats install` asks about each one. Codex asks you to trust the changed hooks the next time it starts.

`ats uninstall` removes all of it and puts back what it moved, after the same diff and question.

## Use

The agent keeps running commands as before. A command that finishes within 90 seconds returns as usual. One that takes longer prints a line such as

```
[ats] submission s_x job j_y is running; log: ...; check with: ats wait s_x
```

and exits with 75 while the command continues. Other exit codes from `ats` itself: 76 when a predecessor given with `--after` failed, 77 when the submission was rejected, 78 when `ats` failed.

| Command | |
|---|---|
| `ats run [options] -- <program> [args...]` | Run a program when admitted |
| `ats run [options] --sh '<shell source>'` | Run shell source with your shell |
| `ats wait <id>` | Wait for a submission to end and report it |
| `ats status [<id>]` | Show jobs, or one submission or job |
| `ats logs <id> [-f]` | Print or follow a job's output |
| `ats why <id>` | Explain why a job is still waiting |
| `ats cancel <id>` / `ats detach <id>` | Stop a submission, or let the agent stop without waiting for it; the agent still hears when it ends |
| `ats unblock <id>` | Run a job whose predecessor failed |

Options of `ats run`: `--wait <duration>` (default 90s) or `--no-wait`; `--after <id,...>` to run after other jobs succeed; `--lock <path or name>` for a resource only one job may use at a time; `--hint mem=6G` when you know the command needs more memory than predicted; `--now` to skip the queue.

## Configuration

Files live in `~/Library/Application Support/ats/`.

`config.toml` lists programs that run the rest of their arguments as a command, so `ats` recognizes the command inside:

```toml
[recognize]
wrappers = ["mytool"]
```

Tool profiles say how to recognize a tool, how much memory it typically needs, which paths it writes and how to set its parallelism. Files in `profiles.d/` add profiles or replace built-in ones by name; see `profiles/builtin.toml` for the format.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
