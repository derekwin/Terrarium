//! sandboxd — in-guest sandbox runtime daemon.
//!
//! Manages the full isolation stack (namespaces, OverlayFS, cgroup v2,
//! Landlock, seccomp-bpf) for each agent execution unit inside the VM.
//! Coordinates with the host controller over vsock.

fn main() {
    println!("sandboxd starting...");
}
