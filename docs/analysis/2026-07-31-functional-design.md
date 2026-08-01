# Terrarium 功能设计与实现完整性分析

> 基于全代码库审计(引擎/协议、SDK/CLI/MCP、guest-proxy/fs 三路验证,含 file:line 证据)。
> 结论分级:**完整** / **部分**(有缺口) / **占位**(stub) / **暴露未接线**(协议有、客户端无)。

---

## 一、整体架构设计

```
┌─ 客户端层 ─────────────────────────────────────────────┐
│  Python SDK (terra) │ CLI (terra) │ MCP (terra-mcp)    │
│  共用同一 JSON 行协议 (crates/protocol 单一事实源)      │
└──────────────┬─────────────────────────────────────────┘
               │ Unix socket / TCP+token
┌──────────────▼─────────────────────────────────────────┐
│  engine daemon — 命令分发 + VmManager(注册表)           │
│   pool_registry │ session_registry │ sandbox_registry   │
└──────┬───────────────────────┬─────────────────────────┘
       │ VmAdapter trait        │ SandboxAdapter(独立,未接线)
┌──────▼──────────────┐  ┌──────▼──────┐
│ cloud-hypervisor    │  │ sandlock    │
│ (VM 生命周期/热插/   │  │ (参考实现)  │
│  vsock 中继)        │  └─────────────┘
└──────┬──────────────┘
       │ virtiofs (fs crate: EROFS 层 + initramfs) / vsock
┌──────▼───────────────────────────────────────────────┐
│ guest-proxy (VM 内 agent) — exec/mount/kill/ping      │
│   sandlock run(默认策略 + 用户 policy)                 │
└───────────────────────────────────────────────────────┘
```

**设计意图**:硬件级隔离(KVM)+ 进程级约束(sandlock)双层;租户=VM、沙箱=VM 内会话;层文件系统共享页缓存;预热池毫秒级供给。客户端层统一走协议,引擎解耦后端(adapter traits)。

---

## 二、模块功能设计与完整性

### 1. engine daemon(核心调度器)— 完整

| 功能 | 状态 | 说明 |
|---|---|---|
| 命令分发 + 注册表 | **完整** | VmManager 已拆为 pool/session/sandbox 三个子注册表;`unregister()` 统一清理,无悬挂记录 |
| 并发 | **完整** | P2-1 后 blocking exec 在全局锁外 await,并发测试验证 |
| sandbox 生命周期 | **完整** | create/exec/kill/list/info/tenant_destroy + 池集成 + 12hex id 防碰撞 |
| 动态 resize | **部分** | CPU 热插(guest-proxy onliner 已实现,e2e 验证)+ virtio-mem 内存。**缺口**:create 未设 `max_cpus` 时无热插余量;CPU 缩容无 guest 侧 offlining 逻辑(仅 onlining) |
| 网络 | **完整** | NAT+DHCP(net crate);`net_allow` 出站策略经 guest-proxy 传给外部 sandlock 二进制(见 §4) |
| snapshot/restore | **部分/占位** | snapshot 真实(CH 内存快照,path 硬编码 /tmp,拒绝自定义);**restore 三层 stub**(engine 命令 + adapter not_supported + 无 CH client 方法)。协议暴露但无客户端、无编排 |

### 2. 协议层(protocol crate)— 完整,一处暴露未接线

- Command 25 字段全部被引擎消费(前面审计验证);`deny_unknown_fields` 严格。
- **暴露未接线**:background exec 的 `session_status`/`session_kill`/`session_list` 命令引擎实现完整,**但任何客户端(CLI/SDK/MCP)都没有对应方法**。README 声称"无客户端暴露"——实际更糟:SDK 底层 `client.sandbox_exec(exec_mode="background")` 能**启动**后台会话并拿到 session_id,但无法查询/终止(没有 session_* 方法)——"只开不管"。

### 3. Python SDK — 大部分完整,若干真实缺口

| 功能 | 状态 | 缺口 |
|---|---|---|
| Sandbox exec/files/resize/metrics/kill/policy | **完整** | `files.download()` 用 text 模式写文件,**二进制下载损坏**;`env` 参数通过 shell 前缀生效(非 guest agent env),docstring 过期 |
| Sandbox background | **暴露未接线** | 高层 `Sandbox.exec` 无 exec_mode;底层 client 有但无 session 管理方法 |
| AsyncSandbox | **部分** | 缺 `vm`/`tenant`/`policy`/`pool_backed`/`destroy_tenant` 属性与方法(与 Sandbox 面不一致) |
| Pool.grow / CLI pool scale | **部分(bug)** | `grow(n)` 传 `self._size`(增量后的总数)而非 delta → **过度供给**(3+1 变成加 4 台);`pool scale` 同病,还丢弃 kernel/net |
| Template.build | **完整** | 有本地 daemon 约束(upperdir 在 daemon 主机),文档已注明 |

### 4. guest-proxy(VM 内 agent)— 完整,一个死接口

- 5 命令(exec/kill/mount/umount/ping)全实现;exec 模型完整(process group、timeout killpg、10MB 输出上限、reader 线程防管道死锁);exec_id 注册表(registry.rs)完整。
- **死接口**:`/tmp/sandboxd.sock`(guest 本地 unix socket)被绑定但**仓库内无任何客户端连接**——只有 main.rs 绑定,无使用者。
- **net_allow 委托**:repo 内只做 flag 透传(`--net-allow`)+ 非空校验;实际出站限制由**外部 sandlock 二进制**(multikernel/sandlock v0.8.5,seccomp/network 引擎)执行。**无 e2e 测试证明真实拦截**(docs/sdk.md:151 自认"尚未在带 NAT 环境实测")。
- 不共享 protocol 类型:guest-proxy 手写 JSON(与 host 端 adapter 的 guest_cmd 一致,双层手写)。

### 5. fs crate(层文件系统)— 完整,两个小设计瑕疵

- layer 生命周期(build/list/remove/resolve + EROFS 自动挂载 + mtime 重建检测)完整;initramfs builders(agent/virtiofs)完整且 e2e 验证可启动 guest-proxy。
- **瑕疵 1**:PyO3 `resolve_layer` 每次调用新建 `mounted_layers` 集合——Python 侧跨调用挂载缓存不共享(仅 Rust adapter 路径经 Arc 共享)。功能不破,但缓存失效。
- **瑕疵 2**:`resolve_layer` 重建触发 umount 后,若集合仍含该名,会返回未挂载路径(rebuild-while-running 边界)。

### 6. MCP — 完整(按设计),管理面刻意缺席

- 15 工具全映射到已实现引擎命令;会话式 exec + 自愈;`net_allow` 默认沙箱化。
- **刻意不暴露**(mcp.md 声明):sandbox 管理(sandbox_list/info/kill)、tenant_destroy、pool_create、net_*、snapshot/restore、session_*(后台)、daemon_stop——这些归平台 CLI。

### 7. CLI — 完整命令面,8 个 reserved 标志

- 完整:sandbox/vm/pool/tool/net/daemon/setup 各子命令。
- **reserved(解析但未使用)**:`sandbox create --name/--disk/--backend`、`sandbox exec --detach/--follow`、`pool create --name`、`pool claim/scale` 的位置参数、`net create --name`。
- **无** session_*/snapshot/restore 子命令。

---

## 三、核心功能完整性总表

| 功能 | 引擎 | 协议 | SDK | CLI | MCP | 综合 |
|---|---|---|---|---|---|---|
| VM 生命周期 | ✅ | ✅ | ✅ | ✅ | ✅ | **完整** |
| Sandbox 会话(含池集成) | ✅ | ✅ | ✅ | ✅ | ✅ | **完整** |
| 分层文件系统(EROFS+virtiofs 热插) | ✅ | ✅ | ✅ | ✅ | ✅ | **完整** |
| 预热池(claim/release/readiness) | ✅ | ✅ | ✅ | ✅ | ❌(无工具,刻意) | **完整** |
| 双层隔离(sandlock 默认策略) | ✅ | ✅ | ✅ | ✅ | ✅ | **完整**(net_allow 拦截未实测) |
| 动态 resize(CPU/内存) | ✅ | ✅ | ✅ | ✅ | ✅ | **部分**(缩容无 guest offlining) |
| 后台 exec 会话 | ✅ | ✅ | ⚠️ 只开不管 | ❌ | ❌ | **暴露未接线** |
| snapshot | ⚠️ | ✅ | ❌ | ❌ | ❌ | **部分**(无 restore/客户端) |
| restore | ❌ stub | ❌ | ❌ | ❌ | ❌ | **占位** |
| 网络 NAT+DHCP | ✅ | ✅ | ✅ | ✅ | ❌(刻意) | **完整** |
| 池扩容 | ✅(底层) | ✅ | ⚠️ bug | ⚠️ bug | ❌ | **部分** |

---

## 四、真实缺口清单(按价值排序)

1. **restore 全栈 stub** — 协议文档已诚实标注"not implemented";快照无恢复=灾难恢复能力缺失(Roadmap"snapshot fault tolerance"未做)。这是最实质的功能缺口。
2. **后台会话"只开不管"** — SDK 能启动不能管理;补齐 `session_status/kill/list` 客户端方法(或从 SDK 面撤回 exec_mode)即可闭环。
3. **Pool.grow / pool scale 过度供给 bug** — 传总数而非增量;一行修复(delta)。
4. **net_allow 无实测** — 拦截逻辑在外部二进制,repo 内无 e2e 证明;至少加一个真实 egress 拦截测试(需 root daemon)。
5. **CPU 缩容无 guest offlining** — 上行 onliner 只会 online;缩容语义未实现。
6. **AsyncSandbox 面不一致** — 补 4 属性 + destroy_tenant 或文档声明不支持。
7. **CLI 8 个 reserved 标志** — 要么实现(如 --name、--detach)要么移除(避免误导)。
8. **files.download 二进制损坏** — text mode → 二进制安全(binary mode)。
9. **guest-proxy 死接口** `/tmp/sandboxd.sock` — 删除或补客户端。
10. **文档 drift** — `assets.ensure_engine()/publish_engine()` 不存在、`terra.configure` 未导出、`duration_ms` 不存在、`env` docstring 过期。

## 五、刻意未做(设计决策,非缺口)

- SandboxAdapter/sandlock adapter 不接入引擎(guest 侧沙箱是正确形态,adapter 是参考实现)
- MCP 无管理面工具(归平台 CLI)
- daemon TCP token 明文(文档注明仅限可信网络)
- snapshot 自定义路径拒绝(防误用,restore 落地后再放开)
