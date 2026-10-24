-- Schema version 1 (design 11). Only the daemon opens this database.

CREATE TABLE jobs (
    id                  TEXT PRIMARY KEY,
    state               TEXT NOT NULL,
    cwd                 TEXT NOT NULL,
    project_root        TEXT NOT NULL,
    entry               TEXT NOT NULL CHECK (entry IN ('argv', 'sh')),
    command             TEXT NOT NULL,      -- redacted display form
    profile             TEXT,               -- JSON list of matched profile names
    history_key         TEXT,               -- tool and subcommand of a one-command line
    predicted_private   INTEGER NOT NULL DEFAULT 0,
    predicted_seconds   REAL,
    run_now             INTEGER NOT NULL DEFAULT 0, -- submitted with --now
    parallel_env        TEXT,               -- JSON list of variables set to the parallelism
    jobserver           INTEGER NOT NULL DEFAULT 0, -- offer a jobserver pool
    reserved_private    INTEGER NOT NULL DEFAULT 0,
    exit_code           INTEGER,
    main_exit_code      INTEGER,            -- main process result; the job may still run
    peak_private        INTEGER,
    log_path            TEXT,
    executor_pid        INTEGER,
    executor_start      TEXT,               -- process start time; with the pid it identifies the executor
    pgid                INTEGER,            -- process group of the command
    launcher_pid        INTEGER,            -- the CLI that starts the executor
    launcher_start      TEXT,
    created_at          TEXT NOT NULL,
    started_at          TEXT,
    ended_at            TEXT
);
CREATE INDEX jobs_state ON jobs(state);
CREATE INDEX jobs_line ON jobs(project_root, command);
CREATE INDEX jobs_history_key ON jobs(history_key);

CREATE TABLE submissions (
    id                  TEXT PRIMARY KEY,
    job_id              TEXT NOT NULL REFERENCES jobs(id),
    session_id          TEXT,               -- NULL for anonymous submissions
    canceled            INTEGER NOT NULL DEFAULT 0,
    detached            INTEGER NOT NULL DEFAULT 0, -- Stop does not wait for it
    quiet               INTEGER NOT NULL DEFAULT 0, -- its end is not reported
    created_at          TEXT NOT NULL
);
CREATE INDEX submissions_job ON submissions(job_id);
CREATE INDEX submissions_session ON submissions(session_id);

CREATE TABLE dependencies (
    job_id              TEXT NOT NULL REFERENCES jobs(id),
    after_job_id        TEXT NOT NULL REFERENCES jobs(id),
    PRIMARY KEY (job_id, after_job_id)
);

CREATE TABLE leases (
    job_id              TEXT PRIMARY KEY REFERENCES jobs(id),
    granted_at          TEXT NOT NULL,
    parallelism         INTEGER NOT NULL,
    jobserver_tokens    INTEGER
);

CREATE TABLE locks (
    job_id              TEXT NOT NULL REFERENCES jobs(id),
    resource            TEXT NOT NULL,      -- normalized path or named resource
    kind                TEXT NOT NULL CHECK (kind IN ('path', 'named')),
    PRIMARY KEY (job_id, resource)
);

CREATE TABLE job_events (
    job_id              TEXT NOT NULL REFERENCES jobs(id),
    at                  TEXT NOT NULL,
    kind                TEXT NOT NULL,
    detail              TEXT
);
CREATE INDEX job_events_job ON job_events(job_id);
