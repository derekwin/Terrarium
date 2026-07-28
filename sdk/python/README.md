# terra-sdk — Terrarium Engine Python SDK

Python SDK for [Terrarium Engine](../../README.md): VM sandbox orchestration
for AI agent workloads — Cloud Hypervisor VMs, layered virtiofs rootfs,
warm pool, in-guest exec.

## Install

```bash
pip install -e .                     # from the repo root
# or: pip install -e sdk/python      # from the sdk/python directory
```

You do **not** need to install host binaries yourself. On first use the SDK
downloads what it needs into `~/.local/share/terra/` (override with
`TERRA_HOME`):

- `cloud-hypervisor` — official static release (GitHub)
- `virtiofsd` — qemu build via apt download (no sudo) or `cargo install`
- `mkfs.erofs` / `erofsfuse` — erofs-utils via apt download
- guest images — built from a repo checkout, or downloaded from
  `TERRA_ARTIFACT_BASE` when provided

## Usage model

Two roles, two tools:

| Role | Tool | What they do |
|---|---|---|
| **Platform / host admin** | `terra` CLI | Runs `terra daemon` (or a systemd unit), prepares images once: `terra image kernel/rootfs/initramfs/layer`, creates the warm pool |
| **Agent application** | this SDK | Client only — talks to the running daemon: `pool_claim` → `vm_exec` → `pool_release`. Never starts daemons or touches host setup |

The SDK's `Daemon` context manager below is a **dev/test convenience** for
single-process scenarios, not the production shape.

## Quickstart

```python
# High-level Sandbox API (recommended) — tenant-first model, auto-starts daemon
from terra.sandbox import Sandbox

with Sandbox(tenant="my-org", template="py312", network=True) as sb:
    result = sb.exec(["python3", "-c", "print(2+2)"])
    print(result.stdout)              # "4\n"
    sb.files.write("/workdir/hello.txt", "Hello, Terrarium!")
    print(sb.id)                      # "tenant-my-org/sb-a3f2"
    print(sb.vm)                      # "tenant-my-org"

# Multiple sandboxes share one tenant VM
sb1 = Sandbox(tenant="research-team", template="py312")
sb2 = Sandbox(tenant="research-team")   # reuses same VM, new workdir

sb1.kill()  # removes session workdir, VM survives
Sandbox.destroy_tenant("research-team")  # destroys VM + all sessions

# Warm pool — pre-booted VMs, each acquire returns a Sandbox in a shared pool VM
from terra.pool import Pool

pool = Pool(template="py312", size=3)
sb1 = pool.acquire()
sb2 = pool.acquire()  # same VM as sb1, different workdir
print(sb1.exec(["python3", "--version"]).stdout)
pool.release(sb1)
pool.release(sb2)

# Direct VM — full control
from terra.vm import create

vm = create("dev", "/path/to/vmlinux.bin",
            initramfs="/path/to/initramfs-virtiofs.cpio.gz",
            layers=["python", "base"])
print(vm.info())
vm.exec(["ls", "/"])
vm.shutdown()
```

## API map

- `terra.sandbox.Sandbox` — tenant-first sandbox: exec, files (read/write/upload/download/list), resize, metrics, context manager. Multiple sandboxes share one tenant VM.
- `terra.async_sandbox.AsyncSandbox` — async wrapper for asyncio applications
- `terra.pool.Pool` — warm pool management: acquire/release/status/grow
- `terra.template.Template` — named environment compositions: from_layers/list/load/remove/build
- `terra.exceptions` — structured exception hierarchy: TerraError, ExecError, SandboxTimeoutError, etc.
- `terra.daemon.Daemon` — engine daemon lifecycle (auto-started by Sandbox/Pool, zero env vars)
- `terra.client.TerraClient` — VM + pool + exec protocol client
- `terra.vm` — `create()`, `list_vms()`, `Vm` objects
- `terra.assets` — binary management (`ensure_ch`, `ensure_virtiofsd`, ...)
- `terra.images` — guest images (`ensure`, `ensure_all`, `build_layer`)
- `terra.paths` — managed directory layout

## Environment variables (all optional)

| Var | Purpose | Default |
|---|---|---|
| `TERRA_HOME` | managed root | `~/.local/share/terra` |
| `TERRA_CH_BINARY` | CH binary override | auto-download |
| `TERRA_VIRTIOFSD` | virtiofsd override | auto-resolve/download |
| `TERRA_ENGINE` | engine binary override | repo build / PATH |
| `TERRA_ARTIFACT_BASE` | prebuilt guest-image URL | repo build scripts |

## Tests & demo

```bash
python3 sdk/python/examples/warm_pool_demo.py   # full app demo (real KVM)
python3 sdk/python/tests/test_e2e_real.py       # e2e suite (real KVM)
```
