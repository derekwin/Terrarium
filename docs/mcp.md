# MCP Server 接口文档

`terra-mcp` 以 stdio 运行 JSON-RPC 2.0（MCP 协议），为 AI agent 提供
Terrarium 的用户面工具。管理员操作（daemon 启停、镜像构建、网络拆除、
建池、租户清理）刻意不在其中。

## 配置

```json
{
  "mcpServers": {
    "terrarium": {
      "command": "/path/to/terra-mcp",
      "env": {
        "TERRA_SOCKET": "/tmp/terra.sock",
        "TERRA_KERNEL": "/path/to/vmlinux.bin",
        "TERRA_INITRAMFS": "/path/to/initramfs-virtiofs.cpio.gz"
      }
    }
  }
}
```

- `TERRA_SOCKET`：engine daemon 地址，unix 路径或 `tcp://host:port`
- `TERRA_TOKEN`：TCP 模式下的共享 token（首行发送）
- `TERRA_KERNEL` / `TERRA_INITRAMFS`：会话首次创建时冷启动 VM 所需的
  内核与 initramfs 路径（**建议配置**；若 warm pool 有空闲槽则认领
  热 VM，不需要它们）

协议细节：notification（无 `id`）不产生响应；日志只写 stderr，
stdout 仅承载 JSON-RPC 消息。

## 工具（21 个，全为用户面）

### 会话式执行（推荐入口）

agent 面向**会话**而非 VM：`terra_exec` 在隔离的沙箱会话内执行，
会话按名字自动创建并复用，无需 agent 管理生命周期。所有 MCP 会话
共享同一租户 VM（`tenant-mcp`），各自独立工作目录，每次 exec 默认
经 sandlock 约束。平台侧统一清理：`terra sandbox destroy-tenant mcp`。

| 工具 | 参数 | 说明 |
|---|---|---|
| `terra_exec` | `args` (array, 必填), `session?`, `sandboxed?` (默认 true), `cwd?`, `layers?` (仅首次创建会话), `timeout_secs?` | 在会话内执行命令。`session` 省略 → 共享 `"default"` 会话；不同会话名 = 隔离工作目录。`layers` 仅决定会话首次创建时的环境（默认 `["base"]`） |
| `terra_audit_list` | `limit?`, `event?`, `sandbox_id?` | 查询引擎审计环形缓冲（P2 可观测性）：exec/deny/resource 事件，最新优先 |
| `terra_exec_background` | `args` (array, 必填), `session?`, `layers?` (仅首次创建会话), `timeout_secs?` | 在会话内后台启动命令，不等待完成；立即返回 `{session_id, sandbox, status:"started"}`。轮询 `terra_session_status` 获取进度，`terra_session_kill` 终止 |
| `terra_session_status` | `session_id` (必填) | 查询后台会话的引擎记录。`session_id` 是引擎会话 id（来自 `terra_exec_background` 响应），不是 MCP 会话名 |
| `terra_session_kill` | `session_id` (必填) | 终止后台会话（guest 内 killpg），其工作目录一并移除 |
| `terra_session_read` | `path`, `session?` | 读取会话内文件（相对路径解析到会话工作目录） |
| `terra_session_write` | `path`, `content`, `session?` | 写入会话内文件（base64 桥接，相对路径解析到会话工作目录） |

`cwd` 覆盖时以 `sh -c "cd <cwd> && ..."` 包装；`sandboxed: false`
为逃生舱（不经 sandlock，如安装系统软件包）。

### VM 生命周期（高级/管理面）

| 工具 | 参数 | 说明 |
|---|---|---|
| `terra_vm_create` | `name`, `kernel`, `initramfs?`, `cpus?`, `memory_mb?`, `layers?` | 创建 VM |
| `terra_vm_list` | — | 列出 VM |
| `terra_vm_info` | `name` | 查询状态/资源 |
| `terra_vm_resize` | `name`, `cpus?`, `memory_bytes?` | 在线扩缩 |
| `terra_vm_shutdown` | `name` | 停止并注销 |
| `terra_vm_kill` | `name` | 强制停止并注销 |
| `terra_vm_destroy` | `name` | 停止并注销 |
| `terra_vm_snapshot` | `name`, `snapshot_path?` | 捕获 VM 当前状态到快照目录（P1 快速重置；捕获后 VM 保持暂停） |
| `terra_vm_restore` | `name`, `snapshot_path`, `layers?`, `net?` | 从快照目录创建**新 VM**（P1 快速重置，~200ms 对照冷启动 ~850ms） |

### 池与文件系统（高级/管理面）

| 工具 | 参数 | 说明 |
|---|---|---|
| `terra_pool_claim` | `layers` (array) | 认领池 VM 并热插层 |
| `terra_pool_list` | — | 池槽位状态 |
| `terra_pool_release` | `name` | 归还池 VM |
| `terra_attach_fs` | `name`, `layers` | 热插层到运行中 VM（挂 `/workdir`） |
| `terra_detach_fs` | `name` | 卸载 |

## 典型调用流

```
# 默认会话（自动创建/复用，默认沙箱化）
terra_exec(args=["python3","-c","print(2**10)"])
terra_session_write(path="result.txt", content="1024")
terra_session_read(path="result.txt")

# 并发任务隔离：不同 session 名 = 独立工作目录
terra_exec(session="task-a", args=["python3","train.py"])
terra_exec(session="task-b", args=["python3","eval.py"])

# 长任务：后台启动，轮询进度，按需终止
terra_exec_background(args=["make","build"])          # → {session_id:"sess-…", sandbox:"sb-…", status:"started"}
terra_session_status(session_id="sess-…")             # → {session_id, sandbox, status:"running"|"completed"|"failed"|"killed", exit_code, stdout, stderr}
terra_session_kill(session_id="sess-…")               # 终止并清理工作目录
```

平台侧清理会话租户：`terra sandbox destroy-tenant mcp`。
