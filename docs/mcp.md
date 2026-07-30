# MCP Server 接口文档

`terra-mcp` 以 stdio 运行 JSON-RPC 2.0（MCP 协议），为 AI agent 提供
Terrarium 的用户面工具。管理员操作（daemon 启停、镜像构建、网络拆除、
建池）刻意不在其中。

## 配置

```json
{
  "mcpServers": {
    "terrarium": {
      "command": "/path/to/terra-mcp",
      "env": { "TERRA_SOCKET": "/tmp/terra.sock" }
    }
  }
}
```

- `TERRA_SOCKET`：engine daemon 地址，unix 路径或 `tcp://host:port`
- `TERRA_TOKEN`：TCP 模式下的共享 token（首行发送）

协议细节：notification（无 `id`）不产生响应；日志只写 stderr，
stdout 仅承载 JSON-RPC 消息。

## 工具（13 个，全为用户面）

### VM 生命周期

| 工具 | 参数 | 说明 |
|---|---|---|
| `terra_vm_create` | `name`, `kernel`, `initramfs?`, `cpus?`, `memory_mb?`, `layers?` | 创建 VM |
| `terra_vm_list` | — | 列出 VM |
| `terra_vm_info` | `name` | 查询状态/资源 |
| `terra_vm_resize` | `name`, `cpus?`, `memory_bytes?` | 在线扩缩 |
| `terra_vm_shutdown` | `name` | 停止并注销 |
| `terra_vm_kill` | `name` | 强制停止并注销 |
| `terra_vm_destroy` | `name` | 停止并注销 |

### 执行

| 工具 | 参数 | 说明 |
|---|---|---|
| `terra_exec` | `name`, `args` (array), `timeout_secs?` | VM 内执行命令（guest agent 启动窗口自动重试）。注意：MCP 发送的是普通 exec（不带 `sandbox` 标志），**不经 sandlock 约束**——已知缺口，待补 |

### 池

| 工具 | 参数 | 说明 |
|---|---|---|
| `terra_pool_claim` | `layers` (array) | 认领池 VM 并热插层 |
| `terra_pool_list` | — | 池槽位状态 |
| `terra_pool_release` | `name` | 归还池 VM |

### 文件系统

| 工具 | 参数 | 说明 |
|---|---|---|
| `terra_attach_fs` | `name`, `layers` | 热插层到运行中 VM（挂 `/workdir`） |
| `terra_detach_fs` | `name` | 卸载 |

## 典型调用流

```
terra_pool_claim(layers=["python312","base"])
  → terra_exec(name, args=["python3","-c","print(2**10)"])
  → terra_pool_release(name)
```
