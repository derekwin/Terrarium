# Python SDK 接口文档

安装：`pip install -e .`（从仓库根目录，同时获得 `python -m terra` 与 `terra` 命令）。

## 层次化 API

Terrarium SDK 提供三层 API，从高层到低层：

| 层次 | 类 / 函数 | 适用场景 |
|---|---|---|
| **高层沙箱** | `terra.Sandbox`, `terra.AsyncSandbox` | 开箱即用，自动起 daemon，context manager 自动回收 |
| **池 / 模板** | `terra.Pool`, `terra.Template` | 预热池 + 命名环境组合，适合批量任务 |
| **底层直连** | `terra.TerraClient`, `terra.create()` | 原生 VM 操作，完全控制 |

## 三种使用方式

```python
import terra

# 1) 高层沙箱（推荐）：自动起 daemon，自动回收
from terra.sandbox import Sandbox

with Sandbox(tenant="my-org", template="py312", network=True) as sb:
    result = sb.exec(["python3", "-c", "print(2+2)"])
    print(result.stdout)          # ExecResult(exit_code=0, stdout="4\n", ...)

# 2) 直连（临时 VM）：首次调用自动起托管引擎
vm = terra.create(layers=["python312", "base"])
vm.exec(["python3", "--version"])
vm.destroy()

# 3) 远程客户端：一行 connect，之后代码与直连完全一致
terra.connect("tcp://server:19099", token="secret")
vm = terra.create(layers=["python312", "base"])   # 底层走 pool_claim
vm.destroy()                                       # 底层走 pool_release
```

## 高层 API

### `terra.Sandbox`

统一的沙箱抽象，**租户优先模型**：VM 是租户隔离边界，Sandbox 是 VM 内的会话。
同一租户下的多个 Sandbox 共享一个 VM，各自拥有独立工作目录。

#### 租户优先架构

```
租户 "research-team" 的 VM
 ┌────────────────────────────────────┐
 │  guest-proxy (vsock)              │
 │  virtiofs: layers + per-sb wd     │
 │  ┌──────────┐ ┌──────────┐        │
 │  │ sb-a3f2  │ │ sb-b7d1  │  ...   │
 │  │ /workdir │ │ /workdir │        │
 │  └──────────┘ └──────────┘        │
 └────────────────────────────────────┘

租户 "production" 的 VM —— 完全独立的 KVM 隔离
```

```python
from terra.sandbox import Sandbox
from terra.exceptions import ExecError, SandboxTimeoutError

# 从模板创建（推荐）——首次为租户创建 VM，后续复用
sb = Sandbox(tenant="my-org", template="py312", network=True)

# 从显式 layers 创建
sb = Sandbox(tenant="my-org", layers=["python312", "base"],
             kernel="k612", cpu=2, memory_mb=512)

# 属性
print(sb.id)        # 完整标识："tenant-my-org/sb-a3f2"（租户/会话）
print(sb.vm)        # VM 名称："tenant-my-org"
print(sb.status)    # "running" / "stopped" / "paused"
print(sb.backend)   # "ch" (Cloud Hypervisor)

# 多 Sandbox 共享同一租户 VM
sb1 = Sandbox(tenant="research-team", template="py312")
sb2 = Sandbox(tenant="research-team")   # 复用同一 VM，新工作目录
sb3 = Sandbox(tenant="research-team")
# sb1、sb2、sb3 在同一个 VM 内，各自拥有独立的 /workdir

# exec — blocking 执行，返回 ExecResult
result = sb.exec(["python3", "-c", "print(1+1)"])
print(result.stdout, result.stderr, result.exit_code, result.duration_ms)

# check=True — 非零退出码自动抛 ExecError
try:
    sb.exec(["false"], check=True)
except ExecError as e:
    print(f"exit={e.exec_result.exit_code} stderr={e.exec_result.stderr}")

# 自定义 cwd / env / timeout
result = sb.exec(["ls", "-la"], cwd="/tmp", env={"LANG": "C.UTF-8"}, timeout=30)

# 文件操作（通过 sb.files）
sb.files.write("/workdir/hello.txt", "Hello from host!")
content = sb.files.read("/workdir/hello.txt")
sb.files.upload("./local_file.txt", "/workdir/uploaded.txt")
sb.files.download("/workdir/data.txt", "./downloaded.txt")
files = sb.files.list("/workdir")              # → list[FileInfo]
sb.files.mkdir("/workdir/sub")
print(sb.files.exists("/workdir/sub"))         # → True
sb.files.remove("/workdir/sub")

# 在线扩缩 / 指标
sb.resize(cpu=4, memory_mb=1024)
metrics = sb.metrics()   # {cpu_count: 4, memory_mb: 1024}

# 生命周期
sb.kill()                # 移除会话工作目录，VM 保留供同租户其余 Sandbox 使用
Sandbox.destroy_tenant("my-org")  # 销毁租户 VM 及全部会话

# Context manager — 自动 kill
with Sandbox(tenant="my-org", template="py312") as sb:
    print(sb.exec(["uname", "-a"]).stdout)
```

> **注意：** `sb.kill()` 仅移除当前会话的工作目录，不会销毁 VM（同租户的其他 Sandbox 仍需使用）。要完全销毁租户 VM，使用 `Sandbox.destroy_tenant("tenant-name")`。

### `terra.AsyncSandbox`

`Sandbox` 的 asyncio 版本，通过线程池执行阻塞操作，保持 event loop 不阻塞。

```python
from terra.async_sandbox import AsyncSandbox

# 推荐：async 工厂方法
sb = await AsyncSandbox.create(template="py312")

# Async context manager
async with await AsyncSandbox.create(template="py312") as sb:
    result = await sb.exec(["python3", "--version"])
    print(result.stdout)

# API 与 Sandbox 一致，所有方法都是 async
status = sb.status
await sb.exec(["ls"], timeout=10)
await sb.kill()
await sb.resize(cpu=2)
metrics = await sb.metrics()
```

### `terra.Pool`

预热池管理 — 预先启动空闲 VM，acquire 时返回池 VM 内的 Sandbox 会话。
同一池的多次 acquire 共享同一 VM，各自拥有独立工作目录。

```python
from terra.pool import Pool
from terra.sandbox import Sandbox

# 从模板创建池
pool = Pool(template="py312", size=3)   # 3 个预热 VM

# 显式 layers
pool = Pool(layers=["python312", "base"], size=2, net=True)

# 查询状态
st = pool.status()  # {"idle": 3, "claimed": 0, "total": 3}

# 认领就绪沙箱——返回 Sandbox（已启动，层已就绪）
sb1 = pool.acquire()         # → Sandbox（在共享池 VM 中）
sb2 = pool.acquire()         # → 另一个 Sandbox（同一 VM，不同工作目录）
sb1.exec(["python3", "-c", "print(42)"])

# 归还 / 注销
pool.release(sb1)            # 归还池，可再次认领
sb2.kill()                   # 永久注销会话

# 动态缩放
pool.grow(2)                # 追加 2 个 VM
```

### `terra.Template`

命名环境组合 — 内核 + base distro + 工具 layer 的持久化配置。

```python
from terra.template import Template

# 从已有 layer 创建模板
t = Template.from_layers(
    base="alpine",                     # alpine → musl；ubuntu → glibc
    layers=["python312", "base"],      # 工具层，高优先级在前
    kernel="k612",
    name="py312",
)

# 列举 / 加载 / 删除
names = Template.list()               # → ["py312"]
t = Template.load("py312")
print(t.base, t.layers, t.kernel)
Template.remove("py312")
```

### `terra.exceptions` — 异常体系

```python
from terra.exceptions import (
    TerraError,           # 基类：sandbox_id, engine_error
    EngineError,          # daemon 层（启动、协议、传输）
    BuildError,           # layer / image 构建失败
    SandboxError,         # 沙箱相关错误基类
    SandboxTimeoutError,  # 操作超时
    SandboxStateError,    # 沙箱状态无效（已停止、已归还等）
    ResourceError,        # 资源耗尽（OOM、磁盘满）
    ExecError,            # exec 非零退出码（含 exec_result 属性）
)

try:
    sb.exec(["false"], check=True)
except ExecError as e:
    print(e.exec_result.exit_code)   # 1
    print(e.exec_result.stderr)      # ""
except SandboxTimeoutError:
    print("timed out")
except TerraError as e:
    print(f"engine: {e.engine_error} on {e.sandbox_id}")
```

## 底层 API

### 模块级（直连，`terra.direct`）

| 函数 | 说明 |
|---|---|
| `terra.create(name=None, *, layers=None, kernel=None, initramfs=None, cpus=None, memory_mb=None, net=None)` | 建 VM；名字/内核/initramfs 全部自动解析 |
| `terra.list_vms()` | 列出 VM |
| `terra.connect(address, token=None)` | 默认会话指向（远程）daemon；此后 create 走池分配 |
| `terra.configure(HostConfig)` | 直连模式下一次性声明宿主配置（须在首次调用前） |

### `TerraClient`（显式客户端）

`TerraClient(socket_path=None, token=None)`——`socket_path` 可为 unix
路径或 `tcp://host:port`；缺省读 `TERRA_SOCKET` 环境变量，再退化到
托管默认路径（自动跳过无权限的 root 占用）。

方法：`vm_create / vm_list / vm_info / vm_resize / vm_shutdown /
vm_kill / vm_destroy / vm_exec(name, args, timeout_secs=60)` /
`vm_attach_fs / vm_detach_fs` / `pool_create / pool_list / pool_claim /
pool_release`。错误统一抛 `TerraError`。

### `Vm`

`info() / resize(cpus=, memory_bytes=) / exec(args, timeout_secs=) /
shutdown() / kill() / destroy()`。池分配的 VM（`pooled`）的
`destroy()` 自动变为 `pool_release`；`release()` 是其别名。

### `HostConfig`（daemon 可编程）

字段：`kernel / agent_initramfs / virtiofs_initramfs / layer_dir /
state_dir / ch_binary / virtiofsd / pool_size / default_cpus /
default_memory_mb / default_layers / default_net / token`。
`Daemon(config=...)` 将其注入引擎环境；引擎只做薄运行时。

### `terra.daemon.Daemon`

`Daemon(config=None, socket=None, tcp=None, ...)`——启动引擎（自动
解析引擎二进制：环境变量 → 托管 bin → repo 构建 → PATH → 制品 URL），
`start()/stop()`，可作上下文管理器。

SDK 用户通常无需手动管理 daemon——`Sandbox` / `Pool` 在首次使用时自动启动托管引擎。

### `terra.assets` / `terra.images` / `terra.paths`

- `assets.ensure_ch() / ensure_virtiofsd() / ensure_erofs_tools() /
  ensure_engine() / publish_engine()`：二进制自动获取（GitHub/apt 解包/cargo/托管 bin）
- `images.ensure(name) / ensure_all() / build_layer(src, name)`：guest
  镜像（repo 构建或 `TERRA_ARTIFACT_BASE` 制品）与 EROFS 层打包
- `paths`：托管目录布局（`~/.local/share/terra/{bin,images,layers,state,run}`）

## `python -m terra`（CLI）

### 命令分组

| 命令组 | 子命令 | 说明 |
|---|---|---|
| `sandbox` | `create`, `ls`, `info`, `exec`, `cp`, `resize`, `metrics`, `kill`, `destroy-tenant` | 高层沙箱操作（推荐入口，租户优先） |
| `template` | `ls`, `create`, `info`, `remove` | 命名模板管理 |
| `image` | `build kernel`, `build rootfs`, `build initramfs`, `ls`, `info`, `remove` | guest 镜像管理（内核/rootfs/initramfs 合并） |
| `layer` | `ls`, `create`, `info`, `remove` | 文件系统层管理 |
| `vm` | `create`, `ls`, `info`, `exec`, `resize`, `attach`, `detach`, `shutdown`, `kill`, `destroy` | 底层 VM 操作 |
| `pool` | `create`, `ls`, `claim`, `release`, `scale`, `remove` | 预热池管理 |
| `net` | `create`, `ls`, `remove` | NAT 网络管理 |
| `daemon` | `start`, `status`, `config`, `logs [-f]`, `stop`, `destroy` | 引擎 daemon 生命周期 |

### 全局选项

- `--json`：机器可读 JSON 输出
- `--socket <path|tcp://host:port>`：指定 daemon 连接地址
- `-v` / `--verbose`：详细输出（dict 按 JSON 格式打印）

### 退出码

| 码 | 含义 |
|---|---|
| 0 | 成功 |
| 1 | 一般错误 |
| 2 | 用法错误（参数不合法） |
| 3 | daemon 错误（未运行、连接拒绝、权限不足） |
| 4 | 未找到（VM、sandbox、image 等不存在） |
| 5 | 超时 |

### 典型工作流

```bash
# 1) 首次搭建
sudo env "PATH=$PATH" terra daemon start                    # 启动引擎（root 才有 NAT 网络）
terra image build kernel -n k612 --version 6.12              # 构建内核
terra image build rootfs -n alpine                           # 构建 alpine base rootfs

# 2) 高层沙箱（推荐日常使用）
terra template create -n py312 --kernel k612 --rootfs alpine --layers python312,base
terra sandbox create --tenant research-team --template py312 --net    # 租户优先沙箱
terra sandbox ls                                                      # 列出沙箱
terra sandbox exec tenant-research-team/sb-a3f2 -- python3 --version  # 执行命令
terra sandbox metrics tenant-research-team/sb-a3f2                    # 查看资源
terra sandbox kill tenant-research-team/sb-a3f2                       # 终止会话（保留 VM）
terra sandbox destroy-tenant research-team                            # 销毁 VM + 全部会话

# 3) 预热池（批量任务）
terra pool create -n mypool --size 3                        # 创建 3 个预热 VM 的池
terra pool claim --template py312                           # 认领就绪 Sandbox
terra pool release <vm-name>                                # 归还池
terra pool scale --size 5                                   # 缩放池大小

# 4) 直接 VM（精细控制）
terra vm create dev --kernel k612 --layers base --net
terra vm exec dev -- python3 --version
terra vm attach dev --layers python312                      # 热插 layer
terra vm detach dev                                         # 卸载 layer
terra vm destroy dev

# 5) daemon 运维
terra daemon status                                         # 查看状态
terra daemon logs -f                                        # 实时日志
terra daemon config                                         # 引擎/池/网络/层一览
terra daemon stop                                           # 优雅停止
```

### 命令参考

**sandbox** — 高层沙箱（租户优先）

```
terra sandbox create --tenant <name> --template <name> [--kernel <var>] [--cpu N] [--memory MB]
                     [--net] [--env KEY=VALUE] [--timeout SEC] [--name <id>]
terra sandbox ls
terra sandbox info <id>          # id 格式：tenant-<tenant>/sb-<hex>
terra sandbox exec <id> [--cwd PATH] [--env KEY=VALUE] [--timeout SEC] -- COMMAND...
terra sandbox cp <src> <dst>    # 本地路径 或 <sandbox-id>:/path
terra sandbox resize <id> --cpu N --memory MB
terra sandbox metrics <id>
terra sandbox kill <id>          # 终止会话（不销毁 VM）
terra sandbox destroy-tenant <tenant-name>  # 销毁租户 VM 及全部会话
```

**template** — 命名模板

```
terra template ls
terra template create -n <name> --kernel <var> --rootfs alpine|ubuntu [--layers L1,L2]
terra template info <name>
terra template remove <name>
```

**image** — guest 镜像（内核/rootfs/initramfs 统一入口）

```
terra image ls
terra image build kernel -n <name> --version <ver>
terra image build rootfs -n <name>         # alpine 或 ubuntu
terra image build initramfs --type virtiofs|agent [-n <name>]
terra image info <name>
terra image remove -n <name>
```

**layer** — 文件系统层

```
terra layer ls
terra layer create -n <name> --rootfs alpine|ubuntu (--from-dir|--script|--from-image) [--kernel <var>]
terra layer info <name>
terra layer remove -n <name>
```

**vm** — 底层 VM

```
terra vm create <name> --kernel <var> [--layers L1,L2] [--net] [--cpus N] [--memory MB]
terra vm ls
terra vm info <name>
terra vm exec <name> [--timeout SEC] -- COMMAND...
terra vm resize <name> --cpus N --memory-bytes B
terra vm attach <name> --layers L1,L2   # 替代旧 attach-fs
terra vm detach <name>                  # 替代旧 detach-fs
terra vm shutdown|kill|destroy <name>
```

**pool** — 预热池

```
terra pool create -n <name> --size N [--kernel <var>] [--net]
terra pool ls
terra pool claim --template <name>  或  --layers L1,L2
terra pool release <name>
terra pool scale --size N
terra pool remove <name>
```

**net** — NAT 网络

```
terra net create [-n <name>]
terra net ls
terra net remove <name>
```

**daemon** — 引擎 daemon

```
terra daemon start [--daemonize] [--tcp host:port]
terra daemon status
terra daemon config
terra daemon logs [-f]
terra daemon stop
terra daemon destroy
```

### 向后兼容

`vm attach-fs` / `vm detach-fs` 作为隐藏别名保留，功能同 `vm attach` / `vm detach`。
`daemon ls` 已重命名为 `daemon status`。
`kernel`、`rootfs` 顶层命令已合并入 `image` 命令组。
