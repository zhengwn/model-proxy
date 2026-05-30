# Anthropic Messages API ↔ OpenAI Chat Completions API 协议对照表

> 基于 Anthropic `anthropic-version: 2023-06-01` 和 OpenAI Chat Completions API (GPT-5.5 era) 的最新文档整理。
> 最后更新: 2026-05-30

---

## 1. 请求体参数对照

### 1.1 基础参数

| 功能 | Anthropic | OpenAI | 可转换 | 备注 |
|------|-----------|--------|:------:|------|
| 模型 | `model: string` | `model: string` | ✅ | 通过 model route 映射 |
| 最大 token | `max_tokens: number` (必填) | `max_tokens: int` 或 `max_completion_tokens: int` | ✅ | `max_completion_tokens` 优先；Anthropic 必填，OpenAI 可选 |
| 消息 | `messages: MessageParam[]` | `messages: ChatCompletionMessageParam[]` | ✅ | 格式不同，见 §2 |
| 系统提示 | `system: string \| TextBlockParam[]` | `messages` 中 `role: "system"` 或 `role: "developer"` | ✅ | Anthropic 是顶级字段，OpenAI 是消息角色 |
| 温度 | `temperature: 0.0-1.0` | `temperature: 0.0-2.0` | ⚠️ | OpenAI 上限更高 |
| top_p | `top_p: number` | `top_p: 0-1` | ✅ | |
| top_k | `top_k: number` | ❌ 无 | ❌ | Anthropic 独有，OpenAI 丢弃 |
| 停止序列 | `stop_sequences: string[]` | `stop: string \| string[]` | ✅ | |
| 流式 | `stream: bool` | `stream: bool` | ✅ | |
| 工具 | `tools: ToolUnion[]` | `tools: ChatCompletionTool[]` | ✅ | 格式不同，见 §3 |
| 工具选择 | `tool_choice: ToolChoice` | `tool_choice: string \| object` | ✅ | 见 §4 |

### 1.2 Anthropic 独有参数

| 参数 | 说明 | OpenAI 对应 |
|------|------|-------------|
| `thinking: ThinkingConfigParam` | 扩展思考配置 | `reasoning_effort` (有损：budget_tokens → "low"/"medium"/"high") |
| `output_config: OutputConfig` | 结构化输出 + effort 级别 | `response_format` (effort 部分丢弃) |
| `cache_control` | 顶级缓存控制 | ❌ 无对应 |
| `metadata.user_id` | 用户标识 | `user: string` |
| `service_tier` | `"auto"` / `"standard_only"` | `service_tier` (枚举值不同) |
| `container` | 代码执行容器 ID | ❌ 无对应 |
| `inference_geo` | 地理路由 | ❌ 无对应 |

### 1.3 OpenAI 独有参数

| 参数 | 说明 | Anthropic 对应 |
|------|------|----------------|
| `n` | 生成多个选项 | ❌ 无对应（Anthropic 总是 1） |
| `logprobs` / `top_logprobs` | token 级概率 | ❌ 无对应 |
| `frequency_penalty` | 频率惩罚 | ❌ 无对应 |
| `presence_penalty` | 存在惩罚 | ❌ 无对应 |
| `seed` | 可复现性种子 | ❌ 无对应 |
| `logit_bias` | token 偏置 | ❌ 无对应 |
| `store` | 存储输出 | ❌ 无对应 |
| `audio` / `modalities` | 音频输入输出 | ❌ 无对应 |
| `prediction` | 预测输出（加速编辑） | ❌ 无对应 |
| `parallel_tool_calls` | 并行工具调用 | ❌ Anthropic 默认支持并行 |
| `response_format` | 输出格式控制 | `output_config.format` |
| `reasoning_effort` | `"low"/"medium"/"high"` | `thinking.budget_tokens` (有损) |
| `stream_options` | 流式选项（含 usage） | ❌ Anthropic 流式自动含 usage |
| `metadata` | 键值对元数据 | `metadata.user_id`（更受限） |
| `functions` / `function_call` | 已废弃 | ❌ 不转换 |

---

## 2. 消息格式对照

### 2.1 角色映射

| OpenAI 角色 | Anthropic 角色 | 可转换 | 说明 |
|-------------|---------------|:------:|------|
| `"system"` | 顶级 `system` 字段 | ✅ | 提取到 Anthropic 顶级字段 |
| `"developer"` | 顶级 `system` 字段 | ✅ | 合并到 system，语义最接近 |
| `"user"` | `"user"` | ✅ | |
| `"assistant"` | `"assistant"` | ✅ | |
| `"tool"` | `user` 消息内的 `tool_result` 块 | ✅ | 角色不同，语义等价 |

### 2.2 用户消息内容块

| OpenAI `type` | Anthropic `type` | 可转换 | 备注 |
|---------------|-----------------|:------:|------|
| `"text"` | `"text"` | ✅ | |
| `"image_url"` (data URI) | `"image"` (base64 source) | ✅ | 解析 data URI 拆分 media_type 和 data |
| `"image_url"` (HTTP URL) | `"image"` (url source) | ✅ | |
| `"input_audio"` | ❌ | ❌ | Anthropic 不支持音频输入 |

### 2.3 助手消息内容

| OpenAI 字段 | Anthropic 字段 | 可转换 | 备注 |
|-------------|---------------|:------:|------|
| `content: string` | `content: [{type:"text", text}]` | ✅ | |
| `reasoning_content: string` | `content: [{type:"thinking", thinking, signature:""}]` | ⚠️ | **signature 丢失**，多轮对话回传时会被 Anthropic 拒绝 |
| `tool_calls: [...]` | `content: [{type:"tool_use", ...}]` | ✅ | ID 需转换 `call_*` ↔ `toolu_*` |
| `refusal: string` | ❌ | ❌ | 无直接对应，可拼接到 content |
| `audio: {id, data, ...}` | ❌ | ❌ | Anthropic 不支持音频输出 |

### 2.4 工具结果消息

| OpenAI 格式 | Anthropic 格式 | 可转换 | 备注 |
|-------------|---------------|:------:|------|
| `role:"tool", tool_call_id, content` | `role:"user", content:[{type:"tool_result", tool_use_id, content}]` | ✅ | 多个连续 tool 消息合并为一个 user 消息 |
| ❌ | `tool_result.is_error: true` | ❌ | OpenAI 无此语义，错误信息只能放 content |
| ❌ | `tool_result.cache_control` | ❌ | OpenAI 无工具结果缓存 |

### 2.5 Anthropic 特有内容块（OpenAI 无对应）

| 类型 | 说明 |
|------|------|
| `thinking` | 扩展思考块（带 signature） |
| `redacted_thinking` | 被编辑的思考内容 |
| `document` | 文档块（PDF、纯文本、URL PDF） |
| `search_result` | 搜索结果块 |
| `server_tool_use` | 服务端工具调用（web_search 等） |
| `web_search_tool_result` | web_search 返回结果 |
| `web_fetch_tool_result` | web_fetch 返回结果 |
| `code_execution_tool_result` | 代码执行结果 |
| `mid_conv_system` | 对话中间的系统指令 |
| `container_upload` | 容器文件上传 |
| `tool_reference` | 工具引用（tool search 使用） |

---

## 3. 工具定义对照

| 字段 | Anthropic | OpenAI | 可转换 |
|------|-----------|--------|:------:|
| 工具名 | `name: string`（无字符限制） | `function.name: string`（`^[a-zA-Z0-9_-]+$`，最长 64） | ⚠️ 名称可能需 sanitize |
| 描述 | `description: string` | `function.description: string`（最长 1024） | ✅ |
| 参数 | `input_schema: JSON Schema` | `function.parameters: JSON Schema` | ✅ |
| 严格模式 | `strict: bool` | `function.strict: bool` | ✅ |
| 缓存 | `cache_control` | ❌ | ❌ |
| 延迟加载 | `defer_loading: bool` | ❌ | ❌ |
| 调用者限制 | `allowed_callers: [...]` | ❌ | ❌ |
| 自定义工具 | ❌ | `type: "custom"` | ❌ |
| 内置工具 | bash/code_exec/web_search 等 | ❌ | ❌ |

---

## 4. 工具选择对照

| Anthropic `tool_choice` | OpenAI `tool_choice` | 可转换 |
|------------------------|---------------------|:------:|
| `{type: "auto"}` | `"auto"` | ✅ |
| `{type: "auto", disable_parallel_tool_use: true}` | `"auto"` | ⚠️ 并行控制丢失 |
| `{type: "any"}` | `"required"` | ✅ |
| `{type: "any", disable_parallel_tool_use: true}` | `"required"` | ⚠️ 并行控制丢失 |
| `{type: "tool", name: "..."}` | `{type:"function", function:{name:"..."}}` | ✅ |
| `{type: "none"}` | `"none"` | ✅ |
| ❌ | `{type:"allowed_tools", ...}` | ❌ Anthropic 无此模式 |

---

## 5. 响应体对照

### 5.1 顶层结构

| 字段 | Anthropic | OpenAI | 说明 |
|------|-----------|--------|------|
| ID | `id: "msg_..."` | `id: "chatcmpl-..."` | 前缀不同 |
| 类型 | `type: "message"` | `object: "chat.completion"` | |
| 角色 | `role: "assistant"` | `choices[0].message.role: "assistant"` | OpenAI 嵌套在 choices 里 |
| 模型 | `model: string` | `model: string` | |
| 内容 | `content: ContentBlock[]` | `choices[0].message.content: string \| null` | 结构完全不同 |
| 停止原因 | `stop_reason: string` | `choices[0].finish_reason: string` | 枚举值不同 |
| 停止序列 | `stop_sequence: string \| null` | ❌ | Anthropic 独有 |
| 停止详情 | `stop_details: RefusalStopDetails` | ❌ | Anthropic 独有 |
| 容器 | `container: {id, expires_at}` | ❌ | Anthropic 独有 |
| Usage | `usage: Usage` | `usage: CompletionUsage` | 字段名不同 |

### 5.2 停止原因映射

| Anthropic `stop_reason` | OpenAI `finish_reason` | 可转换 |
|------------------------|----------------------|:------:|
| `"end_turn"` | `"stop"` | ✅ |
| `"max_tokens"` | `"length"` | ✅ |
| `"tool_use"` | `"tool_calls"` | ✅ |
| `"stop_sequence"` | `"stop"` | ✅ |
| `"pause_turn"` | `"stop"` | ⚠️ 语义丢失 |
| `"refusal"` | `"content_filter"` | ✅ |
| ❌ | `"function_call"` (废弃) | — |

### 5.3 响应内容块映射

| Anthropic 响应块 | OpenAI message 字段 | 可转换 | 备注 |
|-----------------|--------------------|:------:|------|
| `{type:"text", text}` | `content: "..."` | ✅ | 多个 text 块拼接 |
| `{type:"thinking", thinking, signature}` | `reasoning_content: "..."` | ⚠️ | **signature 丢失** |
| `{type:"redacted_thinking", data}` | ❌ | ❌ | OpenAI 无此概念 |
| `{type:"tool_use", id, name, input}` | `tool_calls: [{id, type:"function", function:{name, arguments}}]` | ✅ | ID 转换，input → JSON string |
| `{type:"server_tool_use", ...}` | ❌ | ❌ | OpenAI 无服务端工具 |
| `{type:"web_search_tool_result", ...}` | ❌ | ❌ | |
| `{type:"code_execution_tool_result", ...}` | ❌ | ❌ | |

---

## 6. Usage 统计对照

| Anthropic 字段 | OpenAI 字段 | 可转换 | 备注 |
|---------------|-------------|:------:|------|
| `input_tokens` | `prompt_tokens` | ✅ | |
| `output_tokens` | `completion_tokens` | ✅ | |
| ❌ | `total_tokens` | — | 需计算 `prompt + completion` |
| `cache_creation_input_tokens` | `prompt_tokens_details.cache_creation_input_tokens` | ✅ | |
| `cache_read_input_tokens` | `prompt_tokens_details.cached_tokens` | ✅ | |
| `output_tokens_details.thinking_tokens` | `completion_tokens_details.reasoning_tokens` | ✅ | |
| `server_tool_use.web_search_requests` | ❌ | ❌ | |
| `server_tool_use.web_fetch_requests` | ❌ | ❌ | |
| `service_tier` | `service_tier` | ⚠️ | 枚举值不同 |
| `inference_geo` | ❌ | ❌ | |
| `cache_creation.ephemeral_5m_input_tokens` | ❌ | ❌ | |
| `cache_creation.ephemeral_1h_input_tokens` | ❌ | ❌ | |
| ❌ | `prompt_tokens_details.audio_tokens` | — | Anthropic 无音频 |
| ❌ | `completion_tokens_details.audio_tokens` | — | |
| ❌ | `completion_tokens_details.accepted_prediction_tokens` | — | |

---

## 7. 流式事件对照

### 7.1 格式差异

| | Anthropic SSE | OpenAI SSE |
|-|---------------|------------|
| 格式 | `event: <type>\ndata: <json>\n\n` | `data: <json>\n\n` |
| 事件类型 | 通过 `event:` 行标识 | 通过 JSON 内 `choices[0].delta` 推断 |
| 结束标记 | `message_stop` 事件 | `data: [DONE]` |
| Ping | `event: ping` | ❌ |

### 7.2 事件流结构

**Anthropic:**
```
message_start → [content_block_start → content_block_delta* → content_block_stop]* → message_delta* → message_stop
```

**OpenAI:**
```
chunk(role) → chunk(content/tool_calls)* → chunk(finish_reason) → [chunk(usage)] → [DONE]
```

### 7.3 流式事件映射

| Anthropic 事件 | OpenAI chunk delta | 可转换 | 备注 |
|---------------|-------------------|:------:|------|
| `message_start` | `delta: {role: "assistant"}` | ✅ | 同时捕获 input_tokens |
| `content_block_start` (text) | 无输出（等待 delta） | ✅ | |
| `content_block_start` (thinking) | 无输出 | ✅ | |
| `content_block_start` (tool_use, id, name) | `delta: {tool_calls: [{index, id:"call_...", function:{name, arguments:""}}]}` | ✅ | ID 转换 |
| `content_block_delta` (text_delta) | `delta: {content: "..."}` | ✅ | |
| `content_block_delta` (thinking_delta) | `delta: {reasoning_content: "..."}` | ✅ | |
| `content_block_delta` (input_json_delta) | `delta: {tool_calls: [{index, function:{arguments: "..."}}]}` | ✅ | |
| `content_block_delta` (signature_delta) | ❌ | ❌ | **签名丢失** |
| `content_block_stop` | 无直接对应 | ✅ | |
| `message_delta` (stop_reason) | `delta: {}, finish_reason: "stop"` | ✅ | |
| `message_delta` (usage) | `usage: {...}` (在最终 chunk) | ✅ | |
| `message_stop` | `data: [DONE]` | ✅ | |
| `ping` | ❌ | — | 忽略 |

### 7.4 流式 Usage

| 场景 | Anthropic | OpenAI |
|------|-----------|--------|
| 始终包含 usage？ | ✅ `message_start` 含 input_tokens，`message_delta` 含 output_tokens | ❌ 仅当 `stream_options.include_usage: true` |
| Usage 累积性 | `message_delta.usage` 是**累积值** | 最终 chunk 的 usage 是**最终值** |

---

## 8. 认证对照

| | Anthropic | OpenAI |
|-|-----------|--------|
| Header | `x-api-key: <key>` + `anthropic-version: 2023-06-01` | `Authorization: Bearer <key>` |
| Endpoint | `POST /v1/messages` | `POST /v1/chat/completions` |

---

## 9. 转换限制汇总

### 9.1 有损转换（信息丢失）

| 丢失项 | 方向 | 严重程度 | 说明 |
|--------|------|:--------:|------|
| thinking `signature` | Anthropic→OpenAI | 🔴 高 | 多轮对话回传 Anthropic 时 signature 为空，thinking 上下文丢失 |
| `tool_result.is_error` | Anthropic→OpenAI | 🟡 中 | 工具错误语义丢失，错误文本仍可通过 content 传递 |
| `top_k` | Anthropic→OpenAI | 🟢 低 | 高级参数，使用率低 |
| `cache_control` (工具/消息级) | Anthropic→OpenAI | 🟢 低 | Anthropic 特有缓存控制 |
| `stop_details` (refusal 详情) | Anthropic→OpenAI | 🟡 中 | 拒绝原因丢失 |
| `pause_turn` 语义 | Anthropic→OpenAI | 🟡 中 | 映射为 "stop"，客户端不知道需要续传 |
| `logprobs` | OpenAI→Anthropic | 🟡 中 | Anthropic 不支持 token 级概率 |
| `n > 1` | OpenAI→Anthropic | 🟢 低 | Anthropic 固定返回 1 个选项 |
| `frequency_penalty` / `presence_penalty` | OpenAI→Anthropic | 🟢 低 | Anthropic 不支持 |
| `audio` / `modalities` | OpenAI→Anthropic | 🔴 高 | Anthropic 不支持音频 |
| `prediction` | OpenAI→Anthropic | 🟢 低 | OpenAI 特有功能 |
| `reasoning_effort` 细粒度 | OpenAI→Anthropic | 🟡 中 | "low"/"medium"/"high" → budget_tokens 是有损映射 |
| `signature_delta` | Anthropic 流式→OpenAI | 🔴 高 | 签名在流式转换中丢失 |

### 9.2 不可转换（无对应概念）

| 特性 | 所属方 | 说明 |
|------|--------|------|
| 内置服务端工具 (web_search, code_exec 等) | Anthropic | OpenAI 无此概念 |
| `server_tool_use` / `web_search_tool_result` 等响应块 | Anthropic | 无 OpenAI 对应 |
| `mid_conv_system` 内容块 | Anthropic | 对话中间系统指令 |
| `redacted_thinking` | Anthropic | 被编辑的思考内容 |
| `document` 内容块（PDF 等） | Anthropic | OpenAI 仅支持 image_url |
| `container` / `inference_geo` | Anthropic | 基础设施级特性 |
| `logprobs` / `top_logprobs` | OpenAI | Anthropic 无 token 概率 |
| `seed` | OpenAI | Anthropic 无复现种子 |
| `logit_bias` | OpenAI | Anthropic 无 token 偏置 |
| `audio` 输入输出 | OpenAI | Anthropic 不支持音频 |
| `prediction` (预测输出) | OpenAI | Anthropic 无此概念 |
| `custom` 工具类型 | OpenAI | Anthropic 工具类型不同 |
| `allowed_tools` tool_choice | OpenAI | Anthropic 无此模式 |

### 9.3 当前代理已处理的转换

| 转换 | 实现位置 | 状态 |
|------|---------|:----:|
| Anthropic 请求 → OpenAI 请求 | `convert.rs: anthropic_to_openai()` | ✅ |
| OpenAI 请求 → Anthropic 请求 | `convert.rs: openai_to_anthropic()` | ✅ |
| OpenAI 非流式响应 → Anthropic 响应 | `response.rs: convert_non_stream_response()` | ✅ |
| Anthropic 非流式响应 → OpenAI 响应 | `response.rs: convert_anthropic_to_openai_response()` | ✅ |
| OpenAI 流式 → Anthropic 流式 | `stream.rs: convert_stream_chunk()` + 状态机 | ✅ |
| Anthropic 流式 → OpenAI 流式 | `stream.rs: convert_anthropic_stream_chunk()` + 状态机 | ✅ |
| thinking signature 处理 | 转 Anthropic 时填空字符串 | ⚠️ 多轮有损 |
| 工具名 sanitize | `convert.rs: sanitize_tool_name()` | ✅ |
| tool_result.is_error | ❌ 未处理 | 缺失 |
| stop_reason: refusal | 已映射到 content_filter | ✅ |
| stop_reason: pause_turn | 已映射到 stop | ✅ |
