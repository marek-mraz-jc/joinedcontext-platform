-- How long one answer took, stage by stage (API/05 §1.3, T-4293): times and counts only, never
-- the question, the answer or the conversation it belongs to.
CREATE TABLE answer_timings (
    project        text NOT NULL,
    deployment     text NOT NULL,
    at             timestamptz NOT NULL DEFAULT now(),
    cached         boolean NOT NULL,
    model_calls    integer NOT NULL CHECK (model_calls >= 0),
    first_token_ms bigint CHECK (first_token_ms >= 0),
    total_ms       bigint NOT NULL CHECK (total_ms >= 0),
    route_ms       bigint CHECK (route_ms >= 0),
    search_ms      bigint CHECK (search_ms >= 0),
    judge_ms       bigint CHECK (judge_ms >= 0),
    answer_ms      bigint CHECK (answer_ms >= 0)
);
CREATE INDEX answer_timings_by_day ON answer_timings (project, deployment, at);
ALTER TABLE answer_timings ENABLE ROW LEVEL SECURITY;
ALTER TABLE answer_timings FORCE ROW LEVEL SECURITY;
CREATE POLICY answer_timings_by_project ON answer_timings
    USING (project = current_setting('jc.project', true))
    WITH CHECK (project = current_setting('jc.project', true));
-- The minute loop deletes timings past 30 days with no project in scope, as it does verdicts.
CREATE POLICY answer_timings_expired ON answer_timings FOR DELETE
    USING (at < now() - interval '30 days');
