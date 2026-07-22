# 0006: sandbox 控制协议与通道选型

- 状态：已接受（M2 Task 1，2026-07）
- 决策者：项目所有者

## 背景

M2 Task 1 实现 guest 内 sandboxd 守护进程，需要定义 host（controller/CLI）
与 guest（sandboxd）之间的控制通道和通信协议。Host↔guest 通信经 vsock 通道
（M1 Task 5 实现），协议需要与 vmm-api 保持一致的风格。

## 决定

### 传输层：guest 内 Unix socket → vsock 桥接

- sandboxd 在 guest 内监听 Unix domain socket（`/run/sandboxd.sock`），
  使用 serde_json 文本帧（一行 JSON + `\n`）。理由：
  1. Unix socket 是 guest 内部通信的最简方式，无需网络栈；
  2. 与 vmm-api 协议风格一致（ADR 0004），降低认知负担；
  3. 后续经 vsock 桥接到 host 时（controller → VMM vsock → guest port → Unix socket），
     JSON 帧格式无需任何改动。
- sandboxd 以静态 musl 编译（`x86_64-unknown-linux-musl`），不依赖 guest 动态库；
  二进制放入 rootfs `/sbin/sandboxd`，由 init 脚本在 switch_root 前以
  `daemon &` 方式拉起。

### 协议：serde_json 文本帧

请求（每行一条）：
```json
{"cmd":"exec","argv":["echo","hello"],"env":{},"cwd":"/"}
{"cmd":"status"}
{"cmd":"terminate"}
{"cmd":"logs"}
```

响应：
```json
{"status":"ok","data":{"exit_code":0,"stdout":"aGVsbG8K","stderr":""}}
{"status":"ok"}
{"status":"error","message":"exec: No such file"}
```

- stdout/stderr 以 base64 编码（避免二进制数据破坏 JSON 格式）；
- 不使用 serde_json 的 `Value` 包装——直接用强类型 enum 反序列化。

### M2 命令面

| 命令 | 说明 | 隔离栈 |
|---|---|---|
| `exec {argv, env, cwd}` | 在沙箱内执行命令，返回 exit_code/stdout/stderr | Task 1: 普通子进程；Task 2: 全隔离栈 |
| `status` | sandboxd 健康检查 | — |
| `terminate` | 关闭 sandboxd | — |
| `logs` | 查询沙箱日志 | Task 2 接入 |

### Host↔Guest 控制链路

```
controller/CLI (host)
  → vmm-api socket → terra-vmm
    → vsock device → guest CID=3, port=1024
      → vsock-aware proxy (M2 后续)
        → /run/sandboxd.sock (guest 内 Unix socket)
```

M2 Task 1 仅实现 guest 侧的 sandboxd + Unix socket 监听。host 侧的
vsock 桥接到 controller 由 Task 5（controller）完成。

### crate 归属

- `crates/sandboxd`：guest 内独立二进制，不依赖其他 crate；
- sandbox 定义结构（id、overlay、配额、白名单）暂时放在 sandboxd 内部，
  M2 Task 2 确定是否需要独立 `sandbox-api` crate。

## 代价与边界

- 每次 exec 启动新进程（`Command::new`），无进程池复用——M2 阶段沙箱创建频率低，
  可接受。
- base64 编码带来 ~33% 的 stdout/stderr 体积膨胀；对调试级输出可接受，
  后续可按需切换到二进制帧。
- sandboxd 当前无认证机制——host↔guest 通信经 vsock 硬件隔离边界，
  信任模型假定 VMM 未被攻破。
