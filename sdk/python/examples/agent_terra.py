#!/usr/bin/env python3
"""Real-agent demo: an LLM agent that executes tasks inside Terrarium sandboxes.

The agent has no direct filesystem/shell access of its own. Every command it
needs to run is dispatched through the terra-mcp server (MCP protocol), which
executes it inside an isolated Terrarium VM session (sandlock-constrained by
default). The agent loop is plain OpenAI-compatible function calling so it
works against any compatible provider (deepseek here; OpenAI/Anthropic work
the same way).

Usage:
    export TERRA_SOCKET=/tmp/terra.sock          # daemon socket (default)
    /tmp/terrarium-venv/bin/python examples/agent_terra.py \
        --task "Write fib.py ... run it and report the result"
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
from pathlib import Path
from typing import Any

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client


REPO_ROOT = Path(__file__).resolve().parents[3]
MCP_BIN = REPO_ROOT / "target" / "debug" / "terra-mcp"


def mcp_env() -> dict[str, str]:
    """Environment for the terra-mcp child: point it at the daemon socket and
    give it the local kernel/initramfs so cold-booted sandboxes can start."""
    env = {**os.environ}
    env.setdefault("TERRA_SOCKET", "/tmp/terra.sock")
    terra_home = Path(os.environ.get("TERRA_HOME", Path.home() / ".local/share/terra"))
    if "TERRA_KERNEL" not in env:
        cand = terra_home / "images" / "kernels" / "default" / "vmlinux.bin"
        if cand.exists():
            env["TERRA_KERNEL"] = str(cand)
    if "TERRA_INITRAMFS" not in env:
        cand = terra_home / "images" / "rootfs" / "initramfs-virtiofs.cpio.gz"
        if cand.exists():
            env["TERRA_INITRAMFS"] = str(cand)
    return env


SYSTEM_PROMPT = """\
You are a coding agent running inside an automated evaluation harness.
You have NO direct access to the host machine. The only way to run commands,
read or write files is through the provided tools, which execute inside an
isolated Terrarium sandbox VM. Always use those tools; never pretend to run
commands directly.

Work plan:
1. Inspect the task and decide what files/commands are needed.
2. Use terra_exec / terra_session_write / terra_session_read to do the work
   inside the sandbox.
3. Verify your result by running the code, then report the verified output.

Keep sessions per task (e.g. session="agent-demo") so workdirs stay isolated.
If a command fails, read the error and fix the code before giving up.
"""


def tool_to_openai_schema(tool: Any) -> dict:
    return {
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description or "",
            "parameters": tool.input_schema or {"type": "object", "properties": {}},
        },
    }


def tool_result_to_text(result: Any) -> str:
    parts: list[str] = []
    content = getattr(result, "content", None) or []
    for block in content:
        t = getattr(block, "type", None)
        if t == "text":
            parts.append(getattr(block, "text", ""))
        else:
            structured = getattr(block, "structuredContent", None)
            if structured is not None:
                parts.append(json.dumps(structured, ensure_ascii=False, default=str))
    if getattr(result, "isError", False):
        return "TOOL ERROR: " + "\n".join(parts)
    return "\n".join(parts) if parts else "(empty result)"


async def run_agent(task: str, model: str, api_key: str, base_url: str, max_steps: int) -> None:
    from openai import AsyncOpenAI

    client = AsyncOpenAI(api_key=api_key, base_url=base_url)

    mcp_params = StdioServerParameters(command=str(MCP_BIN), args=[], env=mcp_env())
    async with stdio_client(mcp_params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            tools_result = await session.list_tools()
            tools = [tool_to_openai_schema(t) for t in tools_result.tools]
            tool_names = [t["function"]["name"] for t in tools]
            print(f"[agent] connected to terra-mcp, {len(tools)} tools: {tool_names}")

            messages: list[dict[str, Any]] = [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": task},
            ]

            for step in range(1, max_steps + 1):
                response = await client.chat.completions.create(
                    model=model,
                    messages=messages,
                    tools=tools,
                )
                choice = response.choices[0]
                msg = choice.message

                if not msg.tool_calls:
                    print(f"\n[agent] final answer after {step} step(s):\n{msg.content}")
                    return

                messages.append(
                    {
                        "role": "assistant",
                        "content": msg.content,
                        "tool_calls": [
                            {
                                "id": tc.id,
                                "type": "function",
                                "function": {"name": tc.function.name, "arguments": tc.function.arguments},
                            }
                            for tc in msg.tool_calls
                        ],
                    }
                )

                for tc in msg.tool_calls:
                    name = tc.function.name
                    try:
                        arguments = json.loads(tc.function.arguments or "{}")
                    except json.JSONDecodeError:
                        arguments = {}
                    print(f"\n[step {step}] calling {name}({json.dumps(arguments, ensure_ascii=False)})")
                    result = await session.call_tool(name, arguments=arguments)
                    text = tool_result_to_text(result)
                    print(f"-> {text[:500]}")
                    messages.append(
                        {"role": "tool", "tool_call_id": tc.id, "content": text[:8000]}
                    )

            print("[agent] reached max steps without a final answer; aborting.")


def main() -> int:
    parser = argparse.ArgumentParser(description="LLM agent that works inside Terrarium sandboxes")
    parser.add_argument("--task", default=None, help="task to run (default: fibonacci demo)")
    parser.add_argument("--model", default="deepseek-v4-flash")
    parser.add_argument("--base-url", default="https://api.deepseek.com/v1")
    parser.add_argument("--max-steps", type=int, default=12)
    args = parser.parse_args()

    api_key = os.environ.get("DEEPSEEK_API_KEY")
    if not api_key:
        # Fall back to the key found in ~/.codex/config.toml (user's own provider).
        codex_cfg = Path.home() / ".codex" / "config.toml"
        if codex_cfg.exists():
            for line in codex_cfg.read_text().splitlines():
                if "experimental_bearer_token" in line:
                    api_key = line.split("=", 1)[1].strip().strip('"')
                    break
    if not api_key:
        print("No API key: set DEEPSEEK_API_KEY (or configure ~/.codex/config.toml).", file=sys.stderr)
        return 1

    task = args.task or (
        'Inside the sandbox (session "agent-demo"), write a Python script fib.py '
        "that computes the 40th Fibonacci number using dynamic programming and prints it. "
        "Run it, verify the output is 102334155, then report the verified result."
    )
    asyncio.run(run_agent(task, args.model, api_key, args.base_url, args.max_steps))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
