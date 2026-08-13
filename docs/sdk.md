# Python SDK 接口文档

安装：`pip install -e .`（从仓库根目录，同时获得 `python -m terra` 与 `terra` 命令）。

## 层次化 API

Terrarium SDK 提供三层 API，从高层到低层：

| 层次 | 类 / 函数 | 适用场景 |
|---|---|---|
| **高层沙箱** | `terra.Sandbox`, `terra.AsyncSandbox` | 开箱即用，自动起 daemon，context manager 自动回收 |
| **池 / 模板** | `terra.Pool`, `terra.template.Template` | 预热池 + 命名环境组合，适合批量任务（`Template` 不在包顶层导出，需 `from terra.template import Template`） |
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
Sandbox 是**引擎级实体**（`sandbox_*` 协议族）：引擎维护注册表
tenant → VM、sandbox id → workdir，全部客户端（SDK/CLI）共享同一视图。

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
print(sb.id)        # 引擎分配的 id："sb-a3f2b1c4"（sb-<12hex>）
print(sb.tenant)    # 租户标识："my-org"
print(sb.vm)        # VM 名称：冷启动 "tenant-my-org"；池认领则 "pool-N"
print(sb.pool_backed)  # True → 租户 VM 来自预热池（毫秒级热启动）
print(sb.status)    # "running" / "stopped" / "paused"
print(sb.backend)   # "ch" (Cloud Hypervisor)

# 多 Sandbox 共享同一租户 VM
sb1 = Sandbox(tenant="research-team", template="py312")
sb2 = Sandbox(tenant="research-team")   # 复用同一 VM，新工作目录
sb3 = Sandbox(tenant="research-team")
# sb1、sb2、sb3 在同一个 VM 内，各自拥有独立的 /workdir

# exec — blocking 执行，返回 ExecResult；默认经 confine（Landlock fs + seccomp + cgroup）沙箱化
result = sb.exec(["python3", "-c", "print(1+1)"])
print(result.stdout, result.stderr, result.exit_code)

# sandboxed=False — 逃生舱：不经权限隔离执行（如调试、安装系统软件包）
result = sb.exec(["apk", "add", "curl"], sandboxed=False)

# check=True — 非零退出码自动抛 ExecError
try:
    sb.exec(["false"], check=True)
except ExecError as e:
    print(f"exit={e.exec_result.exit_code} stderr={e.exec_result.stderr}")

# 自定义 cwd / env / timeout
result = sb.exec(["ls", "-la"], cwd="/tmp", env={"LANG": "C.UTF-8"}, timeout=30)

# 执行策略（policy）——创建时存放，引擎回显于 sb.policy
sb = Sandbox(tenant="my-org", template="py312",
             policy={"capabilities": [
                 {"Network": {"endpoint": {"host": "api.openai.com", "port": 443},
                              "direction": "Outbound"}}],
                 "limits": {"memory_mb": 512}})
print(sb.policy)    # {"capabilities": [...], "limits": {"memory_mb": 512}, ...}

# exec(policy=...) — 单次覆盖（整体替换已存策略，仅该次调用生效）
result = sb.exec(["make", "test"],
                 policy={"capabilities": [
                     {"File": {"path": {"Prefix": "/output"},
                               "access": "ReadWrite"}}],
                     "limits": {"procs": 20}})

# background=True — 后台执行，立即返回 Session 句柄（引擎跟踪，非阻塞）
session = sb.exec(["sh", "-c", "sleep 30 && echo done"], background=True)
print(session.session_id)        # "ses-<hex>"（引擎分配）
print(session.sandbox_id)        # 所属 sandbox id
print(session.status())          # {session_id, vm_name, args, status, exit_code, stdout, stderr, sandbox}
session.kill()                   # {session_id, status: "killed"}
# 注意：后台执行要求引擎级 sandbox（Pool.acquire 认领的旧路径不支持，抛 TerraError）

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
sb.kill()                # 终止本会话全部 running 会话 + 移除工作目录，VM 保留供同租户其余 Sandbox 使用
Sandbox.destroy_tenant("my-org")  # 销毁/归还租户 VM 及全部会话（池 VM 洗净归还，冷启动 VM 销毁）

# Context manager — 自动 kill
with Sandbox(tenant="my-org", template="py312") as sb:
    print(sb.exec(["uname", "-a"]).stdout)
```

> **注意：** `sb.kill()` 终止当前 sandbox 的 running 会话并移除其工作目录（引擎级 `sandbox_kill`），不会销毁 VM（同租户的其他 Sandbox 仍需使用）。要完全销毁租户 VM，使用 `Sandbox.destroy_tenant("tenant-name")`（引擎级 `tenant_destroy`，级联回收全部 sandbox；若该租户 VM 来自预热池则洗净归还池中复用，而非销毁）。
>
> **预热池集成：** 租户首次创建 sandbox 时，引擎优先从预热池认领一台空闲 VM（`pool` 参数，默认 True，毫秒级热启动）；池空或无池则回退冷启动（秒级）。`Sandbox(..., pool=False)` 强制冷启动专用 `tenant-<t>` VM。池认领的租户 VM 名是 `pool-N`（`sb.pool_backed == True`），同租户后续 sandbox 依旧复用该 VM。

> **沙箱化执行（`sandboxed=True`，默认）：** 命令在 guest 内默认经 confine
> （Landlock fs + seccomp 网络监管 + cgroup，烘焙进层；sandlock 为备选后端）
> 约束运行。默认策略：系统目录只读，仅本会话工作目录与 `/tmp` 可写，
> 同 VM 其他会话的工作目录不可达，出网默认拒绝。镜像缺少限制器二进制
> 时是硬错误（无静默回退）。仅当确实需要写系统路径（如调试、安装软件包）
> 时才用 `sandboxed=False`。
>
> **policy dict**（`Sandbox(policy=...)` 存放、`exec(policy=...)` 单次覆盖）：
> 能力化、默认拒绝、显式授予（协议 `SandboxPolicy`，见 `docs/protocol.md`）。
> `capabilities` 为能力列表（单键对象）：`File` 用
> `{"path": {"Prefix"|"Exact": "/abs"}, "access": "Read"|"ReadWrite"|"Execute"}`
> 授予路径访问（路径须绝对）；`Network` 用
> `{"endpoint": {"host": HOST, "port": N}, "direction": "Outbound"}`
> 放行出站端点（port 省略 = 任意端口）；`Device` 授予设备路径。
> `limits` 为逻辑资源配额（`memory_mb` / `procs` 等，⊆ VM 配额）；
> `default` 仅支持 `"deny"`（SDK 拒绝 `"allow"`）；`audit` / `version` 可选。
> 给出的 policy 是**自包含**的——整体替换引擎默认能力集（不叠加）；完全不传
> 时引擎注入默认策略（只读系统目录 + `/tmp` 读写），沙箱化 exec 恒携带完整
> 策略。policy 不能配 `sandboxed=False`（报错）。后端未实现 `Execute` /
> `Inbound` / `Device`——guest-proxy 返回硬错误。
> 注意：`Network` 出站 ACL 尚未在带 NAT 的环境实测（需 root daemon）。
> 具备带 NAT 的 root daemon 环境可实测 e2e——`sdk/python/tests/test_sandbox.py`
> 的 `TestSandboxPolicy` 类：root daemon 启动（`sudo terra daemon start`）后
> 执行 `python3 -m pytest sdk/python/tests/test_sandbox.py::TestSandboxPolicy -v`；
> 无 root/NAT 时该测试会自动 skip。

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

### `terra.sessions.Session` — 后台执行会话句柄

`Sandbox.exec(..., background=True)` 返回的引擎跟踪句柄（非阻塞）：
`session_id` / `sandbox_id` 属性；`status()` 查询 `{session_id, vm_name,
args, status, exit_code, stdout, stderr, sandbox}`；`kill()` 返回
`{session_id, status: "killed"}`。仅引擎级 sandbox 支持（`Pool.acquire`
认领的旧路径抛 `TerraError`）。

### `terra.exceptions` — 异常体系

```python
from terra.exceptions import (
    TerraError,           # 统一基类（client/exceptions 共用）：sandbox_id, engine_error
    SandboxError,         # 沙箱相关错误基类
    SandboxTimeoutError,  # 操作超时
    SandboxStateError,    # 沙箱状态无效（已停止、已归还等）
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
默认路径 `/tmp/terra.sock`。

方法：`vm_create / vm_list / vm_info / vm_resize / vm_shutdown /
vm_kill / vm_destroy / vm_exec(name, args, timeout_secs=60, sandbox=False)` /
`vm_attach_fs / vm_detach_fs` / `pool_create / pool_list / pool_claim /
pool_release` / `sandbox_create / sandbox_exec / sandbox_list /
sandbox_info / sandbox_kill` / `session_status / session_kill /
session_list`（后台执行会话，见 `docs/protocol.md`）/
`tenant_destroy`（引擎级沙箱实体）。错误统一抛 `TerraError`。

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

`Daemon(config=None, socket=None, tcp=None, ...)`——经 PyO3 FFI
（`terrarium_engine` crate）在进程内启动引擎，`start()/stop()`，
可作上下文管理器。默认 socket 为 `/tmp/terra.sock`。

SDK 用户通常无需手动管理 daemon——`Sandbox` / `Pool` 在首次使用时自动启动托管引擎。

### `terra.assets` / `terra.images` / `terra.paths`

- `assets.ensure_ch() / ensure_virtiofsd() / ensure_erofs_tools()`：
  二进制自动获取（GitHub/apt 解包/cargo/托管 bin）
- `images.ensure(name) / ensure_all() / build_layer(src, name)`：guest
  镜像（repo 构建或 `TERRA_ARTIFACT_BASE` 制品）与 EROFS 层打包
- `paths`：托管目录布局（`~/.local/share/terra/{bin,images,layers,templates,state,run}`）

## `python -m terra`（CLI）

### 命令分组

| 命令组 | 子命令 | 说明 |
|---|---|---|
| `setup` | — | 一键环境搭建（内核 + rootfs + initramfs + base 层 + sandlock + 模板，幂等） |
| `sandbox` | `create`, `ls`, `info`, `exec`, `cp`, `resize`, `metrics`, `kill`, `destroy-tenant` | 高层沙箱操作（引擎级实体，租户优先） |
| `tool` | `create`, `ls`, `remove` | 工具层（在发行版模板上构建） |
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
# 1) 首次搭建（一条命令，幂等）
terra setup alpine            # 内核 + rootfs + initramfs + base 层（含 sandlock）+ 模板；ubuntu 用 terra setup ubuntu
terra daemon start            # 启动引擎（自动 sudo 提权；--no-root 免提权，仅无 --net VM）

# 2) 高层沙箱（推荐日常使用）
terra sandbox create --template alpine --net    # 输出引擎 id 形如 sb-a3f2b1c4
terra sandbox ls                                # 引擎注册表中的全部 sandbox
terra sandbox exec sb-a3f2b1c4 -- echo hi       # 默认 confine 沙箱化
terra sandbox exec sb-a3f2b1c4 --detach -- sleep 60   # 后台执行：立即返回 session_id
terra sandbox session status <session_id>       # 查询后台会话状态
terra sandbox session ls                        # 列出全部后台会话
terra sandbox session kill <session_id>         # 终止后台会话
terra sandbox metrics sb-a3f2b1c4               # 查看资源（CPU/内存属租户 VM）
terra sandbox kill sb-a3f2b1c4                  # 终止会话 + 删工作目录，VM 保留
terra sandbox destroy-tenant <tenant>           # 销毁租户 VM + 全部 sandbox

# 3) 预热池（批量任务）
terra pool create --size 3                                  # 创建 3 个预热 VM 的池
terra pool claim --template alpine                          # 认领就绪 Sandbox
terra pool release <vm-name>                                # 归还池
terra pool scale --size 5                                   # 缩放池大小

# 4) 直接 VM（精细控制）
terra vm create dev --kernel default --layers base --net
terra vm exec dev -- echo hi
terra vm attach dev --layers python312                      # 热插 layer
terra vm detach dev                                         # 卸载 layer
terra vm destroy dev

# 5) 工具层（在发行版模板上构建）
terra tool create -n python312 --template alpine --script images/examples/python312.sh
terra tool ls
terra tool remove -n python312

# 6) daemon 运维
terra daemon status                                         # 查看状态
terra daemon logs -f                                        # 实时日志
terra daemon config                                         # 引擎/池/网络/层一览
terra daemon stop                                           # 优雅停止
```

### 命令参考

**sandbox** — 高层沙箱（引擎级实体：id 形如 `sb-<12hex>`）

```
terra sandbox create [--template <name>] [--layers L1,L2] [--kernel <var>]
                     [--cpu N] [--memory MB] [--net] [--env KEY=VALUE] [--timeout SEC]
                     [--read-path PATH]... [--write-path PATH]...
                     [--net-allow HOST[:PORT]]... [--memory-mb N] [--procs N]
terra sandbox ls                     # 列出引擎注册表中的 sandbox（含 policy）
terra sandbox info <id>              # 单个 sandbox（含存入的 policy）
terra sandbox exec <id> [--cwd PATH] [--env KEY=VALUE] [--timeout SEC]
                      [--no-sandbox] [--detach] [--net-allow HOST[:PORT]]... -- COMMAND...
terra sandbox session status <session_id>   # 后台会话状态
terra sandbox session kill <session_id>     # 终止后台会话
terra sandbox session ls                    # 列出全部后台会话
terra sandbox cp <src> <dst>    # 本地路径 或 <id>:/path
terra sandbox resize <id> [--cpu N] [--memory MB]   # 作用于整个租户 VM
terra sandbox metrics <id>
terra sandbox kill <id>          # 终止该 sandbox 的会话 + 删工作目录，租户 VM 保留
terra sandbox destroy-tenant <tenant>   # 销毁租户 VM 及其全部 sandbox
```

`sandbox` 命令直接操作引擎注册表中的沙箱实体（`sandbox_*` 协议族）。
`cp` / `resize` / `metrics` 接受 sandbox id 或 VM 名（id 解析失败时按
VM 名回退）。`exec` 默认经 confine（Landlock fs + seccomp + cgroup）沙箱化执行（同
Python API 的 `sandboxed=True`）；`--no-sandbox` 为逃生舱。镜像缺少
sandlock 二进制时报错（先跑 `terra setup` 将其烘焙进系统层）。
`exec --detach` 后台执行：立即返回 `{session_id, sandbox, status}`，
会话由引擎跟踪（协议 `session_status`/`session_kill`/`session_list`，
同 SDK `Sandbox.exec(background=True)`），用 `sandbox session`
子命令查询/终止。

policy 旗标（`create` 存放、`exec` 单次覆盖）映射到协议 `policy` 对象的能力形状：
`--read-path PATH` → `File { path: Prefix, access: Read }`；`--write-path PATH` →
`File { path: Prefix, access: ReadWrite }`；`--net-allow HOST[:PORT]` →
`Network { endpoint, direction: Outbound }`（出站默认拒绝、仅放行所列端点，
至少给一条）；`--memory-mb N` / `--procs N` → `limits.memory_mb` /
`limits.procs`。路径须绝对；给出的 policy 自包含（替换引擎默认能力集，
不叠加）；与 `--no-sandbox` 同用会报错（策略只在沙箱化执行时有意义）。

**tool** — 工具层（系统资源由 `terra setup` 准备；工具层一律用 `tool create` 构建）

```
terra tool create -n <name> --template <distro> [--script <file>] [--no-net] [--timeout SEC]
terra tool ls
terra tool remove -n <name>
```

**vm** — 底层 VM

```
terra vm create <name> --kernel <var> [--layers L1,L2] [--net] [--cpus N] [--memory MB]
terra vm ls
terra vm info <name>
terra vm exec <name> [--timeout SEC] -- COMMAND...
terra vm resize <name> --cpus N --memory-bytes B
terra vm snapshot <name> [--path DIR]           # P1 快速重置：捕获就绪态（捕获后暂停）
terra vm restore <name> --snapshot DIR [--layers L1,L2]
terra vm attach <name> --layers L1,L2   # 替代旧 attach-fs
terra vm detach <name>                  # 替代旧 detach-fs
terra vm shutdown|kill|destroy <name>
```

**pool** — 预热池

```
terra pool create --size N [--kernel <var>] [--net]
terra pool ls
terra pool claim --template <name>  或  --layers L1,L2
terra pool release <name>
terra pool scale --size N
terra pool remove <name>
```

**net** — NAT 网络

```
terra net create
terra net ls
terra net remove <name>
```

**setup** — 一键环境搭建（幂等，各步骤已存在即跳过，`--force` 强制重建）

```
terra setup [alpine|ubuntu] [--kernel-version <ver>] [--force]
```

步骤：宿主二进制 → 默认内核 → 发行版 rootfs → initramfs（agent +
virtiofs）→ base 层 → **guest 二进制**（构建静态 musl `sandlock`
——`images/build-sandlock.sh`，钉在上游 tag `go/v0.8.5` 加本地
`thirdparty/` 补丁——并连同当前 `guest-proxy` 一起装进系统层
`/usr/bin/sandlock` 与 `bin/guest-proxy`）→ 模板（与发行版同名）。

**daemon** — 引擎 daemon

```
terra daemon start [--no-root] [--tcp host:port]   # 非 root 自动 sudo 提权；--no-root 免提权
terra daemon status
terra daemon config
terra daemon logs [-f]
terra daemon stop
terra daemon destroy
```

### 向后兼容

`vm attach-fs` / `vm detach-fs` 作为隐藏别名保留，功能同 `vm attach` / `vm detach`。
`daemon ls` 已重命名为 `daemon status`。
`template`、`image`、`layer` 命令组已移除——系统资源统一由 `terra setup` 准备，工具层用 `tool create` 构建；模板由 `terra setup` 或 SDK 的 `Template` 类写入。
