# CI 接线：两层测试矩阵

Terrarium 的验证分两层，对应两个 GitHub Actions workflow：

| Workflow | 触发 | Runner | 内容 |
|---|---|---|---|
| `ci.yml` | push / PR | ubuntu-latest（无 KVM） | Rust 构建/格式/clippy/单测/审计 + SDK 非 e2e 测试 + 对抗探针构建 + harness 编译检查 |
| `e2e.yml` | push(main)/手动 | **自托管 `kvm`** | 真实 KVM 全套（4 个 e2e 套件，含对抗隔离）+ 跨基线矩阵 + 密度扫描 + artifact |

## 自托管 KVM runner 要求（e2e.yml）

GitHub 托管 runner 没有 `/dev/kvm`，所以真实 KVM 套件跑在自托管
runner（label `kvm`）上。注册 runner 前确保：

1. **KVM 设备**：`/dev/kvm`、`/dev/vhost-vsock` 存在；workflow 会尝试
   `sudo chmod 0666`，若宿主不想放宽权限，可给 runner 用户加 `kvm` 组
   （`sudo usermod -aG kvm <runner-user>` 后重新登录）。
2. **免密 sudo**：root daemon 需要 `CAP_NET_ADMIN`（NAT/tap），
   `terra daemon start` 会自行经 sudo 提权。runner 用户需要
   `NOPASSWD` sudo（`/etc/sudoers.d/terra`：
   `<runner-user> ALL=(ALL) NOPASSWD: ALL`）。
3. **工具链**：gcc（对抗探针静态编译）、cargo + musl target（guest
   二进制）、python 3.12 + pip。
4. **可选基线**：docker、runsc（gVisor）——harness 对缺失项自动 skip，
   但对比矩阵的覆盖度取决于它们是否安装。
5. **网络**：`terra setup ubuntu` 需要下载 ubuntu rootfs；出网 DNS 需
   正常（对抗/安全套件的白名单正例用宿主 LAN 目标，可复现）。

## 本地跑同一套

```bash
# 1) 基础 e2e 门禁（4 个套件，含对抗隔离）
bash sdk/python/tests/run_e2e.sh

# 2) 跨基线工作负载矩阵（bare / docker / gVisor / Terrarium）
python sdk/python/tests/compare_baselines.py --out /tmp/baseline.json

# 3) 密度扫描（默认 100 实例；CI 用 32）
python sdk/python/tests/density_compare.py --instances 32 --out /tmp/density.json
```

## 为什么密度/对抗矩阵要单独一条线

- `compare_baselines.py` 与 `density_compare.py` 需要 root daemon
  （NAT）、KVM、以及 docker/gVisor 等宿主基线——不属于普通单元测试门禁；
- 它们产出的是**证据**（JSON 结果 + 文档表格），适合放在自托管
  runner 上定期跑并上传 artifact，而不是阻塞每次 push；
- `ci.yml` 保留的探针构建 + `py_compile` 检查保证 harness 语法/探针
  可编译在无 KVM 环境也不会静默烂掉。
