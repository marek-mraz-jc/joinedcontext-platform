-- The questions a conversation asked, held to the deployment's budget.messagesPerConversation
-- (T-4149).
ALTER TABLE conversations ADD COLUMN questions integer NOT NULL DEFAULT 0;

-- What a deployment's model calls cost on the day, in millionths of a US dollar, summed from the
-- provider's `usage.cost`; budget.costPerDayUsd holds it (T-4149).
ALTER TABLE usage ADD COLUMN cost_micros bigint NOT NULL DEFAULT 0;
