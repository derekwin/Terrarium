# Terrarium Engine

**Production-grade agent sandboxing — secure, isolated execution environments with high single-node density.**

Terrarium is a scheduling and control layer for agent execution environments. It pairs hardware-level VM isolation (Cloud Hypervisor) with a composable layered filesystem (EROFS + OverlayFS + virtiofs) and a warm pool, so untrusted agent code runs in real VMs that start in well under a second and share read-only environment layers across the host.

**Tenant-first model:** VMs are tenant isolation boundaries. A `Sandbox` is a session inside a VM, not a VM itself. Multiple sandboxes in the same tenant share one VM with isolated workdirs. This gives you both strong security isolation (between tenants) and high density (within a tenant).

## Why Terrarium

| | Container | MicroVM | Terrarium |
|---|---|---|---|
| Isolation | Shared kernel | KVM | **KVM + sandbox** |
| Density | High | Low | **High (shared page-cache layers)** |
| Provisioning | Fast | ~1s | **Warm pool (pre-booted VMs)** |
| Environments | OCI image | Disk image | **Composable named layers** |
| Backends | — | — | **Pluggable (CH, Sandlock, OpenShell)** |

## Architecture

```
┌─ Host ──────────────────────────────────────────────────────┐
│  ┌──────────────────────────────────────────────────────┐   │
│  │                Terrarium Engine                       │   │
│  │     daemon · CLI · Python SDK · MCP Server            │   │
│  └──────────────────────┬───────────────────────────────┘   │
│          ┌──────────────┴──────────────┐                     │
│          ▼                             ▼                     │
│  ┌───────────────┐            ┌────────────────┐            │
│  │  CH Adapter   │            │ SandboxAdapter  │            │
│  │  (VmAdapter)  │            │ Sandlock/OpenShell│          │
│  └───────┬───────┘            └───────┬────────┘            │
│          ▼                            ▼                      │
│  ┌─────────────────────────────────────────────┐            │
│  │       Cloud Hypervisor VM (per tenant)      │            │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────┐    │            │
│  │  │ Sandbox  │ │ Sandbox  │ │ Sandbox  │    │            │
│  │  │ /workdir │ │ /workdir │ │ /workdir │    │            │
│  │  │ session  │ │ session  │ │ session  │    │            │
│  │  └──────────┘ └──────────┘ └──────────┘    │            │
│  │  guest-proxy (vsock relay, per VM)          │            │
│  │  virtiofs rootfs (shared layers + per-sb wd)│            │
│  └─────────────────────────────────────────────┘            │
└──────────────────────────────────────────────────────────────┘
```

The engine is decoupled from backends via two trait families:
`VmAdapter` (Cloud Hypervisor) and `SandboxAdapter` (Sandlock, OpenShell).

## Repository Structure

```
terrarium/
├── README.md / README_zh.md
├── LICENSE / NOTICE / THIRD-PARTY
├── crates/
│   ├── engine/               # Engine daemon: PyO3-backed lib with 7 command submodules
│   ├── adapter/
│   │   ├── traits/           # VmAdapter / SandboxAdapter traits, VmSpec, FsSpec, error types
│   │   ├── cloud-hypervisor/ # CH adapter: 5 modules (FS/VM decoupled), virtiofs, hotplug, network, landlock
│   │   ├── sandlock/         # Sandlock adapter (Landlock ABI capability gating)
│   │   └── openshell/        # OpenShell adapter
│   ├── fs/                   # Independent filesystem crate: EROFS, cpio, layer build/list/remove (PyO3 bindings)
│   ├── protocol/             # Shared Command / Response types (single source of truth)
│   ├── guest-proxy/          # In-guest agent: vsock relay, exec, mount, umount
│   ├── network/              # Tap / NAT / dnsmasq DHCP, tc QoS
│   └── mcp/                  # MCP server (stdio JSON-RPC, 13 user-facing tools)
├── sdk/python/               # Python SDK (terra package: Sandbox, Pool, Template, client, daemon, assets, images)
├── images/                   # Guest kernel / rootfs / initramfs build scripts and examples
└── docs/                     # Protocol, SDK, MCP docs and design ADRs
```

## Quick Start

Install from the repository root:

```bash
pip install -e .
```

**Sandbox API** — the recommended high-level entry point (auto-starts daemon, context-manager cleanup):

```python
from terra.sandbox import Sandbox

# Sandbox = session inside a tenant VM (tenant-first model)
with Sandbox(tenant="my-org", template="py312", network=True) as sb:
    result = sb.exec(["python3", "-c", "print(2+2)"])
    print(result.stdout)              # "4\n"
    sb.files.write("/workdir/hello.txt", "Hello, Terrarium!")
    print(sb.files.read("/workdir/hello.txt"))
    print(sb.id)                      # "tenant-my-org/sb-a3f2"
    print(sb.vm)                      # "tenant-my-org"

# Multiple sandboxes share one tenant VM
sb1 = Sandbox(tenant="research-team", template="py312")
sb2 = Sandbox(tenant="research-team")   # reuses same VM, new workdir
sb3 = Sandbox(tenant="research-team")

sb1.kill()  # removes session workdir only — VM survives for siblings
Sandbox.destroy_tenant("research-team")  # destroys the VM and all sessions
```

**Warm pool** — pre-booted shared-tenant VMs, layers hot-plugged on claim:

```python
from terra.pool import Pool

pool = Pool(template="py312", size=3)  # 3 pre-booted VMs
sb1 = pool.acquire()                   # Sandbox in a shared pool VM
sb2 = pool.acquire()                   # same VM, different workdir
print(sb1.exec(["python3", "--version"]).stdout)
pool.release(sb1)                      # back to idle
pool.release(sb2)
```

**CLI** — resource groups with uniform verbs:

```bash
sudo env "PATH=$PATH" terra daemon start                     # engine daemon (root enables NAT networking)
terra image build kernel -n k612 --version 6.12               # build a guest kernel
terra image build rootfs -n alpine                             # build a distro base system
terra layer create -n python312 --rootfs alpine --script images/examples/python312.sh --kernel k612
terra template create -n py312 --kernel k612 --rootfs alpine --layers python312,base
terra sandbox create --tenant research-team --template py312 --net   # tenant-first sandbox
terra sandbox kill tenant-research-team/sb-a3f2                       # kill one session
terra sandbox destroy-tenant research-team                            # destroy VM + all sessions
terra pool create -n mypool --size 3                                  # warm pool
terra pool claim --template py312                                     # claim a ready sandbox
terra daemon config                                            # engine, pool, net, layers at a glance
```

**MCP** — point your agent at the stdio server:

```json
{"mcpServers": {"terrarium": {"command": "terra-mcp", "env": {"TERRA_SOCKET": "/tmp/terra.sock"}}}}
```

## Features

- **High-level Sandbox API** — `terra.Sandbox` / `terra.AsyncSandbox` with tenant-first model: VMs are tenant isolation boundaries, sandboxes are sessions within a VM. Multiple sandboxes in the same tenant share one VM with isolated workdirs. Automatic daemon start, context-manager cleanup, file operations (read/write/upload/download/list), metrics, and online resize.
- **Layered filesystem** — read-only EROFS layers star-composed on the host (arbitrary combinations, shared page cache), exposed via virtiofs. Distro base layers come from a config-driven pipeline (alpine and ubuntu ship today; more are a 3-line config). Tool layers are built by configuring a real VM and packing the delta, so environments are runnable by construction.
- **Warm pool** — pre-booted idle VMs as shared tenant containers; acquiring returns a sandbox session within a pool VM. Multiple acquires from the same pool share the same VM with isolated workdirs. Pool VMs release back to idle for reuse. Dynamic `grow()` / `scale` for live size adjustment.
- **Named templates** — `terra.template.Template` persists kernel + base distro + tool layer compositions. `template create` / `template ls` / `template remove` managed from CLI or SDK.
- **In-guest exec** — blocking and background execution inside VMs through the guest agent, with session tracking (`session_status`, `session_kill`, `session_list`), per-command timeouts, and structured `ExecResult`.
- **Networking** — one-flag NAT networking (`--net`) with DHCP; lifecycle managed via `terra net`.
- **Dynamic resize** — CPU and memory online adjustment without reboot.
- **Zero-config Python SDK** — managed directories, automatic binary and image resolution, daemon auto-start, programmable host configuration (`HostConfig`).

## Documentation

- [docs/protocol.md](docs/protocol.md) — engine wire protocol (commands, transports, semantics)
- [docs/sdk.md](docs/sdk.md) — Python SDK and CLI reference
- [docs/mcp.md](docs/mcp.md) — MCP server tools
- [docs/plans/](docs/plans/) — design ADRs

## Roadmap

- ✅ CH base, engine daemon, adapter layer, Python SDK
- ✅ virtiofs layered filesystem, warm pool, NAT networking, layer build-by-doing
- ✅ High-level Sandbox / Pool / Template API, exception hierarchy, async support
- 🔲 Pool auto-scaling, snapshot fault tolerance, density benchmarks, observability

## License

Apache-2.0. Built on Cloud Hypervisor and Linux kernel features. See `THIRD-PARTY` for acknowledgments.
