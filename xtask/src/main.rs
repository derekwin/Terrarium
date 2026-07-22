//! xtask：项目构建辅助任务入口。
//!
//! `cargo xtask kernel [--version X.Y.Z]`：
//! 一键下载上游稳定版内核，应用最小裁剪配置编译 bzImage，
//! 并构建 initramfs（静态 busybox，`/init` 挂载 devtmpfs 后 exec `/bin/sh`）。
//! 产物放 `target/guest/`（不进 git）：
//!   - `target/guest/bzImage`
//!   - `target/guest/initramfs.cpio.gz`

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, Subcommand};

/// 默认内核版本（6.12 LTS 的已存在小版本；可用 --version 覆盖）
const DEFAULT_KERNEL_VERSION: &str = "6.12.41";
/// 默认 busybox 版本
const BUSYBOX_VERSION: &str = "1.36.1";

/// 内核配置片段：在 x86_64 defconfig 之上合入。
/// 基线要求见 AGENTS.md Task 1：串口控制台、virtio-mmio、devtmpfs、initrd；
/// 其余能删就删（关模块、关调试信息、关 ORC 栈回溯以免依赖 libelf-dev）。
const KERNEL_CONFIG_FRAGMENT: &str = r#"
CONFIG_MODULES=n
CONFIG_DEBUG_INFO=n
CONFIG_STACK_VALIDATION=n
CONFIG_UNWINDER_ORC=n
CONFIG_UNWINDER_FRAME_POINTER=y
CONFIG_SERIAL_8250=y
CONFIG_SERIAL_8250_CONSOLE=y
CONFIG_TTY=y
CONFIG_PRINTK=y
CONFIG_BLK_DEV_INITRD=y
CONFIG_DEVTMPFS=y
CONFIG_DEVTMPFS_MOUNT=y
CONFIG_VIRTIO=y
CONFIG_VIRTIO_MMIO=y
CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y
# 无 RTC 设备：M0 VMM 不仿真 mc146818，留着驱动每次读要等 UIP 超时 ~1.2s
# （启动实测三次共 ~3.8s）。M1 再考虑在 VMM 侧仿真 RTC。
CONFIG_RTC_CLASS=n
# microVM guest 用不到的子系统，缩短启动与镜像体积（实测 PCI/USB/DRM 等
# 占内核体积与初始化时间的大头；virtio-mmio 与 8250 串口均不依赖 PCI）
CONFIG_PCI=n
CONFIG_USB=n
CONFIG_SCSI=n
CONFIG_ATA=n
CONFIG_DRM=n
CONFIG_FB=n
CONFIG_VGA_CONSOLE=n
CONFIG_HWMON=n
CONFIG_WATCHDOG=n
CONFIG_INPUT=n
CONFIG_WLAN=n
CONFIG_WIRELESS=n
CONFIG_CFG80211=n
CONFIG_SOUND=n
# 再一轮启动耗时裁剪（每处实测占 0.1~0.3s 初始化时间）
CONFIG_QUOTA=n
CONFIG_HIBERNATION=n
CONFIG_SUSPEND=n
CONFIG_NVDIMM=n
CONFIG_MTD=n
CONFIG_NVRAM=n
CONFIG_SYSTEM_TRUSTED_KEYRING=n
# initcall 实测各耗 6~50ms 的可裁剪项（kprobe trace 44ms、jitterentropy 18ms、
# perf events 11ms、/dev/msr 7ms、ptp 6ms 等）
CONFIG_KPROBES=n
CONFIG_PERF_EVENTS=n
CONFIG_CRYPTO_JITTERENTROPY=n
CONFIG_PTP_1588_CLOCK=n
CONFIG_X86_MSR=n
CONFIG_X86_CPUID=n
# PNP 枚举全靠 PIO，每次访问都是一次 KVM 退出，serial8250_init 实测因此
# 耗 264ms（initcall_debug）；microVM 无可 PNP 设备
CONFIG_PNP=n
CONFIG_SERIAL_8250_NR_UARTS=1
CONFIG_SERIAL_8250_RUNTIME_UARTS=1
# LZ4 解压最快，缩短冷启动（默认 gzip 解压 12MB 量级镜像要几百毫秒）
CONFIG_KERNEL_LZ4=y
# M1 Task 1：virtio-blk 与 ext4 根文件系统
CONFIG_VIRTIO_BLK=y
CONFIG_EXT4_FS=y
# M1 Task 3：virtio-mem 内存热插拔
CONFIG_VIRTIO_MEM=y
CONFIG_MEMORY_HOTPLUG=y
CONFIG_MEMORY_HOTREMOVE=y
# M1 Task 4：多 vCPU 与 CPU 逻辑上下线
CONFIG_SMP=y
CONFIG_HOTPLUG_CPU=y
CONFIG_NR_CPUS=8
# M1 Task 5：virtio-vsock
CONFIG_VSOCKETS=y
CONFIG_VIRTIO_VSOCKETS=y
# M2 sandbox 层与 eBPF 观测所需内核功能（ADR 0005）
CONFIG_OVERLAY_FS=y
CONFIG_CGROUPS=y
CONFIG_SECCOMP=y
CONFIG_SECCOMP_FILTER=y
CONFIG_SECURITY_LANDLOCK=y
CONFIG_LSM="landlock,lockdown,yama,integrity,bpf"
CONFIG_BPF_SYSCALL=y
CONFIG_BPF_LSM=y
CONFIG_DEBUG_INFO_BTF=y
# M1.5 Task 0：virtio-net
CONFIG_VIRTIO_NET=y
# M1.5 Task 2：Ubuntu GPT 分区表支持
CONFIG_EFI_PARTITION=y
"#;

#[derive(Parser)]
#[command(name = "xtask", about = "Terrarium 构建辅助任务")]
struct Cli {
    #[command(subcommand)]
    command: Command_,
}

#[derive(Subcommand)]
enum Command_ {
    /// 下载/配置/编译 guest 内核与 initramfs
    Kernel {
        /// 上游稳定版内核版本，如 6.12.41
        #[arg(long, default_value = DEFAULT_KERNEL_VERSION)]
        version: String,
    },
    /// 创建 ext4 rootfs 镜像（依赖 kernel 子命令先跑过）
    Rootfs,
    /// 下载 Ubuntu noble cloud image（raw 格式）
    Ubuntu,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command_::Kernel { version } => {
            if let Err(e) = kernel(&version) {
                eprintln!("xtask kernel 失败: {e}");
                std::process::exit(1);
            }
        }
        Command_::Rootfs => {
            if let Err(e) = rootfs() {
                eprintln!("xtask rootfs 失败: {e}");
                std::process::exit(1);
            }
        }
        Command_::Ubuntu => {
            if let Err(e) = ubuntu() {
                eprintln!("xtask ubuntu 失败: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn kernel(version: &str) -> Result<(), String> {
    let guest_dir = guest_dir()?;
    let src_dir = guest_dir.join("src");
    std::fs::create_dir_all(&src_dir).map_err(|e| e.to_string())?;

    let bzimage = build_kernel(version, &src_dir, &guest_dir)?;
    let initramfs = build_initramfs(&src_dir, &guest_dir)?;

    println!("产物就绪:");
    println!("  kernel:   {}", bzimage.display());
    println!("  initramfs: {}", initramfs.display());
    Ok(())
}

/// workspace 根目录（xtask 的 CARGO_MANIFEST_DIR 是 <root>/xtask）
fn workspace_root() -> PathBuf {
    let manifest = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest)
        .parent()
        .expect("xtask 必须位于 workspace 根目录下")
        .to_path_buf()
}

fn guest_dir() -> Result<PathBuf, String> {
    let dir = workspace_root().join("target/guest");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
    Ok(dir)
}

/// 运行外部命令并检查退出码
fn run(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<(), String> {
    println!("+ {program} {}", args.join(" "));
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let status = cmd
        .status()
        .map_err(|e| format!("无法执行 {program}: {e}"))?;
    if !status.success() {
        return Err(format!("{program} 退出码非零: {status}"));
    }
    Ok(())
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    if dest.exists() {
        println!("已存在，跳过下载: {}", dest.display());
        return Ok(());
    }
    // 下载到临时文件再改名，避免留下半截产物
    let tmp = dest.with_extension("partial");
    run(
        "curl",
        &["-fSL", "--retry", "3", "-o", tmp.to_str().unwrap(), url],
        None,
    )?;
    std::fs::rename(&tmp, dest).map_err(|e| format!("改名 {:?} 失败: {e}", tmp))?;
    Ok(())
}

fn build_kernel(version: &str, src_dir: &Path, guest_dir: &Path) -> Result<PathBuf, String> {
    let out = guest_dir.join("bzImage");
    let tarball = src_dir.join(format!("linux-{version}.tar.xz"));
    let tree = src_dir.join(format!("linux-{version}"));

    // 1. 下载上游稳定版内核（https 取自 cdn.kernel.org；签名校验留待后续）
    let major = version.split('.').next().unwrap_or("6");
    download(
        &format!("https://cdn.kernel.org/pub/linux/kernel/v{major}.x/linux-{version}.tar.xz"),
        &tarball,
    )?;

    // 2. 解压
    if !tree.join("Makefile").exists() {
        run("tar", &["-xf", tarball.to_str().unwrap()], Some(src_dir))?;
    } else {
        println!("已解压，跳过: {}", tree.display());
    }

    // 3. defconfig + 最小裁剪片段
    let fragment = tree.join("terra-fragment.config");
    std::fs::write(&fragment, KERNEL_CONFIG_FRAGMENT).map_err(|e| e.to_string())?;
    let nproc = std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "4".into());
    run("make", &["defconfig"], Some(&tree))?;
    run(
        "bash",
        &[
            "scripts/kconfig/merge_config.sh",
            "-m",
            ".config",
            fragment.to_str().unwrap(),
        ],
        Some(&tree),
    )?;
    run("make", &["olddefconfig"], Some(&tree))?;

    // 4. 编译 bzImage
    run("make", &[&format!("-j{nproc}"), "bzImage"], Some(&tree))?;

    // 5. 导出产物并检查体积（目标 ≤ 30MB，见 AGENTS.md Task 1）
    let built = tree.join("arch/x86/boot/bzImage");
    std::fs::copy(&built, &out).map_err(|e| format!("拷贝 bzImage 失败: {e}"))?;
    let size_mb = std::fs::metadata(&out).map_err(|e| e.to_string())?.len() / 1024 / 1024;
    println!("bzImage 大小: {size_mb} MiB");
    if size_mb > 30 {
        return Err(format!("bzImage 超过 30MB 目标: {size_mb} MiB"));
    }
    Ok(out)
}

fn build_initramfs(src_dir: &Path, guest_dir: &Path) -> Result<PathBuf, String> {
    let out = guest_dir.join("initramfs.cpio.gz");
    let root = guest_dir.join("initramfs-root");
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;

    // 1. 下载并编译静态 busybox
    let tarball = src_dir.join(format!("busybox-{BUSYBOX_VERSION}.tar.bz2"));
    let tree = src_dir.join(format!("busybox-{BUSYBOX_VERSION}"));
    download(
        &format!("https://busybox.net/downloads/busybox-{BUSYBOX_VERSION}.tar.bz2"),
        &tarball,
    )?;
    if !tree.join("Makefile").exists() {
        run("tar", &["-xf", tarball.to_str().unwrap()], Some(src_dir))?;
    }
    let busybox = tree.join("busybox");
    if !busybox.exists() {
        run("make", &["defconfig"], Some(&tree))?;
        // 静态链接：不依赖 guest 内任何动态库
        run(
            "sed",
            &[
                "-i",
                "s/^# CONFIG_STATIC is not set$/CONFIG_STATIC=y/",
                ".config",
            ],
            Some(&tree),
        )?;
        // busybox 1.36.1 的 tc 与新版内核头文件/glibc 冲突（tc.o 编译失败），
        // initramfs 用不到它，直接关掉
        run(
            "sed",
            &[
                "-i",
                "-e",
                "s/^CONFIG_TC=y$/CONFIG_TC=n/",
                "-e",
                "s/^CONFIG_FEATURE_TC_INGRESS=y$/CONFIG_FEATURE_TC_INGRESS=n/",
                ".config",
            ],
            Some(&tree),
        )?;
        run("make", &["oldconfig"], Some(&tree))?;
        let nproc = std::thread::available_parallelism()
            .map(|n| n.get().to_string())
            .unwrap_or_else(|_| "4".into());
        run("make", &[&format!("-j{nproc}")], Some(&tree))?;
    }

    // 2. /init：挂载 devtmpfs 后，若检测到 /dev/vda 则切到 ext4 rootfs；
    // 否则直接 exec /bin/sh（console 由内核 cmdline 指向 ttyS0）。
    // echo 的就绪标记是给 boot smoke test 断言用的。
    let init = root.join("init");
    std::fs::write(
        &init,
        "#!/bin/sh\n\
          /bin/mount -t devtmpfs devtmpfs /dev\n\
          if [ -b /dev/vda ]; then\n\
            /bin/mount -t ext4 /dev/vda /newroot || exec /bin/sh\n\
            if [ -f /newroot/terra_persist ]; then\n\
              echo TERRA_PERSIST_OK\n\
            else\n\
              echo first > /newroot/terra_persist\n\
              echo TERRA_FIRST_WRITE_OK\n\
            fi\n\
            /newroot/sbin/sandboxd &\n\
            /newroot/sbin/observe &\n\
            exec /bin/switch_root /newroot /bin/sh\n\
          fi\n\
          echo TERRA_GUEST_SHELL_READY\n\
          exec /bin/sh\n",
    )
    .map_err(|e| e.to_string())?;

    // 3. 用内核树的 gen_init_cpio 打包（规格文件可描述设备节点，无需 root）
    let kernel_tree = find_kernel_tree(src_dir)?;
    let gen_init_cpio = kernel_tree.join("usr/gen_init_cpio");
    if !gen_init_cpio.exists() {
        run("make", &["usr/gen_init_cpio"], Some(&kernel_tree))?;
    }
    let spec = root.join("initramfs.spec");
    std::fs::write(
        &spec,
        format!(
            "dir /bin 0755 0 0\n\
             dir /dev 0755 0 0\n\
             dir /newroot 0755 0 0\n\
             file /init {} 0755 0 0\n\
             file /bin/busybox {} 0755 0 0\n\
             slink /bin/sh /bin/busybox 0777 0 0\n\
             slink /bin/mount /bin/busybox 0777 0 0\n\
             slink /bin/switch_root /bin/busybox 0777 0 0\n\
             slink /bin/echo /bin/busybox 0777 0 0\n\
             nod /dev/console 0600 0 0 c 5 1\n\
             nod /dev/null 0666 0 0 c 1 3\n",
            init.display(),
            busybox.display()
        ),
    )
    .map_err(|e| e.to_string())?;

    // gen_init_cpio 输出到 stdout，经 gzip 压缩为产物
    println!("+ gen_init_cpio | gzip > {}", out.display());
    let spec_str = spec.to_str().unwrap().to_string();
    let out_str = out.to_str().unwrap().to_string();
    run(
        "bash",
        &[
            "-c",
            &format!(
                "'{gen_init_cpio}' '{spec_str}' | gzip -9 > '{out_str}'",
                gen_init_cpio = gen_init_cpio.display()
            ),
        ],
        None,
    )?;
    Ok(out)
}

fn rootfs() -> Result<(), String> {
    let guest_dir = guest_dir()?;
    let src_dir = guest_dir.join("src");

    // 1. 检查必需工具。
    for tool in &["mkfs.ext4", "debugfs"] {
        run("which", &[tool], None).map_err(|_| format!("{tool} 未安装或不在 PATH 中"))?;
    }

    // 2. 找到 busybox（来自 kernel 子命令产物）。
    let busybox = src_dir
        .join(format!("busybox-{BUSYBOX_VERSION}"))
        .join("busybox")
        .canonicalize()
        .map_err(|e| format!("找不到 busybox ({}): {e}", BUSYBOX_VERSION))?;
    if !busybox.exists() {
        return Err("busybox 未构建，请先运行 `cargo xtask kernel`".to_string());
    }

    // 3. 创建 64MiB 空 ext4 镜像。
    let out = guest_dir.join("rootfs.ext4");
    run("truncate", &["-s", "64M", out.to_str().unwrap()], None)?;
    run("mkfs.ext4", &["-q", "-F", out.to_str().unwrap()], None)?;

    // 4. 编译 sandboxd（musl 静态链接）并放入 rootfs。
    let ws_root = workspace_root();
    println!("+ cargo build --target x86_64-unknown-linux-musl --release -p sandboxd");
    run(
        "cargo",
        &[
            "build",
            "--target",
            "x86_64-unknown-linux-musl",
            "--release",
            "-p",
            "sandboxd",
        ],
        Some(&ws_root),
    )?;
    let sandboxd_bin = ws_root
        .join("target/x86_64-unknown-linux-musl/release/sandboxd")
        .canonicalize()
        .map_err(|e| format!("找不到 sandboxd 二进制: {e}"))?;

    // 编译 observe（musl 静态链接）。
    run(
        "cargo",
        &[
            "build",
            "--target",
            "x86_64-unknown-linux-musl",
            "--release",
            "-p",
            "observe",
        ],
        Some(&ws_root),
    )?;
    let observe_bin = ws_root
        .join("target/x86_64-unknown-linux-musl/release/observe")
        .canonicalize()
        .map_err(|e| format!("找不到 observe 二进制: {e}"))?;

    // 用 debugfs 填充（免 root）。
    let out_str = out.to_str().unwrap().to_string();
    let busybox_str = busybox.to_str().unwrap().to_string();
    let sandboxd_str = sandboxd_bin.to_str().unwrap().to_string();
    let observe_str = observe_bin.to_str().unwrap().to_string();
    let script = format!(
        "mkdir /bin\n\
         write {busybox_str} /bin/busybox\n\
         symlink /bin/sh /bin/busybox\n\
         symlink /bin/echo /bin/busybox\n\
         mkdir /sbin\n\
         write {sandboxd_str} /sbin/sandboxd\n\
         write {observe_str} /sbin/observe\n\
         mkdir /run\n"
    );
    let mut child = Command::new("debugfs")
        .args(["-w", "-f", "-", &out_str])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 debugfs 失败: {e}"))?;
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .map_err(|e| format!("写 debugfs 命令失败: {e}"))?;
    let status = child.wait().map_err(|e| format!("debugfs 失败: {e}"))?;
    if !status.success() {
        return Err(format!("debugfs 退出码非零: {status}"));
    }

    println!("产物就绪:");
    println!("  rootfs:   {}", out.display());
    Ok(())
}

fn ubuntu() -> Result<(), String> {
    let guest_dir = guest_dir()?;
    let out = guest_dir.join("ubuntu.raw");

    // Ubuntu noble 24.04 cloud image（raw 格式，直接可用）。
    let url = "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img";

    download(url, &out)?;

    let size_mb = std::fs::metadata(&out).map_err(|e| e.to_string())?.len() / 1024 / 1024;
    println!("产物就绪:");
    println!("  ubuntu:   {} ({} MiB)", out.display(), size_mb);
    println!("  启动: cargo run -p vmm --example boot -- --kernel target/guest/bzImage --disk {} --mem 1024 --cmdline 'root=/dev/vda1 console=ttyS0 cloud-init=disabled'", out.display());
    Ok(())
}

/// 找到刚解压的内核源码树（src_dir 下唯一的 linux-* 目录）
fn find_kernel_tree(src_dir: &Path) -> Result<PathBuf, String> {
    for entry in std::fs::read_dir(src_dir).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("linux-"))
        {
            return Ok(path);
        }
    }
    Err(format!("{} 下找不到内核源码树", src_dir.display()))
}
