# 快照快速重置（P1）— RL/评测场景的环境回收原语

> 目标：把 `snapshot` 从"实验性平台扩展"升级为 **环境快速重置原语**——
> episode 结束不是冷启动（~850ms），而是从已知良态快照恢复（目标
> **<300ms**，热缓存下），且每次恢复得到**相同的初始状态**（确定性）。
> 这是 RL 训练/评测规模化的关键；与"崩溃容错"无关（后者维持不做）。

## 1. 现状与重新定位

现状（2026-08-03）：
- `snapshot` 命令已工作：引擎 → `VmHandle::snapshot()` → CH
  `vm.snapshot` API → 产出 `{snapshot_dir}/terra-snap-<name>.bin` + 兄弟
  `.mem`。快照路径硬编码、不可命名。
- `restore` 在契约清理（6474fa9）时被整体移除；`docs/design/agent-exec-env-
  boundaries.md` 将 snapshot 定位为"平台容错扩展，非 agent 契约"。
- CH 原生支持启动式恢复：`--restore source_url=file://<snapshot>,resume=true`
  （`resume=false` 可停在暂停态）。

**重新定位**：训练场景把快照从"容错扩展"变成 **P1 产品原语**。快照 = 一个
环境在"就绪"时刻的完整 guest 状态（内存 + 设备），restore = 从该状态创建
新 VM。每次 episode 从同一快照恢复 ⇒ 确定性重置 + 免内核启动/层挂载。

## 2. 设计决策

### D1 — restore 是"创建"，不是"恢复原 VM"

`VmAdapter::restore(snapshot, spec)` 返回一个新的 `VmHandle`（如同
`create`）：主机侧栈（overlay 组合 + virtiofsd + vsock socket + CH 子进程）
全新构建，guest 侧状态来自快照。语义与 warm pool 正交——快照是另一个
"就绪 VM 的来源"。

### D2 — VmSpec.kernel 改为可选

restore 没有 kernel/initramfs（guest 状态在快照里）。`VmSpec.kernel:
Option<String>`，`create` 仍要求 Some；CH 适配器在 restore 模式下发
`--restore` 而忽略 kernel。

### D3 — restore 的 ch_args

```
--api-socket <new> --memory <same size, shared=on> --balloon ...
--fs tag=rootfs,socket=<recomposed> --vsock cid=<new>,socket=<new>
--serial null --console off --landlock
--restore source_url=file://<snapshot.bin>,resume=true
```

CH 从快照恢复设备树，virtiofs/vsock 的 socket 由主机侧重新提供（与启动
一致）；`--cpus`/`--memory` 必须与快照时一致（引擎命令显式携带）。

### D4 — 命名快照 + 受管目录（快照目标=目录）

- CH 的 `vm.snapshot` 目标必须是一个**目录**（CH 向其中写入内存 + 状态
  文件），restore 的 `source_url` 指向同一目录。
- `snapshot` 命令支持 `name`（默认 = VM 名）→ 目录
  `{snapshot_dir}/terra-snap-{name}`。
- `snapshot_dir` 从硬编码 `/tmp` 改为 daemon 配置（SDK 传入
  `TERRA_HOME/state/snapshots`），以便跨 episode 保留。
- `restore` 命令：`{name: 新VM名, snapshot_path: <快照目录>, cpus?,
  memory_mb?, layers?, upper?, net?}`。

### D7 — 快照前暂停

CH 拒绝快照运行中的 VM（"Trying to snapshot while VM is running"）。
引擎在捕获前自动 `vm.pause`，完成后总是 `vm.resume`（失败路径也恢复）。

### D5 — 沙箱/会话语义

restore 创建的是新 VM，引擎注册表里没有旧 sandbox 记录。RL 流程：
`restore` → 重新 `sandbox_create`（mkdir -p 幂等，guest 里工作目录已在
快照中就绪）。引擎不做隐式 sandbox 迁移。

### D6 — 验收指标

同一宿主、256MB VM、热缓存下：restore 全链路（命令 → VM 注册 → guest
agent 可 exec）目标 **<300ms**，对照冷启动 ~850ms；episode 间状态一致
（确定性：快照后对 guest 的修改在恢复后消失）。

## 3. 任务拆分

### T1 — traits：restore 契约 + kernel 可选
- `VmSpec.kernel` → `Option<String>`；`validate` 与 CH `ch_args` 适配。
- `VmAdapter::restore(&self, snapshot: &Snapshot, spec: &VmSpec) ->
  Result<Box<dyn VmHandle>, AdapterError>`，默认 `not_supported`。
- 更新 mock 适配器（restore = create 的别名，记录调用）。

### T2 — CH 适配器：restore 实现
- `ch_args` 增加 restore 分支（`--restore ...`，跳过 kernel/initramfs/
  cmdline）。
- `ChAdapter::restore`：compose fs → 建 vsock/api socket → spawn CH →
  wait ready → 返回 `ChVmHandle`（复用 `spawn` 的主机侧流程）。

### T3 — 引擎：snapshot 命名 + restore 命令
- `snapshot` 支持 `name`（受管路径）；`restore` 命令构建 spec（kernel
  None）→ `mgr.restore(...)` → 注册 VM（含 vm_policy/net/pool 登记）。
- `snapshot_list` 命令（列出受管快照）。

### T4 — 协议/SDK/CLI/MCP
- protocol.md：`restore`/`snapshot_list` 行 + `snapshot` 的 `name` 参数；
  更新 agent-exec-env-boundaries.md 的定位说明。
- SDK `client.snapshot/restore`、CLI `terra vm snapshot/restore`；
  MCP 工具（后续）。

### T5 — 测试与验证
- 单元/集成：mock restore 注册 VM、ch_args restore 分支断言、
  snapshot 命名路径。
- 真实 KVM（特权容器）：snapshot → 修改 guest → restore → 验证状态回滚
  + exec 可用 + 测延迟（`manual_density_bench` 增加 `--restore` 模式或
  独立脚本）。

## 5. 验证状态（2026-08-03，真实 KVM 特权容器）

**已验证**：
- `snapshot` 全链路可用：**82ms** 捕获完整状态（`config.json` +
  `memory-ranges` 268MB + `state.json`）；快照前自动 pause、之后保持
  paused；paused 态 VM 可正常 destroy（验证通过）。
- `restore` 引擎侧可用：命令 → 适配器 → CH 启动 → VM 注册，**207ms**
  （对照冷启动 ~850ms）。

**被环境阻塞（诚实记录）**：本机 Cloud Hypervisor **v53.0** 的 CLI
`--restore` 实际不生效——传了 `--restore` 仍按全新 boot 启动（clap 还
强制要求 `--kernel`）。因此恢复后 guest 的 vsock/agent 活性无法在本环境
验证（快照内容完整，恢复路径代码就位）。CH 另有 `vm.restore` API 路径
（迁移机制），列为后续候选；最终运行时验证需在参考 KVM 宿主完成。

## 4. 明确不做（本轮）

- 崩溃容错/自动恢复（agent 可重跑，重启优于恢复）。
- 快照池（预热恢复槽位）——P3 方向，接口先留好。
- 增量快照/COW 快照——CH 全量内存快照先跑通。
