ALTER TABLE conversations DROP COLUMN IF EXISTS origin;
DROP POLICY IF EXISTS conversations_expired ON conversations;
