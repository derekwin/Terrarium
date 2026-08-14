# 对抗评估与基线对比（2026-08-13）

> 真实 KVM 宿主机实测。目标：把安全验证从"功能验证"升级为"对抗评估 +
> 同负载对比"，为论文（USENIX Security Cycle 2）与产品准入提供证据。

## 1. 本次产出

- 对抗套件：`sdk/python/tests/test_adversarial_isolation.py`（31 项，真实
  KVM）；静态逃逸探针：`sdk/python/tests/adversarial/probes/escape_probe.c`
  （fd 扫描 / TCP / UDP / sendmsg / AF_UNIX / AF_VSOCK / raw / ping /
  bind / fds / mem / fork）。
- 基线对比 harness：`sdk/python/tests/compare_baselines.py`；结果：
  `docs/baseline-compare-2026-08-13.json`。
- 威胁模型：`docs/security-threat-model.md`。

## 2. 对抗评估发现并修复的 4 个真实问题

套件不是走形式：第一轮真机运行抓到了 4 个问题，其中 3 个是安全相关
的缺陷，全部修复并纳入回归。

### 2.1 seccomp 监听 fd 交接竞态（网络治理不可靠）

**现象**：terra-confine 的子进程安装 seccomp filter 后，把监听 fd 编号经
管道传给父进程，父进程用 `pidfd_getfd` 复制。监听 fd 在 exec 时被
CLOEXEC 关闭，子进程执行很快时父进程拿到 EBADF/ESRCH——filter 已装但
无人监听，网络 syscall 退化为 ENOSYS，且无 deny 审计。竞态是随机的，
功能测试偶发通过掩盖了问题。

**修复**（`crates/guest/confine/src/supervisor.rs`）：父进程确认复制成功
前，子进程阻塞在 go 管道上；复制失败则杀掉子进程 fail-closed，绝不让
"无监听者的 filter"继续运行。

### 2.2 治理层可被 `kill -9 $PPID` 撤销

**现象**：被约束进程与 supervisor 同 uid（guest root），`kill -9 $PPID`
可直接杀死治理层（supervisor 一旦死亡，后续网络 syscall 无审计地失败，
审计与结构化裁决全部丢失）。

**修复**：BPF 增加 kill/tgkill/tkill 拦截，supervisor 裁决——目标为
supervisor 自身、init（pid 1）或 supervisor 所在进程组时拒绝（EPERM +
deny 审计），其余信号（对自身子进程）放行。实测：kill 被拦、沙箱继续
受治理。

### 2.3 memory.max 单位错误（64MB 限制变成 64 字节）

**现象**：`limits.memory_mb` 把兆字节原样写入 cgroup v2 `memory.max`
（单位是字节）——64MB 限制实际是 64 字节，进程一进 cgroup 立即被 OOM
杀，表现为 `read notif fd: short read or error`，子进程无声死亡。

**修复**（`crates/guest/confine/src/cgroup.rs`）：按字节换算。实测：64MB
限制下分配 256MB 失败（ENOMEM），8MB 正常。

### 2.4 策略 fd 泄漏进被约束进程

**现象**：guest-proxy 的 vsock 套接字（agent 通道）与 deny 管道读端没有
CLOEXEC，被继承进 terra-confine 再进入被约束命令：进程内可见 fd 63
（deny 管道写端，可伪造审计记录）、vsock 通道套接字、deny 读端。

**修复**：
- vsock 监听/接受 fd 显式置 CLOEXEC（`crates/guest/proxy/src/vsock.rs`）；
- guest-proxy pre-exec 关闭 deny 管道两端原件（`sandbox.rs`）；
- terra-confine 子进程 exec 前关闭 fd 63、给监听 fd 显式置 CLOEXEC。

实测：被约束进程只剩 0/1/2 三个 fd，`>&63` 报 Bad fd number。

## 3. 对抗套件结果（31 passed）

真实 KVM、ubuntu 层、confine 后端（2026-08-13）：

| 面 | 覆盖 | 断言 |
|---|---|---|
| L2 fs | 写 /etc、符号链接、硬链接、rename、/proc、/sys、/dev/mem 偏移 | 拒（EACCES/ENOENT） |
| L2 fs 正例 | /proc 显式授权后可读 | 放行 |
| L2 网络 | TCP/UDP/sendmsg 默认拒、白名单精确 host:port、AF_UNIX/AF_VSOCK fail-closed、raw/ping 拒、bind 放行（入站不受 L2 约束） | 拒（结构化 200）/ 精确放行 |
| L2 资源 | fork 炸弹（procs）、fd 耗尽（nofile）、内存超限（cgroup） | 拒 |
| L2 治理完整性 | kill supervisor 被拦、无策略 fd 继承 | 拒 |
| L1 爆炸半径 | 宿主 fs 不可达、审计文件不可达、无 /dev/kvm、pid 数正常、跨租户不可达 | 拒 |
| 审计 | deny 事件可查、deny 通道不可伪造 | 通过 |

## 4. 基线对比（同负载，真实本机）

同一批命令在 5 个运行时的结果（`docs/baseline-compare-2026-08-13.json`）：

| 负载 | bare | docker-default | docker-hardened | gVisor | Terrarium |
|---|---|---|---|---|---|
| 写 /etc/passwd | 拒(用户权限) | **放行**(容器 overlay) | 拒(只读) | **放行**(overlay) | 拒(Landlock) |
| 写 /root/x | 拒 | **放行** | 拒 | **放行** | 拒 |
| 读 /etc/shadow | 拒(非 root) | 放行(容器内) | 放行(容器内) | 放行 | 放行(guest 内) |
| /dev/mem RAM 区 | 拒 | 拒 | 拒 | 拒 | 拒 |
| /proc/kcore | 拒 | 拒 | 拒 | 拒 | 拒 |
| 出网 TCP | 放行 | **放行**(bridge) | 拒(no-net) | 拒(no-net) | 拒(默认策略) |
| 出网 UDP | 放行 | **放行** | 拒 | 拒 | 拒 |
| raw 套接字 | 拒(非 root) | **放行**(NET_RAW) | 拒 | 拒 | 拒(seccomp) |
| CapEff 可读 | 放行 | 放行 | 放行 | 放行 | **拒**(/proc 默认拒) |
| /proc pid 可见 | 放行 | 放行 | 放行 | 放行 | **拒** |

读法（诚实）：
- Docker 默认与 gVisor 给的是**隔离**：容器内 root 可以改 /etc/passwd、
  开 raw socket、出网，但改动落在容器自己的 overlay/命名空间里；
- Terrarium 给的是**治理**：同一操作被策略直接拒绝（共享层不可污染、
  网络白名单精确、raw 不可用），且 /proc 默认不可探测；
- "读 /etc/shadow 放行"在三者中语义相同：读的是各自隔离环境内的影子
  文件，不是宿主的。

## 5. 成本对比（同宿主实测）

| 指标 | bare | docker | gVisor | Terrarium |
|---|---|---|---|---|
| 冷启动到首 exec | ~1.2 ms | ~420 ms | ~176 ms | ~1070 ms |
| 稳态 exec 延迟（p50） | ~0.8 ms | ~78 ms（docker exec） | n/a（do 无 exec） | **~4.7 ms** |
| 单实例宿主内存 | — | ~0.6 MB（sleep） | ~34 MB（Sentry RSS） | ~71 MB（CH+virtiofsd RSS） |

读法：Terrarium 的 exec 延迟显著低于 docker exec（agent 高频工具调用
场景的关键指标）；单实例内存高于容器但远低于"每 agent 一台裸 VM"的
传统方案，且密度基准（docs/benchmarks.md）显示层共享后增量成本 ~52MB。
gVisor 内存数字是 Sentry 进程 RSS，需注意其 syscall 拦截的用户态 TCB
与 KVM 硬件边界的本质差异。

## 5.1 规模化密度对比（100 实例，2026-08-14）

`sdk/python/tests/density_compare.py` 在同一宿主上各起 100 个长驻实例，
测创建速率、宿主内存与并发 exec 吞吐（`docs/density-compare-2026-08-14.json`）：

| 基线 | 100 实例创建 | 创建速率 | 单实例宿主内存（RSS） | 聚合 exec 吞吐 |
|---|---|---|---|---|
| Terrarium（冷启动） | 14.3 s | 7.0/s（8 并发） | **67.5 MB**（CH+virtiofsd） | **1869 exec/s** |
| Terrarium（快照恢复） | 1.9 s | **53.2/s**（64 并发，含一次性基 VM+快照） | **62.6 MB** | **2117 exec/s** |
| Terrarium（快照恢复·预置快照） | 0.67 s | **148.2/s**（64 并发，稳态） | **62.8 MB** | **2219 exec/s** |
| Docker | 28.8 s | 3.5/s | 0.6 MB（sleep 容器） | 173 exec/s（docker exec） |
| gVisor | 1.6 s（到 sentry 就绪） | 65/s | **84.4 MB**（runsc+gofer+sentry） | n/a（do 无 exec 通道） |

读法（对论文最有用的一张表）：
- **gVisor 的每实例宿主成本（84.4 MB）高于 Terrarium（67.5 MB）**：runsc
  do 每沙箱是 runsc wrapper + gofer + sentry 三个进程；Terrarium 是 CH +
  virtiofsd 两个进程。加上 exec 延迟（~5ms vs 无 exec 通道）与 TCB
  （用户态拦截 vs KVM 硬件边界），"gVisor 更轻"的常见假设在本负载下
  不成立。
- Docker 每实例内存极低（0.6MB），但 exec 延迟 ~78ms（docker exec 客户端
  往返）且默认无治理（root 可改 /etc/passwd、开 raw socket）。
- **Terrarium 冷创建 7.0/s，是 docker 的两倍**（2026-08-14 锁修复后，
  见下）；聚合 ~1870 exec/s；暖池（`Pool.acquire`，docs/benchmarks.md）
  把认领降到 ~18ms，是规模化 RL/CI 场景的既定路径。
- **Terrarium 快照恢复创建 53.2/s，反超 gVisor**（同轮 40.8/s）：一份
  基 VM 快照（~1.3s 一次性成本）后，100 个环境从快照并行恢复 + 绑定
  sandbox 仅 1.9s。这是 P1 快照快速重置在"创建"语义上的直接兑现；
  手动拆解 restore+bind 在 64/128 路下实测 176/183 VMs/s（暖页缓存）。
- **预置快照（生产形态）稳态 148 VMs/s**（`--snapshot-path` 复用一份
  快照，跳过一次性基 VM 引导）：100 环境 0.67s，gVisor 的 3.3 倍。
  快照是固定资产（常驻页缓存），因此这是部署后实际看到的创建速率。

**密度扫描暴露并修复的真实缺陷**：`tap_name` 把 VM 名截断到 9 字符，
同前缀租户（如 `tenant-dens-0..7`）全部落到同一个 tap 设备名上，第二个
起的 VM 的 CH 以 `Resource busy` 启动失败——批量创建被间歇打断。修复
（`crates/adapter/cloud-hypervisor/src/process.rs`）：tap 名改为
`<前4字符>-<16bit FNV-1a 摘要>`，15 字符内保持唯一。另修复 daemon
keep-alive wrapper 在瞬时心跳失败时自杀的问题（连续 8 次失败才退出），
100 实例并行创建下服务 daemon 保持存活。

**2026-08-14：sandbox_create 锁串行化修复**——并发创建从 0.95/s 提升到
7.0/s（8 并发，100 实例实测）。根因：`sandbox_create` 全程持有 manager
锁（VM 启动 + agent 就绪 + workdir 都在锁内），并发客户端全部排队；
而 `vm_create` 早已是锁外 spawn（并行 18.8 VMs/s）。修复把
`sandbox_create` 拆成"锁内准备 + 锁外 spawn/bind + 重入注册"
（`crates/engine/src/daemon.rs`、`commands/sandbox.rs`），并修掉了
过程中发现的 tokio Mutex 不可重入死锁。暖池路径不变（~18ms）。

**2026-08-14：快照恢复创建 + 高并发健壮性**——
- 恢复路径的真实缺陷：CH restore 从快照 config.json 重挂设备，但 net
  设备的 tap 仍指向基 VM 的 tap（Resource busy，所有带网恢复失败）；
  `prepare_restore_dir` 现在把 tap 一并改写为恢复 VM 自己的（vsock/fs
  早已如此）。
- 高并发下 daemon 可被饿死：每次 VM 启动的 `ip`/`ebtables` 子进程同步
  阻塞 tokio worker；NAT 桥初始化改为每 daemon 一次（缓存），网络设置
  包进 `spawn_blocking`。另修复 SDK 在 daemon 忙时静默起内嵌 daemon
  抢占 socket 的问题（socket 存在但不响应时明确报错，不再自动接管），
  并把 keep-alive wrapper 的心跳容错从 8 次放宽到 30 次。

**daemon 偶发死亡的排查结论**：历史上观察到一次负载后 daemon 无响应
（accept 循环饿死 → wrapper 退出），后续 4 轮重负载（100 实例快照扫描
+ 空载 90s + 完整 e2e×2 + 128 路并发）均未复现，daemon 全程存活。根因
指向异步运行时上同步子进程调用（已用桥缓存 + spawn_blocking 缓解）；
wrapper 退出前现在会写诊断日志（30 次连续心跳失败），下次发生可立即
定位是"饿死退出"还是"daemon 线程崩溃"。

## 6. 已知功能取舍（产品决策项）

对抗套件同时固定了当前"默认拒绝面"的真实边界，其中三项是功能取舍而非
漏洞：

1. **`>/dev/null` 被拒**：设备授权按目录放宽为只读 /dev（Landlock 路径
   规则无法精确到字符设备）。影响：agent 常用重定向写法会失败。
   候选解法：guest 内提供可写的 /dev/null 替代（如 tmpfs 挂载）、或
   把 /dev/null 语义改由 busybox 内置处理——需产品决策。
2. **/proc、/sys 默认全拒**：安全上有利（不可探测 pid/内核），但 ps、
   JVM 等依赖 /proc 的工具需要显式 `File{Read, /proc}` 授权（能力模型
   已验证可用）。
3. **AF_UNIX/AF_VSOCK fail-closed**：策略模型只覆盖 TCP/UDP 出网，本地
   IPC 一律拒绝；如未来 agent 需要本地 IPC，需扩展策略类型。

## 7. 对论文的含义

- 对抗评估把"我们默认拒绝"从口头声明变成了 31 项可复现测试 + 4 个真实
  缺陷的修复闭环——这是 artifact 与相关工作对比的硬证据；
- 基线对比给出论文核心对照：**隔离 ≠ 治理**；Terrarium 在同一负载下
  提供两者，且 exec 延迟（~5ms）支撑 agent 高频调用的真实场景；
- 下一步补强：同负载下的密度扩展对比（100 实例）、把对抗矩阵接入 CI
  （`run_e2e.sh` 已纳入）、以及 L1 侧 KVM 隔离的引用论证。
