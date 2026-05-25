# 架构设计

本文档描述 Model Proxy 的整体架构、核心设计决策和模块间交互方式。

## 系统概览

Model Proxy 是一个 AI 模型请求代理，接收 Anthropic Messages API 格式的请求，根据配置将其转发到不同的上游 Provider（OpenAI、Anthropic、DeepSeek 等）。

```
┌─────────────────────────────────────────────────────────────┐
│                      Tauri 桌面应用                           │
│  ┌──────────┐  ┌──────────────┐  ┌───────────────────────┐  │
│  │ React UI │←→│ Tauri IPC    │←→│ commands.rs (AppState)│  │
│  │ (Ant Design)│  │ (invoke)     │  │ service.rs            │  │
│  └──────────┘  └──────────────┘  └───────────┬───────────┘  │
│                                               │ Arc<ArcSwap> │
│  ┌────────────────────────────────────────────┼───────────┐  │
│  │                proxy-core crate            │           │  │
│  │  ┌─────────┐  ┌──────────┐  ┌─────────────▼─────────┐│  │
│  │  │ axum    │→ │ handlers │→ │ AppState              ││  │
│  │  │ router  │  │          │  │  - active_provider    ││  │
│  │  └─────────┘  └────┬─────┘  │  - registry           ││  │
│  │                     │        │  - log_collector       ││  │
│  │              ┌──────┴──────┐ └───────────────────────┘│  │
│  │              │ convert.rs  │                           │  │
│  │              │ stream.rs   │                           │  │
│  │              │ response.rs │                           │  │
│  │              │passthrough.rs│                          │  │
│  │              └─────────────┘                           │  │
│  └────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│                      CLI binary (src/main.rs)                │
│  独立运行 proxy-core，无 GUI，适合服务器部署                    │
└─────────────────────────────────────────────────────────────┘
```

## Crate 结构

| Crate | 职责 |
|-------|------|
| `proxy-core` | 核心代理逻辑：HTTP 服务、格式转换、流式处理、日志 |
| `model-proxy-tauri` (src-tauri) | Tauri 桌面壳：IPC 命令、服务生命周期、系统托盘 |
| `model-proxy` (src/) | CLI 入口：独立运行代理服务，无 GUI |

`proxy-core` 是纯库 crate，不依赖 Tauri，可以被 CLI 和 Tauri 两个 binary 共同使用。

## 核心设计决策

### 1. ArcSwap 无锁热切换

**问题**：切换 Provider 时不能中断正在处理的请求，也不能用 RwLock 阻塞高并发读取。

**方案**：使用 `arc-swap` crate 的 `ArcSwap<ProviderConfig>` 存储当前活跃 Provider。

```rust
pub struct AppState {
    pub active_provider: Arc<ArcSwap<ProviderConfig>>,
    pub registry: Arc<ArcSwap<ProviderRegistry>>,
    // ...
}
```

- **读取**（每个请求）：`state.active_provider.load()` — 无锁，返回 `Guard<Arc<ProviderConfig>>`
- **写入**（切换时）：`state.active_provider.store(Arc::new(new_provider))` — 原子替换

优势：
- 读取路径零开销（无 mutex/rwlock 竞争）
- 切换是原子操作，不存在中间状态
- 进行中的请求持有旧 Arc 引用，不受影响

### 2. 共享状态模型（Tauri ↔ proxy-core）

Tauri 层和 proxy-core 通过 `Arc<ArcSwap<...>>` 共享同一份状态：

```rust
// Tauri commands.rs 中的 AppState
pub struct AppState {
    pub registry: Arc<ArcSwap<ProviderRegistry>>,
    pub active_provider: Arc<ArcSwap<ProviderConfig>>,
    // ...
}

// proxy-core 的 AppState 持有相同的 Arc
let proxy_state = ProxyCoreAppState::new_shared(
    config,
    app_state.active_provider.clone(),  // 同一个 Arc
    app_state.registry.clone(),         // 同一个 Arc
    log_collector,
);
```

这意味着 Tauri 层调用 `active_provider.store(...)` 后，proxy-core 的下一个请求立即看到新值，无需重启服务。

### 3. 请求格式转换策略

代理对外暴露 **Anthropic Messages API** 格式的端点 (`/v1/messages`)。内部根据目标 Provider 的 `format` 字段决定处理方式：

| Provider format | 处理方式 |
|----------------|----------|
| `openai` | Anthropic → OpenAI 格式转换（convert.rs），响应再转回 Anthropic 格式 |
| `anthropic` | 直接透传（passthrough.rs），不做格式转换 |

这个设计的原因是：大多数 AI IDE 工具（Cursor、Windsurf 等）使用 Anthropic 格式与模型交互，而很多 Provider（DeepSeek、OpenAI）只提供 OpenAI 格式的 API。

### 4. 流式响应状态机

`stream.rs` 中的 OpenAI → Anthropic 流式转换维护一个状态机：

```
                    ┌─────────────────┐
                    │   未开始         │
                    │ (started=false) │
                    └────────┬────────┘
                             │ 收到 role: assistant
                             ▼
                    ┌─────────────────┐
              ┌────→│   进行中         │←────┐
              │     │ (started=true)  │     │
              │     └──┬──────────┬───┘     │
              │        │          │         │
    reasoning_content  content   tool_calls │
              │        │          │         │
              ▼        ▼          ▼         │
         ┌────────┐ ┌──────┐ ┌────────┐    │
         │Thinking│ │ Text │ │ToolUse │    │
         │ Block  │ │Block │ │ Block  │────┘
         └────────┘ └──────┘ └────────┘  (可切换)
                             │
                    finish_reason != null
                             │
                             ▼
                    ┌─────────────────┐
                    │   已结束         │
                    │ (ended=true)    │
                    └─────────────────┘
```

关键规则：
- 同一时刻只能有一个 text/thinking block 处于打开状态
- 切换 block 类型时自动发送 `content_block_stop` + `content_block_start`
- tool_use blocks 可以与 text block 并存，独立管理生命周期
- `[DONE]` 信号触发最终的 `message_delta`（含 usage）和 `message_stop`

### 5. 日志系统架构

```
┌──────────────┐     broadcast::channel(256)
│  handlers.rs │────────────────┬──────────────────┐
│  (emit entry)│                │                  │
└──────────────┘                ▼                  ▼
                    ┌───────────────────┐  ┌──────────────────┐
                    │   FileLogger      │  │  EventEmitter    │
                    │ (日志文件轮转)     │  │ (Tauri 事件推送)  │
                    │ proxy-YYYY-MM-DD  │  │ → 前端 LogViewer │
                    │     .jsonl        │  │                  │
                    └───────────────────┘  └──────────────────┘
```

- `LogCollector` 持有 `broadcast::Sender`，handler 通过 `emit()` 发送日志条目
- `FileLogger` 和 `EventEmitter` 各自订阅一个 `broadcast::Receiver`
- 使用 `CancellationToken` 统一管理生命周期，服务停止时所有后台任务一起退出
- FileLogger 按日期轮转文件，启动时清理超过 `retention_days` 的旧文件

### 6. 模型路由

模型路由是全局规则，独立于 Provider：

```toml
[[model_routes]]
match = "sonnet"
target = "deepseek-v4-pro"
reasoning_effort = "max"
```

匹配逻辑：客户端请求的 model 名称（转小写后）如果 **包含** `match` 字段的值，则路由到 `target` 模型。第一个匹配的规则生效。

这允许用户在 IDE 中使用 `claude-3-5-sonnet` 等模型名，实际请求被路由到 DeepSeek 等替代模型。

## 并发模型

- **HTTP 服务**：axum + tokio，每个请求一个 task
- **状态读取**：ArcSwap load（无锁）
- **状态写入**：ArcSwap store（原子）+ Mutex 保护配置文件读写
- **配置持久化**：`config_lock: Arc<Mutex<()>>` 序列化所有文件操作
- **日志写入**：单一 FileLogger task 顺序写入，避免文件竞争

## 错误处理策略

- `proxy-core` 对外暴露的 HTTP 错误使用英文（面向 API 消费者）
- Tauri IPC 错误使用中文（面向 GUI 用户）
- 内部 tracing 日志使用中文（面向开发者）
- `thiserror` 用于库 crate 的错误类型定义
- `anyhow` 仅用于 CLI binary 的顶层错误处理
