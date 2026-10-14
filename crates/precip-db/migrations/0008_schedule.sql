-- Migration calendar: global rules, blackouts, per-pair rules and
-- dependencies, each pair's status with an event lineage, and every
-- computed schedule kept as a version.

-- Global rules live in settings: schedule.per_day, schedule.per_week,
-- schedule.per_month, schedule.weekdays (comma list, empty = any),
-- schedule.target_end (date), schedule.nonprod_before_prod (true|false).
INSERT INTO settings (key, value) VALUES
    ('schedule.per_day', '1'),
    ('schedule.per_week', '3'),
    ('schedule.per_month', '10'),
    ('schedule.weekdays', ''),
    ('schedule.target_end', ''),
    ('schedule.nonprod_before_prod', 'true');

-- Freezes and holidays. A NULL workload applies to every pair.
CREATE TABLE schedule_blackouts (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL,
    start_date   TEXT NOT NULL,       -- YYYY-MM-DD
    end_date     TEXT NOT NULL,
    workload     TEXT,
    environment  TEXT
);

-- Per-pair rules. Absent rows mean the global rules apply.
CREATE TABLE schedule_rules (
    workload     TEXT NOT NULL,
    environment  TEXT NOT NULL,
    weekdays     TEXT NOT NULL DEFAULT '',   -- comma list, empty = global
    earliest     TEXT,                       -- YYYY-MM-DD
    priority     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workload, environment)
);

CREATE TABLE schedule_dependencies (
    workload     TEXT NOT NULL,
    environment  TEXT NOT NULL,
    on_workload  TEXT NOT NULL,
    on_environment TEXT NOT NULL,
    PRIMARY KEY (workload, environment, on_workload, on_environment)
);

-- Where each pair stands. Absent rows are proposed.
CREATE TABLE migration_status (
    workload        TEXT NOT NULL,
    environment     TEXT NOT NULL,
    status          TEXT NOT NULL,          -- proposed | confirmed | missed | migrated
    confirmed_date  TEXT,                   -- YYYY-MM-DD when confirmed
    migrated_date   TEXT,
    PRIMARY KEY (workload, environment)
);

-- Every status change, appended: confirm, unconfirm, missed, migrated,
-- expired (a confirmed date passed without a migration).
CREATE TABLE migration_events (
    id           INTEGER PRIMARY KEY,
    workload     TEXT NOT NULL,
    environment  TEXT NOT NULL,
    event        TEXT NOT NULL,
    date         TEXT,                      -- the date the event concerns
    at           TEXT NOT NULL,             -- RFC 3339
    note         TEXT
);
CREATE INDEX migration_events_by_pair ON migration_events (workload, environment, id);

-- Each computed schedule, so projected end dates and reschedules can be
-- tracked over time.
CREATE TABLE schedule_versions (
    id           INTEGER PRIMARY KEY,
    computed_at  TEXT NOT NULL,             -- RFC 3339
    as_of        TEXT NOT NULL,             -- YYYY-MM-DD the schedule starts from
    target_end   TEXT,
    ends         TEXT,                      -- last scheduled date
    late         INTEGER NOT NULL,
    trigger      TEXT NOT NULL              -- open | status_change | rule_change | command
);

CREATE TABLE schedule_slots (
    version_id   INTEGER NOT NULL REFERENCES schedule_versions(id),
    workload     TEXT NOT NULL,
    environment  TEXT NOT NULL,
    date         TEXT NOT NULL,
    locked       INTEGER NOT NULL,
    reason       TEXT NOT NULL,
    alternatives TEXT NOT NULL,             -- comma list of dates
    PRIMARY KEY (version_id, workload, environment)
);
