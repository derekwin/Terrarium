# Terrarium 文档索引

文档按主题分五组。想看结论的从 **性能与基准** 或 **入门** 开始；
安全/论文组是论证材料；设计组记录为什么这么做。

## 入门与使用

| 文档 | 内容 |
|---|---|
| [tutorial-real-agent.md](tutorial-real-agent.md) | 真实 agent 应用教程：MCP 接入、LangGraph/Claude SDK 替换、常见问题 |
| [sdk.md](sdk.md) | Python SDK：Sandbox / Pool / Template 高层 API、直连与远程客户端 |
| [mcp.md](mcp.md) | MCP server：28 个工具、默认会话、并发与后台任务 |
| [protocol.md](protocol.md) | 引擎 wire 协议：传输、命令一览、语义约定（单一事实源） |

## 设计决策

| 文档 | 内容 |
|---|---|
| [design/decisions.md](design/decisions.md) | 关键决策记录（D1-D5a）：架构分层、L1/L2 分离、降权、部署形态 |
| [design/policy-model.md](design/policy-model.md) | 策略模型：能力集、默认拒绝、校验与后端翻译（与实现一致） |
| [design/agent-exec-env-boundaries.md](design/agent-exec-env-boundaries.md) | 完备契约的历史设计稿：R1-R10 需求、能力域；类型草稿以 policy-model.md 为准 |

## 安全

| 文档 | 内容 |
|---|---|
| [security-threat-model.md](security-threat-model.md) | 威胁模型：L1（VM 边界）/ L2（治理）两层保证、对抗假设、论文叙事 |
| [security-verification.md](security-verification.md) | 安全验证闭环：套件、真实 KVM 结果、能力面收敛记录 |
| [security-adversarial.md](security-adversarial.md) | 对抗评估：31 项测试、修复的 4 个真实缺陷、与 docker/gVisor 基线对比 |

## 性能与基准

| 文档 | 内容 |
|---|---|
| [performance.md](performance.md) | **性能结论汇总**（面向读者）：exec 路径 vs docker/gVisor、治理≈0、密度、降权开销 |
| [workload-overhead.md](workload-overhead.md) | 真实负载开销的**方法学与原始数据**（workload_overhead.py 的论文证据） |
| [benchmarks.md](benchmarks.md) | 密度基准的**方法与历史数据档案**（含 8-14 统一热池、批量编排） |
| [perf/](perf/) | 图表：exec 延迟、负载开销、治理、降权 A/B、密度对比、内存共享 |
| [scripts/](scripts/) | 图表渲染脚本（CI 自动执行，可复现） |

原始测量数据（JSON）与各文档对应：

| 数据文件 | 对应文档/用途 |
|---|---|
| `workload-overhead-2026-08-15-vmm.json` | 5 环境 × 4 负载中位数（performance.md §2、workload-overhead.md） |
| `density-compare-2026-08-15.json` | 100 实例冷启动对比（performance.md §4） |
| `density-compare-snapshot-2026-08-15.json` | 100 实例快照路径对比（performance.md §4，密度主路径） |
| `bench-privilege-drop-*.json` | 降权 A/B（performance.md §3） |
| `benchmark-results-2026-08-*.json` | 历史密度/重置/批处理扫描（benchmarks.md 各章节） |
| `baseline-compare-2026-08-13.json`、`density-compare-2026-08-14.json` | 对抗/基线对比（security-adversarial.md） |

## 论文

| 文档 | 内容 |
|---|---|
| [paper/](paper/) | 论文素材：L1 论证、威胁模型/对抗/开销/密度证据的组织（USENIX Security 2027） |

## 运维

| 文档 | 内容 |
|---|---|
| [ci.md](ci.md) | CI 两层矩阵：GitHub 标准 job + 自托管 KVM runner（e2e.yml）要求 |

## 阅读建议

- **判断 Terrarium 值不值用**：`performance.md` + `security-threat-model.md`。
- **接入自己的 agent**：`tutorial-real-agent.md` → `sdk.md`/`mcp.md`。
- **给审稿人/同行看**：`paper/` 组 + 各安全文档 + 基准原始数据。
- **改代码前**：`design/decisions.md` + `protocol.md` + 对应 crates 的模块文档。
