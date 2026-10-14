-- Holidays are chosen one by one, since not every company honours every
-- one; freezes no longer belong to sets.
ALTER TABLE schedule_blackouts DROP COLUMN set_name;
UPDATE plan_events SET body = json_remove(body, '$.set') WHERE kind IN ('blackout_added', 'blackout_removed');
