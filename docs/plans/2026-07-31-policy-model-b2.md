# 策略模型全面落地(B2)— 无兼容层替换

> **原则**:`SandboxPolicy`/`Capability` 成为唯一策略类型,贯穿协议、引擎、
> guest-proxy、SDK。删除 `ExecPolicy` 与旧 dict 形状,不做兼容映射。
> 面向 agent 真实需求(R5/R10),单一事实源,默认策略引擎权威。

---

## 设计决策(先行定案)

| # | 决策 | 理由 |
|---|---|---|
| D1 | `guest-proxy` 加 `adapter-traits` 依赖,用同一 `SandboxPolicy` 类型反序列化 | 单一事实源;类型 crate 无重依赖 |
| D2 | 引擎在无用户策略时注入 `default_sandbox_policy()`——exec 协议始终携带完整策略 | guest 不再硬编码默认;引擎权威 |
| D3 | 能力→sandlock 映射:Read→`-r`、ReadWrite→`-w`、`Network{Outbound}`→`--net-allow host:port`、limits→`-m`/`-P` | 对齐现有 sandlock 能力 |
| D4 | `Execute`/`Device`/`Inbound` 由类型承载,guest sandlock 后端返回明确"不支持"错误(不静默忽略) | 契约先行,实现逐步满足;诚实 |
| D5 | SDK 暴露新策略 JSON 形状;CLI `--read-path/--write-path/--net-allow/--memory-mb/--procs` 保留,内部构造 `Capability`/`limits` | CLI 体验不变,协议形状统一 |
| D6 | `DefaultAccess::Allow` 生产禁用(仅测试/调试逃逸门) | 默认拒绝是安全基石 |

---

## 任务(T 串行,类型链依赖)

### T1 — traits:替换策略类型
- `crates/adapter/traits/src/lib.rs`:
  - 删除 `ExecPolicy`(L219-241)
  - `ExecOpts.policy: Option<ExecPolicy>` → `Option<SandboxPolicy>`(L258)
  - `SandboxSpec` 加 `policy: Option<SandboxPolicy>`(与 limits 并列)
  - `SandboxPolicy` 加 `validate()`(能力集合法性:路径绝对、net_allow 端点非空、default 语义)——替代引擎 validate_policy
- 编译:全 workspace 会因类型缺失报错——这是预期的级联信号

### T2 — protocol:Command retype
- `crates/protocol/src/lib.rs`:
  - re-export `ExecPolicy` → `SandboxPolicy`(L14,连带 `Command.policy` L93/`with_policy` L259 自动 retype)
  - 重写 4 个策略测试(L339-412)为新 JSON 形状(roundtrip/deny 缺省/未知字段拒绝)

### T3 — engine:校验替换 + 默认注入
- `crates/engine/src/commands/mod.rs`:删 `validate_policy`,改用 `SandboxPolicy::validate()`;`run_exec`/`prepare_blocking_exec` policy 参数 retype;无策略时注入 `default_sandbox_policy()`(sandbox_exec 与 exec 一致)
- `crates/engine/src/commands/sandbox.rs`:`cmd_sandbox_create` 存储 `SandboxPolicy`;`cmd_sandbox_exec` merge 保留;无存储策略时注入默认
- `crates/engine/src/manager.rs`/`sandboxes.rs`/`sessions.rs`:`SandboxRecord.policy`、`exec`/`exec_background` 参数 retype
- 默认策略含 workdir 吗?——不含;workdir 走 `ExecOpts.work_dir`(动态),静态能力集为默认策略

### T4 — guest-proxy:统一类型 + 能力翻译
- `crates/guest-proxy/Cargo.toml`:加 `adapter-traits` 依赖
- `crates/guest-proxy/src/sandbox.rs`:删本地 `SandboxPolicy`(L47-65)与 `READ_GRANTS`/`WRITE_GRANTS`(L32-42);反序列化 `adapter_traits::SandboxPolicy`;`wrap_for_sandbox` 按 D3 翻译能力集(D4 的能力返回"不支持"错误)
- `crates/guest-proxy/src/main.rs`:policy 解析改用新类型(L215-234);3 个测试重写

### T5 — SDK:新策略形状
- `sdk/python/terra/client.py`:删 `_POLICY_KEYS`/旧 `validate_policy`;新校验(构造 `SandboxPolicy` JSON:capabilities/limits/default/version)
- `sdk/python/terra/sandbox.py`:`policy` 参数接受新 dict 形状;`policy` property 回显新形状
- `sdk/python/terra/__main__.py`:`_build_policy_from_args` 构造新形状
- 9 个 Python 策略测试更新

### T6 — docs:协议/语义重写
- `docs/protocol.md` L65-90:新策略对象形状 + 能力语义
- `docs/sdk.md` L104-165/L410-438:新形状 + CLI 映射
- `docs/design/policy-model.md` §4(兼容映射表)→ 删除,改为"已落地"注记
- `README.md` feature bullet 更新

---

## 验证

- 每任务:workspace 编译 + 相关测试绿
- T3 后:引擎策略测试全绿(注入默认/覆盖/合并/拒绝语义)
- T4 后:guest-proxy 测试绿(能力翻译/默认下发/不支持能力报错)
- T5 后:35 pytest 全绿
- 最终:全量门禁 + 真实 e2e(manual_e2e.py,含 policy 相关)

## 明确不做

- 兼容映射层(用户否决)——旧 dict 直接失效
- `Execute`/`Device`/`Inbound` 的 sandlock 实现(B2 只承载类型+诚实报错;实现留待后端能力扩展)
- 审计/度量接口(D 阶段)
