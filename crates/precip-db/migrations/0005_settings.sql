-- Plan-wide settings. The default disposition is applied to every workload
-- and environment pair when it is first seen; pairs can then be changed
-- individually.
CREATE TABLE settings (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);
INSERT INTO settings (key, value) VALUES ('default_disposition', 'rehost');
