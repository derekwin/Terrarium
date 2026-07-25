//! terra-mcp — MCP Server exposing Terrarium Engine tools to AI agents.
//!
//! Communicates with the engine daemon via Unix socket JSON protocol.
//! Listens on stdio for JSON-RPC requests (MCP protocol).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

const ENGINE_SOCKET: &str = "/tmp/terra.sock";
const SERVER_NAME: &str = "terrarium-mcp";
const SERVER_VERSION: &str = "0.1.0";

fn main() {
    tracing_subscriber::fmt::init();
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();

    while reader.read_line(&mut line).is_ok() {
        if line.trim().is_empty() {
            line.clear();
            continue;
        }
        let request: serde_json::Value = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => {
                line.clear();
                continue;
            }
        };
        let response = handle_request(&request);
        println!("{}", serde_json::to_string(&response).unwrap_or_default());
        std::io::stdout().flush().ok();
        line.clear();
    }
}

fn handle_request(req: &serde_json::Value) -> serde_json::Value {
    let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = req["method"].as_str().unwrap_or("");

    match method {
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
                (
                    "toolfs_disk",
                    "string",
                    "Tool layer qcow2 path (repeatable, optional)",
                ),
            ],
        ),
        tool("terra_vm_list", "List all running VMs.", vec![]),
        tool(
            "terra_sandbox_exec",
            "Execute a command in a sandbox.",
            vec![("args", "string", "Command and arguments (space-separated)")],
        ),
        tool(
            "terra_file_read",
            "Read a file from the sandbox filesystem.",
            vec![("path", "string", "File path inside the sandbox")],
        ),
        tool(
            "terra_file_write",
            "Write a file to the sandbox filesystem.",
            vec![
                ("path", "string", "File path inside the sandbox"),
                ("content", "string", "File content"),
            ],
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
            let mut c = serde_json::json!({"command": "create"});
            if let Some(v) = args.get("name") {
                c["name"] = v.clone();
            }
            if let Some(v) = args.get("kernel") {
                c["kernel"] = v.clone();
            }
            if let Some(v) = args.get("initramfs") {
                c["initramfs"] = v.clone();
            }
            if let Some(v) = args.get("cpus") {
                c["cpus"] = v.clone();
            }
            if let Some(v) = args.get("memory_mb") {
                c["memory_mb"] = v.clone();
            }
            if let Some(v) = args.get("rootfs_disk") {
                c["base_disk"] = v.clone();
            }
            if let Some(v) = args.get("toolfs_disk") {
                c["tool_layers"] = serde_json::json!([v.as_str().unwrap_or("")]);
            }
            send_to_engine(&c)
        }
        "terra_vm_list" => send_to_engine(&serde_json::json!({"command": "list"})),
        "terra_sandbox_exec" => {
            let raw = args.get("args").and_then(|a| a.as_str()).unwrap_or("");
            let parts: Vec<&str> = raw.split_whitespace().collect();
            send_to_engine(&serde_json::json!({"command": "exec", "args": parts}))
        }
        "terra_file_read" => {
            let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
            send_to_engine(&serde_json::json!({"command": "file_read", "file_path": path}))
        }
        "terra_file_write" => {
            let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|c| c.as_str()).unwrap_or("");
            send_to_engine(
                &serde_json::json!({"command": "file_write", "file_path": path, "file_content": content}),
            )
        }
        _ => "Unknown tool".to_string(),
    };
    cmd
}

fn send_to_engine(cmd: &serde_json::Value) -> String {
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
                r#"{"status":"error","message":"no response from engine"}"#.to_string()
            }
        }
        Err(e) => format!(
            r#"{{"status":"error","message":"engine unavailable: {}"}}"#,
            e
        ),
    }
}
