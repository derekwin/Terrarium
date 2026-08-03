# Density Benchmarks

> **Status: harness ready, numbers pending.** The benchmark script is
> committed and import/compile-verified. First real measurements exist
> (2026-08-03, below); they require a KVM-capable host with guest assets
> (`terra setup <distro>`) and user/mount-namespace privileges for
> virtiofsd. The script refuses to run (exit 2) when KVM is absent rather
> than reporting fake numbers.

## Results — 2026-08-03 (first real run)

Host: 128-core x86-64 with nested KVM; Terrarium inside a **privileged
Docker container** on that host (the containerized workspace blocks
user/mount namespaces otherwise); guest assets rebuilt with the current
guest-proxy. Raw JSON: `docs/benchmark-results-2026-08-03.json`.

| Metric | Value |
|---|---|
| `cold_create_ms` (p50 / p95) | **803 / 876 ms** per new tenant VM (boot included) |
| `per_vm_memory_mb` (4 tenants) | RSS **230.6 MB** total (~57.7/VM), Pss **206.0 MB** (~51.5/VM) |
| `sandbox_in_tenant_ms` (p50) | **11.3 ms** — ~70× cheaper than a cold VM |
| `warm_claim_ms` / `warm_exec_ms` | **627 ms** / 18.6 ms |
| `exec_ms` (p50 / p95) | **7.7 / 17.6 ms** |
| `execs_per_sec` (16 concurrent) | **239** |

Reading: Pss < RSS by ~11% at 4 tenants — the shared-layer page-cache
effect exists but is modest for the small base layer; it should grow with
layer count and more VMs sharing the same layers. The 70× in-tenant
sandbox cost vs cold boot is the tenant-first density model working.

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
