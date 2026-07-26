# FS-M2 基准报告：virtiofs IO 性能与 DAX 结论（2026-07-26）

> 全部为真实 KVM 实测（CH v53.0、guest 内核 6.12、qemu virtiofsd 1.10、Alpine 层 94 文件 / 7.8MB）。
> 基准方法：bench initramfs 启动即挂载 virtiofs 并全量读取层文件，计时输出到 serial。

## 1. DAX 结论（推翻了原设计的关键决策）

**DAX 在 Cloud Hypervisor 已死**：v24（2022-05）标记废弃，随后移除（[CH issue #5591](https://github.com/cloud-hypervisor/cloud-hypervisor/issues/5591)）；v53 的 CLI 与 openapi 的 FsConfig 均无 `dax` 字段（实测 `dax=on` 报 `UnknownOption("dax")`）。QEMU 上游也从未合并 virtiofs DAX。

影响：**原设计「DAX 必须启用」不成立，划掉**。替代：`--cache=always`（virtiofsd 利用宿主 page cache）。

> guest 内核已补 `CONFIG_FUSE_DAX` 及依赖链（`ZONE_DEVICE`/`FS_DAX`，已验证进 `.config`）——CH 端用不了，保留无妨，未来 VMM 恢复支持即可启用。

## 2. 文件 IO 基准（3 次取代表值）

| 指标 | cache=always | cache=never | 结论 |
|---|---|---|---|
| 冷读全层（94 文件 7.8MB） | **0.15–0.17s** | 0.30–0.35s | always 快 ~2× |
| 热读全层 | **0.09–0.10s** | 0.20–0.24s | always 快 ~2.3× |
| VM 启动→基准完成 | ~640ms | ~850ms | always 稳定更快 |

注：cache=never 的热读也变快是因为 **guest 自己的 page cache** 生效（与宿主 cache 模式无关）。

**决策：virtiofsd 统一 `--cache=always`**（adapter 已如此接线）。

## 3. 内存密度（诚实版）

- 5 VM 并行读同一层：全部完成，墙钟 ~2s（两种模式无显著差异）。
- **宿主 page cache 共享实测不可证**：本机已有 226GB 暖 cache 且无 root（无法 drop_caches），51MB 新数据集写入即进 cache，磁盘读取恒为 0。需要 root 环境补测。
- 架构层面的诚实分析（无 DAX 后）：
  - **保留的收益**：宿主侧 page cache 一份共享（多个 virtiofsd 读同一份层文件，内核天然去重）；文件级粒度星型组合（qcow2 链式给不了）；层即目录，无镜像解析开销；组合即时生效。
  - **丧失的收益**：guest 侧 RAM 去重（DAX 原本要让 guest 直接 mmap 宿主页，N 个 guest 零拷贝共享）。**无 DAX 时每个 guest 的 page cache 仍会各自缓存一份**——这一点与 qcow2 相同，密度目标的实现路径必须从「DAX 去重」改为「层只读共享 + guest working set 控制」。

## 4. 裁决

virtiofs 路线**继续**（组合语义、粒度、启动速度优势成立且不依赖 DAX），但：

1. 设计文档「DAX 必须启用」修正为「cache=always 必需，DAX 不可用（CH 已移除）」
2. 「100 VM 内存一份」的口号降级：guest 侧去重不成立，密度收益限于宿主 cache 共享与镜像体积（LZ4/EROFS）
3. 密度实测定量补测需要 root 环境（drop_caches + 冷数据集）

## 复现

```
bash images/build-initramfs-virtiofs.sh          # 薄启动 initramfs
# bench initramfs：images/rootfs/init-bench 模板（启动即跑基准，serial 输出）
virtiofsd --socket-path=$S --shared-dir=<merged> --sandbox=none --cache=always
cloud-hypervisor --kernel ... --initramfs initramfs-bench.cpio.gz \
  --memory size=256M,shared=on --fs tag=rootfs,socket=$S ...
```
