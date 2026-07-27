# Terrarium Engine

**Production-grade agent sandboxing — secure, isolated execution environments with high single-node density.**

Terrarium is a scheduling and control layer for agent execution environments. It pairs hardware-level VM isolation (Cloud Hypervisor) with a composable layered filesystem (EROFS + OverlayFS + virtiofs) and a warm pool, so untrusted agent code runs in real VMs that start in well under a second and share read-only environment layers across the host.

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
│  │          Cloud Hypervisor VM × N            │            │
│  │  ┌───────────────────────────────────────┐  │            │
│  │  │  guest-proxy ← host→guest relay       │  │            │
│  │  │  Agent process ◄── sandbox isolation  │  │            │
│  │  └───────────────────────────────────────┘  │            │
│  │  per VM: virtiofs rootfs (layers + /workdir)│            │
│  └─────────────────────────────────────────────┘            │
└──────────────────────────────────────────────────────────────┘
```

The engine is decoupled from backends via two trait families:
`VmAdapter` (Cloud Hypervisor) and `SandboxAdapter` (Sandlock, OpenShell).

## Quick Start

Install the CLI and SDK:

```bash
pip install -e sdk/python
```

**CLI** — resource groups with uniform `ls / create / remove` verbs:

```bash
terra daemon start                              # engine daemon
terra kernel create -n k612 --version 6.12      # build a guest kernel
terra layer create -n python312 --script images/examples/python312.sh
terra pool create --size 3                      # warm pool
terra vm create dev --kernel k612 --rootfs alpine --layers python312,base --net
terra vm exec dev -- python3 --version
terra vm remove dev
```

**Python** — direct mode for throwaway VMs:

```python
import terra

vm = terra.create(layers=["python312", "base"])
print(vm.exec(["python3", "-c", "import numpy; print(numpy.__version__)"]))
vm.destroy()
```

**Client–server** — same code, one connect line, fulfilled by the server's warm pool:

```python
import terra

terra.connect("tcp://server:19099", token="secret")

vm = terra.create(layers=["python312", "base"])
print(vm.exec(["python3", "--version"]))
vm.destroy()
```

**MCP** — point your agent at the stdio server:

```json
{"mcpServers": {"terrarium": {"command": "terra-mcp", "env": {"TERRA_SOCKET": "/tmp/terra.sock"}}}}
```

## Features

- **Layered filesystem** — read-only EROFS layers star-composed on the host (arbitrary combinations, shared page cache), exposed via virtiofs. Tool layers are built by configuring a real VM and packing the delta, so environments are runnable by construction.
- **Warm pool** — pre-booted idle VMs; claiming hot-plugs the requested layers and returns a ready VM. Pool VMs release back to idle for reuse.
- **In-guest exec** — command execution inside VMs through the guest agent, with per-command timeouts.
- **Networking** — one-flag NAT networking (`--net`) with DHCP; lifecycle managed via `terra net`.
- **Dynamic resize** — CPU and memory online adjustment without reboot.
- **Zero-config Python SDK** — managed directories, automatic binary and image resolution, programmable host configuration (`HostConfig`).

## Documentation

- [docs/protocol.md](docs/protocol.md) — engine wire protocol (commands, transports, semantics)
- [docs/sdk.md](docs/sdk.md) — Python SDK and CLI reference
- [docs/mcp.md](docs/mcp.md) — MCP server tools
- [docs/plans/](docs/plans/) — design ADRs

## Roadmap

- ✅ CH base, engine daemon, adapter layer, Python SDK
- ✅ virtiofs layered filesystem, warm pool, NAT networking, layer build-by-doing
- 🔲 Pool auto-scaling, snapshot fault tolerance, density benchmarks, observability

## License

Apache-2.0. Built on Cloud Hypervisor and Linux kernel features. See `THIRD-PARTY` for acknowledgments.
