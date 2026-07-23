//! terra-controller — host daemon and sole control plane entry point.
//!
//! Manages Cloud Hypervisor VM processes, implements resource-aware
//! scheduling and placement decisions, warm pools, and the closed-loop
//! resource control (PSI/DAMON signals → CH resize API).

fn main() {
    println!("terra-controller starting...");
}
