# 0005: guest 内核功能集边界——M2 sandbox 层与 eBPF 观测

- 状态：已接受（M2 Task 0，2026-07）
- 决策者：项目所有者

## 背景

M0+M1 内核基于 x86_64 defconfig 极限裁剪，只保留 VMM 启动所需的最小功能集
（串口、virtio-mmio、blk/ext4、virtio-mem、SMP、vsock），其余全部裁掉。
M2 引入 sandboxd（guest 内沙箱守护）和 observe（guest 内 eBPF 观测），
需要回加一批内核功能。本 ADR 界定回加范围与边界。

## 决定

### 回加的功能（M2 必须）

| 配置项 | 用途 |
|---|---|
| `CONFIG_OVERLAY_FS=y` | sandbox rootfs 分层：lower=rootfs(只读) + upper=per-sandbox tmpfs |
| `CONFIG_CGROUPS=y` | sandbox 资源配额：cpu.max / memory.max |
| `CONFIG_SECCOMP=y` + `CONFIG_SECCOMP_FILTER=y` | sandbox seccomp-bpf 危险 syscall 过滤 |
| `CONFIG_SECURITY_LANDLOCK=y` | sandbox 文件路径白名单（用户态可编程 LSM） |
| `CONFIG_LSM="landlock,lockdown,yama,integrity,bpf"` | 启用 Landlock 和 BPF LSM |
| `CONFIG_BPF_SYSCALL=y` | eBPF 程序加载的基础设施 |
| `CONFIG_BPF_LSM=y` | BPF LSM——运行时可更新的动态安全策略 |
| `CONFIG_DEBUG_INFO_BTF=y` | 内核 BTF 类型信息，eBPF CO-RE 的硬依赖（需要宿主 `pahole`） |

### 确认未裁的配置

- `CONFIG_NAMESPACES=y`、`CONFIG_USER_NS=y`、`CONFIG_NET_NS=y`：
  sandbox 隔离栈（pid/mount/uts/ipc/net/user namespace）依赖这三项；
  当前裁剪片段未显式关闭它们，x86_64 defconfig 默认开启，确认不会因
  `olddefconfig` 被间接关闭。

### 仍然裁掉的功能（M2 不加）

- 网络文件系统（NFS/CIFS 等）——sandbox 内不需要
- 音频、GPU、USB ——microVM 无对应宿主设备
- 内核调试（KPROBES/KALLSYMS 等）——M2 用 eBPF，不需要 kprobe
- 模块支持（CONFIG_MODULES=n）——全部 built-in，简化 initramfs
- 电源管理、休眠 ——microVM 不需要

### ADR 0005 不改变 ADR 0001 的决定

仍然是极简设备模型（virtio-mmio only）、仍然不引入 PCI/ACPI。

## 代价与边界

- `CONFIG_DEBUG_INFO_BTF=y` 使 bzImage 体积增加约 2~4MB（BTF 数据段），
  但仍远低于 30MB 目标。
- Landlock 需要内核 ≥5.13（当前 6.12.41 满足）。
- BPF LSM 在无 BPF 程序附加时无运行时开销。
- 这些功能在 M2 版本的内核中为"运行时可用"，sandboxd/observe 未就位前
  不影响 VMM 启动行为。
