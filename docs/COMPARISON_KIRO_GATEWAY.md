# model-proxy (Rust) vs kiro-gateway (Python) 像素级对比报告

> 对比日期: 2026-06-17
> 对比对象: model-proxy (Rust/Axum/Tauri) vs kiro-gateway (Python/FastAPI)
> 对比范围: Kiro 网关功能的全部核心模块

---

## 目录

1. [总体架构](#1-总体架构)
2. [认证系统](#2-认证系统)
3. [EventStream 二进制协议解析](#3-eventstream-二进制协议解析)
4. [请求转换 (Anthropic → Kiro)](#4-请求转换-anthropic--kiro)
5. [流式响应处理 (Kiro → Anthropic/OpenAI SSE)](#5-流式响应处理-kiro--anthropicopenai-sse)
6. [请求分发与错误恢复](#6-请求分发与错误恢复)
7. [会话历史与截断处理](#7-会话历史与截断处理)
8. [Thinking 标签提取](#8-thinking-标签提取)
9. [独有功能清单](#9-独有功能清单)
10. [Bug 与正确性问题汇总](#10-bug-与正确性问题汇总)
11. [综合评分](#11-综合评分)

---

## 1. 总体架构

| 维度 | model-proxy (Rust) | kiro-gateway (Python) |
|------|-------------------|----------------------|
| **语言/框架** | Rust + Axum + Tauri 2.0 桌面应用 | Python + FastAPI + uvicorn |
| **定位** | 多 Provider 代理 (OpenAI/Anthropic/Kiro)，桌面 GUI | 专用 Kiro 代理，纯 HTTP 服务 |
| **前端** | React 18 + Ant Design 5，完整 GUI 管理 | 无前端，纯 API |
| **部署形态** | 桌面应用 + CLI 双模式 | Docker / 直接运行 |
| **核心代码量** | Kiro 相关 ~21 个 Rust 源文件 | ~31 个 Python 模块 |
| **测试** | 单元测试 + 集成测试 + proptest | 完整测试套件 |
| **文档** | 6 份详细设计文档 + 协议逆向文档 | 多语言 README + docs 目录 |

**判定**: 架构层面 model-proxy 更成熟（GUI、双模式、模块化），但复杂度也更高。kiro-gateway 更轻量、易于部署。

---

## 2. 认证系统

### 2.1 Token 刷新逻辑

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 两种 Auth 路径 | Social OAuth + AWS SSO OIDC | Social OAuth + AWS SSO OIDC |
| 过期判断 | `is_expired()`: 5分钟内视为过期；`is_expiring_soon()`: 10分钟 | 单一阈值 `TOKEN_REFRESH_THRESHOLD`（可配置）；`is_expired()`: 无缓冲 |
| 安全缓冲 | is_expired 有 5 分钟缓冲 | `expires_in - 60` 秒（60秒缓冲） |
| force_refresh | 有，403 触发 | 有，403 触发 |
| User-Agent 版本 | `KiroIDE-0.11.107-{machine_id}`（可配置） | `KiroIDE-0.7.45-{fingerprint}`（硬编码，**版本过旧**） |

**判定**: 基本对等。Python 的 User-Agent 版本过旧，可能被服务端拒绝。

### 2.2 OAuth 流程

| 能力 | model-proxy | kiro-gateway |
|------|------------|--------------|
| OIDC Device Flow | 完整实现（注册/获取设备码/轮询） | **无** |
| Social OAuth + PKCE | Google + GitHub，完整 PKCE 流程 | **无** |
| IAM IdC PKCE Flow | 完整授权码流程 + state 验证 | **无** |
| SSO Token 导入 | 7步自动化流程 | **无** |

**判定**: **model-proxy 完胜**。Python 依赖外部工具获取凭证。

### 2.3 多账号管理

| 能力 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 负载均衡模式 | Priority / Balanced / Smart（复合评分） | 仅 Sticky + Failover |
| 熔断器 | 三态: Active/Broken/HalfOpen + 指数退避 | 指数退避（相同公式：60s 起步，2倍递增，最大1天） |
| 健康评分 | 0-100 分，失败 -20，成功 +10 | 仅失败计数 |
| 自愈机制 | 全部不可用时减半错误计数，强制 HalfOpen | **无** |
| 概率重试 | 10% 概率重试 Broken 账号 | 10% 概率（相同） |
| 指标追踪 | inflight_count, latency_ema, recent_requests | **无** |
| Per-Model 账号追踪 | **无** | 有，动态学习模型→账号映射 |
| 懒初始化 | 否（启动时全部初始化） | 是（首次使用时初始化） |
| 状态持久化 | account-state.json（原子写入） | state.json（原子写入） |

**判定**: model-proxy 负载均衡更智能（Smart 模式 + 健康评分 + 自愈）。Python 有 per-model 跟踪这一独有优势。

### 2.4 凭证存储安全

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 文件权限 | **0o600 文件 / 0o700 目录** | 默认 umask（**安全漏洞**） |
| SQLite 写入 | 通过 stdin 调用 sqlite3 CLI，避免参数泄露 | Python sqlite3 模块，参数化查询 |
| Token 验证写入前 | 长度限制 + 控制字符检测 + SQL 注入防护 | 无额外验证 |
| SQLITE_READONLY | **无** | 有（生产安全） |
| SQLite Key 拼写 | `kirocli:oidc:token` | `kirocli:odic:token`（**与实际 kiro-cli 不一致，可能是 Bug**） |

**判定**: model-proxy 安全性更高（文件权限 + token 验证）。Python 的 `odic` 拼写问题需要确认实际 kiro-cli 的 key 名。

---

## 3. EventStream 二进制协议解析

**这是两个项目差异最大的模块。**

| 能力 | model-proxy | kiro-gateway |
|------|------------|--------------|
| **本质** | 真正的 AWS EventStream 二进制协议解析器 | 文本级 JSON 模式匹配器（名不副实） |
| 二进制帧解析 | 完整 prelude + headers + payload | **无**（`errors='ignore'` 解码 UTF-8 丢弃二进制数据） |
| CRC32 校验 | Prelude CRC + Message CRC（ISO-HDLC） | **无** |
| Header 类型支持 | 全部 10 种类型（Bool/Byte/Short/Int/Long/ByteArray/String/Timestamp/UUID） | **无** |
| 状态机 | 4态: Ready/Parsing/Recovering/Stopped | **无** |
| 错误恢复 | 字节跳过 + 帧跳过 + 5次连续错误熔断 | 跳过畸形 JSON |
| 部分帧处理 | 缓冲等待完整帧 | 隐式（JSON 不完整则等待） |
| 消息大小限制 | 16MB 上限 | **无** |
| 内容去重 | 无 | 有（跟踪 last_content） |
| Tool Call 组装 | 无（传递原始帧） | 有（增量组装 tool_start→input→stop） |
| 截断诊断 | 无（在其他模块处理） | 有（启发式括号/花括号平衡检测） |

**判定**: **model-proxy 完胜**。Rust 实现了真正的二进制协议解析器，有 CRC 校验和完整的错误恢复。Python 的解析器本质上是"碰巧能工作"——依赖二进制帧在 `errors='ignore'` 后不影响 JSON 提取，这在数据损坏时无法检测。

---

## 4. 请求转换 (Anthropic → Kiro)

### 4.1 消息处理

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 角色映射 | 未知 → user | 未知 → user（一致） |
| 消息分割 | 找到最后 user 消息，丢弃其后的 assistant（prefill 支持） | 最后一条即为 current，保留全部消息 |
| 连续 user-user | 插入 `"OK"` / `"understood"` 合成 assistant | 插入 `"(empty placeholder)"` 合成 assistant |
| 连续 assistant-assistant | 插入 `"Continue"` 合成 user | 插入 `"(empty placeholder)"` 合成 user |
| 保证最后一条是 user | **是**（sanitize pass 4） | **否** |
| Thinking 内容块 | 包裹为 `<thinking>...</thinking>` 标签 | **丢弃**（不处理 history 中的 thinking 块） |

**判定**: model-proxy 历史处理更完整（prefill、连续 assistant 修复、thinking 保留）。Python 丢弃 history 中的 thinking 内容是数据丢失。

### 4.2 系统提示注入

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 注入方式 | 独立 user/assistant 历史对（"I will follow these instructions."） | 拼接到第一条 user 消息的 content 前面 |
| 额外系统提示 | Chunked policy + 截断恢复 + Agent 提示 + UTC 时间戳 | Thinking 模式合法化 + 截断恢复 |

**判定**: 注入方式不同，各有优劣。model-proxy 更干净（不污染用户内容），但多占 2 条历史。Python 更紧凑但混入用户消息。

### 4.3 工具定义转换

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 工具名超长（>63字符） | **静默缩短**（prefix + _sha256_8） | **拒绝请求**（ValueError） |
| Schema 规范化 | 移除空 required/additionalProperties，强制 type:"object" | 移除空 required/additionalProperties，处理 anyOf/oneOf |
| 长描述处理 | >10000 字符移至系统提示 | 可配置阈值移至系统提示 |
| 空描述处理 | 使用空字符串（**可能导致 Kiro 拒绝**） | 替换为 `"Tool: {name}"` |

**判定**: Python 空描述处理更好。Rust 工具名缩短更实用（不中断请求）。

### 4.4 图片处理

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| Anthropic image (base64) | 支持（jpeg/png/gif/webp） | 支持 |
| OpenAI image_url (data URL) | 支持 | 支持 |
| URL 类型图片 | 跳过 + 警告 | 支持（提取 URL） |
| **工具结果中的图片** | **丢弃**（仅处理 text） | **支持**（提取到 user 消息图片中） |

**判定**: **Python 在工具结果图片处理上更完整**（浏览器/MCP 工具截图场景）。Rust 丢失这些图片。

### 4.5 缓存控制

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| cache_control → cachePoint | 完整转换 | **无** |
| 缓存 token 模拟 | SHA256 指纹 + cache_creation/cache_read 计算 | **无** |

**判定**: **model-proxy 完胜**。Python 完全忽略 prompt caching。

### 4.6 模型 ID 映射

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| GPT → Claude 映射 | 有（gpt-4o → claude-sonnet-4.5） | **无** |
| Claude 3.x → 4.x | 有（claude-3-5-sonnet → claude-sonnet-4.5） | 保留 3.x 格式 |
| 上下文窗口后缀 | 不处理 | 剥离 `[1m]`、`[200k]` |
| 反转格式 | 不处理 | 处理 `claude-4.5-opus-high` |
| 未知模型 | 回退到 `claude-sonnet-4.5` | 透传（可能被 Kiro 拒绝） |
| 动态模型列表 | 静态列表 | `/ListAvailableModels` API 动态获取 |

**判定**: 各有优势。Rust 的 fallback 更安全，Python 的动态模型发现更灵活。

### 4.7 请求体构造

| 字段 | model-proxy | kiro-gateway |
|------|------------|--------------|
| agentContinuationId | 生成 UUID v4 | **未设置** |
| agentTaskType | `"vibe"` | **未设置** |
| profileArn | 未设置 | 设置 |
| Thinking 配置 | 支持 Anthropic thinking + OpenAI reasoning_effort | 仅 Anthropic thinking |
| Thinking 预算上限 | **无** | `FAKE_REASONING_BUDGET_CAP` 可配置上限 |

---

## 5. 流式响应处理 (Kiro → Anthropic/OpenAI SSE)

### 5.1 事件类型

| 事件 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 文本内容 | `AssistantResponse`（含前缀去重） | `content` |
| 推理内容 | `ReasoningContent`（原生 EventStream 事件） | `thinking`（来自 ThinkingParser） |
| 工具调用 | `ToolUse`（JSON 累积） | `tool_use`（增量组装） |
| 上下文用量 | `ContextUsage`（百分比→token 换算） | `context_usage` |
| 计量 | `Metering`（仅日志） | `usage`（缓存字段提取） |
| 错误 | `Error`（日志 + 警告） | **未显式处理** |
| 异常 | `Exception`（`ContentLengthExceededException` → `max_tokens`） | **未显式处理** |

**判定**: model-proxy 错误/异常事件处理更完整。Python 缺少流式错误检测。

### 5.2 块生命周期管理

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 管理方式 | 泛化状态机 `AnthropicStreamState` | 按类型的布尔标记 |
| 索引分配 | `alloc_block_index()` 单调递增 | `current_block_index++` |
| 块类型转换 | `stop_current_block()` + `close_open_tool_blocks()` | 检查 `text_block_started`/`thinking_block_started` 逐个关闭 |
| 新增块类型支持 | 只需扩展状态机 | 需添加新布尔标记（**易遗漏**） |

**判定**: model-proxy 状态机设计更健壮、可扩展。

---

## 6. 请求分发与错误恢复

### 6.1 端点管理

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 端点数量 | **3 个**（kiro / codewhisperer / amazonq） | **1 个**（runtime.{region}.kiro.dev） |
| 域名 | q.{region}.amazonaws.com, codewhisperer.{region}.amazonaws.com | runtime.{region}.kiro.dev |
| 端点回退 | 429 / 传输错误时尝试下一端点 | **无**（仅账号级 failover） |
| 端点偏好 | Auto/Kiro/Codewhisperer/AmazonQ 可配置 | **无** |
| x-amz-target | 仅 codewhisperer/amazonq 端点发送 | 始终发送 |

**判定**: **model-proxy 完胜**。多端点回退是重要的生产可靠性保障。

### 6.2 重试逻辑

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 最大重试 | 3 次（4 总尝试） | 3 次（4 总尝试） |
| 退避策略 | 1s × 2^attempt | 1s × 2^attempt（一致） |
| 403 重试 | **仅第 0 次**尝试时 force_refresh | **任何尝试**都 force_refresh（更健壮） |
| 429/5xx | 指数退避重试 | 指数退避重试 |
| 网络错误 | 所有 reqwest Error 统一重试 | 分类处理（DNS/SSL/连接拒绝等，SSL 不重试） |
| **首 Token 超时** | **无** | **有**（15 秒超时，最多 3 次新请求） |
| amz-sdk-request | 硬编码 `attempt=1`（**Bug**） | 硬编码 `attempt=1`（**Bug**） |

**判定**: Python 的 403 重试策略更健壮，首 Token 超时是独有优势。两者都有 `attempt=1` 硬编码 Bug。

### 6.3 HTTP 客户端

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 连接池 | 32 空闲/主机 | 20 keepalive / 100 总连接 |
| 双客户端策略 | **无**（共享单一客户端） | **有**（流式请求每次新建客户端，避免 CLOSE_WAIT 泄漏） |
| TCP 优化 | tcp_nodelay + tcp_keepalive(60s) | 依赖平台默认 |
| Connection: close | **未设置**（潜在 CLOSE_WAIT 风险） | 流式请求显式设置 |

**判定**: Python 的双客户端策略是生产经验的产物，更稳健。Rust 连接池配置更慷慨。

---

## 7. 会话历史与截断处理

### 7.1 截断策略

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 策略数量 | **4 种**（AutoTruncate/ErrorRetry/PreEstimate/None） | **1 种**（字节大小裁剪） |
| 渐进重试 | 50% → 25% → 0% 三阶递减 | **无** |
| 智能摘要 | 有（截断消息替换为 200 字摘要） | **无** |
| 裁剪粒度 | 单条消息 JSON 字符数累计 | 整体 payload 字节数 |

**判定**: **model-proxy 完胜**。智能摘要和渐进重试是重要差异化能力。

### 7.2 截断恢复

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 工具截断恢复 | 按 tool_use_id 索引 | 按 tool_call_id 索引 |
| 内容截断恢复 | 列表存储，无去重 | SHA256 哈希去重（**潜在 Bug**: 相同前 500 字符的内容会冲突） |
| 恢复消息差异 | 按 TruncationReason 分类（MissingUsage/ContentLengthExceeded/TruncatedToolCall） | 单一通用消息 |
| 检索语义 | `pop_all_truncations()` 原子全量 | 逐 key 检索 |

### 7.3 孤儿修复

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| 孤儿 tool_use | 注入合成错误 tool_result（保留结构化格式） | 转换为文本描述（丢失结构） |
| 孤儿 tool_result | 转换为文本描述 + 清理 | 转换为文本 + 内联 |

**判定**: model-proxy 孤儿 tool_use 修复更完整（保留 Kiro tool_result 结构）。

---

## 8. Thinking 标签提取

| 维度 | model-proxy | kiro-gateway |
|------|------------|--------------|
| FSM 状态 | PreContent / InThinking / Streaming | PRE_CONTENT / IN_THINKING / STREAMING（一致） |
| 原生推理事件 | **有**（`ReasoningContent` EventStream 事件直接映射） | **无**（仅依赖标签解析） |
| Thinking 模式 | 4 种：as_reasoning_content / remove / pass / strip_tags | 可配置 |
| 缓冲区大小 | 硬编码 20/30 | 可配置（`FAKE_REASONING_INITIAL_BUFFER_SIZE`） |
| 标签检测 | `<thinking>` 等 | 更多标签变体（可配置 `open_tags` 列表） |

**判定**: model-proxy 有原生推理事件支持（更可靠）。Python 的标签检测更灵活可配。

---

## 9. 独有功能清单

### 仅 model-proxy 有

| 功能 | 重要程度 |
|------|---------|
| OAuth 全流程（Device / Social+PKCE / IdC PKCE / SSO 导入） | 高 |
| 多端点域名回退（kiro/codewhisperer/amazonq） | 高 |
| CRC32 校验 + 完整二进制协议解析 | 高 |
| Prompt Cache 支持（cache_control → cachePoint + token 模拟） | 高 |
| 智能截断摘要（截断消息替换为摘要） | 中 |
| 3 种负载均衡模式（Priority/Balanced/Smart） | 中 |
| 健康评分 + 自愈机制 | 中 |
| GPT → Claude 模型映射 | 中 |
| GUI 管理界面（Tauri 桌面应用） | 中 |
| 端点健康追踪（EMA 延迟 / 连续错误） | 中 |
| OpenAI reasoning_effort 支持 | 低 |
| 文件权限强制（0600/0700） | 低（安全） |
| agentContinuationId / agentTaskType 字段 | 低 |
| Prometheus 指标 | 低 |
| IP 黑名单 / 站点维护模式 | 低 |
| DNS 缓存 | 低 |

### 仅 kiro-gateway 有

| 功能 | 重要程度 |
|------|---------|
| **首 Token 超时重试**（15秒超时，3次重试） | 高 |
| **流式请求独立 HTTP 客户端**（避免 CLOSE_WAIT 泄漏） | 高 |
| **Per-Model 账号追踪 + 动态学习** | 中 |
| 工具结果中的图片提取（浏览器/MCP 截图） | 中 |
| Content hash 去重截断恢复 | 中 |
| SQLITE_READONLY 生产安全标志 | 中 |
| 动态模型列表（/ListAvailableModels API） | 中 |
| 用户友好的错误增强（reason code → 可读消息） | 低 |
| Thinking 预算上限配置（FAKE_REASONING_BUDGET_CAP） | 低 |
| 懒初始化账户 | 低 |
| 多级 API Region 自动检测 | 低 |
| Connection: close 流式头 | 低 |

---

## 10. Bug 与正确性问题汇总

### 共同 Bug

| Bug | 影响 |
|-----|------|
| `amz-sdk-request: attempt=1; max=3` 硬编码 | AWS SDK 规范要求 attempt 反映实际尝试次数 |
| 两者都没有 HTTP 级 gzip 压缩 | 可能影响大 payload 性能 |

### model-proxy 专有 Bug

| Bug | 严重度 |
|-----|--------|
| 工具结果中的图片被丢弃 | **中** — MCP/浏览器工具截图丢失 |
| 403 仅在第 0 次重试，后续直接 Fatal | **中** — 后续 force_refresh 可能成功 |
| 5xx 不尝试下一端点 | **中** — 一个端点故障不影响另一端点 |
| 空工具描述不处理 | **低** — 可能导致 Kiro 拒绝 |
| UUID v4 不符合 RFC 4122（variant bits 未设置） | **低** — 大概率不影响功能 |
| EndpointHealthTracker 数据未用于路由决策 | **低** — 有数据但没使用 |
| compression.rs 模块注释错误 | **低** |

### kiro-gateway 专有 Bug

| Bug | 严重度 |
|-----|--------|
| **EventStream 解析器不是真正的二进制解析**（依赖 `errors='ignore'`） | **高** — 数据损坏无法检测 |
| **文件权限未限制**（凭证可能 world-readable） | **高** — 安全漏洞 |
| **SQLite key 拼写 `odic` vs `oidc`** | **高** — 可能无法读取 kiro-cli 凭证 |
| **User-Agent 版本 0.7.45 过旧** | **中** — 可能被服务端拒绝 |
| **Content hash 截断恢复前 500 字符冲突** | **中** — 相同前缀内容截断恢复可能丢失 |
| 不处理 consecutive assistant 消息 | **中** — 可能导致 Kiro 拒绝 |
| 不保证最后一条消息是 user | **中** — 同上 |
| 不处理 history 中的 thinking 内容块 | **中** — 数据丢失 |
| **不处理流式 Error/Exception 事件** | **中** — 上下文溢出检测缺失 |
| 死代码（converters_anthropic.py:204） | **低** |
| 传递未知模型名不做 fallback | **低** — 拼写错误会导致请求失败 |

---

## 11. 综合评分

| 模块 | model-proxy | kiro-gateway | 胜者 |
|------|------------|--------------|------|
| 认证系统 | 9/10 | 5/10 | **model-proxy** |
| EventStream 解析 | 10/10 | 3/10 | **model-proxy** |
| 请求转换 | 8/10 | 7/10 | model-proxy |
| 流式响应 | 8/10 | 7/10 | model-proxy |
| 分发与恢复 | 7/10 | 7/10 | 平手 |
| 历史与截断 | 9/10 | 6/10 | **model-proxy** |
| Thinking 处理 | 8/10 | 7/10 | model-proxy |
| 运维与部署 | 6/10 | 8/10 | **kiro-gateway** |
| 安全性 | 9/10 | 5/10 | **model-proxy** |
| **总分** | **82/100** | **55/100** | **model-proxy** |

### 总结

**model-proxy 在核心代理能力上全面领先**，主要优势在于：
1. 正确实现了 AWS EventStream 二进制协议（CRC 校验 + 完整 header 解析）
2. 完整的 OAuth 流程支持（用户可直接登录，不依赖外部工具）
3. 多端点回退 + 智能负载均衡
4. Prompt cache 支持
5. 更强的安全性（文件权限 + token 验证）

**kiro-gateway 的可借鉴之处**：
1. 首 Token 超时重试（应对上游挂起连接）
2. 流式请求独立 HTTP 客户端（避免 CLOSE_WAIT 泄漏）
3. Per-Model 账号追踪（更精细的负载均衡）
4. 工具结果中的图片提取（MCP 浏览器工具兼容）
5. 用户友好的错误增强消息
6. 动态模型列表获取

建议 model-proxy 优先引入 kiro-gateway 的**首 Token 超时重试**和**流式独立客户端**策略，这两个是生产环境中实际会遇到的问题。
