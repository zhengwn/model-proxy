# Kiro API 协议规范

> 基于 kiro.rs (Rust)、Kiro-Go (Go)、KiroProxy (Python)、KiroGate (Python)、kiro2api (Go)、kiro-gateway (Python) 六个实现交叉验证。
> Kiro API 是 AWS 的私有协议，无官方公开文档。此规范为逆向工程结果。
> 最后更新: 2026-05-30

---

## 1. 端点

### 主端点（流式）

```
POST https://q.{apiRegion}.amazonaws.com/generateAssistantResponse
```

`apiRegion` 默认 `us-east-1`，可从凭证的 `profileArn` 中提取（第 4 段）。

### 降级端点（Kiro-Go 方案）

| 优先级 | URL | X-Amz-Target |
|:------:|-----|-------------|
| 1 | `https://q.us-east-1.amazonaws.com/generateAssistantResponse` | (无) |
| 2 | `https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse` | `AmazonCodeWhispererStreamingService.GenerateAssistantResponse` |
| 3 | `https://q.us-east-1.amazonaws.com/generateAssistantResponse` | `AmazonQDeveloperStreamingService.SendMessage` |

429 时自动降级到下一个端点。

### 辅助端点

```
GET  https://codewhisperer.us-east-1.amazonaws.com/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST
POST https://codewhisperer.us-east-1.amazonaws.com/GetUserInfo
GET  https://codewhisperer.us-east-1.amazonaws.com/ListAvailableModels?origin=AI_EDITOR&maxResults=50
```

---

## 2. 请求头

| Header | 值 | 必须 | 说明 |
|--------|---|:----:|------|
| `Content-Type` | `application/json` | ✅ | 所有实现一致 |
| `Authorization` | `Bearer {access_token}` | ✅ | |
| `x-amzn-codewhisperer-optout` | `"true"` | ✅ | |
| `x-amzn-kiro-agent-mode` | `"vibe"` | ✅ | |
| `x-amz-user-agent` | `aws-sdk-js/1.0.34 KiroIDE-{version}-{machineId}` | ✅ | |
| `user-agent` | `aws-sdk-js/1.0.34 ua/2.1 os/{os} lang/js md/nodejs#{ver} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{version}-{machineId}` | ✅ | |
| `amz-sdk-invocation-id` | `{uuid-v4}` | ✅ | 每请求唯一 |
| `amz-sdk-request` | `attempt=1; max=3` | ✅ | |
| `tokentype` | `API_KEY` | 条件 | 仅 API Key 凭证时 |
| `X-Amz-Target` | 见降级端点表 | 条件 | 仅非 Kiro IDE 端点时 |

SDK 版本: `1.0.34`（流式），`1.0.0`（REST 辅助端点）

---

## 3. 请求体

### 顶层结构

```json
{
  "conversationState": {
    "agentContinuationId": "uuid-v4",
    "agentTaskType": "vibe",
    "chatTriggerType": "MANUAL",
    "conversationId": "uuid-v4",
    "currentMessage": { "userInputMessage": { ... } },
    "history": [ ... ]
  },
  "profileArn": "arn:aws:codewhisperer:{region}:...",
  "inferenceConfig": {                        // 可选
    "maxTokens": 8192,
    "temperature": 0.7,
    "topP": 0.9
  }
}
```

**约束:**
- `chatTriggerType` 必须为 `"MANUAL"`（`"AUTO"` 会返回 400）
- `agentTaskType` 必须为 `"vibe"`
- `profileArn` 在请求体 JSON 根级别注入（由 endpoint 层处理，不是 converter）
- `inferenceConfig` 可选，Kiro-Go 发送，kiro.rs 不发送
- `agentContinuationId`、`agentTaskType`、`chatTriggerType` 都是 `Option`，默认 `None`（不序列化）
- `history` 为空时不序列化（`skip_serializing_if = "Vec::is_empty"`）

### 序列化行为（已验证）

- History 使用 `#[serde(untagged)]` 枚举，user 消息序列化为 `{"userInputMessage": {...}}`，assistant 消息序列化为 `{"assistantResponseMessage": {...}}`
- History user 消息的 `userInputMessageContext` 在 tools 和 toolResults 都为空时**完全省略**（自定义 `is_default_context` skip）
- Current message 的 `userInputMessageContext` **始终存在**（无 skip）
- `ToolResult.isError` 仅在 `true` 时序列化（`skip_serializing_if = "is_false"`）
- `AssistantMessage.toolUses` 为 `None` 时不序列化

### currentMessage（必填）

```json
{
  "userInputMessage": {
    "content": "用户文本内容",
    "modelId": "claude-sonnet-4.5",
    "origin": "AI_EDITOR",
    "images": [                               // 可选
      {
        "format": "jpeg",                     // jpeg|png|gif|webp
        "source": { "bytes": "base64数据" }
      }
    ],
    "userInputMessageContext": {              // 可选
      "tools": [ ... ],
      "toolResults": [ ... ]
    }
  }
}
```

### history（可选）

交替的 user/assistant 消息数组。经验证的行为规则：

1. **Prefill 丢弃**: 如果原始消息的最后一条不是 user 角色，静默丢弃（从末尾找到最后一条 user 消息，截断到那里）
2. **消息拆分**: 最后一条 user 消息 → `currentMessage`，其余 → `history`
3. **连续同角色合并**: 连续 user 消息合并（文本用 `\n` 连接，images 和 toolResults 拼接）；连续 assistant 消息合并（文本用 `\n\n` 连接，toolUses 拼接）
4. **孤立 user 消息补全**: history 末尾的孤立 user 消息（后面没有 assistant）会自动补一条 assistant 回复 `"OK"`
5. **System prompt 注入**: 作为 history 的第一条 user+assistant 消息对

**User 历史消息:**
```json
{
  "userInputMessage": {
    "content": "...",
    "modelId": "claude-sonnet-4.5",
    "origin": "AI_EDITOR",
    "images": [ ... ],
    "userInputMessageContext": {
      "toolResults": [ ... ]
    }
  }
}
```

**Assistant 历史消息:**
```json
{
  "assistantResponseMessage": {
    "content": "回复文本",
    "toolUses": [
      {
        "toolUseId": "tool-use-id",
        "name": "tool_name",
        "input": { ... }
      }
    ]
  }
}
```

### 工具定义

```json
{
  "toolSpecification": {
    "name": "tool_name",
    "description": "工具描述",
    "inputSchema": {
      "json": { /* JSON Schema 对象 */ }
    }
  }
}
```

**注意:** JSON Schema 嵌套在 `inputSchema.json` 里，不是直接放在 `inputSchema` 上。

**约束:**
- 工具名最长 **63** 字节（超出需缩短：`{54字节前缀}_{SHA256前8位hex}`，恰好 63 字节）
- 工具描述最长 **10,000** 字符（UTF-8 安全截断）
- `Write` 工具描述会自动追加分块操作提示后缀
- `Edit` 工具描述会自动追加分块编辑提示后缀
- JSON Schema 经过 `normalize_json_schema` 规范化：
  - 缺失 `type` → 设为 `"object"`
  - 缺失 `properties` → 设为 `{}`
  - 缺失 `required` → 设为 `[]`
  - 缺失 `additionalProperties` → 设为 `true`（不是 `false`！）
- 历史中引用但当前 tools 列表中不存在的工具，会创建占位工具定义

### 工具结果

```json
{
  "toolUseId": "tool-use-id",
  "content": [{ "text": "结果文本" }],      // Vec<Map<String, Value>>，每项必须有 "text" 键
  "status": "success"                       // 可选，"success" 或 "error"
  // "isError": true                        // 仅在 true 时序列化，false 时完全省略
}
```

**注意:** `content` 的类型是 `Vec<Map<String, Value>>`，不是 `Vec<String>`。每个元素是一个 JSON 对象，至少包含 `{"text": "..."}` 键。
```

---

## 4. 模型 ID

Kiro 接受的模型 ID（使用点号分隔副版本号）:

| 模型 ID | 上下文窗口 |
|---------|:---------:|
| `claude-sonnet-4.5` | 200K |
| `claude-sonnet-4.6` | 1M |
| `claude-opus-4.5` | 200K |
| `claude-opus-4.6` | 1M |
| `claude-opus-4.7` | 1M |
| `claude-haiku-4.5` | 200K |
| `claude-sonnet-4` | 200K |
| `auto` | — |

### 模型名规范化规则

客户端发送的模型名需要规范化后才能发给 Kiro：

| 客户端格式 | → Kiro 格式 |
|-----------|-------------|
| `claude-sonnet-4-5-20250929` | `claude-sonnet-4.5` |
| `claude-sonnet-4.5` | `claude-sonnet-4.5`（不变） |
| `claude-3-5-sonnet-20241022` | `claude-sonnet-4.5` |
| `claude-3-opus-*` | `claude-opus-4.5` |
| `claude-3-haiku-*` | `claude-haiku-4.5` |
| `gpt-4o` / `gpt-4` | `claude-sonnet-4.5` |

规则：去掉日期后缀，把 `-` 改为 `.`（仅副版本号），映射旧版名称。

---

## 5. 响应格式：AWS EventStream 二进制协议

**这不是 SSE。** Kiro 返回 AWS EventStream 二进制帧。

### 帧结构

```
偏移    大小      字段
------  --------  --------------------------
0       4 字节   总长度 (big-endian u32)，包含自身
4       4 字节   Header 长度 (big-endian u32)
8       4 字节   Prelude CRC32 (big-endian u32)
12      变长     Headers
12+HL   变长     Payload (JSON)
TL-4    4 字节   消息 CRC32 (big-endian u32)
```

- **总长度** 最小 16 字节，最大 16 MB
- **Prelude CRC** = CRC32(buffer[0..8])
- **消息 CRC** = CRC32(buffer[0..TL-4])
- **CRC 算法**: ISO-HDLC (多项式 0xEDB88320)

### Header 格式

每个 header:
```
1 字节   name 长度 (u8, >0)
N 字节   name (UTF-8)
1 字节   value 类型标签 (0-9)
变长     value (类型相关)
```

Value 类型:

| 标签 | 类型 | 大小 |
|:----:|------|------|
| 0 | BoolTrue | 0 |
| 1 | BoolFalse | 0 |
| 2 | Byte | 1 |
| 3 | Short | 2 (BE) |
| 4 | Integer | 4 (BE) |
| 5 | Long | 8 (BE) |
| 6 | ByteArray | 2+N (u16 长度前缀) |
| 7 | String | 2+N (u16 长度前缀) |
| 8 | Timestamp | 8 (BE, 毫秒) |
| 9 | Uuid | 16 |

关键 Header:

| Header 名 | 含义 |
|-----------|------|
| `:message-type` | `"event"` / `"error"` / `"exception"` |
| `:event-type` | 见事件类型表 |
| `:error-code` | 错误码 |
| `:exception-type` | 异常类型 |

### 事件类型

| `:event-type` | Payload JSON | 说明 |
|--------------|-------------|------|
| `assistantResponseEvent` | `{"content": "文本块"}` | 文本内容（可能是累积的，需去重） |
| `reasoningContentEvent` | `{"text": "思考文本"}` | 思考/推理内容 |
| `toolUseEvent` | `{"name":"...", "toolUseId":"...", "input":"部分JSON", "stop":false}` | 工具调用（增量） |
| `meteringEvent` | `{"usage": 数字}` | 计费 |
| `contextUsageEvent` | `{"contextUsagePercentage": 42.5}` | 上下文窗口使用率 |

**错误/异常帧:**
- `:message-type` = `"error"`: payload 是错误消息文本
- `:message-type` = `"exception"`: payload 是异常消息文本

### 工具调用流式行为

1. 收到 `toolUseEvent` 且 `stop: false` → 开始累积 `input` 字符串
2. 后续 `toolUseEvent` 继续追加 `input` 片段
3. 收到 `toolUseEvent` 且 `stop: true` → 拼接完整 JSON，解析为对象

### 文本去重

Kiro 后端可能发送**累积文本**而非增量 delta。需要实现 `normalizeChunk` 逻辑：
- 如果新 chunk 是之前内容的前缀 → 跳过
- 如果新 chunk 以之前内容为前缀 → 只取新增部分
- 否则 → 直接使用

### 流式状态管理（已验证）

SSE 输出状态管理器字段：
- `message_started` / `message_delta_sent` / `message_ended` — 事件发送状态
- `active_blocks: HashMap<i32, BlockState>` — 每个 block 的状态（started/stopped/type）
- `next_block_index: i32` — 自增 block 索引
- `stop_reason: Option<String>` — 显式覆盖
- `has_tool_use: bool` — 是否有工具调用

**关键行为规则：**
1. `message_start` 最多发送一次
2. 开始 `tool_use` block 时，所有已打开的 `text` block **自动关闭**
3. `tool_use` block 关闭后，新的文本内容会自动创建新的 `text` block（自愈）
4. `content_block_delta` 仅在 block 存在且 started=true 且 stopped=false 时接受
5. 最终事件：关闭所有 open blocks → `message_delta`（含 stop_reason + usage）→ `message_stop`

**Stop reason 确定逻辑：**
1. 显式设置优先（`model_context_window_exceeded`、`max_tokens`）
2. 有 tool_use → `"tool_use"`
3. 否则 → `"end_turn"`

**显式 stop_reason 来源：**
- `contextUsagePercentage >= 100.0` → `"model_context_window_exceeded"`
- `ContentLengthExceededException` → `"max_tokens"`
- 仅有 thinking 输出（无 text、无 tool_use）→ `"max_tokens"`

### Thinking 标签检测（已验证）

Kiro 的 thinking 内容以 `<thinking>...</thinking>` 标签嵌入在文本流中，需要在流式解析时提取。

**标签过滤规则：**
- 标签前后如果有引号类字符（`` ` " ' \ # ! @ $ % ^ & * ( ) - _ = + [ ] { } ; : < > , . ? / ``），视为**被引用**，不是真正的标签

**`<thinking>` 开始标签：**
- 搜索 `<thinking>` 字符串
- 检查前后字符是否为引号字符
- 非引用 → 返回位置

**`</thinking>` 结束标签：**
- 搜索 `</thinking>` 字符串
- 检查前后字符是否为引号字符
- 非引用 + 后面紧跟 `\n\n`（双换行）→ 返回位置
- 如果后面不是 `\n\n` → 跳过，继续搜索

**边界结束标签（用于 thinking 结束后紧跟 tool_use 的情况）：**
- 同样搜索 `</thinking>`
- 非引用 + 标签后所有内容都是空白 → 返回位置

**首行换行剥离：** 进入 thinking block 后，如果内容以 `\n` 开头，剥离这一个 `\n`（处理 `<thinking>\n...content` 模式）

### Context Usage 处理

- `contextUsageEvent` → `actual_input_tokens = (percentage × window_size) / 100`
- 窗口大小：`claude-sonnet-4.6` / `claude-opus-4.6` / `claude-opus-4.7` = 1,000,000；其他 = 200,000
- `percentage >= 100.0` → stop_reason = `"model_context_window_exceeded"`
- 最终事件中，`context_input_tokens` 覆盖估算的 `input_tokens`

### 解码器状态机

```
Ready → feed() → Ready
Ready → decode() → Parsing
Parsing → 成功 → Ready (error_count 归零)
Parsing → 数据不足 → Ready
Parsing → 错误 (count < 5) → Recovering → Ready
Parsing → 错误 (count >= 5) → Stopped (终止)
```

恢复策略:
- Prelude 错误 → 跳过 1 字节
- 数据错误 → 跳过整个帧（如果 total_length 可信）
- 连续 5 次错误 → 停止

---

## 6. 认证

### 两种 Token 刷新方式

**Social (Kiro Desktop OAuth):**
```
POST https://prod.{region}.auth.desktop.kiro.dev/refreshToken
Content-Type: application/json

{"refreshToken": "..."}

→ {"accessToken": "...", "refreshToken": "...", "expiresIn": 3600, "profileArn": "..."}
```

**IdC (AWS SSO OIDC):**
```
POST https://oidc.{region}.amazonaws.com/token
Content-Type: application/json
x-amz-user-agent: aws-sdk-js/3.980.0 KiroIDE
user-agent: aws-sdk-js/3.980.0 ua/2.1 os/{os} lang/js md/nodejs#{ver} api/sso-oidc#3.980.0 m/E KiroIDE
amz-sdk-invocation-id: {uuid-v4}
amz-sdk-request: attempt=1; max=4
Connection: close

{
  "clientId": "...",
  "clientSecret": "...",
  "refreshToken": "...",
  "grantType": "refresh_token"
}

→ {"accessToken": "...", "refreshToken": "...", "expiresIn": 3600, "profileArn": "..."}
```

### 凭证来源

| 来源 | 路径 | 格式 |
|------|------|------|
| Kiro Desktop JSON | `~/.kiro/credentials.json` 或配置路径 | JSON (camelCase) |
| kiro-cli SQLite | macOS: `~/Library/Application Support/kiro-cli/data.sqlite3` | `state` 表 (key/value) |
| kiro-cli SQLite | Linux: `~/.local/share/kiro-cli/data.sqlite3` | 同上 |
| API Key | 配置文件 | `ksk_xxxxxxxx` 格式 |

### SQLite Schema

```sql
CREATE TABLE state (key TEXT, value TEXT);
```

关键 key 映射:

| SQLite key | 含义 |
|-----------|------|
| `accessToken` | 当前 access token |
| `refreshToken` | 刷新 token |
| `expiresAt` | 过期时间 |
| `profileArn` | AWS profile ARN |
| `oidcClientId` / `clientId` | OIDC 客户端 ID |
| `oidcClientSecret` / `clientSecret` | OIDC 客户端密钥 |
| `region` | 区域 |
| `ssoRegion` | SSO 区域（可能与 region 不同） |

### Token 过期策略

- **已过期**: `expires_at <= now + 5 分钟`
- **即将过期**: `expires_at <= now + 10 分钟`（触发预刷新）
- **永久失效**: HTTP 400 + `invalid_grant` → 禁用该凭证
- **刷新 token 验证**: 长度 >= 100，不以 `"..."` 结尾

---

## 7. Thinking 模式

Kiro 不原生支持 Anthropic 的 `thinking` 参数。通过在 system prompt 中注入特殊标签实现：

```xml
<thinking_mode>enabled</thinking_mode>
<max_thinking_length>200000</max_thinking_length>
```

或自适应模式：
```xml
<thinking_mode>adaptive</thinking_mode>
<thinking_effort>high</thinking_effort>
```

**已验证细节：**
- `budget_tokens` 来自 Anthropic 请求的 `thinking.budget_tokens`，默认 20000，上限 24576
- Thinking 标签注入到 system prompt 前面，用 `\n` 分隔
- 如果 system prompt 已包含 `<thinking_mode>` 或 `<max_thinking_length>`，不重复注入
- 如果没有 system prompt 但有 thinking 配置，创建一条只含 thinking 标签的 user 消息

**Assistant 历史消息中的 thinking 处理：**
- thinking content blocks 转为 `<thinking>{content}</thinking>` 包裹
- 后面跟两个换行再接 text content
- 如果只有 thinking 没有 text，只输出 `<thinking>...</thinking>`
- 如果既没有 thinking 也没有 text 但有 tool_uses，content 设为 `" "`（单空格，Kiro 要求非空）

响应中 thinking 内容以 `<thinking>...</thinking>` 标签包裹在文本流中，需要在流式解析时提取并转换为 Anthropic `thinking` content block。

---

## 8. System Prompt 注入

Kiro 没有原生的 system prompt 字段。两种注入方式（都有效）：

**方式 A（kiro.rs，推荐）:** 作为 history 的第一条 user+assistant 消息对：
```json
"history": [
  {"userInputMessage": {"content": "{thinking_prefix}\n{system_prompt}\n{SYSTEM_CHUNKED_POLICY}", "modelId": "...", "origin": "AI_EDITOR"}},
  {"assistantResponseMessage": {"content": "I will follow these instructions."}},
  ...后续真实历史...
]
```

`SYSTEM_CHUNKED_POLICY` 常量内容：
```
When the Write or Edit tool has content size limits, always comply silently. Never suggest bypassing these limits via alternative tools. Never ask the user whether to switch approaches. Complete all chunked operations without commentary.
```

**方式 B（Kiro-Go）:** 拼接到 currentMessage 的 content 前面：
```
--- SYSTEM PROMPT ---
{system_prompt}
--- END SYSTEM PROMPT ---

{用户实际消息}
```

---

## 9. 实现间矛盾汇总

| 方面 | kiro.rs | Kiro-Go | 建议 |
|------|---------|---------|------|
| API 端点 | 动态 region | 硬编码 us-east-1 | 动态 region |
| inferenceConfig | 不发送 | 发送 | 发送（无害） |
| 工具结果 isError | 发送 | 不发送 | 发送 status + isError |
| System prompt | history 首条 | 拼接到 content | history 首条（更干净） |
| Thinking budget | 从请求读取 | 硬编码 200K | 从请求读取 |
| 工具描述上限 | 10,000 | 10,237 | 10,000（保守） |
| conversationId | 随机 UUID | 确定性哈希 | 随机 UUID |
| 文本去重 | 未实现 | 实现 | 必须实现 |
| 旧版模型 | 不支持 | 支持 | 支持（兼容性好） |
| Accept header | 不设 | `*/*` | `*/*` |
| X-Amz-Target | 不设 | 条件设置 | 不设（IDE 端点不需要） |
| expiresAt 格式 | RFC3339 | Unix 整数 | RFC3339 + 兼容解析两种 |

---

## 10. 关键实现参考

| 组件 | 最佳参考 | 语言 |
|------|---------|------|
| EventStream 解析器 | `kiro.rs/src/kiro/parser/` | Rust |
| Anthropic → Kiro 转换 | `kiro.rs/src/anthropic/converter.rs` | Rust |
| 流式转换 + thinking 提取 | `kiro.rs/src/anthropic/stream.rs` | Rust |
| 多凭证管理 | `kiro.rs/src/kiro/token_manager.rs` | Rust |
| 端点降级 | `Kiro-Go/proxy/kiro.go` | Go |
| SQLite 凭证读取 | `KiroProxy/kiro_proxy/credential/sqlite_auth.py` | Python |
| Prompt 过滤 | `Kiro-Go/proxy/translator.go` | Go |
| Prompt cache 模拟 | `Kiro-Go/proxy/cache_tracker.go` | Go |
| 截断恢复 | `KiroProxy/kiro_proxy/truncation_recovery.py` | Python |
