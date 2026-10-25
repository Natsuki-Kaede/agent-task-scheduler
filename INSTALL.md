# Install ats with an AI agent

This document is for an AI coding agent. The user asked you to install Agent Task Scheduler (`ats`). Do the steps in sequence.

Some steps change files outside the current project. Before each of these steps, show the user the changes and ask. Do not continue without the agreement of the user.

## 1. Check the machine

Run `uname -sm`. The output must be `Darwin arm64`. If the output is different, stop. Tell the user that ats runs only on macOS with Apple Silicon.

## 2. Install the binary

If `ats --version` prints a version, go to step 3.

If `brew` is available, run this command:

```sh
brew install natsuki-kaede/tap/ats
```

If `brew` is not available and `cargo` is available, run this command:

```sh
cargo install --locked --git https://github.com/Natsuki-Kaede/agent-task-scheduler
```

If neither `brew` nor `cargo` is available, stop. Tell the user to install Homebrew or a Rust toolchain.

Run `ats --version`. If the shell does not find `ats`, tell the user to add the install directory to `PATH`: `$(brew --prefix)/bin` or `~/.cargo/bin`.

## 3. Connect your agent

`ats install` writes nothing when you give it `--dry-run`. It writes without a question when you give it `--yes`.

### Claude Code or Codex

1. Run `ats install --agent claude --dry-run`. For Codex, use `--agent codex`. The command shows the changes as diffs.
2. Show the diffs to the user and ask.
3. If the user agrees, run the same command with `--yes` in place of `--dry-run`.

This adds the instructions for the model, two reminder hooks, and a LaunchAgent that starts the daemon at login.

### A different agent

1. Run `ats install --service --dry-run`. Show the diff to the user and ask. If the user agrees, run `ats install --service --yes`. This starts the daemon at login.
2. Run `ats instructions`. It prints a section between `<!-- ats:begin -->` and `<!-- ats:end -->`.
3. Find the file with your global instructions, for example `~/.config/opencode/AGENTS.md` for OpenCode or `~/.gemini/GEMINI.md` for Gemini CLI. Tell the user that you will add the section at the end of this file, and ask. If the user agrees, add the section.

If `launchctl` fails because your sandbox blocks it, tell the user to run the same `ats install` command in a terminal.

## 4. Check the result

Run `ats status`. It must print `no jobs` or a list of jobs. If it prints an error, show the error to the user.

Tell the user these items:

- which files changed;
- that new agent sessions read the instructions, and sessions that run now do not;
- that Codex asks to trust the changed hooks when it starts the next time;
- that `ats uninstall` removes the changes of `ats install`. For a different agent, the user also removes the section between the two markers from the instructions file.
