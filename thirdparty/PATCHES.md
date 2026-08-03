# Cloud Hypervisor Local Patches

This file registers every local patch applied to the Cloud Hypervisor fork.
Each entry must include: purpose, upstream status, rebase risk.

## Patch Registry

| # | Description | Purpose | Upstream Status | Rebase Risk | Date |
|---|-------------|---------|-----------------|-------------|------|
|   |             |         |                 |             |      |

Currently: ZERO local patches. All configuration is done via CH command-line
flags and API parameters.

# Sandlock Local Patches

Local patches applied to the vendored sandlock build (upstream tag v0.8.5,
built as a static musl binary for the Alpine/musl guest images).

## Patch Registry

| # | Description | Purpose | Upstream Status | Rebase Risk | Date |
|---|-------------|---------|-----------------|-------------|------|
| 1 | `sandlock-v0.8.5-musl.patch` | Fix musl libc signature divergences so sandlock compiles for `x86_64-unknown-linux-musl`: `libc::ioctl` request is `c_int` on musl vs `c_ulong` on glibc (SECCOMP_IOCTL_NOTIF_* and UFFDIO_* constants now cast with `as _`), `libc::ptrace` request is `c_int` on musl vs `c_uint` on glibc (PTRACE_* casts changed to `as _`; `ptrace_resume` takes `c_uint` with `as _` at the call and its PTRACE_CONT argument is cast explicitly since the constant's type differs per target), `msghdr.msg_controllen` is `socklen_t` on musl vs `size_t` on glibc (`c.len() as _`). Type-only changes; no logic touched; compiles on both musl and glibc. | Not upstreamed; upstream CI builds gnu-only. Candidate for upstreaming. | Low: 24 lines across 8 files, all one-line type casts | 2026-07-30 |

| 2 | `sandlock-v0.8.5-denyfd.patch` | Structured policy-denial channel: the seccomp supervisor writes one JSON line per deny response (`{"syscall":"<name>","errno":<n>}` for EACCES/EPERM responses, `{"syscall":"<name>","killed":<sig>}` for supervisor kills) to the fd named by `SANDBOX_DENY_FD`, so supervising runtimes (Terrarium guest-proxy) classify denials structurally instead of sniffing child stderr text. Best-effort writes; a missing/closed fd never affects execution. Coverage boundary (documented in docs/design/policy-model.md): only supervisor-observed denials (seccomp-notify paths) are reported; static Landlock fs denials are kernel-enforced and invisible to the supervisor. | Not upstreamed; Terrarium-specific integration. | Low: 129 lines in one file (`crates/sandlock-core/src/seccomp/notif.rs`), additive — the deny record is advisory, no response path changes. | 2026-08-02 |

| 3 | `sandlock-v0.8.5-fsgrant.patch` | Static fs-grant mirror (fs observability): in non-chroot mode the supervisor resolves open/exec-family syscalls that would otherwise Continue to the kernel, and denies (`EACCES`, recorded by the denyfd channel) any access outside the static `fs_readable`/`fs_writable` grants — the same verdict Landlock would make, surfaced so default-policy fs denials are observable. Continue-only call site keeps dedicated handlers (procfs shims, COW, chroot) winning; Landlock remains the enforcement backstop. Coverage boundary: open/openat/openat2 + execve/execveat; link/rename/other path syscalls and chroot mode keep the kernel path. | Not upstreamed; Terrarium-specific integration. | Low–medium: ~200 lines in one file (`crates/sandlock-core/src/seccomp/notif.rs`), additive gate + shared verdict reuse; 5 new unit tests. | 2026-08-03 |
## Rebuild Procedure

```sh
git clone https://github.com/multikernel/sandlock sandlock-src
cd sandlock-src && git checkout go/v0.8.5
git apply /home/liujinyao/2606/Terrarium/thirdparty/sandlock-v0.8.5-musl.patch
export CC_x86_64_unknown_linux_musl=$HOME/.cache/terrarium/toolchain/x86_64-linux-musl-cross/bin/x86_64-linux-musl-gcc
cargo build --release --target x86_64-unknown-linux-musl -p sandlock-cli
# artifact: target/x86_64-unknown-linux-musl/release/sandlock (static-pie)
```
