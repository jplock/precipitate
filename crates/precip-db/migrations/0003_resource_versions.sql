-- Resources are stored as versions, not per-scan copies. A version row is
-- written only when a resource first appears or its content changes, and
-- carries the range of scans it was observed in. An unchanged resource costs
-- one update of last_seen_seq per scan.
--
-- The per-scan copies this replaces hold no production data yet and cannot
-- be converted in SQL (the content hash is computed in Rust), so they are
-- dropped.

ALTER TABLE scans ADD COLUMN seq INTEGER;
UPDATE scans SET seq = (
    SELECT COUNT(*) FROM scans s2
    WHERE s2.started < scans.started
       OR (s2.started = scans.started AND s2.rowid <= scans.rowid)
);
CREATE UNIQUE INDEX scans_by_seq ON scans (seq);

DROP TABLE workload_members;
DROP TABLE relationships;
DROP TABLE resources;

CREATE TABLE resource_versions (
    arn             TEXT NOT NULL,
    version         INTEGER NOT NULL,   -- 1, 2, 3 ... per ARN
    kind            TEXT NOT NULL,
    account_id      TEXT NOT NULL,
    region          TEXT NOT NULL,
    tags            TEXT NOT NULL,      -- JSON object
    body            TEXT NOT NULL,      -- JSON, the resource as collected
    content_hash    TEXT NOT NULL,      -- SHA-256 of tags and body
    first_seen_seq  INTEGER NOT NULL,   -- scans.seq
    last_seen_seq   INTEGER NOT NULL,   -- scans.seq, inclusive
    PRIMARY KEY (arn, version)
);
CREATE INDEX resource_versions_by_span
    ON resource_versions (first_seen_seq, last_seen_seq);
CREATE INDEX resource_versions_by_account
    ON resource_versions (account_id, region, kind);

CREATE TABLE relationship_versions (
    from_arn        TEXT NOT NULL,
    edge            TEXT NOT NULL,
    to_arn          TEXT NOT NULL,
    first_seen_seq  INTEGER NOT NULL,
    last_seen_seq   INTEGER NOT NULL,
    PRIMARY KEY (from_arn, edge, to_arn, first_seen_seq)
);

-- Grouping result per scan. No foreign key to a resource row because the
-- resource's version may change independently of its workload.
CREATE TABLE workload_members (
    scan_id      TEXT NOT NULL REFERENCES scans(id),
    arn          TEXT NOT NULL,
    workload     TEXT NOT NULL,
    environment  TEXT NOT NULL,
    signal       TEXT NOT NULL,
    PRIMARY KEY (scan_id, arn)
);
CREATE INDEX workload_members_by_workload
    ON workload_members (workload, environment, scan_id);
