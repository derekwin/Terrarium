//! terra-mcp — MCP Server exposing Terrarium Engine tools to AI agents.
//!
//! Communicates with the engine daemon via Unix socket JSON protocol.
//! Listens on stdio for JSON-RPC requests (MCP protocol).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use terrarium_protocol::Command;

const ENGINE_SOCKET: &str = "/tmp/terra.sock";
const SERVER_NAME: &str = "terrarium-mcp";
const SERVER_VERSION: &str = "0.1.0";

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("read error: {}", e);
                break;
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        let request: serde_json::Value = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => {
                continue;
            }
        };
        let response = handle_request(&request);
        println!("{}", serde_json::to_string(&response).unwrap_or_default());
        std::io::stdout().flush().ok();
    }
}

fn handle_request(req: &serde_json::Value) -> serde_json::Value {
    let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = req["method"].as_str().unwrap_or("");

    match method {
        // JSON-RPC notifications have no "id" field — do not respond.
        "" if id == serde_json::Value::Null => serde_json::Value::Null,
        "initialize" => jsonrpc_ok(
            id,
            &serde_json::json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                "capabilities": {"tools": {}}
            }),
        ),
        "tools/list" => jsonrpc_ok(id, &serde_json::json!({"tools": tools_list()})),
        "tools/call" => {
            let tool_name = req["params"]["name"].as_str().unwrap_or("");
            let args = &req["params"]["arguments"];
            let result = call_tool(tool_name, args);
            jsonrpc_ok(
                id,
                &serde_json::json!({"content": [{"type": "text", "text": result}]}),
            )
        }
        _ => jsonrpc_ok(id, &serde_json::json!({})),
    }
}

fn jsonrpc_ok(id: serde_json::Value, result: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn tools_list() -> Vec<serde_json::Value> {
    vec![
        tool(
            "terra_vm_create",
            "Create a new VM for agent sandboxing.",
            vec![
                ("name", "string", "Unique VM name"),
                ("kernel", "string", "Path to kernel image"),
                ("initramfs", "string", "Path to initramfs (optional)"),
                ("cpus", "number", "vCPU count (default 2)"),
                ("memory_mb", "number", "Memory in MB (default 512)"),
                (
                    "rootfs_disk",
                    "string",
                    "Root filesystem qcow2 path (optional)",
                ),
            ],
        ),
        tool("terra_vm_list", "List all running VMs.", vec![]),
        tool(
            "terra_vm_info",
            "Get info about a running VM.",
            vec![("name", "string", "VM name")],
        ),
        tool(
            "terra_vm_shutdown",
            "Gracefully shut down a VM.",
            vec![("name", "string", "VM name")],
        ),
        tool(
            "terra_vm_destroy",
            "Shut down and delete VM disk.",
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

fn call_tool(name: &str, args: &serde_json::Value) -> String {
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
            if let Some(v) = args.get("rootfs_disk").and_then(|a| a.as_str()) {
                c = c.with_base_disk(v);
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
        "terra_vm_destroy" => {
            let name = args.get("name").and_then(|a| a.as_str()).unwrap_or("");
            send_to_engine(&Command::new("destroy").with_name(name))
        }
        _ => r#"{"status":"error","error":"unknown tool"}"#.to_string(),
    };
    cmd
}

fn send_to_engine(cmd: &Command) -> String {
    match UnixStream::connect(ENGINE_SOCKET) {
        Ok(mut stream) => {
            let json = serde_json::to_string(cmd).unwrap_or_default();
            let _ = writeln!(stream, "{}", json);
            let _ = stream.flush();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                line.trim().to_string()
            } else {
                r#"{"status":"error","error":"no response from engine"}"#.to_string()
            }
        }
        Err(e) => format!(
            r#"{{"status":"error","error":"engine unavailable: {}"}}"#,
            e
        ),
    }
}
