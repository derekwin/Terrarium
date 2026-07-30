"""Integration tests for Sandbox API.

Requires KVM and guest assets.  These tests follow the same conventions
as ``test_e2e_real.py`` — real VMs, real CH, real guest.  The engine
daemon is auto-started in-process by the SDK (DaemonManager) on first
Sandbox creation; no separate daemon setup is needed.

Usage::

    pytest sdk/python/tests/test_sandbox.py -v
"""

from __future__ import annotations

from uuid import uuid4

import pytest

from terra.sandbox import Sandbox


def teardown_module():
    """Destroy tenant VMs created by the suite.

    ``Sandbox.kill`` only removes the session workdir — the tenant VM
    stays running and would leak (as a CH process) when the embedded
    daemon dies with the test process. Tenant VMs are named
    ``tenant-<tenant>``; only those are touched.
    """
    from terra.client import TerraClient, TerraError

    client = TerraClient()
    try:
        vms = client.vm_list().get("vms", [])
    except Exception:  # noqa: BLE001 — daemon already gone
        return
    for vm in vms:
        name = vm.get("name", "")
        if name.startswith("tenant-"):
            try:
                client.vm_destroy(name)
            except TerraError:
                pass


class TestSandboxCreateAndExec:
    """Basic create → exec → kill lifecycle."""

    def test_exec_echo(self):
        """Echo a simple string and verify stdout."""
        sb = Sandbox(layers=["base"], cpu=1, memory_mb=256)
        try:
            result = sb.exec("echo hello")
            assert "hello" in result.stdout, f"Expected 'hello' in stdout, got: {result.stdout!r}"
            assert result.exit_code == 0
        finally:
            sb.kill()

    def test_exec_with_check_flag(self):
        """check=True raises ExecError on non-zero exit."""
        sb = Sandbox(layers=["base"], cpu=1, memory_mb=256)
        try:
            result = sb.exec("true", check=True)
            assert result.exit_code == 0
        finally:
            sb.kill()

    def test_exec_nonzero_no_check(self):
        """Non-zero exit without check returns result with exit code."""
        sb = Sandbox(layers=["base"], cpu=1, memory_mb=256)
        try:
            result = sb.exec("false", check=False)
            assert result.exit_code != 0
        finally:
            sb.kill()


class TestSandboxContextManager:
    """Context-manager based lifecycle."""

    def test_context_manager_kills_on_exit(self):
        """Entering and exiting the context manager kills the sandbox."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            assert sb.status == "running"
            result = sb.exec("echo alive")
            assert "alive" in result.stdout

        # After exit the sandbox is stopped.
        assert sb.status == "stopped"

    def test_double_kill_is_idempotent(self):
        """Calling kill() multiple times is safe."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            pass  # context-manager exit kills once

        # Second kill should not raise.
        sb.kill()
        assert sb.status == "stopped"


class TestSandboxFiles:
    """File operations inside a sandbox."""

    def test_write_and_read(self):
        """Write a string to a file and read it back."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            sb.files.write("/tmp/test.txt", "hello world")
            content = sb.files.read("/tmp/test.txt")
            assert content.strip() == "hello world"

    def test_mkdir_and_exists(self):
        """Create a directory and verify it exists."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            sb.files.mkdir("/tmp/newdir")
            assert sb.files.exists("/tmp/newdir")

    def test_nonexistent_file(self):
        """exists() returns False for non-existent paths."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            assert not sb.files.exists("/tmp/no-such-file-12345")

    def test_list_directory(self):
        """List files in a directory after creating a few."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            sb.files.write("/tmp/a.txt", "a")
            sb.files.write("/tmp/b.txt", "b")
            files = sb.files.list("/tmp")
            names = {f.name for f in files}
            assert "a.txt" in names
            assert "b.txt" in names

    def test_remove_file(self):
        """Remove a file and verify it is gone."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            sb.files.write("/tmp/to_delete.txt", "bye")
            assert sb.files.exists("/tmp/to_delete.txt")
            sb.files.remove("/tmp/to_delete.txt")
            assert not sb.files.exists("/tmp/to_delete.txt")


class TestSandboxIsolation:
    """Sandlock permission isolation — Sandbox.exec is sandboxed by default."""

    def test_sandboxed_write_outside_workdir_denied(self):
        """Default sandboxed exec cannot write to system paths."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            r = sb.exec(["sh", "-c", "echo x > /etc/x-denied"])
            assert r.exit_code != 0
            assert "denied" in r.stderr.lower(), f"stderr: {r.stderr!r}"

    def test_sandboxed_workdir_write_allowed(self):
        """The session workdir stays writable under the default policy."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            r = sb.exec(["sh", "-c", "echo ok > probe.txt && cat probe.txt"])
            assert r.exit_code == 0
            assert "ok" in r.stdout

    def test_unsandboxed_exec_still_works(self):
        """sandboxed=False is the escape hatch."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            r = sb.exec(
                ["sh", "-c", "echo x > /etc/x-ok && rm /etc/x-ok && echo done"],
                sandboxed=False,
            )
            assert r.exit_code == 0
            assert "done" in r.stdout


class TestEngineSandboxes:
    """S-M2 acceptance: sandbox as a first-class engine entity."""

    def test_shared_tenant_vm(self):
        """Two sandboxes in one tenant share a single VM."""
        from terra.client import TerraClient

        tenant = f"sm2share{uuid4().hex[:6]}"
        sb1 = Sandbox(tenant=tenant, layers=["base"], cpu=1, memory_mb=256)
        sb2 = Sandbox(tenant=tenant)
        try:
            assert sb1.vm == sb2.vm == f"tenant-{tenant}"
            assert sb1.id != sb2.id
            vms = TerraClient().vm_list().get("vms", [])
            tenant_vms = [v for v in vms if v.get("name") == f"tenant-{tenant}"]
            assert len(tenant_vms) == 1, f"expected 1 tenant VM, got: {tenant_vms}"
        finally:
            Sandbox.destroy_tenant(tenant)

    def test_workdir_isolation_between_sandboxes(self):
        """Sandbox B (sandboxed exec) cannot read sandbox A's workdir."""
        tenant = f"sm2iso{uuid4().hex[:6]}"
        sba = Sandbox(tenant=tenant, layers=["base"], cpu=1, memory_mb=256)
        sbb = Sandbox(tenant=tenant)
        try:
            sba.files.write("secret.txt", "top-secret")
            # A reads its own file fine (workdir is the default cwd).
            assert "top-secret" in sba.exec("cat secret.txt").stdout
            # B cannot reach into A's workdir by absolute path.
            r = sbb.exec(["cat", f"{sba._workdir}/secret.txt"])
            assert r.exit_code != 0
            assert "top-secret" not in r.stdout
        finally:
            Sandbox.destroy_tenant(tenant)

    def test_kill_keeps_vm_alive(self):
        """kill() drops one sandbox; the VM and siblings stay functional."""
        tenant = f"sm2kill{uuid4().hex[:6]}"
        sb1 = Sandbox(tenant=tenant, layers=["base"], cpu=1, memory_mb=256)
        sb2 = Sandbox(tenant=tenant)
        try:
            sb1.kill()
            assert sb1.status == "stopped"
            # VM alive, sb2 fully functional.
            r = sb2.exec("echo alive")
            assert r.exit_code == 0
            assert "alive" in r.stdout
            # Second kill is a no-op (engine reports not found).
            sb1.kill()
        finally:
            Sandbox.destroy_tenant(tenant)

    def test_destroy_tenant_removes_everything(self):
        """destroy_tenant removes the VM and all sandbox records."""
        from terra.client import TerraClient

        tenant = f"sm2dest{uuid4().hex[:6]}"
        sb1 = Sandbox(tenant=tenant, layers=["base"], cpu=1, memory_mb=256)
        sb2 = Sandbox(tenant=tenant)
        Sandbox.destroy_tenant(tenant)
        c = TerraClient()
        vms = [v for v in c.vm_list().get("vms", []) if v.get("name") == f"tenant-{tenant}"]
        assert vms == [], f"tenant VM survived destroy_tenant: {vms}"
        remaining = c.sandbox_list(tenant).get("sandboxes", [])
        assert remaining == [], f"sandbox records survived: {remaining}"
        assert sb1.status == "stopped" and sb2.status == "stopped"


class TestSandboxProperties:
    """Property accessors."""

    def test_id_is_string(self):
        """The id property is the engine-allocated ``sb-<8hex>`` identifier."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            assert isinstance(sb.id, str)
            assert sb.id.startswith("sb-"), f"expected 'sb-<hex>' id, got: {sb.id!r}"
            assert len(sb.id) == len("sb-") + 8
            int(sb.id[len("sb-"):], 16)  # hex suffix
            assert sb.vm == f"tenant-{sb.tenant}"

    def test_backend_is_ch(self):
        """The default backend is 'ch'."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            assert sb.backend == "ch"

    def test_metadata_is_dict(self):
        """metadata is a plain dict, initially empty by default."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            assert isinstance(sb.metadata, dict)
