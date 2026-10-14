-- Plan tables. Unlike scans, these hold user decisions and are updated in
-- place. They are keyed by workload name or ARN, never by scan id, so a
-- rescan keeps every decision.
CREATE TABLE workloads (
    name         TEXT PRIMARY KEY,
    disposition  TEXT NOT NULL DEFAULT 'rehost',
    notes        TEXT
);

-- Per-environment disposition override. NULL inherits the workload's.
CREATE TABLE workload_environments (
    workload     TEXT NOT NULL REFERENCES workloads(name),
    environment  TEXT NOT NULL,
    disposition  TEXT,
    PRIMARY KEY (workload, environment)
);

-- Manual placement of one resource, by ARN. Wins over every signal.
CREATE TABLE workload_overrides (
    arn          TEXT PRIMARY KEY,
    workload     TEXT NOT NULL,
    environment  TEXT NOT NULL
);

-- Grouping result for one scan: which workload and environment each
-- resource landed in and the signal that put it there. Recomputed on every
-- scan and appended like the scan itself.
CREATE TABLE workload_members (
    scan_id      TEXT NOT NULL,
    arn          TEXT NOT NULL,
    workload     TEXT NOT NULL,
    environment  TEXT NOT NULL,
    signal       TEXT NOT NULL,
    PRIMARY KEY (scan_id, arn),
    FOREIGN KEY (scan_id, arn) REFERENCES resources(scan_id, arn)
);
CREATE INDEX workload_members_by_workload
    ON workload_members (workload, environment, scan_id);
