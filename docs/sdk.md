# Python SDK 接口文档

安装：`pip install -e sdk/python`（同时获得 `python -m terra` 与 `terra` 命令）。

## 三种使用方式

```python
import terra

# 1) 直连（临时 VM）：什么都不用管，首次调用自动起托管引擎
vm = terra.create(layers=["python312", "base"])
vm.exec(["python3", "--version"])
vm.destroy()

# 2) 远程客户端：一行 connect，之后代码与直连完全一致
terra.connect("tcp://server:19099", token="secret")
vm = terra.create(layers=["python312", "base"])   # 底层走 pool_claim
vm.destroy()                                       # 底层走 pool_release

# 3) 自己做管理员：一个 Python 脚本就是 daemon 程序
from terra.daemon import Daemon
from terra.config import HostConfig
Daemon(config=HostConfig(layer_dir="/var/lib/terra/layers",
                         pool_size=4, token="secret"),
       tcp="0.0.0.0:19099").start()
```

## API

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
pool_release`。错误统一抛 `TerraError`（含引擎错误文本）。

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
`start()/stop()`，可作上下文管理器。socket 被他人占用时自动改用私有 socket。

### `terra.assets` / `terra.images` / `terra.paths`

- `assets.ensure_ch() / ensure_virtiofsd() / ensure_erofs_tools() /
  ensure_engine() / publish_engine()`：二进制自动获取（GitHub/apt 解包/cargo/托管 bin）
- `images.ensure(name) / ensure_all() / build_layer(src, name)`：guest
  镜像（repo 构建或 `TERRA_ARTIFACT_BASE` 制品）与 EROFS 层打包
- `paths`：托管目录布局（`~/.local/share/terra/{bin,images,layers,state,run}`）

## `python -m terra`（CLI）

资源分组 + 统一动词（`ls / create / remove [-n 名字]`）：

- `vm ls/create/remove/info/exec/resize/shutdown/kill/attach-fs/detach-fs`
（`--kernel`/`--rootfs` 均可传绝对路径或 ls 出来的变体名，如
`--kernel k612 --rootfs alpine`。`rootfs ls` 只显示系统镜像；
两个引导器镜像（virtiofs/agent）是内部基础设施——带 `--layers`
时引导器自动选择，用户无需关心）
- `kernel ls/create -n <名> --version/remove -n`
- `rootfs ls/create -n <名> [alpine|ubuntu]/remove -n`
- `layer ls/create -n <名> (--from-dir|--script|--from-image) --rootfs <alpine|ubuntu> [--kernel <名>]/remove -n`
（`--rootfs` 必填，指定构建在哪个系统上；`--script` 构建需显式指定 `--kernel`，无默认值）
- `pool ls/create/remove/claim/release`（池大小**运行时可调**：
create 追加、remove 缩减，无需重启 daemon）
- `net ls/create/remove`、`daemon start/ls/stop/destroy/config [--tcp]`

层与镜像统一为「托管目录命名工件」：`layer create -n <名> --from-image`
把系统 rootfs 铺成 `layers/<name>/`（且 `layer ls` 只显示附加层，系统层隐藏）；
`layer create -n <名> --script` 在 builder VM 中「做中建」，增量打包为
`<名>.erofs`。系统 base 本身通过 `rootfs create` 创建，不走 `layer
create`。

**目录即标识**：工件分区存放——内核在 `images/kernels/<name>/
vmlinux.bin`，rootfs/initramfs 在 `images/rootfs/...`（旧布局自动
迁移）。使用时只写名字，内部按约定解析：
`terra.create(kernel="k612")` / `vm create --kernel k612`。
rootfs 同理（`rootfs create -n alpine321` 产 `images/rootfs/alpine321/`）。

连接：`--socket <path|tcp://host:port>` 或 `TERRA_SOCKET` /
`TERRA_TOKEN`。
