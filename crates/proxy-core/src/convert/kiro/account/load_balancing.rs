//! Load balancing mode for Kiro account selection.

/// Load balancing mode for account selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadBalancingMode {
    /// Sticky to current account, fall back on failure
    Priority,
    /// Round-robin across available accounts
    Balanced,
    /// Composite scoring: health + inflight + usage balance + idle time + latency + expiry
    Smart,
}

impl LoadBalancingMode {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "balanced" => Self::Balanced,
            "smart" => Self::Smart,
            _ => Self::Priority,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::Balanced => "balanced",
            Self::Smart => "smart",
        }
    }
}
