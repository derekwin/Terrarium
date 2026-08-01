"""Manual real-KVM e2e verification of the function-completion features.

REQUIRES: a running daemon with root/NAT privileges (sudo terra daemon
start) and KVM + guest assets. Not part of CI (no KVM runner).

Run:  python3 sdk/python/tests/manual_e2e.py

Covers: background sessions (SDK Session lifecycle, 3600s default
timeout), CLI --detach + sandbox session subcommands, pool grow/scale
delta semantics, CPU-shrink rejection, binary download round-trip.
"""

import asyncio
import json
import os
import subprocess
import sys
import time

REPO = "/home/liujinyao/2606/Terrarium"
failures = []


def check(label, cond, detail=""):
    status = "PASS" if cond else "FAIL"
    print(f"[{status}] {label}" + (f" — {detail}" if detail and not cond else ""))
    if not cond:
        failures.append(label)


from terra.client import TerraClient
from terra.sandbox import Sandbox
from terra.pool import Pool
from terra.exceptions import TerraError

c = TerraClient()
KERNEL = f"{REPO}/target/guest/vmlinux.bin"
IRFS = f"{REPO}/target/guest/initramfs-virtiofs.cpio.gz"

# ── 1. Background session via SDK Session ─────────────────────────────
sb = Sandbox(tenant="e2e-bg", layers=["base"])
check("sandbox created", sb.id.startswith("sb-"), sb.id)

s = sb.exec(["sleep", "3"], background=True)
check("background exec returns Session", type(s).__name__ == "Session", str(type(s)))
check("session has id", s.session_id.startswith("ses-") or len(s.session_id) > 4, s.session_id)

st = s.status()
check("session status running", st.get("status") == "running", json.dumps(st)[:100])

# wait for completion
for _ in range(40):
    st = s.status()
    if st.get("status") != "running":
        break
    time.sleep(0.25)
check("session completes", st.get("status") in ("completed", "failed"), json.dumps(st)[:100])
check("session exit_code 0", st.get("exit_code") == 0, json.dumps(st)[:100])

# kill a long-running session
s2 = sb.exec(["sleep", "60"], background=True)
time.sleep(1)
check("second session running", s2.status().get("status") == "running")
s2.kill()
st = s2.status()
check("session kill works", st.get("status") in ("killed", "completed"), json.dumps(st)[:100])

# ── 2. Background with default 3600 timeout (not killed at 600s) ──────
s3 = sb.exec(["sleep", "15"], background=True)
time.sleep(2)
check("background not prematurely killed (600s default)", s3.status().get("status") == "running")
s3.kill()

# ── 3. session_list shows engine-tracked sessions ─────────────────────
lst = c.session_list()
sess = [x for x in lst.get("sessions", []) if x.get("sandbox") == sb.id]
check("session_list has sandbox sessions", len(sess) >= 3, json.dumps(lst)[:150])

# ── 4. CLI session consistency ─────────────────────────────────────────
env = dict(os.environ)
env.update({"TERRA_SOCKET": "/tmp/terra.sock"})


def cli(*args):
    return subprocess.run(["python3", "-m", "terra", "--json", *args], capture_output=True,
                          text=True, env=env, cwd=REPO)


r = cli("sandbox", "exec", sb.id, "--detach", "--", "sh", "-c", "echo cli-bg && sleep 60")
check("CLI --detach returns session", r.returncode == 0 and "session_id" in r.stdout,
      f"{r.returncode}: {r.stdout[:120]} {r.stderr[:120]}")
cli_sid = json.loads(r.stdout).get("session_id") if r.stdout else None
if cli_sid:
    time.sleep(1)
    r = cli("sandbox", "session", "status", cli_sid)
    check("CLI session status", r.returncode == 0 and "running" in r.stdout, r.stdout[:120])
    r = cli("sandbox", "session", "kill", cli_sid)
    check("CLI session kill", r.returncode == 0, r.stdout[:120] + r.stderr[:120])
r = cli("sandbox", "session", "ls")
check("CLI session ls", r.returncode == 0 and "count" in r.stdout, r.stdout[:120])

# ── 5. Pool grow (delta) and scale (to target) ─────────────────────────
pool = Pool(size=1, layers=["base"])
time.sleep(2)
st = pool.status()
check("pool starts at 1 idle", st["idle"] == 1 and st["claimed"] == 0, json.dumps(st))

pool.grow(2)  # delta: should add 2, not 3
time.sleep(6)
st = pool.status()
check("grow(2) adds exactly 2 (total 3)", st["idle"] == 3, json.dumps(st))

pool.scale(1)  # shrink to exactly 1 idle
time.sleep(3)
st = pool.status()
check("scale(1) shrinks to 1 idle", st["idle"] == 1 and st["claimed"] == 0, json.dumps(st))

# ── 6. CPU shrink rejected ─────────────────────────────────────────────
try:
    sb.resize(cpu=4)
    info = c.vm_info(sb.vm)
    check("resize up works", info.get("cpus") == 4, json.dumps(info))
    try:
        sb.resize(cpu=2)
        check("CPU shrink rejected", False, "resize down succeeded unexpectedly")
    except TerraError as e:
        check("CPU shrink rejected", "shrink" in str(e).lower(), str(e)[:120])
except TerraError as e:
    check("resize up works", False, str(e)[:120])

# ── 7. Binary download round-trip ──────────────────────────────────────
import base64 as _b64
binary = bytes(range(256)) * 4  # 1KB with all byte values
b64enc = _b64.b64encode(binary).decode()
sb.exec(["sh", "-c", f"echo {b64enc} | base64 -d > bin.dat"])
sb.files.download("bin.dat", "/tmp/e2e_bin_download.dat")
with open("/tmp/e2e_bin_download.dat", "rb") as f:
    got = f.read()
check("binary download round-trip", got == binary, f"len {len(got)} vs {len(binary)}")

# ── cleanup ────────────────────────────────────────────────────────────
sb.kill()
Sandbox.destroy_tenant("e2e-bg")
try:
    for name in [s["name"] for s in c.pool_list().get("pool", [])]:
        c.vm_destroy(name)
except Exception:
    pass

print("\n" + ("ALL PASS" if not failures else f"{len(failures)} FAILURES: {failures}"))
sys.exit(1 if failures else 0)
