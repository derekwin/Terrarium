# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **RL protocol via MCP (P1 #3)** — five new tools: `terra_pool_create_snapshot`
  (ready-pool fill), `terra_batch_create` / `terra_batch_exec` /
  `terra_batch_recycle` (one-command N-env create / task injection +
  collect / recycle), and `terra_vm_reset` (in-place episode reset). The
  RL episode loop is now three MCP calls. Protocol Command gains
  count/prefix/sandboxes/tenants/mode builders; recycle results report
  released/destroyed/reset accurately.
- **Batch lifecycle API (P1 #2)** — `batch_create(prefix, N)` /
  `batch_exec(ids, args)` / `batch_recycle(tenants, mode)` run N
  environments in one daemon command: prepare under the lock, spawn/bind/
  exec/reset/teardown on a JoinSet outside it, register on re-lock.
  Measured at 16 envs: create 753ms (ready-pool claims + cold fallback),
  exec 13ms, recycle-reset 25ms, recycle-destroy 138ms — the RL episode
  loop is now three calls instead of a client-side N-loop. Protocol
  Command gains count/prefix/sandboxes/tenants/mode fields.
- **`terra cleanup` command + daemon-start orphan warning** — reaps
  cloud-hypervisor/virtiofsd/erofsfuse processes left by a crashed daemon
  and removes stale `/tmp/terra-*` sockets (refuses while a daemon is
  running; self-elevates via sudo). `daemon start` now warns when orphaned
  host processes are detected — fixes the pool-N "API socket already in
  use" collision after a crash (observed repeatedly during development).
- **Unified hot pool (Ready slots)** — `pool_create_snapshot` pre-restores
  VMs from a snapshot with the layered fs attached and the agent running.
  `sandbox_create(pool=True)` prefers a Ready slot whose layer set matches
  exactly: claim is a direct sandbox bind (~9ms vs ~950ms cold / ~617ms
  warm-with-fs-attach). Release resets the VM in place (guest kill +
  episode dirs back to the layer baseline) — verified no data leaks
  between sequential claims. PoolSlot carries a `ready` flag; `pool_list`
  reports it.
- Adversarial isolation suite (`test_adversarial_isolation.py`, 31 real-KVM
  tests) with a static escape probe (`adversarial/probes/escape_probe.c`,
  gcc-built, chunked-upload to the guest): Landlock bypass attempts
  (symlink/hardlink/rename), /proc+/sys+/dev enforcement, network bypass
  attempts (TCP/UDP/sendmsg/AF_UNIX/AF_VSOCK/raw/ping + whitelist
  precision), resource limits (procs/fds/memory), governance-integrity
  attacks (kill supervisor, inherited policy fds, audit forgery), L1
  blast-radius checks (host fs/devices/audit unreachable, sibling tenants
  isolated) and audit-query integrity. Added to `run_e2e.sh`.
- Cross-baseline comparison harness (`compare_baselines.py`): the same
  workload matrix on bare host / docker-default / docker-hardened /
  gVisor(runsc) / Terrarium, plus cold-start latency, steady exec latency
  and per-instance memory; results in
  `docs/baseline-compare-2026-08-13.json`.
- Paper-oriented threat model (`docs/security-threat-model.md`) and
  adversarial-evaluation writeup (`docs/security-adversarial.md`) covering
  the L1/L2 two-layer guarantee model, adversarial assumptions and the
  fixed-findings loop.

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
- Sandlock deny channel + fsgrant gate ACTIVATED in the guest images
  (base/ubuntu layers now carry the fully patched sandlock): verified on
  real KVM — a denied sandboxed read exits with the reserved code 200 and
  produces an `audit.deny` event (reason carries stderr), while allowed
  reads stay exit 0.
- RL episode-loop benchmark (`manual_episode_bench.py`): task injection →
  parallel run → collect → snapshot recycle. Snapshot recycle dominates
  (~94% of episode time); 32 envs sustain ~53 env-episodes/s with
  deterministic resets. In-place guest reset identified as the next
  ~10x lever.
- In-place episode reset (P1/RL fast path): guest-proxy `reset` command
  (kill episode process groups + clear /workdir,/tmp,/run back to the
  layer baseline + dcache invalidation); `reset_vm` protocol command runs
  lock-free. Verified on real KVM: 32-env episode drops from 603 ms
  (snapshot recycle) to ~82 ms (7.4×), deterministic across episodes.
- Environment-layer toolchain (P1/RL): `terra tool create --script` bakes
  the environment READY state into a layer (builder VM → upper delta →
  EROFS), which is exactly what makes the in-place reset correct — episode
  writes live in the upper, ready-state lives in the layer.
  `images/examples/rl-env.sh` is the reference RL example; the layer-backed
  episode flow is verified by `sdk/python/tests/manual_envlayer.sh` (4 envs,
  3 in-place resets: ready-state survives, episode writes cleared).
- `restore` now applies the same implicit system-base stacking as `create`
  (`apply_system_base`): restoring a snapshot whose layers were given as
  `["rl-env"]` composes `["rl-env", "base"]` instead of producing a VM
  without the distro rootfs.
- Asset self-containment: the SDK now bundles the runtime libraries of its
  extracted erofs tools — `libdeflate.so.0` for `mkfs.erofs` and
  `libfuse.so.2` for `erofsfuse` — into the managed bin dir (with the bin
  dir on `LD_LIBRARY_PATH`), so layer builds work on hosts without the
  system packages; apt package lists are refreshed on first download miss.
- Layer-baseline episode benchmark: `manual_episode_bench.py --task rltask`
  runs the LAYER-provided task (`/usr/local/bin/rl-task`) against the
  layer-backed ready state. Real KVM, 32 envs: end-to-end episode drops
  from 604 ms (snapshot recycle) to **34 ms** (in-place reset, 17.6×);
  throughput 53 → 934 env-episodes/s. Reset is no longer the bottleneck
  (~55-60% of episode vs ~96% for recycle) — task execution now dominates.
  Raw data: `docs/benchmark-results-2026-08-04-envlayer-*.json`.
- SWE-bench agent-execution proof (pallets__flask-4160, Decimal JSON
  encoding): a real SWE-bench instance runs end-to-end on Terrarium —
  repo + pinned toolchain + tests baked into an EROFS layer
  (`terra tool create --script`), 4 parallel envs restored from one
  snapshot, the target test FAILS at baseline in every env, the agent's
  fix turns the suite green (33 passed), and `reset_in_place()` restores
  the broken baseline. See `sdk/python/tests/manual_swebench.sh`.
  Verified on the host (sudo daemon), 2026-08-05.
- Guest networking fixes that made ubuntu-layer VMs usable on hosts with
  an internal resolver and docker: dnsmasq now hands out the NAT gateway
  (10.200.0.1) as DNS instead of hardcoded public resolvers; the NAT
  bridge adds explicit FORWARD ACCEPT rules (docker sets FORWARD DROP);
  the ubuntu layer bundles a static busybox (ip/udhcpc) with a bounded
  background DHCP, and mounts devpts (apt needs it).
- Host-side guest-command timeout (`guest_cmd`): handshake/response reads
  are bounded (10s handshake, cmd timeout + 15s response) so a booting or
  absent guest agent fails fast and callers retry instead of hanging the
  daemon forever.
- SDK client `_send` socket timeout scales with the command's declared
  `timeout_secs` (default 180s for lifecycle commands) — long execs were
  previously killed at a fixed 30s.
- Layer remount fix: when a rebuilt `.erofs` image invalidates a shared
  mount (mtime change), the stale registration is dropped so the new
  image is actually mounted — previously the mountpoint came back empty
  and the layer silently vanished from VMs.
- `find_mkfs_erofs` prefers the managed `~/.local/share/terra/bin` path
  (bare `mkfs.erofs` only resolves via PATH); `terra tool create` purges
  apt caches before packing the upperdir (root-owned 0700 dirs broke the
  erofs pack for the invoking non-root user).
- Batch SWE-bench verification (`manual_swebench_batch.py`): 5 flask
  instances (4160/4169/4544/4935/5014, versions 2.0/2.1/2.3) all pass —
  per-instance layer build with build-time bug-reproduction check,
  snapshot, 2 parallel envs, baseline FTP fails -> gold patch -> FTP +
  touched test files pass -> reset restores the baseline. Raw:
  `docs/benchmark-results-2026-08-05-swebench-batch.json`. flask-4992 is
  excluded (needs py3.11+ tomllib; environment requirement, not a
  Terrarium failure).
- `terra tool create` packing: open up anything others can't read or
  traverse under /etc /usr /var /opt /srv /home before packing the
  upperdir (root-owned 0600 files like debconf's passwords.dat also
  broke the non-root erofs pack).
- Agent-CI isolated verification (dogfooding): `ci_verify.py` runs an
  agent's patch in an isolated layer-backed environment — pristine
  snapshot -> copy repo into the per-VM workspace -> git apply the
  patch -> run the test command -> verdict. The `ci-terra` layer bakes
  this repo + SDK test toolchain with a build-time self-check (baseline
  suite passes). Demo: a good patch PASSES (42 passed), a bad patch is
  REJECTED (1 failed), ~1.5s per verification; reset_in_place() returns
  the environment to the pristine baseline between submissions.
  `terra tool create` packing now opens the whole rootfs (minus /root)
  for the non-root erofs pack instead of a fixed directory list.
- RL episode-loop example (`sdk/python/examples/rl_episode_loop.py`): the
  minimal training-loop contract — layer task reads `/workdir/input.json`
  (episode input), writes `/workdir/output.json`, echo result for
  collection; `Batch.reset_in_place()` back to the layer baseline.
  Verified on real KVM: 8 envs × 10 episodes, every episode's injected
  input reflected in its collected result (deterministic data flow),
  ~24 ms/episode steady-state.
  Snapshot restore remains the full-determinism path.

### Fixed
- **daemon dispatch restructured so lock-free handlers cannot re-lock** —
  the reentrant-`Mutex` deadlock recurred four times (sandbox_create,
  tenant_destroy, pool_create_snapshot, batch_*) by re-acquiring the
  top-of-dispatch guard. Dispatch now owns a `LockGuard` (the ONLY lock
  entry point; double-lock PANICS instead of deadlocking) and every
  lock-free command is an extracted handler that receives only
  `&mut LockGuard` — never the raw `&Mutex` — so a stray
  `manager.lock().await` cannot compile. Verified: `manager.lock()`
  appears only inside LockGuard (+ daemon shutdown), handlers use only
  `take()`/`relock()`.
- **pool_create_snapshot now fills in parallel** — the READY-pool fill
  restored VMs sequentially under the manager lock (~300ms each; 16 slots
  ≈ 4.8s). The daemon now builds the specs under the lock, restores +
  pings all VMs in parallel (JoinSet), and registers the slots on re-lock
  (16 slots ≈ 1.5s incl. the one-time tap-pool fill). Also fixed a
  reentrant Mutex deadlock in the new daemon branch (third occurrence of
  the same top-of-dispatch guard re-acquisition; a comment now guards the
  pattern).
- **tenant_destroy held the manager lock across the whole teardown** —
  session kills, sandbox destroys and the pool reset/detach/shutdown are
  vsock/CH round trips that serialized on `Mutex<VmManager>`, capping the
  claim/release churn at ~68 episodes/s (16 slots). The daemon now
  resolves a teardown plan under the lock, runs the slow work outside it
  and finalizes bookkeeping on re-lock (same pattern as the earlier
  sandbox_create lock fix). Measured: 462 episodes/s at 16 slots (~7x),
  pool count stays stable across claim/release cycles.
- **VM launch/restore latency + throughput** — two hot-path fixes:
  (1) the virtiofsd and CH API socket waits polled every 100ms, adding
  ~200ms of pure polling latency to every launch (single restore 225ms →
  32ms with 5ms polls); (2) per-VM `ip tuntap add`/`ip link set master`
  serialize on the kernel RTNL lock, capping net launches at ~274/s (vs
  400/s without networking). A tap pool now pre-creates + pre-attaches
  256 taps once (~3s one-time); launch claims a name with zero kernel
  ops and destroy returns it. Controlled net-restore throughput: 539/s
  at 96 workers (~10x gVisor); the old per-VM tap naming is removed.
- **daemon keep-alive wrapper now logs its exit reason** — before exiting
  after 30 consecutive heartbeat failures it prints a diagnostic line, so
  a future daemon death distinguishes "accept loop starved" from "daemon
  thread crashed". No death reproduced across 4 heavy-load rounds after
  the spawn_blocking/bridge-cache/SDK-guard fixes.
- **net restores failed with tap "Resource busy"** — CH restore re-attaches
  devices from the snapshot's config.json, whose net device still pointed
  at the source VM's tap; `prepare_restore_dir` now rewrites the tap to the
  restored VM's own (as it already did for vsock/fs sockets). Restore is
  now the fast-create path: 100 instances from one snapshot in ~1.9s
  (53.2 VMs/s end-to-end, ~101/s restore+bind at 64-way), ahead of gVisor.
- **daemon accept loop starved by blocking subprocess calls** — every VM
  launch ran `ip`/`ebtables` synchronously on the tokio workers; heavy
  parallel creation starved the accept loop and the SDK silently replaced
  the (merely busy) root daemon with a rootless embedded one. NAT bridge
  setup is now cached per daemon, the network setup runs in
  `spawn_blocking`, the SDK refuses to auto-start an embedded daemon when
  the socket path exists but does not answer, and the keep-alive wrapper
  tolerates 30 consecutive heartbeat failures.
- **sandbox_create serialized all creates on the manager lock** — VM boot
  + agent-ready + workdir ran entirely under `Mutex<VmManager>`, so
  concurrent clients queued ~1s each (1.05 VMs/s with 8 workers) while
  `vm_create` already spawned lock-free (18.8 VMs/s). `sandbox_create` is
  now split into prepare (lock: validate/resolve/quota/id) + bind
  (lock-free: spawn, L2 session, workdir) + finish (lock: register +
  audit); measured 7.0 VMs/s at 100 instances (8 workers), ahead of
  docker's 3.5/s. Also fixed a tokio Mutex re-entrancy deadlock found by
  the daemon lock tests.
- **tap name collision broke same-prefix tenant bursts** — `tap_name`
  truncated VM names to 9 chars, so tenants sharing a prefix (e.g.
  `tenant-dens-0..7`) all got one tap; the second and later VMs failed to
  boot with "Resource busy". Tap names now embed a 16-bit FNV-1a digest
  of the full name (`<4 chars>-<hex>`) for uniqueness within the 15-char
  ifname budget.
- **daemon keep-alive wrapper killed the service under load** — the
  `daemon start` heartbeat exited on ANY transient connect failure (a
  burst of concurrent creates could momentarily refuse it), taking the
  root daemon down; SDK then auto-started a rootless embedded daemon that
  cannot do NAT. The wrapper now exits only after 8 consecutive failures.
- **ebtables isolation rule was never idempotent** — `ebtables -C` does
  not match rules added with interface wildcards (`-i terra-+`), so every
  net-enabled VM appended a duplicate DROP rule and the FORWARD chain grew
  without bound (545 copies observed). `ensure_vm_isolation` now probes
  the table listing; 3 net VMs → exactly 1 rule.
- **dnsmasq lease exhaustion under VM churn** — the 12h lease time filled
  the 151-address pool after ~150 create/destroy cycles and new guests
  booted without eth0 ("no address available"). Lease time is now 10m so
  destroyed VMs' addresses return to the pool quickly.
- **confine: seccomp listener handoff race** — the listener fd is
  close-on-exec, so a fast-exec'ing confined command could outrun the
  parent's `pidfd_getfd` and leave the filter installed with no listener
  (network syscalls silently failed ENOSYS with no deny audit). The child
  now blocks on a go-pipe until the parent duplicated the listener fd;
  handoff failure kills the child (fail closed, never an unattended
  filter).
- **confine: supervisor killable by the confined process** — the agent
  shares the supervisor's uid (guest root); `kill -9 $PPID` could remove
  governance. The BPF now traps kill/tgkill/tkill and the supervisor
  denies signals aimed at itself, init (pid 1) or its process group.
- **confine: memory.max unit bug** — `limits.memory_mb` was written as raw
  bytes to cgroup v2 `memory.max`, capping a 64 MB limit at 64 bytes and
  OOM-killing every process in the cgroup. Now converted to bytes.
- **fd hygiene across the agent channel** — guest-proxy's vsock sockets
  and the deny-pipe read end leaked into the confined process (fd 63 let
  the sandbox forge audit records; channel sockets were reachable). vsock
  fds are now CLOEXEC, the pre-exec closes the deny-pipe originals, and
  the confined child closes fd 63 and forces CLOEXEC on the listener.
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
