-- Freezes loaded together, such as a year of public holidays, carry the
-- name of their set so screens can show and remove them as one.
ALTER TABLE schedule_blackouts ADD COLUMN set_name TEXT;

-- Holidays loaded before sets existed become the set for their year, in
-- the table and in the log, so a replay lands in the same place.
UPDATE schedule_blackouts
SET set_name = 'US federal holidays ' || substr(start_date, 1, 4)
WHERE set_name IS NULL AND workload IS NULL AND start_date = end_date
  AND name IN ("New Year's Day", 'Martin Luther King Jr. Day', "Presidents' Day",
               'Memorial Day', 'Juneteenth', 'Independence Day', 'Labor Day',
               'Columbus Day', 'Veterans Day', 'Thanksgiving', 'Christmas Day');

UPDATE plan_events
SET body = json_set(body, '$.set', 'US federal holidays ' || substr(json_extract(body, '$.start'), 1, 4))
WHERE kind = 'blackout_added'
  AND json_extract(body, '$.workload') IS NULL
  AND json_extract(body, '$.start') = json_extract(body, '$.end')
  AND json_extract(body, '$.name') IN ("New Year's Day", 'Martin Luther King Jr. Day', "Presidents' Day",
               'Memorial Day', 'Juneteenth', 'Independence Day', 'Labor Day',
               'Columbus Day', 'Veterans Day', 'Thanksgiving', 'Christmas Day');
