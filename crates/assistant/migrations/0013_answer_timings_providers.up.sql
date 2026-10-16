-- The OpenRouter provider that served each stage's call and the reasoning tokens its calls spent
-- (T-4360), so the eval reads both per row: provider names and counts, never a question or an
-- answer.
ALTER TABLE answer_timings
    ADD COLUMN router_provider text CHECK (length(router_provider) <= 64),
    ADD COLUMN judge_provider text CHECK (length(judge_provider) <= 64),
    ADD COLUMN answer_provider text CHECK (length(answer_provider) <= 64),
    ADD COLUMN route_reasoning_tokens integer,
    ADD COLUMN judge_reasoning_tokens integer,
    ADD COLUMN answer_reasoning_tokens integer;
