//! observe — in-guest eBPF observability daemon.
//!
//! Collects per-sandbox syscall, file, network, and resource usage
//! metrics via eBPF (CO-RE), reporting to the host over vsock.

fn main() {
    println!("observe starting...");
}
