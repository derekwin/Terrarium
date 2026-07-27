# Terrarium 引擎协议（接口文档）

engine daemon 与所有客户端之间是**单行 JSON** 请求/响应协议。CLI、
MCP、Python SDK 共用同一协议（`crates/protocol` 为单一事实源）。

## 传输

| 方式 | 地址 | 认证 |
|---|---|---|
| Unix socket | `/tmp/terra.sock` 或托管路径 `~/.local/share/terra/run/terra.sock`（chmod 0600） | 同 UID 本机访问 |
| TCP | daemon 以 `--tcp host:port` 启动 | 首行必须是 `TERRA_TOKEN` 的共享 token（明文，仅限可信网络） |

请求：一行 JSON（≤64KB）。响应：一行 JSON：

```json
{"status": "ok", "data": {...}}        // 或 {"status": "error", "error": "..."}
```

TCP + token 时，客户端先发送一行 token，再发送命令行。

## 命令一览

### VM 生命周期

| command | 字段 | 说明 |
|---|---|---|
| `create` | `name`, `kernel`, `initramfs?`, `cmdline?`, `cpus?`, `max_cpus?`, `memory_mb?`, `max_memory_mb?`, `layers?`, `upper?`, `net?` | 创建 VM。`layers` 为 virtiofs 层名列表（优先级从高到低，base 最后）；`upper` 为 Persistent upperdir 名 |
| `list` | — | 所有运行中 VM |
| `info` | `name` | state / cpus / memory_mb / pid |
| `resize` | `name`, `cpus?`, `memory_bytes?` | 在线扩缩（至少一项，否则报错） |
| `shutdown` / `kill` | `name` | 停止并注销（数据保留） |
| `destroy` | `name` | 停止并注销（同义，永不删数据） |
| `snapshot` | `name`, `snapshot_path?` | 内存快照（restore 未实现） |
| `restore` | — | 当前固定返回 not implemented |

### 执行

| command | 字段 | 说明 |
|---|---|---|
| `exec` | `name`, `args`, `timeout_secs?` | 经 guest-proxy（vsock）在 VM 内执行，返回 `{stdout, stderr, exit_code}`。默认 60s，上限 3600s |

### 文件系统（分层 virtiofs）

| command | 字段 | 说明 |
|---|---|---|
| `attach_fs` | `name`, `layers` | 热插层到运行中 VM（挂在 `/workdir`） |
| `detach_fs` | `name` | 卸载并回收 |

### 预热池

| command | 字段 | 说明 |
|---|---|---|
| `pool_create` | `pool_size`, `kernel?`, `net?` | 建空转 VM 池（agent initramfs 由 `TERRA_AGENT_INITRAMFS` 指定） |
| `pool_list` | — | 槽位与认领状态 |
| `pool_claim` | `layers` | 认领一台空转 VM 并热插层；满员返回 exhausted |
| `pool_release` | `name` | 卸载层并归还空转 |

### 网络

| command | 字段 | 说明 |
|---|---|---|
| `net_list` | — | 网桥（terra0, 10.200.0.1/24, NAT）与各 VM 的 tap |
| `net_down` | — | 拆除网桥/DHCP/NAT；有带网 VM 时拒绝 |

`create --net` 的语义：tap + 宿主 `terra0` 网桥 + dnsmasq DHCP
（10.200.0.100-250），需要 daemon 有 CAP_NET_ADMIN。

## 语义约定

- `shutdown`/`kill`/`destroy` 均为「停止 + 注销」；**VM 命令永不删除数据**
- 热插的层挂载于 guest 内 `/workdir`，exec 默认 cwd 也是 `/workdir`
- 池 VM 被 destroy 时自动从池移除；异常死亡由 reap 自动清理
- 错误响应统一 `{"status":"error","error":"<可读信息>"}`，不会静默成功
