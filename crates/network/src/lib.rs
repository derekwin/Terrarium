//! Host-level NAT networking for VMs: tap devices, bridge, DHCP, masquerade.

use std::process::Command;

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

fn run_ebtables(args: &[&str]) -> Result<(), String> {
    let output = Command::new("ebtables")
        .args(args)
        .output()
        .map_err(|e| format!("ebtables command failed: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "ebtables {}: {} (need CAP_NET_ADMIN — run the daemon as root)",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Tenant isolation at L2: no frame may pass between two VM tap ports.
///
/// All VMs share one bridge (terra0) and one subnet for DHCP/routing
/// simplicity, which on a plain bridge means VMs can reach each other's
/// MACs directly. A single ebtables rule drops any frame forwarded
/// between two `terra-*` ports, so tenants are mutually unreachable
/// while VM↔host traffic (gateway, DHCP/ARP to the bridge port) keeps
/// working. The `+` suffix is ebtables' interface-prefix wildcard.
pub fn ensure_vm_isolation() -> Result<(), String> {
    let check = [
        "-C", "FORWARD", "-i", "terra-+", "-o", "terra-+", "-j", "DROP",
    ];
    let exists = Command::new("ebtables")
        .args(check)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        run_ebtables(&[
            "-A", "FORWARD", "-i", "terra-+", "-o", "terra-+", "-j", "DROP",
        ])?;
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
    run_ip(&[
        "addr",
        "replace",
        &format!("{}/{}", gateway, prefix),
        "dev",
        bridge,
    ])?;
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
            "-t",
            "nat",
            "-C",
            "POSTROUTING",
            "-s",
            &format!("{}/{}", subnet_of(gateway), prefix),
            "-j",
            "MASQUERADE",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !rule_exists {
        run_iptables(&[
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-s",
            &format!("{}/{}", subnet_of(gateway), prefix),
            "-j",
            "MASQUERADE",
        ])?;
    }
    // Forwarding for the bridge subnet. Some hosts (e.g. running docker)
    // set the FORWARD policy to DROP — without explicit ACCEPT rules the
    // guest's outbound packets die at the bridge.
    for args in [
        ["-i", bridge, "-j", "ACCEPT"],
        ["-o", bridge, "-j", "ACCEPT"],
    ] {
        let mut check = vec!["-C", "FORWARD"];
        check.extend_from_slice(&args);
        let exists = Command::new("iptables")
            .args(&check)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !exists {
            let mut add = vec!["-A", "FORWARD"];
            add.extend_from_slice(&args);
            run_iptables(&add)?;
        }
    }
    // DHCP for guests: dnsmasq bound to the bridge (idempotent).
    ensure_dhcp(bridge, gateway)?;

    // Tenant L2 isolation (drop frames between VM tap ports).
    ensure_vm_isolation()?;

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
    let pattern = format!("dnsmasq.*{}", bridge);
    if let Ok(status) = Command::new("pgrep").args(["-f", &pattern]).output() {
        if status.status.success() && !status.stdout.is_empty() {
            return Ok(());
        }
    }
    let bin = std::env::var("TERRA_DNSMASQ")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "dnsmasq".into());
    // stdout must be null and stderr goes to a log file: dnsmasq
    // daemonizes, and a background child holding our PIPE open would
    // hang output() forever — but a plain file keeps errors diagnosable.
    let log_path = format!("/tmp/terra-dnsmasq-{}.log", bridge);
    let log_file =
        std::fs::File::create(&log_path).map_err(|e| format!("create dnsmasq log: {}", e))?;
    let out = Command::new(&bin)
        .args([
            &format!("--interface={}", bridge),
            "--bind-interfaces",
            "--except-interface=lo",
            // Ephemeral VMs churn fast; a 12h lease exhausted the
            // 151-address pool after ~150 create/destroy cycles within
            // half a day ("no address available" from dnsmasq, guests
            // boot without eth0). A short lease (renewed silently by
            // udhcpc while the VM lives) lets destroyed VMs' addresses
            // return to the pool quickly.
            &format!("--dhcp-range={},10m", dhcp_range_of(gateway)),
            // Point guests at dnsmasq itself (the NAT gateway), which
            // forwards to the host's resolver. Hardcoding public DNS
            // (8.8.8.8) breaks on hosts where outbound DNS is blocked or
            // an internal resolver is required.
            &format!("--dhcp-option=option:dns-server,{}", gateway),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log_file))
        .output()
        .map_err(|e| {
            format!(
                "dnsmasq not found (apt install dnsmasq, or set TERRA_DNSMASQ): {}",
                e
            )
        })?;
    if !out.status.success() {
        let detail = std::fs::read_to_string(&log_path).unwrap_or_default();
        return Err(format!("dnsmasq failed: {}", detail.trim()));
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

/// Stop the dnsmasq DHCP server bound to the bridge (best-effort).
pub fn stop_dhcp(bridge: &str) {
    let pattern = format!("dnsmasq.*{}", bridge);
    if let Ok(out) = Command::new("pgrep").args(["-f", &pattern]).output() {
        for pid in String::from_utf8_lossy(&out.stdout).lines() {
            let _ = Command::new("kill").arg(pid.trim()).output();
        }
    }
}

/// Tear down the NAT bridge, masquerade rule, and DHCP server.
/// Caller must guarantee no VM is using the bridge anymore.
pub fn teardown_nat_bridge(bridge: &str, gateway: &str, prefix: u8) -> Result<(), String> {
    stop_dhcp(bridge);
    // Best-effort: remove the tenant-isolation rule if present.
    let _ = run_ebtables(&[
        "-D", "FORWARD", "-i", "terra-+", "-o", "terra-+", "-j", "DROP",
    ]);
    let _ = run_iptables(&[
        "-t",
        "nat",
        "-D",
        "POSTROUTING",
        "-s",
        &format!("{}/{}", subnet_of(gateway), prefix),
        "-j",
        "MASQUERADE",
    ]);
    let exists = Command::new("ip")
        .args(["link", "show", bridge])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if exists {
        run_ip(&["link", "del", bridge])?;
    }
    tracing::info!(%bridge, "NAT bridge torn down");
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
