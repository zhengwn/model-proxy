# Changelog

本文件记录项目的版本变更。格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## [Unreleased]

### Added
- 请求计数统计（总请求数、失败请求数）实时显示在服务状态面板
- 系统托盘启动/停止服务功能
- FileLogger 文件句柄缓存，减少高频写入时的系统调用
- Anthropic 直通模式流式响应结束日志
- 项目文档体系（architecture、api、configuration、development、deployment）

### Changed
- `switch_provider` 命令改为先持久化再更新内存状态，确保一致性
- `Config.provider` 字段标记为 `skip_serializing`，不再泄漏到前端
- `convert_non_stream_response` 从 async 改为同步函数
- HTTP 错误响应统一使用英文（面向 API 消费者）
- `ConfigError` Display 实现改为英文
- 服务状态轮询从 2s 改为 5s + 事件驱动即时刷新
- Tauri CSP 从 null 改为合理的安全策略

### Fixed
- `ProviderRegistry` 添加 `is_empty()` 方法（消除 Clippy 警告）

## [0.1.0] - 初始版本

### Added
- 多 Provider 管理与运行时热切换（ArcSwap 无锁）
- Anthropic → OpenAI 格式转换（流式 + 非流式）
- Anthropic 直通模式
- 模型路由（子串匹配）
- 图形化配置管理（React + Ant Design）
- 服务启停控制与状态面板
- 系统托盘（最小化到托盘、状态显示）
- 请求日志（JSONL 文件轮转 + 前端实时查看）
- CLI 独立运行模式
- 配置向后兼容（旧格式自动迁移）
- 推理强度（reasoning_effort）映射
- 工具调用（tool_use）格式转换
- JSON Schema 响应格式降级处理
