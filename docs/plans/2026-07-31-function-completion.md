# 功能完整性完善方案(Function Completion Plan)

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close the ten functional gaps identified in the audit (`docs/analysis/2026-07-31-functional-design.md`), each resolved per sound engineering judgment — implement what the product needs, fix what is broken, remove what is meaningless, and explicitly document what is deliberately deferred.

**Architecture:** No new subsystems. Every task is either a small fix, a client-surface completion over existing protocol/engine support, a removal of dead/misleading surface, or a documentation alignment. The engine and protocol cores are treated as correct and stable.

**Governing principle:** For each gap, the *correct* engineering answer is not "implement everything" but "do the right thing for its actual role" — some gaps deserve implementation, some deserve removal (a half-wired API is a liability), some deserve an explicit unsupported error, and some are already handled and only need documentation.

---

## Per-gap design analysis (the "what is correct" for each)

### G1. `restore` stub + snapshot half-wired
**Analysis:** `restore` is stubbed at all three layers (engine command, adapter, CH client); `snapshot` works (CH memory snapshot) but has no client and hardcodes paths. Full restore is a large effort (CH restore API wrapping, VM rebuild-from-snapshot, guest bring-up) and the Roadmap lists "snapshot fault tolerance" as a *future* item — there is no near-term product demand.
**Correct design: defer restore explicitly, harden the snapshot side.** A half-promise ("snapshot works") without restore is worse than a documented experimental feature. The right move is (a) mark snapshot experimental in protocol/sdk docs, (b) confirm snapshot artifacts are lifecycle-managed (destroy cleanup exists — verify `.mem` sibling), (c) do NOT add client surface for a feature whose restore half is missing.
**Decision: DOCUMENT + VERIFY, no feature work.** Deliverable: protocol.md/sdk.md snapshot row marked experimental with restore-not-implemented; verify+fix artifact cleanup if `.mem` leaks.

### G2. Background exec "start but cannot manage"
**Analysis:** Engine + protocol + guest-proxy fully implement background sessions (`session_status/kill/list`). The SDK's raw client can *start* one (`exec_mode="background"`) but no client can query/kill/list it — "只开不管" (start-only). Background execution is the core agent long-running-task pattern.
**Correct design: complete the client surface** (the engine is done; only clients lag): SDK client methods + high-level `Sandbox.exec(background=True)` returning a `Session` handle; CLI `sandbox exec --detach` + `sandbox session status/kill/ls`; MCP optional (agent entry point benefits — long tasks). This is surface completion over working machinery, not new engine work.
**Decision: IMPLEMENT** (largest task, split into SDK / CLI / MCP steps).

### G3. `Pool.grow` / `pool scale` over-provisioning
**Analysis:** `grow(n)` passes the post-increment total to `pool_create` (which spawns exactly `size` new VMs), so a pool of 3 + `grow(1)` spawns 4 new VMs (total 7). CLI `pool scale` has the same bug and also drops kernel/net config. The docstring promises "add *count* more".
**Correct design: separate the two intents.** `grow(by=n)` = add n (delta); `scale(target)` = reach exactly target (grows or destroys idle slots). Engine needs a `pool_shrink`-capable path (destroy surplus *idle* slots; never claimed ones).
**Decision: IMPLEMENT** — fix delta semantics + add scale-to-target with idle-slot destruction.

### G4. `net_allow` untested
**Analysis:** Egress ACL enforcement lives in the external sandlock binary (seccomp engine); the repo passes flags through. An e2e test exists (`test_net_allow_live_egress` in test_sandbox.py) but is **skipped** without a root daemon + NAT. This is a test-environment gap, not an implementation gap.
**Correct design: make the verification runnable and documented**, not write new code. The test already asserts allowed-host-passes / unlisted-host-denied. Add a documented manual/e2e run path (root daemon + NAT) and CI note (self-hosted runner).
**Decision: DOCUMENT + make test executable**, no product code.

### G5. CPU shrink without guest offlining
**Analysis:** guest-proxy only *onlines* hot-added vCPUs; nothing *offlines* for shrink. CH vCPU removal requires guest-side offlining. CPU shrink is rare in production and half-supporting it (shrink "works" sometimes) is worse than an explicit error.
**Correct design: restrict with an honest error.** Engine `resize` rejects `cpus` lower than current with "CPU shrink not supported" (memory shrink via virtio-mem stays — the guest driver handles it). Document the limitation.
**Decision: IMPLEMENT restriction + document.**

### G6. AsyncSandbox surface mismatch
**Analysis:** AsyncSandbox wraps most of Sandbox but omits `vm/tenant/policy/pool_backed` properties and `destroy_tenant`.
**Correct design: complete the thin wrapper** (pure delegation, no logic) so the async surface matches sync.
**Decision: IMPLEMENT** (low risk).

### G7. CLI reserved flags
**Analysis:** 8 flags are parsed but unused: `sandbox create --name/--disk/--backend`, `sandbox exec --detach/--follow`, `pool create --name`, `pool claim/scale` positional names, `net create --name`. Each has a distinct fate: some are meaningless in this architecture (no disk concept in virtiofs; pool/net have no names server-side), some are the CLI half of G2.
**Correct design per flag:**
- `sandbox create --disk` / `--backend` → **remove** (no virtiofs disk; single backend auto).
- `sandbox create --name` → **remove** (engine allocates ids; no server-side names).
- `pool create --name`, `pool claim <name>`, `pool scale <name>`, `net create --name` → **remove** (no names server-side).
- `sandbox exec --detach` → **implement** (background exec, ties into G2).
- `sandbox exec --follow` → **remove** (replaced by `sandbox session status` polling in G2).
**Decision: REMOVE meaningless flags + IMPLEMENT --detach.**

### G8. `files.download` binary corruption
**Analysis:** `download()` opens the local file in text mode (`open(local, "w")`) — binary content corrupts (newline translation on some platforms; more importantly bytes pass through str). `write`/`upload` are already base64-safe.
**Correct design: binary mode + round-trip test.**
**Decision: IMPLEMENT** (one line + test).

### G9. guest-proxy dead unix socket
**Analysis:** `/tmp/sandboxd.sock` is bound but nothing in the repo connects to it (vsock is the only host path).
**Correct design: remove the dead transport** (YAGNI; vsock is the single supported channel).
**Decision: REMOVE.**

### G10. Docs drift
**Analysis:** `assets.ensure_engine()/publish_engine()` don't exist; `terra.configure` not exported (only `terra.direct.configure`); `ExecResult.duration_ms` doesn't exist; `env` docstring is stale (env works via shell prefix).
**Correct design: align docs with implementation** — remove non-existent references, fix the env docstring, keep `terra.direct.configure` documented as such.
**Decision: DOCUMENT ALIGNMENT** (no code change except possibly exporting configure — decide: export it, it's one line and matches the doc intent).

---

## Implementation plan

**Phase 1 — small fixes & removals (low risk, ~half day):**
T1 (G8) download binary, T2 (G9) remove unix socket, T3 (G7) remove meaningless CLI flags, T4 (G6) AsyncSandbox completion, T5 (G10) doc alignment (+ export configure).

**Phase 2 — engine behavior (medium):**
T6 (G5) CPU-shrink explicit error, T7 (G3) pool grow-delta + scale-to-target.

**Phase 3 — background sessions (largest):**
T8 (G2) SDK client session methods + Sandbox.exec background + Session handle, T9 (G2) CLI `sandbox exec --detach` + `sandbox session` subcommands, T10 (G2) MCP background tool (optional but recommended — agent long tasks).

**Phase 4 — documentation & verification env (G1, G4):**
T11 (G1) snapshot experimental docs + artifact cleanup verify, T12 (G4) net_allow e2e runnability docs/CI note.

### Task T1: `files.download` binary-safe
- Modify: `sdk/python/terra/sandbox.py:95-100` — `open(local_path, "wb")` (and read path already str — download returns bytes? No: read() returns str; download writes to file — change write mode only).
- Test: add non-e2e test using a real Sandbox is e2e… so test the file-open logic indirectly is hard; add a focused unit test if the download body can be extracted, else verify by e2e (existing suite). Mark: low-risk one-liner, covered by existing e2e files tests.
- Commit: `fix(sdk): binary-safe files.download (wb mode)`

### Task T2: remove guest-proxy unix socket
- Modify: `crates/guest-proxy/src/main.rs` — remove the `/tmp/sandboxd.sock` bind/accept branch and its constants; keep vsock path only.
- Verify: `cargo test -p guest-proxy` (check crate has tests), `cargo test --workspace`, clippy, fmt.
- Commit: `refactor(guest-proxy): drop unused unix socket transport (vsock only)`

### Task T3: remove meaningless CLI flags
- Modify: `sdk/python/terra/__main__.py` — remove `sandbox create --disk/--backend/--name`, `pool create --name`, `pool claim <name>`, `pool scale <name>`, `net create --name`, `sandbox exec --follow` parsers and handler references (T9 re-adds --detach differently).
- Also update any handler that reads removed args; docs/sdk.md CLI reference sections updated.
- Verify: CLI smoke (`terra sandbox create -h`), py_compile, non-e2e pytest.
- Commit: `refactor(cli): remove meaningless reserved flags (--disk/--backend/--name/pool net names)`

### Task T4: AsyncSandbox completion
- Modify: `sdk/python/terra/async_sandbox.py` — add `vm`, `tenant`, `policy`, `pool_backed` properties (run sync call in executor or read cached fields) and `destroy_tenant` classmethod (delegating to sync; note: classmethod is not async — provide `async def destroy_tenant(cls, tenant_id)` wrapper calling the sync one in executor).
- Verify: py_compile, non-e2e pytest, a small async smoke test.
- Commit: `feat(sdk): complete AsyncSandbox surface (vm/tenant/policy/pool_backed/destroy_tenant)`

### Task T5: docs alignment
- Modify: `docs/sdk.md` — remove `assets.ensure_engine()/publish_engine()`, fix `terra.configure` → `terra.direct.configure` (or export configure in `__init__.py` — prefer export, one line, matches doc), remove `duration_ms`, fix env description.
- Modify: `sdk/python/terra/__init__.py` — export `configure` if choosing that path.
- Verify: grep for the removed symbols → zero hits in docs; import smoke.
- Commit: `docs(sdk): align with implementation (configure export, remove ghost APIs)`

### Task T6: CPU shrink explicit error
- Modify: `crates/engine/src/commands/vm.rs` cmd_resize — before calling resize, if `cpus` present and smaller than current (via handle.info()) → `Response::err("CPU shrink not supported")` (memory shrink still allowed). Also the sandbox resize path (`ensure_tenant_vm`) only resizes up (already no-op filtering) — confirm.
- Test: engine test in command_tests.rs — resize to fewer cpus returns the explicit error; memory shrink still attempts.
- Verify: workspace tests, clippy, fmt.
- Commit: `feat(engine): reject CPU shrink with explicit error (memory shrink via virtio-mem stays)`

### Task T7: pool grow-delta + scale-to-target
- Modify: `sdk/python/terra/pool.py` grow — `pool_create(count)` not `pool_create(self._size)`; add `scale(target)` that grows to target or destroys surplus **idle** slots (via `pool_release`? no — destroy idle VM slots; engine needs a helper or reuse destroy on idle slot names from pool_list).
- Modify (engine if needed): `manager/pool.rs` — expose a way to drop idle slots (destroy VM + remove slot) OR do it client-side via `vm_destroy` on idle names (check: engine `destroy` already cleans pool slots via unregister — so client-side `vm_destroy(name)` on idle slots suffices! verify).
- Test: non-e2e mock test for grow delta and scale shrink (mock pool_list/claim/destroy).
- Commit: `fix(sdk): Pool.grow adds delta; add Pool.scale to exact target`

### Task T8: SDK background session client
- Modify: `sdk/python/terra/client.py` — add `session_status(session_id)`, `session_kill(session_id)`, `session_list()` methods.
- Modify: `sdk/python/terra/sandbox.py` — `Sandbox.exec(..., background=False)`: when True, call sandbox_exec with exec_mode="background" and return a `Session` dataclass (session_id, sandbox_id, vm); `Sandbox.session(session_id)` accessor? Keep minimal: return a `Session` object with `.status()/.kill()`.
- New: `sdk/python/terra/session.py` (small `Session` class) or inline dataclass.
- Test: protocol mock tests (client methods build correct commands) + non-e2e logic tests.
- Commit: `feat(sdk): background exec sessions — client methods + Sandbox.exec(background=True)`

### Task T9: CLI background sessions
- Modify: `sdk/python/terra/__main__.py` — `sandbox exec --detach` now real (calls sandbox_exec background, prints session_id); add `sandbox session status <id>` / `sandbox session kill <id>` / `sandbox session ls`.
- Verify: CLI help + mock test.
- Commit: `feat(cli): background exec (--detach) and sandbox session status/kill/ls`

### Task T10: MCP background tool
- Modify: `crates/mcp/src/tools.rs` — add `terra_exec_background` (or `session` param) mapping to sandbox_exec exec_mode=background returning session_id; add `terra_session_status`/`terra_session_kill` tools. Consider: keep it minimal — one background tool + status/kill.
- Test: mcp unit tests (MockEngine pattern exists).
- Commit: `feat(mcp): background exec + session status/kill tools`

### Task T11: snapshot experimental docs + artifact verify
- Modify: `docs/protocol.md` (snapshot row marked experimental, restore not implemented, paths), `docs/sdk.md` (no snapshot client — add a note).
- Verify: `crates/engine/src/commands/vm.rs` destroy cleanup removes `terra-snap-{name}.bin` AND `.mem` — if `.mem` sibling not cleaned, fix.
- Commit: `docs: snapshot marked experimental (restore deferred); verify artifact cleanup`

### Task T12: net_allow e2e runnability
- Modify: `sdk/python/tests/test_sandbox.py` — ensure `test_net_allow_live_egress` skip reason is actionable ("requires root daemon + NAT — run manually or on self-hosted runner"); add a short comment doc in CI notes (no CI change — CI has no KVM).
- Maybe: `docs/` note on how to run the e2e egress test.
- Commit: `test(sdk): document net_allow egress e2e run conditions`

---

## Order & dependencies

Phase 1 (T1-T5) independent, do first (unblocks nothing, low risk). Phase 2 (T6, T7) independent of Phase 3. T9 depends on T8 (same surface); T10 independent but shares protocol understanding. T11/T12 are docs — anytime, prefer after code settles.

Suggested execution: Phase 1 → Phase 2 → Phase 3 (T8, T9, T10) → Phase 4. Each task is an atomic commit, tested green, per the established subagent-driven review loop.

## Explicitly NOT in scope (decisions, not omissions)

- **G1 restore implementation** — deferred deliberately (no product demand; Roadmap item; large effort). Documented experimental.
- **G5 CPU shrink** — restricted with explicit error, not implemented (rare in production; requires guest offlining machinery).
- **G4** — no new code; the enforcement lives in the external sandlock binary and the e2e test exists but needs a NAT environment to run.
- Pool auto-scaling, snapshot fault tolerance, density benchmarks — Roadmap items, out of scope for completion.
