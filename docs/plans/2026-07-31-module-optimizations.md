# Module-Level Optimization Proposals

> Review of the full codebase (9 Rust crates + Python SDK) against the principle:
> **keep core functionality correct, elegant, and simple — introduce no redundancy.**
> Every proposal below is a removal, deduplication, or consistency fix — zero new features.

**Date:** 2026-07-31
**Basis:** three structural scans (engine; adapter+fs+network; protocol+guest-proxy+mcp+sdk) with file:line evidence; `cargo check`/`clippy` clean at review time.

---

## Priority tiers

| Tier | Scope | Risk | Effort |
|---|---|---|---|
| **P0 — dead code & unused deps** | pure removal, zero behavior change | trivial | <1 day |
| **P1 — duplication & consistency** | dedup + state-consistency bug fixes | low–medium | 1–2 days |
| **P2 — structural** | the global-lock and God-object issues | medium | 2–4 days, staged |

Explicitly **NOT proposed** (YAGNI / product scope): metrics endpoints, fuzz harnesses, wiring the standalone `adapter-sandlock` into the engine (guest-side sandlock is the correct current design), extending `async_sandbox`, splitting the CLI file.

---

## P0 — Dead code and unused dependencies (pure removal)

### P0-1. adapter/traits: remove never-called trait surface
Evidence: engine never calls these; only the CH impl and tests touch them.
- `VmCapabilities` + `VmAdapter::capabilities()` (traits:162, 387) — doc at traits:403 claims "the engine checks capabilities() first" but nothing does. ChAdapter returns all-true (CH lib.rs:41-52). Remove type + method + impl.
- `pause`/`resume` (traits:455-459) — required methods with zero production callers. Make them **defaulted to `NotSupported`** (like `exec`) instead of required, so future backends aren't forced to implement what nothing calls.
- `AdapterError::is_retryable` (traits:49) — zero callers.
- `ExecOpts::with_policy` (traits:312) — zero callers; engine assigns `opts.policy` directly (manager.rs:161, 447).
- `VmSpec.backend_config` (traits:157) — always `None` (mod.rs:136, manager.rs:196), never read. Remove field + plumbing.

### P0-2. CH adapter: remove dead client methods
- `client.rs:202 vm_balloon` + `api.rs:89-90 ResizeConfig.balloon_size` — zero callers anywhere (not even tests).
- `client.rs:145 vm_create`, `:152 vm_boot`, `:162 vm_delete` + `api.rs` types `VmConfig`/`PayloadConfig`/`ConsoleConfig`/`DiskConfig` — production spawns CH as a CLI subprocess (process.rs `ch_args`); these HTTP paths are test-only (`tests/mock.rs`). Remove methods and their schema types; keep `VmDetails`/`VmInfoConfig`/`CpusConfig`/`MemoryConfig`/`ResizeConfig` (production via `vm_info`/`vm_resize`).

### P0-3. Unused dependencies (Cargo.toml)
- `engine`: `serde` (workspace) — never referenced in src (only `serde_json`).
- `fs`: `regex`, `serde`, `libc`, `tokio` — zero source references (the `libc` hits are comments about filenames, cpio.rs:124,246).
- `network`: `adapter-traits` — zero source references.
Removal is a 3-line Cargo.toml change per crate; verify with `cargo check --workspace`.

### P0-4. Engine protocol surface honesty
- `commands/snapshot.rs:19-21` `restore` stub — keep (an explicit "not implemented" error is honest), but **remove the `"restore"` arm's false promise**: the command is already documented as not-implemented in protocol.md; no code change needed. Leave as-is; listed for awareness only.

---

## P1 — Duplication and consistency (low–medium risk)

### P1-1. Engine: unify exec / sandbox_exec (biggest dedup win)
Evidence: `cmd_sandbox_exec` (sandbox.rs:188-263) vs `cmd_exec` (exec.rs:4-63) share: timeout clamp (200/12), policy-requires-sandbox gate (204/14), validate_policy (207/17), blocking/background dispatch (214/24), response shapes. ~45 duplicated lines; the only deltas are workdir, stored-policy inheritance, and background linkage.
**Proposal:** extract a shared private helper `run_exec(mgr, name_or_id, args, timeout, mode, sandbox_default, policy, work_dir)` in `commands/`, and have both handlers call it. Do **not** merge the two protocol commands (they have different defaults and semantics — exec is VM-scoped unsandboxed-by-default, sandbox_exec is sandbox-scoped sandboxed-by-default).
**Also fix the asymmetric defaults deliberately:** keep `exec.sandbox=false` / `sandbox_exec.sandbox=true` but document the rationale at both call sites (currently only implied).

### P1-2. Engine: state-consistency fixes (correctness, not refactor)
Evidence (manager.rs):
- `shutdown` (331-337) / `kill` (341-346) remove from `vms` only — a pool-N or net VM killed this way leaves a stale pool slot / net count. `destroy` (351-360) cleans all three; `reap_dead` (374-397) cleans `net_vms`+`pool` but **not `sandboxes`**; `destroy` cleans sandboxes but **not sessions**.
**Proposal:** extract `fn unregister(&mut self, name) -> Option<Arc<dyn VmHandle>>` that removes from `vms` + `net_vms` + `pool` + `sandboxes` + in-flight `sessions` atomically; make `shutdown`/`kill`/`destroy`/`reap_dead` all call it. This is the single highest-value correctness fix — it eliminates the dangling-record class of bugs (stale pool slot, blocked `net_down`, orphaned sandbox/session records).

### P1-3. Engine: sandbox id collision guard
Evidence: `new_sandbox_id` (sandbox.rs:13-15) = `sb-<8hex>` (32 bits); `sandbox_insert` is a bare `HashMap::insert` (manager.rs:526-528) — a collision silently **overwrites** an existing record (birthday ~65k).
**Proposal:** loop until `insert` reports a fresh key (or widen to 12 hex). Two lines; removes a silent-data-loss path.

### P1-4. Engine: `"Missing 'name' field"` ×6 + `"base"` default ×3
Evidence: mod.rs:28,106; vm.rs:81,92,103; pool.rs:82; exec.rs:7 hand-roll field extraction. `"base"` default lives in mod.rs:60, sandbox.rs:60, SYSTEM_BASES.
**Proposal:** add `Command::require_name(&self) -> Result<String, Response>` (or a `missing(field)` helper) and a shared `DEFAULT_SYSTEM: &str = "base"` const; replace the 6 hand-rolled checks.

### P1-5. fs crate: internal dedup
Evidence:
- `build_initramfs_agent` (cpio.rs:78-154) vs `build_initramfs_virtiofs` (162-222): ~80% identical (mkdirs, busybox copy, musl libs, chmod, pack, cleanup); only symlink sets + one subdir differ.
- `pack_cpio_rootfs` (17-45) vs `pack_work_dir` (254-278): same `sh -c` pipeline + tmp/rename.
**Proposal:** extract `build_initramfs(kind: InitramfsKind)` and have `pack_cpio_rootfs` call `pack_work_dir`. Pure refactor, covered by the SDK's images.py e2e.
**Also:** `SYSTEM_LAYER_NAMES` (layer.rs:30) vs engine `SYSTEM_BASES` (mod.rs:37) — pick one owner (fs crate) and have the engine import it, or accept the duplication with a cross-reference comment. Given engine can't depend on fs (layering), a comment cross-reference is the pragmatic fix.

### P1-6. Python SDK: collapse the 4× `_SYSTEM_MAP`
Evidence: sandbox.py:51, pool.py:44, template.py:206 (inline `system_map`), __main__.py:528 (`_DISTRO_SYSTEM_LAYER`).
**Proposal:** define once in `terra/sandbox.py` (or a tiny `terra/_labels.py`), import everywhere else. Zero behavior change.

### P1-7. Python SDK: unify the two `TerraError` classes + daemon lifecycle
Evidence:
- `terra.client.TerraError` (client.py:12) vs `terra.exceptions.TerraError` (exceptions.py:35) — two distinct base classes both exported publicly; `__init__.py` exports the client one, tests import the exceptions one. `sandbox.py` imports both (client as `ClientError`).
- Daemon lifecycle duplicated: `Daemon` (daemon.py, user-facing) vs `DaemonManager` (_engine.py, auto-start) — `_start_daemon`/`_wait_ready`/`_fix_socket_owner` duplicate `Daemon.start()`; `EMBEDDED_STOP_REFUSAL` string duplicated (daemon.py:28, _engine.py:74).
- `exceptions.BuildError`/`ResourceError`/`EngineError` never raised anywhere; daemon/_engine/assets/images each define their own `RuntimeError` subclass outside the hierarchy.
**Proposal:** make `client.TerraError` alias the exceptions hierarchy root (one class), delete the never-raised exception subclasses, and have `DaemonManager` delegate to `Daemon` for start/stop. Medium-touch, high-value for a library that's an SDK.

### P1-8. Python SDK: `Pool.acquire()` bypasses the constructor
Evidence: pool.py:134-147 builds a `Sandbox` via `__new__` + 14 manual field sets, creating the `_from_pool` legacy branch in sandbox.py (id fallback 314, exec 459-484, kill 543-546, policy 338-343).
**Proposal (defer or do carefully):** either (a) add a documented classmethod `Sandbox._from_claimed_vm(...)` that `Pool.acquire` calls, centralizing state construction; or (b) keep as-is and instead **delete the legacy `_from_pool` branch** by making pool-claimed sandboxes real engine sandboxes (pool claim → `sandbox_create` on the pool VM). Option (b) is a behavior change (denser, engine-tracked) — only if you want pool sessions to become first-class engine sandboxes. Recommend (a) now, (b) as a separate design decision.

### P1-9. MCP: stale session registry after engine restart
Evidence: `SessionRegistry` (tools.rs:17) caches sandbox ids for the process lifetime; after a daemon restart the ids are dead and every exec fails "Sandbox not found" until the MCP process restarts.
**Proposal:** in `ensure_session`, on `sandbox_exec` failure containing "not found", drop the cached id once and retry creation (self-heal). ~5 lines, removes a permanent-failure failure mode.

### P1-10. MCP/CLI: docs drift
Evidence: README.md:67 "13 user-facing tools" vs actual 15; docs/mcp.md already lists 15.
**Proposal:** README "13" → "15". One word.

---

## P2 — Structural (staged, highest value but needs care)

### P2-1. Engine daemon: don't hold the global lock across blocking exec
Evidence: daemon.rs:155-157 locks `Mutex<VmManager>` for the whole `execute().await`; a blocking exec (up to 3600s) serializes every other command (session_status, pool_claim, sandbox_list all queue).
**Proposal:** in `manager.exec`/`sandbox_exec`'s blocking path, clone the `Arc<dyn VmHandle>` under the lock, drop the lock, `handle.exec().await` outside, then re-lock to write results. The handle is already `Arc`; only the registry mutation needs the lock. This is the one change that turns the daemon from serialized to concurrent. Must re-verify: session tracking (background) already runs outside the lock (spawned task holds `sessions` Arc), so the pattern is proven — just extend it to blocking exec.

### P2-2. manager.rs: split the 5-responsibility God object
Evidence: manager.rs (542 LOC) = VM lifecycle + warm pool + sessions + sandboxes + readiness probe. Clean adapter-traits-only coupling makes this a safe extraction.
**Proposal:** extract `PoolRegistry` (pool slots + claim/release + readiness) and `SessionRegistry` (background sessions) as private submodules with the same methods, keeping `VmManager` as the facade. Purely organizational; the tests (manager_tests, pool_sandbox_tests) are the safety net. Do this **after** P2-1 (lock change alters the same file).

### P2-3. Reap efficiency: don't scan all VMs per command
Evidence: daemon.rs:156 calls `reap_dead()` on every command (O(n) `Arc::get_mut` per request).
**Proposal:** reap on a timer (e.g. every 5s) or only on commands that touch VM state (create/destroy/pool ops), not on `list`/`info`/`exec`. Low-risk, measurable under load.

---

## Verification strategy for each tier

- **P0:** `cargo check --workspace` + `cargo test --workspace` after each removal; the adapter's own tests/mock.rs will surface any missed reference.
- **P1:** existing 124 Rust tests + 8 Python non-e2e tests are the safety net; P1-1/P1-2/P1-4 additionally need a focused test for the extracted helper and `unregister` (sandbox-record cascade already covered by pool_sandbox_tests).
- **P2-1:** the critical one — a concurrency test (two concurrent execs + a session_status mid-exec) before/after.
- All tiers: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo deny check`, `pytest -m "not e2e"` (CI gates now exist and run).

## Suggested order

1. P0-1 → P0-2 → P0-3 (one afternoon, pure deletion, zero risk)
2. P1-2 (state consistency — correctness) → P1-3 → P1-4 → P1-1 (dedup)
3. P1-6 → P1-7 → P1-10 (Python hygiene)
4. P1-5 → P1-9 (fs dedup, MCP self-heal)
5. P2-1 (concurrency) → P2-3 (reap) → P2-2 (split, last — after the file settles)
