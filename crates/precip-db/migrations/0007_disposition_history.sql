-- Every disposition change, appended and never edited, so a pair's
-- lineage (retire in October, rehost in November) is a query.
CREATE TABLE disposition_history (
    id           INTEGER PRIMARY KEY,
    workload     TEXT NOT NULL,
    environment  TEXT NOT NULL,
    disposition  TEXT NOT NULL,
    previous     TEXT,                -- NULL for the first record
    changed_at   TEXT NOT NULL,       -- RFC 3339
    source       TEXT NOT NULL,       -- default | user | bulk
    note         TEXT
);
CREATE INDEX disposition_history_by_pair
    ON disposition_history (workload, environment, id);

-- Existing pairs get an opening record so history never starts blank.
INSERT INTO disposition_history (workload, environment, disposition, previous, changed_at, source)
SELECT workload, environment, disposition, NULL,
       strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'default'
FROM workload_environments;
