# Model Proxy

一个基于 Tauri 2.0 的 AI 模型代理桌面应用，提供图形化界面管理代理服务配置和生命周期。支持 OpenAI 和 Anthropic 格式的请求代理转发，多 Provider 配置与运行时热切换，以及基于模式匹配的模型路由。

## 功能特性

- **多 Provider 管理** — 配置多个 AI 服务提供商（OpenAI、Anthropic、DeepSeek 等），通过 GUI 一键切换
- **运行时热切换** — 切换 Provider 无需重启服务，进行中的请求不受影响，新请求立即使用新 Provider
- **图形化配置管理** — 通过 GUI 编辑端口、API Key、Provider 设置、模型路由等所有配置项
- **服务启停控制** — 一键启动/停止代理服务，实时显示运行状态、请求计数和当前活跃 Provider
- **系统托盘** — 最小化到托盘，右键菜单快速启停服务
- **OpenAI / Anthropic 格式代理** — 接收 Anthropic 格式请求，自动转换为目标 Provider 格式
- **模型路由** — 基于模式匹配将客户端请求的模型名映射到实际目标模型
- **请求日志** — JSONL 格式日志文件按天轮转，前端实时查看
- **配置向后兼容** — 旧的单 Provider 配置文件自动迁移为新的多 Provider 格式

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

1. 启动应用后，在 **Provider 管理** 页面添加你的 AI 服务提供商配置
2. 在 **服务器设置** 页面配置监听端口和认证密钥
3. 在 **服务状态** 页面点击启动服务
4. 将 IDE 或客户端的 API Base URL 指向 `http://localhost:4000`，API Key 设为你配置的 `server.api_key`

## 项目结构

```
model-proxy/
├── crates/proxy-core/    # 代理核心逻辑（独立 crate）
├── src/                  # CLI binary 入口
├── src-tauri/            # Tauri 桌面应用后端
├── ui/                   # React 前端（Ant Design）
├── docs/                 # 详细文档
└── config.example.toml   # 配置文件示例
```

## 技术栈

| 层级 | 技术 |
|------|------|
| 桌面框架 | Tauri 2.0 |
| 前端 | React 18 + TypeScript + Ant Design 5 |
| 构建工具 | Vite 6 |
| 后端 | Rust (Tokio + axum) |
| 状态管理 | ArcSwap（无锁热切换）|

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
