//! IP filter middleware: blacklist and sliding-window request counting.
//!
//! Provides:
//! - IP banning / unbanning via a shared `IpFilter` state.
//! - Per-IP request rate tracking using a 60-second sliding window.
//! - Extraction of client IP from `X-Forwarded-For` (first entry) or `X-Real-IP`.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const SLIDING_WINDOW: Duration = Duration::from_secs(60);

/// Shared, thread-safe IP filter state.
#[derive(Clone)]
pub struct IpFilter {
    inner: Arc<RwLock<IpFilterInner>>,
}

struct IpFilterInner {
    banned: HashSet<IpAddr>,
    requests: HashMap<IpAddr, VecDeque<Instant>>,
}

impl IpFilter {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(IpFilterInner {
                banned: HashSet::new(),
                requests: HashMap::new(),
            })),
        }
    }

    /// Ban an IP address.
    pub fn ban_ip(&self, ip: IpAddr) {
        let mut inner = self.inner.write().unwrap();
        inner.banned.insert(ip);
    }

    /// Unban an IP address.
    pub fn unban_ip(&self, ip: IpAddr) {
        let mut inner = self.inner.write().unwrap();
        inner.banned.remove(&ip);
    }

    /// Check whether an IP is currently banned.
    pub fn is_banned(&self, ip: IpAddr) -> bool {
        let inner = self.inner.read().unwrap();
        inner.banned.contains(&ip)
    }

    /// Return a snapshot of all banned IPs.
    pub fn list_banned(&self) -> Vec<IpAddr> {
        let inner = self.inner.read().unwrap();
        inner.banned.iter().copied().collect()
    }

    /// Record a request for the given IP and return the current count within
    /// the 60-second sliding window.
    pub fn record_request(&self, ip: IpAddr) -> u64 {
        let mut inner = self.inner.write().unwrap();
        let now = Instant::now();
        let entry = inner.requests.entry(ip).or_insert_with(VecDeque::new);

        // Evict timestamps outside the window
        while let Some(&front) = entry.front() {
            if now.duration_since(front) > SLIDING_WINDOW {
                entry.pop_front();
            } else {
                break;
            }
        }

        entry.push_back(now);
        entry.len() as u64
    }

    /// Get the current request count for an IP within the sliding window
    /// without recording a new request.
    pub fn request_count(&self, ip: IpAddr) -> u64 {
        let inner = self.inner.read().unwrap();
        match inner.requests.get(&ip) {
            Some(entry) => {
                let now = Instant::now();
                entry
                    .iter()
                    .filter(|ts| now.duration_since(**ts) <= SLIDING_WINDOW)
                    .count() as u64
            }
            None => 0,
        }
    }
}

impl Default for IpFilter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------- Client IP extraction ----------

/// Extract the client IP from request headers.
///
/// Prefers the first entry in `X-Forwarded-For`, then falls back to `X-Real-IP`.
pub fn extract_client_ip<B>(req: &Request<B>) -> Option<IpAddr> {
    // X-Forwarded-For: client, proxy1, proxy2 ...
    if let Some(val) = req.headers().get("x-forwarded-for") {
        if let Ok(s) = val.to_str() {
            if let Some(first) = s.split(',').next() {
                if let Ok(ip) = first.trim().parse::<IpAddr>() {
                    return Some(ip);
                }
            }
        }
    }
    // X-Real-IP
    if let Some(val) = req.headers().get("x-real-ip") {
        if let Ok(s) = val.to_str() {
            if let Ok(ip) = s.parse::<IpAddr>() {
                return Some(ip);
            }
        }
    }
    None
}

// ---------- axum middleware ----------

/// axum `from_fn` middleware that applies IP filtering.
///
/// Rejects banned IPs with 403. Records each request for rate tracking.
pub async fn ip_filter_middleware(
    State(filter): State<IpFilter>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let client_ip = extract_client_ip(&req);

    if let Some(ip) = client_ip {
        // Check ban list
        if filter.is_banned(ip) {
            return (StatusCode::FORBIDDEN, format!("IP {} is banned", ip)).into_response();
        }

        // Record request for rate tracking
        filter.record_request(ip);
    }

    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ban_and_unban() {
        let filter = IpFilter::new();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        assert!(!filter.is_banned(ip));
        filter.ban_ip(ip);
        assert!(filter.is_banned(ip));

        let banned = filter.list_banned();
        assert_eq!(banned.len(), 1);
        assert!(banned.contains(&ip));

        filter.unban_ip(ip);
        assert!(!filter.is_banned(ip));
        assert!(filter.list_banned().is_empty());
    }

    #[test]
    fn request_counting() {
        let filter = IpFilter::new();
        let ip: IpAddr = "192.168.1.1".parse().unwrap();

        assert_eq!(filter.request_count(ip), 0);
        filter.record_request(ip);
        assert_eq!(filter.request_count(ip), 1);
        filter.record_request(ip);
        filter.record_request(ip);
        assert_eq!(filter.request_count(ip), 3);
    }

    #[test]
    fn empty_filter_lists_nothing() {
        let filter = IpFilter::new();
        assert!(filter.list_banned().is_empty());
    }
}
