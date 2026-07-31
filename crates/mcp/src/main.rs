//! terra-mcp — MCP Server exposing Terrarium Engine tools to AI agents.
//!
//! Communicates with the engine daemon via Unix socket JSON protocol.
//! Listens on stdio for JSON-RPC requests (MCP protocol).

mod client;
mod tools;

use std::io::{BufRead, BufReader, Write};

use crate::tools::{call_tool, tools_list, SessionRegistry};

const SERVER_NAME: &str = "terrarium-mcp";
const SERVER_VERSION: &str = "0.1.0";

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();
    // session name → engine sandbox id; persists for the process lifetime.
    let mut sessions: SessionRegistry = SessionRegistry::new();

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
        // JSON-RPC notifications have no "id" field — process but never respond.
        let is_notification = request.get("id").is_none();
        let response = handle_request(&request, &mut sessions);
        if is_notification {
            continue;
        }
        println!("{}", serde_json::to_string(&response).unwrap_or_default());
        std::io::stdout().flush().ok();
    }
}

fn handle_request(req: &serde_json::Value, sessions: &mut SessionRegistry) -> serde_json::Value {
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
            let result = call_tool(tool_name, args, sessions);
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
