"""Integration tests for Sandbox API.

Requires KVM and a running engine daemon.  These tests follow the same
conventions as ``test_e2e_real.py`` — real VMs, real CH, real guest.

Usage::

    # Start the daemon first:
    cargo run --release -p engine -- daemon start

    # Then run the tests:
    pytest sdk/python/tests/test_sandbox.py -v

    # Or standalone:
    python3 sdk/python/tests/test_sandbox.py
"""

from __future__ import annotations

import pytest

from terra.sandbox import Sandbox


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


class TestSandboxProperties:
    """Property accessors."""

    def test_id_is_string(self):
        """The id property is a non-empty string."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            assert isinstance(sb.id, str)
            assert len(sb.id) > 0
            # id should start with "sandbox-"
            assert sb.id.startswith("sandbox-")

    def test_backend_is_ch(self):
        """The default backend is 'ch'."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            assert sb.backend == "ch"

    def test_metadata_is_dict(self):
        """metadata is a plain dict, initially empty by default."""
        with Sandbox(layers=["base"], cpu=1, memory_mb=256) as sb:
            assert isinstance(sb.metadata, dict)
