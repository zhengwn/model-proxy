export interface ServerConfig {
  port: number;
  host?: string;
  api_key?: string;
  max_body_bytes?: number;
  max_concurrent_requests?: number;
}

export interface ProviderQuirks {
  reasoning_all_or_nothing: boolean;
  no_json_schema: boolean;
  supports_reasoning_effort: boolean;
  max_reasoning_effort: string;
}

export interface ModelRoute {
  match: string;
  target: string;
  reasoning_effort?: string;
}

export type ProviderFormat = "openai" | "anthropic" | "kiro";

/** One calendar day's request count, from the `get_daily_usage` command. */
export interface DailyUsage {
  date: string; // YYYY-MM-DD
  count: number;
}

export interface KiroAccountEntry {
  auth_method?: string;
  refresh_token?: string;
  client_id?: string;
  client_secret?: string;
  profile_arn?: string;
  region?: string;
  api_region?: string;
  proxy_url?: string;
  priority?: number;
  disabled?: boolean;
}

export interface KiroConfig {
  auth_method: string;
  refresh_token?: string;
  client_id?: string;
  client_secret?: string;
  profile_arn?: string;
  region: string;
  api_region?: string;
  model_aliases?: Record<string, string>;
  hidden_models?: string[];
  kiro_version?: string;
  proxy_url?: string;
  thinking_mode?: string;
  web_search_enabled?: boolean;
  accounts?: KiroAccountEntry[];
  load_balancing_mode?: string;
  agentic_prompt_injection?: boolean;
  first_token_timeout?: number;
  streaming_read_timeout?: number;
  first_token_max_retries?: number;
  quota_cooldown_secs?: number;
  health_score_decay?: number;
  health_score_recovery?: number;
  preferred_endpoint?: string;
  endpoint_fallback?: boolean;
}

export interface ProviderConfig {
  name: string;
  base_url: string;
  api_key: string;
  model: string;
  format: ProviderFormat;
  quirks: ProviderQuirks;
  model_routes?: ModelRoute[];
  kiro_config?: KiroConfig;
}

export interface ProvidersInfo {
  providers: ProviderConfig[];
  active_provider: string;
}

export interface Config {
  server: ServerConfig;
  active_provider?: string;
  providers: ProviderConfig[];
  model_routes?: ModelRoute[];
  model_routes_enabled?: boolean;
  logging?: LoggingConfig;
  fallback?: FallbackConfig;
}

export interface FallbackConfig {
  enabled: boolean;
  on_status_codes: number[];
  max_attempts: number;
}

export interface TestProviderResult {
  success: boolean;
  latency_ms: number;
  model?: string;
  error?: string;
}

export interface ServiceStatus {
  running: boolean;
  listen_addr?: string;
  started_at?: string;
  total_requests: number;
  failed_requests: number;
  error_message?: string;
}

export interface LogEntry {
  id: string;
  timestamp: string;
  method: string;
  path: string;
  provider: string;
  model: string;
  requested_model?: string;
  status: number;
  duration_ms: number;
  proxy_overhead_ms?: number;
  ttft_ms?: number;
  is_stream: boolean;
  error_message?: string;
  request_body?: string;
  response_body?: string;
  token_count?: number;
}

export interface LoggingConfig {
  enabled: boolean;
  level: "all" | "errors_only";
  log_dir?: string;
  record_body: boolean;
  max_body_bytes: number;
  retention_days: number;
}

// ---- Kiro management types ----

export interface KiroCredential {
  id: string;
  priority: number;
  disabled: boolean;
  failure_count: number;
  is_current: boolean;
  is_available: boolean;
  auth_method: string;
  total_requests: number;
  successful_requests: number;
  failed_requests: number;
  proxy_url?: string;
  region: string;
  health_score: number;
}

export interface KiroCredentialFull {
  id: string;
  priority: number;
  disabled: boolean;
  proxy_url?: string;
  credentials?: {
    auth_method: string;
    region: string;
    api_region: string;
  };
  circuit: {
    state: string;
    failures: number;
    health_score: number;
    total_requests: number;
    successful_requests: number;
    failed_requests: number;
  };
  inflight_count: number;
  latency_ema: number;
  last_success_at?: string;
}

export interface KiroBatchResult {
  success: boolean;
  results: string[];
}

export interface KiroTestResult {
  success: boolean;
  latency_ms?: number;
  status?: number;
  error?: string;
}

export interface KiroEndpointHealth {
  endpoints: {
    endpoint: string;
    success_count: number;
    fail_count: number;
    latency_ema_ms: number;
    consecutive_errors: number;
    success_rate: number;
  }[];
}

export interface KiroThinkingConfig {
  mode: string;
}

export interface KiroSettings {
  preferred_endpoint?: string;
  endpoint_fallback?: boolean;
}

export interface KiroSsoStartResult {
  session_id: string;
  authorize_url: string;
  expires_in: number;
}

export interface KiroSsoCompleteResult {
  success: boolean;
  credential_id?: string;
  error?: string;
}
