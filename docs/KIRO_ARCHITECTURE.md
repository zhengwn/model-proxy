# Kiro Provider 集成架构设计

> 将 Kiro (Amazon Q Developer) 作为新 ProviderFormat 集成到 model-proxy 的设计方案。

---

## 1. 目标

让 `/v1/messages` 和 `/v1/chat/completions` 端点支持 Kiro 作为上游 Provider，实现：

```
Anthropic 客户端 → /v1/messages → [转换] → Kiro API → [EventStream 解析] → [转换] → Anthropic 响应
OpenAI 客户端   → /v1/chat/completions → [转换] → Kiro API → [EventStream 解析] → [转换] → OpenAI 响应
```

---

## 2. ProviderFormat 扩展

```rust
// config.rs
pub enum ProviderFormat {
    Openai,
    Anthropic,
    Kiro,       // 新增
}
```

配置文件示例：
```toml
[[providers]]
name = "kiro-free"
base_url = "https://q.us-east-1.amazonaws.com"
format = "kiro"
model = "claude-sonnet-4.5"

[providers.kiro]
auth_method = "social"              # social | idc | api_key
refresh_token = "..."
client_id = ""                      # idc 时需要
client_secret = ""                  # idc 时需要
profile_arn = ""                    # 可选，自动从刷新响应获取
region = "us-east-1"
```

---

## 3. 模块结构

```
crates/proxy-core/src/
├── kiro/                           # 新增模块
│   ├── mod.rs                      # 模块导出
│   ├── auth.rs                     # Token 管理（刷新、过期、多凭证）
│   ├── converter.rs                # Anthropic/OpenAI → Kiro payload 转换
│   ├── eventstream.rs              # AWS EventStream 二进制帧解析器
│   ├── stream.rs                   # Kiro 事件 → Anthropic/OpenAI SSE 流式转换
│   └── model_map.rs                # 模型 ID 规范化
├── proxy/
│   ├── convert.rs                  # 新增 openai_to_kiro() 调用入口
│   ├── handlers.rs                 # proxy_chat_completions Kiro 分支
│   ├── fallback.rs                 # InputFormat::Kiro 支持
│   └── ...
```

---

## 4. 模块设计

### 4.1 `kiro/auth.rs` — Token 管理

```rust
pub struct KiroAuthManager {
    credentials: Vec<KiroCredential>,
    client: reqwest::Client,
}

pub struct KiroCredential {
    pub auth_method: AuthMethod,        // Social | IdC | ApiKey
    pub refresh_token: Option<String>,
    pub access_token: Option<String>,
    pub client_id: Option<String>,      // IdC 时需要
    pub client_secret: Option<String>,  // IdC 时需要
    pub profile_arn: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub region: String,
    pub api_region: String,
    pub machine_id: String,
}

pub enum AuthMethod { Social, IdC, ApiKey }

impl KiroAuthManager {
    pub async fn get_valid_token(&mut self) -> Result<String>;
    async fn refresh_social(&self, cred: &mut KiroCredential) -> Result<()>;
    async fn refresh_idc(&self, cred: &mut KiroCredential) -> Result<()>;
}
```

关键逻辑：
- 过期判断: `expires_at <= now + 5min`
- 预刷新: `expires_at <= now + 10min`
- `invalid_grant` → 永久禁用该凭证
- 双重检查锁防止惊群

### 4.2 `kiro/eventstream.rs` — AWS EventStream 解析器

```rust
pub struct EventStreamDecoder {
    buffer: BytesMut,
    state: DecoderState,        // Ready | Parsing | Recovering | Stopped
    error_count: usize,
    max_errors: usize,          // 默认 5
}

pub struct Frame {
    pub headers: HashMap<String, HeaderValue>,
    pub payload: Vec<u8>,
}

pub enum Event {
    AssistantResponse { content: String },
    ReasoningContent { text: String },
    ToolUse { name: String, tool_use_id: String, input: String, stop: bool },
    ContextUsage { percentage: f64 },
    Metering { usage: f64 },
    Error { code: String, message: String },
    Exception { type_name: String, message: String },
    Unknown,
}
```

从 kiro.rs 复用：
- 帧解析（CRC32 ISO-HDLC 校验）
- Header 解析（10 种类型标签）
- 状态机（4 状态 + 恢复策略）
- 事件分发

### 4.3 `kiro/converter.rs` — 请求体转换

```rust
/// Anthropic Messages → Kiro payload
pub fn anthropic_to_kiro(
    body: &Value,
    provider: &ProviderConfig,
    global_routes: &[ModelRoute],
    auth: &KiroAuthManager,
) -> Result<Value>

/// OpenAI Chat Completions → Kiro payload
pub fn openai_to_kiro(
    body: &Value,
    provider: &ProviderConfig,
    global_routes: &[ModelRoute],
    auth: &KiroAuthManager,
) -> Result<Value>
```

转换逻辑：
1. 模型 ID 规范化（`claude-sonnet-4-5-*` → `claude-sonnet-4.5`）
2. System prompt 提取 → history 首条 user+assistant 对
3. 消息拆分：最后一条 user → currentMessage，其余 → history
4. 工具定义转换：`input_schema` → `inputSchema.json` 包装
5. 工具名缩短（>63 字符 → `{前缀}_{sha256_8}`）
6. 图片转换：base64 → `{format, source: {bytes}}`
7. Thinking 标签注入
8. Tool result 转换
9. profileArn 注入到 JSON 根
10. conversationId 生成

### 4.4 `kiro/stream.rs` — 流式转换

```rust
/// Kiro EventStream → Anthropic SSE
pub async fn handle_stream_anthropic_output(
    upstream_resp: reqwest::Response,
    model: &str,
    tool_name_map: HashMap<String, String>,
    ...
) -> Result<Response>

/// Kiro EventStream → OpenAI SSE
pub async fn handle_stream_openai_output(
    upstream_resp: reqwest::Response,
    model: &str,
    tool_name_map: HashMap<String, String>,
    ...
) -> Result<Response>
```

状态机追踪：
- `started` / `ended` 状态
- 当前 content block 类型（text / thinking / tool_use）
- 工具输入累积 buffer
- contextUsage 百分比 → input_tokens 估算
- thinking 标签提取（`<thinking>...</thinking>`）
- 文本去重（normalizeChunk）

### 4.5 `kiro/model_map.rs` — 模型映射

```rust
pub fn normalize_model_id(input: &str) -> Option<String> {
    // 1. 去日期后缀: claude-sonnet-4-5-20250929 → claude-sonnet-4-5
    // 2. 副版本号改点: claude-sonnet-4-5 → claude-sonnet-4.5
    // 3. 旧版映射: claude-3-5-sonnet → claude-sonnet-4.5
    // 4. GPT 映射: gpt-4o → claude-sonnet-4.5
    // 5. 校验是否为已知 Kiro 模型
}
```

---

## 5. 请求处理流程

### 5.1 `proxy_messages` (Anthropic 输入 → Kiro 上游)

```
1. check_auth
2. 读取 body
3. provider.format == Kiro =>
   a. anthropic_to_kiro() 转换请求体
   b. auth.get_valid_token() 获取 token
   c. 构建 HTTP 请求（Kiro headers）
   d. 发送到 https://q.{region}.amazonaws.com/generateAssistantResponse
4. 处理响应：
   - 非流式: EventStream 解析 → 收集所有事件 → 构建 Anthropic 响应
   - 流式: handle_stream_anthropic_output()
5. 失败时 fallback（与 OpenAI/Anthropic 分支同理）
```

### 5.2 `proxy_chat_completions` (OpenAI 输入 → Kiro 上游)

```
1. check_auth
2. 读取 body
3. prepare_kiro_body() (openai → kiro 转换)
4. auth.get_valid_token()
5. 构建请求 + 发送
6. 处理响应：
   - 非流式: EventStream → OpenAI 格式响应
   - 流式: handle_stream_openai_output()
```

---

## 6. Fallback 集成

`InputFormat` 枚举扩展：

```rust
pub enum InputFormat {
    Anthropic,
    OpenAI,
    // Kiro 不需要单独的 InputFormat
    // Kiro 的 fallback 按上游 Provider 的 format 分支处理
}
```

Fallback 矩阵（输入格式 × 上游 Provider 格式）:

| 输入 | OpenAI 上游 | Anthropic 上游 | Kiro 上游 |
|------|:-----------:|:--------------:|:---------:|
| Anthropic | ✅ 现有 | ✅ 现有 | ✅ 新增 |
| OpenAI | ✅ 现有 | ✅ 现有 | ✅ 新增 |

---

## 7. 依赖

新增 Cargo 依赖：
```toml
[dependencies]
crc = "3"              # CRC32 ISO-HDLC
bytes = "1"            # BytesMut buffer
```

已有依赖（复用）：`reqwest`, `tokio`, `serde_json`, `uuid`, `chrono`, `tracing`

---

## 8. 配置扩展

`ProviderConfig` 新增 `kiro_config` 字段：

```rust
pub struct ProviderConfig {
    // ... 现有字段 ...
    pub kiro_config: Option<KiroConfig>,
}

pub struct KiroConfig {
    pub auth_method: String,        // "social" | "idc" | "api_key"
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub profile_arn: Option<String>,
    pub region: String,             // 默认 "us-east-1"
    pub api_region: Option<String>, // 默认同 region
}
```

---

## 9. 实现顺序

| 阶段 | 内容 | 依赖 |
|:----:|------|------|
| 1 | `eventstream.rs` — EventStream 解析器 | crc, bytes |
| 2 | `model_map.rs` — 模型 ID 规范化 | — |
| 3 | `auth.rs` — Token 管理 | reqwest |
| 4 | `converter.rs` — Anthropic → Kiro 转换 | 1, 2, 3 |
| 5 | `stream.rs` — Kiro → Anthropic 流式转换 | 1 |
| 6 | handlers.rs 集成 + 非流式响应 | 4, 5 |
| 7 | OpenAI 方向支持 | 4, 5, 6 |
| 8 | Fallback 集成 | 6, 7 |
| 9 | 测试 | 全部 |

---

## 10. 风险与对策

| 风险 | 对策 |
|------|------|
| Kiro API 格式变更 | 核心协议在 eventstream.rs 中隔离，变更影响范围小 |
| Token 刷新失败 | 多凭证自动降级，invalid_grant 永久禁用 |
| 文本累积 vs 增量 | 实现 normalizeChunk 去重逻辑 |
| 工具参数截断 | 检测 JSON 不完整，注入系统提示通知模型 |
| Thinking 标签误判 | 仅在非代码块（反引号外）匹配 `<thinking>` 标签 |
| Payload 超 600KB | 实现 history 自动裁剪（从最旧开始删） |
