ALTER TABLE answer_timings
    DROP COLUMN router_provider,
    DROP COLUMN judge_provider,
    DROP COLUMN answer_provider,
    DROP COLUMN route_reasoning_tokens,
    DROP COLUMN judge_reasoning_tokens,
    DROP COLUMN answer_reasoning_tokens;
