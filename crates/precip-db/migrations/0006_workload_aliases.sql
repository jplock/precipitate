-- A workload name that is really another workload. Applied after every
-- placement, so grouping and regrouping both honour the merge.
CREATE TABLE workload_aliases (
    alias      TEXT PRIMARY KEY,
    workload   TEXT NOT NULL
);
