# 发布流程与版本号规则

## 版本号规则

采用语义化版本 `vMAJOR.MINOR.PATCH`（Git tag 与 GitHub Release 标题一致，如 `v0.2.0`）：

| 变更类型 | 版本位 | 示例 |
|----------|--------|------|
| 仅缺陷修复 | PATCH+1 | `v0.1.0` → `v0.1.1` |
| 新功能 / 新平台支持 | MINOR+1，PATCH 归零 | `v0.1.0` → `v0.2.0` |
| 破坏性变更（协议不兼容、CLI 不兼容） | MAJOR+1 | `v0.2.0` → `v1.0.0` |

说明：

- 代码内 `Ver: 3.2.0e…`（`HDC_VERSION_NUMBER`）是**协议版本**，与发布版本号相互独立，发布时不改动。
- 每次发布必须包含完整产物矩阵（见下）+ `sha256sums.txt` + release notes（列出变更与构建环境）。
- release notes 需注明各产物的构建环境与工具链，便于追溯。

## 产物矩阵与命名

命名规则：`hdc-<os>-<arch>[变体]`。

| 产物 | 平台 | 构建环境 |
|------|------|----------|
| `hdc-windows-x86_64.exe` | Windows x86_64（静态 CRT） | 本机 Windows + MSVC |
| `hdc-linux-x86_64` | Linux x86_64 glibc | 本机 WSL Debian 12 |
| `hdc-linux-x86_64-glibc2.24` | Linux x86_64 glibc≥2.24（zig cc） | 本机 WSL Debian 12 |
| `hdc-linux-x86_64-musl-static` | Linux x86_64 全静态（musl） | 本机 WSL Debian 12 |
| `hdc-linux-aarch64` | Linux aarch64 glibc | orangepi@192.168.8.120（Debian 12） |
| `hdc-linux-aarch64-glibc2.24` | Linux aarch64 glibc≥2.24（zig cc） | 同上 |
| `hdc-linux-aarch64-musl-static` | Linux aarch64 全静态（musl） | 同上 |
| `hdc-macos-arm64` | macOS arm64（≥11.0） | mac03@192.168.8.133 |
| `hdc-macos-x86_64` | macOS x86_64（≥11.0） | 同上（交叉编译） |

## 构建步骤

### Windows（本机）

```bash
RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --bin hdc
# 产物: target/standalone/x86_64-pc-windows-msvc/release/hdc.exe → hdc-windows-x86_64.exe
```

### Linux x86_64（本机 WSL Debian 12）

工具链：zig 0.16.0（`~/zig/zig`）、musl-gcc（Debian 包）、`~/opt/aarch64-linux-musl-cross`（仅 aarch64 musl 用）。

```bash
scripts/build-release.sh x86_64-gnu x86_64-glibc2.24 x86_64-musl
```

### Linux aarch64（orangepi@192.168.8.120，Debian 12 aarch64）

工具链：rustup + `~/zig/zig`（zig-linux-aarch64）+ musl.cc 原生工具链 `~/opt/aarch64-linux-musl-cross`（aarch64 主机需用 `aarch64-linux-musl-native`，x86_64 版无法在 aarch64 上运行）。

```bash
scripts/build-release.sh aarch64-gnu aarch64-glibc2.24 aarch64-musl
```

### macOS（mac03@192.168.8.133）

```bash
MACOSX_DEPLOYMENT_TARGET=11.0 cargo build --release --target aarch64-apple-darwin --bin hdc
MACOSX_DEPLOYMENT_TARGET=11.0 cargo build --release --target x86_64-apple-darwin --bin hdc
```

### 验证与发布

```bash
scripts/verify-release.sh          # 架构 / glibc 版本需求 / musl 静态性
cd target/dist && sha256sum hdc-* > sha256sums.txt
gh release create vX.Y.Z --title "Muka Rust HDC vX.Y.Z" <所有产物>
```
