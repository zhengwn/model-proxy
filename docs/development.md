# 开发指南

## 环境搭建

### 前置条件

- **Rust** — 最新 stable 版本（[安装](https://rustup.rs/)）
- **Node.js** — 18+ 版本
- **npm** — 随 Node.js 安装
- **WebView2 Runtime** — Windows 10+ 通常已预装；若未安装请从 [Microsoft](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) 下载

### 安装依赖

```bash
npm install
```

Rust 依赖在首次构建时自动下载。

### 开发模式

```bash
# Tauri 开发模式（前端热重载 + Rust 后端自动重编译）
npm run tauri dev

# 仅运行 CLI 版本
cargo run -- --config config.example.toml

# 仅构建 Rust（不启动前端）
cargo build --workspace
```

## 项目结构

```
model-proxy/
├── crates/proxy-core/    # 核心代理逻辑（纯库 crate）
├── src/                  # CLI binary 入口
├── src-tauri/            # Tauri 桌面应用
├── ui/                   # React 前端
├── docs/                 # 项目文档
└── config.example.toml   # 配置示例
```

详细结构见 [architecture.md](./architecture.md)。

## 代码规范

### 语言使用规则

| 场景 | 语言 | 原因 |
|------|------|------|
| HTTP 响应中的错误信息 | 英文 | API 消费者是程序，不应假设语言 |
| `Display` trait 实现 | 英文 | 可能出现在 HTTP 响应中 |
| `ConfigError` 等错误类型 | 英文 | 可能流入 HTTP 响应 |
| Tauri IPC 错误字符串 | 中文 | 直接展示给 GUI 用户 |
| `tracing` 日志 | 中文 | 面向开发者，团队内部使用 |
| 代码注释 | 中文 | 团队内部使用 |
| 文档 | 中文 | 面向中文开发者 |

### Rust 代码风格

- 使用 `thiserror` 定义库 crate 的错误类型
- `anyhow` 仅用于 binary crate 的顶层
- 优先使用 `Arc<ArcSwap<T>>` 而非 `Arc<RwLock<T>>` 做读多写少的共享状态
- 异步函数如果没有 `.await` 调用，应改为同步函数
- 所有 `pub struct` 如果有 `len()` 方法，必须同时提供 `is_empty()`

### TypeScript 代码风格

- 使用函数组件 + Hooks
- 类型定义集中在 `ui/types/index.ts`
- 自定义 Hook 放在 `ui/hooks/` 目录
- Tauri IPC 调用通过 `@tauri-apps/api/core` 的 `invoke`

## 测试

### 运行测试

```bash
# Rust 测试（包含 property-based tests）
cargo test --workspace

# 前端测试
npm run test

# 前端测试（watch 模式）
npm run test:watch
```

### 测试策略

| 模块 | 测试类型 | 说明 |
|------|----------|------|
| `config.rs` | 单元测试 + Property-based | 配置解析、验证、序列化的正确性 |
| `provider_registry.rs` | 单元测试 + Property-based | 注册表查找、重复检测 |
| `logging/` | Property-based | 日志条目序列化、截断、过滤、文件轮转 |
| `proxy/stream.rs` | 单元测试 | SSE 解析、流式转换 |
| `proxy/convert.rs` | 单元测试 | 格式转换正确性 |
| `ui/components/` | 组件测试 | React Testing Library |
| `ui/utils/` | Property-based | 日志过滤逻辑 |

Property-based testing 使用：
- Rust: `proptest` crate
- TypeScript: `fast-check`

### 添加新测试

Rust 测试文件约定：
- 模块内测试：`#[cfg(test)] mod tests { ... }`
- 独立测试文件：`#[cfg(test)] #[path = "xxx_tests.rs"] mod tests;`

## 构建

### 开发构建

```bash
cargo build --workspace
```

### 生产构建

```bash
# 桌面应用安装包
npm run tauri build

# 仅 CLI binary
cargo build --release
```

构建产物：
- Windows 安装包：`target/release/bundle/nsis/`
- CLI binary：`target/release/model-proxy.exe`

## 添加新功能指南

### 添加新的 Provider 格式

1. 在 `config.rs` 的 `ProviderFormat` 枚举中添加新变体
2. 在 `convert.rs` 的 `prepare_body()` 中添加新格式的请求体处理
3. 在 `convert.rs` 的 `build_provider_request()` 中添加新格式的 HTTP 请求构建
4. 在 `handlers.rs` 的 `proxy_messages()` 中添加新格式的响应处理分支
5. 如果需要流式处理，在 `stream.rs` 或新文件中实现
6. 更新前端 `ui/types/index.ts` 的 `ProviderConfig.format` 类型
7. 更新 `config.example.toml` 和文档

### 添加新的 Tauri IPC 命令

1. 在 `src-tauri/src/commands.rs` 中定义 `#[tauri::command]` 函数
2. 在 `src-tauri/src/lib.rs` 的 `invoke_handler` 中注册
3. 在前端通过 `invoke<ReturnType>("command_name", { args })` 调用
4. 如果需要新的权限，更新 `src-tauri/capabilities/default.json`

### 添加新的前端页面

1. 在 `ui/components/` 中创建组件
2. 在 `ui/App.tsx` 的 `items` 数组中添加 Tab
3. 如果需要状态管理，在 `ui/hooks/` 中创建自定义 Hook

## 调试

### Rust 后端日志

CLI 模式下设置环境变量启用 stdout 日志：

```bash
set MODEL_PROXY_STDOUT_LOG=1
cargo run
```

日志级别通过 `RUST_LOG` 环境变量控制：

```bash
set RUST_LOG=proxy_core=debug,tower_http=debug
```

### 前端调试

Tauri 开发模式下可以使用浏览器 DevTools（右键 → 检查）。

### 网络调试

使用 `test_proxy.py` 脚本测试代理功能：

```bash
python test_proxy.py
```
