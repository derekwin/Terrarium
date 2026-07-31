# Terrarium 引擎协议（接口文档）

engine daemon 与所有客户端之间是**单行 JSON** 请求/响应协议。CLI、
MCP、Python SDK 共用同一协议（`crates/protocol` 为单一事实源）。

## 传输

| 方式 | 地址 | 认证 |
|---|---|---|
| Unix socket | 默认 `/tmp/terra.sock`（chmod 0600；客户端可用 `TERRA_SOCKET` 覆盖） | 同 UID 本机访问 |
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
| `create` | `name`, `kernel`, `initramfs?`, `cmdline?`, `cpus?`, `max_cpus?`, `memory_mb?`, `max_memory_mb?`, `layers?`, `system?`, `upper?`, `net?` | 创建 VM。`layers` 为**附加层**（工具层，高优先级在前）；系统底座自动补 `system`（默认 `base`），已带系统层结尾则不补。`upper` 为 Persistent upperdir 名 |
| `list` | — | 所有运行中 VM |
| `info` | `name` | state / cpus / memory_mb / pid |
| `resize` | `name`, `cpus?`, `memory_bytes?` | 在线扩缩（至少一项，否则报错） |
| `shutdown` / `kill` | `name` | 停止并注销（数据保留） |
| `destroy` | `name` | 停止并注销（同义，永不删数据） |
| `snapshot` | `name` | 内存快照（restore 未实现；自定义 `snapshot_path` 暂不支持，传入会报错） |
| `restore` | — | 当前固定返回 not implemented |

### 执行

| command | 字段 | 说明 |
|---|---|---|
| `exec` | `name`, `args`, `timeout_secs?`, `exec_mode?`, `sandbox?`, `policy?` | 经 guest-proxy（vsock）在 VM 内执行，返回 `{stdout, stderr, exit_code}`。`exec_mode` 可选 `"blocking"`（默认）或 `"background"`（返回 `{session_id}`）。`sandbox: true` 时在 guest 内经 sandlock（Landlock/seccomp）约束运行。默认 60s，上限 3600s |

`sandbox: true` 的默认策略（hardcode 在 guest-proxy）：只读授予
`/usr /lib /lib64 /bin /sbin /etc /tmp`（按存在性过滤）与
`/dev/urandom`；可写仅会话工作目录、`/tmp` 与 `/dev/null`；**绝不授予
`/`**（同 VM 其他会话的工作目录因此不可读）；网络暂不限制。guest 内按序
探测 `/usr/bin/sandlock` 与 `/workdir/usr/bin/sandlock`（池 VM 的层热插在
`/workdir`）；二进制缺失时返回硬错误，绝不静默回退为非沙箱执行。

### 执行会话（background exec）

| command | 字段 | 说明 |
|---|---|---|
| `session_status` | `session_id` | 查询后台执行会话状态，返回 `{session_id, vm_name, args, status, exit_code, stdout, stderr, sandbox}` |
| `session_kill` | `session_id` | 终止后台执行会话：经 guest-proxy `kill` 命令在 guest 内 killpg 进程组，返回 `{session_id, status: "killed"}`；未知/非 running 会话或 VM 已消失时硬报错 |
| `session_list` | — | 列出全部执行会话，返回 `{sessions: [{session_id, vm_name, status, sandbox}], count}` |

### 沙箱（引擎级实体）

Sandbox 是租户共享 VM（`tenant-<tenant>`）内的一个会话（独立工作目录），
引擎维护注册表：tenant → VM，sandbox id → {tenant, workdir}。VM 内隔离
由 guest 侧 sandlock 保证（见 `exec` 的 `sandbox` 字段）。

| command | 字段 | 说明 |
|---|---|---|
| `sandbox_create` | `tenant`, `kernel?`, `initramfs?`, `layers?`, `system?`, `cpus?`, `memory_mb?`, `net?`, `policy?` | 幂等确保租户 VM 存在（已存在则复用，忽略 VM 规格字段；租户名按 VmName 白名单校验），分配 sandbox 并在 guest 建工作目录，返回 `{id: "sb-<12hex>", vm: "tenant-<tenant>", workdir: "/workdir/sb-<hex>"}`。`policy` 存入 sandbox 记录 |
| `sandbox_exec` | `id`, `args`, `timeout_secs?`, `exec_mode?`, `sandbox?`, `policy?` | 在租户 VM 内执行，cwd 由引擎设为该 sandbox 的工作目录。`sandbox` 缺省为 **true**（sandlock 约束）。blocking 返回 `{stdout, stderr, exit_code}`；`exec_mode: "background"` 返回 `{session_id, sandbox, status: "started"}`。`policy` 为单次覆盖 |
| `sandbox_list` | `tenant?` | 列出 sandbox 记录（可按租户过滤），返回 `{sandboxes: [{id, tenant, vm, workdir, created_at, policy}], count}` |
| `sandbox_info` | `id` | 单个 sandbox 记录，字段同上（含存入的 `policy`） |
| `sandbox_kill` | `id` | 真实终止该 sandbox 的全部 running 会话（killpg）→ guest 内 `rm -rf` 工作目录 → 删除注册记录；**共享租户 VM 保持运行**。返回 `{id, sessions_killed, status: "killed"}` |
| `tenant_destroy` | `tenant` | 销毁租户 VM（语义同 `destroy`）并级联删除该租户全部 sandbox 记录，返回 `{tenant, vm, sandboxes_removed, status: "destroyed"}` |

**`policy` 对象**（`exec` / `sandbox_create` / `sandbox_exec` 通用，同一形状）：

```json
{"read_paths": ["/opt/data"], "write_paths": ["/output"],
 "net_allow": ["api.openai.com:443", "pypi.org"], "memory_mb": 512, "procs": 20}
```

- `read_paths` / `write_paths`：额外只读/读写路径授予，**追加**在内置默认
  策略之上（不替换）；必须是绝对路径。
- `net_allow`：缺省（字段省略）→ 出站网络不限制（现状默认）；出现 →
  sandlock `--net-allow` 逐条放行、其余默认拒绝。**必须非空**——空列表在
  客户端/引擎/guest-proxy 三层都是硬错误（"net_allow must be a non-empty
  list (omit the field for unrestricted network)"），因为零条旗标会静默
  变成不限制。
- `memory_mb` / `procs`：sandlock 资源限制（`-m <n>M` / `-P <n>`）。
- `policy` 与 `"sandbox": false` 同现 → 报错（策略只在沙箱化执行时有意义）。
- 存放与覆盖：`sandbox_create` 的 policy 存入记录（`sandbox_info` /
  `sandbox_list` 回显），后续 `sandbox_exec` 继承；`sandbox_exec` 自带的
  policy 仅对该次调用生效，不影响已存策略。未知字段一律拒绝。

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
| `net_up` | — | 建立宿主 `terra0` 网桥 + dnsmasq DHCP + NAT（`terra net create` 所用） |
| `net_down` | — | 拆除网桥/DHCP/NAT；有带网 VM 时拒绝 |

`create --net` 的语义：tap + 宿主 `terra0` 网桥 + dnsmasq DHCP
（10.200.0.100-250），需要 daemon 有 CAP_NET_ADMIN。

### Daemon 生命周期

| command | 字段 | 说明 |
|---|---|---|
| `daemon_stop` | — | 停止 daemon（服务模式）：先 shutdown 所有 VM 再退出。daemon 嵌入 Python 进程（PyO3 FFI）时返回 not supported 错误 |

## 语义约定

- `shutdown`/`kill`/`destroy` 均为「停止 + 注销」；**VM 命令永不删除数据**
- **语义模型**：rootfs = 系统（可启动镜像），layer = 系统之上的附加层。`layer ls` 只列附加层；`rootfs ls` 只列系统镜像
- 热插的层挂载于 guest 内 `/workdir`，exec 默认 cwd 也是 `/workdir`
- 池 VM 被 destroy 时自动从池移除；异常死亡由 reap 自动清理
- 错误响应统一 `{"status":"error","error":"<可读信息>"}`，不会静默成功
