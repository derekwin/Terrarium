# L1 论证：VM 边界是隔离声明的依据

> 论文章节素材（USENIX Security 2027 Cycle 2）。回答两个问题：
> “隔离靠什么？guest 完全失控会怎样？”。核心主张：
> **L1 把爆炸半径限制在租户 VM 内——不是“不可逃逸”，而是
> “最坏情况后果有界”，依据是 KVM 硬件隔离；攻击面与不承诺项见下文。**

## 1. 我们声明什么（L1 的保证）

即使租户 VM 内的**任意代码（含 guest 内核级）**完全失控：

- 无法访问宿主文件系统、进程、设备（/dev/kvm、宿主内存）；
- 无法访问其它租户的 VM（二层网络隔离 + 独立地址空间）与数据；
- 无法访问审计数据（宿主侧 `$TERRA_HOME/audit`，0600，不在 guest 的
  任何挂载中）；
- 无法经 virtiofs 越出本租户的合并层（virtiofsd 只暴露本 VM 的
  upper/workdir）。

这由 KVM 硬件虚拟化强制执行，不是软件约定。

## 2. TCB（可信计算基）

```
宿主物理硬件（CPU 虚拟化扩展、内存控制器、IOMMU）
  └─ 宿主内核 + KVM 模块
       └─ Cloud Hypervisor（用户态 VMM，Rust，最小设备模型）
            ├─ virtiofsd（宿主侧数据面，root 运行）
            └─ 引擎 daemon（管理面，root）
```

**明确不在 TCB**：guest 内核、guest-proxy、terra-confine（L2）、agent
代码——它们全部可被 agent 控制，是攻击面的一部分，但被 L1 兜底。

TCB 大小是对比 gVisor 的关键：gVisor 的 sentry 在宿主用户态实现完整
syscall 拦截（一个大而全的 TCB）；Terrarium 的 L1 TCB 是 KVM + 微VM
（小设备模型），guest 的 syscall 直接由硬件隔离，不经用户态软件模拟。

## 3. 隔离机制与引用

### 硬件虚拟化
- KVM 使用 CPU 虚拟化扩展（Intel VT-x / AMD-V）：guest 以非特权 ring
  运行，敏感指令触发 VM-exit；EPT/NPT 二级地址转换把 guest 物理地址
  严格限制在宿主为每个 VM 分配的内存槽内——guest 无法引用未映射的
  宿主物理内存。[[Kivity et al., OLS 2007]](https://www.kernel.org/doc/ols/2007/ols2007v1-pages-225-230.pdf)
- KVM API 是稳定 ABI（v12），扩展机制向后兼容；VM 生命周期绑定 fd。
  [[KVM API docs]](https://docs.kernel.org/virt/kvm/api.html)

### 微VM 最小设备模型
- Firecracker（Agache et al., NSDI 2020）确立的微VM 安全立场：仅 virtio
  设备、无 BIOS/legacy 设备模拟、Rust VMM；威胁模型把**所有 vCPU 从
  启动即视为运行恶意代码**。[[Firecracker: Lightweight Virtualization
  for Serverless Applications]](https://www.usenix.org/conference/nsdi20/presentation/agache)、
  [[Firecracker design/threat containment]](https://github.com/firecracker-microvm/firecracker/blob/main/docs/design.md#threat-containment)
- Cloud Hypervisor 与 Firecracker 同源设计（最小设备面、无 legacy），
  但支持 hotplug/live migration/virtiofs（我们用的 virtiofs 是 CH 特有的
  数据面，见 §4）。
- **Terrarium 采用同一立场**：guest 的 vCPU 自启动即视为恶意；L2
  （confine）是治理层，不是安全边界。

## 4. 攻击面分析

guest→宿主的数据面就是**virtio 设备**（guest 驱动 virtio 队列与 VMM
交互）。Terrarium 暴露的设备：virtio-net（tap）、virtio-vsock
（guest-proxy 通道）、virtiofs（根文件系统 + workdir）、balloon、
virtio-mem。这是论文必须正面处理的攻击面：

- **virtio 设备是历史逃逸高发区**：QEMU 的 virtio-crypto 堆溢出逃逸
  （Timekiller，HITB 2023）；2026 年公开的 KVM/x86 shadow-MMU UAF 逃逸
  （Zapscape，CVE-2026-64561）。微VM 因设备面小、Rust 内存安全而收窄，
  但不为零。
- **Cloud Hypervisor 的公开逃逸类 CVE**：CVE-2026-45782（CH 首个
  escape-class）；Kata+CH 的 virtio-pmem MAP_PRIVATE 信任失败
  （CVE-2026-24834）——说明设备模型正确性仍是现实风险。
- **virtiofsd 是我们新增的宿主侧进程**：解析 guest 的 virtiofs 请求，
  当前以 root 运行——是 TCB 内最大的自定义面。硬化方向（P3）：
  virtiofsd 降权（专用 uid + 自身 Landlock/seccomp）、CH 进程降权。
- **vsock**：guest-proxy 经 vsock 与 CH 交互，是第二个数据面；已无
  legacy 串口/控制台（--serial null --console off）。

结论：L1 的保证是“逃逸需要先攻破 KVM 或 CH/virtiofsd”，这依赖
生态的持续硬化，Terrarium 不自行证明 KVM 正确性——论文表述为
“blast radius bounded by KVM hardware isolation”，并引用微VM 设备面
最小化作为缩小 VMM 攻击面的设计依据。

## 5. 侧信道

- **CPU 微架构侧信道**（Spectre/Meltdown/L1TF/MDS）：KVM 依赖宿主级
  缓解——微码、`mitigations=auto,nosmt`（L1TF/MDS 建议关 SMT）、
  L1D flush on VMENTER。Terrarium 不额外缓解，把宿主配置作为部署前提。
  [[Spectre]](https://ieeexplore.ieee.org/document/8406199)、
  [[Meltdown]](https://www.usenix.org/conference/usenixsecurity18/presentation/lipp)、
  [[Foreshadow/L1TF]](https://www.usenix.org/conference/usenixsecurity18/presentation/weisse)、
  [[RIDL/MDS]](https://www.usenix.org/conference/usenixsecurity19/presentation/van-schaik)
- **共享页缓存时序**：我们的密度卖点（跨租户共享只读 EROFS 层）引入
  读路径时序侧信道（某页是否已被缓存/被其它租户访问）。当前不承诺
  防御；这是共享层架构的固有折衷，论文明示。
- **定时/功率等物理侧信道**：超出范围。

## 6. 不承诺项（范围声明）

1. 不承诺对抗宿主内核 / KVM / CH 的 0-day（§4 的 CVE 即证据）——依赖
   生态，论文把这一点写成“TCB 外的边界由 KVM 生态负责”；
2. 不承诺对抗微架构侧信道（§5）；
3. “L1 有界”不是“L1 不可逃逸”——论文措辞用 bounded / contained，
   不用 impossible；
4. L2（confine）不承诺对抗同 uid 撤销（kill supervisor）——这是治理
   层的已知边界，L1 兜底。

## 7. 与对比基线的 TCB 对照（论文表格素材）

| 基线 | 隔离机制 | TCB 位置 | guest→host 数据面 |
|---|---|---|---|
| Docker（默认） | 共享内核 + namespaces/cgroups | 整个宿主内核 | 全部 syscall 经宿主内核 |
| gVisor | 用户态 sentry syscall 拦截 | sentry（宿主用户态，大） | sentry 解析全部 syscall |
| Terrarium L1 | KVM 硬件虚拟化 | KVM + 微VM（小设备模型） | virtio 设备队列 |

对照点：**容器逃逸 = 宿主内核逃逸（直接到 host）**；gVisor 逃逸 =
sentry 逃逸（sentry 是用户态进程，但仍需攻破一个巨大的 syscall
仿真面）；Terrarium 逃逸 = 攻破 KVM 或小设备模型。TCB 面最小。

## 8. 为论文/产品需要补的工程项

- **CH 降权**（已完成）：daemon 以 root 启动时，CH 以专用系统用户
  `terra-vmm` 运行（`terra setup` 自动创建）；tap 由 daemon 预建并
  以 fd 传入（`--net fd=N,id=net0`），CH 不再需要 CAP_NET_ADMIN 或
  `/dev/net/tun`；加上 CH 自带的 Landlock 路径域，VMM 进程变成
  “非 root + 路径受限”的受限进程（Firecracker 的“VMM 不需 root”
  原则）。restore 走 `net_fds=[net0@fd]` 同样降权。
- **virtiofsd 降权 + 沙箱化**（已完成）：以 `terra-vmm` 运行，导出树
  （per-VM upper/work/merged + 普通目录层）chown 给该用户，
  `--translate-uid/gid host:<vmm>:0:1` 保持 guest 侧 root 属主语义，
  保留 virtiofsd 自带 seccomp。guest 逃逸进 virtiofsd 只能拿到
  `terra-vmm` 的权限（该用户只拥有导出树），不再直接是宿主 root。
  现有 EROFS 镜像内容世界可读，新打包用 `mkfs.erofs --force-uid/
  --force-gid` 对准 vmm 用户。
- **宿主配置文档**：`mitigations=auto,nosmt`、微码要求、IOMMU
  （VT-d）可选启用。

降权后的效果：**CH/virtiofsd 不是宿主 root**——即使 guest 逃逸
打入这两个进程，也先要在一个专用受限用户内继续提权才能影响宿主。
这是 L1“爆炸半径有界”声明在工程上的落地。

## 9. 引用清单（提交前需逐条核验）

- Kivity et al., “kvm: the Linux Virtual Machine Monitor”, OLS 2007.
- Agache et al., “Firecracker: Lightweight Virtualization for Serverless
  Applications”, NSDI 2020.
- KVM API documentation, docs.kernel.org/virt/kvm/api.html.
- Firecracker design / threat-containment docs (github.com/firecracker).
- Kocher et al., “Spectre Attacks”, IEEE S&P 2019.
- Lipp et al., “Meltdown”, USENIX Security 2018.
- Weisse et al., “Foreshadow: Extracting the Keys to the Intel SGX Kingdom
  with Transient Out-of-Order Execution”, USENIX Security 2018.
- van Schaik et al., “RIDL: Rogue In-Flight Data Load”, USENIX Security
  2019.
- Timekiller (virtio-crypto escape), HITB 2023.
- CVE-2026-64561 (Zapscape), CVE-2026-45782 (CH escape-class),
  CVE-2026-24834 (Kata/CH virtio-pmem).
