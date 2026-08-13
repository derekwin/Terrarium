# 安全验证闭环

在真实 KVM 上验证"默认拒绝"沙箱模型的隔离面是否真的拦得住。
对应策略模型（`docs/design/policy-model.md`）的执行层验证（P2 项）。

## 套件

`sdk/python/tests/test_security_isolation.py`（`@pytest.mark.e2e`，真实
KVM + sandlock）。每个测试尝试一个真实的逃逸/越权原语，断言沙箱拒绝；
同时保留正例（读 /etc、写 /tmp/workdir），防止"全拒"回归。

```bash
pytest sdk/python/tests/test_security_isolation.py -v
```

## 验证结果（2026-08-05，真实 KVM）

| 隔离面 | 测试 | 结果 |
|---|---|---|
| 文件系统 | 写 `/etc/passwd`、`/root`、`/home` | 拒（exit 200，deny 信号） |
| 文件系统 | 读 `/dev/mem` | 拒 |
| 文件系统 | 读 `/etc`、写 `/tmp`、写 workdir（正例） | 放行 |
| 跨沙箱 | A 读/写 B 的 workdir（同 VM 内） | 拒 |
| 进程 | `limits.procs` 超限 fork | 拒（EAGAIN，busybox fork 也被拦） |
| 进程 | 限额内单进程（正例） | 放行 |
| 网络 | 默认策略出网 | 拒（"Permission denied"） |
| 网络 | 显式 `Network{Outbound}` 白名单 | 精确放行；未授权目标拒 |
| 租户网络 | 跨租户 VM 二层互访 | 拒（ebtables 隔离规则） |
| 租户网络 | 出网/网关/路由（正例） | 正常 |
| 审计 | deny 事件进入 `audit_list` | 记录 |

结果：**14 passed**。

## 审计持久化

审计事件（exec/deny/resource）追加写入 `$TERRA_HOME/audit/audit.jsonl`
（JSONL，0600，daemon root 所有——沙箱用户读不到自己留下的痕迹）。
写盘是 best-effort：磁盘满/损坏只记日志，绝不阻塞沙箱执行。

```bash
# 查内存环形缓冲（最近 10k 条）
terra audit ls
# 查持久化历史（daemon 重启后仍可查）
terra audit ls --history
```

SDK：`TerraClient().audit_list(history=True, event="deny", ...)`。

## e2e 真机门禁

CI（ubuntu-latest 无 /dev/kvm）跑的是非 e2e 门禁；真机套件由
`sdk/python/tests/run_e2e.sh` 串联（test_e2e_real + test_sandbox +
test_security_isolation），在有 KVM 的机器/自托管 runner 上执行：

```bash
bash sdk/python/tests/run_e2e.sh
```

## 租户间网络隔离

所有 VM 共享 `terra0` 桥与一个子网（DHCP/路由简单），但桥的默认语义是
二层互通——租户 VM 可以互 ping/扫描。`crates/network` 增加一条 ebtables
规则（`net up` 时幂等安装，`net down` 移除）：

```
ebtables -A FORWARD -i terra-+ -o terra-+ -j DROP
```

任意两个 VM tap 端口（`terra-*` 前缀）之间的帧直接丢弃；VM↔宿主网关
（`-o terra0` 不匹配）与 DHCP/ARP 广播不受影响。跨租户 VM 因此互相不可达
（ARP 都解析不到），同时各租户出网、DHCP、网关路由保持正常。

## 本次修复：默认拒绝网络

此前 guest-proxy 只在策略含网络能力时传 sandlock net 规则，而 sandlock
的语义是"无规则 = 全放行"——默认策略（无 Network 能力）下的沙箱实际
可以任意出网，违背"默认拒绝"模型。修复（`crates/guest-proxy`）：策略无
任何 `--net-allow` 时注入 `--net-deny 0.0.0.0/0` 与 `--net-deny ::/0`
（sandlock 的 deny 规则按协议展开 TCP/UDP，双栈全拒；`--net-deny` 与
`--net-allow` 互斥，显式授权场景不受影响）。

需要网络的沙箱显式声明能力即可（SDK policy 的 `Network` capability）。

## 已知缺口（跟踪中）

1. **审计默认关闭**：`SandboxPolicy.audit` 默认全 false，生产部署需显式
   开启，或把引擎默认策略的 deny 审计改为默认开。
2. **公网出口依赖宿主网络**：网络测试用宿主内网目标（10.102.0.254:80）
   保持可复现；受限公网（宿主 443 不通）环境下 NAT 出公网未覆盖。

已修复：**进程数限制**（sandlock `-P` 被 busybox `fork(2)` 绕过）——
见下节。

## 进程数限制修复（sandlock `-P`）

`limits.procs` 经 guest-proxy 翻译为 sandlock `--max-processes`，但
sandlock 的 supervisor 只拦截 clone/clone3/vfork——busybox ash 用
`fork(2)` 派发后台任务，直接绕过进程计数（实测 `-P 1` 下 3 个并发 sleep
全部成功）。修复（`thirdparty/sandlock-v0.8.5-procs.patch`）：只要
`max_processes` 被设置（非默认哨兵 64），`fork(2)` 也加入 seccomp-notify
拦截集，复用 `handle_fork` 的计数逻辑（超限返回 EAGAIN）。busybox 与
dash 的沙箱现在都受进程限制约束。

## Native 后端（默认）：terra-sandbox

默认 L2 后端已切换为自研 `terra-sandbox`（`crates/guest-sandbox` +
`crates/adapter/native`），sandlock 保留为 alternative（engine
`--features sandlock`）。选型与 sandlock 的关键差异：

| 能力 | native（terra-sandbox） | sandlock（alternative） |
|---|---|---|
| 文件系统 | **Landlock 静态**（内核执行，~1.3×，无 deny 审计） | supervisor 观察（4.6×，denyfd 审计） |
| 网络 | seccomp-notify 白名单/默认拒绝（低频，denyfd 审计） | Landlock/supervisor |
| 进程限制 | v1 不支持（guest 内核无 cgroup pids；VM 配额兜底） | `-P`（patch 修复后生效） |
| 维护 | 自研，无第三方 patch | 4 个 sandlock patch |

真实 KVM 验证（base 层）：安全套件 14 passed + 1 skipped（procs），文件
系统/网络/审计语义与 sandlock 一致（fs deny 是静态 EACCES 而非结构化
200；网络 deny 仍是结构化 200 + 审计）。

## 已知问题

- **多层（erofs + 目录层）组合 VM 启动卡**：单层（base/ubuntu/ci-terra/
  swe）与目录+目录组合正常，但 erofs 层 + 目录层的组合在
  `sandbox_create` 的 guest exec 处挂起（疑似 virtiofs/overlay 组合大
  rootfs 的遍历问题）。与 sandbox 后端无关（sandbox_create 阶段即卡）。
  待单独调查（可能需重建层或调整 virtiofs 挂载）。
