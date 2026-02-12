use std::sync::Arc;

use dashmap::DashSet;
use teloxide::types::ChatId;

pub struct LockGuard {
    set: Arc<DashSet<ChatId>>,
    id: ChatId,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        log::info!("Releasing lock for chat_id: {}", self.id);
        self.set.remove(&self.id);
    }
}

#[derive(Clone, Default)]
pub struct ConcurrencyLimiter {
    processing_users: Arc<DashSet<ChatId>>,
}

impl ConcurrencyLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn try_lock(&self, chat_id: ChatId) -> Option<LockGuard> {
        if self.processing_users.insert(chat_id) {
            log::info!("Acquired lock for chat_id: {}", chat_id);
            Some(LockGuard {
                set: Arc::clone(&self.processing_users),
                id: chat_id,
            })
        } else {
            log::info!("User {} is already being processed.", chat_id);
            None
        }
    }
}
