# ADR: 双层沙箱组合模型——VM 隔离底座 + sandbox 权限隔离（2026-07-29）

> 状态：草案，待批准。本 ADR 定义产品核心模型，取代同日「移除 SandboxAdapter」草案（该草案基于错误前提，已废弃）。

## 定位（先对齐）

Terrarium 是**面向 agent 场景的沙箱管理层**：

- **底层：VM（Cloud Hypervisor / KVM）= 硬隔离底座**，租户级边界；
- **上层：sandbox（sandlock / openshell）= agent 权限隔离**，会话级边界；
- **引擎的价值 = 替用户完成 VM 与 sandbox 的组合和管理**：租户 VM 的供给（预热池）、sandbox 在 VM 内的创建/执行/回收、资源声明与运行中调整。

用户视角即现有的 `Sandbox(tenant=...)`：同一 tenant 的多个 sandbox 共享一台 VM，sandbox 之间由 sandlock/openshell 做权限隔离。

## 背景与问题

2026-07 的 Sandbox 重设计方向与本模型一致，但实现把模型放在了纯客户端命名约定层：

- sandbox id `tenant-<t>/sb-<hex>` 过不了协议 `VmName` 白名单，`sandbox info/exec/kill` 端到端不可达；CLI `sandbox ls` 过滤前缀写错，恒空；
- `SandboxAdapter`（sandlock/openshell）已实现但 engine 零调用——**组合模型的上半层没接线**，且带着未使用掩盖的 bug（sandlock 空 env 时 `env_clear` 不生效，宿主环境含 `TERRA_TOKEN` 泄入沙箱；openshell `tools` 被静默忽略）；
- 引擎 session 机制未完工（`session_kill` 假成功）；
- MCP/远程面无 sandbox 概念；docs 描述了不存在的 CLI 流程。

## 决策

1. **Sandbox 是引擎级实体**。引擎维护 `SandboxRegistry`：tenant → VM（`tenant-<t>`，复用 `VmName` 校验）；sandbox id（扁平 `sb-<hex>`）→ `{tenant, workdir, backend, created_at, exec_sessions}`。
2. **协议新增 sandbox 命令族**：
   - `sandbox_create {tenant, layers/..., backend?, cpus?, memory_mb?}` → 幂等确保租户 VM 存在 → 建 sandbox → 返回 id；
   - `sandbox_exec {id, command, timeout_secs?}` → 在租户 VM 内、经 sandbox 层隔离后执行，cwd 为该 sandbox 的 workdir；
   - `sandbox_list {tenant?}` / `sandbox_info {id}` / `sandbox_kill {id}`（终止进程 + 清理 workdir，不动共享 VM）；
   - `tenant_destroy {tenant}` → destroy 租户 VM + 全部 sandbox。
3. **沙箱后端：只用 sandlock，打进系统镜像（2026-07-29 修订，已批准）**：
   - **openshell 完全放弃**：它是 CLI + Gateway + Docker/K3s driver 的控制面架构，打进每台 VM 与密度目标直接冲突；adapter 删除（git 历史留存）。
   - **sandlock 是唯一后端**：单二进制、无 daemon、~5ms 启动，恰好匹配「VM 内每会话权限隔离」。在 `terra setup` 阶段构建 musl 静态二进制（上游只发 gnu 动态版）并**打进 base 层**（`/usr/bin/sandlock`），不作为独立 tool 层。
   - **执行路径**：`sandbox exec` → engine → vsock → guest-proxy → `sandlock run <policy> -- cmd`，Landlock/seccomp 在 guest 内核生效（`CONFIG_SECURITY_LANDLOCK=y` + `CONFIG_LSM` 含 landlock，6.12 构建树 `.config` 已实证）。
   - **`SandboxAdapter` trait 保留**：作为未来更优方案的扩展点；host 侧 sandlock adapter 作为 trait 参考实现保留，但不进默认执行路径。
4. **先修后用**：guest-proxy 调用 sandlock 时遵守既有纪律——env 白名单（恒 `env_clear` + 按需回加）、stderr/stdout 不落未 drain 管道、超时 killpg。
5. **前置依赖：session_kill 诚实化**——guest-proxy exec 支持按 session killpg；做不到返回 `not_implemented`，禁止假成功。
6. **每 sandbox 独立 upperdir/workdir**（沿用现有 `upper` 机制），文件隔离由引擎保证，权限隔离由 sandbox 层保证；CPU/内存属租户 VM，resize 作用于整个租户。
7. **SDK/CLI/MCP 变薄**：`Sandbox(tenant=...)` API 形状不变，内部改 `sandbox_*` 映射；MCP 增加 sandbox 工具。
8. **止血先行（S-M0）**：扁平 id `tenant-<t>-sb-<hex>` 过白名单、修 `sandbox ls` 过滤、文档回收至代码现状。

## 备选方案

- **维持客户端命名约定**：拒绝——三个使用面已漂移，且「kill 一个 sandbox」「跨客户端可见」必须引擎参与。
- **VM-per-sandbox**：拒绝——失去共享 VM 的密度，背离产品定位。
- **sandbox 状态放 guest-proxy**：拒绝——注册表是调度依据，必须留在控制面。

## 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| S-M0 止血 | ~~扁平 id + CLI 修复 + 文档回收~~ ✅ 2026-07-29 完成（CLI 收敛：`setup`/`tool`/`sandbox` 七组命令面；`session_kill` 返回 not-implemented） | `sandbox info/exec/kill` 端到端可跑 |
| S-M1 沙箱执行层 | ✅ 2026-07-30 完成（修订：不做 host 模式，直接 guest 内）：sandlock musl 静态移植（`thirdparty/sandlock-v0.8.5-musl.patch`，24 处 libc 签名差异，已登记 PATCHES.md）；`terra setup` 阶段把 sandlock 打进系统层 `/usr/bin/sandlock`；协议 exec 加 `sandbox` 字段 → engine 透传 → vsock → guest-proxy `sandlock run` 包裹；默认策略=系统目录只读、仅 session workdir+/tmp 可写、不授予 `/`；缺失二进制硬报错不降级；池模式探测 `/workdir/usr/bin/sandlock` | ✅ 真机实测：沙箱内写 `/etc` 被 EACCES 拒绝、workdir 可写、系统可读、`sandboxed=False` 逃生口正常；池路径同样生效；e2e 12/12 + test_sandbox 16/16（含 3 个隔离用例） |
| S-M2 引擎实体 | ✅ 2026-07-30 完成：SandboxRegistry（tenant → VM、sandbox id → workdir）+ `sandbox_*` 协议族（create/exec/list/info/kill）+ `tenant_destroy` 级联 + `session_kill` 真取消（guest-proxy `kill` 命令 killpg）；SDK/CLI 映射（`sb.id` 即引擎 id `sb-<8hex>`，新增 `sandbox destroy-tenant`） | ✅ e2e 实证：租户 VM 内两 sandbox 共享 VM、workdir 互相隔离、kill 邻居不受影响、`tenant_destroy` 级联回收；e2e 12/12 + test_sandbox 20/20 |
| S-M3 全面对齐 | 网络策略 ✅ 2026-07-30：policy 透传落地（协议/引擎/vsock/SDK 同一 `policy` 对象——`read_paths`/`write_paths` 追加授予、`net_allow` 出站默认拒绝（空列表三层硬报错）、`memory_mb`/`procs` 资源限制；`sandbox_create` 存放 + `sandbox_exec` 单次覆盖 + info/ls 回显；CLI/SDK 旗标齐全）。`net_allow` 出站 ACL 已真机验证（2026-07-30，root daemon + NAT，手动探针：白名单主机 80/443 放行并收到 HTTP 跳转响应，白名单外主机与跳转目标均在 DNS 阶段被拒；另有 skip-guarded 用例 `test_net_allow_live_egress`）。**剩余**：MCP sandbox 工具 + 池化租户 VM | `tenant_destroy` 全回收，e2e 通过 |

S-M1 关键教训（实施实录）：① 上游 sandlock 只发 gnu 动态版，musl 需交叉工具链（musl.cc，免 root）+ 本地补丁；② 冷启动 VM 的 exec agent 是 **base 层里的 guest-proxy**（rootfs 构建时打进 alpine.cpio），不是 initramfs 里那个——改 guest-proxy 后必须刷新 base 层，否则旧 agent 静默忽略新协议字段，`sandbox` 旗标假生效；setup 已加 `_refresh_layer_guest_proxy` 幂等同步。

## 后续方向（记录在案，不在本 ADR 范围）

**资源动态扩缩与复用**：自动扩缩沙箱的 CPU/内存/磁盘，实现资源动态复用，提高单机虚拟环境密度。已有基础：CPU/内存 resize 已实测 100% 生效（virtio-mem）；待研究：基于负载的自动伸缩策略、内存回收（virtio-mem unplug / free page reporting）、upperdir 磁盘配额与回收、与预热池扩缩容策略联动。启动该方向前另行 ADR。

## 与既有设计的关系

- 不改变 VM 生命周期语义（停止+注销、不删数据）；sandbox workdir 清理属数据策略（临时模式 upperdir），需显式文档化；
- 协议变更同步全部分发面（引擎 dispatch、SDK、MCP）。
