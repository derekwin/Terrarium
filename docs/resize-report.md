# M0 Dynamic Resource Resize Testing Report

**Cloud Hypervisor v53.0 | Linux 6.12.0 Guest | Terrarium M0 Milestone**

## Summary

This report documents the dynamic resource resize testing methodology for Cloud Hypervisor as the Terrarium VMM base. Three resource dimensions are tested against a single long-lived VM: CPU hotplug via ACPI, memory hotplug/unplug via virtio-mem, and disk expansion via virtio-blk resize and hot-add.

| Test | Scope | Status |
|------|-------|--------|
| CPU resize | 2 ↔ 16 vCPUs, 20 rounds each direction | Pending KVM |
| Memory resize | 512M ↔ 8G, 3 guest states, 10 rounds/state | Pending KVM |
| Disk resize | Online expand + hot-add new disk | Pending KVM |

**KVM dependency:** All resize operations require KVM acceleration (`/dev/kvm`). The current test host lacks KVM access (user not in `kvm` group, no passwordless sudo). The commands and methodology documented below are ready to execute once KVM is available. No measurement data is fabricated.

## Environment

| Component | Version / Path | Notes |
|-----------|---------------|-------|
| Cloud Hypervisor | v53.0 (`/tmp/cloud-hypervisor-static`) | Static binary, x86_64 |
| Guest kernel | Linux 6.12.0 (`target/guest/vmlinux.bin`, 5.5MB) | Built with all dynamic resource configs |
| Guest rootfs | busybox initrd (`target/guest/rootfs/`, 2.2MB) | `/bin/sh` shell, `/proc`, `/sys`, `/dev` mounted |
| Kernel config | `images/kernel/config-6.12` | `CONFIG_ACPI_HOTPLUG_CPU=y`, `CONFIG_VIRTIO_MEM=y`, `CONFIG_MEMORY_HOTPLUG=y`, `CONFIG_VIRTIO_BALLOON=y` |
| Host OS | Linux x86_64 | KVM module loaded but inaccessible to current user |
| CH patches | Zero local patches | See `hypervisor/PATCHES.md` |

## Prerequisites

All commands assume the guest images have been built:

```bash
# One-shot guest image build
bash images/build.sh
# Produces: target/guest/vmlinux.bin + target/guest/rootfs/
```

The CH static binary must be present and executable:

```bash
chmod +x /tmp/cloud-hypervisor-static
```

---

## 1. CPU Resize

### 1.1 Setup

Boot the VM with a CPU range that allows hotplug. The `boot=2,max=16` configuration starts with 2 vCPUs and permits expansion up to 16 via the CH API.

```bash
# Terminal 1: Start VM
/tmp/cloud-hypervisor-static \
    --api-socket /tmp/ch-api.sock \
    --kernel target/guest/vmlinux.bin \
    --cmdline "console=ttyS0" \
    --cpus boot=2,max=16 \
    --memory size=512M \
    --disk path=target/guest/rootfs.ext4 \
    --serial tty \
    --console off
```

In a second terminal, verify the guest environment:

```bash
# Terminal 2: Connect to guest serial, verify initial CPU count
# Guest should show 2 CPUs
cat /proc/cpuinfo | grep processor | wc -l
# Expected: 2
```

Start a background stress workload to ensure the guest has active CPU load during resize:

```bash
# Inside guest
stress-ng --cpu 0 --timeout 0 &
# --cpu 0: one stressor per online CPU, runs indefinitely
```

### 1.2 Test Procedure

The resize loop cycles between 2 and 16 vCPUs, 20 full round-trips (40 resize operations total). Each operation is timed and verified from within the guest.

```bash
# On host, with CH API socket
for round in $(seq 1 20); do
    echo "=== Round $round: scale up to 16 ==="
    time ch-remote --api-socket /tmp/ch-api.sock resize --cpus 16
    sleep 2

    # Verify from within guest
    # Expected: nproc returns 16
    echo "=== Round $round: scale down to 2 ==="
    time ch-remote --api-socket /tmp/ch-api.sock resize --cpus 2
    sleep 2

    # Verify from within guest
    # Expected: nproc returns 2
done
```

Guest-side verification after each resize:

```bash
# Inside guest, after each resize operation
nproc
# Should match the target vCPU count (16 or 2)

# Also check that stress-ng workers are still running
ps aux | grep stress-ng
# Should show active stress-ng processes, count should match nproc
```

### 1.3 Expected Outcome

ACPI CPU hotplug should add or remove vCPUs without guest reboot. The guest kernel, compiled with `CONFIG_ACPI_HOTPLUG_CPU=y`, detects the ACPI events and onlines/offlines CPUs accordingly. stress-ng workers should scale with CPU count without interruption.

**M0 acceptance:** 100% success rate across all 40 resize operations. No guest kernel panics, no CH process crashes.

### 1.4 Measurement Table

| Round | Direction | Latency (ms) | Success | Guest nproc | CH API Response | Notes |
|-------|-----------|-------------|---------|-------------|-----------------|-------|
| 1 | 2 → 16 | (pending KVM) | | | | |
| 1 | 16 → 2 | (pending KVM) | | | | |
| 2 | 2 → 16 | (pending KVM) | | | | |
| 2 | 16 → 2 | (pending KVM) | | | | |
| ... | ... | ... | ... | ... | ... | ... |
| 20 | 2 → 16 | (pending KVM) | | | | |
| 20 | 16 → 2 | (pending KVM) | | | | |

**Aggregate summary (to be filled after test run):**

| Metric | Value |
|--------|-------|
| Total operations | 40 (20 up + 20 down) |
| Successes | (pending) |
| Failures | (pending) |
| Success rate | (pending) |
| Mean expand latency | (pending) ms |
| Mean shrink latency | (pending) ms |
| Guest kernel panics | (pending) |
| CH crashes | (pending) |

### 1.5 CH API Endpoint

```
PUT /api/v1/vm.resize
Body: {"desired_vcpus": <N>}
```

Equivalent CLI: `ch-remote --api-socket <path> resize --cpus <N>`

The `vm.resize` endpoint accepts `desired_vcpus` and `desired_ram` independently. For CPU-only resize, omit `desired_ram`.

---

## 2. Memory Resize

### 2.1 Setup

Boot the VM with virtio-mem hotplug support. `size=512M` sets the initial boot memory; `hotplug_size=32G` sets the maximum pluggable memory. virtio-mem is the preferred hotplug method over ACPI-based DIMM hotplug because it supports both expansion and shrinkage at a fine (2MB block) granularity.

```bash
# Terminal 1: Start VM
/tmp/cloud-hypervisor-static \
    --api-socket /tmp/ch-api.sock \
    --kernel target/guest/vmlinux.bin \
    --cmdline "console=ttyS0" \
    --cpus boot=2,max=8 \
    --memory size=512M,hotplug_method=virtio-mem,hotplug_size=32G \
    --disk path=target/guest/rootfs.ext4 \
    --serial tty \
    --console off
```

Verify initial memory state from the guest:

```bash
# Inside guest
free -m
# Expected: total memory around 500MB
```

### 2.2 Test Procedure: Three States

Memory resize is tested under three distinct guest memory pressure states. Each state runs 10 expand-shrink cycles (512M → 8G → 512M). The expectation is that expansion always succeeds; shrink behavior varies by state.

#### State 1: Idle

Guest has minimal memory pressure. Most pages are free. virtio-mem should shrink seamlessly.

```bash
# Inside guest: no additional workload
# Just verify that idle memory is high
free -m
# Expected: used memory well below 512M

# On host, run the resize loop
for i in $(seq 1 10); do
    echo "=== Idle round $i: expand to 8G ==="
    time ch-remote --api-socket /tmp/ch-api.sock resize --memory $((8 * 1024 * 1024 * 1024))
    sleep 1
    echo "=== Idle round $i: shrink to 512M ==="
    time ch-remote --api-socket /tmp/ch-api.sock resize --memory $((512 * 1024 * 1024))
    sleep 1
done
```

#### State 2: Under Memory Pressure

Allocate 80% of current guest RAM using stress-ng to simulate a workload that holds pages. virtio-mem must reclaim pages despite active usage. Shrink success depends on how much memory the guest kernel can free from the plugged region.

```bash
# Inside guest: allocate 80% of current memory
MEM_MB=$(free -m | awk '/Mem:/{print $2}')
STRESS_MB=$((MEM_MB * 80 / 100))
stress-ng --vm 1 --vm-bytes ${STRESS_MB}M --vm-keep --timeout 0 &

# Verify allocation
free -m
# Expected: used memory around 80% of total

# On host, run the same resize loop
for i in $(seq 1 10); do
    echo "=== Pressure round $i: expand to 8G ==="
    time ch-remote --api-socket /tmp/ch-api.sock resize --memory $((8 * 1024 * 1024 * 1024))
    sleep 1
    echo "=== Pressure round $i: shrink to 512M ==="
    time ch-remote --api-socket /tmp/ch-api.sock resize --memory $((512 * 1024 * 1024))
    sleep 1
done
```

#### State 3: Pinned Pages

Lock a portion of guest memory with `mlock()` to create unmovable pages. virtio-mem cannot reclaim pinned pages, so shrink is expected to fail or partially succeed (reducing only to the size of pinned + base memory).

```bash
# Inside guest: use a small C program or stress-ng with mlock
# stress-ng does not directly support mlock for --vm workers,
# so use a helper that calls mlockall(MCL_CURRENT | MCL_FUTURE)
#
# Alternative: use the mlock-test helper (see appendix)
./mlock-and-sleep 256M &
# Locks 256MB of anonymous memory and sleeps

# On host, run the resize loop
for i in $(seq 1 10); do
    echo "=== Pinned round $i: expand to 8G ==="
    time ch-remote --api-socket /tmp/ch-api.sock resize --memory $((8 * 1024 * 1024 * 1024))
    sleep 1
    echo "=== Pinned round $i: shrink to 512M ==="
    time ch-remote --api-socket /tmp/ch-api.sock resize --memory $((512 * 1024 * 1024))
    sleep 1
done
```

### 2.3 Guest Verification

After each resize, verify from within the guest:

```bash
# Inside guest
free -m
# Compare total memory against the expected target:
#   Expand target: ~8192 MB (8G)
#   Shrink target: ~500 MB (512M)
```

### 2.4 Expected Outcome

| State | Expand | Shrink | Rationale |
|-------|--------|--------|-----------|
| Idle | 100% success | 100% success | No held pages; virtio-mem unplugs all blocks cleanly |
| Under pressure | 100% success | Variable (target ≥70%) | stress-ng holds pages but kernel can reclaim under memory pressure; success depends on reclaim speed and fragmentation |
| Pinned | 100% success | Limited (shrink blocked at pinned boundary) | mlock'd pages are not migratable; virtio-mem reports the minimum achievable size; CH API returns the actual size after attempted shrink |

**M0 acceptance:** Memory expansion 100% success across all states. Shrink gives a success rate distribution and failure mode analysis (how many blocks could not be unplugged, and why).

### 2.5 Measurement Tables

#### State: Idle

| Round | Direction | Latency (ms) | Success | Guest free -m (total) | Notes |
|-------|-----------|-------------|---------|----------------------|-------|
| 1 | 512M → 8G | (pending KVM) | | | |
| 1 | 8G → 512M | (pending KVM) | | | |
| ... | ... | ... | ... | ... | |
| 10 | 512M → 8G | (pending KVM) | | | |
| 10 | 8G → 512M | (pending KVM) | | | |

#### State: Under Pressure

| Round | Direction | Latency (ms) | Success | Guest free -m (total) | Notes |
|-------|-----------|-------------|---------|----------------------|-------|
| 1 | 512M → 8G | (pending KVM) | | | |
| 1 | 8G → 512M | (pending KVM) | | | |
| ... | ... | ... | ... | ... | |
| 10 | 512M → 8G | (pending KVM) | | | |
| 10 | 8G → 512M | (pending KVM) | | | |

#### State: Pinned

| Round | Direction | Latency (ms) | Success | Actual shrink to (MB) | Guest free -m (total) | Notes |
|-------|-----------|-------------|---------|----------------------|----------------------|-------|
| 1 | 512M → 8G | (pending KVM) | | | | |
| 1 | 8G → 512M | (pending KVM) | | | | |
| ... | ... | ... | ... | ... | ... | |
| 10 | 512M → 8G | (pending KVM) | | | | |
| 10 | 8G → 512M | (pending KVM) | | | | |

#### Aggregate Summary (to be filled)

| Metric | Idle | Pressure | Pinned |
|--------|------|----------|--------|
| Expand success rate | (pending) | (pending) | (pending) |
| Shrink success rate | (pending) | (pending) | (pending) |
| Mean expand latency | (pending) ms | (pending) ms | (pending) ms |
| Mean shrink latency | (pending) ms | (pending) ms | (pending) ms |
| Min shrink achieved | (pending) MB | (pending) MB | (pending) MB |
| Guest kernel panics | (pending) | (pending) | (pending) |
| CH crashes | (pending) | (pending) | (pending) |

### 2.6 CH API Endpoint

```
PUT /api/v1/vm.resize
Body: {"desired_ram": <bytes>}
```

Equivalent CLI: `ch-remote --api-socket <path> resize --memory <bytes>`

The `vm.resize` endpoint accepts raw byte values for `desired_ram`. virtio-mem handles the block-level plug/unplug internally; the API caller only specifies the target size.

---

## 3. Disk Resize

### 3.1 Overview

Two distinct disk operations are tested:

1. **Resize existing disk** (`vm.resize-disk`): Expand the capacity of an already-attached virtio-blk device at runtime, then grow the guest filesystem to fill the new space.
2. **Hot-add new disk** (`vm.add-disk`): Attach a brand-new block device to a running VM.

### 3.2 Prerequisites

Create the disk images before booting the VM:

```bash
# Create the root disk (already done by build-rootfs.sh)
# For testing, ensure it's an ext4 raw image
qemu-img create -f raw target/guest/rootfs.ext4 2G
mkfs.ext4 target/guest/rootfs.ext4
# Populate rootfs into the ext4 image...

# Create a spare disk for hot-add testing
qemu-img create -f raw /tmp/extra.raw 1G
```

### 3.3 Path A: Resize Existing Disk

Boot the VM with a virtio-blk root disk:

```bash
# Start VM
/tmp/cloud-hypervisor-static \
    --api-socket /tmp/ch-api.sock \
    --kernel target/guest/vmlinux.bin \
    --cmdline "console=ttyS0 root=/dev/vda rw" \
    --cpus boot=2,max=8 \
    --memory size=512M \
    --disk path=target/guest/rootfs.ext4,id=root \
    --serial tty \
    --console off
```

From the guest, verify the current disk size:

```bash
# Inside guest
lsblk
# Expected: vda with size matching the image (e.g., 2G)
df -h /
# Expected: filesystem size matches the block device
```

Now expand the disk image and the block device:

```bash
# Step 1: Expand the backing image on the host
qemu-img resize target/guest/rootfs.ext4 20G

# Step 2: Notify CH to update the virtio-blk device size
ch-remote --api-socket /tmp/ch-api.sock resize-disk --id root --size 20G
# 20G in bytes: 20 * 1024^3 = 21474836480

# Step 3: In guest, verify the block device sees the new size
lsblk
# Expected: vda now shows 20G

# Step 4: Online resize the ext4 filesystem
resize2fs /dev/vda
# Or: resize2fs /dev/root (if using partition labels)

# Step 5: Verify filesystem size
df -h /
# Expected: / now shows ~20G total
```

### 3.4 Path B: Hot-Add New Disk

With the VM still running from the previous test (or a fresh boot), attach a new disk:

```bash
# On host: hot-add a new raw disk
ch-remote --api-socket /tmp/ch-api.sock add-disk path=/tmp/extra.raw,id=extra
```

Verify from the guest that the new device appears:

```bash
# Inside guest
lsblk
# Expected: new device /dev/vdb appears (vda is root, vdb is the hot-added disk)
cat /proc/partitions
# Expected: vdb listed with its size

# Optional: format and mount the new disk
mkfs.ext4 /dev/vdb
mkdir -p /mnt/extra
mount /dev/vdb /mnt/extra
df -h /mnt/extra
```

### 3.5 Expected Outcome

| Operation | Expected Result | Notes |
|-----------|----------------|-------|
| `resize-disk` | Block device size updates without guest reboot | virtio-blk device capacity reported via config change interrupt |
| `resize2fs` (in guest) | Filesystem grows to fill new capacity | ext4 online resize is a standard kernel feature; no unmount needed |
| `add-disk` | New `/dev/vdb` block device appears within seconds | virtio-blk hotplug via PCI; guest kernel detects and creates device node |

### 3.6 CH API Endpoints

```
PUT /api/v1/vm.resize-disk
Body: {"id": "<disk-id>", "size": <bytes>}
```

Equivalent CLI: `ch-remote --api-socket <path> resize-disk --id <id> --size <bytes>`

```
PUT /api/v1/vm.add-disk
Body: {"path": "<path>", "id": "<disk-id>"}
```

Equivalent CLI: `ch-remote --api-socket <path> add-disk path=<path>,id=<id>`

---

## 4. CH API Endpoint Reference

All dynamic resource operations use the HTTP API over a Unix domain socket.

| Operation | Method | Endpoint | Key Parameters |
|-----------|--------|----------|---------------|
| CPU resize | PUT | `/api/v1/vm.resize` | `desired_vcpus` |
| Memory resize | PUT | `/api/v1/vm.resize` | `desired_ram` (bytes) |
| Disk resize | PUT | `/api/v1/vm.resize-disk` | `id`, `size` (bytes) |
| Disk hot-add | PUT | `/api/v1/vm.add-disk` | `path`, `id` |

The CH API socket is a standard HTTP/1.1 server over Unix domain sockets. The `ch-remote` CLI tool is the canonical client shipped with Cloud Hypervisor. In M1, the `ch-client` Rust crate will provide a programmatic Rust interface for these same endpoints.

---

## 5. Appendix A: mlock Helper

For the pinned memory test state, a small C helper is useful to lock anonymous memory:

```c
/* mlock-and-sleep.c: lock N bytes and sleep forever */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "Usage: %s <size>\n", argv[0]);
        return 1;
    }
    size_t sz;
    if (strcmp(argv[1], "256M") == 0) sz = 256UL * 1024 * 1024;
    else if (strcmp(argv[1], "512M") == 0) sz = 512UL * 1024 * 1024;
    else if (strcmp(argv[1], "1G") == 0) sz = 1024UL * 1024 * 1024;
    else sz = atol(argv[1]);

    void *p = mmap(NULL, sz, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_LOCKED, -1, 0);
    if (p == MAP_FAILED) { perror("mmap"); return 1; }
    memset(p, 0xAB, sz); /* fault in all pages */
    printf("Locked %zu bytes at %p. Sleeping...\n", sz, p);
    fflush(stdout);
    pause();
    return 0;
}
```

Compile statically for the guest rootfs:

```bash
gcc -static -o target/guest/rootfs/bin/mlock-and-sleep mlock-and-sleep.c
```

---

## Appendix B: Troubleshooting Notes

1. **API socket not responding:** Verify the socket path matches the `--api-socket` flag. CH creates the socket only after the VM boots.
2. **Resize returns "not supported":** Check that kernel config includes `CONFIG_ACPI_HOTPLUG_CPU` (CPU), `CONFIG_VIRTIO_MEM` (memory), and that CH was started with `max` values above `boot` values for CPUs and `hotplug_method=virtio-mem` for memory.
3. **Shrink fails on all attempts:** virtio-mem requires at least one 2MB block to be free in the pluggable region. If the guest has no free memory or all pages are pinned, shrink will return the minimum achievable size rather than failing silently.
4. **stress-ng workers don't scale with CPU count:** stress-ng `--cpu 0` starts one stressor per online CPU at launch time. Use `--cpu N` with explicit count, or restart stress-ng after resize to match the new CPU count.
5. **Disk resize-disk fails:** The backing image must first be expanded on the host (`qemu-img resize` or equivalent). CH validates that the requested size does not exceed the backing file size.
