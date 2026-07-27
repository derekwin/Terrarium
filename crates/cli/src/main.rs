//! terra — Terrarium Engine CLI.
//!
//! Communicates with the engine daemon over Unix socket JSON protocol.

use clap::{Parser, Subcommand};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use terrarium_protocol::{Command, Response};

const DEFAULT_SOCKET: &str = "/tmp/terra.sock";

#[derive(Parser)]
#[command(name = "terra", about = "Terrarium Engine CLI")]
struct Cli {
    #[arg(long, default_value = DEFAULT_SOCKET)]
    socket: String,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Create {
        name: String,
        #[arg(long)]
        kernel: String,
        #[arg(long)]
        initramfs: Option<String>,
        #[arg(long, default_value = "2")]
        cpus: u8,
        #[arg(long)]
        max_cpus: Option<u8>,
        #[arg(long)]
        max_memory: Option<u64>,
        #[arg(long, default_value = "512")]
        memory: u64,
        /// virtiofs layers, comma-separated, highest priority first
        /// (e.g. --layers python,base). Empty = initramfs boot.
        #[arg(long, value_delimiter = ',')]
        layers: Vec<String>,
        /// Persistent upperdir name (user data survives VM destruction).
        #[arg(long)]
        upper: Option<String>,
        /// Attach virtio-net (tap + host NAT, DHCP in guest).
        #[arg(long)]
        net: bool,
    },
    List,
    Info {
        name: String,
    },
    Resize {
        name: String,
        #[arg(long)]
        cpus: Option<u8>,
        #[arg(long)]
        memory_bytes: Option<u64>,
    },
    Shutdown {
        name: String,
    },
    Kill {
        name: String,
    },
    /// Stop and deregister a VM.
    Destroy {
        name: String,
    },
    /// Hot-plug a layered filesystem into a running VM.
    AttachFs {
        name: String,
        #[arg(long, value_delimiter = ',')]
        layers: Vec<String>,
    },
    /// Detach a previously attached layered filesystem.
    DetachFs {
        name: String,
    },
    /// Create warm-pool idle VMs.
    PoolCreate {
        #[arg(long, default_value = "1")]
        size: u32,
        #[arg(long)]
        kernel: Option<String>,
        /// Attach virtio-net to pool VMs.
        #[arg(long)]
        net: bool,
    },
    PoolList,
    /// Claim an idle pool VM and hot-plug layers.
    PoolClaim {
        #[arg(long, value_delimiter = ',')]
        layers: Vec<String>,
    },
    PoolRelease {
        name: String,
    },
    /// Show NAT bridge and per-VM network attachments.
    NetList,
    /// Execute a command inside a VM (via the guest agent).
    Exec {
        name: String,
        /// Per-command timeout in seconds (default 60, max 3600).
        #[arg(long, default_value = "60")]
        timeout: u64,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Host-side image preparation (admin operations, run once per host).
    #[command(subcommand)]
    Image(ImageCommands),
}

#[derive(Subcommand)]
enum ImageCommands {
    /// Build the guest kernel.
    Kernel {
        /// Kernel version, e.g. 6.12 (default per images/build-kernel.sh).
        #[arg(long)]
        version: Option<String>,
    },
    /// Build the guest rootfs cpio.
    Rootfs {
        /// System type: busybox | alpine (needs ROOTFS_SRC for alpine).
        #[arg(long, default_value = "busybox")]
        r#type: String,
    },
    /// Build the virtiofs boot initramfs.
    Initramfs,
    /// Build the warm-pool idle initramfs.
    AgentInitramfs,
    /// Pack a directory into an EROFS layer image.
    Layer {
        /// Directory containing the layer content.
        src: String,
        /// Layer name (referenced by `layers` at VM create/claim).
        name: String,
    },
    /// Build a tool layer by configuring inside a builder VM: boot a VM
    /// from the base layer, run a setup script inside it, then pack the
    /// filesystem changes (copy-up delta) as the new layer.
    LayerBuild {
        /// New layer name.
        name: String,
        /// Setup script executed inside the builder VM (sh).
        #[arg(long)]
        script: String,
        /// Base layer to build on.
        #[arg(long, default_value = "base")]
        base: String,
        /// Kernel image path.
        #[arg(long, default_value = "target/guest/vmlinux.bin")]
        kernel: String,
        /// virtiofs boot initramfs path.
        #[arg(long, default_value = "target/guest/initramfs-virtiofs.cpio.gz")]
        initramfs: String,
        /// Disable networking for the builder VM (network is on by
        /// default — environment builds usually need downloads, and
        /// networking requires a privileged daemon).
        #[arg(long)]
        no_net: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Create {
            name,
            kernel,
            initramfs,
            cpus,
            max_cpus,
            max_memory,
            memory,
            layers,
            upper,
            net,
        } => {
            let mut cmd = Command::create(&name, &kernel)
                .with_cpus(cpus)
                .with_memory_mb(memory)
                .with_layers(layers);
            if let Some(u) = upper {
                cmd = cmd.with_upper(u);
            }
            cmd = cmd.with_net(net);
            if let Some(i) = initramfs {
                cmd = cmd.with_initramfs(i);
            }
            if let Some(m) = max_cpus {
                cmd = cmd.with_max_cpus(m);
            }
            if let Some(m) = max_memory {
                cmd = cmd.with_max_memory_mb(m);
            }
            print_response(send(&cli.socket, &cmd));
        }
        Commands::List => {
            print_response(send(&cli.socket, &Command::new("list")));
        }
        Commands::Info { name } => {
            print_response(send(&cli.socket, &Command::new("info").with_name(name)));
        }
        Commands::Resize {
            name,
            cpus,
            memory_bytes,
        } => {
            let mut cmd = Command::new("resize").with_name(name);
            if let Some(c) = cpus {
                cmd = cmd.with_cpus(c);
            }
            if let Some(m) = memory_bytes {
                cmd = cmd.with_memory_bytes(m);
            }
            print_response(send(&cli.socket, &cmd));
        }
        Commands::Shutdown { name } => {
            print_response(send(&cli.socket, &Command::new("shutdown").with_name(name)));
        }
        Commands::Kill { name } => {
            print_response(send(&cli.socket, &Command::new("kill").with_name(name)));
        }
        Commands::Destroy { name } => {
            print_response(send(&cli.socket, &Command::new("destroy").with_name(name)));
        }
        Commands::AttachFs { name, layers } => {
            let cmd = Command::new("attach_fs")
                .with_name(name)
                .with_layers(layers);
            print_response(send(&cli.socket, &cmd));
        }
        Commands::DetachFs { name } => {
            print_response(send(
                &cli.socket,
                &Command::new("detach_fs").with_name(name),
            ));
        }
        Commands::PoolCreate { size, kernel, net } => {
            let mut cmd = Command::new("pool_create").with_pool_size(size);
            if let Some(k) = kernel {
                cmd.kernel = Some(k);
            }
            cmd = cmd.with_net(net);
            print_response(send(&cli.socket, &cmd));
        }
        Commands::PoolList => {
            print_response(send(&cli.socket, &Command::new("pool_list")));
        }
        Commands::PoolClaim { layers } => {
            let cmd = Command::new("pool_claim").with_layers(layers);
            print_response(send(&cli.socket, &cmd));
        }
        Commands::PoolRelease { name } => {
            print_response(send(
                &cli.socket,
                &Command::new("pool_release").with_name(name),
            ));
        }
        Commands::NetList => {
            print_response(send(&cli.socket, &Command::new("net_list")));
        }
        Commands::Exec {
            name,
            timeout,
            args,
        } => {
            let cmd = Command::new("exec")
                .with_name(name)
                .with_args(args)
                .with_timeout_secs(timeout);
            print_response(send(&cli.socket, &cmd));
        }
        Commands::Image(img) => match img {
            ImageCommands::LayerBuild {
                name,
                script,
                base,
                kernel,
                initramfs,
                no_net,
            } => layer_build(
                &cli.socket,
                &name,
                &script,
                &base,
                &kernel,
                &initramfs,
                !no_net,
            ),
            other => run_image(other),
        },
    }
}

/// Build a tool layer by doing: builder VM -> setup script -> pack delta.
fn layer_build(
    socket: &str,
    name: &str,
    script: &str,
    base: &str,
    kernel: &str,
    irfs: &str,
    net: bool,
) {
    let builder = format!("lb-{}", name);
    let upper = builder.clone();

    // 1) boot the builder VM from the base layer with a persistent upper
    let create = Command::create(&builder, kernel)
        .with_initramfs(irfs)
        .with_cpus(1)
        .with_memory_mb(512)
        .with_layers(vec![base.to_string()])
        .with_upper(&upper)
        .with_net(net);
    let resp = send(socket, &create);
    if !resp.contains("\"ok\"") && !resp.contains("\"status\":\"ok\"") {
        eprintln!("ERROR: builder VM create failed: {}", resp);
        std::process::exit(1);
    }
    println!("builder VM {} running", builder);

    // 2) run the setup script inside the VM. The guest agent takes a
    // moment to come up after create returns — retry, and fail hard if
    // the script never succeeds (never pack an empty/garbage layer).
    let content = std::fs::read_to_string(script).unwrap_or_else(|e| {
        eprintln!("ERROR: read script {}: {}", script, e);
        std::process::exit(1);
    });
    let mut resp = String::new();
    let mut ok = false;
    for _ in 0..30 {
        let exec = Command::new("exec").with_name(&builder).with_args(vec![
            "sh".into(),
            "-c".into(),
            content.clone(),
        ]);
        resp = send(socket, &exec);
        // protocol ok AND script exit code 0 — packing on a failed
        // script would silently produce an empty/garbage layer.
        if resp.contains("\"status\":\"ok\"") && resp.contains("\"exit_code\":0") {
            ok = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if !ok {
        let _ = send(socket, &Command::new("destroy").with_name(&builder));
        eprintln!("ERROR: setup script failed in builder VM: {}", resp);
        std::process::exit(1);
    }
    println!("setup output: {}", resp);
    // Best-effort network settle for download-heavy scripts: the guest
    // DHCP lease can still be in flight when the agent answers.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // 3) clean runtime noise from the delta (not part of the environment)
    let cleanup = Command::new("exec").with_name(&builder).with_args(vec![
        "sh".into(),
        "-c".into(),
        "rm -rf /tmp/* /run/* /var/log/* /etc/resolv.conf 2>/dev/null; sync".into(),
    ]);
    let _ = send(socket, &cleanup);

    // 4) destroy the builder VM
    let destroy = Command::new("destroy").with_name(&builder);
    let _ = send(socket, &destroy);
    println!("builder VM destroyed");

    // 5) pack the upperdir (copy-up delta) as the new layer
    let fs_root = std::env::var("TERRA_STATE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/tmp/terra-disks".into());
    let upper_dir = format!("{}/fs/uppers/{}", fs_root, upper);
    if !std::path::Path::new(&upper_dir).is_dir() {
        eprintln!(
            "ERROR: upperdir {} not found — is TERRA_STATE_DIR correct?",
            upper_dir
        );
        std::process::exit(1);
    }
    run_image(ImageCommands::Layer {
        src: upper_dir,
        name: name.to_string(),
    });
    println!("layer '{}' built and ready to use in layers=[...]", name);
}

/// Image commands are host-side build operations, not daemon protocol.
fn run_image(img: ImageCommands) {
    let (script, args): (&str, Vec<String>) = match img {
        ImageCommands::Kernel { version } => {
            ("images/build-kernel.sh", version.into_iter().collect())
        }
        ImageCommands::Rootfs { r#type } => ("images/build-rootfs.sh", vec![r#type]),
        ImageCommands::Initramfs => ("images/build-initramfs-virtiofs.sh", vec![]),
        ImageCommands::AgentInitramfs => ("images/build-initramfs-agent.sh", vec![]),
        ImageCommands::Layer { src, name } => {
            let layer_dir = std::env::var("TERRA_LAYER_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    format!(
                        "{}/.local/share/terra/layers",
                        std::env::var("HOME").unwrap_or_default()
                    )
                });
            ("images/build-layer.sh", vec![src, name, layer_dir])
        }
        ImageCommands::LayerBuild { .. } => {
            unreachable!("LayerBuild is handled before run_image")
        }
    };
    if !std::path::Path::new(script).exists() {
        eprintln!(
            "ERROR: {} not found — run `terra image` from the Terrarium repo root",
            script
        );
        std::process::exit(1);
    }
    let status = std::process::Command::new("bash")
        .arg(script)
        .args(&args)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("ERROR: failed to run {}: {}", script, e);
            std::process::exit(1);
        });
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn send(socket: &str, cmd: &Command) -> String {
    match UnixStream::connect(socket) {
        Ok(mut stream) => {
            let json = serde_json::to_string(cmd).unwrap_or_default();
            let _ = writeln!(stream, "{}", json);
            let _ = stream.flush();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                line.trim().to_string()
            } else {
                r#"{"status":"error","error":"no response from engine"}"#.to_string()
            }
        }
        Err(e) => format!(
            r#"{{"status":"error","error":"engine unavailable: {}"}}"#,
            e
        ),
    }
}

fn print_response(raw: String) {
    match serde_json::from_str::<Response>(&raw) {
        Ok(resp) => {
            if resp.is_ok() {
                if let Some(data) = &resp.data {
                    println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
                } else {
                    println!("OK");
                }
            } else {
                eprintln!(
                    "ERROR: {}",
                    resp.error.as_deref().unwrap_or("unknown error")
                );
                std::process::exit(1);
            }
        }
        Err(_) => {
            eprintln!("ERROR: invalid response: {}", raw);
            std::process::exit(1);
        }
    }
}
