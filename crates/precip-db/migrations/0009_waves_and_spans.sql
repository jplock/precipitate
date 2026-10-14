-- Waves, migration spans and weights, rollback, and the verification of
-- migrated pairs against the latest scan.

-- Named waves. A pair's wave number need not have a row here; the row
-- adds a name and a target end to report against.
CREATE TABLE schedule_waves (
    wave         INTEGER PRIMARY KEY,
    name         TEXT NOT NULL DEFAULT '',
    target_end   TEXT                        -- YYYY-MM-DD
);

-- Plan per pair: which wave, how many consecutive days the migration
-- occupies, and how many capacity units it takes on each of them.
ALTER TABLE schedule_rules ADD COLUMN wave INTEGER NOT NULL DEFAULT 1;
ALTER TABLE schedule_rules ADD COLUMN duration_days INTEGER NOT NULL DEFAULT 1;
ALTER TABLE schedule_rules ADD COLUMN weight INTEGER NOT NULL DEFAULT 1;

-- Slots now span start..end and belong to a wave. migration_status gains
-- the rolled_back status; migration_events gains the rolled_back event.
ALTER TABLE schedule_slots ADD COLUMN end_date TEXT;
ALTER TABLE schedule_slots ADD COLUMN wave INTEGER NOT NULL DEFAULT 1;
