//! Kiro auth acquisition and multi-endpoint dispatch with retry logic.

use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, warn};

use crate::convert::kiro::auth::KiroAuthManager;
use crate::server::state::elapsed_ms;
use crate::error::{AppError, Result};

// ---- Kiro shared dispatch ----

/// Auth info obtained from Kiro credential management.
pub(crate) struct KiroAuthInfo {
    pub(crate) token: String,
    pub(crate) amz_user_agent: String,
    pub(crate) user_agent: String,
    pub(crate) is_account_managed: bool,
    pub(crate) acct_idx: usize,
    pub(crate) profile_arn: Option<String>,
}

/// Result of a successful Kiro upstream dispatch.
pub(crate) struct KiroDispatchResult {
    pub(crate) response: reqwest::Response,
    pub(crate) upstream_headers_ms: u128,
    /// Account/endpoint metadata retained for diagnostics and future routing logic.
    #[allow(dead_code)]
    pub(crate) acct_idx: usize,
    #[allow(dead_code)]
    pub(crate) is_account_managed: bool,
    #[allow(dead_code)]
    pub(crate) endpoint_name: String,
}

/// Build a `KiroAuthInfo` from an auth manager after fetching a valid token.
///
/// Centralizes the construction shared by all auth-acquisition branches
/// (tenant token, multi-account, single-account, flat-config).
async fn auth_info_from_manager(
    auth: &mut KiroAuthManager,
    is_account_managed: bool,
    acct_idx: usize,
) -> Result<KiroAuthInfo> {
    let token = auth.get_valid_token().await?;
    Ok(KiroAuthInfo {
        token,
        amz_user_agent: auth.amz_user_agent(),
        user_agent: auth.user_agent(),
        is_account_managed,
        acct_idx,
        profile_arn: auth.profile_arn().map(|s| s.to_string()),
    })
}

/// Acquire auth info from the Kiro credential manager.
pub(crate) async fn acquire_kiro_auth(
    state: &crate::server::state::AppState,
    kiro_config: &crate::config::KiroConfig,
    tenant_refresh_token: Option<&str>,
) -> Result<KiroAuthInfo> {
    // 1. Tenant-supplied refresh token: build an ad-hoc social auth manager.
    if let Some(tenant_token) = tenant_refresh_token {
        let mut tenant_cfg = kiro_config.clone();
        tenant_cfg.auth_method = "social".to_string();
        tenant_cfg.refresh_token = Some(tenant_token.to_string());
        let mut auth = KiroAuthManager::new(&tenant_cfg, state.client.clone());
        return auth_info_from_manager(&mut auth, false, 0).await;
    }

    // 2. Multi-account manager: pick an available account (with self-heal).
    if let Some(account_mgr) = state.kiro_account_manager() {
        let mut mgr = account_mgr.lock().await;
        let (acct_idx, _id, auth_arc_ref) = match mgr.get_available_account(&[]) {
            Some(triple) => triple,
            None => {
                if mgr.self_heal() {
                    mgr.get_available_account(&[]).ok_or_else(|| {
                        AppError::Request("所有 Kiro 账户不可用 (已尝试自愈)".to_string())
                    })?
                } else {
                    return Err(AppError::Request("所有 Kiro 账户不可用".to_string()));
                }
            }
        };
        let auth_arc = auth_arc_ref.clone();
        drop(mgr);
        let mut auth = auth_arc.lock().await;
        return auth_info_from_manager(&mut auth, true, acct_idx).await;
    }

    // 3. Single-account auth manager.
    if let Some(auth_arc) = state.kiro_auth() {
        let mut auth = auth_arc.lock().await;
        return auth_info_from_manager(&mut auth, false, 0).await;
    }

    // 4. Fallback: build a manager directly from the flat config.
    let mut auth = KiroAuthManager::new(kiro_config, state.client.clone());
    auth_info_from_manager(&mut auth, false, 0).await
}

/// Outcome of attempting a single Kiro endpoint.
enum EndpointOutcome {
    /// Endpoint succeeded; dispatch is complete.
    Success(Box<KiroDispatchResult>),
    /// Endpoint failed in a recoverable way; try the next endpoint.
    /// Carries a human-readable reason for diagnostics.
    TryNext(String),
    /// Endpoint returned a non-success status; return immediately to client.
    Fatal(AppError),
}

/// Apply circuit-breaker / cooldown bookkeeping after an upstream error response.
async fn record_account_error(
    state: &crate::server::state::AppState,
    auth_info: &KiroAuthInfo,
    status_code: u16,
    err_body: &str,
) {
    use crate::convert::kiro::account::ErrorClass;

    if !auth_info.is_account_managed {
        return;
    }
    let Some(mgr_arc) = state.kiro_account_manager() else {
        return;
    };
    let error_class = crate::convert::kiro::account::classify_error(status_code, err_body);
    let mut mgr = mgr_arc.lock().await;
    match error_class {
        ErrorClass::Suspended => {
            if let Some(acct_id) = mgr.account_id_at(auth_info.acct_idx) {
                warn!(
                    account_id = acct_id.as_str(),
                    "账户已自动禁用 (temporarily_suspended)"
                );
                mgr.set_disabled(&acct_id, true);
            }
        }
        ErrorClass::Recoverable => {
            mgr.record_failure_at(auth_info.acct_idx, error_class.clone());
            if status_code == 429 {
                if let Some(acct_id) = mgr.account_id_at(auth_info.acct_idx) {
                    mgr.set_cooldown_for_account(&acct_id);
                }
            }
        }
        ErrorClass::Fatal => {}
    }
}

/// Set a cooldown on the current managed account (used on 429 before trying next endpoint).
async fn cooldown_current_account(state: &crate::server::state::AppState, auth_info: &KiroAuthInfo) {
    if !auth_info.is_account_managed {
        return;
    }
    if let Some(mgr) = state.kiro_account_manager() {
        let mut mgr = mgr.lock().await;
        if let Some(acct_id) = mgr.account_id_at(auth_info.acct_idx) {
            mgr.set_cooldown_for_account(&acct_id);
        }
    }
}

/// Release the inflight counter for a managed account, recording latency.
async fn release_inflight(
    state: &crate::server::state::AppState,
    auth_info: &KiroAuthInfo,
    latency_ms: f64,
) {
    if !auth_info.is_account_managed {
        return;
    }
    if let Some(mgr) = state.kiro_account_manager() {
        mgr.lock()
            .await
            .release_inflight_with_latency_at(auth_info.acct_idx, latency_ms);
    }
}

/// Attempt a single endpoint: send the request (with retries) and classify the result.
#[allow(clippy::too_many_arguments)]
async fn try_endpoint(
    state: &crate::server::state::AppState,
    endpoint: &crate::convert::kiro::endpoint::KiroEndpoint,
    region: &str,
    kiro_client: &reqwest::Client,
    payload_bytes: &[u8],
    auth_info: &KiroAuthInfo,
    request_id: &str,
    timeout: Option<Duration>,
) -> EndpointOutcome {
    use crate::convert::kiro::endpoint::{build_endpoint_headers, build_endpoint_url};

    let url = build_endpoint_url(endpoint, region);
    let headers = build_endpoint_headers(
        endpoint,
        &auth_info.token,
        &auth_info.amz_user_agent,
        &auth_info.user_agent,
    );

    let upstream_start = Instant::now();

    let upstream_resp = match kiro_request_with_retry(
        kiro_client,
        &url,
        payload_bytes,
        &headers,
        state.kiro_auth(),
        request_id,
        timeout,
    )
    .await
    {
        Ok(resp) => resp,
        Err(e) => {
            if let Some(tracker) = state.endpoint_health() {
                tracker.record_failure(endpoint.name);
            }
            return EndpointOutcome::TryNext(e.to_string());
        }
    };

    let status = upstream_resp.status();
    let status_code = status.as_u16();

    // 429: record cooldown and try the next endpoint.
    if status_code == 429 {
        if let Some(tracker) = state.endpoint_health() {
            tracker.record_failure(endpoint.name);
        }
        cooldown_current_account(state, auth_info).await;
        warn!(
            request_id,
            endpoint = endpoint.name,
            status = status_code,
            "Kiro endpoint 返回 429，尝试下一个"
        );
        return EndpointOutcome::TryNext(format!("429 from {}", endpoint.name));
    }

    // 401/403/402: record failure but do NOT try the next endpoint.
    if status_code == 401 || status_code == 403 || status_code == 402 {
        if let Some(tracker) = state.endpoint_health() {
            tracker.record_failure(endpoint.name);
        }
    }

    // Release inflight tracking now that we have a final response from this endpoint.
    release_inflight(state, auth_info, upstream_start.elapsed().as_millis() as f64).await;

    let upstream_headers_ms = elapsed_ms(upstream_start);

    if status.is_success() {
        if auth_info.is_account_managed {
            if let Some(mgr) = state.kiro_account_manager() {
                mgr.lock().await.record_success_at(auth_info.acct_idx);
            }
        }
        if let Some(tracker) = state.endpoint_health() {
            tracker.record_success(endpoint.name, upstream_headers_ms as f64);
        }
        return EndpointOutcome::Success(Box::new(KiroDispatchResult {
            response: upstream_resp,
            acct_idx: auth_info.acct_idx,
            is_account_managed: auth_info.is_account_managed,
            upstream_headers_ms,
            endpoint_name: endpoint.name.to_string(),
        }));
    }

    // Non-success status: update circuit breaker and return the error to client.
    let err_body = upstream_resp.text().await.unwrap_or_default();
    record_account_error(state, auth_info, status_code, &err_body).await;
    error!(
        request_id,
        endpoint = endpoint.name,
        status = status_code,
        body = err_body.as_str(),
        "Kiro 上游返回错误"
    );
    EndpointOutcome::Fatal(AppError::UpstreamStatus(status_code, err_body))
}

/// Dispatch a Kiro payload to the upstream API with multi-endpoint fallback.
///
/// Tries each endpoint in priority order. On 429, records failure and tries next.
/// On 401/403/402, returns immediately (no cross-endpoint retry).
pub(crate) async fn dispatch_kiro_request(
    state: &crate::server::state::AppState,
    _kiro_payload: &Value,
    payload_bytes: &[u8],
    auth_info: &KiroAuthInfo,
    kiro_config: &crate::config::KiroConfig,
    request_id: &str,
    is_stream: bool,
) -> Result<KiroDispatchResult> {
    use crate::convert::kiro::endpoint::{get_sorted_endpoints, PreferredEndpoint};

    let region = kiro_config
        .api_region
        .as_deref()
        .unwrap_or(&kiro_config.region);

    let preferred = PreferredEndpoint::from_str_opt(kiro_config.preferred_endpoint.as_deref());
    let fallback = kiro_config.endpoint_fallback.unwrap_or(true);
    let endpoints = get_sorted_endpoints(preferred, fallback);

    // Track inflight for account manager
    if auth_info.is_account_managed {
        if let Some(mgr) = state.kiro_account_manager() {
            mgr.lock().await.increment_inflight_at(auth_info.acct_idx);
        }
    }

    // Acquire concurrency permit
    let _permit = if let Some(ref sem) = state.concurrency_semaphore {
        match sem.try_acquire() {
            Ok(permit) => Some(permit),
            Err(_) => {
                state.inc_failed_requests();
                if auth_info.is_account_managed {
                    if let Some(mgr) = state.kiro_account_manager() {
                        mgr.lock().await.release_inflight_at(auth_info.acct_idx);
                    }
                }
                return Err(AppError::TooManyRequests);
            }
        }
    } else {
        None
    };

    // Rate limiter check
    if let Some(rl) = state.rate_limiter() {
        let mut limiter = rl.lock().await;
        if let Err(wait) = limiter.check("kiro") {
            warn!(wait_ms = wait.as_millis(), "Kiro 请求被限流，等待");
            tokio::time::sleep(wait).await;
        }
    }

    let timeout = if !is_stream {
        Some(Duration::from_secs(crate::server::state::NON_STREAM_REQUEST_TIMEOUT_SECS))
    } else {
        None
    };

    // Build proxy-aware client (cached for connection-pool reuse).
    let proxy_url = state.kiro_account_manager().and_then(|m| {
        m.try_lock()
            .ok()
            .and_then(|mgr| mgr.current_proxy_url().map(|s| s.to_string()))
    });
    let kiro_client = match state.kiro.as_ref() {
        Some(kiro) => match kiro.client_for_proxy(proxy_url.as_deref()).await {
            Ok(client) => client,
            Err(e) => {
                // Release the inflight counter we incremented above so a bad
                // proxy config doesn't permanently inflate the account's load.
                if auth_info.is_account_managed {
                    if let Some(mgr) = state.kiro_account_manager() {
                        mgr.lock().await.release_inflight_at(auth_info.acct_idx);
                    }
                }
                state.inc_failed_requests();
                return Err(e);
            }
        },
        None => state.client.clone(),
    };

    let mut last_error: Option<String> = None;

    for endpoint in &endpoints {
        match try_endpoint(
            state,
            endpoint,
            region,
            &kiro_client,
            payload_bytes,
            auth_info,
            request_id,
            timeout,
        )
        .await
        {
            EndpointOutcome::Success(result) => return Ok(*result),
            EndpointOutcome::Fatal(err) => return Err(err),
            EndpointOutcome::TryNext(reason) => {
                last_error = Some(reason);
                continue;
            }
        }
    }

    // All endpoints exhausted
    Err(AppError::Request(
        last_error.unwrap_or_else(|| "所有 Kiro endpoint 均不可用".to_string()),
    ))
}

/// Send a request to the Kiro API with retry logic.
/// - 403: force_refresh token + retry once
/// - 429/5xx: exponential backoff (1s × 2^attempt), max 3 retries
async fn kiro_request_with_retry(
    client: &reqwest::Client,
    url: &str,
    payload: &[u8],
    headers: &[(String, String)],
    auth: Option<&Arc<tokio::sync::Mutex<KiroAuthManager>>>,
    request_id: &str,
    timeout: Option<Duration>,
) -> Result<reqwest::Response> {
    const MAX_RETRIES: u32 = 3;
    const BASE_DELAY_SECS: f64 = 1.0;

    let mut last_resp: Option<reqwest::Response> = None;

    for attempt in 0..=MAX_RETRIES {
        // Build request
        let mut req = client.post(url);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req = req.body(payload.to_vec());
        if let Some(t) = timeout {
            req = req.timeout(t);
        }

        // Send
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                if attempt < MAX_RETRIES {
                    let delay = BASE_DELAY_SECS * 2.0_f64.powi(attempt as i32);
                    warn!(
                        request_id,
                        attempt,
                        error = %e,
                        delay_secs = delay,
                        "Kiro 请求发送失败，重试中"
                    );
                    tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                    continue;
                }
                return Err(AppError::Http(e));
            }
        };

        let status = resp.status().as_u16();

        // 403: force refresh token and retry once
        if status == 403 {
            if let Some(auth_arc) = auth {
                if attempt == 0 {
                    warn!(request_id, "Kiro 返回 403，强制刷新 token");
                    if let Ok(mut auth_guard) = auth_arc.try_lock() {
                        let _ = auth_guard.force_refresh().await;
                    }
                    last_resp = Some(resp);
                    continue;
                }
            }
            last_resp = Some(resp);
            break;
        }

        // 429/5xx: exponential backoff
        if status == 429 || (500..600).contains(&status) {
            if attempt < MAX_RETRIES {
                let delay = BASE_DELAY_SECS * 2.0_f64.powi(attempt as i32);
                warn!(
                    request_id,
                    status,
                    attempt,
                    delay_secs = delay,
                    "Kiro 返回 {}，重试中",
                    status
                );
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                last_resp = Some(resp);
                continue;
            }
            last_resp = Some(resp);
            break;
        }

        // Success or non-retryable error: return immediately
        return Ok(resp);
    }

    // Exhausted retries — return last response
    last_resp.ok_or_else(|| AppError::Request("Kiro 重试耗尽且无响应".to_string()))
}
