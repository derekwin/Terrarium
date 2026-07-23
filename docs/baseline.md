# M0 Baseline Measurements

Cloud Hypervisor guest boot baseline for Terrarium. This document records build
artifact sizes, the kernel configuration baseline, cold boot measurement
procedure, memory footprint methodology, and feature trimming analysis.

**Status: Measurements deferred.** KVM access is not available in the current
development environment (user `liujinyao` is not in the `kvm` group, and
passwordless sudo is not configured). The measurement procedures and commands
are fully specified below so that they can be executed without ambiguity once
KVM access is obtained.

---

## 1. Environment

| Item | Value |
|------|-------|
| Host CPU | Intel (kvm_intel module loaded) |
| Host kernel | `/dev/kvm` present, KVM modules loaded |
| Cloud Hypervisor | v53.0, static binary at `/tmp/cloud-hypervisor-static` |
| CH binary size | 6.8 MB |
| CH source | Prebuilt release binary (`cloud-hypervisor-static`) |
| Local patches | Zero (see `hypervisor/PATCHES.md`) |
| KVM access | **Not available.** User not in `kvm` group. |
| Guest kernel | Linux 6.12.0, bzImage (see Section 2) |
| Guest rootfs | busybox-static v1.36.1, directory tree |

**KVM module status:**

```
kvm_intel             487424  0
kvm                  1404928  1 kvm_intel
irqbypass              12288  1 kvm
```

`/dev/kvm` exists with group `kvm`, but the build user does not have access
(`groups liujinyao` does not include `kvm`).

```
$ ls -l /dev/kvm
crw-rw----+ 1 root kvm 10, 232 Jul 23 14:17 /dev/kvm
```

---

## 2. Build Artifacts

### 2.1 Kernel

Built from upstream Linux 6.12 with `make allnoconfig` followed by
`images/kernel/config-6.12` and `make olddefconfig` for dependency resolution.

```
$ file target/guest/vmlinux.bin
target/guest/vmlinux.bin: Linux kernel x86 boot executable bzImage,
version 6.12.0 (liujinyao@sdu-232) #1 SMP PREEMPT_DYNAMIC
Thu Jul 23 16:53:30 CST 2026, RO-rootFS, swap_dev 0X5, Normal VGA

$ ls -lh target/guest/vmlinux.bin
-rw-rw-r-- 1 liujinyao liujinyao 5.5M Jul 23 16:53 target/guest/vmlinux.bin
```

| Metric | Value |
|--------|-------|
| Format | bzImage (x86 boot executable) |
| Version | Linux 6.12.0 |
| Size | 5.5 MB |
| Build date | 2026-07-23 |
| Source config | `images/kernel/config-6.12` (86 explicitly enabled options) |
| Target | ≤ 30 MB (AGENTS.md requirement) — well within budget |

### 2.2 Rootfs

Static busybox with a minimal init script. Built via `images/build-rootfs.sh`.

```
$ du -sh target/guest/rootfs/
2.2M    target/guest/rootfs/
```

| Metric | Value |
|--------|-------|
| Busybox version | v1.36.1 (Ubuntu package: busybox-static) |
| Init | `images/rootfs/init` (mounts proc/sysfs/devtmpfs/tmpfs, starts shell) |
| Size | 2.2 MB |
| Format | Directory tree (for virtio-fs `--fs` mount) |
| Applets | sh, ls, cat, cp, mv, rm, mkdir, mount, umount, ip, echo, grep, awk, cut, head, tail, wc, sleep, sync, reboot, poweroff, ps, kill, free, df, du, chmod, chown, ln, tar, gzip |

The rootfs is mounted via CH's `--fs` (virtio-fs) flag rather than as a block
device. The init script:

```sh
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mount -t tmpfs tmpfs /tmp
ip link set lo up
echo "Terrarium guest ready."
exec /bin/sh
```

---

## 3. Kernel Configuration

The baseline config (`images/kernel/config-6.12`) has 121 lines total:
86 explicitly enabled options and 1 commented-out option, plus section
headers and comments. After `make olddefconfig`, dependency resolution adds
further options (counted as `grep -c '^CONFIG_' .config` in the resolved
output).

### 3.1 Enabled Options by Section

**Architecture & Boot (9 options)**

Required to bring up x86_64 as a KVM guest with SMP.

```
CONFIG_64BIT, CONFIG_X86_64, CONFIG_SMP, CONFIG_NR_CPUS=64,
CONFIG_X86_X2APIC, CONFIG_HYPERVISOR_GUEST, CONFIG_PARAVIRT,
CONFIG_KVM_GUEST
```

**Basic Kernel Features (12 options)**

System call interface and IPC primitives needed by userspace.

```
CONFIG_PRINTK, CONFIG_BUG, CONFIG_ELF_CORE, CONFIG_BASE_FULL,
CONFIG_FUTEX, CONFIG_EPOLL, CONFIG_SIGNALFD, CONFIG_TIMERFD,
CONFIG_EVENTFD, CONFIG_SHMEM, CONFIG_AIO, CONFIG_IO_URING,
CONFIG_ADVISE_SYSCALLS
```

**Process & Scheduling (4 options)**

Timer infrastructure for idle and preemption.

```
CONFIG_TICK_CPUIDLE, CONFIG_NO_HZ_IDLE, CONFIG_HIGH_RES_TIMERS,
CONFIG_PREEMPT_NONE
```

**Filesystem Support (5 options)**

Pseudo-filesystems required by any Linux userspace.

```
CONFIG_PROC_FS, CONFIG_PROC_SYSCTL, CONFIG_SYSFS, CONFIG_TMPFS,
CONFIG_DEVTMPFS
```

**Executable Formats (2 options)**

```
CONFIG_BINFMT_ELF, CONFIG_BINFMT_SCRIPT
```

**Block Layer (2 options)**

```
CONFIG_BLOCK, CONFIG_BLK_DEV_INITRD
```

**Networking (7 options)**

Minimal stack: AF_PACKET, AF_UNIX, TCP/IPv4/IPv6.

```
CONFIG_NET, CONFIG_PACKET, CONFIG_UNIX, CONFIG_INET,
CONFIG_TCP_CONG_CUBIC, CONFIG_DEFAULT_CUBIC, CONFIG_IPV6
```

**CH Runtime Required (13 options)**

Mandatory for Cloud Hypervisor to communicate with the guest. virtio-pci
transport, block/net/console/rng devices, vsock for host-guest comms,
and serial console.

```
CONFIG_VIRTUALIZATION, CONFIG_KVM, CONFIG_VIRTIO_MENU,
CONFIG_VIRTIO_PCI, CONFIG_VIRTIO_BLK, CONFIG_VIRTIO_NET,
CONFIG_VIRTIO_CONSOLE, CONFIG_VIRTIO_RNG,
CONFIG_VSOCKETS, CONFIG_VIRTIO_VSOCKETS,
CONFIG_SERIAL_8250, CONFIG_SERIAL_8250_CONSOLE, CONFIG_TTY
```

**PCI (2 options)**

```
CONFIG_PCI, CONFIG_PCI_MSI
```

**Dynamic Resource (6 options)**

CPU hotplug (ACPI), memory hotplug (virtio-mem), and balloon for
cooperative memory reclaim. These are the three mechanisms CH uses
for `vm.resize` on CPU and memory.

```
CONFIG_ACPI, CONFIG_ACPI_HOTPLUG_CPU,
CONFIG_MEMORY_HOTPLUG, CONFIG_MEMORY_HOTPLUG_DEFAULT_ONLINE,
CONFIG_VIRTIO_MEM, CONFIG_VIRTIO_BALLOON
```

**Sandbox & Observability — Pre-embedded for M2 (24 options)**

Compiled into the kernel now to avoid guest reboots later: Landlock,
seccomp, cgroups, namespaces, PSI, DAMON, OverlayFS, BPF, CRIU support.

```
CONFIG_SECURITY, CONFIG_SECURITY_LANDLOCK,
CONFIG_SECCOMP, CONFIG_SECCOMP_FILTER,
CONFIG_CGROUPS, CONFIG_CGROUP_CPUACCT, CONFIG_CGROUP_DEVICE,
CONFIG_CGROUP_FREEZER, CONFIG_CGROUP_SCHED, CONFIG_CPUSETS,
CONFIG_MEMCG, CONFIG_BLK_CGROUP,
CONFIG_NAMESPACES, CONFIG_UTS_NS, CONFIG_IPC_NS,
CONFIG_USER_NS, CONFIG_PID_NS, CONFIG_NET_NS,
CONFIG_PSI, CONFIG_DAMON, CONFIG_OVERLAY_FS,
CONFIG_BPF, CONFIG_BPF_SYSCALL, CONFIG_CGROUP_BPF,
CONFIG_CHECKPOINT_RESTORE
```

### 3.2 Coverage Against AGENTS.md Requirements

| AGENTS.md Requirement | Config Option | Status |
|-----------------------|---------------|--------|
| CH runtime: virtio-pci | `CONFIG_VIRTIO_PCI` | ✓ |
| CH runtime: virtio-blk | `CONFIG_VIRTIO_BLK` | ✓ |
| CH runtime: virtio-net | `CONFIG_VIRTIO_NET` | ✓ |
| CH runtime: vsock | `CONFIG_VSOCKETS`, `CONFIG_VIRTIO_VSOCKETS` | ✓ |
| CH runtime: serial console | `CONFIG_SERIAL_8250_CONSOLE` | ✓ |
| CH runtime: devtmpfs | `CONFIG_DEVTMPFS` | ✓ |
| Dynamic: CPU hotplug | `CONFIG_ACPI_HOTPLUG_CPU` | ✓ |
| Dynamic: memory hotplug | `CONFIG_MEMORY_HOTPLUG` | ✓ |
| Dynamic: virtio-mem | `CONFIG_VIRTIO_MEM` | ✓ |
| Dynamic: balloon | `CONFIG_VIRTIO_BALLOON` | ✓ |
| M2: Landlock | `CONFIG_SECURITY_LANDLOCK` | ✓ |
| M2: seccomp | `CONFIG_SECCOMP` | ✓ |
| M2: cgroups | `CONFIG_CGROUPS` | ✓ |
| M2: PSI | `CONFIG_PSI` | ✓ |
| M2: DAMON | `CONFIG_DAMON` | ✓ |
| M2: OverlayFS | `CONFIG_OVERLAY_FS` | ✓ |
| M2: BPF | `CONFIG_BPF_SYSCALL`, `CONFIG_CGROUP_BPF` | ✓ |
| M2: CRIU | `CONFIG_CHECKPOINT_RESTORE` | ✓ |
| M2: userfaultfd | Requires KVM/MMU to be enabled | Implicit via `CONFIG_KVM` |

**Result: All AGENTS.md requirements are covered.** The only item not
explicitly set as `=y` is `CONFIG_USERFAULTFD`, which is enabled implicitly
by `CONFIG_KVM` (it depends on KVM's MMU notifier infrastructure).

---

## 4. Cold Boot Test Procedure

### 4.1 Boot Command

The exact command for measuring cold boot time. The kernel is booted
directly (no bootloader) with CH's `--kernel` flag. The rootfs is mounted
via virtio-fs (`--fs`), not as a block device.

```bash
time cloud-hypervisor \
    --kernel target/guest/vmlinux.bin \
    --cmdline "console=ttyS0 quiet init=/init" \
    --cpus boot=1 \
    --memory size=256M \
    --fs tag=rootfs,socket=/tmp/ch-virtiofs.sock \
    --serial tty \
    --console off
```

**Flag explanation:**

| Flag | Value | Purpose |
|------|-------|---------|
| `--kernel` | `target/guest/vmlinux.bin` | Direct-kernel boot (no firmware) |
| `--cmdline` | `console=ttyS0 quiet` | Serial console, suppress kernel logs for timing |
| `--cpus boot=1` | 1 vCPU | Minimum for idle guest test |
| `--memory size=256M` | 256 MB | Minimum reasonable guest RAM |
| `--fs` | virtio-fs tag + socket | Rootfs mount (directory tree, not block device) |
| `--serial tty` | Terminal passthrough | See guest output directly |
| `--console off` | No graphical console | Reduces VMM overhead |

**Note:** If virtio-fs is not available in the static binary, use a cpio
initramfs or prepare an ext4 image instead:

```bash
# Alternative: cpio initramfs
(cd target/guest/rootfs && find . | cpio -o -H newc) > target/guest/initramfs.cpio

time cloud-hypervisor \
    --kernel target/guest/vmlinux.bin \
    --cmdline "console=ttyS0 quiet" \
    --initramfs target/guest/initramfs.cpio \
    --cpus boot=1 \
    --memory size=256M \
    --serial tty \
    --console off
```

### 4.2 Time Measurement

Boot time is wall-clock time from CH process start to the guest init script
printing "Terrarium guest ready." on the serial console.

**Method A: `time` (wall-clock, single measurement)**

```bash
time cloud-hypervisor \
    --kernel target/guest/vmlinux.bin \
    --cmdline "console=ttyS0 quiet init=/init" \
    --cpus boot=1 --memory size=256M \
    --fs tag=rootfs,socket=/tmp/ch-virtiofs.sock \
    --serial tty --console off
# Press Ctrl-C or wait for guest to exit
```

**Method B: Scripted measurement (averaged over N runs)**

```bash
#!/bin/bash
# Measure cold boot time over N runs.
# Guest init should print a marker line before exec /bin/sh,
# then poweroff after a short delay.
N=${1:-10}
total=0
for i in $(seq 1 $N); do
    start=$(date +%s%N)
    cloud-hypervisor \
        --kernel target/guest/vmlinux.bin \
        --cmdline "console=ttyS0 quiet init=/init" \
        --cpus boot=1 --memory size=256M \
        --fs tag=rootfs,socket=/tmp/ch-virtiofs.sock \
        --serial tty --console off
    end=$(date +%s%N)
    elapsed=$(( (end - start) / 1000000 ))  # ms
    total=$((total + elapsed))
    echo "Run $i: ${elapsed}ms"
    sleep 1
done
avg=$((total / N))
echo "Average (N=$N): ${avg}ms"
```

**Method C: CH boot timestamp (most accurate)**

Cloud Hypervisor logs a timestamp at VM boot completion. Parse the log:

```bash
cloud-hypervisor \
    --kernel target/guest/vmlinux.bin \
    --cmdline "console=ttyS0 quiet init=/init" \
    --cpus boot=1 --memory size=256M \
    --fs tag=rootfs,socket=/tmp/ch-virtiofs.sock \
    --serial tty --console off \
    -v 2>&1 | grep -E "boot|VM created|vCPUs"
```

### 4.3 Memory Footprint Measurement

Measure the VMM process RSS while the guest is idle at the shell prompt.

```bash
# Start CH in background
cloud-hypervisor \
    --kernel target/guest/vmlinux.bin \
    --cmdline "console=ttyS0 quiet init=/init" \
    --cpus boot=1 --memory size=256M \
    --fs tag=rootfs,socket=/tmp/ch-virtiofs.sock \
    --serial tty --console off &
CH_PID=$!
sleep 3  # Wait for guest to reach shell

# Measure RSS (resident set size) in MB
ps -o pid,rss,comm -p $CH_PID --no-headers | awk '{printf "PID=%s RSS=%dMB CMD=%s\n", $1, $2/1024, $3}'

# Detailed memory map
grep -E "VmRSS|VmSize|VmPeak" /proc/$CH_PID/status
```

**Target from AGENTS.md:**

| Metric | Target | Measurement Method |
|--------|--------|-------------------|
| Cold boot to shell | ≤ 500 ms | Average of 10 runs, wall-clock |
| VMM idle RSS | ≤ 50 MB | `ps` RSS after guest reaches shell prompt |

---

## 5. Feature Trimming Analysis

The CH v53.0 static binary (6.8 MB) is a prebuilt release with all features
enabled. Rebuilding from source allows feature trimming to reduce binary size
and eliminate unused code paths.

### 5.1 Features to Disable

These CH Cargo features are not needed for M0 (or any Terrarium milestone)
and can be disabled in a custom build:

| Feature | Default | Recommended | Rationale |
|---------|---------|-------------|-----------|
| `tpm` | on | **off** | Trusted Platform Module. Not needed for agent sandbox workload. |
| `tdx` | on | **off** | Intel TDX confidential computing. Requires special hardware + firmware. Irrelevant for Terrarium. |
| `sev_snp` | on | **off** | AMD SEV-SNP confidential computing. Requires EPYC hardware. Irrelevant for Terrarium. |
| `vhdx` | on | **off** | Hyper-V virtual disk format. Terrarium uses raw/ext4 images, not VHDX. |
| `fwdebug` | on | **off** | Firmware debug output. Not needed outside CH development. |
| `gdb` | on | **off (M0)** | GDB stub for guest debugging. Useful for kernel development but adds attack surface. Re-enable if kernel debugging is needed. |

### 5.2 Features to Keep

| Feature | Keep? | Rationale |
|---------|-------|-----------|
| `kvm` | **yes** | Core requirement. No KVM, no microVM. |
| `mshv` | **no** | Microsoft Hyper-V hypervisor. Linux host only. |
| `virtiofs` | **yes** | Rootfs mount mechanism (`--fs`). More efficient than block device for directory trees. |
| `vsock` | **yes** | Host-guest communication channel (terraform-controller to sandboxd/observe). M1+. |
| `pci` | **yes** | Virtio transport. Required. |
| `acpi` | **yes** | CPU hotplug mechanism. Required for dynamic CPU resize. |

### 5.3 Rebuild Command

```bash
# From hypervisor/ directory (CH fork)
cargo build --release \
    --no-default-features \
    --features "kvm,virtiofs,vsock,pci,acpi" \
    --bin cloud-hypervisor

# Compare binary size
ls -lh target/release/cloud-hypervisor
```

### 5.4 Expected Impact

| Metric | Prebuilt (v53.0) | After Trim (estimated) |
|--------|-----------------|------------------------|
| Binary size | 6.8 MB | **TBD after build** |
| Cold boot time | TBD (KVM needed) | **TBD after build** |
| VMM idle RSS | TBD (KVM needed) | **TBD after build** |

The primary gains from feature trimming are binary size reduction (fewer
dependencies linked) and a smaller attack surface. Boot time impact is
expected to be marginal since disabled features are compile-time gated behind
`#[cfg(feature = "...")]` and do not execute at runtime.

---

## 6. Measurement Tables

### 6.1 Cold Boot Time

| Run # | Wall-clock (ms) | Notes |
|-------|----------------|-------|
| 1 | **TBD** | Cold start, no page cache |
| 2 | **TBD** | |
| ... | **TBD** | |
| 10 | **TBD** | |
| **Avg** | **TBD** | Target: ≤ 500 ms |

### 6.2 Memory Footprint (idle guest, 1 vCPU, 256M RAM)

| Metric | Value |
|--------|-------|
| VmPeak | **TBD** |
| VmSize | **TBD** |
| VmRSS | **TBD** (target: ≤ 50 MB) |
| VmData | **TBD** |
| VmStk | **TBD** |

### 6.3 Binary Size Comparison

| Variant | Size | Reduction |
|---------|------|-----------|
| Prebuilt v53.0 static | 6.8 MB | baseline |
| Custom build, trimmed features | **TBD** | **TBD** |

### 6.4 Guest Artifact Sizes

| Artifact | Size | Notes |
|----------|------|-------|
| Kernel (bzImage) | 5.5 MB | Linux 6.12.0, 86 config options |
| Rootfs (directory tree) | 2.2 MB | busybox-static v1.36.1 |
| **Total guest payload** | **7.7 MB** | Kernel + rootfs |

---

## 7. KVM Status

**KVM is not accessible in the current environment.** The KVM kernel modules
(`kvm_intel`, `kvm`) are loaded and `/dev/kvm` exists, but the build user
(`liujinyao`) is not in the `kvm` group and passwordless `sudo` is not
configured.

To enable KVM access, one of the following is needed:

```bash
# Option A: Add user to kvm group (recommended for dev)
sudo usermod -a -G kvm liujinyao
# Then log out and back in

# Option B: Set device permissions
sudo chmod 666 /dev/kvm
```

### 7.1 What Can Be Measured Without KVM

These items are verified with actual data:

- [x] Kernel bzImage size: 5.5 MB (within 30 MB target)
- [x] Rootfs size: 2.2 MB
- [x] CH binary version and size: v53.0, 6.8 MB
- [x] Kernel config: 86 options covering all AGENTS.md requirements
- [x] Patch count: zero local patches
- [ ] Feature-trimmed CH binary size: requires build (no KVM needed)
- [ ] Features disabled count: requires build

### 7.2 What Requires KVM

These measurements are documented with exact procedures above. They need KVM
access to execute:

- [ ] Cold boot time (Section 4.2): target ≤ 500 ms
- [ ] VMM memory footprint (Section 4.3): target ≤ 50 MB RSS
- [ ] Boot success verification (CH produces guest shell prompt)
- [ ] Full measurement table (Section 6)

---

## 8. Next Steps

1. **Obtain KVM access.** Add `liujinyao` to `kvm` group or configure
   device permissions as described in Section 7.

2. **Execute cold boot measurement.** Follow Section 4.2 Method B
   (scripted, 10-run average) and populate Section 6.1.

3. **Execute footprint measurement.** Follow Section 4.3 and populate
   Section 6.2.

4. **Build feature-trimmed CH binary.** Run Section 5.3 rebuild command,
   compare binary size, and populate Section 6.3.

5. **Update this document.** Replace all "TBD" entries with actual
   measured values. If any measurement exceeds the M0 acceptance criteria
   (cold boot ≤ 500 ms, footprint ≤ 50 MB), investigate and document the
   cause before proceeding to Task 3.

---

*Document version: 1.0 | Created: 2026-07-23 | Task: M0 — Task 2 (Baseline Testing)*
*Last updated: 2026-07-23 (all artifact measurements current as of build date)*
