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

    // Clear existing rules (best-effort: may fail if no rules exist)
    let _ = Command::new("tc")
        .args(["qdisc", "del", "dev", iface, "root"])
        .output();
    let _ = Command::new("tc")
        .args(["qdisc", "del", "dev", iface, "ingress"])
        .output();

    // Egress shaping (HTB)
    if qos.egress_kbps > 0 {
        run_tc(&[
            "qdisc", "add", "dev", iface, "root", "handle", "1:", "htb", "default", "1",
        ])?;
        run_tc(&[
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
        ])?;
    }

    // Ingress policing
    if qos.ingress_kbps > 0 {
        run_tc(&["qdisc", "add", "dev", iface, "handle", "ffff:", "ingress"])?;
        run_tc(&[
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
        ])?;
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

fn run_tc(args: &[&str]) -> Result<(), String> {
    let output = Command::new("tc")
        .args(args)
        .output()
        .map_err(|e| format!("tc command failed: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "tc {}: {}",
            args.first().unwrap_or(&"?"),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// NAT networking (tap + bridge + masquerade)
// ---------------------------------------------------------------------------

/// Default bridge and subnet used for VM NAT networking.
pub const DEFAULT_BRIDGE: &str = "terra0";
pub const DEFAULT_GATEWAY: &str = "10.200.0.1";
pub const DEFAULT_PREFIX: u8 = 24;

fn run_ip(args: &[&str]) -> Result<(), String> {
    let output = Command::new("ip")
        .args(args)
        .output()
        .map_err(|e| format!("ip command failed: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "ip {}: {} (need CAP_NET_ADMIN — run the daemon as root or pre-create devices)",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn run_iptables(args: &[&str]) -> Result<(), String> {
    let output = Command::new("iptables")
        .args(args)
        .output()
        .map_err(|e| format!("iptables command failed: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "iptables {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Ensure the NAT bridge + forwarding rules exist (idempotent).
/// Requires CAP_NET_ADMIN.
pub fn ensure_nat_bridge(bridge: &str, gateway: &str, prefix: u8) -> Result<(), String> {
    // Create bridge if missing.
    let exists = Command::new("ip")
        .args(["link", "show", bridge])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        run_ip(&["link", "add", "name", bridge, "type", "bridge"])?;
    }
    run_ip(&["addr", "replace", &format!("{}/{}", gateway, prefix), "dev", bridge])?;
    run_ip(&["link", "set", bridge, "up"])?;

    // Enable forwarding (read-only check; warn only).
    let fwd = std::fs::read_to_string("/proc/sys/net/ipv4/ip_forward")
        .unwrap_or_default()
        .trim()
        .to_string();
    if fwd != "1" {
        std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")
            .map_err(|e| format!("enable ip_forward: {}", e))?;
    }

    // Masquerade outbound traffic from the bridge subnet (idempotent check).
    let rule_exists = Command::new("iptables")
        .args([
            "-t", "nat", "-C", "POSTROUTING", "-s",
            &format!("{}/{}", subnet_of(gateway), prefix),
            "-j", "MASQUERADE",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !rule_exists {
        run_iptables(&[
            "-t", "nat", "-A", "POSTROUTING", "-s",
            &format!("{}/{}", subnet_of(gateway), prefix),
            "-j", "MASQUERADE",
        ])?;
    }
    // DHCP for guests: dnsmasq bound to the bridge (idempotent).
    ensure_dhcp(bridge, gateway)?;

    tracing::info!(%bridge, %gateway, "NAT bridge ready");
    Ok(())
}

/// Ensure a dnsmasq DHCP server is running on the bridge.
///
/// Resolution: $TERRA_DNSMASQ, PATH, /usr/sbin/dnsmasq. dnsmasq is a host
/// dependency (like cloud-hypervisor); a clear error is returned when
/// missing rather than silently leaving guests without DHCP.
pub fn ensure_dhcp(bridge: &str, gateway: &str) -> Result<(), String> {
    // Already serving this bridge?
    if let Ok(status) = Command::new("pgrep").args(["-f", "dnsmasq.*", bridge]).output() {
        if status.status.success() && !status.stdout.is_empty() {
            return Ok(());
        }
    }
    let bin = std::env::var("TERRA_DNSMASQ")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "dnsmasq".into());
    // stdio must be null: dnsmasq daemonizes and the background child
    // would hold our pipes open forever otherwise (output() never EOFs).
    let out = Command::new(&bin)
        .args([
            &format!("--interface={}", bridge),
            "--bind-interfaces",
            "--except-interface=lo",
            &format!("--dhcp-range={},12h", dhcp_range_of(gateway)),
            "--dhcp-option=option:dns-server,8.8.8.8,223.5.5.5",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            format!(
                "dnsmasq not found (apt install dnsmasq, or set TERRA_DNSMASQ): {}",
                e
            )
        })?;
    if !out.status.success() {
        return Err(format!(
            "dnsmasq: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    tracing::info!(%bridge, "dnsmasq DHCP started");
    Ok(())
}

/// "10.200.0.100,10.200.0.250" style range derived from the gateway.
fn dhcp_range_of(gateway: &str) -> String {
    let parts: Vec<&str> = gateway.split('.').collect();
    if parts.len() == 4 {
        format!(
            "{}.{}.{}.100,{}.{}.{}.250",
            parts[0], parts[1], parts[2], parts[0], parts[1], parts[2]
        )
    } else {
        gateway.to_string()
    }
}

/// /24-style subnet string from a gateway address (last octet zeroed).
fn subnet_of(gateway: &str) -> String {
    let mut parts: Vec<&str> = gateway.split('.').collect();
    if parts.len() == 4 {
        parts[3] = "0";
        parts.join(".")
    } else {
        gateway.to_string()
    }
}

/// Create (or reuse) a tap device and enslave it to the bridge.
pub fn ensure_tap(tap: &str, bridge: &str) -> Result<(), String> {
    let exists = Command::new("ip")
        .args(["link", "show", tap])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        run_ip(&["tuntap", "add", "dev", tap, "mode", "tap"])?;
    }
    run_ip(&["link", "set", tap, "master", bridge])?;
    run_ip(&["link", "set", tap, "up"])?;
    Ok(())
}

/// Remove a tap device (best-effort on missing).
pub fn remove_tap(tap: &str) -> Result<(), String> {
    let exists = Command::new("ip")
        .args(["link", "show", tap])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if exists {
        run_ip(&["link", "del", tap])?;
    }
    Ok(())
}
