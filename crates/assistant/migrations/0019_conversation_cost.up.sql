-- What each conversation's model calls cost so far, in millionths of a US dollar, which
-- budget.costPerConversationUsd caps before each call (AG-107, T-4737).
ALTER TABLE conversations ADD COLUMN cost_micros bigint NOT NULL DEFAULT 0;
