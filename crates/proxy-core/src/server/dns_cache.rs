//! DNS cache with fresh/stale TTL semantics.
//!
//! - Fresh TTL: 5 minutes (entries considered reliable)
//! - Stale TTL: 30 minutes (entries returned but may be outdated)
//!
//! Uses `tokio::net::lookup_host` for actual DNS resolution.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const FRESH_TTL: Duration = Duration::from_secs(5 * 60);
const STALE_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
struct DnsEntry {
    ips: Vec<IpAddr>,
    resolved_at: Instant,
}

/// Thread-safe DNS cache.
#[derive(Clone)]
pub struct DnsCache {
    inner: Arc<RwLock<HashMap<String, DnsEntry>>>,
}

impl DnsCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Resolve a hostname, returning cached results if available (within stale TTL).
    ///
    /// Returns `None` if no cached entry exists or the stale TTL has expired.
    /// If the entry is stale but within the stale TTL window, the IPs are still returned.
    pub async fn resolve(&self, host: &str) -> Option<Vec<IpAddr>> {
        // Check cache first
        {
            let inner = self.inner.read().unwrap();
            if let Some(entry) = inner.get(host) {
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
            let mut inner = self.inner.write().unwrap();
            inner.insert(host.to_string(), entry);
        }
        Some(ips)
    }

    /// Pre-warm the cache by resolving a list of hosts concurrently.
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
            let mut inner = self.inner.write().unwrap();
            inner.insert(host.to_string(), entry);
        }
        Ok(ips)
    }

    /// Check if a host has a fresh (within fresh TTL) cached entry.
    pub fn is_fresh(&self, host: &str) -> bool {
        let inner = self.inner.read().unwrap();
        inner
            .get(host)
            .map(|e| e.resolved_at.elapsed() < FRESH_TTL)
            .unwrap_or(false)
    }

    /// Get the number of cached entries.
    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }

    /// Clear all cached entries.
    pub fn clear(&self) {
        self.inner.write().unwrap().clear();
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

    let result = match tokio::net::lookup_host(&addr_with_port).await {
        Ok(addrs) => {
            let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
            if ips.is_empty() {
                None
            } else {
                Some(ips)
            }
        }
        Err(_) => None,
    };
    result
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
