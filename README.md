# Terrarium

Terrarium is a lightweight sandbox platform for AI Agent workloads: **microVMs as the isolation boundary, process sandboxes as the execution unit** — providing a secure, elastic, observable, and fault-tolerant agent execution environment.

Containers share a kernel with the agent process, limiting how well they constrain untrusted code. Traditional VMs are heavy and statically provisioned. Terrarium combines the best of both: hardware-level isolation with per-agent sandboxing inside the VM, all under a dynamic resource model that scales CPU, memory, and disk on demand.

**Tech stack**: VMM base is a [Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor) fork (thin fork — "configure, don't patch"). The self-developed control plane and sandbox layer form the core IP.

## Core Goals

- **Dynamic resources**: CPU, memory, and disk scale online via Cloud Hypervisor's resize API. The model is "pre-create at boot, adjust at runtime" — vCPUs declared up to a cap, virtio-mem attached at boot and resized via config change. No guest kernel patches required.
- **Two-layer isolation**:
  - **VM layer**: KVM hardware virtualization as the security boundary
  - **Sandbox layer**: namespaces + OverlayFS + cgroup v2 + Landlock + seccomp-bpf, one execution unit per agent
- **Observability**: in-guest eBPF (CO-RE) collects syscalls, file, network, and resource usage per sandbox, reported to the host over vsock
- **Snapshot fault tolerance**: three levels — FS CoW snapshots, per-process CRIU at agent step boundaries, and full VM snapshot/restore via Cloud Hypervisor
- **Phase-aware scheduling**: sched_ext scheduler on the host reclaims vCPU time during LLM inference wait periods
- **Warm pools**: pre-booted VMs ready for instant sandbox claims

## Architecture

```
┌─ Host ──────────────────────────────────────────────────────────┐
│  terra-controller daemon (sole control plane entry)             │
│  Input: PSI / DAMON / eBPF metering → Output: CH resize API     │
│  sched_ext scheduler (phase-aware CPU reclamation)               │
│                                                                  │
│  cloud-hypervisor: one process per VM (spawned & managed by      │
│  controller via unix domain socket API)                          │
│  ┌─ VM (KVM isolation) ───────────────────────────────────────┐ │
│  │  sandboxd: sandbox lifecycle management                    │ │
│  │  ┌───────────┐ ┌───────────┐ ┌───────────┐                │ │
│  │  │  Agent    │ │  Agent    │ │  Agent    │ ...            │ │
│  │  │  sandbox  │ │  sandbox  │ │  sandbox  │                │ │
│  │  └───────────┘ └───────────┘ └───────────┘                │ │
│  │  observe (eBPF)          │  checkpoint daemon             │ │
│  └────────────────────┬──────────────────────────────────────┘ │
│                vsock control / telemetry channel                │
└─────────────────────────────────────────────────────────────────┘
```

| Layer | Isolation | Resources | Monitoring | Fault Tolerance | Form Factor |
|---|---|---|---|---|---|
| **VM** | KVM hardware virtualization | virtio-mem / vCPU / balloon / blk resize | PSI, DAMON | CH VM snapshot / restore | One CH process per VM, managed by controller |
| **Sandbox** | namespaces + Landlock + seccomp | cgroup v2 quotas & throttling | per-sandbox eBPF | FS CoW + CRIU (step boundary) | Resident daemon inside guest |

## Usage (M2+)

The sandbox, not the VM, is Terrarium's first-class citizen:

```python
import terra

with terra.sandbox.create(name='dev', image='python:3.12') as sb:
    proc = sb.exec('python', '-c', 'print(2 ** 10)')
    proc.wait()
    print(proc.stdout.read())          # 1024

    snap = sb.snapshot()

sb2 = terra.sandbox.create(name='dev2', snapshot=snap)
```

- **Python SDK**: `create / exec / terminate / snapshot / pause / resume / resize / ls` with async `.aio` variants
- **CLI**: `terra sandbox create / exec / ls / terminate / snapshot / pool`
- **MCP Server**: sandbox capabilities exposed as MCP tools
- **Warm pools**: `create_pool(image=..., replicas=...)` for instant claims
- **Online adjustment**: `pause() / resume()` per VM, `resize(cpus=..., memory_gb=...)` live

## Repository Structure

```
terrarium/
├── AGENTS.md / README.md / README_zh.md
├── LICENSE (Apache-2.0) / NOTICE / THIRD-PARTY
├── hypervisor/             # Cloud Hypervisor fork (git submodule or vendored branch)
│   └── PATCHES.md          # local patch registry
├── crates/
│   ├── ch-client/          # CH API socket client (create/start/resize/add-disk/snapshot)
│   ├── controller/         # terra-controller daemon (control plane)
│   ├── sandboxd/           # in-guest sandbox runtime (M2)
│   ├── observe/            # in-guest eBPF observability daemon (M2)
│   ├── checkpoint/         # snapshot coordination (M3)
│   ├── cli/                # terra CLI (M2)
│   └── mcp/                # MCP Server (M2)
├── sdk/python/             # Python SDK (M2)
├── images/                 # guest kernel config & rootfs build scripts
├── docs/decisions/         # Architecture Decision Records
└── .github/workflows/      # CI
```

## Roadmap

- **M0 — CH Base & Dynamic Resource Validation**: fork integration, guest image build, baseline startup, CPU/memory/disk resize testing, ch-client skeleton
- **M1 — Controller Skeleton & Manual Resource Loop**: full ch-client API, VM lifecycle management, manual resize trigger validation
- **M2 — Sandbox Layer & Developer Interfaces**: sandboxd full isolation stack, eBPF telemetry, Python SDK / CLI / MCP Server
- **M3 — Snapshot Fault Tolerance**: FS CoW snapshots → CH VM snapshot/restore → per-process CRIU
- **M4 — Automation & Density**: PSI/DAMON closed-loop decisions, sched_ext scheduling, warm pools, density benchmarks

## Acknowledgments

Terrarium is built on [Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor) (Apache License 2.0). We maintain a thin fork with minimal, well-documented patches. See `hypervisor/PATCHES.md` and `THIRD-PARTY` for details.

This project is released under the Apache License 2.0.
