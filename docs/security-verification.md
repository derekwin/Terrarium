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

## Native 后端（默认）：terra-confine

默认 L2 后端已切换为自研 `terra-confine`（`crates/guest/confine` +
`crates/adapter/confine`），sandlock 保留为 alternative（engine
`--features sandlock`）。选型与 sandlock 的关键差异：

| 能力 | native（terra-confine） | sandlock（alternative） |
|---|---|---|
| 文件系统 | **Landlock 静态**（内核执行，~1.3×，无 deny 审计） | supervisor 观察（4.6×，denyfd 审计） |
| 网络 | seccomp-notify 白名单/默认拒绝（低频，denyfd 审计） | Landlock/supervisor |
| 进程限制 | cgroup v2 `pids.max`（guest 内核重编后含 CGROUP_PIDS） | `-P`（patch 修复后生效） |
| fds 限制 | `setrlimit(RLIMIT_NOFILE)` | 接口未接 |
| cpu 份额 | cgroup v2 `cpu.weight`（cpu_shares → weight） | 忽略 |
| bandwidth | 接口拒绝（VM 层语义，未实现） | 忽略 |
| 维护 | 自研，无第三方 patch | 4 个 sandlock patch |

真实 KVM 验证（base 层）：安全套件 **15 passed**（含 procs 限制真断言），
文件系统/网络/审计语义与 sandlock 一致（fs deny 是静态 EACCES 而非
结构化 200；网络 deny 仍是结构化 200 + 审计）。

guest 内核重编（#10）：`config-minimal` 基础上启用
`CONFIG_CGROUP_PIDS`（confine 进程限制）、`CONFIG_EROFS_FS` +
`CONFIG_BLK_DEV_LOOP`（层镜像内核挂载）、`CONFIG_IKCONFIG`（便于未来
提取配置）。

## 能力面收敛（评审后移除）

| 移除项 | 理由 |
|---|---|
| `File::Execute` | Linux 上执行即读，由 `Read` 隐含 |
| `Network::Inbound` | VM 内监听（bind）不设限，外部由 NAT 隔离；Bridge 未实现前无外部入站语义 |
| `Device` | 安全设备默认覆盖（/dev 只读 + STRICT_DEVMEM），危险设备不该授权 |
| `bandwidth_kbps` | 进程级带宽成本高价值低，属 VM 配额（未实现） |
| `VmNetwork::Bridge` | 对外暴露场景才有需求（P3），且绕过租户隔离有安全含义 |

上述能力已从类型与校验中**移除**（接口只表达后端真实支持的能力）；
`VmNetwork::Bridge` 保留枚举但标注 P3。

## 已知问题

- **已修复：多层（erofs + 目录层）组合 VM 启动卡**。根因：应用层
  （ci-terra）误带 `/bin` 目录（只有 guest-proxy），而系统层（ubuntu）
  的 `/bin` 是指向 `/usr/bin` 的符号链接——overlay 合并时"目录 vs
  符号链接"冲突，`/bin` 被应用层的目录整体覆盖，guest 丢失 `sh`，
  `switch_root` 执行 `#!/bin/sh` 的 init 时 ENOENT。修复：应用层不带
  `/bin`，guest-proxy 部署到 `/usr/bin`（经 ubuntu 的 `/bin -> /usr/bin`
  符号链接可达），系统工具完全由系统层提供。真机验证：ci-terra+ubuntu、
  swe-4160+ubuntu、base 单层全部正常，python 在 confine 沙箱内执行
  通过。

**层部署约定**：guest-proxy 放各层的 `/usr/bin/guest-proxy`（应用层的
`/bin` 保持不存在，避免覆盖系统层的 `/bin` 符号链接）；terra-confine
放 `/usr/bin/terra-confine`；sandlock（alternative）放
`/usr/bin/sandlock`。
