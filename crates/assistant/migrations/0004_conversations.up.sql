-- A conversation of a deployment: its id, what it spent and when, never what was said (AG-99,
-- T-3055). The channel holds the text and sends the last turns back with each question.
CREATE TABLE conversations (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    project    text NOT NULL,
    deployment text NOT NULL,
    tokens     bigint NOT NULL DEFAULT 0 CHECK (tokens >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    last_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX conversations_age ON conversations (created_at);
ALTER TABLE conversations ENABLE ROW LEVEL SECURITY;
ALTER TABLE conversations FORCE ROW LEVEL SECURITY;
CREATE POLICY conversations_by_project ON conversations
    USING (project = current_setting('jc.project', true))
    WITH CHECK (project = current_setting('jc.project', true));
