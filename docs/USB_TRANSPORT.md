# HDC USB 传输层技术笔记

> 基于 OpenHarmony HDC (HarmonyOS Device Connector) 原版 C++ 实现分析，为 Rust 重写项目提供 USB 传输层实现参考。

## 1. 概述

HDC 支持三种传输通道：TCP、USB、UART。USB 是连接 PC 与鸿蒙设备的主要方式之一。

- **Host 端**（PC）：使用 `libusb` 枚举设备、建立连接、收发数据
- **Daemon 端**（设备）：使用 Linux USB FunctionFS (GadgetFS) 框架，通过文件读写操作 USB 端点

## 2. USB 设备识别标准

鸿蒙设备在 USB 层使用与 ADB (Android Debug Bridge) **完全一致的接口描述符**：

| 字段 | 值 | 说明 |
|------|-----|------|
| `bInterfaceClass` | `0xFF` | Vendor Specific |
| `bInterfaceSubClass` | `0x50` | 厂商自定义子类 |
| `bInterfaceProtocol` | `0x01` | 厂商自定义协议 |
| `bNumEndpoints` | `2` | 两个端点：Bulk IN + Bulk OUT |

### 2.1 识别逻辑（Host 端）

遍历 USB 配置描述符中的所有接口：

```cpp
bool HostUsb::IsDebuggableDev(const struct libusb_interface_descriptor *ifDescriptor)
{
    constexpr uint8_t harmonyEpNum = 2;
    constexpr uint8_t harmonyClass = 0xff;
    constexpr uint8_t harmonySubClass = 0x50;
    constexpr uint8_t harmonyProtocol = 0x01;

    if (ifDescriptor->bInterfaceClass != harmonyClass || 
        ifDescriptor->bInterfaceSubClass != harmonySubClass ||
        ifDescriptor->bInterfaceProtocol != harmonyProtocol) {
        return false;
    }
    if (ifDescriptor->bNumEndpoints != harmonyEpNum) {
        return false;
    }
    return true;
}
```

**注意**：鸿蒙设备不通过 VID/PID 过滤，而是通过接口描述符特征识别。

### 2.2 端点识别

匹配到目标接口后，遍历其端点描述符：

```cpp
for (k = 0; k < ifDescriptor->bNumEndpoints; ++k) {
    const struct libusb_endpoint_descriptor *ep_desc = &ifDescriptor->endpoint[k];
    // 1. 确认是 Bulk 端点
    if ((ep_desc->bmAttributes & 0x03) != LIBUSB_TRANSFER_TYPE_BULK) {
        continue;
    }
    // 2. 判断方向
    if (ep_desc->bEndpointAddress & LIBUSB_ENDPOINT_IN) {
        // Bulk IN (设备 → Host，即 PC 读取)
        hUSB->hostBulkIn.endpoint = ep_desc->bEndpointAddress;
    } else {
        // Bulk OUT (Host → 设备，即 PC 写入)
        hUSB->hostBulkOut.endpoint = ep_desc->bEndpointAddress;
        hUSB->wMaxPacketSizeSend = ep_desc->wMaxPacketSize; // 记录最大包大小
    }
}
```

## 3. Host 端 USB 实现

### 3.1 依赖库

原版使用 `libusb-1.0` C 库。Rust 重写方案：

| 方案 | Crate | 说明 |
|------|-------|------|
| 绑定方案 | `rusb` | `libusb1-sys` 绑定，API 接近原生 libusb |
| 现代方案 | `nusb` | 纯 Rust 实现，API 更现代，跨平台支持好 |

**推荐**：`nusb` crate，因为它：
- 纯 Rust 实现，无需系统安装 libusb
- 更好的 Windows 原生支持
- 更现代的异步/同步 API

### 3.2 核心数据结构

```cpp
struct HdcUSB {
    libusb_device *device;           // libusb 设备引用
    libusb_device_handle *devHandle; // 设备句柄
    uint8_t busId;                   // USB 总线号
    uint8_t devId;                   // USB 设备地址
    string serialNumber;             // 设备序列号（作为 connectKey）
    uint8_t interfaceNumber;         // 接口号
    HostUSBEndpoint hostBulkIn;      // Bulk IN 端点
    HostUSBEndpoint hostBulkOut;     // Bulk OUT 端点
    uint16_t wMaxPacketSizeSend;     // 最大发送包大小
};

struct HostUSBEndpoint {
    uint8_t endpoint;        // 端点地址
    bool bulkInOut;          // true=IN, false=OUT
    libusb_transfer *transfer; // 异步传输对象
    std::mutex mutexIo;
    std::condition_variable cv;
    bool isComplete;         // 传输完成标记
    bool isShutdown;         // 关闭标记
};
```

### 3.3 设备发现流程

```
1. libusb_init() 初始化上下文
2. 启动定时器，定期执行 WatchUsbNodeChange()：
   a. libusb_get_device_list() 获取设备列表
   b. 对每个设备：libusb_get_device_descriptor() 获取描述符
   c. 对每个配置：libusb_get_active_config_descriptor()
   d. 对每个接口：IsDebuggableDev() 判断是否为目标设备
   e. 若是：libusb_open() → libusb_claim_interface() → 读取 iSerialNumber
   f. 将设备加入 mapUsbDevice，同时更新 DaemonMap
3. 启动 libusb 工作线程 UsbWorkThread()：
   - 循环调用 libusb_handle_events() 处理异步事件
```

### 3.4 数据传输

原版使用 **libusb 异步传输 API**：

#### 读取数据 (Bulk IN)

```cpp
PersistBuffer HostUsb::ReadUsbIO(HUSB hUSB, int exceptedSize)
{
    uint8_t *g_bufPtr = new uint8_t[MAX_SIZE_IOBUF]; // 全局/线程局部缓冲区
    HostUSBEndpoint* ep = &hUSB->hostBulkIn;
    
    ep->isComplete = false;
    libusb_fill_bulk_transfer(
        ep->transfer, hUSB->devHandle, ep->endpoint,
        g_bufPtr, exceptedSize,
        USBBulkCallback, ep, timeout
    );
    libusb_submit_transfer(ep->transfer);
    
    // 等待回调通知完成
    ep->cv.wait(lock, [ep]() { return ep->isComplete; });
    
    return PersistBuffer{reinterpret_cast<char *>(g_bufPtr), 
                         static_cast<uint64_t>(ep->transfer->actual_length)};
}

// 传输完成回调
void LIBUSB_CALL HostUsb::USBBulkCallback(struct libusb_transfer *transfer)
{
    auto *ep = reinterpret_cast<HostUSBEndpoint *>(transfer->user_data);
    ep->isComplete = true;
    ep->cv.notify_one();
}
```

#### 写入数据 (Bulk OUT)

```cpp
int HostUsb::WriteUsbIO(HUSB hUSB, SerializedBuffer buf)
{
    HostUSBEndpoint *ep = &hUSB->hostBulkOut;
    uint8_t* ptr = reinterpret_cast<uint8_t *>(buf.ptr);
    size_t size = static_cast<size_t>(buf.size);
    
    ep->isComplete = false;
    libusb_fill_bulk_transfer(
        ep->transfer, hUSB->devHandle, ep->endpoint,
        ptr, size, USBBulkCallback, ep, timeout
    );
    libusb_submit_transfer(ep->transfer);
    
    ep->cv.wait(lock, [ep]() { return ep->isComplete; });
    return ep->transfer->actual_length;
}
```

**写入重试逻辑**：如果一次写入未发送完全部数据，会自动重试：
```cpp
if (!ep->bulkInOut && transfer->actual_length != transfer->length) {
    transfer->length -= transfer->actual_length;
    transfer->buffer += transfer->actual_length;
    // 重新提交传输
    libusb_submit_transfer(transfer);
}
```

### 3.5 设备状态管理

| 状态 | 值 | 说明 |
|------|-----|------|
| `STATUS_READY` | 0 | USB 设备已连接，等待 HDC 握手 |
| `STATUS_CONNECTED` | 1 | HDC 握手完成，会话已建立 |
| `STATUS_OFFLINE` | 2 | 设备离线/断开 |

状态转换：
```
设备插入 → STATUS_READY (USB 枚举完成)
HDC 握手成功 → STATUS_CONNECTED
设备拔出 → STATUS_OFFLINE
```

设备信息存储在 `mapDaemon` 中，key 为设备的 serialNumber（connectKey）。

## 4. Daemon 端 USB 实现（设备端）

### 4.1 USB FunctionFS 路径

```
/dev/usb-ffs/hdc/ep0    # 控制端点 (EP0)
/dev/usb-ffs/hdc/ep1    # Bulk IN 端点 (设备 → Host)
/dev/usb-ffs/hdc/ep2    # Bulk OUT 端点 (Host → 设备)
```

### 4.2 初始化流程

```cpp
int ConfigEpPoint(int& controlEp, const std::string& path)
{
    struct Hdc::UsbFunctionfsDescV2 descUsbFfs = {};
    FillUsbV2Head(descUsbFfs); // 填充 USB 描述符
    
    // 1. 打开控制端点
    controlEp = open("/dev/usb-ffs/hdc/ep0", O_RDWR);
    
    // 2. 写入 USB 描述符 (FS/HS/SS 配置)
    write(controlEp, &descUsbFfs, sizeof(descUsbFfs));
    
    // 3. 写入字符串描述符
    write(controlEp, &Hdc::USB_FFS_VALUE, sizeof(Hdc::USB_FFS_VALUE));
    
    // 4. 设置系统属性，通知 USB 框架 hdc 已就绪
    SetDevItem("sys.usb.ffs.ready.hdc", "0");
    SetDevItem("sys.usb.ffs.ready", "1");
    SetDevItem("sys.usb.ffs.ready.hdc", "1");
}
```

### 4.3 数据传输

Daemon 端不使用 libusb，而是直接通过文件 I/O：

```cpp
// 写入数据 (通过 Bulk IN)
int WriteData(int bulkIn, const uint8_t *data, const int length)
{
    int writen = 0;
    while (writen < length) {
        ret = write(bulkIn, data + writen, length - writen);
        if (ret < 0) {
            if (errno == EINTR) continue; // 被中断，重试
            break; // 其他错误
        }
        writen += ret;
    }
    return ret < 0 ? ret : writen;
}

// 读取数据 (通过 Bulk OUT)
size_t ReadData(int bulkOut, uint8_t* buf, const size_t size)
{
    size_t readed = 0;
    while (readed < size) {
        ret = read(bulkOut, buf + readed, size - readed);
        if (ret >= 0) {
            readed += ret;
        } else if (errno == EINTR) {
            continue; // 被中断，重试
        } else {
            break; // 其他错误
        }
    }
    return readed;
}
```

**关键细节**：
- 写入前阻塞 `SIGCHLD` 信号，写入后恢复（防止信号中断 write）
- 遇到 `EINTR` 错误时自动重试

## 5. Rust 重写建议

### 5.1 Host 端（PC）

```rust
// Cargo.toml 依赖
[dependencies]
nusb = "0.1"  # 或 rusb = "0.9"

// 设备枚举伪代码
use nusb::{list_devices, DeviceInfo, Speed};

const HARMONY_CLASS: u8 = 0xFF;
const HARMONY_SUBCLASS: u8 = 0x50;
const HARMONY_PROTOCOL: u8 = 0x01;
const HARMONY_EP_NUM: u8 = 2;

fn enumerate_harmony_devices() -> Vec<HarmonyDevice> {
    let mut devices = Vec::new();
    for dev_info in list_devices().unwrap() {
        if let Ok(device) = dev_info.open() {
            let desc = device.device_descriptor();
            for config in device.configurations() {
                for interface in config.interfaces() {
                    for alt in interface.alt_settings() {
                        if alt.class() == HARMONY_CLASS
                            && alt.subclass() == HARMONY_SUBCLASS
                            && alt.protocol() == HARMONY_PROTOCOL
                            && alt.endpoints().count() == HARMONY_EP_NUM as usize
                        {
                            // 找到鸿蒙设备
                            let serial = read_serial_number(&device, desc);
                            let (bulk_in, bulk_out, max_packet) = find_bulk_endpoints(&alt);
                            devices.push(HarmonyDevice {
                                serial,
                                bulk_in,
                                bulk_out,
                                max_packet_size: max_packet,
                            });
                        }
                    }
                }
            }
        }
    }
    devices
}
```

### 5.2 Daemon 端（设备）

```rust
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const EP0_PATH: &str = "/dev/usb-ffs/hdc/ep0";
const EP1_PATH: &str = "/dev/usb-ffs/hdc/ep1"; // Bulk IN
const EP2_PATH: &str = "/dev/usb-ffs/hdc/ep2"; // Bulk OUT

async fn init_usb_ffs() -> io::Result<()> {
    // 1. 打开控制端点
    let mut ep0 = File::options().read(true).write(true).open(EP0_PATH).await?;
    
    // 2. 写入 USB 描述符
    let desc = build_usb_descriptor();
    ep0.write_all(&desc).await?;
    
    // 3. 写入字符串描述符
    let strings = build_string_descriptor();
    ep0.write_all(&strings).await?;
    
    // 4. 打开 Bulk 端点
    let bulk_in = File::options().read(true).write(true).open(EP1_PATH).await?;
    let bulk_out = File::options().read(true).write(true).open(EP2_PATH).await?;
    
    Ok((bulk_in, bulk_out))
}

async fn usb_read(bulk_out: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut readed = 0;
    while readed < buf.len() {
        match bulk_out.read(&mut buf[readed..]).await {
            Ok(0) => break, // EOF
            Ok(n) => readed += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(readed)
}

async fn usb_write(bulk_in: &mut File, data: &[u8]) -> io::Result<usize> {
    let mut written = 0;
    while written < data.len() {
        match bulk_in.write(&data[written..]).await {
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(written)
}
```

## 6. 数据协议

USB 传输层承载的是与 TCP 相同的 HDC 协议数据包：

```
+------------+----------------+-------------------+
|  PayloadHead (11 bytes)  | PayloadProtect (variable) |  Payload  |
+------------+----------------+-------------------+
```

USB 传输不增加额外的头部封装，直接传输序列化后的 `TaskMessage`。

## 7. 注意事项

### 7.1 权限问题（Host 端）

Linux/macOS 上访问 USB 设备需要权限：
- **udev 规则**（Linux）：添加规则文件让普通用户可访问
- **driver 替换**（Windows）：可能需要使用 WinUSB/libusbK 驱动替换原厂驱动

udev 规则示例：
```udev
# /etc/udev/rules.d/50-hdc.rules
SUBSYSTEM=="usb", ATTR{bInterfaceClass}=="ff", ATTR{bInterfaceSubClass}=="50", ATTR{bInterfaceProtocol}=="01", MODE="0666", GROUP="plugdev"
```

### 7.2 并发访问

- 每个 HDC 会话独占一个 USB 接口
- Host 端使用 `libusb_claim_interface()` 占用接口
- 需要处理多线程并发读写 USB 端点

### 7.3 超时处理

原版使用 `GLOBAL_TIMEOUT * TIME_BASE` 作为传输超时（约 15 秒）。

### 7.4 Windows 兼容性

- `nusb` 在 Windows 上原生支持，无需额外驱动（如果设备使用 WinUSB）
- 如果设备使用其他驱动（如华为原厂驱动），可能需要使用 Zadig 等工具替换为 WinUSB 驱动

## 8. 参考

- [OpenHarmony HDC 源码](https://gitee.com/openharmony/developtools_hdc)
- [libusb 文档](https://libusb.sourceforge.io/api-1.0/)
- [nusb crate](https://docs.rs/nusb)
- [USB FunctionFS 文档](https://www.kernel.org/doc/Documentation/usb/functionfs.txt)
- [ADB 协议](https://android.googlesource.com/platform/system/core/+/master/adb/OVERVIEW.TXT)
