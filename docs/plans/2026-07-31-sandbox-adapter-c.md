# C 阶段:L2 边界统一——SandboxAdapter 接入引擎

> **目标**:引擎的会话隔离从"直接调 guest-proxy"抽象为 `SandboxAdapter` 契约面——
> 会话创建/执行统一走 adapter,默认后端 = guest-sandlock 代理(现有机制封装),
> 为 gVisor 等异构后端铺平替换路径。B 阶段已让 `SandboxPolicy`/`Capability`
> 成为唯一策略类型;本阶段把 `SandboxAdapter` 变成它的执行入口。

---

## 1. 现状(执行路径错位)

```
engine ── sandbox_exec ──► vm.exec(ExecOpts{sandbox:true, policy}) ──► guest-proxy ──► guest sandlock
                                    │
SandboxAdapter(独立参考实现,未接入)  ── 未参与执行
```

- L2 隔离实际由 **guest 侧 sandlock** 执行(guest-proxy 包装)
- host 侧 `SandboxAdapter`/`SandboxHandle` 是独立参考实现(sandlock crate),引擎不调用
- 策略已统一为 `SandboxPolicy`,但执行路径仍是"每命令透传 policy 给 vm.exec"

## 2. C 阶段设计(契约驱动)

### 2.1 接口定形(4a)

```rust
/// 会话边界(L2)契约——见 docs/design/agent-exec-env-boundaries.md
pub trait SandboxAdapter: Send + Sync {
    /// 创建会话:绑定 VM + 完整策略。策略在 host 侧被理解并转化为
    /// 后端的隔离原语(默认:经 vsock 下发 guest sandlock)。
    async fn create(
        &self,
        vm: &dyn VmHandle,
        spec: &SandboxSpec,   // 含 policy(已加)
    ) -> Result<Box<dyn SandboxHandle>, AdapterError>;
}

pub trait SandboxHandle: Send + Sync {
    /// 在会话内执行命令(策略已在 create 时绑定;per-call override 可选)
    async fn exec(&self, cmd: &ExecCommand) -> Result<ExecResult, AdapterError>;
    async fn setup(&self, tools: &[String]) -> Result<(), AdapterError>;
    async fn destroy(&self) -> Result<(), AdapterError>;
}
```

- `SandboxHandle.exec` 不再携带 policy(create 时绑定)——会话 = 持久策略上下文
- per-call override:扩展 `ExecCommand` 带 `policy_override: Option<SandboxPolicy>`(可选,或 B 阶段保持每命令透传?**决策点**)

### 2.2 默认后端 = guest-sandlock 代理(4b)

```
engine ── SandboxAdapter::create(vm, spec) ──► 默认后端(guest-proxy 代理)
                                                  │ 会话上下文:{vm, policy}
engine ── SandboxHandle.exec(cmd) ──► 默认后端 ──► vm.exec(ExecOpts{sandbox:true, policy})
                                                  └── 复用现有 guest-proxy sandlock 路径
```

- 默认实现:持有 vm + 策略,exec 时构造 `ExecOpts{sandbox:true, policy}` 调 `vm.exec`
- **零行为变化**:guest 侧 sandlock 机制原样复用;adapter 是封装层
- 引擎的 sandbox 记录(SandboxRecord)持有 `SandboxHandle`(而非裸 vm_name + policy)

### 2.3 引擎接入(4c)

- `sandbox_create` → `adapter.create(vm, spec{policy})` 返回 `SandboxHandle`,存记录
- `sandbox_exec` → `handle.exec(cmd)`(含 per-call policy override 合并)
- `sandbox_kill`/`tenant_destroy` → `handle.destroy()`
- 后端选择:引擎配置 `SandboxAdapter`(默认 = guest-proxy 代理;未来 gVisor)
- 池 claim 的 sandbox 同样走 adapter

## 3. 关键决策点

| # | 决策 | 倾向 |
|---|---|---|
| C1 | per-call policy override 的承载 | `ExecCommand.policy_override`(create 绑定 + 单次覆盖,对齐现有语义) |
| C2 | 默认后端放哪 | 引擎内嵌默认实现(guest-proxy 代理),sandlock crate 改造为第二个实现(未来)或保留参考 |
| C3 | SandboxRecord 持有 handle | 持有 `Box<dyn SandboxHandle>`(会话句柄),vm_name 仍存(隔离/清理用) |
| C4 | 引擎如何拿 adapter | `VmManager` 持有 `Box<dyn SandboxAdapter>`(构造注入,默认 = 引擎内嵌代理) |
| C5 | ExecCommand 现状 | 已有(exec/args/workdir/timeout/limits)——扩展 policy_override |

## 4. 任务拆分(原子提交)

- **C1 接口定形**:SandboxAdapter/SandboxHandle trait 更新(exec 经会话;ExecCommand 加 policy_override);trait 文档写边界契约(机密/完整/资源隔离)
- **C2 默认后端**:引擎内嵌 `GuestSandlockAdapter`(持 vm + policy,exec 构造 ExecOpts 调 vm.exec);测试锁定
- **C3 引擎接入**:VmManager 持 adapter;sandbox_create/exec/kill 走 handle;SandboxRecord 持 handle;后端注入
- **C4 验证**:引擎测试全绿;真实 e2e(策略 9 + 标准 19)全绿——行为零变化证明

## 5. 明确不做

- gVisor 等第二实现(4d,未来——契约先落地,替换性后验证)
- SandboxAdapter 移除 guest 直连旁路(4b 是封装,guest-proxy 仍是传输层)
- 会话 pause/freeze 等新生命周期(不在 C 范围)

---

*C 阶段为纯重构 + 契约落地:执行路径不变(guest sandlock 仍是隔离执行者),接口统一为 SandboxAdapter。完成后,L2 边界有明确的契约入口,多后端可插。*
