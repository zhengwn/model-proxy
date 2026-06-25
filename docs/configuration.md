# 配置详解

## 配置文件位置

| 运行模式 | 配置文件路径 |
|----------|-------------|
| 桌面应用 | `%APPDATA%/com.model-proxy.app/config.toml` (Windows) |
| CLI | 可执行文件同级目录的 `config.toml`，或当前工作目录的 `config.toml` |

桌面应用首次启动时配置文件不存在，通过 GUI 保存后自动创建。

## 完整配置参考

```toml
# 当前活跃的 Provider 名称
# 必须匹配某个 [[providers]] 的 name 字段
# 如果省略，使用 providers 列表中的第一个
active_provider = "deepseek"
model_routes_enabled = true

[server]
# 代理服务监听端口
port = 4000
# 监听地址，默认仅本机访问；对外提供服务可设为 "0.0.0.0"
host = "127.0.0.1"

# 客户端认证密钥（可选）
# 设置后，客户端必须通过 x-api-key 或 Authorization: Bearer 提供此值
# api_key = "your-server-api-key"

# 请求体大小上限（字节），默认 64MB
# max_body_bytes = 67108864

# 每个 Provider 的最大并发请求数，0 表示不限制
# max_concurrent_requests = 0

# --- 全局模型路由 ---
# 当客户端请求的 model 名称包含 match 值时，路由到 target 模型
# 按定义顺序匹配，第一个命中的生效

[[model_routes]]
match = "sonnet"           # 匹配模式（大小写不敏感的子串匹配）
target = "deepseek-v4-pro" # 实际使用的模型名
reasoning_effort = "max"   # 可选：覆盖推理强度

[[model_routes]]
match = "haiku"
target = "deepseek-v4-flash"
reasoning_effort = "high"

# --- Provider 定义 ---
# 支持定义多个 Provider，通过 active_provider 或 GUI 切换

[[providers]]
name = "deepseek"                        # 唯一标识名（1-64 字符）
base_url = "https://api.deepseek.com/v1" # API 基础 URL
api_key = "sk-deepseek-key"              # Provider 的 API Key
model = "deepseek-v4-pro"                # 默认模型（未匹配路由时使用）
format = "openai"                        # API 格式：openai、anthropic 或 kiro

# Provider 特殊行为配置
[providers.quirks]
reasoning_all_or_nothing = true    # 历史 assistant 消息是否必须包含 reasoning_content
no_json_schema = true              # 不支持 json_schema，降级为 json_object
supports_reasoning_effort = true   # 是否转发 reasoning_effort 参数
max_reasoning_effort = "max"       # "max"/"adaptive" 映射到的最大推理强度值

[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key = "sk-openai-key"
model = "gpt-4o"
format = "openai"

[[providers]]
name = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "sk-ant-key"
model = "claude-sonnet-4-20250514"
format = "anthropic"

[[providers]]
name = "kiro"
base_url = "https://q.us-east-1.amazonaws.com"
model = "claude-sonnet-4.5"
format = "kiro"

[providers.kiro_config]
auth_method = "social"                   # social | idc | api_key
refresh_token = "your-kiro-refresh-token"
region = "us-east-1"
thinking_mode = "as_reasoning_content"
preferred_endpoint = "auto"
endpoint_fallback = true

# --- 故障回退配置 ---

[fallback]
enabled = false
on_status_codes = [429, 500, 502, 503, 504]
max_attempts = 2

# --- 日志配置 ---

[logging]
enabled = true           # 是否启用请求日志
level = "all"            # 日志级别：all | errors_only
# log_dir = "/path/to/logs"  # 日志目录（默认：应用数据目录/logs）
record_body = false      # 是否记录请求/响应体
max_body_bytes = 4096    # 记录体时的最大字节数
retention_days = 7       # 日志文件保留天数
```

## 字段详解

### server

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `port` | u16 | 必填 | 监听端口 |
| `host` | string | "127.0.0.1" | 监听地址 |
| `api_key` | string? | 无 | 客户端认证密钥，不设置则不鉴权 |
| `max_body_bytes` | usize | 67108864 (64MB) | 请求体大小上限 |
| `max_concurrent_requests` | usize | 0 | 每个 Provider 最大并发请求数，0 表示不限制 |

### providers

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `name` | string | 必填 | 唯一标识，1-64 字符 |
| `base_url` | string | 必填 | API 基础 URL（不含路径） |
| `api_key` | string | 非 Kiro 必填 | Provider API Key |
| `model` | string | 必填 | 默认模型名 |
| `format` | string | "openai" | API 格式：`openai`、`anthropic` 或 `kiro` |
| `quirks` | object | 全 false | 特殊行为开关 |
| `kiro_config` | object? | 无 | Kiro 专用认证和运行配置 |

### providers.quirks

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `reasoning_all_or_nothing` | bool | false | 历史消息是否必须包含 reasoning_content 字段 |
| `no_json_schema` | bool | false | 不支持 json_schema 响应格式，降级为 json_object |
| `supports_reasoning_effort` | bool | false | 是否转发 reasoning_effort 参数 |
| `max_reasoning_effort` | string | "high" | Anthropic "max" 推理强度映射到的值 |

### providers.kiro_config

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `auth_method` | string | 必填 | `social`、`idc` 或 `api_key` |
| `refresh_token` | string? | 无 | social/idc refresh token；api_key 模式下作为 access token |
| `client_id` | string? | 无 | IAM Identity Center client id |
| `client_secret` | string? | 无 | IAM Identity Center client secret |
| `profile_arn` | string? | 无 | AWS profile ARN |
| `region` | string | "us-east-1" | 认证区域 |
| `api_region` | string? | 同 region | Kiro API 区域 |
| `proxy_url` | string? | 无 | HTTP/SOCKS5 代理 |
| `thinking_mode` | string? | "as_reasoning_content" | thinking 处理模式 |
| `preferred_endpoint` | string? | "auto" | `auto`、`kiro`、`codewhisperer` 或 `amazonq` |
| `endpoint_fallback` | bool? | true | 429 时是否降级到其他端点 |
| `accounts` | array? | 无 | 多账户凭据池 |

### model_routes

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `match` | string | 必填 | 匹配模式（子串匹配，大小写不敏感） |
| `target` | string | 必填 | 路由到的目标模型名 |
| `reasoning_effort` | string? | 无 | 可选：覆盖推理强度（low/medium/high/max） |

### logging

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `enabled` | bool | true | 是否启用请求日志 |
| `level` | string | "all" | `all` 记录所有请求，`errors_only` 仅记录 4xx/5xx |
| `log_dir` | string? | 应用数据目录/logs | 日志文件存储目录 |
| `record_body` | bool | false | 是否记录请求/响应体内容 |
| `max_body_bytes` | usize | 4096 | 记录体时截断的最大字节数 |
| `retention_days` | u32 | 7 | 日志文件保留天数，超过后自动删除 |

## 向后兼容

旧格式（单 `[provider]` 节）会在加载时自动迁移：

```toml
# 旧格式（仍可识别）
[provider]
base_url = "https://api.deepseek.com/v1"
api_key = "sk-key"
model = "deepseek-chat"
```

迁移规则：
1. 旧 `[provider]` 转为 `[[providers]]`，name 设为 `"default"`
2. `active_provider` 设为 `"default"`
3. Provider 级别的 `model_routes` 提升为全局 `[[model_routes]]`
4. 通过 GUI 保存后写入新格式，旧格式不再保留

## Provider 格式说明

### format = "openai"

代理将 Anthropic 格式请求转换为 OpenAI Chat Completions 格式后发送到 `{base_url}/v1/chat/completions`。

适用于：OpenAI、DeepSeek、Azure OpenAI、兼容 OpenAI 格式的其他服务。

### format = "anthropic"

代理直接透传请求到 `{base_url}/v1/messages`，不做格式转换。

适用于：Anthropic Claude API。

### format = "kiro"

代理使用 Kiro/Amazon Q Developer 上游协议。该格式需要配置 `[providers.kiro_config]`，Provider 的 `api_key` 可留空。

适用于：Kiro IDE / Amazon Q Developer 兼容访问。

## 限制

- 最多配置 20 个 Provider
- Provider name 长度 1-64 字符
- 不允许重复的 Provider name
- 不能删除当前活跃的 Provider
- 不能删除最后一个 Provider
