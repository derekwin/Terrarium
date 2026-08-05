#!/usr/bin/env python3
"""Batch SWE-bench verification on Terrarium (agent-execution proof).

For each SWE-bench instance:
  1. build the environment LAYER (repo @ base_commit + pinned toolchain +
     SWE-bench test_patch baked; build-time self-check: FTP tests FAIL)
  2. snapshot the ready state
  3. restore 2 parallel envs: baseline FTP fails -> apply the gold patch
     -> FTP + PASS_TO_PASS all pass -> reset_in_place restores the broken
     baseline

Reports per-instance results and the pass rate. This is the "real
workload" proof for the Agent-execution use case: standard SWE-bench
instances, not self-written tasks.

Requirements: real KVM host, `terra setup ubuntu`, sudo-started daemon
(root runs the NAT bridge + mounts layers), and the standalone CPython
3.10 tarball pre-seeded into the builder upper as /py310.tar.gz (the
build script falls back to downloading it from GitHub).

Run (against the running sudo daemon, as the regular user):
    python3 sdk/python/tests/manual_swebench_batch.py
"""

from __future__ import annotations

import ast
import base64
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
if str(REPO) not in sys.path:
    sys.path.insert(0, str(REPO / "sdk" / "python"))

from terra.batch import Batch  # noqa: E402
from terra.client import TerraClient  # noqa: E402
from terra.sandbox import Sandbox  # noqa: E402
from terra._engine import DaemonManager  # noqa: E402

INSTANCES = [
    "pallets__flask-4160",
    "pallets__flask-4169",
    "pallets__flask-4544",
    "pallets__flask-4935",
    "pallets__flask-5014",
]
# Note: pallets__flask-4992 (Config.from_file TOML) needs Python 3.11+
# (tomllib); our pinned toolchain is py3.10, so it is excluded — an
# environment requirement, not a Terrarium failure.

# flask era -> pinned toolchain (verified on py3.10 in the host probe)
DEPS = {
    "2.0": ('"werkzeug==2.0.3" "jinja2==3.0.3" "itsdangerous==2.0.1" "click==8.1.7" "pytest==6.2.5"', "pytest==6.2.5"),
    "2.1": ('"werkzeug==2.1.2" "jinja2==3.1.2" "itsdangerous==2.1.2" "click==8.1.7" "pytest==6.2.5"', "pytest==6.2.5"),
    "2.3": ('"werkzeug==2.3.6" "jinja2==3.1.2" "itsdangerous==2.1.2" "click==8.1.7" "pytest==7.4.4"', "pytest==7.4.4"),
}

INSTANCE_DIR = Path("/tmp/swe-instances")
LAYER_PREFIX = "swe"
SNAPS = Path("/tmp/swe-batch-snaps")


def parse(v):
    if isinstance(v, list):
        return v
    try:
        return ast.literal_eval(v)
    except Exception:
        return []


def layer_name(iid: str) -> str:
    return f"{LAYER_PREFIX}-{iid.split('-')[-1]}"


def layer_exists(iid: str) -> bool:
    d = Path(os.environ["TERRA_HOME"]) / "layers"
    name = layer_name(iid)
    return (d / name).is_dir() or (d / f"{name}.erofs").exists()


def build_script(instance: dict, version: str) -> str:
    """Generate the layer-build script for one instance."""
    deps, pytest_pin = DEPS[version]
    test_patch = instance["test_patch"]
    ftp = parse(instance["FAIL_TO_PASS"])
    ftp_ids = " ".join(f'"{t}"' for t in ftp)
    return f"""#!/bin/sh
set -e

# background DHCP from the ubuntu layer init; wait for an IPv4 address
i=0
while [ $i -lt 30 ]; do
    if ip addr show eth0 2>/dev/null | grep -q 'inet '; then break; fi
    sleep 1; i=$((i + 1))
done

sed -i 's|archive.ubuntu.com|mirrors.aliyun.com|g; s|security.ubuntu.com|mirrors.aliyun.com|g' \\
    /etc/apt/sources.list.d/ubuntu.sources 2>/dev/null || true
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \\
    curl git ca-certificates >/dev/null

mkdir -p /opt/python310
if [ -f /py310.tar.gz ]; then
    cp /py310.tar.gz /tmp/py310.tar.gz
else
    curl -sL --fail -o /tmp/py310.tar.gz \\
        "https://github.com/astral-sh/python-build-standalone/releases/download/20241016/cpython-3.10.15%2B20241016-x86_64-unknown-linux-gnu-install_only.tar.gz"
fi
tar -xzf /tmp/py310.tar.gz -C /opt/python310 --strip-components=1
rm -f /tmp/py310.tar.gz
PY=/opt/python310/bin/python3.10
$PY -m pip --version >/dev/null 2>&1 || $PY -m ensurepip --upgrade >/dev/null
$PY -m pip install --break-system-packages --root-user-action=ignore \\
    -i https://mirrors.aliyun.com/pypi/simple/ \\
    {deps}

rm -rf /opt/flask
git clone -q https://github.com/pallets/flask /opt/flask
cd /opt/flask
git checkout -q {instance["base_commit"]}

git apply <<'TESTPATCH'
{test_patch}
TESTPATCH

if PYTHONPATH=src $PY -m pytest {ftp_ids} -q 2>&1 | grep -q "failed"; then
    echo "baseline OK: FTP fails (bug reproduced)"
else
    echo "baseline check failed: FTP did NOT fail" >&2
    exit 1
fi
# the erofs pack runs as the invoking (non-root) user: open up anything
# others can't read/traverse that packages left behind (apt cache is
# purged by tool create; /root is intentionally excluded).
find /etc /usr /var /opt /srv /home -xdev \\( -type d ! -perm -o+x -o -type f ! -perm -o+r \\) -exec chmod o+rX {{}} + 2>/dev/null || true
echo "swebench layer ready"
"""


def seed_python(instance: str) -> None:
    """Clean + pre-seed the builder upper with the py310 tarball."""
    name = layer_name(instance)
    upper = Path(os.environ["TERRA_HOME"]) / "state/fs/uppers" / f"lb-{name}"
    subprocess.run(
        ["docker", "run", "--rm", "-v", f"{upper}:/u", "python:3.12-slim",
         "sh", "-c", "rm -rf /u/* && mkdir -p /u"],
        check=False,
    )
    subprocess.run(
        ["docker", "run", "--rm",
         "-v", f"{upper}:/u",
         "-v", "/tmp/py310-uv.tar.gz:/py310.tar.gz",
         "python:3.12-slim", "sh", "-c", "cp /py310.tar.gz /u/py310.tar.gz"],
        check=True,
    )


def build_layer(instance: str, version: str) -> tuple[bool, float]:
    if layer_exists(instance):
        print(f"  layer exists, skipping build", flush=True)
        return True, 0.0
    iid = instance
    data = json.load(open(INSTANCE_DIR / f"{iid}.json"))
    script = build_script(data, version)
    script_path = Path(f"/tmp/swebuild-{iid}.sh")
    script_path.write_text(script)
    seed_python(instance)
    t0 = time.perf_counter()
    env = dict(os.environ)
    cmd = [
        sys.executable, "-m", "terra", "tool", "create",
        "-n", layer_name(instance),
        "--template", "ubuntu",
        "--script", str(script_path),
        "--timeout", "1100",
    ]
    r = subprocess.run(cmd, capture_output=True, text=True, env=env)
    dt = time.perf_counter() - t0
    ok = f"tool layer '{layer_name(instance)}' built" in r.stdout
    if not ok:
        print(f"  BUILD FAIL: {r.stdout[-400:]} {r.stderr[-200:]}", flush=True)
    return ok, dt


def verify(instance: str) -> tuple[bool, dict]:
    """One instance: snapshot -> 2 envs -> baseline/gold/reset."""
    iid = instance
    data = json.load(open(INSTANCE_DIR / f"{iid}.json"))
    ftp = parse(data["FAIL_TO_PASS"])
    ptp = parse(data["PASS_TO_PASS"])
    # Test files touched by the test_patch: run them in full for the
    # regression check. Parametrized PTP ids can contain embedded
    # newlines (multi-line ids), which break shell-joined pytest args —
    # whole-file runs cover the same tests without that fragility.
    test_files = sorted(set(re.findall(r"tests/[\w/]+\.py", data.get("test_patch", ""))))
    gold = data["patch"].encode()
    b64 = base64.b64encode(gold).decode()
    layers = [layer_name(instance), "ubuntu"]
    snap = str(SNAPS / f"{iid}.snap")
    # fresh snapshot dir (previous runs leave root-owned dirs behind)
    subprocess.run(
        ["docker", "run", "--rm", "-v", f"{SNAPS}:/s", "python:3.12-slim",
         "sh", "-c", f"rm -rf /s/{iid}.snap"],
        check=False,
    )

    DaemonManager().ensure_running()
    c = TerraClient()
    tenant = f"batch-{layer_name(instance)}"
    Sandbox(tenant=tenant, layers=layers)
    c.vm_snapshot(f"tenant-{tenant}", snap)
    c.vm_destroy(f"tenant-{tenant}")

    with Batch(snap, 2, layers=layers, prefix=f"b-{iid}-") as envs:
        prep = "cp -r /opt/flask /workdir/flask"
        base = envs.collect(
            ["sh", "-c", f"{prep} && cd /workdir/flask && PYTHONPATH=src /opt/python310/bin/python3.10 -m pytest {' '.join('\"'+t+'\"' for t in ftp)} -q 2>&1 | tail -1"],
            timeout_secs=60,
        )
        baseline_ok = all("failed" in v for v in base.values())

        envs.exec(
            ["sh", "-c", f"echo {b64} | base64 -d > /tmp/fix.patch && cd /workdir/flask && git apply /tmp/fix.patch && rm /tmp/fix.patch"],
            timeout_secs=30,
        )
        fixed = envs.collect(
            ["sh", "-c", f"cd /workdir/flask && PYTHONPATH=src /opt/python310/bin/python3.10 -m pytest {' '.join('\"'+t+'\"' for t in ftp)} {' '.join(test_files)} -q 2>&1 | tail -1"],
            timeout_secs=120,
        )
        fixed_ok = all("passed" in v for v in fixed.values())

        envs.reset_in_place()
        after = envs.collect(
            ["sh", "-c", f"{prep} && cd /workdir/flask && PYTHONPATH=src /opt/python310/bin/python3.10 -m pytest {' '.join('\"'+t+'\"' for t in ftp)} -q 2>&1 | tail -1"],
            timeout_secs=60,
        )
        reset_ok = all("failed" in v for v in after.values())

    ok = baseline_ok and fixed_ok and reset_ok
    return ok, {
        "baseline": baseline_ok,
        "fixed": fixed_ok,
        "reset": reset_ok,
        "ftp": len(ftp),
        "ptp": len(ptp),
    }


def main() -> int:
    if not os.path.exists("/dev/kvm"):
        print("requires /dev/kvm", file=sys.stderr)
        return 2
    SNAPS.mkdir(parents=True, exist_ok=True)
    results: dict[str, dict] = {}
    for iid in INSTANCES:
        data = json.load(open(INSTANCE_DIR / f"{iid}.json"))
        version = str(data.get("version") or "")
        if version not in DEPS:
            print(f"{iid}: unsupported flask version {version!r}", flush=True)
            results[iid] = {"build": False, "error": f"version {version}"}
            continue
        print(f"\n=== {iid} (flask {version}) ===", flush=True)
        built, bt = build_layer(iid, version)
        if not built:
            results[iid] = {"build": False}
            continue
        t0 = time.perf_counter()
        try:
            ok, detail = verify(iid)
            results[iid] = {"build": True, "ok": ok, **detail}
            print(f"  -> {'PASS' if ok else 'FAIL'} ({time.perf_counter()-t0:.0f}s)", flush=True)
        except Exception as e:  # noqa: BLE001
            results[iid] = {"build": True, "ok": False, "error": str(e)[:200]}
            print(f"  -> ERROR {e}", flush=True)

    print("\n=== SUMMARY ===")
    n_pass = sum(1 for r in results.values() if r.get("ok"))
    for iid, r in results.items():
        status = "PASS" if r.get("ok") else ("BUILD-FAIL" if not r.get("build") else "FAIL")
        print(f"  {iid}: {status}" + (f" ({r.get('error')})" if r.get("error") else ""))
    print(f"pass rate: {n_pass}/{len(INSTANCES)}")
    out = REPO / "docs/benchmark-results-2026-08-05-swebench-batch.json"
    out.write_text(json.dumps(results, indent=2) + "\n")
    print(f"results: {out}")
    return 0 if n_pass == len(INSTANCES) else 1


if __name__ == "__main__":
    raise SystemExit(main())
