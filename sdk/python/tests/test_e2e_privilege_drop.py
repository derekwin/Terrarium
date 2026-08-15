#!/usr/bin/env python3
"""Root-daemon e2e: CH/virtiofsd privilege drop (L1 降权).

Runs the engine daemon AS ROOT (in-process), so the vmm-user resolution
activates and Cloud Hypervisor + virtiofsd must run under the dedicated
`terra-vmm` user instead of root. Verifies:

  - CH process uid == terra-vmm uid; virtiofsd uid == terra-vmm uid;
  - CH cmdline uses fd-backed net (``--net fd=``, no ``/dev/net/tun``
    Landlock rule — CH never opens the tap itself);
  - guest exec + layered virtiofs work, and the guest sees base files as
    root-owned (``--translate-uid host:<vmm>:0:1``);
  - snapshot + restore (net + layered fs) still work, restored CH also
    runs as terra-vmm.

Run as root (real KVM required):

    sudo python3 sdk/python/tests/test_e2e_privilege_drop.py
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO / "sdk" / "python"))

from terra.client import TerraClient  # noqa: E402
from terra.vm import create  # noqa: E402

KERNEL = REPO / "target/guest/vmlinux.bin"
INITRAMFS = REPO / "target/guest/alpine.cpio"
IRFS_VIRTIOFS = REPO / "target/guest/initramfs-virtiofs.cpio.gz"
SOCKET = "/tmp/terra-sdk-drop.sock"
VMM_USER = os.environ.get("TERRA_VMM_USER", "terra-vmm")


def _vmm_ids() -> tuple[int, int]:
    out = subprocess.run(
        ["id", "-u", VMM_USER], capture_output=True, text=True, check=True
    ).stdout.strip()
    uid = int(out)
    out = subprocess.run(
        ["id", "-g", VMM_USER], capture_output=True, text=True, check=True
    ).stdout.strip()
    return uid, int(out)


def _ensure_vmm_user() -> None:
    r = subprocess.run(["id", VMM_USER], capture_output=True)
    if r.returncode == 0:
        return
    subprocess.run(
        [
            "useradd", "--system", "--no-create-home",
            "--shell", "/usr/sbin/nologin", VMM_USER,
        ],
        check=True,
    )
    if subprocess.run(["getent", "group", "kvm"], capture_output=True).returncode == 0:
        subprocess.run(["usermod", "-aG", "kvm", VMM_USER], check=True)


def _ch_pids() -> list[int]:
    pids = []
    for p in Path("/proc").iterdir():
        if p.name.isdigit():
            try:
                if (p / "comm").read_text().strip() == "cloud-hyperviso":
                    pids.append(int(p.name))
            except (OSError, PermissionError):
                pass
    return pids


def _find_proc(needle: str) -> int | None:
    for p in Path("/proc").iterdir():
        if p.name.isdigit():
            try:
                if needle in (p / "cmdline").read_bytes().decode(errors="ignore"):
                    return int(p.name)
            except (OSError, PermissionError):
                pass
    return None


def _proc_uid(pid: int) -> int:
    return int(Path(f"/proc/{pid}/status").read_text().split("Uid:")[1].split()[0])


def _guest_exec(vm_name: str, args: list[str], timeout: float = 15.0) -> str:
    import json as _json
    import socket as _sock

    path = f"/tmp/terra-{vm_name}-vsock.sock"
    deadline = time.time() + timeout
    last: Exception | None = None
    while time.time() < deadline:
        try:
            s = _sock.socket(_sock.AF_UNIX, _sock.SOCK_STREAM)
            s.settimeout(3)
            s.connect(path)
            s.sendall(b"CONNECT 1024\n")
            f = s.makefile("rw")
            assert f.readline().startswith("OK")
            f.write(_json.dumps({"command": "exec", "args": args}) + "\n")
            f.flush()
            resp = _json.loads(f.readline())
            s.close()
            if resp.get("status") == "ok":
                return resp["data"]["stdout"]
            raise RuntimeError(resp.get("message"))
        except (OSError, AssertionError, RuntimeError) as e:
            last = e
            time.sleep(0.4)
    raise RuntimeError(f"guest exec {args} failed: {last}")


def _guest_exec_ok(vm_name: str, args: list[str], want: str = "ok\n") -> bool:
    try:
        return _guest_exec(vm_name, args, timeout=6).strip() == want.strip()
    except RuntimeError:
        return False


def _wait_for(cond, timeout: float, what: str) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if cond():
            return
        time.sleep(0.3)
    raise RuntimeError(f"timeout waiting for: {what}")


def _build_irfs_virtiofs() -> None:
    import terrarium_fs

    src = REPO / "target/guest/rootfs"
    init = str(REPO / "images" / "rootfs" / "init-virtiofs")
    terrarium_fs.build_initramfs_virtiofs(str(src), init, str(IRFS_VIRTIOFS))


def main() -> int:
    if os.geteuid() != 0:
        print("must run as root (the daemon must be root for the vmm drop)", file=sys.stderr)
        return 2
    for req, hint in (
        (Path("/dev/kvm").exists(), "/dev/kvm missing"),
        (shutil.which("cloud-hypervisor") or Path("/home/liujinyao/.local/bin/cloud-hypervisor").exists(), "cloud-hypervisor missing"),
        (KERNEL.exists(), f"{KERNEL} missing (images/build.sh)"),
        (INITRAMFS.exists(), f"{INITRAMFS} missing"),
    ):
        if not req:
            print(f"preflight failed: {hint}", file=sys.stderr)
            return 2

    _ensure_vmm_user()
    vmm_uid, vmm_gid = _vmm_ids()
    ch = shutil.which("cloud-hypervisor") or "/home/liujinyao/.local/bin/cloud-hypervisor"
    vfsd = (
        os.environ.get("TERRA_VIRTIOFSD")
        or shutil.which("virtiofsd")
        or str(Path.home() / ".cargo/bin/virtiofsd")
    )
    if not Path(vfsd).exists():
        print(f"preflight failed: virtiofsd missing ({vfsd})", file=sys.stderr)
        return 2

    state_dir = Path(tempfile.mkdtemp(prefix="terra-drop-"))
    # mkdtemp is 0700 root; the vmm user must traverse the path chain to
    # reach the per-VM fs/snapshot trees (production: $TERRA_HOME 0755).
    state_dir.chmod(0o755)
    layer_dir = state_dir / "layers"
    (layer_dir / "base").mkdir(parents=True)
    subprocess.run(
        f"zcat {INITRAMFS} | cpio -idm --quiet",
        shell=True, cwd=layer_dir / "base", check=True,
    )
    marker = layer_dir / "marker" / "usr" / "bin"
    marker.mkdir(parents=True)
    (marker / "hello.py").write_text("print('hello from marker layer')\n")
    if not IRFS_VIRTIOFS.exists():
        _build_irfs_virtiofs()
    snap_dir = state_dir / "snapshots"
    snap_dir.mkdir()

    if Path(SOCKET).exists():
        Path(SOCKET).unlink()
    os.environ.update({
        "TERRA_STATE_DIR": str(state_dir / "vms"),
        "TERRA_LAYER_DIR": str(layer_dir),
        "TERRA_SNAPSHOT_DIR": str(snap_dir),
        "TERRA_CH_BINARY": ch,
        "TERRA_VIRTIOFSD": vfsd,
        "TERRA_VMM_USER": VMM_USER,
    })

    import terrarium_engine
    terrarium_engine.start_daemon(SOCKET, ch_binary=ch)
    _wait_for(lambda: Path(SOCKET).exists(), 10, "daemon socket")
    client = TerraClient(socket_path=SOCKET)
    print(f"[setup] root daemon on {SOCKET}; vmm user={VMM_USER} uid={vmm_uid}")

    created: list[str] = []
    failures: list[tuple[str, BaseException]] = []
    try:
        # 1) layered boot + net: CH and virtiofsd must run as terra-vmm
        vm = create(
            "drop1", str(KERNEL),
            initramfs=str(IRFS_VIRTIOFS),
            cpus=1, memory_mb=256,
            layers=["marker", "base"],
            net=True,
            client=client,
        )
        created.append("drop1")
        assert vm.info()["state"] == "Running"
        ch_pid = vm.pid
        fsd_pid = _find_proc(f"terra-drop1-fs.sock")
        assert fsd_pid, "virtiofsd not found"
        ch_uid = _proc_uid(ch_pid)
        fsd_uid = _proc_uid(fsd_pid)
        print(f"  CH uid={ch_uid} virtiofsd uid={fsd_uid} (vmm={vmm_uid})")
        assert ch_uid == vmm_uid, f"CH running as uid {ch_uid}, want {vmm_uid}"
        assert fsd_uid == vmm_uid, f"virtiofsd running as uid {fsd_uid}, want {vmm_uid}"

        # CH must use the fd-backed tap and not whitelist /dev/net/tun
        cmdline = Path(f"/proc/{ch_pid}/cmdline").read_bytes().decode().split("\0")
        net_idx = cmdline.index("--net")
        net_arg = cmdline[net_idx + 1]
        assert net_arg.startswith("fd=") and "id=net0" in net_arg, net_arg
        assert not any("/dev/net/tun" in a for a in cmdline), cmdline

        # guest works; ownership is translated back to guest root
        out = _guest_exec("drop1", ["cat", "/usr/bin/hello.py"])
        assert "marker layer" in out, out
        out = _guest_exec("drop1", ["ls", "-ln", "/bin/busybox"])
        assert out.split()[2] == "0", f"base file not guest-root owned: {out}"
        _guest_exec("drop1", ["sh", "-c", "echo x > /tmp/owned-by-guest"])
        out = _guest_exec("drop1", ["ls", "-ln", "/tmp/owned-by-guest"])
        assert out.split()[2] == "0", f"guest-created file not root owned: {out}"

        # 2) snapshot + restore (net + layered fs) under the vmm user
        # Explicit path: destroy GC's the DEFAULT location
        # ({snapshot_dir}/terra-snap-<name>) — the keep path survives.
        keep = snap_dir / "keep-drop1"
        snap = client.vm_snapshot("drop1", snapshot_path=str(keep))
        snap_path = snap["snapshot_path"]
        client.vm_destroy("drop1")
        created.remove("drop1")
        _wait_for(lambda: not _ch_pids(), 10, "CH cleanup after destroy")
        client.vm_restore(
            "drop1r", snap_path,
            layers=["marker", "base"],
            net=True,
        )
        created.append("drop1r")
        _wait_for(lambda: _guest_exec_ok("drop1r", ["echo", "ok"]), 25, "restored guest exec")
        ch_pid = _find_proc("terra-drop1r.sock")
        fsd_pid = _find_proc("terra-drop1r-fs.sock")
        assert ch_pid and fsd_pid, "restored CH/virtiofsd not found"
        assert _proc_uid(ch_pid) == vmm_uid, "restored CH not under vmm user"
        assert _proc_uid(fsd_pid) == vmm_uid, "restored virtiofsd not under vmm user"
        out = _guest_exec("drop1r", ["cat", "/usr/bin/hello.py"])
        assert "marker layer" in out, out
        print("  snapshot+restore OK (both processes still under vmm)")
    except BaseException as e:  # noqa: BLE001
        import traceback

        traceback.print_exc()
        failures.append(("e2e", e))
    finally:
        for name in created:
            try:
                client.vm_destroy(name)
            except Exception:
                pass
        time.sleep(1)
        leftovers = _ch_pids()
        if leftovers:
            failures.append(("teardown", RuntimeError(f"CH leaked: {leftovers}")))
        try:
            Path(SOCKET).unlink()
        except FileNotFoundError:
            pass
        if failures:
            print(f"[teardown] state kept: {state_dir}")
        else:
            shutil.rmtree(state_dir, ignore_errors=True)

    if failures:
        for name, e in failures:
            print(f"FAIL {name}: {e}", file=sys.stderr)
        return 1
    print("PASS: CH/virtiofsd privilege drop (root daemon, vmm user)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
