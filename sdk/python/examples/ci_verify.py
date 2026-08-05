#!/usr/bin/env python3
"""Isolated CI verification for agent-written code (dogfooding demo).

The Agent-CI use case: an agent produces a patch against a repo; the
patch must pass the test suite in an ISOLATED environment before it is
merged. Terrarium's layer toolchain makes that environment cheap and
reusable:

  - the repo + test toolchain are baked into a LAYER once
    (images/examples/ci-terra.sh for this repo, with a build-time
    self-check that the baseline suite passes)
  - each verification restores a snapshot (~200ms), copies the pristine
    repo into the per-VM workspace, applies the agent's patch and runs
    the test command
  - reset_in_place() returns the environment to the pristine baseline
    for the next submission

Usage:
    python3 sdk/python/examples/ci_verify.py            # demo: good+bad patch
    python3 sdk/python/examples/ci_verify.py --patch p.diff --test "pytest ..."

Requires: the ci-terra layer (or any layer carrying /opt/<repo> + test
toolchain), sudo daemon, real KVM.
"""

from __future__ import annotations

import argparse
import base64
import os
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

DEFAULT_LAYERS = ["ci-terra", "ubuntu"]
DEFAULT_TEST = (
    "cd /workdir/terrarium && PYTHONPATH=sdk/python python3.12 "
    "-m pytest -m 'not e2e' sdk/python/tests -q"
)


class CiVerify:
    """One pristine snapshot + repeated isolated patch verifications."""

    def __init__(
        self,
        layers: list[str] | None = None,
        snapshot: str = "/tmp/ci-verify-snap",
        repo_source: str = "/opt/terrarium",
        repo_work: str = "/workdir/terrarium",
    ):
        self.layers = list(layers or DEFAULT_LAYERS)
        self.snapshot = snapshot
        self.repo_source = repo_source
        self.repo_work = repo_work
        DaemonManager().ensure_running()
        c = TerraClient()
        if not os.path.isdir(self.snapshot):
            Sandbox(tenant="ci-verify-src", layers=self.layers)
            c.vm_snapshot("tenant-ci-verify-src", self.snapshot)
            c.vm_destroy("tenant-ci-verify-src")
            print(f"pristine snapshot: {self.snapshot}", flush=True)

    def verify(self, patch: str, test_cmd: str = DEFAULT_TEST) -> dict:
        """Apply *patch* in an isolated env, run *test_cmd*, return verdict."""
        b64 = base64.b64encode(patch.encode()).decode()
        with Batch(self.snapshot, 1, layers=self.layers, prefix="civ-") as envs:
            envs.exec(
                ["sh", "-c", f"cp -r {self.repo_source} {self.repo_work}"],
                timeout_secs=60,
            )
            applied = envs.exec(
                ["sh", "-c", f"echo {b64} | base64 -d > /tmp/agent.patch && cd {self.repo_work} && git init -q && git apply /tmp/agent.patch && rm /tmp/agent.patch"],
                timeout_secs=30,
            )
            ap = next(iter(applied.values()))
            if ap.get("exit_code", -1) != 0:
                envs.reset_in_place()
                return {"passed": False, "exit_code": ap.get("exit_code"),
                        "summary": "patch apply failed: " + (ap.get("stderr") or "")[-120:]}
            r = envs.exec(["sh", "-c", test_cmd], timeout_secs=300)
            out = next(iter(r.values()))
            stdout = out.get("stdout", "")
            exit_code = out.get("exit_code", -1)
            envs.reset_in_place()
        passed = exit_code == 0 and "passed" in stdout
        return {
            "passed": passed,
            "exit_code": exit_code,
            "summary": stdout.strip().splitlines()[-1] if stdout else "",
        }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--patch", default=None, help="git diff file to verify")
    ap.add_argument("--test", default=DEFAULT_TEST, help="test command (sh -c)")
    args = ap.parse_args()

    cv = CiVerify()
    if args.patch:
        patch = Path(args.patch).read_text()
        r = cv.verify(patch, args.test)
        print(f"{'PASS' if r['passed'] else 'FAIL'}: {r['summary']}")
        return 0 if r["passed"] else 1

    patches = Path(__file__).resolve().parent / "patches"
    good = (patches / "good.diff").read_text()
    bad = (patches / "bad.diff").read_text()

    print("=== good agent patch ===", flush=True)
    t0 = time.perf_counter()
    g = cv.verify(good)
    print(f"  verdict: {'PASS' if g['passed'] else 'FAIL'} ({g['summary']}) "
          f"[{time.perf_counter()-t0:.1f}s]", flush=True)

    print("=== bad agent patch (must be rejected) ===", flush=True)
    t0 = time.perf_counter()
    b = cv.verify(bad)
    print(f"  verdict: {'PASS' if b['passed'] else 'FAIL'} ({b['summary']}) "
          f"[{time.perf_counter()-t0:.1f}s]", flush=True)

    ok = g["passed"] and not b["passed"]
    print("CI verify demo:", "OK" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
