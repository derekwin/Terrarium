# 真实 Agent 应用教程：让 LLM agent 在 Terrarium 沙箱里执行任务

本文用一个可运行的 agent 应用（`sdk/python/examples/agent_terra.py`）演示
Terrarium 的核心用法：**LLM agent 通过 MCP 接口把任务放进隔离沙箱执行**。
agent 本身没有任何宿主文件系统 / shell 访问权——它唯一的执行通道是
`terra-mcp` 暴露的 `terra_*` 工具，每个命令都在隔离 VM 会话中运行
（默认经 sandlock 约束）。

读完本文你可以：跑通一个真实 agent 编码任务、理解 agent 循环与 MCP 的
衔接方式、把示例换成你自己的任务或框架（LangGraph / Claude SDK / Codex）。

## 1. 前置条件

- Terrarium daemon 已启动（用户手动 sudo 启动，见 `terra daemon start`）。
- 本地镜像已就绪：`$TERRA_HOME/images/kernels/default/vmlinux.bin` 与
  `$TERRA_HOME/images/rootfs/initramfs-virtiofs.cpio.gz`（`terra setup`
  或构建产物会生成）。
- Python 3.10+ 环境，装有 `openai` 与 `mcp` 两个包。
- 任意 OpenAI 兼容的 LLM API key（示例默认 deepseek，也可以换 OpenAI /
  Anthropic 兼容端点）。

## 2. 准备环境

```bash
# 安装依赖（已有 /tmp/terrarium-venv 则直接复用）
/tmp/terrarium-venv/bin/python -m pip install openai mcp

export DEEPSEEK_API_KEY=sk-...            # 你的模型 key
export TERRA_SOCKET=/tmp/terra.sock       # daemon socket
export TERRA_HOME=/home/liujinyao/.local/share/terra
```

## 3. 跑一个真实任务

```bash
cd /home/liujinyao/2606/Terrarium
/tmp/terrarium-venv/bin/python sdk/python/examples/agent_terra.py
```

默认任务：在沙箱会话 `agent-demo` 里写 `fib.py`，用动态规划计算第 40 个
斐波那契数并运行验证。一次真实运行的输出（节选）：

```
[agent] connected to terra-mcp, 18 tools: ['terra_vm_create', ..., 'terra_exec', ...]
[step 1] calling terra_session_write({"session": "agent-demo", "path": "/home/fib.py", ...})
-> {"status":"ok","data":{...,"stderr":"cannot create /home/fib.py: Permission denied",...}}
[step 3] calling terra_exec({"args": ["pwd"], "session": "agent-demo"})
-> {"status":"ok","data":{...,"stdout":"/workdir/sb-d91eaebb50aa\n"}}
[step 6] calling terra_session_write({"path": "/workdir/sb-d91eaebb50aa/fib.py", ...})
-> ok
[step 7] calling terra_exec({"args": ["python3", "/workdir/sb-d91eaebb50aa/fib.py"], ...})
-> {"status":"ok","data":{...,"stdout":"102334155\n"}}
[agent] final answer: The output matches the expected value 102334155, so the result is verified.
```

注意第 1 步：agent 试图写 `/home/fib.py` 被沙箱拒绝（sandlock 只允许会话
workdir），它自主探测出 `pwd` 是 `/workdir/sb-...`，改用正确路径后成功。
这就是"agent 在真实沙箱里干活"与"agent 假装干活"的区别——错误是真实的，
修复也是 agent 自己做的。

换你自己的任务：

```bash
/tmp/terrarium-venv/bin/python sdk/python/examples/agent_terra.py \
    --task "在沙箱里 clone github.com/foo/bar，跑它的测试并汇报结果"
```

## 4. 代码结构剖析

整个应用约 200 行，核心只有三块：

### 4.1 连接 terra-mcp 并枚举工具

```python
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

params = StdioServerParameters(command=str(MCP_BIN), args=[], env=mcp_env())
async with stdio_client(params) as (read, write):
    async with ClientSession(read, write) as session:
        await session.initialize()
        tools_result = await session.list_tools()   # → 18 个 terra_* 工具
```

`mcp_env()` 会自动从 `$TERRA_HOME/images/` 解析 kernel 与 initramfs 注入
子进程环境，冷启动的沙箱才能起得来（对应错误：`Missing 'kernel' field`）。

### 4.2 把 MCP 工具映射成模型可调用的 function schema

```python
def tool_to_openai_schema(tool):
    return {
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description or "",
            "parameters": tool.input_schema or {"type": "object", "properties": {}},
        },
    }
```

这是整个集成的关键一步：MCP 的 `tools/list` 返回带 JSON Schema 的工具，
转成 OpenAI function calling 格式后，任何支持工具调用的模型都能直接用。

### 4.3 agent 循环

```python
while True:
    response = await client.chat.completions.create(model=..., messages=..., tools=tools)
    msg = response.choices[0].message
    if not msg.tool_calls:
        print(msg.content)              # 无工具调用 → 最终答案
        break
    messages.append(assistant_message)  # 回填 assistant 的工具调用
    for tc in msg.tool_calls:
        result = await session.call_tool(tc.function.name,
                                         arguments=json.loads(tc.function.arguments))
        messages.append({"role": "tool", "tool_call_id": tc.id,
                         "content": tool_result_to_text(result)})
```

每一轮：模型决定调用哪个沙箱工具 → 执行 → 把真实结果回填 → 继续，
直到模型给出最终答案（默认最多 12 轮）。工具结果截断到 8000 字符防爆上下文。

## 5. 换框架：LangGraph / Claude SDK / Codex CLI

Terrarium 的接口不绑定任何框架。换框架只做一件事：**把 `tools/list` 的
结果映射成目标框架的 tool schema**，`session.call_tool` 换成目标框架的
工具执行器。

- **LangGraph**：`create_react_agent` 的 `tools` 参数接收 MCP 工具包装成
  `BaseTool` 的列表；或直接用 `langchain-mcp-adapters`
  `load_mcp_tools()` 一行加载。
- **Claude SDK**：`anthropic.types.ToolParam` 结构与 MCP `input_schema`
  几乎一致，字段名照搬即可。
- **Codex CLI**：把 `terra-mcp` 注册进 `~/.codex/config.toml`：

  ```toml
  [mcp_servers.terrarium]
  command = "/path/to/terra-mcp"
  ```

  注意：Codex 0.146 默认把 MCP 工具延迟到 `tool_search` 机制，目前只有
  支持 `tool_search` 的模型（如部分 OpenAI 模型）能看到 `terra_*` 工具；
  deepseek 等第三方模型看不到。这是 Codex 侧限制，不是 terra-mcp 的问题。

## 6. 常见问题

| 现象 | 原因与处理 |
|---|---|
| `Missing 'kernel' field` | 冷启动没配镜像：给 terra-mcp 设 `TERRA_KERNEL` / `TERRA_INITRAMFS`（示例自动处理） |
| `execvp 'python3': No such file or directory` | base 层只有 busybox：换带 Python 的层，如 `["ci-terra", "ubuntu"]`（用 `python3.12`）；且 tenant VM 的层只在首次冷启动生效，换层后要 `terra sandbox destroy-tenant mcp` 重建 |
| `cannot create /home/...: Permission denied` | sandlock 只放行会话 workdir：让 agent 用 `pwd` 探测工作目录 |
| `sh: 0: Cannot fork` | 瞬时资源压力：让 agent 重试即可（真实场景常见） |
| agent 反复绕圈不出结果 | 提高 `--max-steps`，或把工具结果截断长度调大 |

## 7. 这个模式的意义

对 Terrarium 来说，agent 应用是"用户面"第一入口：RL 训练、Agent 生产
执行、Agent CI 三个主场景最终都要落到"某个 agent 通过接口把任务丢进
沙箱"。`terra-mcp` 让这一步变成标准协议——任何 MCP 生态的 agent 框架
开箱即用，而不需要为每个框架写专属适配层。
