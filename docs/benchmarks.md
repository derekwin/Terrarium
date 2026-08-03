# Density Benchmarks

> **Status: harness ready, numbers pending.** The benchmark script is
> committed and import/compile-verified. First real measurements exist
> (2026-08-03, below); they require a KVM-capable host with guest assets
> (`terra setup <distro>`) and user/mount-namespace privileges for
> virtiofsd. The script refuses to run (exit 2) when KVM is absent rather
> than reporting fake numbers.

## Results — 2026-08-03 (12-tenant sweeps)

Host: 128-core x86-64 with nested KVM; Terrarium inside a **privileged
Docker container** on that host (the containerized workspace blocks
user/mount namespaces otherwise); guest assets rebuilt with the current
guest-proxy. Two sweeps of 12 tenants each — `base` (20 MB layer) and
`ubuntu` (99 MB layer). Raw JSON:
`docs/benchmark-results-2026-08-03-{base,ubuntu}-12.json`.

| Metric | base-12 | ubuntu-12 |
|---|---|---|
| `cold_create_ms` (p50 / p95) | 848 / 869 ms | 850 / 872 ms |
| `sandbox_in_tenant_ms` (p50) | 17.7 ms | 19.8 ms |
| `warm_claim_ms` / `warm_exec_ms` | 617 / 12.6 ms | 627 / 15.9 ms |
| `exec_ms` (p50 / p95) | 5.1 / 7.0 ms | 4.7 / 7.6 ms |
| `execs_per_sec` (16 concurrent) | 344 | 330 |
| per-VM RSS / Pss (12 tenants) | 63.6 / 52.5 MB | 65.3 / 51.7 MB |
| shared memory (12 tenants) | **134 MB (17.6%)** | **164 MB (20.9%)** |

## Quantified sharing benefit

The measurement includes the CH VMM process *and* its virtiofsd (the
composed-layer server); Pss counts shared pages once across all VMs, RSS
per-process. Findings:

- **Per-VM host cost is almost layer-size-independent**: a 99 MB ubuntu
  layer costs ~65 MB/VM vs ~64 MB/VM for the 20 MB base — the 79 MB extra
  environment adds ~2 MB per VM. Layer bytes are stored once and shared;
  VM cost is dominated by the fixed boot working set, not the layer.
- **Shared fraction grows slightly with layer size**: 17.6% of per-VM RSS
  is shared pages for base, 20.9% for ubuntu. At 12 VMs that is 134 MB
  (base) / 164 MB (ubuntu) of host memory not duplicated across VMs.
- **Marginal cost is stable**: incremental per-VM Pss holds at ~52 MB
  (base) / ~52 MB (ubuntu) from VM 1 to VM 12 — no per-VM degradation as
  the host fills.
- The shared-file cache itself lives in the kernel page cache behind
  virtiofsd, so real-world sharing grows with how much of the layer the
  workload actually reads; these numbers reflect the boot+echo working
  set.

## Known limit: guest-proxy concurrency

The guest runs **1 vCPU by default**, and guest-proxy serves each vsock
connection on its own thread. At 32 concurrent execs on one VM the guest
becomes CPU-bound and starts dropping connections (vsock handshake
failures); 16 concurrent execs are clean (~330-344 execs/s). The
benchmark retries transient handshake failures (as real clients would)
and defaults to 16 concurrent execs.

### Environment lessons (why the first runs failed)

- The engine's virtiofsd supervisor needs `unshare -Urm`; Ubuntu's
  `kernel.apparmor_restrict_unprivileged_userns=1` (or a container without
  CAP_SYS_ADMIN) blocks it — run in a privileged container or on a host
  with the sysctl set to 0 (the engine now reports this specifically).
- Guest assets must be rebuilt with the **current** guest-proxy before
  running: `terra setup <distro>` refreshes `layers/<system>/bin/guest-proxy`,
  and initramfs rebuilds need `cpio` on the build host (a missing `cpio`
  silently produced empty initramfs and a "vsock handshake rejected"
  symptom).

## What is measured

Terrarium's density claim is: real VM isolation per tenant at high
single-node density, because read-only EROFS layers are **shared across
VMs via the host page cache**. The benchmark turns that claim into
numbers on the host side:

| Metric | Meaning |
|---|---|
| `cold_create_ms` | `Sandbox()` on a fresh tenant — includes VM boot, so this is the worst-case provisioning latency |
| `per_vm_memory_mb` | Host RSS and **Pss** per registered VM (from `/proc/<pid>/smaps_rollup`, VmRSS fallback). Pss counts shared layer pages only once across VMs — RSS vs Pss gap is the sharing win |
| `sandbox_in_tenant_ms` | Extra `Sandbox()` in an existing tenant VM (density *within* a tenant — no new VM) |
| `warm_claim_ms` / `warm_exec_ms` | `Pool.acquire()` claim + first exec on a pre-booted pool VM |
| `exec_ms` | Blocking exec latency (p50 / p95 / mean) |
| `execs_per_sec` | Concurrent blocking execs on one tenant VM (thread-pool sweep) |

## Running

```bash
terra setup base                           # one-time guest assets
sudo terra daemon start                     # root daemon (NAT + /proc access)
python3 sdk/python/tests/manual_density_bench.py --tenants 4 --out /tmp/bench.json
```

Options: `--tenants` (cold-create sweep size), `--sandboxes-per-tenant`,
`--pool-size`, `--concurrency`, `--total-execs`, `--repeats`, `--out`.

The script cleans up everything it created (tenants + pool VMs) even on
failure. The first `Sandbox()` is a warm-up (asset resolution + daemon
readiness) and is excluded from the measured sweep.

## Reading the results

- `cold_create_ms.p50` is the cold-boot latency; `warm_claim_ms` is the
  warm-pool counterpoint. The gap is the warm pool's value.
- `per_vm_memory_mb`: the last row's `pss_mb / tenants` is the
  *incremental* memory per tenant VM once layers are shared; the
  `rss_mb` row shows gross cost per process. Pss < RSS means the
  page-cache sharing claim is real on the measured host.
- `sandbox_in_tenant_ms` should be orders of magnitude below
  `cold_create_ms` — that is the tenant-first density model.

## Honest limitations

- Memory is measured from the host side only; guest-side workdirs and
  per-sandbox state are small and not isolated in the numbers.
- Exec latency includes vsock + guest-proxy round trips; absolute values
  depend on the host CPU and kernel.
- One run is a single-host snapshot, not a comparative benchmark across
  configurations — use the same host for A/B comparisons.
- The engine also needs namespace privileges for virtiofsd (user/mount
  namespaces). On hosts where those are blocked the benchmark aborts with
  exit 3 and an honest message instead of emitting partial numbers.
