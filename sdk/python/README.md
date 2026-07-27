# terra-sdk — Terrarium Engine Python SDK

Python SDK for [Terrarium Engine](../../README.md): VM sandbox orchestration
for AI agent workloads — Cloud Hypervisor VMs, layered virtiofs rootfs,
warm pool, in-guest exec.

## Install

```bash
pip install -e sdk/python            # from the repo
# pip install terra-sdk[ch]          # Cloud Hypervisor backend (marker extra)
# pip install terra-sdk[sandlock]    # + sandlock python bindings
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
from terra.daemon import Daemon
from terra.client import TerraClient
from terra.vm import create

with Daemon():                       # engine daemon, fully managed env
    client = TerraClient()

    # warm pool: idle VMs with hot-plugged layered rootfs
    client.pool_create(1)
    claim = client.pool_claim(["base"])
    print(client.vm_exec(claim["name"], ["ls", "/newroot"]))
    client.pool_release(claim["name"])

    # plain VM
    vm = create("dev", "/path/to/vmlinux.bin",
                initramfs="/path/to/initramfs-virtiofs.cpio.gz",
                layers=["python", "base"])
    print(vm.info())
    vm.exec(["ls", "/"])
    vm.shutdown()
```

## API map

- `terra.daemon.Daemon` — engine daemon lifecycle (zero env vars)
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
