# model-proxy (Rust) vs reference/KiroProxy (Python) 像素级对比报告

> 对比日期: 2026-06-18
> 对比对象: model-proxy (Rust/Axum/Tauri) vs reference/KiroProxy (Python/FastAPI)
> 对比范围: Kiro 网关功能的全部核心模块，逐文件逐函数对照

---

## 目录

1. [总体架构与技术栈](#1-总体架构与技术栈)
2. [认证系统 (Auth)](#2-认证系统-auth)
3. [EventStream 二进制协议解析](#3-eventstream-二进制协议解析)
4. [请求转换: Anthropic/OpenAI/Gemini → Kiro](#4-请求转换-anthropicopenaigemini--kiro)
5. [流式响应处理: Kiro → 客户端 SSE](#5-流式响应处理-kiro--客户端-sse)
6. [多账号管理与负载均衡](#6-多账号管理与负载均衡)
7. [会话历史管理与截断](#7-会话历史管理与截断)
8. [Thinking 标签解析](#8-thinking-标签解析)
9. [Prompt Caching](#9-prompt-caching)
10. [Truncation Recovery](#10-truncation-recovery)
11. [Payload Guards](#11-payload-guards)
12. [Conversation Sanitization](#12-conversation-sanitization)
13. [Protocol Handlers 对比](#13-protocol-handlers-对比)
14. [Admin API 与 Web UI](#14-admin-api-与-web-ui)
15. [Responses API (Codex CLI)](#15-responses-api-codex-cli)
16. [Gemini Protocol](#16-gemini-protocol)
17. [独有功能清单](#17-独有功能清单)
18. [Bug 与正确性风险](#18-bug-与正确性风险)
19. [综合评分](#19-综合评分)

---

## 1. 总体架构与技术栈

| 维度 | model-proxy (Rust) | KiroProxy (Python) |
|------|-------------------|---------------------|
| **语言** | Rust 2021, Tokio async | Python 3.11+, asyncio |
| **Web 框架** | Axum | FastAPI + uvicorn |
| **桌面应用** | Tauri 2.0 (React 18 + Ant Design 5) | 无，纯 HTTP 服务 |
| **CLI 模式** | 有 (`src/main.rs`) | 有 (`kiro_proxy/cli.py`) |
| **打包** | Cargo + npm + Tauri bundler | PyInstaller (`KiroProxy.spec`) |
| **核心源码行数** | ~14,350 行 (33 files) | ~15,000+ 行 (31+ files) |
| **入站协议** | Anthropic Messages + OpenAI Chat + Responses | Anthropic + OpenAI Chat + Responses + **Gemini** |
| **出站协议** | Kiro EventStream (唯一) | Kiro EventStream (唯一) |
| **并发模型** | Tokio 多线程异步 + ArcSwap 无锁读 | asyncio 单线程事件循环 |
| **锁策略** | `Arc<Mutex<T>>`, `Arc<RwLock<T>>`, `ArcSwap` | 全局单例 + threading.Lock |
| **错误分类** | `ErrorClass` enum: Recoverable/Suspended/Fatal | `classify_error()` 函数返回字符串 |

**判定**: Rust 版架构更严谨，无锁读 (`ArcSwap`) + 结构化错误类型。Python 版更轻量，但 asyncio 单线程限制了 CPU-bound 性能。KiroProxy 独有 Gemini 支持。

---

## 2. 认证系统 (Auth)

### 2.1 凭证数据结构

| 字段 | model-proxy (`KiroCredential`) | KiroProxy (`types.py`) |
|------|-------------------------------|------------------------|
| `auth_method` | Social / IdC / ApiKey (enum) | 无，隐式推断 |
| `access_token` | 有 | 有 |
| `refresh_token` | 有 | 有 |
| `client_id` / `client_secret` | 有 | 有 |
| `profile_arn` | 有 | 有 |
| `expires_at` | 有，带 5min/10min 双阈值 | 有，单一阈值 |
| `region` / `api_region` | 分离 (可选不同区域) | 仅单一 `region` |
| `machine_id` | 有，哈希生成 | 有 (`fingerprint.py`) |
| `disabled` | 有，`invalid_grant` 自动禁用 | 有，手动 toggle |
| `source` | `CredentialSource` enum (6种来源) | 无，不追踪来源 |

**判定**: model-proxy 凭证模型更丰富，支持 `api_region` 分离、来源追踪、自动禁用。

### 2.2 Token 刷新

| 能力 | model-proxy | KiroProxy |
|------|------------|-----------|
| Social 刷新 | `POST prod.{region}.auth.desktop.kiro.dev/refreshToken` | 委托给 `TokenRefresher` (相同端点) |
| IdC 刷新 | `POST oidc.{region}.amazonaws.com/token` | 有 (`refresher.py`) |
| 级联回退 | 5层: 内存→持久化文件→网络刷新→SQLite→SSO cache→返回过期token | 无级联，单一路径 |
| 持久化 | `~/.config/model-proxy/kiro-token-{region}.json` (0o600) | 无文件持久化 |
| 写回源 | SQLite (通过 stdin, 防泄露) + SSO cache + Config | SQLite (`sqlite_auth.py`, 参数化查询) |
| User-Agent | `KiroIDE-0.11.107-{machine_id}` (可配置) | `KiroIDE-{version}-{machine_id}` |

**判定**: **model-proxy 完胜**。5层级联回退远比 KiroProxy 的单一路径健壮。文件持久化 + 自动写回源是生产级必需。

### 2.3 OAuth / SSO 流程

| 能力 | model-proxy | KiroProxy |
|------|------------|-----------|
| OIDC Device Flow | 完整 (注册→设备码→轮询) | 有 (`start_kiro_login` → `poll_kiro_login`) |
| Social OAuth + PKCE | Google + GitHub，S256 挑战 | 有 (`start_social_login` → `exchange_social_token`) |
| IAM IdC PKCE Flow | 授权码 + state 验证 | **无** |
| SSO Token 导入 | 7步自动化 (设备注册→bearer验证→关联) | **无** |
| Remote Login | **无** | 有 (生成链接 + HTML页面 + 手动输入 token) |
| 登录状态管理 | `Mutex<HashMap>` + TTL (10min) | 内存中的 `login_state` 对象 |

**判定**: model-proxy 有更完整的 SSO 流程。KiroProxy 独有 Remote Login (远程登录页面)，适合无 GUI 场景。

### 2.4 凭证安全

| 维度 | model-proxy | KiroProxy |
|------|------------|-----------|
| 文件权限 | 0o600 文件 / 0o700 目录 | 默认 umask |
| Token 验证 | 长度限制 (8192) + 控制字符检测 | 无额外验证 |
| SQLite 写入 | stdin 调 sqlite3 CLI (防 argv 泄露) | 参数化查询 |
| SQL 注入防护 | `sqlite_quote_literal()` + 单元测试 | ORM 参数化 |
| SQLITE_READONLY | **无** | 有 (生产安全) |

**判定**: model-proxy 安全实践更严格。Python 版的 SQLITE_READONLY 是好的生产实践。

---

## 3. EventStream 二进制协议解析

**这是最关键的模块之一。**

| 维度 | model-proxy (`eventstream.rs`, 873行) | KiroProxy (`providers/kiro.py`, 352行) |
|------|---------------------------------------|---------------------------------------|
| **解析器类型** | 流式状态机 (`EventStreamDecoder`) | 批量解析 (`parse_response(raw)`) |
| **CRC32 校验** | 完整双 CRC: Prelude CRC + Message CRC (`CRC_32_ISO_HDLC`) | **无 CRC 校验** |
| **Header 类型支持** | 全部 10 种 AWS 类型 (BoolTrue~Uuid) | 无 header 解析，字符串搜索 |
| **Header 解析** | 完整: name_len + name + type_byte + value | **跳过 headers**，直接搜索 payload |
| **Payload 提取** | 精确: total_len - headers_len - 16 字节偏移 | 粗略: headers 区域搜索关键词后取 JSON |
| **JSON Text 回退** | **无** | 有: `_iter_json_objects_from_text()` 扫描原始字节 |
| **流式处理** | `BytesMut` buffer + `feed()`/`decode()` | 全量 `bytes` 一次性解析 |
| **错误恢复** | 跳 1字节 / 跳整帧 / 5次连续错误停止 | 无恢复逻辑 |
| **Event 类型** | 6 种结构化: AssistantResponse/ToolUse/Reasoning/ContextUsage/Error/Exception | 无类型区分，返回 dict |
| **Tool 输入缓冲** | 在 `stream/state.rs` 中按 `toolUseId` 累积 | 在 `parse_response()` 中累积 `input_parts` |
| **最大帧大小** | 16MB (拒绝更大的帧) | 无限制 |

### 关键差异分析

**model-proxy 优势**:
1. **CRC 校验**: 完整的 AWS EventStream 双 CRC 校验，能检测传输错误
2. **流式处理**: 真正的流式解码器，可以边收边解，不需要等完整响应
3. **Header 解析**: 支持全部 10 种 AWS header 类型，正确提取 `message_type`、`event_type` 等元数据
4. **错误恢复**: 字节级恢复策略，能从损坏数据中恢复
5. **Event 结构化**: 区分 6 种事件类型，类型安全

**KiroProxy 优势**:
1. **JSON Text 回退**: 当二进制协议损坏时 (如经过代理链)，能回退到文本扫描模式
2. **简单可靠**: 不做 header 解析，减少出错点，对畸形数据更宽容

**判定**: **model-proxy 协议实现更正确更完整**。KiroProxy 的 JSON 回退是一个实用的防御性设计，建议 model-proxy 采纳。

---

## 4. 请求转换: Anthropic/OpenAI/Gemini → Kiro

### 4.1 Anthropic → Kiro 转换

| 能力 | model-proxy (`request/`, 1317行) | KiroProxy (`converters.py`, 1296行) |
|------|----------------------------------|--------------------------------------|
| **消息提取** | `process_messages()` → history + currentMessage | `convert_anthropic_messages_to_kiro()` → (user_content, history, tool_results) |
| **系统提示注入** | `build_system_history()` 注入为首个 user/assistant 对 | 直接拼接到首个 user message 前面 |
| **图片处理** | `convert_image_block()` / `convert_openai_image_block()` | `extract_images_from_content()` 两种格式 |
| **工具转换** | `convert_tools()`: name shortening + schema 规范化 | `convert_anthropic_tools_to_kiro()`: 50 上限 + description 截断 |
| **Tool name 缩短** | `shorten_tool_name()`: prefix + _ + 8 hex chars (>63 chars) | **无** (原始名称传递) |
| **Tool description offload** | 超长描述移入 system prompt | 截断到 9216 chars |
| **Schema 规范化** | `normalize_json_schema()`: 剥离 additionalProperties, 确保 type:object | **无** |
| **Web Search 注入** | 通过 MCP 模块 (`mcp.rs`) | 通过 `web_search` tool 映射 |
| **Thinking 配置** | 支持 Anthropic `thinking` + OpenAI `reasoning_effort` | 支持 Anthropic `thinking` (budget/effort) |
| **cache_control 转换** | `convert_cache_control()` → `cachePoint` | `apply_tool_cache_points()` + `apply_message_cache_points()` |
| **历史交替修复** | `ensure_alternating()` + `ConversationSanitizer` (5 pass) | `fix_history_alternation()` (1 pass) |
| **Payload 大小检查** | 600KB 上限 + 自动裁剪 history | 615KB 上限 (`payload_guards.py`) |
| **截断恢复提示** | 注入 system prompt 告知模型 `[System Notice]` 是合法消息 | **无** |

### 4.2 OpenAI → Kiro 转换

| 能力 | model-proxy | KiroProxy (`converters.py`) |
|------|------------|----------------------------|
| **消息转换** | 通过 Anthropic 格式中转 | `convert_openai_messages_to_kiro()` 直接转换 |
| **Tool 处理** | 同 Anthropic 路径 | `convert_openai_tools_to_kiro()` + tool_choice 注入 |
| **tool_choice: required** | 无特殊处理 | 注入 system prompt 强制工具调用 |

### 4.3 关键差异

**model-proxy 优势**:
1. **Tool name shortening**: Kiro 对工具名有 63 字符限制，model-proxy 通过哈希缩短 + 反向映射解决
2. **Schema normalization**: 递归清理 JSON Schema 中 Kiro 不支持的字段
3. **Description offload**: 超长描述不截断，而是移入 system prompt 保持完整
4. **5-pass Sanitizer**: 更彻底的会话清洗，包括空消息剥离、orphan 修复

**KiroProxy 优势**:
1. **直接 OpenAI 转换**: 不经过 Anthropic 格式中转，减少信息损失
2. **tool_choice 支持**: `required` 模式有显式处理
3. **Gemini 转换**: 完整的 Gemini → Kiro 转换链，model-proxy 无此能力
4. **历史交替修复更简单**: 单 pass 够用，不需要 5 pass

**判定**: model-proxy 在工具处理和会话清洗上更精细。KiroProxy 在协议覆盖面上更广 (Gemini)、OpenAI 转换更直接。

---

## 5. 流式响应处理: Kiro → 客户端 SSE

| 维度 | model-proxy (`stream/`, 1329行) | KiroProxy (`handlers/anthropic.py`) |
|------|--------------------------------|--------------------------------------|
| **解析器** | `EventStreamDecoder` + `AnthropicStreamState` 状态机 | 内联二进制解析 (4字节长度前缀) |
| **文本去重** | **有**: Kiro 发送累积内容，model-proxy 提取增量部分 | **无**: 直接发送累积文本 (可能重复) |
| **Tool 输入累积** | `tool_input_buffers: HashMap<String, String>` 按 ID 累积 | `tool_input_buffer` dict 累积 |
| **工具名反向映射** | `tool_name_map` 把缩短名映射回原始名 | **无** (原始名直接使用) |
| **Keep-alive** | 定时发送 `: keepalive\n\n` (streaming_read_timeout/12) | **无** |
| **超时处理** | 可配置 first-token timeout + stream timeout | 仅 httpx 客户端超时 |
| **缓冲模式** | 两种: 直接流式 + 缓冲 (用于 `/cc/v1/messages` 补丁 input_tokens) | 仅直接流式 |
| **Thinking 解析** | 集成 `ThinkingParser`，4种模式 | 集成 `ThinkingParser`，4种模式 |
| **Truncation 检测** | 流中检测 `ContentLengthExceededException` | 流后检测 (HTTP 状态码) |
| **错误分类** | `Error`/`Exception` Event 类型 + 429/503 账号切换 | `classify_error()` + 账号切换/禁用 |
| **SSE 格式** | Anthropic SSE: message_start/delta/stop | 相同 |

### 关键差异

1. **文本去重是最大差异**: Kiro 发送的是累积文本 (每次包含之前所有内容)，不是增量 delta。model-proxy 通过 `last_content` 比对只发送新增部分，KiroProxy 会发送累积文本导致**客户端重复渲染**。

2. **Keep-alive**: 长时间处理时，model-proxy 发送 SSE 注释保活，防止客户端/代理超时断开。KiroProxy 无此机制。

3. **双模式流**: model-proxy 有 direct + buffered 模式，buffered 模式在 `/cc/v1/messages` 端点使用，先收集所有事件再统一发送，可以修补 `message_start` 中的 token 计数。

**判定**: **model-proxy 流式处理显著优于 KiroProxy**。文本去重和 keep-alive 是关键功能缺失。

---

## 6. 多账号管理与负载均衡

| 能力 | model-proxy (`account.rs`, 1256行) | KiroProxy (`core/account.py` + `admin.py`) |
|------|-----------------------------------|---------------------------------------------|
| **负载均衡模式** | Priority (粘性) / Balanced (轮询) / **Smart (复合评分)** | Sticky + Failover |
| **Smart 评分因素** | 6项: health + inflight_penalty + usage_balance + zero_use + idle + latency | 无 |
| **熔断器** | 三态: Active / Broken / HalfOpen | 指数退避 (相同公式) |
| **健康评分** | 0-100 分 (失败-20, 成功+10) | 仅失败计数 |
| **自愈** | 全部不可用时减半错误计数 + 强制 HalfOpen | **无** |
| **概率重试** | 10% 概率重试 Broken 账号 | 10% 概率 (相同) |
| **Inflight 追踪** | 有 (per-account inflight_count) | 无 |
| **Latency EMA** | 有 (alpha=0.3) | 无 |
| **Per-Model 追踪** | **无** | 有，动态学习模型→账号映射 |
| **状态持久化** | `account-state.json` (tmp+rename 原子写入) | `state.json` (原子写入) |
| **定期保存** | 后台 tokio task 每 30 秒 | 无定期保存 |

**判定**: model-proxy 负载均衡能力远超 KiroProxy (Smart 评分 + 自愈 + inflight/latency 追踪)。KiroProxy 的 per-model 账号映射是独有优势。

---

## 7. 会话历史管理与截断

| 能力 | model-proxy (`history.rs` + `truncation.rs`) | KiroProxy (`core/history_manager.py`, 34KB) |
|------|----------------------------------------------|----------------------------------------------|
| **截断策略** | 3种: AutoTruncate / ErrorRetry / PreEstimate | **智能摘要**: 调用 Kiro Haiku 模型总结旧消息 |
| **消息数限制** | 30 条 (可配置) | 可配置 |
| **字符数限制** | 150,000 字符 | 可配置 |
| **渐进截断** | `truncate_for_retry()`: 按百分比递减 | `auto_truncate()`: 逐条删除 |
| **Smart Summary** | `build_summary_text()`: 简单前200字符截取 | **完整 LLM 摘要**: 调用 `claude-haiku-4.5` 生成摘要 |
| **交替修复** | `ensure_alternating()`: 插入 "OK" 占位 | `fix_history_alternation()`: "I understand"/"Continue" |
| **Content-Length 错误重试** | `TRUNCATION_TIERS: [0.5, 0.25, 0.0]` 三级渐进 | 按百分比重试 |
| **状态管理** | `TruncationState`: Arc<Mutex<HashMap>> 异步安全 | `TruncationState`: dataclass 同步 |
| **恢复消息注入** | `build_recovery_messages()`: tool_result + user message | `inject_truncation_recovery()`: tool_result + user message |
| **JSON 截断检测** | `is_json_truncated()`: 字符级括号计数 (处理字符串转义) | `detect_truncation()`: 简单计数 (不处理转义) |

### 关键差异

1. **LLM 摘要 vs 简单截取**: KiroProxy 调用 Haiku 模型生成真正的对话摘要，保留语义信息。model-proxy 只取前 200 字符，信息损失大。这是 **KiroProxy 的重要优势**。

2. **JSON 截断检测**: model-proxy 正确处理字符串内的括号和转义字符，KiroProxy 的简单计数可能误报。

3. **渐进截断**: model-proxy 的 3 级 tier 策略 (50%→25%→0%) 更精细，逐步缩小而不是一步到底。

**判定**: KiroProxy 的 LLM 摘要是重要优势。model-proxy 在截断检测精度和渐进策略上更好。

---

## 8. Thinking 标签解析

| 维度 | model-proxy (`thinking_parser.rs`, 382行) | KiroProxy (`thinking_parser.py`, 206行) |
|------|------------------------------------------|----------------------------------------|
| **FSM 状态** | PreContent / InThinking / Streaming | PRE_CONTENT / IN_THINKING / STREAMING |
| **支持标签** | `<thinking>`, `<think>`, `<reasoning>`, `<thought>` | 可配置 (`THINKING_OPEN_TAGS` env var) |
| **初始缓冲** | 20 字符 | 可配置 (`THINKING_INITIAL_BUFFER_SIZE`) |
| **谨慎发送** | 30 字符尾部缓冲 (防止截断闭合标签) | 等待完整闭合标签 (不渐进发送) |
| **处理模式** | 4种: AsReasoningContent / Remove / Pass / StripTags | 4种: 相同 (可配置) |
| **配置方式** | 编译时 `ThinkingHandlingMode::from_str()` | 环境变量 `THINKING_HANDLING` |
| **输出类型** | `ThinkingOutput` enum: ThinkingDelta / ContentDelta / None | `ThinkingResult` dataclass: thinking_content / regular_content |
| **Flush** | `finalize()` 方法 | `flush()` 方法 |
| **测试覆盖** | 7 个测试 (含 split closing tag) | 无内联测试 |

### 关键差异

1. **谨慎发送策略不同**: model-proxy 在 InThinking 状态下保留尾部 30 字符缓冲区，渐进发送安全部分；KiroProxy 等到找到完整闭合标签才发送所有内容。model-proxy 的策略**延迟更低**。

2. **可配置性**: KiroProxy 通过环境变量支持任意标签列表，更灵活。model-proxy 硬编码 4 种标签。

3. **测试覆盖**: model-proxy 有完整的单元测试，包括边界 case (split closing tag)。

**判定**: 基本对等。model-proxy 的渐进发送策略略优，KiroProxy 的标签可配置性更好。

---

## 9. Prompt Caching

| 维度 | model-proxy (`prompt_cache.rs`, 699行) | KiroProxy (`prompt_caching.py`, 122行) |
|------|----------------------------------------|----------------------------------------|
| **格式转换** | `convert_cache_control()`: tool→cachePoint, message→剥离 | `apply_tool_cache_points()` + `apply_message_cache_points()` |
| **History cache** | `add_history_cache_points()`: 系统 history 行标记 | `apply_system_cache_point()` (仅追踪，不实际转换) |
| **Cache Tracker** | **完整 SHA256 指纹系统**: 按 account_id 追踪，TTL 5min/1h，计算 cache_creation/cache_read | **无** |
| **Token 估算** | 字符类启发式 (`estimate_approx_tokens()`) | 无 (依赖外部 tokenizer) |
| **TTL 解析** | 支持 `1h`/`5m`/`300s` 字符串 + 数字秒 | 无 |
| **Opus 特殊处理** | 4096 token 最低阈值 (vs 默认 1024) | 无 |
| **Canonical 化** | 确定性 JSON 序列化 (排序 key，剥离 cache_control) | 无 |
| **测试覆盖** | 10+ 测试 (含跨 account 隔离验证) | 无 |

**判定**: **model-proxy 完胜**。SHA256 指纹缓存追踪系统是 KiroProxy 完全没有的功能，能在响应中正确报告 `cache_creation_input_tokens` 和 `cache_read_input_tokens`，对 Claude Code 等客户端的计费显示至关重要。

---

## 10. Truncation Recovery

| 维度 | model-proxy (`truncation.rs`, 321行) | KiroProxy (`truncation_recovery.py`, 179行) |
|------|--------------------------------------|----------------------------------------------|
| **截断原因** | 3种 enum: MissingUsage / TruncatedToolCall / ContentLengthExceeded | 通用 detect (启发式) |
| **检测方法** | `is_json_truncated()`: 字符级括号+转义处理 | `detect_truncation()`: 简单计数 + JSON parse 尝试 |
| **Content-Length Tier** | `[0.5, 0.25, 0.0]` 三级渐进截断 | 无 |
| **System Prompt 告知** | 有: `get_truncation_recovery_system_prompt()` 告知模型 `[System Notice]` 是合法的 | **无** |
| **状态管理** | 异步 `TruncationState` (Arc<Mutex<HashMap>>) | 同步 `TruncationState` (dataclass) |
| **恢复消息** | 按原因分类: tool_result / user message / content notice | 统一消息 |
| **Markdown 检测** | **无** | 有: 检测未闭合的 ``` 代码块 |
| **Tool call JSON 验证** | `check_tool_call_truncation()` | json.loads 尝试 |

**判定**: model-proxy 的 system prompt 告知机制是关键优势——模型收到 `[System Notice]` 时不会误认为是 prompt injection。KiroProxy 的 markdown 检测和 JSON parse 验证是好的补充。

---

## 11. Payload Guards

| 维度 | model-proxy | KiroProxy (`payload_guards.py`, 170行) |
|------|------------|----------------------------------------|
| **大小检查** | 600KB 上限，集成在 `request/mod.rs` 中 | 615KB (`KIRO_MAX_PAYLOAD_BYTES`), 独立模块 |
| **自动裁剪** | 有: `truncate_kiro_payload_history()` | 有: `trim_payload_to_limit()` |
| **裁剪后修复** | `ensure_alternating()` | `_repair_orphaned_tool_results()` + `_align_to_user_message()` |
| **空 toolUses 清理** | 无 | 有: `_strip_empty_tool_uses()` |
| **Stats 返回** | 无 | 有: `PayloadTrimStats` |
| **开关** | 始终启用 | 可配置 `AUTO_TRIM_PAYLOAD` |

**判定**: KiroProxy 的 payload guard 更完善：空 toolUses 清理、裁剪后 orphan 修复、可配置开关。model-proxy 缺少空 toolUses 清理可能导致 Kiro API 拒绝。

---

## 12. Conversation Sanitization

| 维度 | model-proxy (`sanitize.rs`, 369行) | KiroProxy (`converters.py::fix_history_alternation`) |
|------|--------------------------------------|------------------------------------------------------|
| **Pass 数量** | 5 pass: strip_empty → boundary_first → alternation → boundary_last → orphan_repair | 1 pass: alternation + orphan fix |
| **空消息剥离** | 有 (保留首个 user) | **无** |
| **边界守卫** | 首条必须 user (插入 "Hello"), 末条必须 user (插入 "Continue") | 无显式检查 |
| **交替修复** | 连续 user: 插入 "understood" assistant; 连续 assistant: 插入 "Continue" user | 插入 "I understand" / "Continue" |
| **Orphan 修复** | assistant 有 tool_use 但下一 user 无 tool_result → 注入合成 error result | 清除孤立 tool_use / tool_result |
| **统计报告** | `SanitizeResult`: inserted/modified/orphans_repaired | 无统计 |

**判定**: **model-proxy 显著更完善**。5 pass 清洗比 1 pass 更可靠，边界守卫是 Kiro API 的硬性要求。

---

## 13. Protocol Handlers 对比

### 13.1 Anthropic Handler

| 能力 | model-proxy | KiroProxy (`handlers/anthropic.py`, 630行) |
|------|------------|--------------------------------------------|
| **count_tokens** | 有 (`/v1/messages/count_tokens`) | 有 (tiktoken 估算) |
| **流式重试** | 有 (429→账号切换, 503→指数退避) | 有 (相同策略, max_retries=2) |
| **Content-Length 重试** | 3 级 tier 渐进截断 + 重试 | history manager 截断 + 重试 |
| **Profile ARN** | 有 (`discover_profile_arn()`) | 有 (`ensure_profile_arn_ready()`) |
| **LLM 摘要** | **无** | 有: 调 Haiku 生成对话摘要 |
| **事件解析** | 独立 `EventStreamDecoder` | 内联 4字节解析 |

### 13.2 OpenAI Handler

| 能力 | model-proxy | KiroProxy (`handlers/openai.py`, 347行) |
|------|------------|----------------------------------------|
| **流式实现** | 真正流式 (binary event-stream 解析) | **假流式**: 获取完整响应后按 20 字符分片 |
| **tool_calls 支持** | 有 (stream + non-stream) | 有 |
| **thinking 提取** | `extra_body.anthropic.thinking` | `extra_body.anthropic.thinking` (相同) |

**判定**: KiroProxy 的 OpenAI handler 流式实现是**严重缺陷**——20 字符分片 + 20ms 延迟不是真正的流式，增加延迟且浪费资源。

---

## 14. Admin API 与 Web UI

| 能力 | model-proxy | KiroProxy |
|------|------------|-----------|
| **Admin HTTP API** | `/api/admin/*` (credentials, endpoints, settings) | `/api/admin/*` (accounts, flows, usage) |
| **Tauri IPC** | 20+ commands (前端直接调用) | **无** |
| **Web UI** | Tauri 桌面应用 (React + Ant Design) | 内嵌 WebUI (`web/webui.py`, **102KB**) |
| **账户 CRUD** | 有 | 有 (更丰富: import/export, manual token) |
| **Token 扫描** | 有 (SQLite + SSO cache) | 有 (SSO cache, 更详细: IdC config 检查) |
| **Flow 监控** | 有 (bookmark/note/tag/query) | 有 (bookmark/note/tag/export) |
| **Health Check** | 有 (`/health`) | 有 (probe all accounts) |
| **Speed Test** | **无** | 有: `speedtest()` 延迟测试 |
| **Usage 查询** | 有 | 有 |
| **Remote Login** | **无** | 有: 生成链接 + HTML 页面 |
| **Export/Import** | **无** | 有: 账户凭证全量导出/导入 |

**判定**: KiroProxy 的 Admin API 功能更丰富 (Remote Login、Export/Import、Speedtest)。model-proxy 的 Tauri IPC 是独有优势，前端体验更好。

---

## 15. Responses API (Codex CLI)

| 能力 | model-proxy (`responses.rs`, 414行) | KiroProxy (`handlers/responses.py`, 1107行) |
|------|--------------------------------------|----------------------------------------------|
| **转换策略** | 两步: Responses → Anthropic → Kiro | 直接: Responses → Kiro |
| **Tool call 类型** | function_call, custom_tool_call | function_call, custom_tool_call, **local_shell_call**, **tool_search_call** |
| **Tool output 类型** | function_call_output, custom_tool_call_output | function_call_output, custom_tool_call_output, **mcp_tool_call_output**, **tool_search_output** |
| **Special calls** | web_search_call, image_generation_call | web_search_call, image_generation_call |
| **Tool 转换** | parameters → input_schema 转换 | flat format 直接处理 + local_shell 特殊 schema |
| **Streaming** | 有: `events_to_responses_sse()` | 有: `_handle_stream()` 真正流式 |
| **Debug 模式** | **无** | 有: `KIRO_PROXY_DEBUG_RESPONSES` 环境变量 |
| **AUTO_TRUNCATE** | **无** | 有: Codex CLI 强制使用 |
| **代码行数** | 414 行 | 1107 行 (2.7x) |

### 关键差异

1. **Tool 类型覆盖**: KiroProxy 支持 `local_shell_call` 和 `tool_search_call`，这是 Codex CLI 的新工具类型。model-proxy 可能无法正确处理这些新工具。

2. **转换策略**: model-proxy 两步转换 (Responses→Anthropic→Kiro) 增加了一层间接性，可能引入信息损失。KiroProxy 直接转换更高效。

3. **Debug 模式**: KiroProxy 可以将请求保存到 `debug_requests/` 目录，对排查协议问题很有帮助。

**判定**: KiroProxy 的 Responses API 支持更完整，尤其是新工具类型。model-proxy 的两步转换策略有设计上的灵活性 (复用 Anthropic 转换器)，但在边界 case 上容易出错。

---

## 16. Gemini Protocol

| 维度 | model-proxy | KiroProxy |
|------|------------|-----------|
| **支持** | **不支持** | 完整支持 (`handlers/gemini.py`, 277行) |
| **端点** | N/A | `/v1/models/{model}:generateContent` |
| **工具转换** | N/A | `convert_gemini_tools_to_kiro()` (functionDeclarations + webSearch) |
| **消息转换** | N/A | `convert_gemini_contents_to_kiro()` (5-tuple 返回) |
| **图片支持** | N/A | 有: inlineData → Kiro 格式 |
| **Streaming** | N/A | **仅非流式** |
| **代码量** | 0 | ~600 行 (handler + converter) |

**判定**: KiroProxy 独有 Gemini 支持，但仅非流式。对 Gemini CLI / AI Studio 用户有用。

---

## 17. 独有功能清单

### model-proxy 独有

| 功能 | 模块 | 影响 |
|------|------|------|
| **Tauri 桌面应用** | `src-tauri/` + `ui/` | 图形化管理，系统托盘，原生体验 |
| **多 Provider 代理** | `config.rs`, `provider_dispatch.rs` | 不只是 Kiro，还支持 OpenAI/Anthropic/DeepSeek/Gemini/Azure |
| **ArcSwap 无锁读** | `state.rs` | 零竞争的活跃 Provider 切换 |
| **Fallback 链** | `fallback.rs` | 自动重试备份 Provider |
| **EventStream CRC 校验** | `eventstream.rs` | 传输错误检测 |
| **流式文本去重** | `stream/state.rs` | 防止客户端重复渲染 |
| **Tool name shortening** | `request/tools.rs` | 处理 Kiro 63 字符限制 |
| **Schema normalization** | `request/tools.rs` | 清理 Kiro 不支持的 JSON Schema 字段 |
| **SHA256 Prompt Cache Tracker** | `prompt_cache.rs` | 正确报告 cache_creation/cache_read tokens |
| **5-pass Conversation Sanitizer** | `sanitize.rs` | 更彻底的会话清洗 |
| **System Prompt Truncation 告知** | `truncation.rs` | 防止模型误解恢复消息 |
| **IAM IdC PKCE Flow** | `auth_flow.rs` | 企业 SSO 登录 |
| **7-step SSO Token Import** | `auth_flow.rs` | 自动化凭证获取 |
| **Keep-alive 机制** | `stream/handler.rs` | 防止长处理时连接超时 |
| **Buffered streaming** | `stream/handler.rs` | `/cc/v1/messages` 精确 token 计数 |
| **Endpoint 多端点 + 健康追踪** | `endpoint.rs` + `endpoint_health.rs` | Kiro/Codewhisperer/AmazonQ 三端点冗余 |
| **Smart Load Balancing** | `account.rs` | 6 因素复合评分 |
| **Self-heal** | `account.rs` | 全部不可用时自动恢复 |
| **State 持久化 (periodic)** | `account.rs` | 30s 定期保存 |
| **5 层 Token 级联回退** | `auth.rs` | 内存→文件→网络→SQLite→SSO |

### KiroProxy 独有

| 功能 | 模块 | 影响 |
|------|------|------|
| **Gemini Protocol** | `handlers/gemini.py` | Gemini CLI / AI Studio 支持 |
| **JSON Text 回退** | `providers/kiro.py` | 二进制损坏时仍能解析 |
| **LLM Smart Summary** | `core/history_manager.py` | Haiku 模型生成对话摘要 |
| **Remote Login Page** | `handlers/admin.py` | 无 GUI 场景的远程登录 |
| **Export/Import Accounts** | `handlers/admin.py` | 凭证全量迁移 |
| **Speed Test** | `handlers/admin.py` | 端点延迟测试 |
| **Per-Model 账号映射** | `core/account.py` | 动态学习模型→最优账号 |
| **PyInstaller 打包** | `KiroProxy.spec` | 跨平台二进制分发 |
| **Debug 请求保存** | `handlers/responses.py` | 协议问题排查 |
| **AUTO_TRUNCATE for Codex** | `handlers/responses.py` | Codex CLI 强制截断策略 |
| **Markdown 截断检测** | `truncation_recovery.py` | 检测未闭合 ``` 代码块 |
| **tokenizer (tiktoken)** | `tokenizer.py` | 精确 token 计数 |
| **local_shell_call / tool_search_call** | `handlers/responses.py` | Codex CLI 新工具类型 |
| **mcp_tool_call_output / tool_search_output** | `handlers/responses.py` | Responses API 新输出类型 |
| **Configurable thinking tags** | `thinking_parser.py` | 任意标签列表 (env var) |
| **Empty toolUses cleanup** | `payload_guards.py` | 防止 Kiro API 拒绝 |
| **Event logging batch** | `handlers/admin.py` | 客户端遥测收集 |
| **Per-account proxy** | `handlers/admin.py` | 每个账号独立代理设置 |
| **Web UI (102KB)** | `web/webui.py` | 内嵌 Web 管理界面 |

---

## 18. Bug 与正确性风险

### model-proxy

| # | 问题 | 严重程度 | 说明 |
|---|------|---------|------|
| 1 | **无 Gemini 支持** | 中 | Gemini CLI 用户无法使用 |
| 2 | **无 JSON Text 回退** | 中 | 经过代理链时二进制解析可能失败 |
| 3 | **两步 Responses 转换** | 低 | Responses→Anthropic→Kiro 可能丢失信息 |
| 4 | **无空 toolUses 清理** | 中 | Kiro API 可能拒绝空 toolUses 数组 |
| 5 | **Smart Summary 只截取 200 字符** | 高 | 长对话上下文损失严重 |
| 6 | **无 Remote Login** | 低 | 纯 CLI 无 GUI 场景不便 |
| 7 | **blocking_lock()** | 中 | `account_region()` 和 `account_full_snapshot()` 用 blocking_lock，异步上下文可能死锁 |
| 8 | **无 local_shell_call / tool_search_call** | 中 | Codex CLI 新工具类型可能无法处理 |

### KiroProxy

| # | 问题 | 严重程度 | 说明 |
|---|------|---------|------|
| 1 | **假流式 OpenAI** | 高 | 20字符+20ms 分片，不是真正流式 |
| 2 | **文本无去重** | 高 | Kiro 累积内容直接发送，客户端重复渲染 |
| 3 | **无 CRC 校验** | 中 | 传输错误无法检测 |
| 4 | **无 Keep-alive** | 中 | 长处理时客户端可能超时断开 |
| 5 | **JSON 截断检测误报** | 低 | 简单计数不处理字符串内括号 |
| 6 | **文件权限默认 umask** | 中 | Token 文件可能被其他用户读取 |
| 7 | **无 Prompt Cache 追踪** | 高 | 响应中无 cache_creation/cache_read tokens，客户端计费显示不准 |
| 8 | **SQLite key 拼写 `odic`** | 中 | 可能与实际 kiro-cli 不一致 |

---

## 19. 综合评分

| 维度 | model-proxy | KiroProxy | 说明 |
|------|:-----------:|:---------:|------|
| **协议正确性** | 9/10 | 6/10 | CRC 校验、流式去重、结构化 Event |
| **协议覆盖度** | 7/10 | 9/10 | KiroProxy 多 Gemini、多 tool 类型 |
| **认证健壮性** | 9/10 | 6/10 | 5层级联、文件持久化、写回源 |
| **负载均衡** | 9/10 | 5/10 | Smart 评分、自愈、inflight 追踪 |
| **会话管理** | 8/10 | 8/10 | 各有优势 (5-pass vs LLM 摘要) |
| **Prompt Cache** | 10/10 | 3/10 | SHA256 指纹系统 vs 无 |
| **流式处理** | 9/10 | 5/10 | 去重、keep-alive、双模式 |
| **安全性** | 9/10 | 5/10 | 文件权限、token 验证、stdin SQLite |
| **可调试性** | 6/10 | 8/10 | Debug 保存、WebUI、详尽日志 |
| **部署便捷性** | 6/10 | 8/10 | PyInstaller vs Tauri+Node |
| **Admin API** | 7/10 | 8/10 | Remote Login、Export/Import、Speedtest |
| **测试覆盖** | 8/10 | 7/10 | 两边都有，model-proxy 结构化更好 |
| **代码质量** | 9/10 | 7/10 | 类型安全、错误处理、模块化 |
| **总体** | **8.2/10** | **6.5/10** | |

### 总结

**model-proxy 的核心优势**:
- 协议实现更正确 (CRC、流式去重、结构化 Event)
- 认证系统更健壮 (5层级联、文件持久化)
- 负载均衡更智能 (Smart 评分、自愈)
- Prompt Cache 追踪 (SHA256 指纹)
- 会话清洗更彻底 (5-pass Sanitizer)
- 安全性更高 (文件权限、token 验证)

**KiroProxy 应该被采纳的能力**:
1. **LLM Smart Summary** — 用 Haiku 生成对话摘要，对长对话体验至关重要
2. **JSON Text 回退** — 代理链环境下二进制损坏时的防御性解析
3. **Gemini Protocol** — 扩展协议覆盖面
4. **Remote Login Page** — 无 GUI 场景的远程认证
5. **Export/Import Accounts** — 凭证迁移能力
6. **Empty toolUses cleanup** — 防止 Kiro API 拒绝
7. **local_shell_call / tool_search_call** — Codex CLI 新工具类型支持
8. **Debug 请求保存** — 协议问题排查工具
