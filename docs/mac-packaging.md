# macOS 打包指南

## 推荐命令

本地测试包：

```bash
npm install
npm run package:mac
```

默认会构建 Apple Silicon 版本：

- target: `aarch64-apple-darwin`
- bundle: `dmg`
- 签名：关闭（`--no-sign`）

产物会输出到：

```text
target/aarch64-apple-darwin/release/bundle/dmg/
```

## 通用包

如果需要同时支持 Apple Silicon 和 Intel Mac：

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
npm run package:mac:universal
```

通用包耗时更久，且需要本机同时安装两个 Rust target。

## 正式分发签名

正式发给其他用户时建议使用 Apple Developer ID 签名和 notarization。先配置好 Tauri 所需的签名环境变量或证书，然后运行：

```bash
npm run package:mac:signed
```

如果只是内部测试或本机安装，使用默认的 `npm run package:mac` 即可。

## 常见问题

### TypeScript 未使用变量导致打包失败

`npm run package:mac` 会先执行 `npm run build`，而前端开启了严格检查。类似错误：

```text
error TS6133: 'Foo' is declared but its value is never read.
```

处理方式：删除未使用的 import、变量或函数，再重新运行打包命令。

### hdiutil: create failed - 设备未配置

这是 macOS 创建 DMG 时的磁盘镜像工具错误，常见于受限沙箱、远程执行器或权限不足的环境。

处理方式：

1. 在普通 macOS Terminal 中运行 `npm run package:mac`
2. 避免在受限沙箱里执行 DMG 打包阶段
3. 如果必须在自动化环境中运行，确保 runner 允许 `hdiutil attach/create/detach`

### 直接 npm run tauri build 没有生成 mac 安装包

仓库主配置历史上偏 Windows 打包。macOS 已增加 `src-tauri/tauri.macos.conf.json`，但仍建议使用 `npm run package:mac`，因为它会显式指定 `dmg`、target 和签名模式，并在缺依赖时给出清晰提示。

### 缺 Rust target

错误示例：

```text
Rust target aarch64-apple-darwin is not installed
```

处理方式：

```bash
rustup target add aarch64-apple-darwin
```

Intel 或通用包还需要：

```bash
rustup target add x86_64-apple-darwin
```
