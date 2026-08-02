# Agent Exec Env: 面向真实需求的完备契约

> **原则**:完备性不迎合任何 VM/沙箱实现,而是锚定 **agent 执行环境的真实需求**。
> 契约定义"agent 需要什么样的执行环境",实现(VM/沙箱)是契约的满足者。
> 当前阶段:定义契约 + 设计策略模型 + 规划 SandboxAdapter 接入引擎(统一 L2 边界)。
> 第二个实现(gVisor 等)暂不接入——先用契约和默认实现验证,再谈可替换。

---

## 1. Agent 执行环境的真实需求(锚点)

LLM 驱动的自主程序,循环为 观察 → 推理 → 行动。从使用中提取的十个真实需求:

| # | 需求 | 表现 | 若缺失的后果 |
|---|---|---|---|
| R1 | **会话性** Sessionful | 任务有状态、长生命周期;环境持久、可恢复 | 每次行动丢失状态,任务不可续 |
| R2 | **工具调用** Tool use | exec/文件/网络/专用工具=受控能力授予 | agent 无法完成真实任务 |
| R3 | **并发隔离** Concurrency | 多任务并行,互不可达 | 任务串行/数据泄漏 |
| R4 | **资源治理** Governance | 配额、限制、防失控(死循环/内存爆炸/网络滥用) | 单 agent 拖垮平台 |
| R5 | **最小权限** Least privilege | 不可信行为默认拒绝,显式授予 | 恶意/误操作越权 |
| R6 | **可观测审计** Observability | 执行审计、资源度量、策略拒绝事件 | 无法理解/追责 agent 行为 |
| R7 | **低延迟** Low latency | agent 循环频繁交互;快速启动 | 交互成本过高,循环变慢 |
| R8 | **弹性** Elasticity | 任务量波动、按需供给 | 高峰排队/低谷浪费 |
| R9 | **环境多样** Variety | 不同任务不同 OS/工具链/网络 | 环境单一,任务受限 |
| R10 | **策略可编程** Policy | 每 agent 信任级别不同 | 一刀切,无法差异化授权 |

**契约的判据**:每一契约能力必须能追溯到至少一个 R#,且无实现特判。

## 2. 从需求到契约能力域(Contract Capability Domains)

八个能力域,每个由若干契约接口表达:

```
┌─────────────────────────────────────────────────────────┐
│ Agent Exec Env 契约                                     │
├─────────────────────────────────────────────────────────┤
│ D1 隔离与信任   R3 R5    边界保证(机密/完整/资源/非干扰)  │
│ D2 执行与工具   R2       命令执行(参数化能力授予)        │
│ D3 会话生命周期 R1       创建/检查/销毁 + 会话状态持久    │
│ D4 资源治理     R4       配额声明/运行时调整/耗尽防护     │
│ D5 网络         R2 R5    方向化端点白名单/带宽            │
│ D6 可观测性     R6       执行审计/资源度量/拒绝事件       │
│ D7 策略         R5 R10   类型化能力集/默认拒绝/继承/版本  │
│ D8 弹性供给     R7 R8 R9 预热/冷启动/池/可组合环境        │
└─────────────────────────────────────────────────────────┘
```

**每个能力域对应引擎现有模块**:
- D1→`VmAdapter`(L1)/`SandboxAdapter`(L2)
- D2→`exec`/`attach_fs`
- D3→生命周期方法(创建/检查/销毁) + 会话状态持久(workdir 随 VM 存活;R1"可恢复"= 会话状态连续性,非 VM 级快照恢复);
  snapshot = 平台容错扩展(非 agent 会话契约;Roadmap 项);restore/pause/resume = 无 agent 需求依据,已从契约移除(实现遗产纠偏)
- D4→`VmSpec` 配额 + `resize` + 回收
- D5→`net` + `net_allow`
- D6→(缺口)tracing→审计/度量
- D7→`ExecPolicy`(需形式化)
- D8→warm pool + 模板 + 分层 fs

## 3. 策略模型设计(学术 × 工程)

### 3.1 模型选择论证

**能力模型(Capability-based)** 是 agent 场景的正确锚点,理由:
1. **工具调用即能力授予**:agent 的每次工具调用(R2)本质是"使用持有的能力"——
   能力模型与之同构。
2. **解决 Confused Deputy**:高权限主体(平台)代表低权限主体(agent)执行时,
   能力模型天然防止 agent 借用平台权限(agent 只持有自己的能力集)。
3. **最小权限的直接表达**(R5):主体显式持有,默认一无所有。
4. **Fail-safe defaults**:默认拒绝是安全设计基石;能力模型强制显式授予。

**纯能力模型的工程缺陷**(需妥协):
- 能力撤销难(agent 长时间持有)、委托模型复杂
- 资源类能力(内存/进程)不是"访问权"而是"配额"——需辅助机制

**结论:能力为主 + 配额辅助的混合模型**:
- 访问类(文件/网络/设备)→ 能力(Capability)
- 资源类(内存/进程/fd/带宽)→ 配额(Limit)
- 默认拒绝;显式授予;支持继承与单次覆盖

### 3.2 类型化策略对象(契约)

```rust
/// 路径模式:精确或前缀(学术:客体引用)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum PathPattern {
    Exact(PathBuf),
    Prefix(PathBuf),
}

/// 文件访问操作(最小权限的操作粒度)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum FileAccess {
    Read,
    ReadWrite,
    Execute,
}

/// 网络端点(host:port;端口省略=任意)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Endpoint {
    pub host: String,
    pub port: Option<u16>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Direction {
    Outbound,
    Inbound,
}

/// 一个能力:主体被授予的单一访问许可(不可伪造的边界内授权记录)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Capability {
    File { path: PathPattern, access: FileAccess },
    Network { endpoint: Endpoint, direction: Direction },
    Device { path: PathBuf },
}

/// 资源配额(与访问控制正交——分离职责)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResourceLimits {
    pub memory_mb: Option<u64>,
    pub procs: Option<u32>,
    pub fds: Option<u32>,
    pub bandwidth_kbps: Option<u64>,
}

/// 默认访问(显式,避免实现漂移)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum DefaultAccess {
    Deny,
    Allow, // 显式逃逸门(仅测试/调试)
}

/// 审计规格
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuditSpec {
    pub deny: bool,
    pub exec: bool,
    pub resource: bool,
}

/// 会话/命令的完整策略
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SandboxPolicy {
    /// 能力集(显式授予;空=除了默认一无所有)
    pub capabilities: Vec<Capability>,
    /// 资源配额
    pub limits: ResourceLimits,
    /// 默认访问(推荐 Deny)
    pub default: DefaultAccess,
    /// 审计规格
    pub audit: AuditSpec,
    /// 策略版本(审计可追溯)
    pub version: u32,
}
```

### 3.3 学术原则 ↔ 工程落点

| 学术原则 | 契约表达 | 工程落点 |
|---|---|---|
| Least Privilege | 默认拒绝,显式授予 | `default: Deny`;能力集空=仅内置 |
| Capability Security | 不可伪造的授权记录 | 策略在引擎→边界内传递;非网络 token |
| Fail-safe Defaults | 缺省=拒绝 | serde 默认 `default: Deny` |
| Complete Mediation | 所有访问经边界检查 | 每次 exec/fs/net 经策略校验(引用监视器) |
| Separation of Duty | 资源治理与访问控制分离 | `limits` 与 `capabilities` 正交 |
| Policy Inheritance | 会话策略+单命令覆盖 | 现有继承模型(存储策略+per-call override) |

### 3.4 与旧 ExecPolicy 的关系(已实现,**无兼容层**)

旧 `ExecPolicy`(`read_paths` / `write_paths` / `net_allow` / `memory_mb` /
`procs`)已整体迁移为 `SandboxPolicy` 能力模型(T5/T6 落地):

```
ExecPolicy.read_paths  → Capability::File { path: Prefix, Read }
ExecPolicy.write_paths → Capability::File { path: Prefix, ReadWrite }
ExecPolicy.net_allow   → Capability::Network { endpoint, Outbound }
ExecPolicy.memory_mb   → limits.memory_mb
ExecPolicy.procs       → limits.procs
```

映射由各客户端直接完成(CLI 标志 `--read-path` / `--write-path` /
`--net-allow` / `--memory-mb` / `--procs` 构造 `SandboxPolicy` JSON;
SDK 的 `validate_policy` 校验)。引擎**不再接收旧 ExecPolicy dict**——
没有向后兼容转换层,wire 上只有 `SandboxPolicy` 一种形状。

### 3.5 关键语义决策

1. **文件默认语义自包含**:不再"追加到硬编码只读系统集",而是策略显式含
   `File { path: "/usr", Read }` 等——同一策略跨实现语义一致(消除漂移)。
   内置"默认能力集"(sandlock 默认)作为**引擎级常量**注入,而非 guest 硬编码。
2. **网络默认方向化**:`Direction::Outbound`;默认拒绝时需显式端点。
3. **资源配额不可被能力授予**:`limits` 只能由策略设置者(平台)设定,agent 不可自授。
4. **审计事件**:`audit.deny`(拒绝事件)、`audit.exec`(执行)、`audit.resource`(超限)。

## 4. SandboxAdapter 接入引擎(统一 L2 边界)

### 4.1 现状与错位

引擎的实际 L2 隔离由 **guest 侧 sandlock 二进制**执行(guest-proxy 包装);
host 侧 `SandboxAdapter` 是独立参考实现,未参与执行路径。接口与实现错位。

### 4.2 接入设计(契约驱动)

```
engine ── exec(策略) ──► SandboxAdapter ──► 后端隔离机制
                            │
              ┌─────────────┴──────────────┐
              ▼                            ▼
     默认:guest sandlock          未来:gVisor / Kata / seccomp 容器
     (guest-proxy 代理,            (host 侧直接执行)
     策略经 vsock 下发)
```

**关键决策**:
1. `SandboxAdapter::create(vm, spec)` 接收完整 `SandboxPolicy`(D7)——策略在
   host 侧被 adapter 理解并转化为后端的隔离原语。
2. **默认实现 = guest-sandlock 代理**:host 侧 `SandboxAdapter` 将策略序列化
   经 vsock 下发给 guest-proxy,由 sandlock 执行——把现有"guest 直连"包装为
   adapter 的默认实现,而非另起炉灶。
3. **引擎执行路径统一**:所有会话 exec 走 `SandboxAdapter`(不再旁路);
   guest-proxy 成为"默认后端的传输层"。
4. **策略到达处校验**:每次 exec 的 `policy` 在边界(adapter)做 complete
   mediation 校验——不是只下发不校验。

### 4.3 接入分阶段

- **4a 接口定形**:`SandboxAdapter`/`SandboxHandle` 方法签名更新为携带
  `SandboxPolicy`;trait 文档声明边界属性契约。
- **4b 默认后端**:guest-sandlock 代理实现(策略经 vsock 下发);引擎切到
  adapter 路径;现有行为(双层隔离)不变——纯重构,测试锁定。
- **4c 验证**:会话隔离 e2e 在 adapter 路径下全绿;证明契约承载现有安全语义。
- **4d(未来)**:第二实现(gVisor)接入,验证可替换性。

## 5. 演进路线(面向真实需求的顺序)

| 阶段 | 内容 | 对应需求 | 工作量 |
|---|---|---|---|
| **A 契约化** | 能力域↔接口映射表 + trait 边界属性注释 | R1-R10 全覆盖 | 轻 |
| **B 策略形式化** | `SandboxPolicy`/`Capability` 类型 + 默认自包含 + 兼容映射 | R5 R10 | 中 |
| **C L2 统一** | SandboxAdapter 接入引擎(默认=guest sandlock 代理) | R3 R5 | 中重 |
| **D 审计度量** | 拒绝事件/资源度量接口 | R6 | 中 |
| **E 多实现验证** | gVisor 等第二后端(契约完备的实证) | 全局 | 重(未来) |

## 6. 完备性验收(面向需求,非面向实现)

契约完备,当:
1. **需求全覆盖**:每个 R# 有契约接口承载,无需求无归属。
2. **语义自包含**:同一策略对象在不同实现产生相同访问语义(默认不硬编码在实现)。
3. **默认拒绝可验证**:任何未显式授予的访问被拒绝,且有审计事件。
4. **执行路径统一**:所有隔离决策经过契约接口(无旁路)。
5. **可替换**:异构实现接入无需改引擎核心(未来验证,当前设计预留)。

---

*当前阶段:本文为契约与设计基线。B(策略形式化)与 C(L2 统一)是近期可执行项,
各自独立评审。A 随文档落地。D/E 后续。*
