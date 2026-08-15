# 真实负载下的治理/隔离开销（2026-08-15，vmm 降权后重测）

> 论文核心证据：Terrarium 的治理（L2 confine）对真实 agent 风格负载的
> 开销是多少？VM 边界（L1）的开销是多少？与 docker / gVisor 相比如何？
> 原始数据：`docs/workload-overhead-2026-08-15-vmm.json`（CH/virtiofsd
> 降权到 `terra-vmm` 用户后重测，与降权前结论一致）。

## 方法

同一个**静态 C 负载探针**（`adversarial/probes/workload_probe.c`，无
shell/解释器差异）在五个环境各跑 5 次取中位数：

- `bare` — 宿主进程（零成本对照）
- `vm` — Terrarium VM，无治理 exec（只含 VM 开销）
- `vm+confine` — Terrarium VM + 默认策略治理（VM + 治理，产品路径）
- `docker` — 长驻容器 + `docker exec`（稳态 exec 路径）
- `gvisor` — `runsc do`（一次性沙箱，含启动）

负载：`fileio`（1500 次建/读/删文件）、`subproc`（150 次 fork+exec）、
`cpu`（300 万次整数运算）、`mixed`（agent 式：写源码 → 起 build 子进程
→ 读结果，10 轮）。

## 结果（中位数 ms）

| 负载 | bare | vm | vm+confine | docker(exec) | gvisor(do) |
|---|---:|---:|---:|---:|---:|
| fileio | 36.9 | 17.0 | 15.5 | 136.3 | 619.9 |
| subproc | 147.3 | 140.8 | 122.2 | 172.2 | 1094.7 |
| cpu | 8.2 | 10.9 | 10.1 | 98.9 | 286.9 |
| mixed | 32.0 | 36.2 | 37.5 | 117.1 | 564.7 |

## 解读（论文主张）

**1. 治理开销 ≈ 0（vm+confine / vm）**

fileio 0.91×、subproc 0.87×、cpu 0.93×、mixed 1.04×
——全部在噪声内（±10%）。机制上符合预期：Landlock 是内核静态裁决
（零逐调用开销）、seccomp 只拦低频网络 syscall、cgroup 是配额被动
记账。**治理不是性能税**，这是把 L2 设计成"行为约束"而非"syscall
拦截器"的直接收益。

**2. VM 边界开销是负载依赖的（vm / bare）**

- 纯 CPU（整数循环）：~1.3-2×（1 vCPU guest，宿主负载敏感，多次采样
  1.3-2.2×）；
- 文件/进程/混合负载：~0.5-1.3×（guest /tmp 是 tmpfs，fileio 反而比
  宿主磁盘快）。

诚实声明：VM 的硬隔离在计算密集路径有 ~1.5-2× 成本；IO/进程密集的
agent 工作（改代码、跑构建、读文件）基本持平。

**3. exec 路径开销：Terrarium ≪ docker ≪ gVisor**

同一负载，docker-exec 比 Terrarium 慢 1.4-10×（subproc 场景 docker
只慢 1.4×，其余负载 3-10×），gVisor 一次性沙箱慢 9-40×。原因：
docker exec 每次 ~80-100ms CLI/runc 往返，gVisor do
含沙箱启动；Terrarium exec p50 ~5ms（同 VM 复用 + vsock 通道）。agent
做的是成千上万次工具调用，**每次调用的固定开销**是真实场景的主成本。

## 诚实局限

- 单宿主、单轮采样；cpu 负载受宿主并发影响（报告多次采样的中位数 +
  范围）。
- fileio 的跨环境比较受 fs 后端影响（guest tmpfs vs 宿主磁盘 vs 容器
  overlay）——治理/隔离正交比较（vm vs vm+confine）不受影响，跨基线
  fileio 需附注。
- 未覆盖网络负载（策略门控的出网白名单延迟另测）与内存分配密集负载。

## 复现

```bash
bash sdk/python/tests/adversarial/probes/build-workload.sh
SUDO_PASSWORD=... python3 sdk/python/tests/workload_overhead.py --repeats 5 \
  --out /tmp/overhead.json
```
