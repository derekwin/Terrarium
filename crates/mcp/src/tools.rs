use crate::client::send_to_engine;
use terrarium_protocol::Command;

pub fn tools_list() -> Vec<serde_json::Value> {
    vec![
        tool(
            "terra_vm_create",
            "Create a new VM for agent sandboxing.",
            vec![
                ("name", "string", "Unique VM name"),
                ("kernel", "string", "Path to kernel image"),
                ("initramfs", "string", "Path to initramfs (optional)"),
                (
                    "layers",
                    "array",
                    "virtiofs layer names, highest priority first, base last (optional)",
                ),
                ("cpus", "number", "vCPU count (default 2)"),
                ("memory_mb", "number", "Memory in MB (default 512)"),
            ],
        ),
        tool("terra_vm_list", "List all running VMs.", vec![]),
        tool(
            "terra_vm_info",
            "Get info about a running VM.",
            vec![("name", "string", "VM name")],
        ),
        tool(
            "terra_vm_resize",
            "Resize VM CPU or memory online.",
            vec![
                ("name", "string", "VM name"),
                ("cpus", "number", "Desired vCPU count (optional)"),
                (
                    "memory_bytes",
                    "number",
                    "Desired memory in bytes (optional)",
                ),
            ],
        ),
        tool(
            "terra_vm_kill",
            "Force-kill a VM (disks are kept).",
            vec![("name", "string", "VM name")],
        ),
        tool(
            "terra_vm_shutdown",
            "Gracefully shut down a VM (disks are kept).",
            vec![("name", "string", "VM name")],
        ),
        tool(
            "terra_vm_destroy",
            "Stop and deregister a VM.",
            vec![("name", "string", "VM name")],
        ),
        tool(
            "terra_exec",
            "Execute a command inside a VM via the guest agent.",
            vec![
                ("name", "string", "VM name"),
                (
                    "args",
                    "array",
                    "Command argv (e.g. [\"python3\",\"-c\",\"print(1)\"])",
                ),
                (
                    "timeout_secs",
                    "number",
                    "Timeout seconds (default 60, max 3600)",
                ),
            ],
        ),
        tool(
            "terra_pool_claim",
            "Claim an idle warm-pool VM and hot-plug the given layers.",
            vec![("layers", "array", "Layer names, highest priority first")],
        ),
        tool(
            "terra_pool_list",
            "List warm-pool slots and claim state.",
            vec![],
        ),
        tool(
            "terra_pool_release",
            "Release a claimed pool VM back to idle.",
            vec![("name", "string", "VM name")],
        ),
        tool(
            "terra_attach_fs",
            "Hot-plug a layered filesystem into a running VM.",
            vec![
                ("name", "string", "VM name"),
                ("layers", "array", "Layer names, highest priority first"),
            ],
        ),
        tool(
            "terra_detach_fs",
            "Detach a previously attached layered filesystem.",
            vec![("name", "string", "VM name")],
        ),
    ]
}

fn tool(name: &str, desc: &str, params: Vec<(&str, &str, &str)>) -> serde_json::Value {
    let properties: serde_json::Map<String, serde_json::Value> = params
        .iter()
        .map(|(k, t, d)| {
            (
                k.to_string(),
                serde_json::json!({"type": t, "description": d}),
            )
        })
        .collect();
    serde_json::json!({
        "name": name,
        "description": desc,
        "inputSchema": {"type": "object", "properties": properties}
    })
}

pub fn call_tool(name: &str, args: &serde_json::Value) -> String {
    let cmd = match name {
        "terra_vm_create" => {
            let mut c = Command::create(
                args.get("name").and_then(|a| a.as_str()).unwrap_or(""),
                args.get("kernel").and_then(|a| a.as_str()).unwrap_or(""),
            );
            if let Some(v) = args.get("cpus").and_then(|a| a.as_u64()) {
                c = c.with_cpus(v as u8);
            }
            if let Some(v) = args.get("memory_mb").and_then(|a| a.as_u64()) {
                c = c.with_memory_mb(v);
            }
            if let Some(v) = args.get("initramfs").and_then(|a| a.as_str()) {
                c = c.with_initramfs(v);
            }
            if let Some(arr) = args.get("layers").and_then(|a| a.as_array()) {
                let layers: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                if !layers.is_empty() {
                    c = c.with_layers(layers);
                }
            }
            send_to_engine(&c)
        }
        "terra_vm_list" => send_to_engine(&Command::new("list")),
        "terra_vm_info" => {
            let name = args.get("name").and_then(|a| a.as_str()).unwrap_or("");
            send_to_engine(&Command::new("info").with_name(name))
        }
        "terra_vm_shutdown" => {
            let name = args.get("name").and_then(|a| a.as_str()).unwrap_or("");
            send_to_engine(&Command::new("shutdown").with_name(name))
        }
        "terra_vm_kill" => {
            let name = args.get("name").and_then(|a| a.as_str()).unwrap_or("");
            send_to_engine(&Command::new("kill").with_name(name))
        }
        "terra_vm_resize" => {
            let name = args.get("name").and_then(|a| a.as_str()).unwrap_or("");
            let mut c = Command::new("resize").with_name(name);
            if let Some(v) = args.get("cpus").and_then(|a| a.as_u64()) {
                c = c.with_cpus(v as u8);
            }
            if let Some(v) = args.get("memory_bytes").and_then(|a| a.as_u64()) {
                c = c.with_memory_bytes(v);
            }
            send_to_engine(&c)
        }
        "terra_vm_destroy" => {
            let name = args.get("name").and_then(|a| a.as_str()).unwrap_or("");
            send_to_engine(&Command::new("destroy").with_name(name))
        }
        "terra_exec" => {
            let name = args.get("name").and_then(|a| a.as_str()).unwrap_or("");
            let argv: Vec<String> = args
                .get("args")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let mut c = Command::new("exec").with_name(name).with_args(argv);
            if let Some(t) = args.get("timeout_secs").and_then(|a| a.as_u64()) {
                c = c.with_timeout_secs(t);
            }
            // Retry while the guest agent is still booting.
            let mut resp = String::new();
            for _ in 0..8 {
                resp = send_to_engine(&c);
                if !resp.contains("handshake") && !resp.contains("connect guest vsock") {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            resp
        }
        "terra_pool_claim" => {
            let layers: Vec<String> = args
                .get("layers")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            send_to_engine(&Command::new("pool_claim").with_layers(layers))
        }
        "terra_pool_list" => send_to_engine(&Command::new("pool_list")),
        "terra_pool_release" => {
            let name = args.get("name").and_then(|a| a.as_str()).unwrap_or("");
            send_to_engine(&Command::new("pool_release").with_name(name))
        }
        "terra_attach_fs" => {
            let name = args.get("name").and_then(|a| a.as_str()).unwrap_or("");
            let layers: Vec<String> = args
                .get("layers")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            send_to_engine(
                &Command::new("attach_fs")
                    .with_name(name)
                    .with_layers(layers),
            )
        }
        "terra_detach_fs" => {
            let name = args.get("name").and_then(|a| a.as_str()).unwrap_or("");
            send_to_engine(&Command::new("detach_fs").with_name(name))
        }
        _ => r#"{"status":"error","error":"unknown tool"}"#.to_string(),
    };
    cmd
}
