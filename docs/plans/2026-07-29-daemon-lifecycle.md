# ADR: daemon 生命周期最小收敛（2026-07-29）

> 状态：草案，待批准。原则：**功能越简单越好**——只修「不诚实」和「分叉」，不引入新概念。

## 背景

daemon 有两种存活形态：嵌入式（PyO3 进 Python 进程，`Sandbox()` 默认会话）和服务式（`terra daemon start` 子进程）。两种形态本身都保留，问题是三处不诚实/分叉：

1. `Daemon.stop()` 发 `shutdown name=all`，引擎按字面 VM 名返回 not found，SDK 吞错——停止语义是假的；
2. 两条启动路径环境注入分叉：`Daemon.start` 注入 `TERRA_STATE_DIR/LAYER_DIR/CH_BINARY/VIRTIOFSD`，`DaemonManager._start_daemon` 不注入 → Sandbox 启动的 daemon 看不到托管目录下的层；
3. 死代码误导维护者：`pgrep -x engine`（cdylib 无此二进制）、`assets.ensure_engine/publish_engine`、`cmd_daemon_start` 里恒真的 `if args.daemonize or not args.daemonize`。

## 决策（就三件事）

1. **协议加一条 `daemon_stop`**：服务式响应（shutdown_all → 退出），嵌入式返回 `not_supported`。`stop()` 改发这条命令，停止语义变真。顺带修 accept 循环不响应 SIGTERM（`select!` on accept vs shutdown，几行的事）。
2. **启动路径合一**：`DaemonManager` 委托 `Daemon.start` 的同一套环境注入，单一来源；补 `TERRA_KERNEL`/`TERRA_AGENT_INITRAMFS` 默认值，修池 kernel 回退 repo 相对路径的 CWD 陷阱。
3. **删死代码**：pgrep 发现路径、ensure_engine/publish_engine、恒真分支；`daemon status` 改为 ping socket。

## 明确不做（需要时再议）

- socket 路径迁移（维持 `/tmp/terra.sock` 现状）、单实例锁、TCP 加固/打通、`daemon_ping` 命令、全局互斥锁拆分。这些都有 review 记录在案，但不进本次范围。

## 验收

- `stop()` 后服务式 daemon 进程真实退出；嵌入式调 stop 得到明确的 not_supported；
- `Sandbox()` 路径启动的 daemon 能列出托管目录下的层；
- 全仓 grep 不到 `pgrep -x engine` / `ensure_engine`。
