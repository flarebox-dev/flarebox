CREATE TABLE effects_in_flight (
    position INTEGER PRIMARY KEY,
    record BLOB NOT NULL
) STRICT;

INSERT INTO effects_in_flight (position, record)
    SELECT 0, in_flight FROM effect_state WHERE in_flight IS NOT NULL;

ALTER TABLE effect_state DROP COLUMN in_flight;
