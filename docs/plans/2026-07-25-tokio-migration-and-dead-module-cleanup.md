# Tokio Migration & Dead Module Cleanup — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Migrate Terrarium Engine from std-thread to tokio async runtime, and delete all 8 unconnected dead modules with their caller code.

**Architecture:** Adapter traits keep `async fn` signatures (unchanged). CH adapter's `ChClient` switches from blocking `UnixStream` to `tokio::net::UnixStream`. Engine daemon switches from `std::thread::spawn` + `std::sync::Mutex` to `tokio::spawn` + `tokio::sync::Mutex`. Engine `VmHandle` and `VmManager` methods that call async ChClient become async. Dead modules (scheduler, pool, placement, metering, registry, cgroup, files, tools) deleted entirely.

**Tech Stack:** Rust 2021, tokio (full features for engine, net+time for adapter), async-trait (existing)

**Dependency order:** Phase 0 → Phase 1 → Phase 2. Within Phase 2: adapter first, then engine.

---

## Phase 0: 3 Local P0 Fixes (No Architecture Dependency)

### Task 0.1: Fix CH stderr pipe hang — engine side

**Files:** `crates/engine/src/vm.rs`

**What:** Replace `Stdio::piped()` with `Stdio::null()` at line 128. Remove `read_child_stderr` function (lines 69-78) and its two call sites. Remove `use std::io::Read` (line 3) if unused.

**Verify:** `cargo check -p engine`

### Task 0.2: Fix CH stderr pipe hang — adapter side

**Files:** `crates/adapter/cloud-hypervisor/src/lib.rs`

**What:** Replace `Stdio::piped()` with `Stdio::null()` at line 94.

**Verify:** `cargo check -p adapter-cloud-hypervisor`

### Task 0.3: Fix is_alive zombie detection

**Files:** `crates/engine/src/vm.rs`

**What:** Replace `/proc/<pid>` existence check (lines 328-331) with `child.try_wait()`. Also fix Drop (line 336) to use inline `try_wait()` instead of `is_alive()` to avoid double-consuming exit status.

```rust
pub fn is_alive(&self) -> bool {
    matches!(self.child.try_wait(), Ok(None))
}
```

**Verify:** `cargo check -p engine`

### Task 0.4: Socket chmod 0600

**Files:** `crates/engine/src/daemon.rs`

**What:** After `UnixListener::bind(socket_path)`, add:
```rust
use std::os::unix::fs::PermissionsExt;
std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
```

**Verify:** `cargo check -p engine`

### Task 0.5: Commit Phase 0

```bash
git add crates/engine/src/vm.rs crates/engine/src/daemon.rs crates/adapter/cloud-hypervisor/src/lib.rs
git commit -m "fix: P0 fixes — stderr drain, zombie detection, socket chmod"
```

---

## Phase 1: Delete Dead Modules

### Task 1.1: Delete 8 dead module files

**Files (delete):**
- `crates/engine/src/scheduler.rs`
- `crates/engine/src/pool.rs`
- `crates/engine/src/placement.rs`
- `crates/engine/src/metering.rs`
- `crates/engine/src/registry.rs`
- `crates/engine/src/cgroup.rs`
- `crates/engine/src/files.rs`
- `crates/engine/src/tools.rs`

**Verify:** `ls crates/engine/src/{scheduler,pool,placement,metering,registry,cgroup,files,tools}.rs 2>&1` — all "No such file"

### Task 1.2: Remove mod declarations from main.rs

**Files:** `crates/engine/src/main.rs`

**What:** Remove lines 6, 10, 12, 13, 14, 15, 16, 18 (mod declarations for dead modules). Keep: `cli`, `commands`, `daemon`, `manager`, `spec`, `vm`.

**Verify:** `cargo check -p engine` — expect errors about unresolved imports (fixed next)

### Task 1.3: Remove pool and file commands from commands.rs

**Files:** `crates/engine/src/commands.rs`

**What:**
1. Delete `pool_execute` and pool command functions (lines 317-371)
2. Delete file command functions (`cmd_file_read`, `cmd_file_write`, `cmd_file_list`, lines 374-412)
3. Remove dispatch entries: `"file_read"`, `"file_write"`, `"file_list"` from `execute()` match
4. Remove pool fields from `Command` struct (lines 50-52): `pool_size`
5. Remove file fields from `Command` struct (lines 55-58): `file_path`, `file_content`
6. Fix `cmd_restore` to return `Response::err("restore not implemented")` instead of fake spawn

**Verify:** `cargo check -p engine`

### Task 1.4: Remove pool code from daemon.rs

**Files:** `crates/engine/src/daemon.rs`

**What:**
1. Remove `use crate::pool::WarmPool` (line 10)
2. Remove `WarmPool::new()` instantiation (line 21)
3. Remove pool clone (line 27)
4. Remove pool dispatch block (lines 73-79)
5. Simplify `handle_client` signature — remove `pools` parameter
6. Apply chmod fix from Task 0.4 (if not already done in Phase 0)

**Verify:** `cargo check -p engine` — clean compilation expected

### Task 1.5: Remove `futures` dependency

**Files:** `crates/engine/Cargo.toml`

**What:** Remove `futures = "0.3"` — was only used by deleted `files.rs`.

**Verify:** `cargo check -p engine`

### Task 1.6: Commit Phase 1

```bash
git add -A
git commit -m "refactor(engine): delete dead modules and their caller code

Remove scheduler, pool, placement, metering, registry, cgroup, files, tools.
Remove pool commands and file read/write/list commands (P0 security holes).
Fix cmd_restore to return 'not implemented' instead of faking success.
Remove unused futures dependency."
```

---

## Phase 2: Tokio Migration

### Task 2.1: Add tokio dependencies

**Files:** `crates/engine/Cargo.toml`, `crates/adapter/cloud-hypervisor/Cargo.toml`

**What:** 
- engine: `tokio = { version = "1", features = ["full"] }`
- adapter-cloud-hypervisor: `tokio = { version = "1", features = ["net", "time"] }`

**Verify:** `cargo check -p engine -p adapter-cloud-hypervisor`

### Task 2.2: Convert ChClient to async

**Files:** `crates/adapter/cloud-hypervisor/src/client.rs`, `crates/adapter/cloud-hypervisor/src/error.rs`

**What:** Rewrite `ChClient` to use `tokio::net::UnixStream` instead of `std::os::unix::net::UnixStream`. All methods become `async fn`. Key changes:
- `UnixStream::connect` → `tokio::net::UnixStream::connect`
- `set_read_timeout/set_write_timeout` → wrap in `tokio::time::timeout(self.timeout, ...)`
- `write_all` / `read_line` / `read_exact` → tokio async equivalents with `.await`
- HTTP response parsing remains the same (sync string manipulation)
- Add `Timeout` variant to `ClientError` if needed

All public API methods (`vm_create`, `vm_boot`, `vm_shutdown`, `vm_info`, `vm_resize`, `vm_resize_disk`, `vm_add_disk`, `vm_pause`, `vm_resume`, `vm_snapshot`, `vm_restore`) become `async fn` and add `.await` to `self.request(...)` calls.

**Verify:** `cargo check -p adapter-cloud-hypervisor` — expect errors in lib.rs (ChVmHandle calls sync), fixed next

### Task 2.3: Convert ChAdapter/ChVmHandle to truly async

**Files:** `crates/adapter/cloud-hypervisor/src/lib.rs`

**What:**
1. Replace `use std::thread;` / `std::time::{Duration, Instant}` with `use tokio::time::{sleep, Duration, Instant};`
2. All `thread::sleep(...)` → `sleep(...).await`
3. `ChVmHandle::spawn` → `async fn spawn`
4. All `self.client.*()` calls in `impl VmHandle for ChVmHandle` add `.await`
5. `ChAdapter::create` → add `.await` to `ChVmHandle::spawn(spec)`
6. Fix `Drop` for `ChVmHandle` — can't call async in Drop. Do best-effort kill+wait:
   ```rust
   impl Drop for ChVmHandle {
       fn drop(&mut self) {
           let _ = self.child.kill();
           let _ = self.child.wait();
           let _ = std::fs::remove_file(format!("/tmp/terra-{}.sock", self.name));
       }
   }
   ```

**Verify:** `cargo check -p adapter-cloud-hypervisor` — clean. `cargo check -p engine` — errors expected (engine calls sync methods).

### Task 2.4: Convert engine daemon to tokio

**Files:** `crates/engine/src/main.rs`, `crates/engine/src/daemon.rs`

**What (main.rs):**
1. Replace `fn main()` with `#[tokio::main] async fn main()`
2. `daemon::run(&socket).expect(...)` → `daemon::run(&socket).await.expect(...)`

**What (daemon.rs):**
1. Replace `std::os::unix::net::{UnixListener, UnixStream}` → `tokio::net::{UnixListener, UnixStream}`
2. Replace `std::sync::Mutex` → `tokio::sync::Mutex`
3. Replace `std::thread::spawn` → `tokio::spawn`
4. Replace `BufReader` / `BufRead` with `tokio::io::BufReader` / `AsyncBufReadExt`
5. Replace `writeln!(&stream, ...)` with `writer_half.write_all(...).await`
6. `run()` → `async fn run()`
7. `handle_client()` → `async fn handle_client()`
8. `listener.incoming()` → `listener.accept().await` in loop
9. `manager.lock().unwrap()` → `manager.lock().await`
10. `reader.read_line(&mut line)` → `reader.read_line(&mut line).await`

**Verify:** `cargo check -p engine` — errors in commands.rs/manager.rs/vm.rs (sync→async mismatch), fixed next.

### Task 2.5: Make engine VmHandle async

**Files:** `crates/engine/src/vm.rs`

**What:** Make methods that call async ChClient methods into `async fn`:

| Method | Becomes | Reason |
|--------|---------|--------|
| `spawn` | `async fn` | Calls `client.vm_info()` in poll loop |
| `info` | `async fn` | Calls `client.vm_info()` |
| `resize_vcpus` | `async fn` | Calls `client.vm_resize()` |
| `resize_memory` | `async fn` | Calls `client.vm_resize()` |
| `shutdown` | `async fn` | Calls `client.vm_shutdown()` |
| `destroy` | `async fn` | Calls `self.shutdown().await` |
| `kill` | stays `fn` | Only `child.kill()` + `child.wait()` (sync) |
| `is_alive` | stays `fn` | `child.try_wait()` (sync) |
| `snapshot_vm` | `async fn` | Calls `client.vm_snapshot()` |

Replace `thread::sleep`/`std::time` with `tokio::time::sleep`/`tokio::time::Duration`/`tokio::time::Instant`.

**Drop** stays sync — already has kill+wait fallback, no ChClient calls.

**Verify:** `cargo check -p engine` — errors in manager.rs (VmManager calls sync), fixed next.

### Task 2.6: Make VmManager and commands async

**Files:** `crates/engine/src/manager.rs`, `crates/engine/src/commands.rs`

**What (manager.rs):**
Make methods async where they call VmHandle async methods:

| Method | Becomes |
|--------|---------|
| `spawn` | `async fn` |
| `get` | stays `fn` |
| `list_names` | stays `fn` |
| `shutdown` | `async fn` |
| `kill` | stays `fn` |
| `destroy` | `async fn` |
| `reap_dead` | stays `fn` |
| `shutdown_all` | `async fn` |

`get_mut` and `list` retain `#[allow(dead_code)]` but drop the attribute once callers exist. For now, remove `#[allow(dead_code)]` since AGENTS.md §7 forbids it — either wire them or delete them. Delete `get_mut`, keep `list` used in tests.

**What (commands.rs):**
Make command handlers async where they call VmManager async methods. All commands that call `mgr.spawn()`, `mgr.shutdown()`, `mgr.destroy()`, or `vm.info()` / `vm.resize_*()` become async. `execute()` becomes `async fn`.

```rust
pub async fn execute(mgr: &mut VmManager, cmd: Command) -> Response {
    match cmd.command.as_str() {
        "create" => cmd_create(mgr, cmd).await,
        "list" => cmd_list(mgr).await,
        "info" => cmd_info(mgr, cmd).await,
        "resize" => cmd_resize(mgr, cmd).await,
        "shutdown" => cmd_shutdown(mgr, cmd).await,
        "kill" => cmd_kill(mgr, cmd),
        "destroy" => cmd_destroy(mgr, cmd).await,
        "snapshot" => cmd_snapshot(mgr, cmd).await,
        "restore" => cmd_restore(mgr, cmd),
        _ => Response::err(format!("Unknown command: {}", cmd.command)),
    }
}
```

Update `handle_client` in daemon.rs: `let response = execute(&mut mgr, cmd).await;`

**Verify:** `cargo check -p engine` — should compile clean.

### Task 2.7: Full workspace check

```bash
cargo check --all
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

Fix any remaining issues. Common problems:
- Unused imports (old `std::thread`, `std::time`, `std::io::Read`)
- `#[allow(dead_code)]` on fields/methods that were dead-module only
- Test files that reference deleted modules

### Task 2.8: Commit Phase 2

```bash
git add -A
git commit -m "refactor: migrate to tokio async runtime

Adapter layer:
- ChClient: blocking UnixStream → tokio::net::UnixStream, all methods async
- ChVmHandle: thread::sleep → tokio::time::sleep, spawn async, Drop kill+wait
- ChAdapter::create now truly async

Engine daemon:
- std::thread::spawn → tokio::spawn
- std::sync::Mutex → tokio::sync::Mutex
- UnixListener/UnixStream → tokio::net equivalents
- #[tokio::main] entry point

Engine VmHandle/Manager:
- VmHandle spawn/info/resize/shutdown/destroy become async
- VmManager spawn/shutdown/destroy become async
- Command handlers and execute() become async"
```

---

## Phase 3: Verification

### Task 3.1: LSP diagnostics on all changed files

```bash
# Check all changed files for errors
cargo check --all 2>&1
```

### Task 3.2: Run full CI suite

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

### Task 3.3: Manual review of key files

Read each changed file to verify:
1. No leftover `thread::sleep` in async contexts
2. No `.unwrap()` on tokio Mutex (still present but now less dangerous since tokio Mutex doesn't poison)
3. All `#[allow(dead_code)]` removed (per AGENTS.md §7)

---

## Summary

| Phase | Tasks | Description |
|-------|-------|-------------|
| 0 | 5 | 3 P0 fixes (stderr ×2, zombie, chmod) + commit |
| 1 | 6 | Delete 8 dead modules + caller code + commit |
| 2 | 8 | Full tokio migration (adapter → engine → commands) + commit |
| 3 | 3 | Verification (lint, clippy, test, manual review) |

**Total: 22 tasks, estimated 2-3 hours.**
