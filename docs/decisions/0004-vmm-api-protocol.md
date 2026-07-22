# 0004: vmm-api 协议——Unix socket + serde_json 文本帧

- 状态：已接受（M1 Task 2，2026-07）
- 决策者：项目所有者

## 背景

M1 Task 2 定义 controller ↔ terra-vmm 之间的 API socket 协议。
需要决定：传输层（Unix domain socket 还是 TCP）、帧格式（二进制还是文本）、
序列化方案。

## 决定

### 传输层：Unix domain socket（seqpacket 语义）

- 用 Unix domain socket（`AF_UNIX`）而非 TCP loopback：
  1. 零网络栈开销——无 TCP 握手、拥塞控制、Nagle 算法；
  2. 免端口分配与端口耗尽问题；
  3. 文件系统路径天然按 VM 实例隔离（`/run/terra/vm-<id>.sock`）；
  4. controller 与 terra-vmm 始终同宿主，不跨机通信。
- 用 `SOCK_STREAM`（字节流）而非 `SOCK_SEQPACKET`：
  Rust std 不直接支持 seqpacket；用换行分隔文本帧等效实现消息边界，
  且不引入 libc / nix 等额外依赖。
- 一次请求 = 一行 JSON，以 `\n` 结尾；响应同格式。

### 序列化：serde_json

- 选 serde_json 而非定长二进制帧的理由：
  1. **可读性**：`echo '{"cmd":"status"}' | nc -U …` 即可调试，无需专用工具；
  2. **无手写编解码**：`#[derive(Serialize, Deserialize)]` 消除编解码 bug
     （定长二进制帧须手写字节序、对齐、变长字段的边界检查）；
  3. **扩展性**：JSON 对新增字段天然向前兼容（忽略未知键），
     M2 新增命令不需改帧格式版本号；
  4. **成熟度**：serde_json 是 Rust 生态事实标准，经过充分测试与 fuzz。
- 代价：每条消息多 20~50 字节的 JSON 语法开销；M1 命令面只有 3 条命令，
  调用频率低（运维/调试场景），开销可忽略。后续若成为瓶颈可演进到二进制帧，
  JSON 与二进制帧的 tag 字段兼容（serde 的 `#[serde(tag = "...")]`
  在 JSON 和 bincode 下语义一致）。
- **新增依赖**：`serde` + `serde_json`（均为 MIT / Apache-2.0 双许可，
  与项目 Apache-2.0 兼容）。

### 命令面（M1）

```
Request:
  {"cmd":"stop"}            → 干净退出进程
  {"cmd":"status"}          → 查询运行状态
  {"cmd":"resize_mem","bytes":268435456}  → 调整内存（M1 Task 3 接入）

Response:
  {"status":"ok","data":{...}}    → 成功（可选负载）
  {"status":"error","message":"…"} → 失败
```

`resize_mem` 在 M1 Task 2 返回未实现错误；Task 3 接入真实逻辑。

### API 与 vCPU 线程的关系

- API 线程通过 `Arc<AtomicBool>` 向 vCPU 线程传递退出信号；
- vCPU 线程在主循环的每次 KVM 退出后检查该标志位；
- 进程退出时自动清理 KVM 资源（`terra-vmm` 进程生命周期 = VM 生命周期）。

## 代价与边界

- 每 VM 一个 Unix socket 文件；controller 侧须在 VM 退出后清理（或 terra-vmm
  在退出前 `unlink` 自己的 socket 文件）。
- 当前版本不支持单个 socket 上多路复用多个连接（无连接池）；仅处理单连接。
- `stop` 命令的延迟取决于最近的 KVM 退出（timer 中断 ≤ jiffy 周期，~1-10ms）。
- 若后续需要事件驱动的异步 I/O（如 vsock 数据转发），当前「逐行读 socket +
  同步响应的线程模型」可能需要演进到 event-manager 模型。
