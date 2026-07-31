# Production-Readiness Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Bring Terrarium to production-grade engineering standards — working CI gates, automated dependency governance, and release traceability — without adding features, abstractions, or runtime dependencies to the existing core.

**Architecture:** The core (engine daemon, adapters, protocol, SDK, MCP) is functionally complete and stays untouched. All changes are confined to the *engineering envelope*: test infrastructure, CI workflows, and governance files. This is repair and automation of what should already exist, not new design.

**Tech Stack:** pytest (test-only), cargo-deny (CI tool), dependabot (CI tool), Rust/Python toolchains already present.

**Governing principles (non-negotiable):**
1. **No new features** — zero product surface changes (no protocol commands, no SDK/MCP tools, no engine behavior).
2. **No new runtime dependencies** — everything added is test-time or CI-time only.
3. **No new abstractions** — no new traits, base classes, or wrapper layers; reuse existing patterns (pytest classes, CI jobs).
4. **Tests must fail without the fix** — every new test guards a real regression or a real gap.

**Current gaps being repaired:**
- CI's Python job is a smoke test only (`import terra`) — it runs **zero** tests.
- No pytest marker layering → e2e tests (real KVM) cannot be excluded from CI.
- No dependency governance (no license/advisory gate beyond `cargo audit`, no update bot).
- No CHANGELOG; README Roadmap is stale (pool↔sandbox integration shipped but unchecked).

---

### Task 1: Add `e2e` pytest marker and tag real-KVM tests

**Files:**
- Modify: `sdk/python/pyproject.toml` (pytest config)
- Modify: `sdk/python/tests/test_sandbox.py` (decorators)
- Modify: `sdk/python/tests/test_e2e_real.py` (decorators)

**Step 1: Register the marker**

In `sdk/python/pyproject.toml`, add a `[tool.pytest.ini_options]` section (create if absent):

```toml
[tool.pytest.ini_options]
markers = [
    "e2e: requires /dev/kvm and guest assets (skipped in CI)",
]
```

**Step 2: Tag the KVM-dependent tests**

In `test_sandbox.py`, decorate the two classes whose tests spawn real VMs. The module docstring already states it requires KVM; make it explicit per test class:

```python
@pytest.mark.e2e
class TestSandboxCreateAndExec:
    ...
```

Apply `@pytest.mark.e2e` to **every** test class in `test_sandbox.py` (all 8 classes spawn VMs via `Sandbox(...)`) and to the module-level test functions in `test_e2e_real.py` (prepend `@pytest.mark.e2e` above each `def test_*`).

Do NOT tag: `tests/test_client_protocol.py` and `tests/test_sandbox_logic.py` (created in Tasks 2–3) — those must run in CI.

**Step 3: Verify marker collection**

Run: `python3 -m pytest sdk/python/tests --collect-only -q`
Expected: every test in `test_sandbox.py`/`test_e2e_real.py` listed with `[e2e]`, and no unknown-marker warning on stderr (marker registered).

**Step 4: Verify e2e exclusion works**

Run: `python3 -m pytest sdk/python/tests -m "not e2e" --collect-only -q`
Expected: only the non-e2e tests (Tasks 2–3) collected.

**Step 5: Commit**

```bash
git add sdk/python/pyproject.toml sdk/python/tests/test_sandbox.py sdk/python/tests/test_e2e_real.py
git commit -m "test(sdk): mark real-KVM tests as @pytest.mark.e2e"
```

---

### Task 2: Protocol-layer mock tests for TerraClient

**Files:**
- Create: `sdk/python/tests/test_client_protocol.py`

**Step 1: Write the failing test**

The gap this guards: client methods must build exactly the protocol JSON the engine expects. A regression here (like the MCP `layers` omission that bricked cold-booted VMs) is invisible without daemon-side testing. Mock `_send` and assert the command dicts.

```python
"""TerraClient protocol construction — pure logic, no daemon."""
from unittest.mock import Mock, patch

import pytest

from terra.client import TerraClient


@pytest.fixture
def client():
    c = TerraClient()
    c._connect = Mock(return_value=None)  # never actually connects
    return c


def _captured(mock_send):
    assert mock_send.call_count == 1
    return mock_send.call_args.args[0]


def test_sandbox_create_carries_full_spec(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.sandbox_create(
            "team", policy={"memory_mb": 256}, pool=True,
            kernel="/k", initramfs="/i", layers=["base"], cpus=1,
            max_cpus=16, memory_mb=256, net=False,
        )
    cmd = _captured(m)
    assert cmd["command"] == "sandbox_create"
    assert cmd["tenant"] == "team"
    assert cmd["layers"] == ["base"]
    assert cmd["kernel"] == "/k"
    assert cmd["cpus"] == 1
    assert "pool" not in cmd or cmd["pool"] is True  # default → omitted


def test_sandbox_create_pool_false_is_explicit(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.sandbox_create("team", pool=False, layers=["base"])
    cmd = _captured(m)
    assert cmd["pool"] is False


def test_vm_exec_sandbox_flag_only_when_true(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.vm_exec("vm", ["echo", "hi"])
    cmd = _captured(m)
    assert "sandbox" not in cmd  # default: unsandboxed VM exec, omit flag

    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.vm_exec("vm", ["echo", "hi"], sandbox=True)
    cmd = _captured(m)
    assert cmd["sandbox"] is True


def test_sandbox_exec_defaults_sandbox_omitted(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.sandbox_exec("sb-1", ["echo", "hi"])
    cmd = _captured(m)
    assert "sandbox" not in cmd  # engine default (True) applies

    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.sandbox_exec("sb-1", ["echo", "hi"], sandbox=False)
    cmd = _captured(m)
    assert cmd["sandbox"] is False


def test_pool_claim_layers_always_sent(client):
    with patch.object(client, "_send", return_value={"status": "ok"}) as m:
        client.pool_claim(["base"])
    cmd = _captured(m)
    assert cmd["layers"] == ["base"]
```

**Step 2: Run test to verify it fails**

Run: `python3 -m pytest sdk/python/tests/test_client_protocol.py -v`
Expected: PASS (the client already constructs these correctly) — this is a **characterization test** locking current correct behavior. If any assertion fails, that is a real protocol bug to fix in the same commit.

**Step 3: Commit**

```bash
git add sdk/python/tests/test_client_protocol.py
git commit -m "test(sdk): characterize TerraClient protocol construction"
```

---

### Task 3: Sandbox logic regression tests (VM-existence probe)

**Files:**
- Create: `sdk/python/tests/test_sandbox_logic.py`

**Step 1: Write the failing test**

The gap this guards: the pool↔sandbox integration fixed `Sandbox.__init__` to probe `sandbox_list(tenant)` instead of `vm_info("tenant-<t>")` (pool-backed VMs are named `pool-N`). Without a regression test, a future refactor can silently break the "second Sandbox of a pool-backed tenant needs no template" invariant. Mock the module-level `DaemonManager` and `TerraClient` so no daemon or VM is touched.

```python
"""Sandbox construction logic — daemon and client mocked, no VM."""
from unittest.mock import Mock, patch

import pytest

import terra.sandbox as sb_mod
from terra.sandbox import Sandbox


def _mock_env(existing_records, create_resp):
    client = Mock()
    client.sandbox_list.return_value = {"sandboxes": existing_records, "count": len(existing_records)}
    client.sandbox_create.return_value = create_resp
    dm = Mock()
    with (
        patch.object(sb_mod, "DaemonManager", return_value=dm),
        patch.object(sb_mod, "TerraClient", return_value=client),
    ):
        return client


def test_pool_backed_tenant_reuses_without_template():
    """Second Sandbox of a pool-backed tenant needs no template/layers."""
    client = _mock_env(
        existing_records=[{"id": "sb-aaaabbbb", "vm_name": "pool-0", "pool_backed": True}],
        create_resp={"id": "sb-ccccdddd", "vm": "pool-0", "workdir": "/workdir/sb-ccccdddd", "pool": True},
    )
    sb = Sandbox(tenant="research")  # no template, no layers → must NOT raise
    assert sb.pool_backed is True
    assert sb.vm == "pool-0"
    client.sandbox_create.assert_called_once()
    kwargs = client.sandbox_create.call_args.kwargs
    assert kwargs["pool"] is True


def test_new_tenant_requires_template_or_layers():
    """First Sandbox of a tenant without VM spec must raise."""
    from terra.exceptions import TerraError

    _mock_env(existing_records=[], create_resp={})
    with pytest.raises(TerraError, match="template or layers required"):
        Sandbox(tenant="fresh")


def test_existing_tenant_no_extra_vm_spec_fields():
    """Reuse path must not demand vmspec — and must pass pool=False through."""
    client = _mock_env(
        existing_records=[{"id": "sb-aaaabbbb", "vm_name": "tenant-x", "pool_backed": False}],
        create_resp={"id": "sb-ccccdddd", "vm": "tenant-x", "workdir": "/workdir/sb-ccccdddd", "pool": False},
    )
    sb = Sandbox(tenant="x", pool=False)
    assert sb.pool_backed is False
    assert sb.vm == "tenant-x"
    assert client.sandbox_create.call_args.kwargs["pool"] is False
```

**Step 2: Run test to verify it fails**

Run: `python3 -m pytest sdk/python/tests/test_sandbox_logic.py -v`
Expected: PASS against current code (it contains the fix). **Sanity check the tests are real**: temporarily change `sandbox.py` line ~207 back to the old `vm_info` probe and confirm `test_pool_backed_tenant_reuses_without_template` FAILS, then restore.

**Step 3: Commit**

```bash
git add sdk/python/tests/test_sandbox_logic.py
git commit -m "test(sdk): regression tests for pool-backed tenant VM probe"
```

---

### Task 4: Make CI run the non-e2e Python tests

**Files:**
- Modify: `.github/workflows/ci.yml` (python job)

**Step 1: Update the python job**

Current job only imports terra. Replace the tail of the `python` job with a real test run, excluding KVM tests:

```yaml
  python:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: "3.10"
      - name: Build PyO3 engine
        run: |
          pip install maturin
          cd crates/engine && maturin develop
      - run: pip install -e sdk/python/
      - run: python -c "import terra; print('SDK OK')"
      - name: Run non-e2e Python tests
        run: |
          pip install pytest
          python -m pytest sdk/python/tests -m "not e2e" -v
```

Keep the `import terra` step as a fast failure signal. Do NOT add KVM e2e to CI — no runner has /dev/kvm; the `e2e` marker documents that.

**Step 2: Verify locally with the same commands**

Run: `python3 -m pytest sdk/python/tests -m "not e2e" -v`
Expected: Task 2 + Task 3 tests pass (2 files, ~9 tests), zero collection of e2e tests, no marker warnings.

**Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: run non-e2e Python tests (was import smoke test only)"
```

---

### Task 5: Add cargo-deny license & advisory gate

**Files:**
- Create: `deny.toml`
- Modify: `.github/workflows/ci.yml` (check job)

**Step 1: Write `deny.toml`**

Minimal gate matching the repo's already-declared license posture (Apache-2.0, see THIRD-PARTY):

```toml
[advisories]
yanked = "deny"

[licenses]
allow = [
    "Apache-2.0",
    "MIT",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Unicode-3.0",
    "CC0-1.0",
]
unlicensed = "deny"

[bans]
multiple-versions = "deny"
wildcards = "deny"

[sources]
unknown-registry = "deny"
unknown-git = "deny"
```

**Step 2: Add the CI job step**

In the `check` job, after the existing `cargo audit` step:

```yaml
      # License + dependency policy gate (cargo-deny)
      - uses: EmbarkStudios/cargo-deny-action@v2
```

**Step 3: Verify locally**

Run: `cargo install cargo-deny --locked && cargo deny check` (or `cargo deny --all-features check` if the action complains about features).
Expected: PASS with `multiple-versions` as the likely only finding — if the workspace already has duplicate versions, add the specific crates to `[bans.skip]` with a short comment in `deny.toml` (do NOT relax the rule globally).

**Step 4: Commit**

```bash
git add deny.toml .github/workflows/ci.yml
git commit -m "ci: cargo-deny license/advisory/bans gate"
```

---

### Task 6: Add dependabot for cargo + pip

**Files:**
- Create: `.github/dependabot.yml`

**Step 1: Write the config**

```yaml
version: 2
updates:
  - package-ecosystem: cargo
    directory: "/"
    schedule:
      interval: weekly
    open-pull-requests-limit: 5

  - package-ecosystem: pip
    directory: "/sdk/python"
    schedule:
      interval: weekly
    open-pull-requests-limit: 5
```

Note: `Cargo.lock` is committed, so cargo updates produce lockfile PRs that CI verifies with `--locked`.

**Step 2: Commit**

```bash
git add .github/dependabot.yml
git commit -m "ci: dependabot weekly updates for cargo and pip"
```

---

### Task 7: CHANGELOG and README Roadmap sync

**Files:**
- Create: `CHANGELOG.md`
- Modify: `README.md` (Roadmap section)

**Step 1: Write `CHANGELOG.md`**

Unreleased section capturing what shipped since 0.1.0 (all verified by tests):

```markdown
# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Warm-pool integration with engine sandbox entities: `sandbox_create` claims
  idle pool VMs (millisecond hot start, `pool` flag, `pool_backed` in records);
  `tenant_destroy` releases pooled VMs back to idle instead of destroying.
- `pool_create` readiness probing (guest-agent ping) with honest partial-failure
  reporting; `destroy` cascades to sandbox records.
- MCP session-scoped `terra_exec` (sandboxed by default) plus
  `terra_session_read` / `terra_session_write`; sessions auto-create/reuse on the
  shared `mcp` tenant.
- SDK `Sandbox.pool_backed` property, `pool=` constructor arg, `--no-pool` CLI flag.

### Fixed
- `sandbox_create` resize no longer errors when the pool VM already matches the
  requested cpus/memory (CH rejects no-op resizes).
- SDK VM-existence probe now indexes by tenant sandbox records, so a second
  `Sandbox()` on a pool-backed tenant no longer wrongly demands template/layers.

### Security
- MCP commands are sandboxed (sandlock) by default; previously all MCP execs
  ran unsandboxed.
```

**Step 2: Update README Roadmap**

In `README.md`, the Roadmap section: mark the pool-integration milestone done and reflect reality:

```markdown
## Roadmap

- ✅ CH base, engine daemon, adapter layer, Python SDK
- ✅ virtiofs layered filesystem, warm pool, NAT networking, layer build-by-doing
- ✅ High-level Sandbox / Pool / Template API, exception hierarchy, async support
- ✅ Warm-pool-backed tenant sandboxes (claim on create, release on destroy); MCP session-scoped exec
- 🔲 Pool auto-scaling, snapshot fault tolerance, density benchmarks, observability
```

**Step 3: Commit**

```bash
git add CHANGELOG.md README.md
git commit -m "docs: CHANGELOG + README Roadmap sync (pool↔sandbox, MCP sessions)"
```

---

### Task 8: Release-profile CI build and full verification

**Files:**
- Modify: `.github/workflows/ci.yml` (check job)

**Step 1: Add a release-profile build to the check job**

Debug builds miss release-only failures (overflow checks, inlining, feature flags). Add after `cargo test`:

```yaml
      # Release-profile build (catches release-only failures)
      - run: cargo build --release --all
```

**Step 2: Full local verification**

Run each gate exactly as CI will:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
cargo build --release --all
python3 -m pytest sdk/python/tests -m "not e2e" -v
```

Expected: all pass. Note: `cargo audit`/`cargo deny` run only in CI (network tool).

**Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: release-profile build in check job"
```

---

## Explicitly NOT in scope (YAGNI)

| Item | Why excluded |
|---|---|
| Coverage gates (tarpaulin/llvm-cov) | Metric-for-metric; adds toolchain weight, no failing gate today |
| Metrics/Prometheus endpoint | Product feature — violates the no-new-features principle; `tracing` structured logs already give observability |
| Fuzz targets for guest-proxy | Security depth that needs a dedicated harness; revisit post-1.0 |
| pre-commit hooks / editorconfig | Dev-machine tooling; CI already gates, adds config surface |
| CONTRIBUTING guide | Single-maintainer repo; deferred until external contributors |
| Release workflow (tag → artifacts/crates.io/PyPI) | Version still 0.1.0, no release demand |
| Restoring deleted `docs/plans` ADRs | Deliberately removed in c3a9ea3; git history preserves rationale |
| Pool auto-scaling, snapshot, benchmarks | Product roadmap items, not engineering hygiene |
