"""Adversarial isolation suite — real-KVM escape attempts, not feature tests.

This suite goes beyond the functional security gate
(``test_security_isolation.py``): each test attempts a *bypass* of the
enforced layer, not just the obvious "write /etc/passwd" path.

Layer model under test:

* L2 (in-VM governance, terra-confine): Landlock fs, seccomp user-notify
  for network + signal syscalls, cgroup/rlimit resource limits. The agent
  runs as guest root, same uid as its supervisor — so the suite explicitly
  attacks the governance layer itself (kill the supervisor, inherit policy
  fds, forge audit records).
* L1 (VM boundary, Cloud Hypervisor/KVM): even with L2 out of the picture
  (``sandboxed=False``), the untrusted process must be unable to reach the
  host, sibling tenants, or the audit trail.

Usage (real KVM host):

    pytest sdk/python/tests/test_adversarial_isolation.py -v

The static escape probe (``adversarial/probes/escape_probe``) is built at
module setup with gcc when missing; probe-dependent tests skip if the
build is unavailable.
"""

from __future__ import annotations

import base64
import shutil
import subprocess
from pathlib import Path
from uuid import uuid4

import pytest

from terra.sandbox import Sandbox


PROBES_DIR = Path(__file__).resolve().parent / "adversarial" / "probes"
PROBE_SRC = PROBES_DIR / "escape_probe.c"
PROBE_BIN = PROBES_DIR / "escape_probe"

DENY_EXIT = 200  # SANDBOX_DENY_EXIT_CODE — structured deny signal

# Host LAN target that answers HTTP (used by the existing e2e suite);
# reproducible on restricted hosts without public internet.
LAN_HOST = "10.102.0.254"
LAN_PORT = 80
OTHER_LAN_HOST = "192.168.2.1"


def _build_probe() -> bool:
    if PROBE_BIN.exists():
        return True
    gcc = shutil.which("gcc")
    if gcc is None or not PROBE_SRC.exists():
        return False
    r = subprocess.run(
        [gcc, "-static", "-O2", "-o", str(PROBE_BIN), str(PROBE_SRC)],
        cwd=PROBES_DIR,
        capture_output=True,
        text=True,
    )
    return r.returncode == 0 and PROBE_BIN.exists()


def _upload(sb: Sandbox, local: Path, remote: str, chunk: int = 60000) -> None:
    """Chunked base64 upload (the daemon rejects single-shot >~512KB)."""
    data = base64.b64encode(local.read_bytes()).decode()
    sb.exec(["sh", "-c", f"rm -f {remote}.b64"], sandboxed=False)
    for i in range(0, len(data), chunk):
        part = data[i : i + chunk]
        r = sb.exec(["sh", "-c", f"echo {part} >> {remote}.b64"], sandboxed=False)
        assert r.exit_code == 0, r
    r = sb.exec(
        ["sh", "-c", f"cat {remote}.b64 | base64 -d > {remote} && chmod +x {remote} && rm -f {remote}.b64"],
        sandboxed=False,
    )
    assert r.exit_code == 0, r


def _fs_denied(r, what: str) -> None:
    """Landlock denial is a kernel EACCES (nonzero, Permission denied)."""
    assert r.exit_code != 0, f"expected {what} denied, got {r}"
    assert "ermission denied" in (r.stderr or ""), f"expected EACCES, got {r}"


def _net_denied(r, what: str) -> None:
    """Seccomp verdict denial is structured (200) + EPERM text."""
    assert r.exit_code == DENY_EXIT, f"expected {what} denied, got {r}"
    assert "Operation not permitted" in (r.stdout or "") or "ermission" in (r.stderr or ""), (
        f"expected EPERM, got {r}"
    )


@pytest.fixture(scope="module")
def sandbox():
    """One shared ubuntu tenant VM; every exec re-enters the confine layer."""
    probe_ok = _build_probe()
    with Sandbox(tenant=f"adv-{uuid4().hex[:8]}", layers=["ubuntu"], network=True, timeout=300) as sb:
        sb._probe_ok = probe_ok  # type: ignore[attr-defined]
        if probe_ok:
            _upload(sb, PROBE_BIN, "/tmp/escape_probe")
        _wait_for_net(sb)
        yield sb


def _probe(sandbox, mode: list[str], **kw):
    assert sandbox._probe_ok, "escape probe unavailable (gcc build failed)"
    return sandbox.exec(["/tmp/escape_probe", *mode], **kw)


def _wait_for_net(sandbox, timeout: float = 20.0) -> None:
    """Wait until the VM's eth0 has an address (DHCP race at boot)."""
    import time

    deadline = time.time() + timeout
    while time.time() < deadline:
        r = sandbox.exec(["sh", "-c", "ip addr show eth0 2>/dev/null | grep -c 'inet '"], sandboxed=False)
        if r.exit_code == 0 and r.stdout.strip() == "1":
            return
        time.sleep(0.5)
    raise AssertionError("eth0 did not get an address in time")


# ─────────────────────────────────────────────────────────────────────
# L2 — filesystem (Landlock): bypass attempts beyond plain writes
# ─────────────────────────────────────────────────────────────────────
@pytest.mark.e2e
class TestFSBypass:
    """Landlock path grants cannot be widened by fs tricks."""

    def test_symlink_into_system_path_denied(self, sandbox):
        """A symlink in a writable dir to a protected path must not grant writes."""
        r = sandbox.exec(
            "sh -c 'ln -s /etc/passwd /tmp/esc-link && echo pwned >> /tmp/esc-link'"
        )
        _fs_denied(r, "write via symlink to /etc/passwd")

    def test_hardlink_to_protected_file_denied(self, sandbox):
        """Hardlinking a protected file into a writable dir is blocked."""
        r = sandbox.exec(["sh", "-c", "ln /etc/passwd /tmp/passwd 2>&1; echo rc=$?"])
        assert "rc=0" not in r.stdout, f"hardlink to /etc/passwd succeeded: {r}"
        assert "rc=1" in r.stdout, f"got {r}"

    def test_rename_of_protected_file_denied(self, sandbox):
        """Moving a protected file out of its tree requires remove rights."""
        r = sandbox.exec(["sh", "-c", "mv /etc/passwd /tmp/passwd 2>&1; echo rc=$?"])
        assert "rc=0" not in r.stdout, f"rename of /etc/passwd succeeded: {r}"
        assert "rc=1" in r.stdout, f"got {r}"

    def test_proc_and_sys_read_denied(self, sandbox):
        """Default-deny covers /proc and /sys: no pid/fs discovery."""
        r = sandbox.exec("sh -c 'cat /proc/self/status; cat /sys/kernel/uevent_seqnum'")
        assert r.exit_code != 0, f"/proc or /sys readable: {r}"
        assert "ermission denied" in (r.stderr or ""), f"got {r}"

    def test_proc_read_is_an_opt_in_capability(self, sandbox):
        """/proc read is available when the policy grants it (capability model)."""
        policy = {
            "capabilities": [
                {"File": {"path": {"Prefix": "/proc"}, "access": "Read"}},
            ],
        }
        r = sandbox.exec("cat /proc/self/status", policy=policy)
        assert r.exit_code == 0 and "Name:" in r.stdout, f"got {r}"

    def test_devmem_beyond_isa_hole_denied(self, sandbox):
        """STRICT_DEVMEM allows only the legacy ISA hole (first 1MB);
        physical RAM at offset is kernel-restricted even with /dev read
        granted at L2."""
        r = sandbox.exec("dd if=/dev/mem bs=1 count=4 skip=4194304 2>&1")
        assert r.exit_code != 0, f"/dev/mem RAM region readable: {r}"

    def test_dev_null_write_currently_denied(self, sandbox):
        """Documented behavior: device grants are directory-scoped, so
        ``>/dev/null`` is denied today. Functional overreach, not a
        security hole — tracked as a product decision (see
        docs/security-adversarial.md)."""
        r = sandbox.exec("sh -c 'echo hi > /dev/null'")
        assert r.exit_code != 0, f"/dev/null write unexpectedly allowed: {r}"


# ─────────────────────────────────────────────────────────────────────
# L2 — network (seccomp user-notify): bypass attempts
# ─────────────────────────────────────────────────────────────────────
@pytest.mark.e2e
class TestNetworkBypass:
    """Outbound policy holds across socket families and syscall shapes."""

    def test_default_deny_tcp(self, sandbox):
        r = _probe(sandbox, ["net", "tcp", LAN_HOST, str(LAN_PORT)])
        _net_denied(r, "default TCP")

    def test_default_deny_udp_sendto(self, sandbox):
        r = _probe(sandbox, ["net", "udp", LAN_HOST, str(LAN_PORT)])
        _net_denied(r, "default UDP sendto")

    def test_default_deny_sendmsg(self, sandbox):
        r = _probe(sandbox, ["net", "sendmsg", LAN_HOST, str(LAN_PORT)])
        _net_denied(r, "default sendmsg")

    def test_whitelist_allows_exact_endpoint(self, sandbox):
        _wait_for_net(sandbox)
        policy = {
            "capabilities": [
                {"Network": {"endpoint": {"host": LAN_HOST, "port": LAN_PORT},
                             "direction": "Outbound"}},
            ],
        }
        r = _probe(sandbox, ["net", "tcp", LAN_HOST, str(LAN_PORT)], policy=policy)
        assert "connect rc=0" in r.stdout, f"granted endpoint should connect: {r}"

    def test_whitelist_denies_wrong_port(self, sandbox):
        policy = {
            "capabilities": [
                {"Network": {"endpoint": {"host": LAN_HOST, "port": LAN_PORT},
                             "direction": "Outbound"}},
            ],
        }
        r = _probe(sandbox, ["net", "tcp", LAN_HOST, "81"], policy=policy)
        _net_denied(r, "whitelisted host, non-granted port")

    def test_whitelist_denies_wrong_host(self, sandbox):
        policy = {
            "capabilities": [
                {"Network": {"endpoint": {"host": LAN_HOST, "port": LAN_PORT},
                             "direction": "Outbound"}},
            ],
        }
        r = _probe(sandbox, ["net", "tcp", OTHER_LAN_HOST, str(LAN_PORT)], policy=policy)
        _net_denied(r, "non-granted host")

    def test_af_unix_connect_fails_closed(self, sandbox):
        """Non-IP families cannot be whitelisted → fail closed (deny)."""
        r = _probe(sandbox, ["net", "unix", "/tmp/esc-unix.sock"])
        assert r.exit_code == DENY_EXIT, f"AF_UNIX connect not denied: {r}"

    def test_af_vsock_connect_fails_closed(self, sandbox):
        """vsock (the guest-proxy channel family) is denied to the sandbox."""
        r = _probe(sandbox, ["net", "vsock", "2"])
        assert r.exit_code == DENY_EXIT, f"AF_VSOCK connect not denied: {r}"

    def test_raw_socket_denied_by_capability(self, sandbox):
        r = _probe(sandbox, ["net", "raw", LAN_HOST])
        assert r.exit_code != 0, f"raw socket available: {r}"
        assert "raw-socket rc=-1" in r.stdout or "raw-sendto rc=-1" in r.stdout, f"got {r}"

    def test_ping_socket_denied(self, sandbox):
        r = _probe(sandbox, ["net", "ping", LAN_HOST])
        assert r.exit_code != 0, f"ping socket available: {r}"

    def test_inbound_bind_is_unrestricted_by_design(self, sandbox):
        """Inbound (bind/listen) is not policy-gated — external exposure is
        prevented by L1 NAT, not L2."""
        r = _probe(sandbox, ["net", "bind", "8080"])
        assert "bind rc=0" in r.stdout, f"bind should succeed inside VM: {r}"

    def test_kill_supervisor_denied(self, sandbox):
        """The confined process shares the supervisor's uid (guest root);
        ``kill -9 $PPID`` must be denied or governance is trivially
        removable. After the attempt the sandbox must still be governed."""
        r = sandbox.exec(
            ["sh", "-c", "kill -9 $PPID 2>&1; sleep 0.2; /tmp/escape_probe net tcp 10.102.0.254 80"],
            timeout=30,
        )
        assert "connect rc=0" not in r.stdout, f"network policy removed by kill: {r}"
        assert r.exit_code == DENY_EXIT, f"expected structured deny, got {r}"
        alive = sandbox.exec("echo alive", timeout=10)
        assert alive.exit_code == 0 and "alive" in alive.stdout, f"sandbox broken: {alive}"

    def test_no_inherited_policy_fds(self, sandbox):
        """The confined process must not inherit the seccomp listener or
        deny-channel fds (fd hygiene — the audit channel is not forgeable
        and the network verdict path cannot be hijacked via an fd)."""
        r = _probe(sandbox, ["fdscan"])
        assert r.exit_code == 0, r
        fds = [ln for ln in r.stdout.splitlines() if ln.startswith("FD ")]
        open_fds = [ln for ln in fds if "flags=0x" in ln and "fstat errno" not in ln]
        nums = sorted(int(ln.split()[1]) for ln in open_fds)
        assert nums == list(range(len(nums))), f"unexpected fd numbers: {nums}"
        assert len(nums) <= 6, f"unexpected inherited fds: {nums}"


# ─────────────────────────────────────────────────────────────────────
# L2 — resources (cgroup v2 + rlimit)
# ─────────────────────────────────────────────────────────────────────
@pytest.mark.e2e
class TestResourceLimits:
    def test_procs_limit_enforced_even_with_fork_bomb(self, sandbox):
        policy = {"limits": {"procs": 4}}
        r = _probe(sandbox, ["fork", "32"], policy=policy)
        assert "forked 32/32" not in r.stdout, f"fork bomb exceeded procs limit: {r}"
        assert "errno=11" in r.stdout or "errno=12" in r.stdout, f"got {r}"

    def test_fds_limit_enforced(self, sandbox):
        policy = {"limits": {"fds": 32}}
        r = _probe(sandbox, ["fds", "100"], policy=policy)
        assert "opened 100/100 fds" not in r.stdout, f"fd limit ignored: {r}"
        assert "errno=24" in r.stdout, f"expected EMFILE, got {r}"

    def test_memory_limit_enforced(self, sandbox):
        policy = {"limits": {"memory_mb": 64}}
        r = _probe(sandbox, ["mem", "256"], policy=policy, timeout=30)
        assert "touched 256MB ok" not in r.stdout, f"memory limit ignored: {r}"
        assert r.exit_code != 0, f"expected OOM kill, got {r}"

    def test_memory_within_limit_ok(self, sandbox):
        policy = {"limits": {"memory_mb": 64}}
        r = _probe(sandbox, ["mem", "8"], policy=policy, timeout=30)
        assert "touched 8MB ok" in r.stdout, f"got {r}"


# ─────────────────────────────────────────────────────────────────────
# L1 — VM boundary: blast radius with L2 out of the picture
# ─────────────────────────────────────────────────────────────────────
@pytest.mark.e2e
class TestVMBlastRadius:
    """Unconfined execs (sandboxed=False) model a fully compromised L2:
    the blast radius must still be bounded by the tenant VM."""

    def test_host_fs_not_mounted(self, sandbox):
        for p in ("/host", "/mnt/terra", "/root/.local/share/terra"):
            exists = sandbox.exec(["sh", "-c", f"test -e {p}; echo $?"], sandboxed=False)
            assert exists.stdout.strip().endswith("1"), f"{p} exists in guest: {exists}"

    def test_audit_trail_not_reachable_from_guest(self, sandbox):
        r = sandbox.exec(
            ["sh", "-c", "find / -xdev -name 'audit.jsonl' 2>/dev/null | head -1"],
            sandboxed=False,
            timeout=30,
        )
        assert r.stdout.strip() == "", f"audit trail reachable: {r.stdout!r}"

    def test_guest_sees_its_own_init_not_host_pids(self, sandbox):
        r = sandbox.exec("cat /proc/1/comm", sandboxed=False)
        comm = r.stdout.strip()
        assert comm in ("init", "guest-proxy", "bash", "sh"), f"unexpected pid1: {comm!r}"
        n = sandbox.exec(["sh", "-c", "ls /proc | grep -c '^[0-9]'"], sandboxed=False)
        assert int(n.stdout.strip()) < 200, f"suspiciously many pids: {n.stdout.strip()}"

    def test_no_kvm_device_in_guest(self, sandbox):
        r = sandbox.exec(["sh", "-c", "test -e /dev/kvm; echo $?"], sandboxed=False)
        assert r.stdout.strip().endswith("1"), f"/dev/kvm present in guest: {r}"

    def test_sibling_tenant_unreachable_at_l2(self):
        with Sandbox(tenant=f"advA-{uuid4().hex[:6]}", layers=["ubuntu"], network=True, timeout=120) as a, \
             Sandbox(tenant=f"advB-{uuid4().hex[:6]}", layers=["ubuntu"], network=True, timeout=120) as b:
            _wait_for_net(a)
            _wait_for_net(b)
            ip_b = b.exec("ip addr show eth0", sandboxed=False).stdout
            b_ip = next(
                line.strip().split()[1].split("/")[0]
                for line in ip_b.splitlines()
                if line.strip().startswith("inet ")
            )
            r = a.exec(
                f"timeout 5 wget -q -O /tmp/w http://{b_ip}:80/",
                sandboxed=False,
            )
            assert "404" not in r.stderr, f"sibling tenant served HTTP: {r}"
            assert r.exit_code != 0, f"sibling tenant reachable: {r}"


# ─────────────────────────────────────────────────────────────────────
# Audit integrity
# ─────────────────────────────────────────────────────────────────────
@pytest.mark.e2e
class TestAuditIntegrity:
    def test_deny_is_recorded_and_queryable(self, sandbox):
        from terra.client import TerraClient

        _probe(sandbox, ["net", "tcp", LAN_HOST, str(LAN_PORT)])
        audit = TerraClient().audit_list(event="deny", sandbox_id=sandbox._id)
        events = audit.get("audit", [])
        assert any(e.get("sandbox_id") == sandbox._id for e in events), f"no deny event: {audit}"

    def test_deny_channel_not_forgeable_from_inside(self, sandbox):
        """The deny channel (fd 63 in the wrapper) must not be inherited by
        the confined process — otherwise a failing command could forge a
        fake policy denial and pollute the audit trail."""
        r = sandbox.exec(["sh", "-c", "echo fake >&63 2>&1; echo rc=$?"])
        assert "rc=0" not in r.stdout, f"fd 63 writable from sandbox: {r}"
        assert r.exit_code != 0, f"fd 63 writable from sandbox: {r}"
        err = r.stderr or ""
        assert "Bad fd number" in err or "Bad file descriptor" in err, f"unexpected: {r}"
