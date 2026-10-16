-- Whether the first round's judgement was skipped because its passages already covered the
-- question (T-4360), so the share of skipped judgements can be read beside the stage times.
ALTER TABLE answer_timings ADD COLUMN judge_skipped boolean NOT NULL DEFAULT false;
