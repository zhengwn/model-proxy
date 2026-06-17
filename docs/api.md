# HTTP API 参考

Model Proxy 对外暴露以下 HTTP 端点。所有端点监听在配置的 `server.host:server.port`（默认 `127.0.0.1:4000`）上。

## 端点列表

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/health` | 健康检查 |
| GET | `/metrics` | Prometheus 文本指标 |
| GET | `/v1/models` | OpenAI 兼容模型列表 |
| POST | `/v1/messages` | 代理请求（主端点） |
| POST | `/v1/messages/count_tokens` | Anthropic count_tokens 兼容端点 |
| POST | `/cc/v1/messages` | Claude Code 兼容 Messages 端点 |
| POST | `/cc/v1/messages/count_tokens` | Claude Code 兼容 count_tokens 端点 |
| POST | `/v1/chat/completions` | OpenAI Chat Completions 代理端点 |
| POST | `/v1/responses` | OpenAI Responses API 兼容端点（Kiro provider） |
| GET | `/api/status` | 服务状态 |
| GET | `/api/usage` | Kiro 用量查询 |
| GET | `/api/flows` | Kiro flow 监控 |
| POST | `/api/kiro/login/start` | Kiro OIDC device flow 登录启动 |
| POST | `/api/kiro/login/poll` | Kiro OIDC device flow 轮询 |
| POST | `/api/kiro/social/start` | Kiro social OAuth 登录启动 |
| POST | `/api/kiro/social/exchange` | Kiro social OAuth code 交换 |
| POST | `/api/event_logging/batch` | 遥测事件接收（静默丢弃） |
| `/api/admin/*` | Admin API | Kiro 凭据、运行设置、IP 和站点管理 |

## 认证

如果配置了 `server.api_key`，代理和状态请求必须携带认证信息。支持两种方式：

```
x-api-key: your-api-key
```

或

```
Authorization: Bearer your-api-key
```

未认证请求返回 `401 Unauthorized`。

`/health`、`/metrics`、`/v1/models` 不要求客户端 API key。`/api/admin/*` 使用独立的 `server.admin_api_key`，不会再叠加要求 `server.api_key`；如果未配置 `server.admin_api_key`，Admin API 返回 `403 Forbidden`。

如果未配置 `server.api_key`，则客户端代理请求不进行认证检查。绑定到非本机地址（如 `0.0.0.0`）时强烈建议配置 `server.api_key`。

## POST /v1/messages

### 请求格式

接受 **Anthropic Messages API** 格式的请求体。这是代理的唯一入口格式，无论目标 Provider 使用什么格式。

```json
{
  "model": "claude-sonnet-4-20250514",
  "max_tokens": 4096,
  "stream": true,
  "system": "You are a helpful assistant.",
  "messages": [
    {
      "role": "user",
      "content": "Hello"
    }
  ]
}
```

支持的字段：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `model` | string | 否 | 请求的模型名，用于模型路由匹配。未匹配时使用 Provider 默认模型 |
| `max_tokens` | integer | 否 | 最大输出 token 数 |
| `stream` | boolean | 否 | 是否使用流式响应，默认 false |
| `system` | string/array | 否 | 系统提示词 |
| `messages` | array | 是 | 对话消息列表 |
| `tools` | array | 否 | 工具定义（Anthropic 格式） |
| `tool_choice` | string/object | 否 | 工具选择策略 |
| `temperature` | number | 否 | 采样温度 |
| `top_p` | number | 否 | Top-p 采样 |
| `top_k` | integer | 否 | Top-k 采样 |
| `stop_sequences` | array | 否 | 停止序列 |
| `thinking` | object | 否 | 推理配置（type: enabled/disabled/adaptive） |
| `output_config` | object | 否 | 输出格式配置（json_schema/json_object） |

### 响应格式

#### 非流式响应

返回 Anthropic Messages API 格式的 JSON：

```json
{
  "type": "message",
  "id": "msg_...",
  "role": "assistant",
  "content": [
    {
      "type": "text",
      "text": "Hello! How can I help you?"
    }
  ],
  "model": "deepseek-v4-pro",
  "stop_reason": "end_turn",
  "stop_sequence": null,
  "usage": {
    "input_tokens": 10,
    "output_tokens": 15,
    "cache_read_input_tokens": 0,
    "cache_creation_input_tokens": 0
  }
}
```

#### 流式响应

返回 `text/event-stream` 格式的 SSE 流。事件类型遵循 Anthropic 流式协议：

```
event: message_start
data: {"type":"message_start","message":{"id":"msg_...","type":"message","role":"assistant","model":"...","usage":{"input_tokens":0,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}

event: message_stop
data: {"type":"message_stop"}
```

支持的 content block 类型：
- `text` — 文本输出
- `thinking` — 推理过程（reasoning）
- `tool_use` — 工具调用

### 模型路由

请求中的 `model` 字段会经过模型路由匹配。匹配规则：

1. 将请求的 model 名转为小写
2. 按配置顺序遍历 `[[model_routes]]`
3. 如果 model 名 **包含** 路由的 `match` 值（大小写不敏感），则使用该路由的 `target` 作为实际模型
4. 未匹配任何路由时，使用当前 Provider 的默认 `model`

示例：配置 `match = "sonnet"`，则请求 `claude-3-5-sonnet-20241022` 或 `claude-sonnet-4-20250514` 都会匹配。

## POST /v1/chat/completions

接受 OpenAI Chat Completions 格式请求。当前 Provider 为 OpenAI 格式时会直通上游；当前 Provider 为 Anthropic 或 Kiro 时会按内部转换路径代理。

## GET /v1/models

返回 OpenAI 兼容模型列表。该端点公开可访问，不要求 `server.api_key`。非 Kiro provider 返回当前默认模型和路由目标模型；Kiro provider 会优先尝试查询 Kiro 模型列表，失败时返回内置静态列表。

## POST /v1/responses

OpenAI Responses API 兼容端点，目前仅支持 Kiro provider。非 Kiro provider 会返回请求错误。

## Admin API

所有 `/api/admin/*` 端点都使用 `server.admin_api_key` 鉴权，支持 `x-api-key` 或 `Authorization: Bearer`。Admin API 不要求客户端 `server.api_key`。

常用端点：

| 方法 | 路径 | 说明 |
|------|------|------|
| GET/PUT/POST | `/api/admin/config` | Kiro 负载均衡配置 |
| GET/POST | `/api/admin/credentials` | Kiro 凭据列表/新增 |
| DELETE | `/api/admin/credentials/{id}` | 删除凭据 |
| POST | `/api/admin/credentials/{id}/disabled` | 启用或禁用凭据 |
| POST | `/api/admin/credentials/{id}/priority` | 调整凭据优先级 |
| POST | `/api/admin/credentials/{id}/refresh` | 强制刷新凭据 |
| POST | `/api/admin/credentials/{id}/test` | 测试凭据 |
| GET/POST | `/api/admin/thinking` | 查询或更新 Kiro thinking 处理模式 |
| GET/POST | `/api/admin/settings` | 查询或更新 Kiro 首选端点和 429 降级配置 |
| GET | `/api/admin/endpoints/health` | Kiro 端点健康快照 |
| GET | `/api/admin/site/status` | 站点保护状态 |
| POST | `/api/admin/site/maintenance` | 切换维护模式 |
| POST | `/api/admin/site/self-use` | 切换自用模式 |
| GET | `/api/admin/ip/list` | 查看封禁 IP |
| POST | `/api/admin/ip/ban` | 封禁 IP |
| POST | `/api/admin/ip/unban` | 解封 IP |

## 错误响应

所有错误返回统一的 JSON 格式：

```json
{
  "type": "error",
  "error": {
    "type": "proxy_error",
    "message": "Upstream service error: connection refused"
  }
}
```

上游错误（非 2xx 响应）会透传上游的错误体：

```json
{
  "type": "error",
  "error": {
    "type": "upstream_error",
    "message": "<upstream response body>"
  }
}
```

### HTTP 状态码

| 状态码 | 含义 |
|--------|------|
| 200 | 成功 |
| 400 | 请求格式错误（JSON 解析失败等） |
| 401 | 认证失败 |
| 413 | 请求体超过 `max_body_bytes` 限制 |
| 502 | 上游服务错误（连接失败或返回非 2xx） |
| 500 | 内部错误 |

## GET /health

健康检查端点，始终返回：

```json
{"status": "ok"}
```

## GET /metrics

Prometheus 文本格式指标端点，公开可访问。

## POST /api/event_logging/batch

接收遥测事件批次。代理仅记录日志后返回成功，不做实际处理。用于兼容某些客户端的遥测上报。

```json
{"status": "ok"}
```

## 超时配置

| 参数 | 值 | 说明 |
|------|-----|------|
| 上游连接超时 | 30s | TCP 连接建立超时 |
| 非流式请求超时 | 300s (5min) | 非流式请求的总超时 |
| 流式请求超时 | 无 | 流式请求不设总超时 |
| 连接池空闲超时 | 90s | 空闲连接回收时间 |
| 请求体大小限制 | 64MB (默认) | 可通过 `server.max_body_bytes` 配置 |

## 使用示例

### curl 测试

```bash
curl -X POST http://localhost:4000/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: your-api-key" \
  -d '{
    "model": "claude-sonnet-4-20250514",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

### 流式请求

```bash
curl -X POST http://localhost:4000/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: your-api-key" \
  -N \
  -d '{
    "model": "claude-sonnet-4-20250514",
    "max_tokens": 1024,
    "stream": true,
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

### IDE 配置

在支持自定义 API 端点的 IDE 中，将 API Base URL 设置为：

```
http://localhost:4000
```

API Key 设置为配置文件中的 `server.api_key` 值。Kiro 管理面板和 `/api/admin/*` 请求使用 `server.admin_api_key`，它可以和客户端 API Key 不同。
