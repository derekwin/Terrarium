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
- Background exec sessions end-to-end: SDK `Sandbox.exec(background=True)` returns
  an engine-tracked `Session`; CLI `sandbox exec --detach` + `sandbox session
  status|kill|ls`; MCP `terra_exec_background` / `terra_session_status` /
  `terra_session_kill`; engine prunes terminal session records.
- Capability-based `SandboxPolicy` (default deny, explicit grants) as the single
  policy type across protocol / engine / guest-proxy / SDK, plus the
  `SandboxAdapter` L2 contract (`create`/`exec`/`destroy`) with the guest-sandlock
  default backend.
- Audit observability: per-policy structured tracing events (`audit.exec` /
  `audit.deny` / `audit.resource`) gated by `SandboxPolicy.audit`; VM `resize`
  emits an always-on platform event.
- Structured sandlock deny signal: guest-proxy classifies denials from a
  supervisor-written fd (`SANDBOX_DENY_FD`) instead of sniffing stderr text, and
  static fs grants are mirrored in the supervisor (`fsgrant` patch) so
  default-policy fs denials are auditable.
- Engine enforces the VM quota on sandbox limits at `sandbox_create` and on
  per-call exec overrides; `resize` syncs the recorded quota.
- Pool `grow(n)` adds n (delta) and `scale(target)` reaches an exact target with
  atomic idle-slot `pool_shrink`.
- Blocking execs run outside the global manager lock (concurrent daemon); VM
  liveness reaping is restricted to state-changing commands.
- Density benchmark harness (`manual_density_bench.py`): cold-create latency,
  per-VM RSS/Pss (shared-layer page-cache evidence), in-tenant sandbox cost,
  warm-pool claim/exec, exec latency and concurrent throughput. Real runs
  need a KVM host — methodology in `docs/benchmarks.md`.
- Snapshot fast-reset (P1): `snapshot` captures a VM's memory + fs upper into
  a directory (auto-pause, stays paused); `restore` creates a NEW VM from it
  (restore-only CH invocation — CH v53 restore is unreachable through the
  normal CLI) with the overlay upper seeded from the snapshot. Verified on
  real KVM: ~200 ms restore vs ~850 ms cold boot, deterministic rollback.
- Batch environment orchestration (`terra.batch.Batch`): restore N
  environments from one snapshot, parallel `collect`, deterministic
  `recycle`, host density `report`. VM lifecycle (create/restore) now runs
  lock-free in the daemon (spec resolved under the lock, adapter call
  outside, handle registered after) — 8 concurrent restores measured at
  232 ms on real KVM, with per-restore snapshot config isolation so
  parallel restores cannot race on the shared config.json.
- VM teardown (destroy/shutdown/kill) also runs lock-free (unregister
  under the lock, CH shutdown outside): a 64-env batch recycle dropped
  from 3487 ms to 644 ms. Reset-scaling benchmark
  (`manual_reset_bench.py`): 4→64 envs restore in ~220-470 ms total,
  per-VM memory constant at 62.7 MB.
- Audit observability productized (P2): the engine keeps a bounded
  in-memory audit ring buffer alongside the tracing stream, queryable via
  the `audit_list` protocol command (`limit`/`event`/`sandbox_id` filters),
  SDK `TerraClient.audit_list`, CLI `terra audit ls`, and the MCP
  `terra_audit_list` tool.

### Fixed
- `sandbox_create` resize no longer errors when the pool VM already matches the
  requested cpus/memory (CH rejects no-op resizes).
- SDK VM-existence probe now indexes by tenant sandbox records, so a second
  `Sandbox()` on a pool-backed tenant no longer wrongly demands template/layers.
- Sandbox ids widened to 12 hex with a collision guard; unified VM `unregister`
  removes dangling pool / net / sandbox / session records atomically.
- Deny audits no longer misfire when stderr happens to contain "denied"; the
  reserved `SANDBOX_DENY_EXIT_CODE` is produced only for supervisor-reported
  denials.
- CPU shrink rejected with an explicit error (guest-side offlining absent);
  memory shrink via virtio-mem stays supported.
- Binary-safe `files.download` via a base64 channel.

### Removed
- Dead adapter surface: `VmCapabilities` / `capabilities()`, `pause` / `resume`,
  `is_retryable`, `ExecOpts::with_policy`, `VmSpec.backend_config`; unused CH
  HTTP client methods; unused protocol builders (`with_max_cpus` / `with_system`
  / `with_upper`); write-only `SandboxSpec.tools` / `SandboxSpec.env` and the
  never-called `SandboxHandle::setup`.
- CLI reserved-but-meaningless flags (`sandbox create --disk/--backend/--name`,
  `pool/net` names, `sandbox exec --follow`).

### Security
- MCP commands are sandboxed (sandlock) by default; previously all MCP execs
  ran unsandboxed.
- `sandbox_create` and every exec path validate the executed policy's limits
  against the tenant VM's physical quota (`validate_with_vm`).
