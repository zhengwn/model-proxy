export interface ServerConfig {
  port: number;
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

export interface ProviderConfig {
  name: string;
  base_url: string;
  api_key: string;
  model: string;
  format: "openai" | "anthropic";
  quirks: ProviderQuirks;
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
