# 0008: virtio-net 设备——异步收包模型与后端选型

- 状态：已接受（M1.5 Task 0，2026-07）
- 决策者：项目所有者

## 背景

M1.5 需要 guest 联网以支持 Ubuntu bring-up（DHCP、apt）。
设备模型仍然是 virtio-mmio（ADR 0001），不引入 PCI/ACPI。

## 决定

### 设备规格

- device_id=1（virtio-net）
- rx(0) / tx(1) 两个队列，不做 ctrl queue
- features：`VIRTIO_NET_F_MAC`(bit 5)——传输层自动附加 `VIRTIO_F_VERSION_1`
- config space：6 字节 MAC 地址（`02:54:45:52:52:41`，本地管理地址，TERRA 前缀）

### 收包模型：独立读线程

virtio-net 的收包是异步的（数据随时从网络到达，不由 guest kick 驱动），与 blk（同步请求-响应）不同。

- **独立 RX 线程**：以 `std::thread::spawn` 创建，循环 `read()` 后端 fd；
- **设备内部缓冲**：RX 线程读到的帧写入 `Arc<Mutex<Vec<u8>>>` 共享环形缓冲区；
- **virtqueue 交互**：guest 通过写 QueueNotify（队列 0）轮询收包时，
  `queue_notify` 从共享缓冲区取数据填充 guest 的 rx 描述符，置 used，返回 `ISR_USED_BUFFER`；
- **IRQ**：used buffer 自动触发 IRQ（框架已有模式，与 blk 一致）。

TX 仍是同步的：guest kick tx 队列 → `queue_notify` 从描述符链读数据 → `write()` 到后端 fd。

### 后端选型：slirp4netns（优先）

| 后端 | 优点 | 缺点 |
|---|---|---|
| slirp4netns | 免 sudo、NAT 语义、宿主已装 `/usr/bin/slirp4netns` | 性能中等（用户态 NAT） |
| TAP | 性能好、内核态 | 需 sudo 一次性建 TAP 设备 |

选 slirp4netns 为推荐起步后端。使用方式：

```bash
# 终端 1: 启动 slirp4netns（创建 tap + 网络栈）
slirp4netns --configure --mtu 1500 --disable-host-loopback <PID> tap0

# 终端 2: terra-vmm 打开 /dev/tap0 作为 --net 后端 fd
terra-vmm --net tap0 ...
```

备选：直接给 terra-vmm 传一个 Unix socket 路径（socketpair 一端给 slirp4netns，
一端给 VMM），简化 fd 传递。

### 内核配置

`CONFIG_VIRTIO_NET=y`（xtask 内核片段新增）。`CONFIG_INET` 系列
（TCP/IP/DHCP）在 x86_64 defconfig 中已默认开启。

## 代价与边界

- RX 线程额外占用一个宿主 OS 线程；M1.5 只有一张网卡，开销可忽略。
- 共享缓冲区增加一次内存拷贝（backend → 缓冲区 → guest 描述符）；
  后续可优化为零拷贝（direct descriptor fill from RX thread，需队列级锁）。
- slirp4netns 的 MTU 默认为 1500；virtio-net 的 mergeable buffers 特性
  暂不实现，单帧 ≤ 1522 字节（Ethernet + VLAN）。
