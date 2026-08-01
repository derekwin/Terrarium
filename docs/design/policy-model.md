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
3. **一 VM 多 sandbox 的隔离 = 物理(VM 内机制)+ 逻辑(能力集无交叉)**:
   能力互不交叉是策略层保证,VM 内隔离(guest sandlock)是执行层保证。
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
    Bridge { iface: String }, // 直通网桥
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
- 覆盖为**替换或并集**?——**并集语义**:override 的能力追加到存储能力集
  (现有 read_paths 追加模型),但 `limits` 覆盖为取小(不可越权放大)。

### 3.5 资源上限校验(VM ⊇ Sandbox)

- 创建 sandbox 时校验:`sandbox.limits ⊆ vm.resources`
  (memory ≤ VM memory;procs/fds 无 VM 对应则仅受实现约束)。
- 违反拒绝创建——防止 sandbox 声索超 VM 物理上限的资源。

---

## 4. 现有 ExecPolicy 的映射(向后兼容层)

| ExecPolicy | SandboxPolicy 映射 |
|---|---|
| `read_paths` | `Capability::File { path: Prefix(p), Read }` |
| `write_paths` | `Capability::File { path: Prefix(p), ReadWrite }` |
| `net_allow` | `Capability::Network { endpoint, Outbound }` |
| `memory_mb` | `limits.memory_mb` |
| `procs` | `limits.procs` |
| 缺省策略(sandlock 内置) | **引擎级常量 DEFAULT_SANDBOX_POLICY**(自包含,消除 guest 硬编码) |

映射层保留协议兼容:引擎接收旧 dict → 转 `SandboxPolicy` → 下发边界;
新策略对象为契约面。

**关键变化**:默认"只读系统集"从 guest-proxy 硬编码 → `DEFAULT_SANDBOX_POLICY`
常量(引擎级),同一策略对象跨实现语义一致。

---

## 5. 一 VM 多 sandbox 的边界执行

| 访问 | VM 层(VmPolicy) | Sandbox 层(SandboxPolicy) | 执行者 |
|---|---|---|---|
| CPU/内存 | 物理配额 | 逻辑上限(⊆) | VmAdapter(硬)+ SandboxAdapter(逻辑) |
| 文件 | 持久盘 upper 策略 | 路径能力 | SandboxAdapter → guest sandlock |
| 网络 | 拓扑(NAT/桥) | 端点白名单 | VM 网络 + sandbox 出站过滤 |
| 设备 | — | Device 能力 | SandboxAdapter → guest |
| 进程 | — | procs 配额 | SandboxAdapter → guest sandlock |

**隔离保证**:
- 同 VM 内 sandbox 互不可达 = 能力集无交叉(策略)+ guest 隔离(执行)。
- 跨 VM = 物理隔离(VM 层),不依赖 sandbox 策略。

---

## 6. 验证与验收

1. **语义自包含测试**:同一 `SandboxPolicy` 序列化→反序列化→下发,
   断言能力集不变(roundtrip)。
2. **默认拒绝测试**:空能力集 + `default: Deny` → 一切访问拒绝,有 deny 审计事件。
3. **上限校验测试**:sandbox limits > VM 配额 → 创建拒绝。
4. **映射兼容测试**:旧 `ExecPolicy` dict → `SandboxPolicy` → 行为等价。
5. **隔离测试**:一 VM 两 sandbox,能力集无交叉 → 互不可达(e2e)。

---

## 7. 实现落点

| 项 | 落点 | 阶段 |
|---|---|---|
| 类型(`VmPolicy`/`SandboxPolicy`/`Capability`…) | `crates/adapter/traits`(与 VmSpec/ExecPolicy 同层) | B1 |
| 默认策略常量 | 引擎层(`DEFAULT_SANDBOX_POLICY`) | B1 |
| ExecPolicy→SandboxPolicy 映射 | 协议/引擎边界 | B2 |
| SandboxAdapter 携带策略 | trait 签名更新 | C(L2 统一时) |

*本规范为 B 阶段设计基线。B1(类型落地)与 B2(映射)独立评审后实现。*
