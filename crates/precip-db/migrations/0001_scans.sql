-- Scans are append-only. Nothing in these tables is updated or deleted.
CREATE TABLE scans (
    id          TEXT PRIMARY KEY,
    started     TEXT NOT NULL,   -- RFC 3339
    finished    TEXT,            -- RFC 3339
    provider    TEXT NOT NULL,   -- aws | azure
    caller      TEXT NOT NULL,
    created     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE scan_targets (
    scan_id     TEXT NOT NULL REFERENCES scans(id),
    account_id  TEXT NOT NULL,
    region      TEXT NOT NULL,
    status      TEXT NOT NULL,   -- scanned | partial | failed
    error       TEXT,
    PRIMARY KEY (scan_id, account_id, region)
);

CREATE TABLE resources (
    scan_id     TEXT NOT NULL REFERENCES scans(id),
    arn         TEXT NOT NULL,
    kind        TEXT NOT NULL,
    account_id  TEXT NOT NULL,
    region      TEXT NOT NULL,
    tags        TEXT NOT NULL,   -- JSON object
    body        TEXT NOT NULL,   -- JSON, the resource as collected
    PRIMARY KEY (scan_id, arn)
);
CREATE INDEX resources_by_arn ON resources (arn, scan_id);

CREATE TABLE relationships (
    scan_id     TEXT NOT NULL REFERENCES scans(id),
    from_arn    TEXT NOT NULL,
    edge        TEXT NOT NULL,
    to_arn      TEXT NOT NULL,
    PRIMARY KEY (scan_id, from_arn, edge, to_arn)
);
