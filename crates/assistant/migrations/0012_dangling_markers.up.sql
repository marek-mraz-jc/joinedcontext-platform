-- Citation markers an answer wrote for no source, removed before it was sent (T-4366): a count
-- per answer, so a model that keeps inventing them shows in the timings.
ALTER TABLE answer_timings ADD COLUMN dangling_markers integer NOT NULL DEFAULT 0 CHECK (dangling_markers >= 0);
