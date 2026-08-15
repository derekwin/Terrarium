# 威胁模型：面向 LLM Agent 的可治理执行环境

> 目标读者：论文（USENIX Security）评审 + 产品安全评审。
> 一句话定位：Terrarium 是 **L1 真 VM 租户隔离 × L2 进程级治理** 的双层
> 执行环境；本文给出威胁模型、对抗假设与每层的可验证声明。

## 1. 场景与动机

LLM agent（编码、数据整理、工具调用）会执行不可信代码：模型输出本身、
工具返回内容中的 prompt injection、依赖链中的恶意包，都可能让 agent
执行任意命令。平台的职责不是"信任 agent"，而是**约束 agent 行为**：

1. 它能做什么（策略：文件、网络、资源）；
2. 它做了什么（审计：exec / deny / resource 事件）；
3. 它最坏能造成什么（爆炸半径：租户边界）。

## 2. 信任边界与参与者

| 参与者 | 位置 | 信任 |
|---|---|---|
| 宿主平台 | 物理机/管理面 | 可信（TCB：内核 + KVM + Cloud Hypervisor + daemon） |
| 租户 VM（L1） | KVM guest | 半可信：agent 代码可完全失控 |
| agent 进程（L2） | guest 内被约束进程 | 不可信：任意代码、root in guest |
| 其它租户 | 同宿主机其它 VM | 不可信 |

TCB 明确为：宿主内核、KVM、Cloud Hypervisor、宿主上的 daemon 与审计存储。
guest 内核与 guest-proxy 不在 TCB 内（对 L1 而言是攻击面的一部分；对
L2 而言是执行环境本身）。

## 3. 两层模型与各自的保证

### L2 —— 进程级治理（默认 terra-confine）

机制：Landlock（fs 静态默认拒绝）+ seccomp user-notify（网络白名单 /
信号保护）+ cgroup v2（memory/pids/cpu）+ rlimit（nofile）。

保证（治理面）：
- **文件系统**：未授权的路径不可读/写/执行/创建/删除/链接/重命名；
  符号链接、硬链接、rename 均不能扩大授权（Landlock 按最终路径裁决）。
- **网络**：无 Network 能力即全拒；显式白名单精确到 `host[:port]`，
  TCP connect/sendto/sendmsg/sendmmsg 与 UDP 均被裁决；非 IP 协议族
  （AF_UNIX/AF_VSOCK）fail-closed。
- **资源**：procs（cgroup pids）、fds（rlimit）、memory（cgroup
  memory.max）、cpu（weight）按策略强制。
- **治理层自身**：被约束进程无法撤销 supervisor（kill/tgkill/tkill 对
  supervisor/init 被拦截）、无法继承 seccomp 监听 fd 或审计通道 fd
  （fd 卫生）、无法伪造 deny 记录。

边界：L2 与 agent **同 uid（guest root）**，是行为约束而非强隔离；
其所有保证在"agent 运行在 guest 内"的假设下成立。L2 的任何逃逸只影响
本租户。

### L1 —— 租户隔离（VM）

机制：KVM + Cloud Hypervisor，每租户一个 VM；virtiofs 只暴露本租户的
合并层；二层隔离规则禁止 VM 间互访；NAT 出网。

保证（隔离面，即使 L2 被完全绕过）：
- 宿主文件系统、进程、设备（/dev/kvm、/dev/mem 等）不可达；
- 其它租户的 VM 与数据不可达（二层隔离 + 独立 VM 地址空间）；
- 审计数据（宿主侧 `$TERRA_HOME/audit/audit.jsonl`，0600）不可读/改；
- 跨租户唯一信道是 NAT 出网（受平台网络策略约束）。

## 4. 对抗假设（明确声明）

防御目标按优先级：
1. **默认拒绝的治理不可被 agent 绕过**（L2 完整性）；
2. **即使治理被绕过，爆炸半径限制在本租户**（L1 完整性）；
3. **审计可信**：deny 事件进入宿主侧只读存储，agent 无法伪造/抹除。

明确不在本防御范围内的：
- L2 不承诺对抗内核级逃逸（e.g. 借助 guest 内核 0-day 逃出 KVM）；该
  承诺属于 L1/KVM 的行业级安全论证，论文引用 KVM 的隔离保证而非自证。
- 网络策略是**目的地址**白名单，不审计内容；经授权端点的隧道/数据外传
  由上层（模型、编排）负责。
- 侧信道（共享页缓存、时序）不在当前声明内。
- 资源配额（带宽）不在 L2 语义内（VM 级配额，未实现）。

## 5. 默认策略的强制面（当前实现）

| 路径/区域 | 默认语义 | 备注 |
|---|---|---|
| /usr /lib /lib64 /bin /sbin /etc | 只读 | 系统目录 |
| /tmp、会话 workdir | 读写 | agent 可写区 |
| /dev | 只读（目录级放宽） | 精确到 /dev/null 的写授权暂无法表达（见下） |
| /proc /sys | 默认拒绝 | 需要时可显式 `File{Read, /proc}` 开启 |
| 网络 | 全拒 | `Network{Outbound, host:port}` 精确白名单 |
| 设备节点 | 内核 STRICT_DEVMEM | /dev/mem 仅 ISA 洞可读，RAM 区拒绝 |

已知功能取舍（产品决策项，非安全漏洞）：
- `>/dev/null` 被拒（设备授权按目录放宽为只读 /dev）；
- /proc 全拒会破坏依赖 /proc 的工具（ps、JVM 等），需显式授权；
- AF_UNIX/AF_VSOCK 一律 fail-closed（策略模型只覆盖 TCP/UDP 出网）。

## 6. 威胁列表 → 验证状态

| # | 威胁 | 层 | 验证 |
|---|---|---|---|
| T1 | 写系统文件（/etc、/root、/home） | L2 | ✅ 功能 + 对抗 |
| T2 | 符号链接/硬链接/rename 扩权 | L2 | ✅ 对抗 |
| T3 | /proc、/sys 探测 | L2 | ✅ 对抗 |
| T4 | 未授权 TCP/UDP/sendmsg 出网 | L2 | ✅ 对抗 |
| T5 | raw/ping 套接字 | L2 | ✅ 对抗（能力 + seccomp） |
| T6 | AF_UNIX/AF_VSOCK | L2 | ✅ 对抗（fail-closed） |
| T7 | fork 炸弹 / fd 耗尽 / 内存超限 | L2 | ✅ 对抗 |
| T8 | 撤销治理层（kill supervisor） | L2 | ✅ 对抗（2026-08-13 修复） |
| T9 | 继承策略 fd / 伪造审计 | L2 | ✅ 对抗（2026-08-13 修复） |
| T10 | 审计缺失/被抹除 | L1 | ✅ 对抗 |
| T11 | 跨租户网络/数据 | L1 | ✅ 对抗 |
| T12 | 宿主 fs/进程/设备 | L1 | ✅ 对抗（不可达） |

## 7. 论文叙事建议

系统的可发表贡献点不是"又一个沙箱"，而是：

1. **面向 agent 的治理语义**：默认拒绝的能力模型（文件/网络/资源/审计）
   与 L2 实现（Landlock + seccomp-notify + cgroup）的系统化对抗验证；
2. **"治理层可被撤销"问题的工程答案**：信号保护 + fd 卫生 + 确定性
   监听交接（本文档记录的修复闭环）；
3. **双层声明**：L2 是治理，L1 是边界；对比实验证明 Docker 默认
   与 gVisor 提供"隔离"但不提供"治理"，Terrarium 两者兼有且单实例
   成本落在容器与微VM 之间。

评审最可能挑战的点（提前准备）：
- "L2 同 uid，为什么不是弱点？" → 答：L2 的承诺是行为约束与审计，
  爆炸半径由 L1 兜底；论文按"治理 + 边界"两层分别论证。
- "seccomp user-notify 性能？" → 答：只拦截网络与信号 syscall，低频；
  exec 路径 p50 ≈ 5ms，fs 用内核 Landlock 零逐调用开销。
- "与 gVisor 相比隔离强度？" → 答：gVisor 用软件 syscall 拦截（用户态
  TCB），Terrarium L1 用硬件虚拟化（KVM），TCB 更小；对比实验量化
  治理面差异（Docker/gVisor 允许容器内写 /etc/passwd，Terrarium 拒绝）。
