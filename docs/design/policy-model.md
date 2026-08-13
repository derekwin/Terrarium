# Agent Exec Env: 策略模型正式规范

> **归属分层**(用户确认):VM 的控制与资源定义在 `VmAdapter`;Sandbox 的控制
> 与资源定义在 `SandboxAdapter`。本规范定义两层策略对象及其在一 VM 多 sandbox
> 模型下的语义。面向 agent 执行环境的真实需求(R5 最小权限 / R10 策略可编程),
> 不迎合任何 VM/沙箱实现。

---

## 1. 两层策略与归属

```
┌───────────────────────────────────────────────────────────────┐
│ VM(租户边界,L1)— 物理强隔离 — VmAdapter / VmPolicy            │
│   资源配额: cpu / memory / 带宽 / 持久盘                      │
│   网络拓扑: 网段 / NAT / 直通                                 │
│   启动参数: 内核 / 镜像 / initramfs                           │
│   ┌─────────────────────────────────────────────────────────┐ │
│   │ Sandbox A(L2)— 逻辑能力控制 — SandboxAdapter            │ │
│   │   能力集: 文件 / 网络端点 / 设备(默认拒绝)               │ │
│   │   逻辑资源: 进程数 / 会话内存(⊆ VM 配额)                │ │
│   │   审计: 拒绝/执行/超限事件                               │ │
│   ├─────────────────────────────────────────────────────────┤ │
│   │ Sandbox B(L2)— 与 A 互不可达                            │ │
│   │   能力集独立;共享 VM 物理资源但逻辑隔离                 │ │
│   └─────────────────────────────────────────────────────────┘ │
└───────────────────────────────────────────────────────────────┘
```

**核心语义**:
1. **VM 资源 = sandbox 逻辑资源的物理上限**:任何 sandbox 的
   `ResourceLimits` 必须 ⊆ VM 配额(创建时校验,违背拒绝)。
2. **VM 提供物理隔离,不提供逻辑授权**:VM 内所有 sandbox 共享物理边界;
   授权差异由 sandbox 能力集表达。
3. **两层分工:VM 管"防恶意",Sandbox 管"防越权"**。安全边界(逃逸、
   跨租户、宿主访问)由 VM 层承担——这是 agent 代码可能恶意的防线。
   Sandbox 层(guest sandlock)不是第二条安全边界,而是**行为治理面**:
   控制 agent(即便代码合法)只能访问被授权的东西,越权被拒并审计。
   一 VM 多 sandbox 之间靠能力集无交叉 + 独立 workdir 达成任务边界,
   但这属于治理而非安全承诺。
4. **两层正交**:VM 层管"能分到多少资源",sandbox 层管"能访问什么"——
   Separation of Duty。

---

## 2. 类型规范

### 2.1 VmPolicy(VM 层,归属 VmAdapter)

```rust
/// VM 级策略:物理资源与拓扑。由平台(管理员/编排器)设定。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VmPolicy {
    /// 物理资源配额
    pub resources: VmResources,
    /// 网络拓扑(网段/NAT/带宽)
    pub network: VmNetwork,
    /// 持久化:upperdir 策略(ephemeral/persistent)
    pub storage: VmStorage,
}

pub struct VmResources {
    pub cpus: u8,
    pub memory_mb: u64,
    /// 可热扩上限(为 resize 预留 headroom)
    pub max_cpus: Option<u8>,
    pub max_memory_mb: Option<u64>,
    pub bandwidth_kbps: Option<u64>,
}

pub enum VmNetwork {
    None,                    // 无网络
    Nat,                     // NAT + DHCP(现有)
    Bridge { iface: String }, // 直通网桥（评估后 P3：对外暴露场景；当前仅 None/Nat 实现）
}

pub struct VmStorage {
    pub upper: UpperPolicy,  // Ephemeral / Persistent(name)
}
```

**归属**:`VmSpec`(现有)是 VmPolicy 的实现载体;VmAdapter::create 消费它。

### 2.2 SandboxPolicy(Sandbox 层,归属 SandboxAdapter)

```rust
/// 路径模式:精确或前缀(客体引用)
pub enum PathPattern { Exact(PathBuf), Prefix(PathBuf) }

/// 文件访问操作(最小权限粒度)
pub enum FileAccess { Read, ReadWrite, Execute }

/// 网络端点(host:port;端口省略=任意)
pub struct Endpoint { pub host: String, pub port: Option<u16> }

pub enum Direction { Outbound, Inbound }

/// 一个能力:单一访问许可(不可伪造的边界内授权记录)
pub enum Capability {
    File { path: PathPattern, access: FileAccess },
    Network { endpoint: Endpoint, direction: Direction },
    Device { path: PathBuf },
}

> **能力可用性**：`Execute` / `Inbound` / `Device` 枚举保留（协议接口面），
> 但策略校验（`SandboxPolicy::validate` / SDK `validate_policy`）一律拒绝——
> 见 §4「拒绝项的语义约定」。`ResourceLimits.bandwidth_kbps` 同样拒绝
> （属 VM 层配额，未实现）。

/// 逻辑资源配额(⊆ VM 配额)
pub struct ResourceLimits {
    pub memory_mb: Option<u64>,
    pub procs: Option<u32>,
    pub fds: Option<u32>,
    pub bandwidth_kbps: Option<u64>,
}

/// 默认访问(显式,消除实现漂移)
pub enum DefaultAccess { Deny, Allow /* 显式逃逸门 */ }

/// 审计规格
pub struct AuditSpec { pub deny: bool, pub exec: bool, pub resource: bool }

/// Sandbox 完整策略
pub struct SandboxPolicy {
    pub capabilities: Vec<Capability>,   // 能力集(默认拒绝下显式授予)
    pub limits: ResourceLimits,          // 逻辑配额(⊆ VM)
    pub default: DefaultAccess,          // 默认 Deny
    pub audit: AuditSpec,
    pub version: u32,                    // 审计可追溯
}
```

**归属**:`SandboxAdapter::create(vm, spec)` 携带它;exec 的单次覆盖走
`policy override`(继承语义,见 §4)。

---

## 3. 语义规则

### 3.1 默认拒绝(Fail-safe Defaults)

- 缺省 `default: Deny`;未在 `capabilities` 中的访问一律拒绝。
- `Allow` 仅作显式逃逸门(调试/测试),生产禁止。
- serde 缺省:策略对象反序列化时 `default` 缺省为 `Deny`。

### 3.2 最小权限(Least Privilege)

- 能力是最小粒度:`File` 区分 Read/ReadWrite/Execute;
  `Network` 区分方向与端口。
- 策略设置者(平台)只授予任务所需;agent 不可自授(能力不可由能力授予)。

### 3.3 Complete Mediation(引用监视器)

- 每次 exec / 文件访问 / 网络连接 都经策略校验,无旁路。
- 校验点:host 侧 SandboxAdapter(决策)+ guest 侧执行(强制)。

### 3.4 继承与覆盖(Policy Inheritance)

- 会话创建时:`SandboxPolicy` 存储(sandbox_create 的 policy)。
- 单次 exec:`override` 仅对该调用生效,不改变存储策略(现有语义)。
- 覆盖为**替换**:per-call 的 `policy` 整体替换存储策略(引擎 `.or()` 链:
  per-call → 存储 → 默认注入),不合并、不并集。`limits` 受 VM 配额约束
  (不可越权放大)。

### 3.5 资源上限校验(VM ⊇ Sandbox)

- 创建 sandbox 时校验:`sandbox.limits ⊆ vm.resources`
  (memory ≤ VM memory;procs/fds 无 VM 对应则仅受实现约束)。
- 违反拒绝创建——防止 sandbox 声索超 VM 物理上限的资源。

---

## 4. 实现状态（Implemented）

B 阶段已落地。**无向后兼容层**——旧 `ExecPolicy` dict（read_paths /
write_paths / net_allow）已移除，一律拒绝：

- **类型落地**：`SandboxPolicy` / `Capability` / `PathPattern` / `FileAccess` /
  `Endpoint` / `Direction` / `ResourceLimits` / `DefaultAccess` / `AuditSpec`
  落在 `crates/adapter/traits`（与 `VmSpec` 同层）。
- **线型统一**：协议 `Command.policy` 即为 `SandboxPolicy`（单一线型，无映射层）；
  引擎 `SandboxPolicy::validate()` 校验（路径绝对、端点 host 非空、port 为正、
  `default: "allow"` 拒绝）。
- **默认注入**：引擎级 `DEFAULT_SANDBOX_POLICY`（`crates/engine/src/policy.rs`）——
  沙箱化 exec 未显式给 policy 时注入，exec 恒携带完整策略（默认集从 guest
  硬编码迁至引擎级常量，跨实现语义一致）。
- **guest 翻译**：guest-proxy 按 backend 标识翻译能力集——默认
  **terra-confine**（`crates/guest/confine`，Landlock fs + seccomp 网络 +
  cgroup），alternative 为 **sandlock** 二进制。两后端均由 guest-proxy
  翻译 policy → 各自 argv（`-r/-w/--net-allow/-m/--max-procs/
  --max-open-files/--cpu-shares`）。`Execute` / `Inbound` / `Device`
  三类能力在**策略校验层**（引擎 + SDK）即拒绝，不落到 guest。

  **拒绝项的语义约定**：
  - `Execute`：Linux 上"能执行 = 能读"（执行须加载代码/解释器），由
    `Read` 隐含；不单独授权。
  - `Inbound`：VM 内监听（bind）不设限（agent 起本地服务/自测是合理
    需求）；外部入站由 VM 网络拓扑（NAT）天然隔离——`Bridge` 直通
    未实现前，"外部可达"不是可表达的能力。因此 Inbound 能力无需显式化。
  - `Device`：microVM 设备面很小；安全设备（/dev/urandom、/dev/null）
    由默认策略覆盖（/dev 只读 + 内核 STRICT_DEVMEM 兜底），危险设备
    （/dev/mem 等）不应授权；不做精确设备授权。
  - `bandwidth_kbps`（sandbox 层）：带宽属于 VM 层配额（未实现）；接口
  拒绝，避免静默不生效。
- **SDK/CLI**：SDK `validate_policy` 客户端校验 + CLI
  `--read-path/--write-path/--net-allow/--memory-mb/--procs` 直接产出新形状。
- **继承语义**：per-call policy **替换**存储策略（`.or()` 链：per-call →
  存储 → 引擎默认），非并集。
- **AuditSpec 已接线（D 阶段）**：`policy.audit.{deny, exec, resource}` 按策略
  门控结构化审计事件——`audit.exec`（沙箱 exec 完成：exit_code +
  duration）、`audit.deny`（guest 拒绝；guest-proxy 将拒绝的 exit code 改写为
  保留值 `SANDBOX_DENY_EXIT_CODE`，引擎按 exit code 判定，不嗅探 stderr
  文本）、`audit.resource`（sandbox_create 的 limits 声明 + VM resize 平台
  动作）。门控用生效策略（默认 ∪ 用户）；事件与发射点见
  `docs/protocol.md`「审计」小节。默认策略的 `audit.deny` 开启。
- **拒绝信号是结构化通道（M7）**：guest-proxy 不从子进程 stderr 推断拒绝。
  后端（confine / sandlock）把拒绝响应写成 JSON 行到 `SANDBOX_DENY_FD`
  指定 fd；guest-proxy 仅在收到记录**且** exec 非零退出时改写 exit code 为
  `SANDBOX_DENY_EXIT_CODE`。**覆盖边界（诚实声明）**：confine 的网络拒绝
  （seccomp-notify）与 sandlock 的网络/动态拒绝精确上报；confine 的静态 fs
  拒绝（Landlock 内核执行）不可观测——不产生 `audit.deny`，这是性能取舍
  （fs ~1.3× vs sandlock 4.6×）。sandlock 的 fsgrant patch 镜像静态 fs
  授权，可上报 fs 拒绝（4.6×）。

原"映射层保留协议兼容"的设计已取消——契约面即唯一实现，无兼容输入。

---

## 5. 一 VM 多 sandbox 的边界执行

| 访问 | VM 层(VmPolicy) | Sandbox 层(SandboxPolicy) | 执行者 |
|---|---|---|---|
| CPU/内存 | 物理配额 | 逻辑上限(⊆) | VmAdapter(硬)+ SandboxAdapter(逻辑) |
| 文件 | 持久盘 upper 策略 | 路径能力 | SandboxAdapter → guest-proxy → confine（Landlock）/ sandlock |
| 网络 | 拓扑(NAT；桥=P3) | 端点白名单 | VM 网络 + sandbox 出站过滤（confine seccomp / sandlock） |
| 设备 | — | 拒绝（见 §4） | — |
| 进程 | — | procs 配额 | cgroup pids（confine）/ sandlock `-P` |

**隔离保证**:
- 同 VM 内 sandbox 互不可达 = 能力集无交叉(策略)+ guest 隔离(执行)。
- 跨 VM = 物理隔离(VM 层),不依赖 sandbox 策略。

---

## 6. 验证与验收

1. **语义自包含测试**:同一 `SandboxPolicy` 序列化→反序列化→下发,
   断言能力集不变(roundtrip)。
2. **默认拒绝测试**:空能力集 + `default: Deny` → 一切访问拒绝,有 deny 审计事件。
3. **上限校验测试**:sandbox limits > VM 配额 → 创建拒绝。
4. **不支持能力测试**:`Execute` / `Inbound` / `Device` / `bandwidth_kbps`
   → 策略校验层拒绝（引擎 + SDK，fail-fast）；`default: "allow"` → 拒绝。
5. **隔离测试**:一 VM 两 sandbox,能力集无交叉 → 互不可达(e2e)。

---

## 7. 实现落点

| 项 | 落点 | 阶段 |
|---|---|---|
| 类型(`VmPolicy`/`SandboxPolicy`/`Capability`…) | `crates/adapter/traits`(与 VmSpec 同层) | ✅ B1 已落地 |
| 默认策略常量 | 引擎层(`DEFAULT_SANDBOX_POLICY`) | ✅ B1 已落地 |
| ~~旧 ExecPolicy 兼容映射~~ | —(兼容层已移除,旧 dict 拒绝) | B2 移除 |
| SandboxAdapter 携带策略 | trait 签名更新 | ✅ C 已实现（adapter/confine + adapter/sandlock） |

*本规范为 B 阶段设计基线。B(类型落地 + 协议统一)已实现,兼容层按设计取消;
C(L2 统一)独立评审后实现。*
