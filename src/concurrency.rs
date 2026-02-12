use dashmap::DashSet;
use teloxide::types::ChatId;

pub struct LockGuard<'a> {
    set: &'a DashSet<ChatId>,
    id: ChatId,
}

impl<'a> Drop for LockGuard<'a> {
    fn drop(&mut self) {
        log::info!("Releasing lock for chat_id: {}", self.id);
        self.set.remove(&self.id);
    }
}

#[derive(Default)]
pub struct ConcurrencyLimiter {
    processing_users: DashSet<ChatId>,
}

impl ConcurrencyLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn try_lock(&self, chat_id: ChatId) -> Option<LockGuard<'_>> {
        if self.processing_users.insert(chat_id) {
            log::info!("Acquired lock for chat_id: {}", chat_id);
            Some(LockGuard {
                set: &self.processing_users,
                id: chat_id,
            })
        } else {
            log::info!("User {} is already being processed.", chat_id);
            None
        }
    }
}
