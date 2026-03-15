use std::fmt;
use std::str::FromStr;

use crate::terms;

/// Invoice payload / product identifier strings used in payments.
pub const PRODUCT_SUB_BASIC: &str = "sub_basic";
pub const PRODUCT_SUB_PRO: &str = "sub_pro";
pub const PRODUCT_TOPUP_60: &str = "topup_60";
/// Price in Telegram Stars for the one-time top-up product.
pub const TOPUP_PRICE_STARS: u32 = 50;
/// AI seconds granted by one top-up purchase (60 minutes).
pub const TOPUP_SECONDS: i32 = 3600;

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

    /// Audio extraction is unlimited for Pro subscribers (no API cost).
    /// Basic subscribers and free users with a top-up balance also get it — checked via SubscriptionInfo.
    pub fn has_audio_extraction(&self) -> bool {
        matches!(self, Self::Pro)
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

    /// Top-up seconds that are still within the expiry window.
    /// Returns 0 if last_topup_at is over TOPUP_EXPIRY_DAYS ago.
    /// If no purchase date is recorded, the balance is honoured (e.g. owner grants).
    fn active_topup_seconds(&self) -> i32 {
        match self.last_topup_at {
            Some(t) if chrono::Utc::now() - t
                < chrono::TimeDelta::days(terms::TOPUP_EXPIRY_DAYS) =>
            {
                self.topup_seconds_available
            }
            None => self.topup_seconds_available,
            _ => 0,
        }
    }

    /// Total available seconds (monthly + active top-up). Free users with unexpired top-ups can use AI.
    pub fn total_available_seconds(&self) -> i32 {
        self.monthly_remaining() + self.active_topup_seconds()
    }

    pub fn can_use_ai(&self, duration_secs: i32) -> bool {
        self.total_available_seconds() >= duration_secs
    }

    pub fn remaining_ai_minutes(&self) -> f64 {
        self.total_available_seconds() as f64 / 60.0
    }

    /// Returns true if this user can extract audio for a video of the given duration.
    /// Pro tier: unlimited, no cost (always true).
    /// Everyone else: requires enough seconds in their balance (monthly or top-up).
    pub fn can_extract_audio(&self, duration_secs: i32) -> bool {
        if self.tier == SubscriptionTier::Pro {
            true
        } else {
            self.total_available_seconds() >= duration_secs
        }
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
        assert!(!SubscriptionTier::Basic.has_audio_extraction());
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
        assert!(!sub.can_extract_audio(60));
    }

    #[test]
    fn test_can_extract_audio_free_with_enough_topup() {
        let mut sub = SubscriptionInfo::free_default();
        sub.topup_seconds_available = 3600;
        sub.last_topup_at = Some(chrono::Utc::now());
        assert!(sub.can_extract_audio(60));
        assert!(!sub.can_extract_audio(3601)); // not enough
    }

    #[test]
    fn test_can_extract_audio_basic_active_has_minutes() {
        // Basic gets audio extraction as long as they have minutes remaining
        assert!(active_basic().can_extract_audio(60));
        assert!(active_basic().can_extract_audio(3600));
        assert!(!active_basic().can_extract_audio(3601)); // over quota
    }

    #[test]
    fn test_can_extract_audio_basic_exhausted_denied() {
        let mut sub = active_basic();
        sub.ai_seconds_used = 3600; // all used up
        assert!(!sub.can_extract_audio(1));
    }

    #[test]
    fn test_can_extract_audio_basic_expired_no_topup() {
        assert!(!expired_basic().can_extract_audio(1));
    }

    #[test]
    fn test_can_extract_audio_pro_unlimited() {
        let sub = SubscriptionInfo {
            tier: SubscriptionTier::Pro,
            ai_seconds_used: 12000, // fully exhausted
            ai_seconds_limit: 12000,
            topup_seconds_available: 0,
            last_topup_at: None,
            expires_at: Some(chrono::Utc::now() + chrono::TimeDelta::days(30)),
        };
        // Pro always gets audio extraction regardless of remaining minutes
        assert!(sub.can_extract_audio(99999));
    }

    #[test]
    fn test_expired_topup_returns_zero() {
        let mut sub = SubscriptionInfo::free_default();
        sub.topup_seconds_available = 3600;
        sub.last_topup_at = Some(chrono::Utc::now() - chrono::TimeDelta::days(366));
        assert_eq!(sub.active_topup_seconds(), 0);
        assert_eq!(sub.total_available_seconds(), 0);
        assert!(!sub.can_extract_audio(1));
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
        assert!((sub.remaining_ai_minutes() - 200.0).abs() < 0.001);
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

