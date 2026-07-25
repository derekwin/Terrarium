//! Host-level network QoS via Linux tc (traffic control).
//!
//! Applies rate limiting and priority to any TAP/veth interface.
//! VMM-agnostic — works with CH, Firecracker, QEMU, K8s pods.

use adapter_traits::NetworkQos;
use std::process::Command;

/// Apply egress shaping (HTB) and ingress policing to an interface.
/// Idempotent — repeated calls replace existing rules.
pub fn apply_tc_qos(iface: &str, qos: &NetworkQos) -> Result<(), String> {
    if qos.egress_kbps == 0 && qos.ingress_kbps == 0 {
        return Ok(());
    }

    // Clear existing rules
    let _ = Command::new("tc")
        .args(["qdisc", "del", "dev", iface, "root"])
        .output();
    let _ = Command::new("tc")
        .args(["qdisc", "del", "dev", iface, "ingress"])
        .output();

    // Egress shaping
    if qos.egress_kbps > 0 {
        let _ = Command::new("tc")
            .args([
                "qdisc", "add", "dev", iface, "root", "handle", "1:", "htb", "default", "1",
            ])
            .output();
        let _ = Command::new("tc")
            .args([
                "class",
                "add",
                "dev",
                iface,
                "parent",
                "1:",
                "classid",
                "1:1",
                "htb",
                "rate",
                &format!("{}kbit", qos.egress_kbps),
                "prio",
                &qos.priority.to_string(),
            ])
            .output();
    }

    // Ingress policing
    if qos.ingress_kbps > 0 {
        let _ = Command::new("tc")
            .args(["qdisc", "add", "dev", iface, "handle", "ffff:", "ingress"])
            .output();
        let _ = Command::new("tc")
            .args([
                "filter",
                "add",
                "dev",
                iface,
                "parent",
                "ffff:",
                "protocol",
                "ip",
                "u32",
                "match",
                "u32",
                "0",
                "0",
                "police",
                "rate",
                &format!("{}kbit", qos.ingress_kbps),
                "burst",
                "10k",
                "drop",
                "flowid",
                ":1",
            ])
            .output();
    }

    tracing::info!(
        iface = %iface,
        egress_kbps = qos.egress_kbps,
        ingress_kbps = qos.ingress_kbps,
        priority = qos.priority,
        "Applied network QoS"
    );
    Ok(())
}
