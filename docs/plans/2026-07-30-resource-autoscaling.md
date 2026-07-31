# ADR: 资源动态扩缩与复用——机制定案与里程碑（2026-07-30）

> 状态：草案，机制调研已完成（CH v53.0 源码级验证），待 R-M1 实测后转正式。

## 目标

在「VM = 租户、Sandbox = 会话」的双层模型上，实现 CPU/内存/磁盘的动态伸缩与复用，提高单机虚拟环境密度。核心矛盾：**声明的资源 ≠ 实际使用的资源**，空闲租户 VM 和池内预热 VM 占着内存不放。

## 前提事实（已盘点）

- CH v53.0；guest 内核 6.12 已含 `VIRTIO_MEM`/`VIRTIO_BALLOON`/`CGROUPS`/`MEMCG`/`CPUSETS`；
- adapter 已有 `vm_resize`(cpus/ram)、`vm_balloon`、`vm_resize_disk` 客户端方法；CPU/内存 resize 向上已实测 100% 生效；
- 文件系统是 virtiofs（无块设备），per-VM 可写层 = host 目录 upperdir；
- 所有 VM 以 `shared=on` 运行（virtiofs 需求）——这曾是内存回收的最大不确定项，调研结论：**不是障碍**（见下）。

## 机制定案（CH v53.0 调研结论，含验证状态）

### 内存缩容——三条互补路径

1. **virtio-mem unplug（耐用缩容，主力）**：`vm.resize desired_ram` 可向下（ACPI 法不行，必须 `hotplug_method=VirtioMem`）。2MiB 块粒度；best-effort（guest 有不可移动页时部分成功——对**空闲/池内 VM 可靠**，对高压 VM 不保证）。关键发现：CH 把 `shared=on` 内存落在 **memfd** 上，unplug 时 `fallocate(PUNCH_HOLE)` 立即归还 host——**与 virtiofs 共享内存兼容**（QEMU 直接拒绝此组合，不可跨 VMM 推广）。（源码验证，置信高）
2. **virtio-balloon inflate（页粒度快速回收，压力响应）**：`vm.resize desired_balloon` 由 host 主动充气，guest 让出空闲页，CH 立即 punch-hole 回收；放气后 guest 懒回收。缺点：充气页碎片 guest 内存，对 page-cache 重的 guest 会挤压（内核已知问题）——适合**临时性**回收，不适合持久缩容。（源码验证，置信高）
3. **free page reporting（被动常开回收，默认开启）**：`--balloon size=0,free_page_reporting=on`——guest 释放页后主动上报，VMM 通知 host 丢弃，无需 balloon 充气、无需控制器介入。解决"guest 已释放但 host 不知道"的核心盲区。CH v22.0 起支持，与 shared=on 兼容。（源码验证，置信高）

### CPU 缩容

`vm.resize desired_vcpus` 向下 = vCPU 热拔，guest 内自动 offline（向上才需 guest 手动 online）。注意：拔除是异步的，未完成时 API 返回 429，控制器需重试退避；下限 1 vCPU。（文档验证，置信高）

### 磁盘

`vm.resize-disk` 只适用 virtio-block raw 镜像（v50+)，与本架构无关（adapter 里该方法是死代码，择机删除）。**磁盘伸缩 = upperdir 配额与回收**：候选为 ext4/xfs project quota（按 upperdir 目录计费）、超限 GC 策略。属后期里程碑。

### 可观测性（诚实盘点：比预期弱）

- CH API 只有 `vm.info.memory_actual_size`（逻辑值 = 总量 − balloon + virtio-mem plugged），**不暴露真实 RSS**；`vm.counters` 无内存计数；balloon 无 STATS_VQ。
- **真实 RSS 只能 host 侧自采**：`/proc/<ch-pid>/status` VmRSS 或 smaps_rollup。
- guest 内压力信号（MemAvailable、loadavg）可**复用现有 exec 通道**采集（guest-proxy `cat /proc/meminfo`)，v1 无需新协议。
- 交叉验证逻辑值与 RSS 即得回收效率。

### 附加杠杆：KSM

`--memory mergeable=on` 开启 KSM 候选——同 base 的 VM 间匿名页去重（共享库、解释器堆）。与 FPR 互补。R-M1 一并实测收益。

## 设计决策

1. **FPR 默认开启**：adapter VM 配置加 balloon 设备 `size=0,free_page_reporting=on`（零成本被动回收，无副作用）。hugepages 保持关闭（开启会断 madvise 回收路径）。
2. **池 VM「启动声明上限、空闲缩到地板」**：池 VM 以 `hotplugged_size` 带足上限启动（预热池语义不变），池管理器对空闲 VM 做 virtio-mem unplug 到地板尺寸（如 256MiB),claim 时扩回。这是「资源动态复用」的核心动作。
3. **租户 VM 闲时回收**：控制器水位策略——RSS/逻辑比 +（可选）guest MemAvailable 低于阈值 → balloon 充气回收临时富余；持续空闲 → virtio-mem unplug 降档 + vCPU 降到 1。恢复压力时反向扩。
4. **指标采集在引擎**：daemon 内后台任务周期采 `/proc` RSS + `vm.info` 逻辑值（+可选 guest 探针），不引入新组件；`Sandbox.metrics()` 从「回显声明值」升级为真实使用值。
5. **磁盘走 upperdir 配额**，与 CH API 无关，独立后期里程碑。

## 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| R-M1 机制实测 | 真实 KVM 上验证：virtio-mem unplug 回收率（RSS 前后对比）、balloon inflate 回收率、FPR 开启后的被动回收、CPU 热拔、KSM 去重收益（同 base 两 VM)。**数据不赢不启用** | docs 实测报告，给出回收率数字 |
| R-M2 FPR 默认化 | adapter 支持 balloon 设备配置（`size=0,free_page_reporting=on` 默认）；删除死代码 `vm_resize_disk` | 新 VM 默认带 FPR;e2e 全绿 |
| R-M3 池资源地板 | 池 VM hotplugged 启动 + 空闲 unplug 到地板 + claim 扩回 | 池内 N 台空闲 VM 的 RSS 总量显著低于声明总量（实测数字） |
| R-M4 指标与控制器 | RSS 采集 + 水位策略（balloon/unplug/vCPU 降档）+ metrics 真实化 | 空闲租户被回收、压力恢复时扩回；e2e 断言 |
| R-M5 磁盘配额 | upperdir project quota + GC（另行细化） | 超限 sandbox 写失败，余量可见 |

## 风险与对策

- **unplug 部分失败**（不可移动页）：只对空闲 VM 做耐用缩容，高压 VM 走 balloon；失败率纳入 R-M1 实测。
- **balloon 挤压 page cache**：水位留余量，充气比例保守（参考业界 ~25% 起步）。
- **429 竞态**（CPU 拔除异步）：控制器串行化单 VM 的调整动作。
- **guest 内核 movable zone 配置未验证**（`MEMORY_HOTPLUG_DEFAULT_ONLINE` 等）:R-M1 第一项检查 `.config` 并实测 unplug 成功率。

## 范围外

快照/恢复与资源伸缩的联动、跨机调度、cgroup 级会话配额（sandlock `memory_mb`/`procs` 已覆盖会话层）。
