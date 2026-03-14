use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq)]
pub enum SubscriptionTier {
    Free,
    Basic,
    Pro,
}

impl SubscriptionTier {
    pub fn ai_seconds_limit(&self) -> i32 {
        match self {
            Self::Free => 0,
            Self::Basic => 3600,   // 60 minutes
            Self::Pro => 12000,    // 200 minutes
        }
    }

    pub fn price_stars(&self) -> u32 {
        match self {
            Self::Free => 0,
            Self::Basic => 50,
            Self::Pro => 150,
        }
    }

    /// Audio extraction is unlimited for all paid tiers (no API cost).
    /// Free users with top-up balance also get it — checked via SubscriptionInfo.
    pub fn has_audio_extraction(&self) -> bool {
        !matches!(self, Self::Free)
    }
}

impl fmt::Display for SubscriptionTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Free => write!(f, "free"),
            Self::Basic => write!(f, "basic"),
            Self::Pro => write!(f, "pro"),
        }
    }
}

impl FromStr for SubscriptionTier {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "free" => Ok(Self::Free),
            "basic" => Ok(Self::Basic),
            "pro" => Ok(Self::Pro),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubscriptionInfo {
    pub tier: SubscriptionTier,
    pub ai_seconds_used: i32,
    pub ai_seconds_limit: i32,
    pub topup_seconds_available: i32,
    pub last_topup_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl SubscriptionInfo {
    /// Default for users with no subscription row.
    pub fn free_default() -> Self {
        Self {
            tier: SubscriptionTier::Free,
            ai_seconds_used: 0,
            ai_seconds_limit: 0,
            topup_seconds_available: 0,
            last_topup_at: None,
            expires_at: None,
        }
    }

    /// Monthly seconds remaining (0 if subscription expired or Free tier).
    fn monthly_remaining(&self) -> i32 {
        let has_active_sub = self.tier != SubscriptionTier::Free
            && self.expires_at.is_some_and(|e| e > chrono::Utc::now());
        if has_active_sub {
            (self.ai_seconds_limit - self.ai_seconds_used).max(0)
        } else {
            0
        }
    }

    /// Total available seconds (monthly + top-up). Free users with top-ups can still use AI.
    pub fn total_available_seconds(&self) -> i32 {
        self.monthly_remaining() + self.topup_seconds_available
    }

    pub fn can_use_ai(&self, duration_secs: i32) -> bool {
        self.total_available_seconds() >= duration_secs
    }

    pub fn remaining_ai_minutes(&self) -> f64 {
        self.total_available_seconds() as f64 / 60.0
    }

    /// Returns true if this user can use audio extraction (paid tier or any top-up balance).
    pub fn can_extract_audio(&self) -> bool {
        self.tier.has_audio_extraction() || self.topup_seconds_available > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_basic() -> SubscriptionInfo {
        SubscriptionInfo {
            tier: SubscriptionTier::Basic,
            ai_seconds_used: 0,
            ai_seconds_limit: 3600,
            topup_seconds_available: 0,
            last_topup_at: None,
            expires_at: Some(chrono::Utc::now() + chrono::TimeDelta::days(30)),
        }
    }

    fn expired_basic() -> SubscriptionInfo {
        SubscriptionInfo {
            tier: SubscriptionTier::Basic,
            ai_seconds_used: 0,
            ai_seconds_limit: 3600,
            topup_seconds_available: 0,
            last_topup_at: None,
            expires_at: Some(chrono::Utc::now() - chrono::TimeDelta::days(1)),
        }
    }

    #[test]
    fn test_tier_display_and_parse_roundtrip() {
        for tier in [SubscriptionTier::Free, SubscriptionTier::Basic, SubscriptionTier::Pro] {
            let s = tier.to_string();
            let parsed: SubscriptionTier = s.parse().expect("should parse");
            assert_eq!(parsed, tier);
        }
    }

    #[test]
    fn test_tier_parse_invalid() {
        assert!("unknown".parse::<SubscriptionTier>().is_err());
        assert!("".parse::<SubscriptionTier>().is_err());
        assert!("Basic".parse::<SubscriptionTier>().is_err()); // case-sensitive
    }

    #[test]
    fn test_tier_limits_and_prices() {
        assert_eq!(SubscriptionTier::Free.ai_seconds_limit(), 0);
        assert_eq!(SubscriptionTier::Free.price_stars(), 0);
        assert_eq!(SubscriptionTier::Basic.ai_seconds_limit(), 3600);
        assert_eq!(SubscriptionTier::Basic.price_stars(), 50);
        assert_eq!(SubscriptionTier::Pro.ai_seconds_limit(), 12000);
        assert_eq!(SubscriptionTier::Pro.price_stars(), 150);
    }

    #[test]
    fn test_tier_audio_extraction() {
        assert!(!SubscriptionTier::Free.has_audio_extraction());
        assert!(SubscriptionTier::Basic.has_audio_extraction());
        assert!(SubscriptionTier::Pro.has_audio_extraction());
    }

    #[test]
    fn test_free_default_has_zero_quota() {
        let sub = SubscriptionInfo::free_default();
        assert_eq!(sub.total_available_seconds(), 0);
        assert_eq!(sub.remaining_ai_minutes(), 0.0);
        assert!(!sub.can_use_ai(1));
    }

    #[test]
    fn test_can_use_ai_zero_duration_is_always_true() {
        let sub = SubscriptionInfo::free_default();
        assert!(sub.can_use_ai(0));
    }

    #[test]
    fn test_active_basic_full_quota() {
        let sub = active_basic();
        assert_eq!(sub.total_available_seconds(), 3600);
        assert!(sub.can_use_ai(3600));
        assert!(!sub.can_use_ai(3601));
    }

    #[test]
    fn test_active_basic_partially_used() {
        let mut sub = active_basic();
        sub.ai_seconds_used = 1800;
        assert_eq!(sub.total_available_seconds(), 1800);
        assert!(sub.can_use_ai(1800));
        assert!(!sub.can_use_ai(1801));
    }

    #[test]
    fn test_active_basic_fully_exhausted() {
        let mut sub = active_basic();
        sub.ai_seconds_used = 3600;
        assert_eq!(sub.total_available_seconds(), 0);
        assert!(!sub.can_use_ai(1));
    }

    #[test]
    fn test_expired_subscription_has_zero_monthly_quota() {
        let sub = expired_basic();
        assert_eq!(sub.total_available_seconds(), 0);
        assert!(!sub.can_use_ai(1));
    }

    #[test]
    fn test_expired_subscription_with_topup_uses_topup_only() {
        let mut sub = expired_basic();
        sub.topup_seconds_available = 600;
        assert_eq!(sub.total_available_seconds(), 600);
        assert!(sub.can_use_ai(600));
        assert!(!sub.can_use_ai(601));
    }

    #[test]
    fn test_free_user_with_topup_can_use_ai() {
        let mut sub = SubscriptionInfo::free_default();
        sub.topup_seconds_available = 3600;
        assert_eq!(sub.total_available_seconds(), 3600);
        assert!(sub.can_use_ai(3600));
        assert!(!sub.can_use_ai(3601));
    }

    #[test]
    fn test_active_subscription_plus_topup_accumulates() {
        let mut sub = active_basic(); // 3600 monthly
        sub.topup_seconds_available = 600;
        assert_eq!(sub.total_available_seconds(), 4200);
        assert!(sub.can_use_ai(4200));
        assert!(!sub.can_use_ai(4201));
    }

    #[test]
    fn test_exhausted_monthly_falls_back_to_topup() {
        let mut sub = active_basic();
        sub.ai_seconds_used = 3600; // monthly exhausted
        sub.topup_seconds_available = 600;
        assert_eq!(sub.total_available_seconds(), 600);
        assert!(sub.can_use_ai(600));
        assert!(!sub.can_use_ai(601));
    }

    #[test]
    fn test_can_extract_audio_free_no_topup() {
        let sub = SubscriptionInfo::free_default();
        assert!(!sub.can_extract_audio());
    }

    #[test]
    fn test_can_extract_audio_free_with_any_topup() {
        let mut sub = SubscriptionInfo::free_default();
        sub.topup_seconds_available = 1;
        assert!(sub.can_extract_audio());
    }

    #[test]
    fn test_can_extract_audio_basic_active() {
        assert!(active_basic().can_extract_audio());
    }

    #[test]
    fn test_can_extract_audio_basic_expired_still_true() {
        // Tier field remains "basic" even when expired; audio is unlimited for paid tiers
        assert!(expired_basic().can_extract_audio());
    }

    #[test]
    fn test_remaining_ai_minutes() {
        let mut sub = active_basic();
        sub.ai_seconds_used = 3600 - 90; // 90 seconds = 1.5 minutes remaining
        assert!((sub.remaining_ai_minutes() - 1.5).abs() < 0.001);
    }

    #[test]
    fn test_pro_tier_full_quota() {
        let sub = SubscriptionInfo {
            tier: SubscriptionTier::Pro,
            ai_seconds_used: 0,
            ai_seconds_limit: 12000,
            topup_seconds_available: 0,
            last_topup_at: None,
            expires_at: Some(chrono::Utc::now() + chrono::TimeDelta::days(30)),
        };
        assert_eq!(sub.total_available_seconds(), 12000);
        assert_eq!((sub.remaining_ai_minutes() - 200.0).abs() < 0.001, true);
    }

    #[test]
    fn test_no_expires_at_means_zero_monthly() {
        // A subscription row with no expires_at has no active sub
        let sub = SubscriptionInfo {
            tier: SubscriptionTier::Basic,
            ai_seconds_used: 0,
            ai_seconds_limit: 3600,
            topup_seconds_available: 0,
            last_topup_at: None,
            expires_at: None, // never set
        };
        assert_eq!(sub.total_available_seconds(), 0);
    }
}

#[derive(Debug, Clone)]
pub struct CallbackContext {
    pub source_url: String,
    pub chat_id: i64,
    pub has_video: bool,
    pub media_duration_secs: Option<i32>,
    pub audio_cache_path: Option<String>,
}
