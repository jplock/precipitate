-- Dispositions, the default disposition and workload merges join the
-- change log, so a replay rebuilds them too. Existing state is backfilled
-- in the order it happened.
INSERT INTO plan_events (at, kind, body)
SELECT changed_at,
       'disposition_set',
       json_object('kind', 'disposition_set', 'workload', workload, 'environment', environment,
                   'disposition', disposition, 'source', source, 'note', note)
FROM disposition_history WHERE source != 'default' ORDER BY id;

INSERT INTO plan_events (at, kind, body)
SELECT COALESCE((SELECT MIN(at) FROM plan_events), strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
       'default_disposition_set',
       json_object('kind', 'default_disposition_set', 'disposition', value, 'apply_to_existing', json('false'))
FROM settings WHERE key = 'default_disposition' AND value != 'rehost';

INSERT INTO plan_events (at, kind, body)
SELECT COALESCE((SELECT MIN(at) FROM plan_events), strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
       'workload_merged',
       json_object('kind', 'workload_merged', 'alias', alias, 'into', workload)
FROM workload_aliases ORDER BY alias;
