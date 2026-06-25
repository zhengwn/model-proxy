//! DNS cache with fresh/stale TTL semantics.
//!
//! - Fresh TTL: 5 minutes (entries considered reliable)
//! - Stale TTL: 30 minutes (entries returned but may be outdated)
//!
//! Uses `tokio::net::lookup_host` for actual DNS resolution.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const FRESH_TTL: Duration = Duration::from_secs(5 * 60);
const STALE_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_ENTRIES: usize = 1000;

#[derive(Debug, Clone)]
struct DnsEntry {
    ips: Vec<IpAddr>,
    resolved_at: Instant,
}

struct DnsCacheInner {
    entries: HashMap<String, DnsEntry>,
    insertion_order: VecDeque<String>,
}

/// Thread-safe DNS cache.
#[derive(Clone)]
pub struct DnsCache {
    inner: Arc<RwLock<DnsCacheInner>>,
}

impl DnsCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(DnsCacheInner {
                entries: HashMap::new(),
                insertion_order: VecDeque::new(),
            })),
        }
    }

    /// Resolve a hostname, returning cached results if available (within stale TTL).
    ///
    /// Returns `None` if no cached entry exists or the stale TTL has expired.
    /// If the entry is stale but within the stale TTL window, the IPs are still returned.
    pub async fn resolve(&self, host: &str) -> Option<Vec<IpAddr>> {
        // Check cache first
        {
            let inner = match self.inner.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(entry) = inner.entries.get(host) {
                if entry.resolved_at.elapsed() < STALE_TTL {
                    return Some(entry.ips.clone());
                }
            }
        }

        // Not in cache or stale -- resolve
        let ips = resolve_host(host).await?;
        let entry = DnsEntry {
            ips: ips.clone(),
            resolved_at: Instant::now(),
        };
        {
            let mut inner = match self.inner.write() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if !inner.entries.contains_key(host) {
                inner.insertion_order.push_back(host.to_string());
            }
            inner.entries.insert(host.to_string(), entry);
            // Evict oldest entries if over limit
            while inner.entries.len() > MAX_ENTRIES {
                if let Some(oldest_key) = inner.insertion_order.pop_front() {
                    inner.entries.remove(&oldest_key);
                } else {
                    break;
                }
            }
        }
        Some(ips)
    }

    /// Pre-warm the cache by resolving a list of hosts.
    pub async fn prewarm(&self, hosts: &[&str]) {
        for host in hosts {
            if let Err(e) = self.resolve_and_store(host).await {
                tracing::warn!(host = *host, error = %e, "DNS prewarm failed");
            }
        }
    }

    /// Resolve and store a host, returning the result.
    async fn resolve_and_store(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        let ips = resolve_host(host)
            .await
            .ok_or_else(|| format!("DNS resolution failed for {}", host))?;

        let entry = DnsEntry {
            ips: ips.clone(),
            resolved_at: Instant::now(),
        };
        {
            let mut inner = match self.inner.write() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if !inner.entries.contains_key(host) {
                inner.insertion_order.push_back(host.to_string());
            }
            inner.entries.insert(host.to_string(), entry);
            while inner.entries.len() > MAX_ENTRIES {
                if let Some(oldest_key) = inner.insertion_order.pop_front() {
                    inner.entries.remove(&oldest_key);
                } else {
                    break;
                }
            }
        }
        Ok(ips)
    }

    /// Check if a host has a fresh (within fresh TTL) cached entry.
    pub fn is_fresh(&self, host: &str) -> bool {
        let inner = match self.inner.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner
            .entries
            .get(host)
            .map(|e| e.resolved_at.elapsed() < FRESH_TTL)
            .unwrap_or(false)
    }

    /// Get the number of cached entries.
    pub fn len(&self) -> usize {
        let inner = match self.inner.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.entries.len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all cached entries.
    pub fn clear(&self) {
        let mut inner = match self.inner.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.entries.clear();
        inner.insertion_order.clear();
    }
}

impl Default for DnsCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Perform actual DNS resolution using tokio.
async fn resolve_host(host: &str) -> Option<Vec<IpAddr>> {
    // tokio::net::lookup_host expects "host:port" format
    let addr_with_port = if host.contains(':') {
        host.to_string()
    } else {
        format!("{}:0", host)
    };

    match tokio::net::lookup_host(&addr_with_port).await {
        Ok(addrs) => {
            let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
            if ips.is_empty() {
                None
            } else {
                Some(ips)
            }
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_cache_is_empty() {
        let cache = DnsCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn fresh_check_on_empty_cache() {
        let cache = DnsCache::new();
        assert!(!cache.is_fresh("example.com"));
    }
}
