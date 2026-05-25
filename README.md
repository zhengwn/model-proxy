# Model Proxy

一个基于 Tauri 2.0 的 AI 模型代理桌面应用，提供图形化界面管理代理服务配置和生命周期。

**核心场景：** 让只支持 Anthropic Messages API 格式的 AI 编程 IDE（Cursor、Windsurf 等）能够透明地使用任何 AI 提供商（OpenAI、DeepSeek、Anthropic、Gemini、Azure 等），无需修改 IDE 配置即可在多个提供商之间热切换。

## 功能特性

- **多 Provider 管理** — 配置多个 AI 服务提供商（OpenAI、Anthropic、DeepSeek、Gemini、Azure 等），通过 GUI 一键切换，支持 6 种预设模板快速添加
- **运行时热切换** — 基于 `ArcSwap` 无锁原子指针交换，切换 Provider 无需重启服务，进行中的请求不受影响，新请求立即使用新 Provider
- **格式自动转换** — 暴露 Anthropic Messages API 格式（`/v1/messages`），内部自动将请求转为 OpenAI Chat Completions 格式发给上游，并将响应转回 Anthropic 格式；同时支持 `/v1/chat/completions` 直通代理
- **流式状态机** — OpenAI ↔ Anthropic 流式转换使用状态机追踪 reasoning → text → tool_use 块切换，确保 content_block_start/stop 事件语义正确
- **模型路由** — 基于子串匹配（大小写不敏感）将客户端请求的模型名路由到实际目标模型，支持 `reasoning_effort` 覆盖（low/medium/high/max），GUI 内置 5 个预设路由模板
- **Provider 兼容性适配（Quirks）** — 针对不同提供商的差异提供细粒度兼容配置：`reasoning_all_or_nothing`、`no_json_schema`、`supports_reasoning_effort`、`max_reasoning_effort`
- **图形化配置管理** — 通过 4 个选项卡（服务状态、Provider 管理、模型路由、请求日志）编辑所有配置项，首次启动引导式配置
- **服务启停控制** — 一键启动/停止代理服务，实时显示运行状态、请求计数（含失败数）、监听地址（可一键复制）
- **系统托盘** — 最小化到托盘，右键菜单快速启停服务
- **请求日志** — JSONL 格式日志文件按天轮转，前端实时流式查看（通过 Tauri 事件推送），支持状态码、Provider、关键词过滤，记录代理开销、首 token 延迟、传输时间
- **配置向后兼容** — 旧的单 `[provider]` 配置文件自动迁移为 `[[providers]]` 数组格式
- **双模式运行** — 支持 Tauri 桌面 GUI 模式和纯 CLI 模式（适合服务器/无头环境）

## 快速开始

### 安装依赖

```bash
npm install
```

### 开发模式

```bash
npm run tauri dev
```

### CLI 模式（无 GUI）

```bash
cargo run
```

配置文件放在可执行文件同级目录或当前工作目录，命名为 `config.toml`。参考 `config.example.toml`。

### 生产构建

```bash
# 桌面应用安装包
npm run tauri build

# 仅 CLI binary
cargo build --release
```

## 使用方式

1. 启动应用后，在 **Provider 管理** 页面添加你的 AI 服务提供商配置（支持 6 种模板快速填充）
2. 在 **服务状态** 页面的内联设置中配置监听端口和认证密钥
3. 在 **服务状态** 页面点击启动服务
4. 将 IDE 或客户端的 API Base URL 指向 `http://localhost:4000`，API Key 设为你配置的 `server.api_key`
5. 可在 **模型路由** 页面配置模型名称映射（如将 `claude-sonnet` 路由到 `deepseek-v4-pro`），随时修改无需重启

## API 端点

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/health` | 健康检查，返回 `{"status": "ok"}` |
| POST | `/v1/messages` | 主代理端点，接受 Anthropic Messages API 格式，自动转换为目标 Provider 格式后转发 |
| POST | `/v1/chat/completions` | OpenAI Chat Completions 格式代理，内部转 Anthropic 再转回 OpenAI |

**认证：** 若配置了 `server.api_key`，客户端需通过 `x-api-key` 或 `Authorization: Bearer` 头提供密钥，否则返回 401。

**超时：** 上游连接 30s，非流式请求 300s，流式请求无总超时，连接池空闲 90s。

## 项目结构

```
model-proxy/
├── crates/proxy-core/       # 代理核心逻辑（纯库 crate，无 Tauri 依赖）
│   ├── src/config.rs        #   配置解析、验证、序列化、向后兼容迁移
│   ├── src/error.rs         #   统一错误类型（thiserror）
│   ├── src/provider_registry.rs  # Provider 注册表（按名查找、重复检测）
│   ├── src/proxy/           #   HTTP 处理器 & 格式转换核心
│   │   ├── convert.rs       #     Anthropic ↔ OpenAI 非流式转换
│   │   ├── stream.rs        #     流式转换状态机（SSE 解析 & 事件生成）
│   │   ├── passthrough.rs   #     Anthropic 格式直通代理
│   │   ├── response.rs      #     响应构建 & SSE 输出
│   │   ├── fallback.rs      #     故障回退 & 重试逻辑
│   │   ├── utils.rs         #     认证、请求体限制等工具
│   │   └── state.rs         #     ArcSwap 共享状态
│   └── src/logging/         #   日志收集器、文件写入（JSONL）、截断、轮转
├── src/                     # CLI 二进制入口
│   └── main.rs              #   配置加载、信号处理、服务器启动
├── src-tauri/               # Tauri 桌面应用壳
│   ├── src/lib.rs           #   Tauri 插件注册 & 启动
│   ├── src/commands.rs      #   16 个 IPC 命令（配置/Provider/路由 CRUD）
│   ├── src/service.rs       #   代理服务生命周期管理
│   ├── src/tray.rs          #   系统托盘集成
│   └── src/logging.rs       #   日志事件转发到前端
├── ui/                      # React 18 + TypeScript + Ant Design 5
│   ├── components/          #   8 个组件（见下方组件树）
│   ├── hooks/               #   useConfig / useProviders / useServiceStatus
│   ├── utils/               #   日志过滤 & 验证工具
│   └── types/               #   10 个 TypeScript 接口定义
├── docs/                    # 详细文档（架构、API、配置、开发、部署）
├── config.example.toml      # 配置文件示例（含所有选项 & 注释）
└── test_proxy.py            # 端到端集成测试脚本
```

### 组件树

```
<ConfigProvider theme={darkAlgorithm}>
  <ErrorBoundary>
    <Layout>
      <Tabs>
        ├── "服务状态"  → <StatusPanel>      ← 仪表盘 + 引导 + 内联设置
        ├── "Provider 管理" → <ProviderManager>
        │                      ├── <Spin>             (加载态)
        │                      ├── <Alert>            (错误 + 重试)
        │                      ├── <ProviderForm>     (添加/编辑表单)
        │                      │    └── <TestConnectionButton>
        │                      └── <ProviderList>     (列表 + 操作按钮)
        ├── "模型路由"  → <ModelRoutesEditor>  ← 匹配→目标 规则编辑
        └── "请求日志"  → <LogViewer>           ← 实时流式日志表格
                             └── <LogSettings>  (内联日志配置)
```

## 核心架构

### 请求处理流水线

```
Client (Anthropic Format)
  │  POST /v1/messages
  ▼
┌─────────────────────────────────┐
│  Auth Middleware                 │  x-api-key / Bearer token
├─────────────────────────────────┤
│  Body Size Check                 │  最大值可配（默认 64MB）
├─────────────────────────────────┤
│  Model Route Matching            │  子串匹配 → target model
├─────────────────────────────────┤
│  Provider Lookup (ArcSwap 读取)  │  无锁获取当前活跃 Provider
├─────────────────────────────────┤
│  Format Conversion               │
│  Anthropic → OpenAI (convert.rs) │  或直通 (passthrough.rs)
├─────────────────────────────────┤
│  Upstream Request (reqwest)      │  HTTPS 连接池
├─────────────────────────────────┤
│  Response Conversion             │
│  OpenAI → Anthropic (stream.rs)  │  流式状态机 / 非流式直接映射
├─────────────────────────────────┤
│  Logging                         │  broadcast channel → FileLogger + EventEmitter
└─────────────────────────────────┘
  │  SSE / JSON Response
  ▼
Client (Anthropic Format)
```

### 无锁热切换

活跃 Provider 存储在 `Arc<ArcSwap<ProviderConfig>>` 中：
- **读取：** 原子指针加载，完全无锁，零阻塞
- **写入：** 原子指针交换，新请求立即生效
- **保护：** 进行中的请求持有旧 `Arc`，直到处理完成才释放

### 流式转换状态机

OpenAI SSE chunks → Anthropic SSE events 的映射使用状态机追踪当前块类型：

```
unstarted → reasoning → text → tool_use → ... → ended
                │          │         │
                └──────────┴─────────┘
                 块切换时自动插入
                 content_block_stop +
                 content_block_start
```

确保 Anthropic 协议要求的每个 content block 都有完整的 start/delta/stop 事件序列。

### 日志管线

```
请求处理完成
  │
  ▼
LogCollector (broadcast::Sender)
  ├──→ FileLogger    → JSONL 文件（按天轮转，按保留天数清理）
  └──→ EventEmitter  → Tauri 事件推送 → 前端 LogViewer 实时更新（上限 100 条）
```

## 技术栈

| 层级 | 技术 | 备注 |
|------|------|------|
| 桌面框架 | Tauri 2.0 | `tray-icon` 特性 |
| 前端 | React 18 + TypeScript 5.6 | 严格模式 |
| UI 组件库 | Ant Design 5 + @ant-design/icons | 暗色主题（darkAlgorithm） |
| 构建工具 | Vite 6 | 前端构建 + HMR |
| 测试框架 | Vitest 4 + React Testing Library 14 + fast-check | 组件测试 + 属性测试 |
| 后端 | Rust 2021 + Tokio + axum + reqwest | 异步 HTTP 代理 |
| 配置 | toml + serde | 配置解析 & 验证 |
| 状态管理 | ArcSwap（Rust）/ 本地 useState（前端） | 无锁原子读写 / 无全局状态库 |
| 日志 | tracing + tracing-subscriber + tracing-appender | 结构化日志 + 文件轮转 |
| 属性测试 | proptest（Rust）+ fast-check（TS） | 随机输入验证 |
| 代码质量 | clippy + rustfmt | 自定义阈值 |
| 打包 | NSIS（Windows）| `installMode: currentUser` |

## 测试策略

| 模块 | 测试类型 | 说明 |
|------|----------|------|
| `proxy-core/config.rs` | 单元 + proptest | 解析、验证、序列化、向后兼容 |
| `proxy-core/provider_registry.rs` | 单元 + proptest | 查找、重复检测 |
| `proxy-core/logging/` | proptest | 序列化、截断、过滤、文件轮转 |
| `proxy-core/proxy/stream.rs` | 单元 | SSE 解析、流式转换正确性 |
| `proxy-core/proxy/convert.rs` | 单元 | Anthropic ↔ OpenAI 格式转换 |
| `ui/components/` | React Testing Library | ProviderForm、ProviderList、ProviderManager 交互测试 |
| `ui/components/LogViewer` | fast-check 属性测试 | 日志过滤（状态码、Provider、关键词） |
| `ui/utils/` | fast-check 属性测试 | `validateMaxBodyBytes`、`validateRetentionDays` 边界值 |
| 端到端 | `test_proxy.py` | 全链路集成测试（启动代理 + mock 上游 + 发送真实请求） |

## 文档

| 文档 | 内容 |
|------|------|
| [架构设计](docs/architecture.md) | 系统架构、设计决策、模块交互 |
| [API 参考](docs/api.md) | HTTP 端点、请求/响应格式、认证 |
| [配置详解](docs/configuration.md) | 所有配置项说明、格式、限制 |
| [开发指南](docs/development.md) | 环境搭建、代码规范、测试、添加新功能 |
| [部署运维](docs/deployment.md) | 部署方式、性能参数、日志、故障排查 |
| [变更日志](CHANGELOG.md) | 版本变更记录 |

## License

Private
