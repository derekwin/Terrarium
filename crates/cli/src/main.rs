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
        } => {
            let mut cmd = Command::create(&name, &kernel)
                .with_cpus(cpus)
                .with_memory_mb(memory);
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
