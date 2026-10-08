-- A conversation past its 24 hours can no longer be continued (chat::CONVERSATION_HOURS), so its
-- row is deleted (T-3314, AG-99). The minute loop deletes with no project in scope: this policy
-- lets a DELETE reach expired rows of every project and nothing else. The DELETE names no column,
-- so no SELECT policy is needed and no row of another project is ever read.
CREATE POLICY conversations_expired ON conversations FOR DELETE
    USING (created_at < now() - interval '24 hours');

-- The origin a conversation started from; it is continued from that origin alone (AG-100).
-- `NULL`: started with no `Origin`, by a client that is not a browser or by the Portal.
ALTER TABLE conversations ADD COLUMN origin text;
