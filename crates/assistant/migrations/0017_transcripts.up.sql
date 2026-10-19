-- Each turn of a deployment's conversations with the steps that answered it (AG-99, API/05 §3.1,
-- T-4670): the texts redacted before the insert, no address (the client is a salted daily
-- hash), kept until `expires_at`, the turn's time plus the deployment's transcriptRetention. The
-- conversation row of the limits is still deleted after a day; a turn names its conversation by
-- id alone. Only the Portal reads these, through the administration routes.
CREATE TABLE turns (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project        text NOT NULL,
    deployment     text NOT NULL,
    conversation   uuid NOT NULL,
    turn           integer NOT NULL CHECK (turn BETWEEN 1 AND 200),
    at             timestamptz NOT NULL DEFAULT now(),
    expires_at     timestamptz NOT NULL,
    origin         text CHECK (length(origin) <= 300),
    client         text CHECK (length(client) <= 64),
    question       text NOT NULL,
    answer         text NOT NULL DEFAULT '',
    blocks         jsonb NOT NULL DEFAULT '[]',
    history        jsonb NOT NULL DEFAULT '[]',
    route          text NOT NULL CHECK (length(route) <= 16),
    first_token_ms bigint CHECK (first_token_ms >= 0),
    total_ms       bigint NOT NULL CHECK (total_ms >= 0),
    error          jsonb,
    timed_out      boolean NOT NULL DEFAULT false,
    tokens_in      bigint NOT NULL DEFAULT 0,
    tokens_out     bigint NOT NULL DEFAULT 0,
    -- The sum of the priced model calls, in millionths of a US dollar; `uncosted` counts the
    -- calls the provider sent no price for, so a sum with uncosted > 0 is a floor (T-4178).
    cost_micros    bigint NOT NULL DEFAULT 0 CHECK (cost_micros >= 0),
    uncosted       integer NOT NULL DEFAULT 0 CHECK (uncosted >= 0),
    UNIQUE (conversation, turn)
);
CREATE INDEX turns_by_time ON turns (project, at DESC);
CREATE INDEX turns_by_conversation ON turns (project, conversation);
ALTER TABLE turns ENABLE ROW LEVEL SECURITY;
ALTER TABLE turns FORCE ROW LEVEL SECURITY;
CREATE POLICY turns_by_project ON turns
    USING (project = current_setting('jc.project', true))
    WITH CHECK (project = current_setting('jc.project', true));
-- The minute loop deletes turns past their retention with no project in scope, as it does
-- verdicts; their steps go with them (ON DELETE CASCADE, which row security does not stop).
CREATE POLICY turns_expired ON turns FOR DELETE USING (expires_at < now());

CREATE TABLE turn_steps (
    turn_id          bigint NOT NULL REFERENCES turns (id) ON DELETE CASCADE,
    project          text NOT NULL,
    ordinal          integer NOT NULL CHECK (ordinal >= 0),
    kind             text NOT NULL CHECK (kind IN ('route', 'search', 'tool', 'model', 'answer')),
    name             text NOT NULL CHECK (length(name) <= 200),
    endpoint         text CHECK (length(endpoint) <= 200),
    input            text CHECK (octet_length(input) <= 8192),
    output           text CHECK (octet_length(output) <= 8192),
    output_bytes     bigint CHECK (output_bytes >= 0),
    status           text NOT NULL CHECK (status IN ('ok', 'failed')),
    start_ms         bigint NOT NULL CHECK (start_ms >= 0),
    duration_ms      bigint NOT NULL CHECK (duration_ms >= 0),
    error            text CHECK (length(error) <= 2000),
    stage            text CHECK (length(stage) <= 16),
    model            text CHECK (length(model) <= 100),
    provider         text CHECK (length(provider) <= 64),
    tokens_in        bigint,
    tokens_out       bigint,
    reasoning_tokens bigint,
    cost_micros      bigint CHECK (cost_micros >= 0),
    PRIMARY KEY (turn_id, ordinal)
);
ALTER TABLE turn_steps ENABLE ROW LEVEL SECURITY;
ALTER TABLE turn_steps FORCE ROW LEVEL SECURITY;
CREATE POLICY turn_steps_by_project ON turn_steps
    USING (project = current_setting('jc.project', true))
    WITH CHECK (project = current_setting('jc.project', true));
