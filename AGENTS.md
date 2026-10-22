# Working in this repository

Agent Task Scheduler (`ats`) queues resource-heavy commands from AI agents and admits them by machine capacity. It is one Rust crate that builds one binary, `ats`; the daemon is its `daemon` subcommand.

## Layout

- `src/main.rs`: command-line entry point.
- `src/daemon/`: daemon lifecycle, the scheduling loop, the executor watch and the pressure response.
- `src/api/`: the HTTP/JSON API, served on a unix socket and through the request-file channel (`src/channel.rs`).
- `src/client.rs`: how commands reach the daemon.
- `src/executor/`: the per-job process that waits for its lease and runs the command.
- `src/hook.rs`: the agent hook that rewrites shell commands into `ats run`.
- `src/recognize/`, `src/profile.rs`, `profiles/builtin.toml`: command recognition and tool profiles.
- `src/sched/`: admission, prediction, locks and pressure as pure functions.
- `src/sample/`: memory measurement.
- `src/store/`: the SQLite store and its schema.
- `api/openapi.yaml`: the API description shared by the CLI, executors and hooks.

ats runs on macOS only.

## Code

- Rust 2024 edition. Async code runs on tokio.
- Write comments, identifiers, commit messages and docs in English.
- Handle a failure where it can happen and where something needs the handling; add fallbacks, retries or extra validation only for a named requirement or a reachable failure.
- Tests are table-driven where several cases share a shape. Tests use temporary directories and never touch the user's real ats data, agent configuration or the network. Set `ATS_HOME` to point a daemon at a separate data directory.

## Checks

Run before every commit:

```sh
scripts/check.sh
```

It runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`.
