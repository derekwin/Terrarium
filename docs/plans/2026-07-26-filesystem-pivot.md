# ADR: 文件系统转向 EROFS+OverlayFS+virtiofs，收缩仓库（2026-07-26）

## 决策

1. **放弃 qcow2 链式镜像与 raw 格式**，删除 `crates/overlay`。文件系统改走 EROFS 只读层 + OverlayFS 组合 + virtiofs 暴露（设计见下文）。
2. **删除 Firecracker adapter**（`crates/adapter/firecracker`）：FC 不支持 virtiofs，无法共享 page cache，与新文件系统方向不兼容。
3. **保留抽象**：`VmAdapter`/`SandboxAdapter` trait、`VmSpec.backend_config`（承载未来的层配置）、`VmCapabilities.virtio_fs` 能力位。VM 生命周期语义维持「计算与数据分离」：VM 命令永不删除数据。
4. qcow2 时代的 disk_* 命令族同步移除——未来数据层 API 围绕「层 + upperdir」重新设计，形状不同，不留半截抽象。

历史代码可从 git 历史恢复（qcow2 overlay 最后形态：见本 ADR 前一个 commit）。

## 文件系统设计（已批准）

- **三层**：系统层（EROFS+LZ4，共享）/ 工具层（EROFS+LZ4，按需组合）/ 用户层（宿主普通目录，per-VM 独立 upperdir）。
- **组合**：宿主侧 OverlayFS `lowerdir=layerN:...:python:base`（右侧优先），星型组合，非链式。
- **暴露**：virtiofsd（per-VM 进程，seccomp+landlock）+ CH `--fs` + DAX；guest initramfs mount virtiofs 后 switch_root。
- **动机**：100 VM 共享一份 Python 层，host page cache 只有一份——qcow2 链式给不了的密度。
- **快照语义**：快照只含内存状态；文件状态在 upperdir，与 VM 生命周期解耦。
- **数据策略**：临时模式（用完删 upperdir）/ 保留模式（持久复用）/ 快照模式（upperdir 打包为新 EROFS 层）。

## 三个已识别的硬问题（评审结论，实施时必读）

1. **挂载权限**：`mount -t erofs/overlayfs` 需要 CAP_SYS_ADMIN。~~MVP 决策：daemon 以 root/特权运行~~ → **已解决（2026-07-26，优于原方案）**：组合栈跑在 `unshare -Urm` 私有 user/mount namespace 里（mount + virtiofsd 同 ns，杀 supervisor 即整体回收，零特权零残留）。注意 Ubuntu 24.04 默认 `kernel.apparmor_restrict_unprivileged_userns=1` 会阻断非特权 userns，需 `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` 或以 root 运行；root 路径不受影响。
2. **热启动需要 host→guest 通道**：预热池热插 virtiofs 设备后，guest 内需 agent 执行 mount——走 vsock（guest 内核已有 `CONFIG_VIRTIO_VSOCKETS`），guest-proxy 需扩展 vsock 监听与 mount 协议。这是 FS-M4 的实质工作量。
3. **guest 内核缺口**：需补 `CONFIG_VIRTIO_FS`（M1）、`CONFIG_FUSE_DAX`（M2）、`CONFIG_HOTPLUG_PCI_ACPI`（M4）。每次改配置后必须核对 `.config` 生效（VIRTIO_MEM 被 olddefconfig 静默丢弃的教训）。

## 简化决策

- **EROFS 后置到 M3**：OverlayFS lowerdir 用普通目录即可，层先做裸目录（`$TERRA_LAYER_DIR/{base,python,...}/`），EROFS 仅作打包/分发优化。不可变语义用版本目录命名守住。
- **upperdir 用宿主普通目录**，不做 ext4 镜像；tmpfs upper 作为 ephemeral 模式候选。

## 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| FS-M1 冷启动 | ~~裸目录层 + OverlayFS + virtiofsd + CH --fs + switch_root init；内核 VIRTIO_FS~~ ✅ 2026-07-26 完成：trait FsSpec、CH adapter 组合栈（unshare supervisor）、三端 layers 参数、e2e 9/9（真实 KVM） | VM 以组合层为 rootfs 启动 |
| FS-M2 基准裁决 | DAX + cache=always；对比 qcow2 历史数据：启动时间、pip install 耗时、内存密度。**数据不赢不换默认** | docs/ 实测报告 |
| FS-M3 EROFS 打包 | ~~mkfs.erofs 工具链 + 层注册表~~ ✅ 2026-07-26：`images/build-layer.sh`（LZ4，base 8.2M→5.3M）；adapter 按名解析 `<name>` 目录或 `<name>.erofs` 镜像并自动挂载（root 走内核 loop mount，非特权走 erofsfuse fallback，`/proc/mounts` 判重，层间共享、daemon 生命周期内不卸载）；e2e 10/10 | 层镜像可构建可组合可启动 |
| FS-M4 预热池热启动 | ✅ 机制层完成（2026-07-27）：vsock 通道（guest-proxy :1024 + CONNECT 握手）+ mount/umount 命令族 + vm.add-fs/remove-device + attach_fs/detach_fs trait 与协议命令 + agent initramfs（images/build-initramfs-agent.sh）+ 内核 HOTPLUG_PCI_ACPI + shared=on 常态化。e2e test_warm_attach_detach 全链路验证（guest 内 exec 读到层内容）。**剩余**：池管理器本身（空转 VM 注册/认领/归还/扩缩容策略） | 机制已验证，池策略待做 |
