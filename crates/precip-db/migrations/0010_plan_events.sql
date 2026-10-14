-- The plan is event-sourced: every change to the rules, freezes, waves,
-- per-pair settings, dependencies and statuses is appended here, the
-- tables above are projections rebuilt from it, and every event yields a
-- new schedule version.
CREATE TABLE plan_events (
    id           INTEGER PRIMARY KEY,
    at           TEXT NOT NULL,             -- RFC 3339
    kind         TEXT NOT NULL,
    body         TEXT NOT NULL,             -- JSON, the whole event
    version_id   INTEGER REFERENCES schedule_versions(id)
);

-- Versions keep the full computed schedule so screens read it back
-- instead of computing again.
ALTER TABLE schedule_versions ADD COLUMN schedule TEXT;
ALTER TABLE schedule_versions ADD COLUMN event_id INTEGER;

-- Backfill the log from the state that exists, so a plan made before
-- this migration replays to the same place. Settings first, then the
-- rest in a stable order, then the status changes with their own times.
INSERT INTO plan_events (at, kind, body)
SELECT COALESCE((SELECT MIN(at) FROM migration_events), strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
       'rules_set',
       json_object(
         'kind', 'rules_set',
         'per_day', CAST((SELECT value FROM settings WHERE key = 'schedule.per_day') AS INTEGER),
         'per_week', CAST((SELECT value FROM settings WHERE key = 'schedule.per_week') AS INTEGER),
         'per_month', CAST((SELECT value FROM settings WHERE key = 'schedule.per_month') AS INTEGER),
         'weekdays', COALESCE((SELECT value FROM settings WHERE key = 'schedule.weekdays'), ''),
         'target_end', NULLIF((SELECT value FROM settings WHERE key = 'schedule.target_end'), ''),
         'nonprod_before_prod', CASE WHEN COALESCE((SELECT value FROM settings WHERE key = 'schedule.nonprod_before_prod'), 'true') = 'true' THEN json('true') ELSE json('false') END
       )
WHERE EXISTS (SELECT 1 FROM settings WHERE key = 'schedule.per_day');

INSERT INTO plan_events (at, kind, body)
SELECT COALESCE((SELECT MIN(at) FROM migration_events), strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
       'blackout_added',
       json_object('kind', 'blackout_added', 'name', name, 'start', start_date,
                   'end', end_date, 'workload', workload, 'environment', environment)
FROM schedule_blackouts ORDER BY id;

INSERT INTO plan_events (at, kind, body)
SELECT COALESCE((SELECT MIN(at) FROM migration_events), strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
       'pair_rule_set',
       json_object('kind', 'pair_rule_set', 'workload', workload, 'environment', environment,
                   'weekdays', weekdays, 'earliest', earliest, 'priority', priority)
FROM schedule_rules ORDER BY workload, environment;

INSERT INTO plan_events (at, kind, body)
SELECT COALESCE((SELECT MIN(at) FROM migration_events), strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
       'pair_plan_set',
       json_object('kind', 'pair_plan_set', 'workload', workload, 'environment', environment,
                   'wave', wave, 'duration_days', duration_days, 'weight', weight)
FROM schedule_rules ORDER BY workload, environment;

INSERT INTO plan_events (at, kind, body)
SELECT COALESCE((SELECT MIN(at) FROM migration_events), strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
       'wave_set',
       json_object('kind', 'wave_set', 'number', wave, 'name', name, 'target_end', target_end)
FROM schedule_waves ORDER BY wave;

INSERT INTO plan_events (at, kind, body)
SELECT COALESCE((SELECT MIN(at) FROM migration_events), strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
       'dependency_added',
       json_object('kind', 'dependency_added', 'workload', workload, 'environment', environment,
                   'on_workload', on_workload, 'on_environment', on_environment)
FROM schedule_dependencies ORDER BY workload, environment, on_workload, on_environment;

INSERT INTO plan_events (at, kind, body)
SELECT at,
       'status_changed',
       json_object('kind', 'status_changed', 'workload', workload, 'environment', environment,
                   'status', CASE event
                               WHEN 'confirmed' THEN 'confirmed'
                               WHEN 'reconfirmed' THEN 'confirmed'
                               WHEN 'unconfirmed' THEN 'proposed'
                               ELSE event END,
                   'date', date, 'note', note)
FROM migration_events ORDER BY id;
