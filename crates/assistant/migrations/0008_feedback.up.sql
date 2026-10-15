-- A visitor's verdict on one answer (API/05 §1.8, T-4174): no address, cookie or other personal
-- data, and no reference to the conversation's row, which is deleted after a day while the
-- verdict is kept for 90.
CREATE TABLE feedback (
    project      text NOT NULL,
    deployment   text NOT NULL,
    conversation uuid NOT NULL,
    turn         integer NOT NULL CHECK (turn BETWEEN 1 AND 200),
    verdict      text NOT NULL CHECK (verdict IN ('up', 'down')),
    note         text CHECK (note IS NULL OR (verdict = 'down' AND char_length(note) BETWEEN 1 AND 500)),
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (conversation, turn)
);
CREATE INDEX feedback_by_day ON feedback (project, deployment, created_at);
ALTER TABLE feedback ENABLE ROW LEVEL SECURITY;
ALTER TABLE feedback FORCE ROW LEVEL SECURITY;
CREATE POLICY feedback_by_project ON feedback
    USING (project = current_setting('jc.project', true))
    WITH CHECK (project = current_setting('jc.project', true));
-- The minute loop deletes verdicts past 90 days with no project in scope, as it does conversations.
CREATE POLICY feedback_expired ON feedback FOR DELETE
    USING (created_at < now() - interval '90 days');
