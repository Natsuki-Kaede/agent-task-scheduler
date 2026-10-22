-- Schema version 1 (design 13). Only the daemon opens this database.
-- Executor environment snapshots and API keys are never stored here.

CREATE TABLE jobs (
    id                  TEXT PRIMARY KEY,
    node_id             TEXT NOT NULL,
    state               TEXT NOT NULL,
    cwd                 TEXT NOT NULL,
    project_root        TEXT NOT NULL,
    entry               TEXT NOT NULL CHECK (entry IN ('argv', 'sh')),
    command             TEXT NOT NULL,      -- redacted display form
    profile             TEXT,
    service             INTEGER NOT NULL DEFAULT 0,
    predicted_private   INTEGER NOT NULL DEFAULT 0,
    predicted_seconds   REAL,
    reserved_private    INTEGER NOT NULL DEFAULT 0,
    exit_code           INTEGER,
    main_exit_code      INTEGER,            -- main execution process result, an observation only
    peak_private        INTEGER,
    cpu_seconds         REAL,
    log_path            TEXT,
    executor_pid        INTEGER,
    executor_start      TEXT,               -- process start time; with the pid it identifies the executor
    pgid                INTEGER,            -- process group of the command
    launcher_pid        INTEGER,            -- the CLI that starts the executor
    launcher_start      TEXT,
    merge_fingerprint   TEXT,               -- set while running, cleared at the terminal state
    created_at          TEXT NOT NULL,
    started_at          TEXT,
    ended_at            TEXT
);
CREATE INDEX jobs_state ON jobs(state);

CREATE TABLE submissions (
    id                  TEXT PRIMARY KEY,
    job_id              TEXT NOT NULL REFERENCES jobs(id),
    session_id          TEXT,               -- NULL for anonymous submissions
    agent               TEXT,
    invocation_key      TEXT,
    request_id          TEXT UNIQUE,
    sandbox_signature   TEXT,
    options             TEXT NOT NULL,      -- JSON
    canceled            INTEGER NOT NULL DEFAULT 0,
    detached            INTEGER NOT NULL DEFAULT 0,
    final_retrieved     INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL
);
CREATE INDEX submissions_job ON submissions(job_id);
CREATE INDEX submissions_invocation ON submissions(invocation_key);
CREATE INDEX submissions_session ON submissions(session_id);

CREATE TABLE invocations (
    key                 TEXT PRIMARY KEY,
    observation         TEXT,               -- e.g. unverified; never overrides job state
    cwd_diagnostic      TEXT,
    created_at          TEXT NOT NULL
);

CREATE TABLE dependencies (
    job_id              TEXT NOT NULL REFERENCES jobs(id),
    after_job_id        TEXT NOT NULL REFERENCES jobs(id),
    PRIMARY KEY (job_id, after_job_id)
);

CREATE TABLE leases (
    job_id              TEXT PRIMARY KEY REFERENCES jobs(id),
    granted_at          TEXT NOT NULL,
    parallelism         INTEGER,
    jobserver_tokens    INTEGER,
    qos                 TEXT
);

CREATE TABLE locks (
    job_id              TEXT NOT NULL REFERENCES jobs(id),
    resource            TEXT NOT NULL,      -- normalized path or named resource
    kind                TEXT NOT NULL CHECK (kind IN ('path', 'named')),
    PRIMARY KEY (job_id, resource)
);

CREATE TABLE budget_records (
    id                  TEXT PRIMARY KEY,
    owner_kind          TEXT NOT NULL CHECK (owner_kind IN ('job', 'shared')),
    owner_id            TEXT NOT NULL,
    bytes               INTEGER NOT NULL,
    updated_at          TEXT NOT NULL
);

CREATE TABLE job_events (
    job_id              TEXT NOT NULL REFERENCES jobs(id),
    at                  TEXT NOT NULL,
    kind                TEXT NOT NULL,
    detail              TEXT
);
CREATE INDEX job_events_job ON job_events(job_id);

CREATE TABLE job_samples (
    job_id              TEXT NOT NULL REFERENCES jobs(id),
    at                  TEXT NOT NULL,
    private_resident    INTEGER,
    private_total       INTEGER,
    cpu_seconds         REAL,
    disk_read           INTEGER,
    disk_write          INTEGER
);

CREATE TABLE time_intervals (
    job_id              TEXT NOT NULL REFERENCES jobs(id),
    kind                TEXT NOT NULL CHECK (kind IN ('frozen', 'throttled')),
    settled_awake_ns    INTEGER NOT NULL DEFAULT 0,
    open_since_awake_ns INTEGER,
    open_state          TEXT,
    boot_id             TEXT,
    daemon_instance     TEXT,
    uncertain           INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (job_id, kind)
);

CREATE TABLE prediction_stats (
    key                 TEXT PRIMARY KEY,
    samples             INTEGER NOT NULL,
    peak_private_p95    INTEGER,
    peak_private_max    INTEGER,
    seconds_p50         REAL,
    updated_at          TEXT NOT NULL
);

CREATE TABLE interference (
    class_a             TEXT NOT NULL,
    class_b             TEXT NOT NULL,
    slowdown            REAL NOT NULL,
    samples             INTEGER NOT NULL,
    PRIMARY KEY (class_a, class_b)
);

CREATE TABLE sessions (
    id                  TEXT PRIMARY KEY,
    agent               TEXT,
    last_interaction    TEXT
);

CREATE TABLE providers (
    id                  TEXT PRIMARY KEY,
    config              TEXT NOT NULL       -- JSON, never contains keys
);

CREATE TABLE settings (
    key                 TEXT PRIMARY KEY,
    value               TEXT NOT NULL
);
