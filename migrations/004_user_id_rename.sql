-- Rename chat_id to user_id in billing tables.
-- Subscriptions, payments, and premium_usage should track who the person is (Telegram user id),
-- not which chat they were in when they paid. This makes premium features work correctly in
-- group chats, where chat_id is the group's id rather than the individual user's id.
-- callback_contexts.chat_id is intentionally unchanged — it records the destination chat
-- (where the bot sends replies), which may be a group.

ALTER TABLE subscriptions RENAME COLUMN chat_id TO user_id;
ALTER TABLE payments RENAME COLUMN chat_id TO user_id;
ALTER TABLE premium_usage RENAME COLUMN chat_id TO user_id;

DROP INDEX IF EXISTS idx_subscriptions_chat_id;
CREATE INDEX idx_subscriptions_user_id ON subscriptions(user_id);
