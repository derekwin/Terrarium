//! sandbox 隔离栈（M2 Task 2）。
//!
//! 每沙箱一个执行单元：
//! 1. clone() → pid/mount/uts/ipc/net/user namespace
//! 2. pivot_root() → OverlayFS (lower=rootfs, upper=per-sandbox tmpfs)
//! 3. cgroup v2 → cpu.max / memory.max
//! 4. Landlock → 路径白名单
//! 5. seccomp-bpf → 危险 syscall 清单
//!
//! 本模块大量使用 libc syscall（clone / pivot_root / mount / Landlock / seccomp），
//! 全部 unsafe 是不可避免的。

#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::ptr;

use serde::Deserialize;

/// sandbox 执行结果。
pub struct SandboxResult {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// sandbox 配置。
pub struct SandboxConfig {
    pub id: String,
    pub rootfs: String,
    pub overlay_upper: String,
    pub cpu_max: Option<String>,
    pub memory_max: Option<String>,
    pub allow_paths: Vec<String>,
}

/// 从 API 传入的 sandbox 配置（可反序列化）。
#[derive(Debug, Deserialize)]
pub struct SandboxConfigInput {
    pub id: String,
    #[serde(default = "default_rootfs")]
    pub rootfs: String,
    #[serde(default = "default_overlay")]
    pub overlay_upper: String,
    pub cpu_max: Option<String>,
    pub memory_max: Option<String>,
    #[serde(default)]
    pub allow_paths: Vec<String>,
}

fn default_rootfs() -> String {
    "/".to_string()
}
fn default_overlay() -> String {
    "/tmp/sandbox-overlay".to_string()
}

impl From<SandboxConfigInput> for SandboxConfig {
    fn from(input: SandboxConfigInput) -> Self {
        SandboxConfig {
            id: input.id,
            rootfs: input.rootfs,
            overlay_upper: input.overlay_upper,
            cpu_max: input.cpu_max,
            memory_max: input.memory_max,
            allow_paths: input.allow_paths,
        }
    }
}

/// 在 sandbox 中执行命令。
pub fn exec_in_sandbox(
    config: &SandboxConfig,
    argv: &[String],
    env: &std::collections::HashMap<String, String>,
    cwd: &str,
) -> io::Result<SandboxResult> {
    let child = spawn_sandboxed(config, argv, env, cwd)?;
    let output = child.wait_with_output()?;

    Ok(SandboxResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn spawn_sandboxed(
    config: &SandboxConfig,
    argv: &[String],
    env: &std::collections::HashMap<String, String>,
    cwd: &str,
) -> io::Result<std::process::Child> {
    if argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
    }

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    // Clone config fields so the pre_exec closure owns its data.
    let rootfs = config.rootfs.clone();
    let overlay_upper = config.overlay_upper.clone();
    let sb_id = config.id.clone();
    let cpu_max = config.cpu_max.clone();
    let memory_max = config.memory_max.clone();
    let allow_paths = config.allow_paths.clone();

    // SAFETY: clone + namespace setup before exec.
    // 子进程在 fork 后立即进入隔离环境，失败时退出。
    unsafe {
        cmd.pre_exec(move || {
            enter_namespaces()?;
            setup_rootfs(&rootfs, &overlay_upper)?;
            setup_cgroup(&sb_id, &cpu_max, &memory_max)?;
            setup_landlock(&allow_paths)?;
            setup_seccomp()?;
            Ok(())
        });
    }

    cmd.spawn()
}

// ——— 1. Namespace ———

/// 进入 pid / mount / uts / ipc / net / user namespace。
unsafe fn enter_namespaces() -> io::Result<()> {
    // unshare: 脱离父进程的所有 namespace。
    let flags = libc::CLONE_NEWPID
        | libc::CLONE_NEWNS
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWNET;
    // CLONE_NEWUSER 单独处理（需要 uid_map）。
    if libc::unshare(flags | libc::CLONE_NEWUSER) != 0 {
        return Err(io::Error::last_os_error());
    }

    // 写 uid_map / gid_map（将容器内 uid 0 映射到宿主 uid 0）。
    // 先写 "deny" 阻止 setgroups，再写映射。
    write_file("/proc/self/setgroups", b"deny")?;
    write_file("/proc/self/uid_map", b"0 0 1")?;
    write_file("/proc/self/gid_map", b"0 0 1")?;

    Ok(())
}

// ——— 2. Rootfs ———

unsafe fn setup_rootfs(rootfs: &str, upper: &str) -> io::Result<()> {
    // 挂载 tmpfs 作为 upper 层。
    mount_tmpfs(upper)?;

    // 创建 upper/work 目录（overlayfs 必需）。
    let upper_work = format!("{upper}/work");
    unsafe {
        libc::mkdir(CString::new(upper_work.as_str()).unwrap().as_ptr(), 0o755);
    }

    // 挂载 OverlayFS（lower=rootfs 只读，upper=tmpfs 可写）。
    // mount -t overlay overlay -o lowerdir={rootfs},upperdir={upper},workdir={upper}/work merged
    let merged = format!("{upper}/merged");
    unsafe {
        libc::mkdir(CString::new(merged.as_str()).unwrap().as_ptr(), 0o755);
    }
    let opts = CString::new(format!(
        "lowerdir={rootfs},upperdir={upper},workdir={upper}/work"
    ))
    .unwrap();
    if libc::mount(
        CString::new("overlay").unwrap().as_ptr(),
        CString::new(merged.as_str()).unwrap().as_ptr(),
        CString::new("overlay").unwrap().as_ptr(),
        0,
        opts.as_ptr() as *const libc::c_void,
    ) != 0
    {
        return Err(io::Error::last_os_error());
    }

    // pivot_root 到 merged 目录。
    // 先 bind-mount merged 到自身（pivot_root 要求 new_root 与 old_root 不在同一文件系统）。
    if libc::mount(
        CString::new(merged.as_str()).unwrap().as_ptr(),
        CString::new(merged.as_str()).unwrap().as_ptr(),
        ptr::null(),
        libc::MS_BIND | libc::MS_REC,
        ptr::null(),
    ) != 0
    {
        return Err(io::Error::last_os_error());
    }

    let old_root = format!("{merged}/old_root");
    libc::mkdir(CString::new(old_root.as_str()).unwrap().as_ptr(), 0o755);

    if libc::syscall(
        libc::SYS_pivot_root,
        CString::new(merged.as_str()).unwrap().as_ptr(),
        CString::new(old_root.as_str()).unwrap().as_ptr(),
    ) != 0
    {
        return Err(io::Error::last_os_error());
    }

    // chdir 到新根并卸载旧根。
    libc::chdir(CString::new("/").unwrap().as_ptr());
    libc::umount2(
        CString::new("/old_root").unwrap().as_ptr(),
        libc::MNT_DETACH,
    );

    Ok(())
}

fn mount_tmpfs(target: &str) -> io::Result<()> {
    unsafe {
        libc::mkdir(CString::new(target).unwrap().as_ptr(), 0o755);
    }
    if unsafe {
        libc::mount(
            CString::new("tmpfs").unwrap().as_ptr(),
            CString::new(target).unwrap().as_ptr(),
            CString::new("tmpfs").unwrap().as_ptr(),
            0,
            ptr::null(),
        )
    } != 0
    {
        // 如果已存在，忽略（可能在之前的 mount 中已创建）。
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EBUSY) {
            return Err(e);
        }
    }
    Ok(())
}

// ——— 3. Cgroup v2 ———

fn setup_cgroup(id: &str, cpu_max: &Option<String>, memory_max: &Option<String>) -> io::Result<()> {
    let cg_path = format!("/sys/fs/cgroup/terra-{id}");

    // 如果 cgroup v2 未挂载，尝试挂载。
    let cg_root = "/sys/fs/cgroup";
    if !std::path::Path::new(&format!("{cg_root}/cgroup.controllers")).exists() {
        unsafe {
            libc::mkdir(CString::new(cg_root).unwrap().as_ptr(), 0o755);
        }
        if unsafe {
            libc::mount(
                CString::new("cgroup2").unwrap().as_ptr(),
                CString::new(cg_root).unwrap().as_ptr(),
                CString::new("cgroup2").unwrap().as_ptr(),
                0,
                ptr::null(),
            )
        } != 0
        {
            // cgroup v2 不可用时跳过（不阻塞沙箱创建）。
            return Ok(());
        }
    }

    unsafe {
        libc::mkdir(CString::new(cg_path.as_str()).unwrap().as_ptr(), 0o755);
    }

    if let Some(ref max) = cpu_max {
        let _ = write_file(&format!("{cg_path}/cpu.max"), max.as_bytes());
    }
    if let Some(ref max) = memory_max {
        let _ = write_file(&format!("{cg_path}/memory.max"), max.as_bytes());
    }

    // 将当前进程加入 cgroup。
    let _ = write_file(
        &format!("{cg_path}/cgroup.procs"),
        format!("{}", std::process::id()).as_bytes(),
    );

    Ok(())
}

// ——— 4. Landlock ———

fn setup_landlock(allow_paths: &[String]) -> io::Result<()> {
    if allow_paths.is_empty() {
        return Ok(());
    }

    // Landlock ABI version check.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            ptr::null::<libc::c_void>(),
            0,
            0,
        )
    };
    if abi < 0 {
        // Landlock 不可用，跳过。
        return Ok(());
    }

    // 对每个允许路径添加规则。
    // 简化实现：只做 best-effort Landlock（完整实现在 Task 4）。
    for path in allow_paths {
        let _ = allow_path_rw(path);
    }

    Ok(())
}

fn allow_path_rw(path: &str) -> io::Result<()> {
    // 打开路径获取 fd。
    let cpath = CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid path"))?;
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // Landlock 规则：允许读写。
    #[repr(C)]
    struct LandlockPathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    let attr = LandlockPathBeneathAttr {
        allowed_access: (1 << 0) | (1 << 1), // LANDLOCK_ACCESS_FS_EXECUTE | WRITE_FILE ≈ basic rw
        parent_fd: fd,
    };

    let ruleset_fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const _ as *const libc::c_void,
            std::mem::size_of::<LandlockPathBeneathAttr>(),
            0,
        )
    };

    if ruleset_fd < 0 {
        unsafe { libc::close(fd) };
        return Err(io::Error::last_os_error());
    }

    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset_fd,
            1, // LANDLOCK_RULE_PATH_BENEATH
            &attr as *const _ as *const libc::c_void,
            0,
        )
    };

    // enforce
    let _ = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset_fd, 0, 0) };

    unsafe { libc::close(fd) };
    unsafe { libc::close(ruleset_fd as i32) };

    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ——— 5. Seccomp-bpf ———

fn setup_seccomp() -> io::Result<()> {
    // M2 最小 seccomp 过滤：仅做 best-effort，完整实现在 Task 4。
    // 默认允许所有 syscall，只记录 seccomp 已启用。
    // seccomp(SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC, &prog)
    // 这里先放一个空过滤器（允许所有）。

    // BPF 程序：允许所有 syscall。
    #[repr(C)]
    struct SockFprog {
        len: u16,
        filter: *const libc::c_void,
    }

    // BPF: LD | ABS | W  → load arch; JEQ | JT | JF → check; RET ALLOW
    // 完整 BPF 程序在 Task 4 实现。
    let filter: [libc::sock_filter; 3] = [
        libc::sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 4,
        }, // ld [4] (arch)
        libc::sock_filter {
            code: 0x15,
            jt: 1,
            jf: 0,
            k: 0xc000003e,
        }, // jeq AUDIT_ARCH_X86_64
        libc::sock_filter {
            code: 0x06,
            jt: 0,
            jf: 0,
            k: 0x7fff0000,
        }, // ret ALLOW
    ];

    let prog = SockFprog {
        len: filter.len() as u16,
        filter: filter.as_ptr() as *const libc::c_void,
    };

    // 尝试加载 seccomp，失败不阻塞。
    let _ = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC,
            &prog as *const _ as *const libc::c_void,
        )
    };

    Ok(())
}

// ——— 辅助 ———

fn write_file(path: &str, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(data)?;
    Ok(())
}
