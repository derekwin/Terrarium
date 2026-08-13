"""Security isolation verification on a real KVM host.

This suite is the security verification loop for the default-deny sandbox
model: each test attempts a real escape / privilege-escalation / cross-tenant
primitive inside an actual Terrarium sandbox (base layer, sandlock) and
asserts the sandbox denies it. Positive controls (read /etc, write /tmp)
guard against a "denies everything" regression.

Requires the same environment as ``test_e2e_real.py``: /dev/kvm, guest
images, and a running engine (DaemonManager auto-starts one in-process).

Usage:
    pytest sdk/python/tests/test_security_isolation.py -v
"""

from __future__ import annotations

import os
from uuid import uuid4

import pytest

from terra.sandbox import Sandbox


DENY_EXIT = 200  # SANDBOX_DENY_EXIT_CODE — structured deny signal

_BACKEND = os.environ.get("TERRA_SANDBOX_BACKEND", "confine")


def _assert_fs_denied(r, what: str):
    """FS denial semantics: nonzero + permission error.

    The confine backend enforces the filesystem statically (Landlock), so a
    denial is the kernel's EACCES (exit != 0), not the structured 200 that
    the sandlock supervisor emits. Both must reject; only the signal
    differs.
    """
    assert r.exit_code != 0, f"expected {what} denied, got {r}"
    assert "ermission denied" in (r.stderr or ""), f"expected permission error, got {r}"


def _assert_net_denied(r, what: str):
    """Network denial: structured 200 + an EACCES/EPERM style message."""
    assert r.exit_code == DENY_EXIT, f"expected {what} denied, got {r}"
    err = r.stderr or ""
    assert "ermission" in err or "Operation not permitted" in err, f"got {r}"


def teardown_module():
    """Destroy all tenant VMs this suite created."""
    from terra.client import TerraClient, TerraError

    client = TerraClient()
    try:
        vms = client.vm_list().get("vms", [])
    except Exception:  # daemon already gone
        return
    for vm in vms:
        name = vm.get("name", "")
        if name.startswith("tenant-"):
            try:
                client.vm_destroy(name)
            except TerraError:
                pass


def _sandbox(**kw) -> Sandbox:
    kw.setdefault("tenant", f"sec-{uuid4().hex[:8]}")
    kw.setdefault("layers", ["base"])
    return Sandbox(**kw)


@pytest.mark.e2e
class TestFileSystemIsolation:
    """Default policy: read-only system, RW /tmp + workdir, devices denied."""

    def test_write_system_path_denied(self):
        with _sandbox() as sb:
            r = sb.exec("sh -c 'echo pwned >> /etc/passwd'")
            _assert_fs_denied(r, "write /etc/passwd")

    def test_write_root_home_denied(self):
        with _sandbox() as sb:
            r = sb.exec("sh -c 'echo x > /root/x; echo x > /home/x'")
            _assert_fs_denied(r, "write /root /home")

    def test_read_device_denied(self):
        with _sandbox() as sb:
            r = sb.exec("cat /dev/mem")
            assert r.exit_code != 0, f"expected deny, got {r}"

    def test_read_system_allowed(self):
        """Positive control: default read grants still work."""
        with _sandbox() as sb:
            r = sb.exec("cat /etc/hostname")
            assert r.exit_code == 0
            assert r.stdout.strip()

    def test_tmp_writable(self):
        """Positive control: /tmp stays writable."""
        with _sandbox() as sb:
            r = sb.exec("sh -c 'echo ok > /tmp/probe && cat /tmp/probe'")
            assert r.exit_code == 0 and "ok" in r.stdout

    def test_workdir_writable(self):
        """Positive control: the session workdir is the scratch area."""
        with _sandbox() as sb:
            r = sb.exec("sh -c 'echo hi > f.txt && cat f.txt'")
            assert r.exit_code == 0 and "hi" in r.stdout


@pytest.mark.e2e
class TestCrossSandboxIsolation:
    """One VM, two sandboxes: sibling workdirs must be unreachable."""

    def test_sibling_workdir_denied(self):
        with _sandbox() as a, _sandbox() as b:
            b_wd = b._workdir
            r = a.exec(f"ls {b_wd}")
            assert r.exit_code != 0, f"sandbox A read B's workdir: {r}"
            r2 = a.exec(f"sh -c 'echo pwned > {b_wd}/owned'")
            assert r2.exit_code != 0, f"sandbox A wrote B's workdir: {r2}"


@pytest.mark.e2e
class TestProcessIsolation:
    """Resource limits are enforced by the sandbox policy."""

    def test_procs_limit_enforced(self):
        policy = {"limits": {"procs": 2}}
        with _sandbox(policy=policy) as sb:
            # 3 concurrent sleeps exceed the 2-proc limit → fork must fail.
            r = sb.exec("sh -c 'sleep 5 & sleep 5 & sleep 5; wait'")
            assert r.exit_code != 0, f"expected procs limit to bite, got {r}"
            assert (
                "Resource temporarily unavailable" in r.stderr
                or "cannot fork" in r.stderr
                or "can't fork" in r.stderr
            ), f"expected fork failure, got {r}"

    def test_procs_within_limit_ok(self):
        """Positive control: a single process stays within the limit."""
        policy = {"limits": {"procs": 2}}
        with _sandbox(policy=policy) as sb:
            r = sb.exec("sh -c 'sleep 1; echo done'")
            assert r.exit_code == 0 and "done" in r.stdout, f"got {r}"


@pytest.mark.e2e
class TestNetworkIsolation:
    """Default policy denies outbound net; explicit grants open a whitelist.

    Targets are the host's own LAN (10.102.0.254:80 answers HTTP) instead of
    the public internet, so the suite is reproducible on restricted hosts.
    """

    LAN_HOST = "10.102.0.254"
    LAN_PORT = 80
    OTHER_LAN_HOST = "192.168.2.1"

    def test_default_net_denied(self):
        """No Network capability → outbound TCP is denied (default-deny)."""
        with _sandbox(network=True) as sb:
            r = sb.exec(f"wget -q -O /tmp/w http://{self.LAN_HOST}:{self.LAN_PORT}/")
            _assert_net_denied(r, "default net")

    def test_explicit_net_allow_grant(self):
        """Explicit Outbound grant to a host opens exactly that endpoint."""
        policy = {
            "capabilities": [
                {"Network": {"endpoint": {"host": self.LAN_HOST, "port": self.LAN_PORT},
                             "direction": "Outbound"}},
            ],
        }
        with _sandbox(network=True, policy=policy) as sb:
            r = sb.exec(f"wget -q -O /tmp/w http://{self.LAN_HOST}:{self.LAN_PORT}/")
            assert "404" in r.stderr, f"granted endpoint should connect (HTTP), got {r}"
            # Whitelist semantics: a non-granted destination stays denied
            # (exit 200 is the structured deny signal, distinct from a
            # plain connection failure's exit 1).
            r2 = sb.exec(f"wget -q -O /tmp/w http://{self.OTHER_LAN_HOST}:{self.LAN_PORT}/")
            assert r2.exit_code == DENY_EXIT, f"expected non-granted dest denied, got {r2}"


@pytest.mark.e2e
class TestTenantNetworkIsolation:
    """Two tenants on one host must not reach each other at L2.

    The ebtables isolation rule drops frames between VM tap ports, so a
    tenant cannot ARP/connect to a sibling VM even though they share the
    bridge and subnet. The positive control (same host, outbound LAN)
    guards against breaking DHCP/routing in the process.
    """

    LAN_HOST = "10.102.0.254"

    def _vm_ip(self, sb) -> str:
        r = sb.exec("ip addr show eth0", sandboxed=False)
        assert r.exit_code == 0, f"cannot read eth0: {r}"
        for line in r.stdout.splitlines():
            line = line.strip()
            if line.startswith("inet "):
                return line.split()[1].split("/")[0]
        raise AssertionError(f"no inet on eth0: {r.stdout!r}")

    def test_sibling_tenant_unreachable(self):
        with _sandbox(network=True) as a, _sandbox(network=True) as b:
            b_ip = self._vm_ip(b)
            # Sibling tenant: must NOT be reachable (ARP dies at the
            # bridge isolation rule → unreachable/timeout, never HTTP).
            r = a.exec(f"timeout 5 wget -q -O /dev/null http://{b_ip}:80/", sandboxed=False)
            assert r.exit_code != 0, f"sibling tenant reachable: {r}"
            assert "404" not in r.stderr, f"sibling tenant served HTTP: {r}"

    def test_outbound_lan_still_works(self):
        """Positive control: isolation must not break NAT/DHCP/routing."""
        with _sandbox(network=True) as a:
            r = a.exec(
                f"timeout 5 wget -q -O /dev/null http://{self.LAN_HOST}:80/",
                sandboxed=False,
            )
            assert "404" in r.stderr, f"outbound LAN should answer HTTP, got {r}"


@pytest.mark.e2e
class TestAudit:
    """Denials leave a structured audit trail."""

    def _trigger_net_deny(self, sb) -> None:
        """A default-policy network connect is denied by the supervisor on
        both backends (native: seccomp-notify; sandlock: supervisor), so
        the structured deny signal + audit trail are backend-independent.
        """
        r = sb.exec("wget -q -O /tmp/w http://10.102.0.254:80/")
        assert r.exit_code != 0, f"expected net deny, got {r}"

    def test_deny_is_audited(self):
        from terra.client import TerraClient

        with _sandbox(network=True) as sb:
            self._trigger_net_deny(sb)
            audit = TerraClient().audit_list(event="deny", sandbox_id=sb._id)
            events = audit.get("audit", [])
            assert any(e.get("sandbox_id") == sb._id for e in events), f"no deny event: {audit}"

    def test_default_policy_deny_is_audited(self):
        """Governance default: no explicit policy still records denials."""
        from terra.client import TerraClient

        with _sandbox(network=True) as sb:  # no policy → engine default (deny audit on)
            self._trigger_net_deny(sb)
            audit = TerraClient().audit_list(event="deny", sandbox_id=sb._id)
            events = audit.get("audit", [])
            assert any(e.get("sandbox_id") == sb._id for e in events), f"no deny event: {audit}"
