//! Flow Monitor for tracking Kiro API request/response flows.
//!
//! Provides per-request tracking with protocol, model, status, timing,
//! and optional bookmarks/tags for debugging.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::info;

/// Flow status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FlowStatus {
    InProgress,
    Success,
    Failed,
    Timeout,
    RateLimited,
}

/// Protocol type for the flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FlowProtocol {
    Anthropic,
    OpenAI,
    Responses,
}

impl std::fmt::Display for FlowProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlowProtocol::Anthropic => write!(f, "anthropic"),
            FlowProtocol::OpenAI => write!(f, "openai"),
            FlowProtocol::Responses => write!(f, "responses"),
        }
    }
}

/// A tracked request/response flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flow {
    pub id: String,
    pub protocol: FlowProtocol,
    pub model: String,
    pub provider: String,
    pub status: FlowStatus,
    pub request_id: String,
    pub account_id: Option<String>,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub upstream_ms: Option<u64>,
    pub total_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub error_message: Option<String>,
    pub bookmarked: bool,
    pub notes: Vec<String>,
    pub tags: Vec<String>,
    /// Request body size in bytes
    pub request_size: Option<usize>,
    /// Response body size in bytes
    pub response_size: Option<usize>,
}

/// Flow statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowStats {
    pub total_flows: usize,
    pub active_flows: usize,
    pub success_count: usize,
    pub failed_count: usize,
    pub timeout_count: usize,
    pub rate_limited_count: usize,
    pub avg_latency_ms: f64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub by_protocol: HashMap<String, usize>,
    pub by_model: HashMap<String, usize>,
}

/// Flow monitor for tracking request/response flows.
pub struct FlowMonitor {
    flows: Vec<Flow>,
    max_flows: usize,
}

impl FlowMonitor {
    pub fn new(max_flows: usize) -> Self {
        Self {
            flows: Vec::new(),
            max_flows,
        }
    }

    /// Start tracking a new flow.
    pub fn start_flow(
        &mut self,
        protocol: FlowProtocol,
        model: &str,
        provider: &str,
        request_id: &str,
    ) -> String {
        let flow_id = format!("flow_{}_{}", now_epoch_secs(), self.flows.len());

        let flow = Flow {
            id: flow_id.clone(),
            protocol,
            model: model.to_string(),
            provider: provider.to_string(),
            status: FlowStatus::InProgress,
            request_id: request_id.to_string(),
            account_id: None,
            started_at: now_epoch_secs(),
            completed_at: None,
            upstream_ms: None,
            total_ms: None,
            input_tokens: None,
            output_tokens: None,
            error_message: None,
            bookmarked: false,
            notes: Vec::new(),
            tags: Vec::new(),
            request_size: None,
            response_size: None,
        };

        self.flows.push(flow);

        // Evict oldest if over limit
        while self.flows.len() > self.max_flows {
            self.flows.remove(0);
        }

        flow_id
    }

    /// Complete a flow with success status.
    pub fn complete_flow(
        &mut self,
        flow_id: &str,
        upstream_ms: u64,
        total_ms: u64,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) {
        if let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) {
            flow.status = FlowStatus::Success;
            flow.completed_at = Some(now_epoch_secs());
            flow.upstream_ms = Some(upstream_ms);
            flow.total_ms = Some(total_ms);
            flow.input_tokens = input_tokens;
            flow.output_tokens = output_tokens;
        }
    }

    /// Mark a flow as failed.
    pub fn fail_flow(&mut self, flow_id: &str, error: &str, status: FlowStatus) {
        if let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) {
            flow.status = status;
            flow.completed_at = Some(now_epoch_secs());
            flow.error_message = Some(error.to_string());
        }
    }

    /// Set account ID for a flow.
    pub fn set_account(&mut self, flow_id: &str, account_id: &str) {
        if let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) {
            flow.account_id = Some(account_id.to_string());
        }
    }

    /// Set request/response sizes.
    pub fn set_sizes(&mut self, flow_id: &str, request_size: usize, response_size: usize) {
        if let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) {
            flow.request_size = Some(request_size);
            flow.response_size = Some(response_size);
        }
    }

    /// Bookmark a flow.
    pub fn bookmark(&mut self, flow_id: &str) {
        if let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) {
            flow.bookmarked = !flow.bookmarked;
        }
    }

    /// Add a note to a flow.
    pub fn add_note(&mut self, flow_id: &str, note: &str) {
        if let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) {
            flow.notes.push(note.to_string());
        }
    }

    /// Add a tag to a flow.
    pub fn add_tag(&mut self, flow_id: &str, tag: &str) {
        if let Some(flow) = self.flows.iter_mut().find(|f| f.id == flow_id) {
            if !flow.tags.contains(&tag.to_string()) {
                flow.tags.push(tag.to_string());
            }
        }
    }

    /// Get a flow by ID.
    pub fn get_flow(&self, flow_id: &str) -> Option<&Flow> {
        self.flows.iter().find(|f| f.id == flow_id)
    }

    /// Query flows with optional filters.
    pub fn query_flows(
        &self,
        protocol: Option<&str>,
        model: Option<&str>,
        status: Option<&str>,
        bookmarked: Option<bool>,
        limit: usize,
    ) -> Vec<&Flow> {
        self.flows
            .iter()
            .rev() // Most recent first
            .filter(|f| {
                if let Some(p) = protocol {
                    if f.protocol.to_string() != p {
                        return false;
                    }
                }
                if let Some(m) = model {
                    if f.model != m {
                        return false;
                    }
                }
                if let Some(s) = status {
                    let status_str = match &f.status {
                        FlowStatus::InProgress => "in_progress",
                        FlowStatus::Success => "success",
                        FlowStatus::Failed => "failed",
                        FlowStatus::Timeout => "timeout",
                        FlowStatus::RateLimited => "rate_limited",
                    };
                    if status_str != s {
                        return false;
                    }
                }
                if let Some(b) = bookmarked {
                    if f.bookmarked != b {
                        return false;
                    }
                }
                true
            })
            .take(limit)
            .collect()
    }

    /// Get flow statistics.
    pub fn get_stats(&self) -> FlowStats {
        let mut by_protocol: HashMap<String, usize> = HashMap::new();
        let mut by_model: HashMap<String, usize> = HashMap::new();
        let mut total_latency: u64 = 0;
        let mut latency_count: usize = 0;

        let mut stats = FlowStats {
            total_flows: self.flows.len(),
            active_flows: 0,
            success_count: 0,
            failed_count: 0,
            timeout_count: 0,
            rate_limited_count: 0,
            avg_latency_ms: 0.0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            by_protocol: HashMap::new(),
            by_model: HashMap::new(),
        };

        for flow in &self.flows {
            match flow.status {
                FlowStatus::InProgress => stats.active_flows += 1,
                FlowStatus::Success => stats.success_count += 1,
                FlowStatus::Failed => stats.failed_count += 1,
                FlowStatus::Timeout => stats.timeout_count += 1,
                FlowStatus::RateLimited => stats.rate_limited_count += 1,
            }

            *by_protocol.entry(flow.protocol.to_string()).or_insert(0) += 1;
            *by_model.entry(flow.model.clone()).or_insert(0) += 1;

            if let Some(ms) = flow.total_ms {
                total_latency += ms;
                latency_count += 1;
            }

            stats.total_input_tokens += flow.input_tokens.unwrap_or(0);
            stats.total_output_tokens += flow.output_tokens.unwrap_or(0);
        }

        if latency_count > 0 {
            stats.avg_latency_ms = total_latency as f64 / latency_count as f64;
        }
        stats.by_protocol = by_protocol;
        stats.by_model = by_model;

        stats
    }
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub type SharedFlowMonitor = Arc<Mutex<FlowMonitor>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_lifecycle() {
        let mut monitor = FlowMonitor::new(100);
        let id = monitor.start_flow(FlowProtocol::Anthropic, "claude-sonnet-4.5", "kiro", "req_1");
        assert!(!id.is_empty());

        monitor.complete_flow(&id, 500, 1000, Some(100), Some(50));
        let flow = monitor.get_flow(&id).unwrap();
        assert_eq!(flow.status, FlowStatus::Success);
        assert_eq!(flow.upstream_ms, Some(500));
    }

    #[test]
    fn flow_fail() {
        let mut monitor = FlowMonitor::new(100);
        let id = monitor.start_flow(FlowProtocol::OpenAI, "gpt-4", "kiro", "req_2");
        monitor.fail_flow(&id, "timeout", FlowStatus::Timeout);
        let flow = monitor.get_flow(&id).unwrap();
        assert_eq!(flow.status, FlowStatus::Timeout);
    }

    #[test]
    fn flow_query_filters() {
        let mut monitor = FlowMonitor::new(100);
        monitor.start_flow(FlowProtocol::Anthropic, "model-a", "kiro", "r1");
        monitor.start_flow(FlowProtocol::OpenAI, "model-b", "kiro", "r2");
        monitor.start_flow(FlowProtocol::Anthropic, "model-a", "kiro", "r3");

        let anthropic = monitor.query_flows(Some("anthropic"), None, None, None, 10);
        assert_eq!(anthropic.len(), 2);

        let model_b = monitor.query_flows(None, Some("model-b"), None, None, 10);
        assert_eq!(model_b.len(), 1);
    }

    #[test]
    fn flow_stats() {
        let mut monitor = FlowMonitor::new(100);
        let id1 = monitor.start_flow(FlowProtocol::Anthropic, "m1", "k", "r1");
        let id2 = monitor.start_flow(FlowProtocol::OpenAI, "m2", "k", "r2");
        monitor.complete_flow(&id1, 100, 200, Some(50), Some(20));
        monitor.fail_flow(&id2, "err", FlowStatus::Failed);

        let stats = monitor.get_stats();
        assert_eq!(stats.total_flows, 2);
        assert_eq!(stats.success_count, 1);
        assert_eq!(stats.failed_count, 1);
        assert_eq!(stats.total_input_tokens, 50);
    }

    #[test]
    fn flow_bookmark_and_notes() {
        let mut monitor = FlowMonitor::new(100);
        let id = monitor.start_flow(FlowProtocol::Anthropic, "m", "k", "r");
        monitor.bookmark(&id);
        assert!(monitor.get_flow(&id).unwrap().bookmarked);
        monitor.add_note(&id, "test note");
        assert_eq!(monitor.get_flow(&id).unwrap().notes.len(), 1);
        monitor.add_tag(&id, "debug");
        assert!(monitor.get_flow(&id).unwrap().tags.contains(&"debug".to_string()));
    }
}
