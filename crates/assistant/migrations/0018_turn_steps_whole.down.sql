DROP INDEX turns_by_expiry;
DROP INDEX turns_by_deployment;
DROP INDEX turns_by_question;
ALTER TABLE turns DROP COLUMN question_hash;
DELETE FROM turn_steps WHERE kind IN ('visual', 'untracked');
ALTER TABLE turn_steps
    DROP COLUMN rows, DROP COLUMN prompt_version, DROP COLUMN effort,
    DROP COLUMN finish_reason, DROP COLUMN reasoning, DROP COLUMN cached_tokens;
ALTER TABLE turn_steps DROP CONSTRAINT turn_steps_kind_check;
ALTER TABLE turn_steps ADD CONSTRAINT turn_steps_kind_check
    CHECK (kind IN ('route', 'search', 'tool', 'model', 'answer'));
UPDATE turn_steps SET input = left(input, 2048) WHERE octet_length(input) > 8192;
UPDATE turn_steps SET output = left(output, 2048) WHERE octet_length(output) > 8192;
ALTER TABLE turn_steps DROP CONSTRAINT turn_steps_input_check;
ALTER TABLE turn_steps ADD CONSTRAINT turn_steps_input_check CHECK (octet_length(input) <= 8192);
ALTER TABLE turn_steps DROP CONSTRAINT turn_steps_output_check;
ALTER TABLE turn_steps ADD CONSTRAINT turn_steps_output_check CHECK (octet_length(output) <= 8192);
