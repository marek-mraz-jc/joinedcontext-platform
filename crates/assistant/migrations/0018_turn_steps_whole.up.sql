-- Every step of a turn kept whole (AG-99, API/05 §3.1, T-4681): a model call's cached tokens,
-- reasoning text, finish reason, effort and prompt version; the tables, charts and maps shown
-- (`visual`, with their rows) and the time no step covers (`untracked`); inputs and outputs up to
-- 64 kB. A long text is compressed by the table's TOAST, so no column of its own does it. The
-- question's hash finds the turns that asked the same, and the sweep reads turns by expiry.
ALTER TABLE turn_steps DROP CONSTRAINT turn_steps_kind_check;
ALTER TABLE turn_steps ADD CONSTRAINT turn_steps_kind_check
    CHECK (kind IN ('route', 'search', 'tool', 'model', 'answer', 'visual', 'untracked'));
ALTER TABLE turn_steps DROP CONSTRAINT turn_steps_input_check;
ALTER TABLE turn_steps ADD CONSTRAINT turn_steps_input_check CHECK (octet_length(input) <= 65536);
ALTER TABLE turn_steps DROP CONSTRAINT turn_steps_output_check;
ALTER TABLE turn_steps ADD CONSTRAINT turn_steps_output_check CHECK (octet_length(output) <= 65536);
ALTER TABLE turn_steps
    ADD COLUMN cached_tokens  bigint CHECK (cached_tokens >= 0),
    ADD COLUMN reasoning      text CHECK (octet_length(reasoning) <= 65536),
    ADD COLUMN finish_reason  text CHECK (length(finish_reason) <= 32),
    ADD COLUMN effort         text CHECK (length(effort) <= 16),
    ADD COLUMN prompt_version text CHECK (length(prompt_version) <= 64),
    ADD COLUMN rows           bigint CHECK (rows >= 0);
ALTER TABLE turns ADD COLUMN question_hash text CHECK (length(question_hash) = 32);
CREATE INDEX turns_by_question ON turns (project, question_hash);
CREATE INDEX turns_by_deployment ON turns (project, deployment, at DESC);
CREATE INDEX turns_by_expiry ON turns (expires_at);
