# 论文素材（USENIX Security 2027 Cycle 2）

> 定位：**治理（VM 内，L2）× 隔离（VM 边界，L1）分离**——同 uid 治理
> 便宜可杀，L1 兜底爆炸半径。目标会议注册 2027-01-19。

## 已有素材

| 素材 | 状态 | 对应文档 |
|---|---|---|
| L1 论证：TCB、KVM 隔离引用、攻击面、侧信道、对比表 | 完成 | [l1-argument.md](l1-argument.md) |
| 威胁模型（L1/L2 分离） | 完成 | [security-threat-model.md](../security-threat-model.md) |
| 对抗评估（31 测试、4 个真实缺陷闭环） | 完成 | [security-adversarial.md](../security-adversarial.md) |
| 安全验证闭环 | 完成 | [security-verification.md](../security-verification.md) |
| 真实负载开销（治理≈0、VM 1.3-2×、exec ≪ docker/gVisor） | 完成 | [workload-overhead.md](../workload-overhead.md) + JSON |
| 密度/快照/热池（共享层、59/s 快照创建、9ms 认领） | 完成 | [performance.md](../performance.md)、[benchmarks.md](../benchmarks.md) |
| 降权（CH/virtiofsd → terra-vmm，fd tap、uid 翻译） | 完成（实现+验证） | [decisions.md](../design/decisions.md) D4a |

## 论文章节组织建议

1. **威胁模型**：L1/L2 分离——L2 是治理层（同 uid、可杀、非安全边界），
   L1 是 KVM 硬件边界（爆炸半径有界）。
2. **对抗评估**：31 测试 + 4 个真实缺陷修复闭环。
3. **真实负载开销**：治理≈0，VM 1.3-2×（CPU 依赖），exec 路径 2.9ms
   vs docker 89ms vs gVisor 293ms。
4. **密度/快照/热池**：共享 EROFS 层、快照恢复 59/s、热池认领 9ms。
5. **L1 论证**：TCB 对照表（Docker 共享内核 / gVisor 大 sentry /
   Terrarium KVM+微VM）、攻击面分析、范围声明。

## 待补

- 论文正文写作（当前是素材级）。
- 可复现 artifact（基准脚本 + 数据已入库，需整理成 artifact 说明）。
- 内部评审。
- 可选工程加分项：宿主配置文档（`mitigations=auto,nosmt`、微码、IOMMU）。
