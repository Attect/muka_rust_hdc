# Muka Rust HDC

华为 **HDC（HarmonyOS Device Connector）** 主机工具的 Rust 重实现 —— 可替代官方 `hdc` 二进制程序，通过 **USB** 和 **TCP/Wi-Fi** 连接 HarmonyOS / OpenHarmony 设备。

- **作者**：Attect
- **项目网站**：<https://muka.cool/rust_hdc>
- **代码仓库**：<https://github.com/Attect/muka_rust_hdc>
- **开源协议**：MIT
- **协议版本**：对齐官方 HDC **3.2.0e**

## 项目背景

官方 HDC 是随 DevEco Studio 分发的闭源 C++ 二进制程序。Muka Rust HDC 使用 Rust 重实现了主机端（以及一个守护进程桩），采用完全相同的线上协议，因此可以：

- 与真实的 HarmonyOS 设备通信（已在 VID `0x12D1` / PID `0x1101` 设备上验证），
- 作为 **DevEco Studio** 的后端服务（在 `127.0.0.1:8710` 上提供双协议服务），
- 使用同一份代码在 Windows（MSVC）、Linux 和 macOS 上构建运行。

## 工作区结构

| Crate | 路径 | 作用 |
|-------|------|------|
| `hdc` | `hdc/` | 主机工具：命令行客户端 + 后台服务（`hdc -m`） |
| `hdcd` | `hdcd/` | 守护进程桩（面向 OpenHarmony 目标；端到端测试使用 Linux 桩） |
| `hdc-protocol` | `crates/hdc-protocol/` | 共享协议原语（数据包帧、序列化、加密） |

逆向自官方实现的协议参考文档位于 `docs/`：

- `docs/HDC_SERVER_SOCKET_PROTOCOL.md` —— 客户端↔服务端 Socket 协议（含 DevEco Studio 握手）
- `docs/USB_TRANSPORT.md` —— USB 传输说明（WinUSB  quirks、端点、超时参数）

## 功能特性

- **传输方式**
  - USB（基于 `rusb`/libusb，Windows 上使用 WinUSB；接口声明后不可调用 `set_alternate_setting` / `clear_halt`，详见 `docs/USB_TRANSPORT.md`）
  - TCP：`hdc tconn <ip:port>`；UDP 设备发现（`hdc discover`，端口 8710）
  - 可选 AES-128-GCM PSK 加密通道（`OHOS_HDC_ENCRYPT_CHANNEL=1`）
- **认证**：RSA 三方握手（3072 位密钥，存于 `%USERPROFILE%/.harmony/hdckey` / `~/.harmony/hdckey`），"始终信任"签名流程，心跳机制（`HeartbeatMsg`）
- **Shell**：单条命令（`hdc shell <cmd>`）与交互式 PTY（`hdc shell`）
- **文件传输**：`hdc file send` / `hdc file recv`，511 KB IO 缓冲
- **应用管理**：`hdc install`（支持 `.hap`，以及 `.app` App Pack 自动解包）、`hdc uninstall`
- **端口转发**：`fport` / `rport`（含 `ls` / `rm`），支持 `tcp:`、`localabstract:`、`localreserved:`、`localfilesystem:`、`dev:`、`jdwp:pid`、`ark:pid@package`（DevEco Studio 的 ArkTS 调试）
- **设备控制**：`target mount`、`target boot`、`target reconnect`、`smode`
- **诊断**：`hilog`、`bugreport`、`jpid` / `track-jpid`（JDWP）
- **Flashd**：`update`、`flash`、`erase`、`format`（主机侧协议，已对桩守护进程验证）
- **多设备**：`-t <key>` 目标选择（USB 序列号或 `ip:port`），仅一台设备时自动选择
- **DevEco Studio 兼容**：`hdc -m` 在同一端口同时支持 Rust 客户端协议与 IDE 的 48 字节 `OHOS HDC` 握手 + 长度前缀命令帧

## 构建

需要较新的 stable Rust 工具链；Windows 上使用 MSVC 工具链；libusb v1.0.27 通过 `libusb1-sys` 内置编译。

```bash
cargo build --release --bin hdc
```

`hdcd` 面向 OpenHarmony 目标，需要 OpenHarmony 工具链；不支持构建到 Android。

## 使用方法

先启动后台服务（客户端在需要时也会自动启动）：

```bash
hdc -m                          # 服务模式（同时在 127.0.0.1:8710 为 DevEco Studio 提供服务）
```

常用命令：

```bash
hdc list targets -v             # 枚举设备
hdc shell echo hello            # 执行单条 shell 命令
hdc shell                       # 交互式 PTY shell
hdc file send test.txt /data/local/tmp/test.txt
hdc file recv /data/local/tmp/test.txt recv.txt
hdc install entry-default-signed.hap
hdc install myapp.app           # .app App Pack：自动解包并依次安装其中的 .hap
hdc uninstall com.example.myapp
hdc fport tcp:8080 tcp:8080     # 将主机 :8080 转发到设备 :8080
hdc fport ls / hdc fport rm tcp:8080
hdc rport tcp:9000 tcp:9000     # 反向转发
hdc tconn 192.168.1.10:5555     # 通过 TCP/Wi-Fi 连接设备
hdc discover                    # UDP 设备发现
hdc -t 192.168.1.10:5555 shell echo hello   # 指定目标设备
hdc hilog                       # 查看设备日志
hdc bugreport                   # 收集故障报告
hdc jpid                        # 列出可调试（JDWP）进程
```

### Git Bash 路径注意事项（Windows）

Git Bash 会自动转换 Unix 风格的远程路径，需使用双斜杠禁用转换：

```bash
# 错误：/data/local/tmp 会被转换为 C:/Program Files/Git/data/local/tmp
hdc file send test.txt //data//local//tmp//test.txt
```

## 开发说明

- `AGENTS.md` 是权威的工程日志：协议细节、文件/应用传输的角色矩阵、已知问题与版本历史。
- `.cargo/config.toml`（机器相关的构建路径）、`.codex/`、`.idea/` 等本地工具目录已被 git 忽略。
- **请勿**用本仓库构建的二进制覆盖官方的 `hdc.exe`。

---

*本项目由 AI 辅助实现。*
