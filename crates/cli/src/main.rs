//! terra — Terrarium Engine CLI.
//!
//! Communicates with the engine daemon over Unix socket JSON protocol.

use clap::{Parser, Subcommand};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

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
    /// Create a new VM.
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
        #[arg(long, default_value = "512")]
        memory: u64,
        #[arg(long)]
        rootfs_disk: Option<String>,
        #[arg(long)]
        toolfs_disk: Vec<String>,
        #[arg(long, default_value = "20")]
        disk_size: u64,
    },
    /// List running VMs.
    List,
    /// Show VM details.
    Info { name: String },
    /// Resize VM CPU or memory.
    Resize {
        name: String,
        #[arg(long)]
        cpus: Option<u8>,
        #[arg(long)]
        memory_bytes: Option<u64>,
    },
    /// Gracefully shut down a VM.
    Shutdown { name: String },
    /// Force-kill a VM.
    Kill { name: String },
    /// Destroy a VM and its overlay disk.
    Destroy { name: String },
    /// Execute a command in a sandbox.
    Exec {
        args: Vec<String>,
        #[arg(long)]
        memory_mb: Option<u64>,
    },
    /// Snapshot user overlay (data only, not VM state).
    SnapshotCreate {
        name: String,
        #[arg(long)]
        tag: Option<String>,
    },
    /// List snapshots for a VM.
    SnapshotList { name: String },
    /// Restore user overlay from a snapshot.
    SnapshotRestore {
        name: String,
        #[arg(long)]
        tag: String,
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
            memory,
            rootfs_disk,
            toolfs_disk,
            disk_size,
        } => {
            let mut cmd = serde_json::json!({
                "command": "create", "name": name, "kernel": kernel,
                "cpus": cpus, "memory_mb": memory, "disk_size_gb": disk_size,
            });
            if let Some(i) = initramfs {
                cmd["initramfs"] = serde_json::json!(i);
            }
            if let Some(m) = max_cpus {
                cmd["max_cpus"] = serde_json::json!(m);
            }
            if let Some(b) = rootfs_disk {
                cmd["base_disk"] = serde_json::json!(b);
            }
            if !toolfs_disk.is_empty() {
                cmd["tool_layers"] = serde_json::json!(toolfs_disk);
            }
            print_response(send(&cli.socket, &cmd));
        }
        Commands::List => print_response(send(&cli.socket, &serde_json::json!({"command":"list"}))),
        Commands::Info { name } => print_response(send(
            &cli.socket,
            &serde_json::json!({"command":"info","name":name}),
        )),
        Commands::Resize {
            name,
            cpus,
            memory_bytes,
        } => {
            let mut cmd = serde_json::json!({"command":"resize","name":name});
            if let Some(c) = cpus {
                cmd["cpus"] = serde_json::json!(c);
            }
            if let Some(m) = memory_bytes {
                cmd["memory_bytes"] = serde_json::json!(m);
            }
            print_response(send(&cli.socket, &cmd));
        }
        Commands::Shutdown { name } => print_response(send(
            &cli.socket,
            &serde_json::json!({"command":"shutdown","name":name}),
        )),
        Commands::Kill { name } => print_response(send(
            &cli.socket,
            &serde_json::json!({"command":"kill","name":name}),
        )),
        Commands::Destroy { name } => print_response(send(
            &cli.socket,
            &serde_json::json!({"command":"destroy","name":name}),
        )),
        Commands::Exec { args, memory_mb } => {
            let mut cmd = serde_json::json!({"command":"exec","args":args});
            if let Some(mb) = memory_mb {
                cmd["limits"] = serde_json::json!({"memory_mb": mb});
            }
            print_response(send(&cli.socket, &cmd));
        }
        Commands::SnapshotCreate { name, tag } => {
            let mut cmd = serde_json::json!({"command":"snapshot_create","name":name});
            if let Some(t) = tag {
                cmd["tag"] = serde_json::json!(t);
            }
            print_response(send(&cli.socket, &cmd));
        }
        Commands::SnapshotList { name } => {
            print_response(send(
                &cli.socket,
                &serde_json::json!({"command":"snapshot_list","name":name}),
            ));
        }
        Commands::SnapshotRestore { name, tag } => {
            print_response(send(
                &cli.socket,
                &serde_json::json!({"command":"snapshot_restore","name":name,"tag":tag}),
            ));
        }
    }
}

fn send(socket: &str, cmd: &serde_json::Value) -> serde_json::Value {
    let mut stream = UnixStream::connect(socket).unwrap_or_else(|e| {
        eprintln!(
            "ERROR: Cannot connect to engine daemon at {}: {}",
            socket, e
        );
        std::process::exit(1);
    });
    let json = serde_json::to_string(cmd).unwrap();
    writeln!(stream, "{}", json).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|_| serde_json::json!({"status":"error"}))
}

fn print_response(resp: serde_json::Value) {
    if resp["status"].as_str() != Some("ok") {
        eprintln!(
            "ERROR: {}",
            resp["message"].as_str().unwrap_or("unknown error")
        );
        std::process::exit(1);
    }
    if let Some(data) = resp.get("data") {
        println!("{}", serde_json::to_string_pretty(data).unwrap());
    } else if let Some(msg) = resp.get("message").and_then(|m| m.as_str()) {
        println!("{}", msg);
    }
}
