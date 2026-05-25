# 部署与运维

## 部署方式

### 桌面应用（推荐个人使用）

构建安装包后直接安装运行：

```bash
npm run tauri build
```

安装包位于 `target/release/bundle/nsis/`（Windows）。

特点：
- 图形化管理界面
- 系统托盘常驻
- 配置文件自动存储在应用数据目录
- 关闭窗口最小化到托盘，不退出

### CLI 模式（适合服务器/无头环境）

```bash
# 构建
cargo build --release

# 运行（配置文件放在可执行文件同级目录）
./model-proxy
```

CLI 模式下配置文件查找顺序：
1. 可执行文件同级目录的 `config.toml`
2. 当前工作目录的 `config.toml`

## 网络配置

### 端口

默认监听 `0.0.0.0:4000`，即所有网络接口。

如果只需要本机访问（推荐），可以在前面加一层反向代理限制访问，或通过防火墙规则限制。

### 防火墙

如果代理仅供本机使用，建议：
- Windows：不需要额外配置，localhost 访问不受防火墙影响
- Linux：`ufw allow from 127.0.0.1 to any port 4000`

如果需要局域网内其他设备访问：
- 开放配置的端口（默认 4000）
- 务必设置 `server.api_key` 进行认证

### 反向代理（可选）

如果需要 HTTPS 或更细粒度的访问控制，可以在前面放 nginx：

```nginx
server {
    listen 443 ssl;
    server_name proxy.local;

    ssl_certificate /path/to/cert.pem;
    ssl_certificate_key /path/to/key.pem;

    location / {
        proxy_pass http://127.0.0.1:4000;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
        proxy_buffering off;           # 重要：流式响应需要关闭缓冲
        proxy_cache off;
        proxy_read_timeout 300s;       # 匹配非流式请求超时
    }
}
```

关键配置：
- `proxy_buffering off` — 流式响应必须关闭缓冲
- `proxy_read_timeout` — 至少 300s，匹配非流式请求超时

## 性能参数

### 内置参数（编译时确定）

| 参数 | 值 | 说明 |
|------|-----|------|
| 上游连接超时 | 30s | TCP 连接建立超时 |
| 非流式请求超时 | 300s | 等待上游完整响应的超时 |
| 连接池空闲超时 | 90s | 空闲连接回收时间 |
| 每主机最大空闲连接 | 32 | 连接池大小 |
| TCP_NODELAY | 开启 | 减少小包延迟 |
| TCP Keepalive | 60s | 保持长连接活跃 |
| 日志 broadcast 容量 | 256 | 日志通道缓冲区大小 |

### 可配置参数

| 参数 | 配置项 | 默认值 | 说明 |
|------|--------|--------|------|
| 监听端口 | `server.port` | 4000 | - |
| 请求体上限 | `server.max_body_bytes` | 64MB | 防止内存溢出 |
| 日志保留天数 | `logging.retention_days` | 7 | 磁盘空间管理 |
| 日志体截断 | `logging.max_body_bytes` | 4096 | 控制日志文件大小 |

### 资源占用

典型场景下的资源占用：
- 内存：~30-50MB（空闲），随并发请求数增长
- CPU：极低（大部分时间在等待 IO）
- 磁盘：日志文件按天轮转，每天大小取决于请求量

## 日志系统

### 日志文件

请求日志以 JSONL 格式存储，按天轮转：

```
logs/
├── proxy-2024-07-08.jsonl
├── proxy-2024-07-09.jsonl
└── proxy-2024-07-10.jsonl
```

每行一个 JSON 对象：

```json
{
  "id": "req_1720000000000_1",
  "timestamp": "2024-07-10T12:00:00+00:00",
  "method": "POST",
  "path": "/v1/messages",
  "provider": "deepseek",
  "model": "deepseek-v4-pro",
  "status": 200,
  "duration_ms": 1500,
  "is_stream": true,
  "error_message": null,
  "request_body": null,
  "response_body": null,
  "token_count": null
}
```

### 日志目录

| 模式 | 默认位置 |
|------|----------|
| 桌面应用 | `%APPDATA%/com.model-proxy.app/logs/` (Windows) |
| CLI | 可执行文件同级目录的 `logs/` |
| 自定义 | 通过 `logging.log_dir` 配置 |

### 日志轮转

- 每天自动创建新文件（UTC 日期）
- 服务启动时自动清理超过 `retention_days` 的旧文件
- 运行期间不会主动清理（下次启动时清理）

### tracing 日志（CLI 模式）

CLI 模式下的结构化运行日志：
- 文件：可执行文件同级目录的 `logs/` 下，按天轮转
- 格式：JSON（通过 `tracing-subscriber` 的 json layer）
- stdout：默认关闭，设置 `MODEL_PROXY_STDOUT_LOG=1` 开启

## 故障排查

### 常见问题

#### 启动失败："绑定端口 X 失败"

端口被占用。检查是否有其他进程使用该端口：

```bash
# Windows
netstat -ano | findstr :4000

# Linux
lsof -i :4000
```

解决：修改 `server.port` 或关闭占用端口的进程。

#### 上游请求失败：502 Bad Gateway

可能原因：
1. Provider 的 `base_url` 配置错误
2. Provider 的 `api_key` 无效或过期
3. 网络连接问题（DNS 解析失败、防火墙阻断）
4. 上游服务本身故障

排查步骤：
1. 检查日志中的具体错误信息
2. 用 curl 直接测试上游 API 是否可达
3. 确认 `base_url` 格式正确（不含路径后缀）

#### 认证失败：401 Unauthorized

客户端未提供正确的 API Key。确认：
1. 配置文件中设置了 `server.api_key`
2. 客户端请求携带了 `x-api-key` 或 `Authorization: Bearer` header
3. 值完全匹配（注意前后空格）

#### 请求超时

非流式请求超时为 300s。如果上游模型响应慢：
- 考虑使用流式模式（`"stream": true`）
- 流式模式无总超时限制

#### 切换 Provider 后请求仍走旧 Provider

正常情况下切换是即时生效的。如果出现这种情况：
1. 确认切换操作成功（GUI 显示新的活跃 Provider）
2. 检查是否有进行中的请求（它们会继续使用旧 Provider 完成）
3. 新请求应该立即使用新 Provider

### 日志级别调整

开发调试时可以通过环境变量调整日志级别：

```bash
# 显示所有 debug 日志
set RUST_LOG=debug

# 只显示 proxy-core 的 debug 日志
set RUST_LOG=proxy_core=debug

# 显示 HTTP 层的 trace 日志
set RUST_LOG=proxy_core=debug,tower_http=trace
```

## 安全建议

1. **始终设置 `server.api_key`** — 防止未授权访问
2. **不要将代理暴露到公网** — 除非有额外的认证/防火墙保护
3. **定期轮换 Provider API Key** — 降低泄露风险
4. **谨慎启用 `record_body`** — 请求/响应体可能包含敏感信息
5. **配置文件权限** — 确保只有当前用户可读（包含 API Key）
