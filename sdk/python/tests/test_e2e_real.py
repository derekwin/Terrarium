#!/usr/bin/env python3
"""Terrarium SDK end-to-end test against a REAL deployment.

No mocks: real KVM, real Cloud Hypervisor, real guest, real engine daemon
lifecycle — driven entirely through the Python SDK.

Usage:
    python3 sdk/python/tests/test_e2e_real.py     # standalone runner
    pytest sdk/python/tests/test_e2e_real.py      # pytest

Environment requirements (checked in preflight, fails fast with guidance):
    - /dev/kvm accessible
    - cloud-hypervisor binary (TERRA_CH_BINARY, /tmp/cloud-hypervisor-static,
      or PATH)
    - guest images: target/guest/vmlinux.bin + target/guest/alpine.cpio
      (build with images/build.sh; memory-resize test needs a kernel with
      CONFIG_VIRTIO_MEM=y)
    - qemu-img
    - target/release/engine (built automatically via cargo if missing)

The suite starts its own daemon on a dedicated socket with an isolated
state dir, and tears it down with SIGTERM at the end (also verifying the
graceful-shutdown path and that no CH processes leak).
"""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "sdk" / "python"))

from terra.client import TerraClient, TerraError  # noqa: E402
from terra.vm import create, list_vms  # noqa: E402

KERNEL = REPO / "target/guest/vmlinux.bin"
INITRAMFS = REPO / "target/guest/alpine.cpio"
IRFS_VIRTIOFS = REPO / "target/guest/initramfs-virtiofs.cpio.gz"
ENGINE = REPO / "target/release/engine"
SOCKET = "/tmp/terra-sdk-e2e.sock"


def _find_virtiofsd() -> str | None:
    for c in (
        os.environ.get("TERRA_VIRTIOFSD"),
        shutil.which("virtiofsd"),
        str(Path.home() / ".cargo/bin/virtiofsd"),
        "/usr/lib/qemu/virtiofsd",
    ):
        if c and Path(c).exists():
            return c
    return None

# ---------------------------------------------------------------------------
# Module state (set up once for the whole suite)
# ---------------------------------------------------------------------------
_state: dict = {}
_PASSED: list[str] = []
_FAILED: list[tuple[str, BaseException]] = []


def _ch_binary() -> str | None:
    for c in (
        os.environ.get("TERRA_CH_BINARY"),
        "/tmp/cloud-hypervisor-static",
        shutil.which("cloud-hypervisor"),
    ):
        if c and Path(c).exists():
            return c
    return None


def _ch_pids() -> list[int]:
    """PIDs whose /proc comm is cloud-hypervisor (comm is 15-char truncated)."""
    pids = []
    for p in Path("/proc").iterdir():
        if p.name.isdigit():
            try:
                if (p / "comm").read_text().strip() == "cloud-hyperviso":
                    pids.append(int(p.name))
            except (OSError, PermissionError):
                pass
    return pids


def _is_ch_pid(pid: int) -> bool:
    """Whether pid currently belongs to a cloud-hypervisor process."""
    try:
        return Path(f"/proc/{pid}/comm").read_text().strip() == "cloud-hyperviso"
    except OSError:
        return False


def _guest_exec(vm_name: str, args: list[str], timeout: float = 10.0) -> str:
    """Send an exec command to guest-proxy over the CH vsock socket."""
    import socket as _sock

    path = f"/tmp/terra-{vm_name}-vsock.sock"
    deadline = time.time() + timeout
    last_err: Exception | None = None
    while time.time() < deadline:
        try:
            s = _sock.socket(_sock.AF_UNIX, _sock.SOCK_STREAM)
            s.settimeout(3)
            s.connect(path)
            s.sendall(b"CONNECT 1024\n")
            f = s.makefile("rw")
            handshake = f.readline()
            assert handshake.startswith("OK"), handshake
            import json as _json

            f.write(_json.dumps({"command": "exec", "args": args}) + "\n")
            f.flush()
            resp = _json.loads(f.readline())
            s.close()
            if resp.get("status") == "ok":
                return resp["data"]["stdout"]
            raise RuntimeError(resp.get("message"))
        except (OSError, AssertionError) as e:
            last_err = e
            time.sleep(0.3)
    raise RuntimeError(f"guest exec failed: {last_err}")


def _find_fs_supervisor(vm_name: str) -> int | None:
    """PID of the virtiofsd supervisor serving the given VM's fs socket."""
    needle = f"terra-{vm_name}-fs.sock"
    for p in Path("/proc").iterdir():
        if p.name.isdigit():
            try:
                if needle in (p / "cmdline").read_bytes().decode(errors="ignore"):
                    return int(p.name)
            except (OSError, PermissionError):
                pass
    return None


def _ch_cmdline(pid: int) -> list[str]:
    return Path(f"/proc/{pid}/cmdline").read_bytes().decode().split("\0")


def _wait_for(cond, timeout: float, what: str) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if cond():
            return
        time.sleep(0.2)
    raise RuntimeError(f"timeout waiting for: {what}")


# ---------------------------------------------------------------------------
# Suite setup / teardown (pytest xunit hooks + standalone runner)
# ---------------------------------------------------------------------------
def setup_module() -> None:  # noqa: ANN001 (pytest passes the module)
    # Preflight -------------------------------------------------------------
    missing = []
    if not Path("/dev/kvm").exists():
        missing.append("/dev/kvm (need KVM; add user to kvm group)")
    ch = _ch_binary()
    if not ch:
        missing.append("cloud-hypervisor binary (set TERRA_CH_BINARY)")
    if not KERNEL.exists():
        missing.append(f"{KERNEL} (run images/build.sh)")
    if not INITRAMFS.exists():
        missing.append(f"{INITRAMFS} (run images/build.sh)")
    if not shutil.which("qemu-img"):
        missing.append("qemu-img")
    if missing:
        raise RuntimeError("preflight failed:\n  - " + "\n  - ".join(missing))

    if not ENGINE.exists():
        subprocess.run(
            ["cargo", "build", "--release", "-p", "engine"],
            cwd=REPO, check=True,
        )

    state_dir = Path(tempfile.mkdtemp(prefix="terra-sdk-e2e-"))

    # virtiofs layers for the layered-boot test (base + marker layer)
    vfsd = _find_virtiofsd()
    layer_dir = state_dir / "layers"
    if vfsd:
        (layer_dir / "base").mkdir(parents=True)
        subprocess.run(
            f"zcat {INITRAMFS} | cpio -idm --quiet",
            shell=True, cwd=layer_dir / "base", check=True,
        )
        marker = layer_dir / "marker" / "usr" / "bin"
        marker.mkdir(parents=True)
        (marker / "hello.py").write_text('print("hello from marker layer")\n')
        if not IRFS_VIRTIOFS.exists():
            subprocess.run(
                ["bash", "images/build-initramfs-virtiofs.sh"],
                cwd=REPO, check=True,
            )

    if Path(SOCKET).exists():
        Path(SOCKET).unlink()
    env = {
        **os.environ,
        "TERRA_STATE_DIR": str(state_dir / "vms"),
        "TERRA_CH_BINARY": ch,
    }
    if vfsd:
        env["TERRA_VIRTIOFSD"] = vfsd
        env["TERRA_LAYER_DIR"] = str(layer_dir)
    daemon_log = open(state_dir / "daemon.log", "w")
    daemon = subprocess.Popen(
        [str(ENGINE), "daemon", "--socket", SOCKET],
        env=env,
        stdout=daemon_log,
        stderr=subprocess.STDOUT,
    )
    _wait_for(lambda: Path(SOCKET).exists(), 10, "daemon socket")

    _state.update(
        daemon=daemon,
        daemon_log=daemon_log,
        client=TerraClient(socket_path=SOCKET),
        state_dir=state_dir,
        vfsd=vfsd,
        fs_root=state_dir / "vms" / "fs",
        created=[],  # names to best-effort destroy in teardown
    )
    print(f"[setup] daemon pid={daemon.pid} state_dir={state_dir} ch={ch}")


def teardown_module() -> None:  # noqa: ANN001
    client: TerraClient = _state["client"]
    for name in _state["created"]:
        try:
            client.vm_destroy(name)
        except Exception:
            pass
    daemon = _state["daemon"]
    daemon.send_signal(signal.SIGTERM)
    try:
        daemon.wait(timeout=15)
    except subprocess.TimeoutExpired:
        daemon.kill()
        raise AssertionError("daemon did not exit on SIGTERM within 15s")
    leftovers = _ch_pids()
    assert not leftovers, f"CH processes leaked after SIGTERM: {leftovers}"
    if _FAILED:
        _state["daemon_log"].close()
        print(f"[teardown] state dir kept for inspection: {_state['state_dir']}")
    else:
        _state["daemon_log"].close()
        shutil.rmtree(_state["state_dir"], ignore_errors=True)
    print("[teardown] daemon exited on SIGTERM, no CH leaks")


def _track(name: str) -> None:
    _state["created"].append(name)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------
def test_create_and_info() -> None:
    vm = create(
        "sdk-t1", str(KERNEL),
        initramfs=str(INITRAMFS),
        cpus=2, max_cpus=8, memory_mb=512, max_memory_mb=2048,
        client=_state["client"],
    )
    _track("sdk-t1")
    assert isinstance(vm.pid, int) and vm.pid > 0, f"pid missing: {vm.pid}"
    info = vm.info()
    assert info["state"] == "Running", info
    assert info["cpus"] == 2, info
    assert info["memory_mb"] == 512, info
    # socket permissions must be owner-only
    mode = oct(Path(SOCKET).stat().st_mode & 0o777)
    assert mode == "0o600", mode



def test_resize_cpus() -> None:
    client = _state["client"]
    client.vm_resize("sdk-t1", cpus=4)
    _wait_for(lambda: client.vm_info("sdk-t1")["cpus"] == 4, 10, "cpu resize")


def test_resize_memory() -> None:
    client = _state["client"]
    client.vm_resize("sdk-t1", memory_bytes=1024 * 1024 * 1024)
    try:
        _wait_for(
            lambda: client.vm_info("sdk-t1")["memory_mb"] == 1024, 15,
            "memory resize",
        )
    except RuntimeError as e:
        raise AssertionError(
            f"{e} — guest kernel needs CONFIG_VIRTIO_MEM=y (+ deps) for "
            "virtio-mem hotplug; rebuild with images/build-kernel.sh"
        )


def test_concurrent_create() -> None:
    def mk(i: int) -> str:
        name = f"sdk-c{i}"
        create(
            name, str(KERNEL),
            initramfs=str(INITRAMFS),
            cpus=1, memory_mb=128,
            client=_state["client"],
        )
        _track(name)
        return name

    with ThreadPoolExecutor(max_workers=3) as ex:
        names = list(ex.map(mk, range(3)))
    for n in names:
        assert _state["client"].vm_info(n)["state"] == "Running", n



def test_list_vms() -> None:
    # self-contained: create our own VM so the test is order-independent
    create("sdk-l1", str(KERNEL), initramfs=str(INITRAMFS),
           cpus=1, memory_mb=128, client=_state["client"])
    _track("sdk-l1")
    names = {v.name for v in list_vms(client=_state["client"])}
    assert "sdk-l1" in names, names
    _state["client"].vm_destroy("sdk-l1")
    _state["created"].remove("sdk-l1")


def test_error_paths() -> None:
    client = _state["client"]
    # invalid name (path traversal)
    try:
        create("../evil", str(KERNEL), client=client)
        raise AssertionError("invalid name accepted")
    except TerraError as e:
        assert "invalid name" in str(e), e
    # duplicate name
    try:
        create("sdk-t1", str(KERNEL), client=client)
        raise AssertionError("duplicate name accepted")
    except TerraError as e:
        assert "already exists" in str(e), e
    # nonexistent VM
    try:
        client.vm_info("no-such-vm")
        raise AssertionError("nonexistent VM did not raise")
    except TerraError as e:
        assert "not found" in str(e), e
    # resize without parameters
    try:
        client.vm_resize("sdk-t1")
        raise AssertionError("no-param resize accepted")
    except TerraError as e:
        assert "cpus" in str(e) or "memory_bytes" in str(e), e



def test_destroy_cleans_up() -> None:
    client = _state["client"]
    create("sdk-t2", str(KERNEL), initramfs=str(INITRAMFS),
           cpus=1, memory_mb=128, client=client)
    _track("sdk-t2")
    client.vm_destroy("sdk-t2")
    _state["created"].remove("sdk-t2")
    assert not Path("/tmp/terra-sdk-t2.sock").exists(), "API socket leaked"
    try:
        client.vm_info("sdk-t2")
        raise AssertionError("destroyed VM still visible")
    except TerraError:
        pass



def test_shutdown_and_kill() -> None:
    client = _state["client"]
    # shutdown contract: stop + deregister
    create("sdk-s1", str(KERNEL), initramfs=str(INITRAMFS),
           cpus=1, memory_mb=128, client=client)
    _track("sdk-s1")
    pid = client.vm_info("sdk-s1")["pid"]
    client.vm_shutdown("sdk-s1")
    _wait_for(lambda: not _is_ch_pid(pid), 10, "CH process exit")
    try:
        client.vm_info("sdk-s1")
        raise AssertionError("shut-down VM still registered")
    except TerraError as e:
        assert "not found" in str(e), e
    _state["created"].remove("sdk-s1")

    # kill contract: same deregistration semantics as shutdown
    create("sdk-k1", str(KERNEL), initramfs=str(INITRAMFS),
           cpus=1, memory_mb=128, client=client)
    _track("sdk-k1")
    kpid = client.vm_info("sdk-k1")["pid"]
    client.vm_kill("sdk-k1")
    _wait_for(lambda: not _is_ch_pid(kpid), 10, "CH process exit after kill")
    try:
        client.vm_info("sdk-k1")
        raise AssertionError("killed VM still registered")
    except TerraError as e:
        assert "not found" in str(e), e
    _state["created"].remove("sdk-k1")


def test_layered_boot() -> None:
    """virtiofs layered rootfs: compose layers -> boot -> copy-up -> teardown."""
    client = _state["client"]
    if not _state.get("vfsd"):
        print("SKIP test_layered_boot: no virtiofsd binary found")
        return
    fs_root = _state["fs_root"]
    vm = create(
        "sdk-fs1", str(KERNEL),
        initramfs=str(IRFS_VIRTIOFS),
        cpus=1, memory_mb=256,
        layers=["marker", "base"],
        client=client,
    )
    _track("sdk-fs1")
    assert vm.info()["state"] == "Running"
    args = _ch_cmdline(vm.pid)
    assert "--fs" in args, "CH missing --fs device"
    assert any("shared=on" in a for a in args), "vhost-user needs shared memory"
    # the overlayfs mount lives in the supervisor's private mount-ns —
    # verify the composed tree through /proc/<sup>/root (host view of it)
    sup = _find_fs_supervisor("sdk-fs1")
    assert sup, "fs supervisor process not found"
    ns_merged = Path(f"/proc/{sup}/root{fs_root}/sdk-fs1/merged")
    assert (ns_merged / "usr/bin/hello.py").exists(), "marker layer missing"
    assert (ns_merged / "bin/busybox").exists(), "base layer missing"
    # writes through merged copy up into the VM's private upperdir,
    # never into the shared layers
    (ns_merged / "tmp/marker.txt").write_text("x")
    assert (fs_root / "sdk-fs1" / "upper" / "tmp/marker.txt").exists()
    assert not (Path(_state["state_dir"]) / "layers" / "base" / "tmp/marker.txt").exists()
    # teardown: VM destroy tears down the fs stack
    client.vm_destroy("sdk-fs1")
    _state["created"].remove("sdk-fs1")
    assert not (fs_root / "sdk-fs1").exists(), "fs work dir leaked"


def test_warm_attach_detach() -> None:
    """FS-M4 warm path: idle VM -> hot-plug layers -> guest mounts -> detach."""
    client = _state["client"]
    if not _state.get("vfsd"):
        print("SKIP test_warm_attach_detach: no virtiofsd binary found")
        return
    agent_irfs = REPO / "target/guest/initramfs-agent.cpio.gz"
    if not agent_irfs.exists():
        subprocess.run(["bash", "images/build-initramfs-agent.sh"], cwd=REPO, check=True)
    vm = create(
        "sdk-w1", str(KERNEL),
        initramfs=str(agent_irfs),
        cpus=1, memory_mb=256,
        client=client,
    )
    _track("sdk-w1")
    assert vm.info()["state"] == "Running"

    # hot-plug the layered fs and verify FROM INSIDE THE GUEST
    client.vm_attach_fs("sdk-w1", ["marker", "base"])
    out = _guest_exec("sdk-w1", ["ls", "/newroot"])
    assert "bin" in out and "usr" in out, out
    out = _guest_exec("sdk-w1", ["cat", "/newroot/usr/bin/hello.py"])
    assert "marker layer" in out, out

    # detach: guest umount + device removal + host stack teardown
    client.vm_detach_fs("sdk-w1")
    out = _guest_exec("sdk-w1", ["ls", "/newroot"])
    assert "bin" not in out, f"mount still present after detach: {out!r}"

    client.vm_destroy("sdk-w1")
    _state["created"].remove("sdk-w1")


def test_warm_pool() -> None:
    """Warm pool: create idle VMs -> claim -> guest exec -> release -> reclaim."""
    client = _state["client"]
    if not _state.get("vfsd"):
        print("SKIP test_warm_pool: no virtiofsd binary found")
        return
    agent_irfs = REPO / "target/guest/initramfs-agent.cpio.gz"
    if not agent_irfs.exists():
        subprocess.run(["bash", "images/build-initramfs-agent.sh"], cwd=REPO, check=True)

    created = client.pool_create(2, kernel=str(KERNEL))
    assert created["count"] == 2, created
    names = created["created"]
    _track(names)
    pool = client.pool_list()["pool"]
    assert all(not s["claimed"] for s in pool), pool

    # claim with layers -> the VM gets the fs hot-plugged
    claim = client.pool_claim(["marker", "base"])
    vm_name = claim["name"]
    assert claim["layers"] == ["marker", "base"], claim
    out = _guest_exec(vm_name, ["cat", "/newroot/usr/bin/hello.py"])
    assert "marker layer" in out, out
    # the other slot stays idle
    pool = client.pool_list()["pool"]
    claimed = [s for s in pool if s["claimed"]]
    assert len(claimed) == 1 and claimed[0]["name"] == vm_name, pool

    # second claim takes the other slot; third must be exhausted
    second = client.pool_claim(["marker", "base"])["name"]
    try:
        client.pool_claim(["marker", "base"])
        raise AssertionError("pool should be exhausted")
    except TerraError as e:
        assert "exhausted" in str(e), e

    client.pool_release(vm_name)
    client.pool_release(second)
    pool = client.pool_list()["pool"]
    assert all(not s["claimed"] for s in pool), pool

    # release of an idle slot is an error
    try:
        client.pool_release(vm_name)
        raise AssertionError("release of idle slot accepted")
    except TerraError:
        pass

    for n in names:
        client.vm_destroy(n)
    _state["created"] = [x for x in _state["created"] if x not in names]
    assert client.pool_list()["count"] == 0


def test_layered_boot_erofs() -> None:
    """EROFS image layers: built on the fly, mounted by the adapter."""
    client = _state["client"]
    if not _state.get("vfsd"):
        print("SKIP test_layered_boot_erofs: no virtiofsd binary found")
        return
    if not shutil.which("mkfs.erofs"):
        print("SKIP test_layered_boot_erofs: no mkfs.erofs")
        return
    layer_dir = Path(_state["state_dir"]) / "layers"
    # build EROFS images from the dir layers, under distinct names
    for src, name in ((layer_dir / "base", "ebase"), (layer_dir / "marker", "emarker")):
        subprocess.run(
            ["bash", "images/build-layer.sh", str(src), name, str(layer_dir)],
            cwd=REPO, check=True, capture_output=True,
        )
    vm = create(
        "sdk-fs2", str(KERNEL),
        initramfs=str(IRFS_VIRTIOFS),
        cpus=1, memory_mb=256,
        layers=["emarker", "ebase"],
        client=client,
    )
    _track("sdk-fs2")
    assert vm.info()["state"] == "Running"
    sup = _find_fs_supervisor("sdk-fs2")
    assert sup, "fs supervisor not found"
    ns_merged = Path(f"/proc/{sup}/root{_state['fs_root']}/sdk-fs2/merged")
    assert (ns_merged / "usr/bin/hello.py").exists(), "EROFS marker layer missing"
    assert (ns_merged / "bin/busybox").exists(), "EROFS base layer missing"
    client.vm_destroy("sdk-fs2")
    _state["created"].remove("sdk-fs2")


# ---------------------------------------------------------------------------
# Standalone runner
# ---------------------------------------------------------------------------
def main() -> int:
    tests = [(n, f) for n, f in globals().items() if n.startswith("test_")]
    setup_module()
    try:
        for name, fn in tests:
            try:
                fn()
                _PASSED.append(name)
                print(f"PASS {name}")
            except BaseException as e:  # noqa: BLE001 — report and continue
                _FAILED.append((name, e))
                print(f"FAIL {name}: {e}")
    finally:
        try:
            teardown_module()
        except BaseException as e:  # noqa: BLE001
            _FAILED.append(("teardown", e))
            print(f"FAIL teardown: {e}")
    print(f"\n{len(_PASSED)} passed, {len(_FAILED)} failed")
    return 1 if _FAILED else 0


if __name__ == "__main__":
    sys.exit(main())
