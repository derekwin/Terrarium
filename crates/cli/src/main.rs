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
    /// Host-side image preparation (admin operations, run once per host).
    #[command(subcommand)]
    Image(ImageCommands),
}

#[derive(Subcommand)]
enum ImageCommands {
    /// Build the guest kernel (images/build-kernel.sh).
    Kernel,
    /// Build the guest rootfs cpio (images/build-rootfs.sh).
    Rootfs,
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
        } => {
            let mut cmd = Command::create(&name, &kernel)
                .with_cpus(cpus)
                .with_memory_mb(memory)
                .with_layers(layers);
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
        Commands::PoolCreate { size, kernel } => {
            let mut cmd = Command::new("pool_create").with_pool_size(size);
            if let Some(k) = kernel {
                cmd.kernel = Some(k);
            }
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
        Commands::Image(img) => run_image(img),
    }
}

/// Image commands are host-side build operations, not daemon protocol.
fn run_image(img: ImageCommands) {
    let (script, args): (&str, Vec<String>) = match img {
        ImageCommands::Kernel => ("images/build-kernel.sh", vec![]),
        ImageCommands::Rootfs => ("images/build-rootfs.sh", vec![]),
        ImageCommands::Initramfs => ("images/build-initramfs-virtiofs.sh", vec![]),
        ImageCommands::AgentInitramfs => ("images/build-initramfs-agent.sh", vec![]),
        ImageCommands::Layer { src, name } => {
            ("images/build-layer.sh", vec![src, name])
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
