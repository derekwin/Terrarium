"""
Terrarium Python SDK — sandbox create / exec / terminate / ls / resize.

纯标准库（socket + json），不依赖第三方包。
"""

import json
import socket
import os
import subprocess
import asyncio

TERRA_SOCKET = os.environ.get("TERRA_SOCKET", "/tmp/terra.sock")


def _send_recv(cmd: dict) -> dict:
    """通过 Unix socket 发送 JSON 请求并读取 JSON 响应。"""
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(TERRA_SOCKET)
    sock.sendall((json.dumps(cmd) + "\n").encode())
    buf = b""
    while b"\n" not in buf:
        buf += sock.recv(4096)
    sock.close()
    return json.loads(buf.split(b"\n")[0])


class Sandbox:
    """一个沙箱实例。"""

    def __init__(self, handle: dict):
        self.vm_id = handle["vm_id"]
        self.sandbox_id = handle["sandbox_id"]

    @staticmethod
    def create(name: str, image: str = "default", **kwargs) -> "Sandbox":
        """创建沙箱并返回 Sandbox 对象。"""
        resp = _send_recv({"cmd": "create_sandbox", "name": name, "image": image, **kwargs})
        if resp.get("status") != "ok":
            raise RuntimeError(resp.get("message", "unknown error"))
        return Sandbox(resp["data"])

    def exec(self, *argv: str, cwd: str = "/", env: dict = None) -> dict:
        """在沙箱内执行命令，返回 {"exit_code": n, "stdout": "...", "stderr": "..."}。"""
        resp = _send_recv({
            "cmd": "exec",
            "vm_id": self.vm_id,
            "sandbox_id": self.sandbox_id,
            "argv": list(argv),
            "cwd": cwd,
            "env": env or {},
        })
        if resp.get("status") != "ok":
            raise RuntimeError(resp.get("message", "unknown error"))
        data = resp.get("data", {})
        # stdout/stderr are base64 encoded by sandboxd.
        import base64
        return {
            "exit_code": data.get("exit_code", -1),
            "stdout": base64.b64decode(data.get("stdout", "")).decode(errors="replace"),
            "stderr": base64.b64decode(data.get("stderr", "")).decode(errors="replace"),
        }

    def terminate(self):
        """销毁沙箱。"""
        _send_recv({
            "cmd": "terminate",
            "vm_id": self.vm_id,
            "sandbox_id": self.sandbox_id,
        })

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.terminate()


class AsyncSandbox:
    """异步版 Sandbox（使用 asyncio）。"""

    def __init__(self, handle: dict):
        self.vm_id = handle["vm_id"]
        self.sandbox_id = handle["sandbox_id"]

    @staticmethod
    async def create(name: str, image: str = "default", **kwargs) -> "AsyncSandbox":
        resp = await _aio_send_recv({"cmd": "create_sandbox", "name": name, "image": image, **kwargs})
        if resp.get("status") != "ok":
            raise RuntimeError(resp.get("message", "unknown error"))
        return AsyncSandbox(resp["data"])

    async def exec(self, *argv: str, cwd: str = "/", env: dict = None) -> dict:
        resp = await _aio_send_recv({
            "cmd": "exec",
            "vm_id": self.vm_id,
            "sandbox_id": self.sandbox_id,
            "argv": list(argv),
            "cwd": cwd,
            "env": env or {},
        })
        if resp.get("status") != "ok":
            raise RuntimeError(resp.get("message", "unknown error"))
        import base64
        data = resp.get("data", {})
        return {
            "exit_code": data.get("exit_code", -1),
            "stdout": base64.b64decode(data.get("stdout", "")).decode(errors="replace"),
            "stderr": base64.b64decode(data.get("stderr", "")).decode(errors="replace"),
        }

    async def terminate(self):
        await _aio_send_recv({
            "cmd": "terminate",
            "vm_id": self.vm_id,
            "sandbox_id": self.sandbox_id,
        })

    async def __aenter__(self):
        return self

    async def __aexit__(self, *args):
        await self.terminate()


def ls() -> list:
    """列出所有运行中的沙箱。"""
    resp = _send_recv({"cmd": "ls"})
    if resp.get("status") != "ok":
        raise RuntimeError(resp.get("message", "unknown error"))
    return resp.get("data", [])


def resize(vm_id: str, cpus: int = None, memory_mb: int = None):
    """调整 VM 资源（placeholder — M3 接入）。"""
    raise NotImplementedError("resize is not yet implemented")


async def _aio_send_recv(cmd: dict) -> dict:
    reader, writer = await asyncio.open_unix_connection(TERRA_SOCKET)
    writer.write((json.dumps(cmd) + "\n").encode())
    await writer.drain()
    data = await reader.readline()
    writer.close()
    await writer.wait_closed()
    return json.loads(data.decode())
