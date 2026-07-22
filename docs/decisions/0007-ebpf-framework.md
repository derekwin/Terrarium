# 0007: eBPF 观测框架选型——aya vs libbpf-rs

- 状态：已接受（M2 Task 3，2026-07）
- 决策者：项目所有者

## 背景

M2 Task 3 需要在 guest 内运行 eBPF 观测守护（observe），按沙箱粒度（cgroup id）
采集 syscall 计数、文件打开、网络连接、资源用量，聚合并经 vsock 上报 host。
eBPF 程序需要 CO-RE（Compile Once, Run Everywhere），依赖内核 BTF 类型信息
（`CONFIG_DEBUG_INFO_BTF=y`，M2 Task 0 已启用）。

需要选择 eBPF 框架：aya（纯 Rust）还是 libbpf-rs（Rust wrapper + C libbpf）。

## 决定

### 选 aya

| 维度 | aya | libbpf-rs |
|---|---|---|
| 依赖 | 纯 Rust，零 C 依赖 | 依赖 libbpf C 库（需交叉编译 libbpf） |
| musl 交叉编译 | 天然支持（`x86_64-unknown-linux-musl`） | 需交叉编译 libbpf → 增加构建复杂度 |
| CO-RE | 内置 BTF 重定位 | 依赖 libbpf 的 CO-RE |
| 文档/社区 | 活跃，但 < libbpf | 最成熟 |
| 许可证 | MIT / Apache-2.0 | LGPL-2.1（libbpf 本身） |

选择 aya 的理由：
1. **无 C 依赖**：sandboxd 已使用 musl 静态编译，observe 同样需要
   `x86_64-unknown-linux-musl` 目标。aya 不需要交叉编译 libbpf，
   消除整个 C 工具链依赖；
2. **CO-RE 内置**：aya 的 `aya-ebpf` 提供 `#[co_re]` 宏和 BTF 重定位，
   无需外部 `bpftool`；
3. **许可证兼容**：MIT/Apache-2.0 与项目 Apache-2.0 完全兼容。
   libbpf 的 LGPL-2.1 虽兼容但增加合规负担。

### M2 observe 架构

```
observe (guest, musl binary)
├── eBPF 程序（aya 加载）
│   ├── syscall_trace.bpf.c → 按 cgroup 计数 syscall
│   ├── file_open.bpf.c     → 记录文件打开事件
│   └── tcp_connect.bpf.c   → 记录网络连接事件
├── 聚合层（Rust）
│   ├── BPF maps 读取 (per-cgroup counters)
│   └── 定时器触发聚合
└── 上报通道
    └── vsock → host controller
```

### M2 实现策略

eBPF 程序编译和 aya 集成需要完整的 BPF 工具链（clang + bpf target）。
为降低 M2 实现复杂度，采用两步走：

1. **M2 初始版**：使用 `/proc` 文件系统（`/proc/pid/stat`、
   `/proc/pid/io`、`/proc/pid/fd`）作为数据源，实现采集 + 聚合 + 上报
   的完整管道。vsock 上报通道使用与 sandboxd 一致的 JSON 协议。
2. **M2 后续**：用等效的 eBPF 程序替换 `/proc` 采集，保持聚合和上报
   接口不变。API 兼容。

## 代价与边界

- aya 的 BPF 程序编译需要 `clang` + `libbpf` 头文件（仅编译时依赖，
  运行时不需要）。xtask 的编译步骤增加这些工具检查。
- `CONFIG_DEBUG_INFO_BTF=y` 使 bzImage 增加约 2-4MB，已在 Task 0 纳入。
- 当前 /proc 采集方案在极端负载下有采样偏差（错过短生命周期进程），
  eBPF 方案可完全解决。
