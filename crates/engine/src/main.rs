//! terra-engine — host daemon and sole control plane entry point.
//!
//! Manages VM lifecycle, sandbox placement, resource scheduling,
//! warm pool management, and billing metering.

mod commands;
mod daemon;
mod manager;

use std::env;

const DEFAULT_SOCKET: &str = "/tmp/terra.sock";

fn usage() -> ! {
    eprintln!(
        r#"terra-controller — Terrarium VM manager

USAGE:
  controller daemon [--socket PATH]    Start daemon (long-running server)
  controller create <name>  [FLAGS]    Create a new VM
  controller list                      List all running VMs
  controller info <name>               Show VM details
  controller resize <name> [FLAGS]     Resize vCPU or memory
  controller shutdown <name>           Gracefully shut down a VM
  controller kill <name>               Force-kill a VM
  controller destroy <name>            Shut down VM and delete overlay disk

CREATE FLAGS:
  --kernel <PATH>        Guest kernel path           [required]
  --initramfs <PATH>     Initramfs cpio archive       [optional]
  --cmdline <STR>        Kernel command line
  --cpus <N>             Boot vCPU count              [default: 2]
  --max-cpus <N>         Max vCPU count               [default: 16]
  --memory <MB>          Boot memory in MB            [default: 512]
  --hotplug-memory <GB>  Virtio-mem ceiling in GB     [optional]
  --ch-binary <PATH>     CH binary path               [default: cloud-hypervisor]

RESIZE FLAGS:
  --cpus <N>             Target vCPU count
  --memory-bytes <N>     Target memory in bytes

EXAMPLES:
  # Terminal 1: start daemon
  controller daemon

  # Terminal 2: interact with daemon
  controller create demo --kernel target/guest/vmlinux.bin --initramfs target/guest/initramfs.cpio
  controller list
  controller info demo
  controller resize demo --cpus 4
  controller shutdown demo
"#
    );
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
    }

    // "daemon" subcommand starts the server
    if args[1] == "daemon" {
        let socket = parse_socket_flag(&args);
        let tcp = parse_flag(&args, "--tcp");
        daemon::run(&socket, tcp.as_deref())
            .await
            .expect("Daemon failed");
        return;
    }

    // CLI client commands have been moved to the `terra` Python CLI.
    // Use `terra vm create/exec/destroy ...` instead.
    usage();
}

fn parse_socket_flag(args: &[String]) -> String {
    parse_flag(args, "--socket").unwrap_or_else(|| DEFAULT_SOCKET.to_string())
}

fn parse_flag(args: &[String], name: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == name && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}
