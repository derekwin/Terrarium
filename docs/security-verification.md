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
| 网络 | 默认策略出网 | 拒（"Permission denied"） |
| 网络 | 显式 `Network{Outbound}` 白名单 | 精确放行；未授权目标拒 |
| 租户网络 | 跨租户 VM 二层互访 | 拒（ebtables 隔离规则） |
| 租户网络 | 出网/网关/路由（正例） | 正常 |
| 审计 | deny 事件进入 `audit_list` | 记录 |

结果：**12 passed, 1 xfailed**。

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

1. **进程数限制不生效**：`limits.procs` 被翻译为 sandlock `-P`，但在
   guest 实测 `-P 1` 下 3 个并发 sleep 全部成功（测试标记 xfail）。sandlock
   supervisor 的进程计数未触发。修复方向：cgroup pids 或 supervisor
   clone 拦截的可靠后端。
2. **审计默认关闭**：`SandboxPolicy.audit` 默认全 false，生产部署需显式
   开启，或把引擎默认策略的 deny 审计改为默认开。
3. **公网出口依赖宿主网络**：网络测试用宿主内网目标（10.102.0.254:80）
   保持可复现；受限公网（宿主 443 不通）环境下 NAT 出公网未覆盖。
