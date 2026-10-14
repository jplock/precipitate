-- A disposition belongs to a workload and environment pair, not to the
-- workload as a whole. The workload row keeps only its name and notes.
CREATE TABLE workload_environments_new (
    workload     TEXT NOT NULL REFERENCES workloads(name),
    environment  TEXT NOT NULL,
    disposition  TEXT NOT NULL DEFAULT 'rehost',
    PRIMARY KEY (workload, environment)
);
INSERT INTO workload_environments_new (workload, environment, disposition)
    SELECT we.workload, we.environment, COALESCE(we.disposition, w.disposition)
    FROM workload_environments we
    JOIN workloads w ON w.name = we.workload;
DROP TABLE workload_environments;
ALTER TABLE workload_environments_new RENAME TO workload_environments;

CREATE TABLE workloads_new (
    name   TEXT PRIMARY KEY,
    notes  TEXT
);
INSERT INTO workloads_new (name, notes) SELECT name, notes FROM workloads;
DROP TABLE workloads;
ALTER TABLE workloads_new RENAME TO workloads;
