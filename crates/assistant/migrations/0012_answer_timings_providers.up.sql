-- The OpenRouter provider that served each stage's call (T-4360), so the eval shows which
-- provider answered each row: a provider name, never a question or an answer.
ALTER TABLE answer_timings
    ADD COLUMN router_provider text CHECK (length(router_provider) <= 64),
    ADD COLUMN judge_provider text CHECK (length(judge_provider) <= 64),
    ADD COLUMN answer_provider text CHECK (length(answer_provider) <= 64);
