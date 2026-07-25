//! terra-engine — host daemon and sole control plane entry point.
//!
//! Manages VM lifecycle, sandbox placement, resource scheduling,
//! warm pool management, and billing metering.

mod cli;
mod commands;
mod daemon;
mod manager;
mod spec;
mod vm;

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

fn main() {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
    }

    // "daemon" subcommand starts the server
    if args[1] == "daemon" {
        let socket = parse_socket_flag(&args);
        daemon::run(&socket).expect("Daemon failed");
        return;
    }

    // All other commands are CLI clients → connect to daemon
    let socket = parse_socket_flag(&args);
    let cmd = cli::build_command(&args);
    let cmd_name = args[1].clone();

    match cli::send_command(&socket, cmd) {
        Ok(response) => cli::print_response(&cmd_name, response),
        Err(e) => {
            eprintln!("ERROR: {}", e);
            std::process::exit(1);
        }
    }
}

fn parse_socket_flag(args: &[String]) -> String {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--socket" && i + 1 < args.len() {
            return args[i + 1].clone();
        }
        i += 1;
    }
    DEFAULT_SOCKET.to_string()
}
