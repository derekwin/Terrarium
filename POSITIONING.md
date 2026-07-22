Terrarium 项目定位
一句话定位：Terrarium 是一个面向 AI Agent 工作负载的轻量 VMM 与沙箱运行时，以 microVM 为隔离边界、以进程沙箱为执行单元，提供安全、弹性、可观测、可容错的 Agent 执行环境。
项目是：
一个基于 KVM + rust-vmm 生态、Rust 编写的自研 VMM（运行形态：每 VM 一个 terra-vmm 进程 + 宿主控制 daemon terra-controller）；
一个双层隔离系统：VM 层 KVM 硬件隔离是安全边界，VM 内进程沙箱（namespace + OverlayFS + cgroup v2 + Landlock + seccomp-bpf）是执行单元；
一个资源可在线伸缩的沙箱平台：CPU/内存/磁盘运行中可调，这是区别于 E2B/CubeSandbox 等固定规格沙箱的核心差异化；
一个带快照容错的执行环境：FS CoW 快照、整 VM 快照 + uffd 懒恢复、进程级 CRIU（Agent step 边界）；
对外的第一公民是沙箱（Sandbox 对象），VM 是默认对开发者隐藏的实现细节；接口形态为 Python SDK + CLI + MCP Server。
项目不是：
不是容器运行时，不是 Kata 的替代品——不解决"把容器安全化"的问题；
不是 K8s 编排器或云平台——不做多租户控制台、计费系统、集群调度（controller 只做单机资源闭环与放置，集群层是以后的事）；
不是通用云计算 VMM——不支持 PCI/ACPI/UEFI，不追求设备兼容性，只服务 Agent 工作负载；
不是 GPU 沙箱（现阶段）——架构预留 VFIO 口子但不实现；
不是 E2B 的克隆——可以参考其 SDK 体验，但 API 模型按自己的能力定义，不被 E2B 兼容性锁死。
不可违背的架构不变量（违反任何一条即为方向偏离）：
设备模型只用 virtio-mmio，禁止引入 PCI/ACPI；
资源调整走「启动预创建 + 运行调整」模型：vCPU 按上限预建 + guest 内逻辑上下线，virtio-mem 启动预挂载 + config change 调整，磁盘容量经 config change 更新；禁止依赖 guest 内核补丁（如 Dragonball upcall）；
VMM 事件循环基于 epoll（rust-vmm event-manager），不引入 tokio 等异步运行时；
代码为原创或带 Apache-2.0 attribution 的移植，禁止整体复制 Firecracker/Dragonball/Cloud Hypervisor；
unsafe 最小化且每处必须有 SAFETY 注释。