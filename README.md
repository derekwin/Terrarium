# Terrarium

> Short name for daily use: **terra** — CLI command `terra`, Python package `import terra`.

Terrarium is a lightweight VMM and sandbox runtime for AI agent workloads. Its goal is to provide a secure, elastic, observable, and fault-tolerant execution environment for agents.

A container's isolation boundary lives in the same user space as the agent process, which limits how well it can constrain untrusted code; traditional VMs are heavy and statically provisioned. Terrarium uses microVMs as the isolation boundary and process sandboxes as the execution units.

## Core Goals

- **Lightweight and fast**: microVM cold boot < 200ms, per-instance memory overhead < 100MB, minimal virtio-mmio device model, no PCI/ACPI dependency
- **Dynamic resources**: online scaling of CPU / memory / disk, based on a "pre-create at boot, adjust at runtime" model — vCPUs are created up to a cap and logically onlined/offlined inside the guest; virtio-mem is attached at boot and resized via config change; no guest kernel patches required
- **Two-layer isolation**:
  - VM layer: KVM hardware isolation as the security boundary
  - Sandbox layer: namespaces + pivot_root + OverlayFS + cgroup v2 + Landlock + seccomp-bpf, one execution unit per agent
- **Observability**: in-guest eBPF (CO-RE) collects syscalls / file / network / resource usage per sandbox, reported to the host over vsock
- **Snapshot and fault tolerance**: three snapshot levels — filesystem CoW snapshots (millisecond-level), per-process CRIU (at agent step boundaries), and full-VM snapshots with userfaultfd lazy restore
- **Security enforcement**: dynamic BPF LSM policies, allowlists for file paths and network egress, per-session resource metering

## Architecture

```
┌─ Host ─────────────────────────────────────────────────┐
│  Resource controller (Terrarium embedded as a library,  │
│  no standalone daemon)                                  │
│  Input: PSI / DAMON working set / eBPF metering         │
│  Output: dynamic resource adjustments                   │
│  sched_ext scheduler (reclaims CPU during LLM waits)    │
│                                                         │
│  terra-vmm: one process per VM (spawned by controller)  │
│  ┌─ Terrarium VM (microVM, KVM isolation)────────────┐ │
│  │  sandboxd: sandbox lifecycle management            │ │
│  │  ┌───────────┐ ┌───────────┐ ┌───────────┐         │ │
│  │  │   Agent   │ │   Agent   │ │   Agent   │ ...     │ │
│  │  │  sandbox  │ │  sandbox  │ │  sandbox  │         │ │
│  │  └───────────┘ └───────────┘ └───────────┘         │ │
│  │  eBPF observability daemon  │  checkpoint daemon   │ │
│  └──────────────────────┬──────────────────────────────┘ │
│                  vsock control/telemetry channel        │
└─────────────────────────────────────────────────────────┘
```

| | VM layer (Terrarium VMM) | Sandbox layer (sandboxd) |
|---|---|---|
| Isolation | KVM hardware virtualization | namespaces + Landlock + seccomp |
| Resources | virtio-mem / balloon / vCPU / blk dynamic resizing | cgroup v2 quotas and throttling |
| Monitoring | VM-level resource profile (PSI / DAMON) | per-sandbox eBPF behavior capture |
| Fault tolerance | full-VM snapshot + uffd lazy restore | FS CoW snapshot + CRIU (step boundary) |
| Form factor | one terra-vmm process per VM (vmm-core crate + thin shell), managed by the controller | resident daemon inside the guest |

## Usage

The sandbox, not the VM, is Terrarium's first-class citizen. Developers only work with a `Sandbox` object:

```python
import terra

with terra.sandbox.create(name='dev', image='python:3.12') as sb:
    proc = sb.exec('python', '-c', 'print(2 ** 10)')
    proc.wait()
    print(proc.stdout.read())          # 1024

    snap = sb.snapshot()               # full state: filesystem + process memory

sb2 = terra.sandbox.create(name='dev2', snapshot=snap)  # restore from snapshot
```

**How a sandbox maps to a VM**: in the default mode, the controller automatically handles VM creation, scaling, and sandbox placement (bin-packing by tenant affinity and resource utilization). The `Sandbox` handle returned by `create()` encapsulates the `(vm, sandbox)` routing information, so subsequent `exec` / `snapshot` calls are routed automatically — developers never need to think about VMs. When explicit control is needed (pausing an entire VM, tenant-exclusive isolation), the VM is also a first-class object:

```python
vm = terra.vm.create(cpus=8, memory_gb=16)   # dedicated VM
sb = vm.sandbox.create(image='python:3.12')
vm.pause()                                   # pauses all sandboxes in the VM
```

Placement hints can also be passed via `terra.sandbox.create(placement=...)`.

- **Python SDK**: `create / exec / terminate / snapshot / pause / resume / resize / ls`, each with an async `.aio` variant; `num_sandboxes=N` for batch creation, suited for RL rollouts and parallel evals
- **CLI**: `terra sandbox create / exec / ls / terminate / snapshot / pool`
- **MCP Server**: sandbox capabilities exposed as MCP tools (create / run / snapshot / terminate), ready for agent clients
- **Warm pools**: `create_pool(image=..., replicas=...)` keeps pre-booted microVMs ready for instant claim; pools can be based on snapshot images
- **Credentials and networking**: `secrets=` / `env=` injected at creation; sandboxes are network-isolated by default and can reach each other only via ports explicitly exposed with `ports=`
- **Online adjustment**: `pause() / resume()` for whole-VM suspend and resume; `resize(cpus=..., memory_gb=...)` for live scaling

## Module Layout

```
terrarium/
├── crates/
│   ├── vmm-core/       # VM lifecycle, address space, vCPU management
│   ├── vmm-devices/    # virtio-mmio devices: blk / virtio-mem / balloon / vsock
│   ├── vmm-snapshot/   # VM state serialization + userfaultfd lazy restore
│   ├── vmm/            # terra-vmm executable (thin shell composing the crates, one process per VM)
│   ├── vmm-api/        # API socket protocol between controller and terra-vmm
│   ├── sandboxd/       # in-guest sandbox runtime: isolation stack, lifecycle, snapshot coordination
│   ├── observe/        # in-guest eBPF observability daemon, reporting over vsock
│   ├── checkpoint/     # CRIU wrapper and step-boundary quiescence protocol
│   ├── controller/     # host resource controller: scheduling, placement, warm pools, resource loop
│   ├── cli/            # command-line tool
│   └── mcp/            # MCP server
├── sdk/python/         # Python SDK (sync + asyncio)
└── xtask/              # build tooling: guest kernel / rootfs packaging
```

## Roadmap

- **M0 Skeleton**: minimal VMM based on rust-vmm, direct-booting a stripped kernel to a shell
- **M1 Dynamic resources**: device layer ready; the "pre-create + adjust" trio (memory / CPU / disk) validated
- **M2 Sandbox layer and developer interfaces**: sandboxd full isolation stack + eBPF telemetry; Python SDK / CLI / MCP Server usable
- **M3 Snapshot and fault tolerance**: FS CoW snapshots → full-VM snapshot with lazy restore (exposed via `snapshot / pause / resume` in the SDK) → per-process CRIU
- **M4 Density and scheduling**: sched_ext scheduling optimizations, single-host density benchmarks, automated resource loop, warm pools online

## Acknowledgments and License

Terrarium is built on the [rust-vmm](https://github.com/rust-vmm) ecosystem. Parts of the device
implementations are derived from [Dragonball](https://github.com/kata-containers/kata-containers)
(Apache License 2.0); see `NOTICE` and `THIRD-PARTY` for details.

This project is released under the Apache License 2.0.
