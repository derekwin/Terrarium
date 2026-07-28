use crate::manager::VmManager;
use terrarium_protocol::Response;

pub(crate) fn cmd_net_up() -> Response {
    match terrarium_network::ensure_nat_bridge(
        terrarium_network::DEFAULT_BRIDGE,
        terrarium_network::DEFAULT_GATEWAY,
        terrarium_network::DEFAULT_PREFIX,
    ) {
        Ok(()) => Response::ok_msg("NAT bridge up (terra0, 10.200.0.1/24)"),
        Err(e) => Response::err(e),
    }
}

pub(crate) fn cmd_net_down(mgr: &VmManager) -> Response {
    let in_use = mgr.net_in_use();
    if in_use > 0 {
        return Response::err(format!(
            "{} VM(s) still using the bridge — destroy them first",
            in_use
        ));
    }
    match terrarium_network::teardown_nat_bridge(
        terrarium_network::DEFAULT_BRIDGE,
        terrarium_network::DEFAULT_GATEWAY,
        terrarium_network::DEFAULT_PREFIX,
    ) {
        Ok(()) => Response::ok_msg("NAT bridge, DHCP, and masquerade removed"),
        Err(e) => Response::err(e),
    }
}

pub(crate) fn cmd_net_list(mgr: &VmManager) -> Response {
    let vms: Vec<_> = mgr
        .list_names()
        .into_iter()
        .filter(|n| mgr.has_net(n))
        .map(|n| {
            let tap = format!(
                "terra-{}",
                n.chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .take(9)
                    .collect::<String>()
            );
            serde_json::json!({"name": n, "tap": tap, "bridge": "terra0"})
        })
        .collect();
    Response::ok(serde_json::json!({
        "bridge": "terra0",
        "gateway": "10.200.0.1/24",
        "mode": "nat",
        "vms": vms,
    }))
}
