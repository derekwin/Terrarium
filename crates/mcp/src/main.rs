//! terra-mcp：MCP server，将 Terrarium sandbox 能力暴露为 MCP 工具。
//!
//! stdio transport，手写 JSON-RPC 2.0 子集，不依赖重型 MCP 框架。

use std::io::{BufRead, BufReader, Write};

use controller::Controller;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

impl JsonRpcResponse {
    fn ok(id: Option<Value>, result: Value) -> Self {
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Option<Value>, code: i64, msg: &str) -> Self {
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(serde_json::json!({"code": code, "message": msg})),
        }
    }
}

fn tools_list() -> Value {
    serde_json::json!({
        "tools": [
            {
                "name": "terra_create",
                "description": "Create a new sandbox",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Sandbox name"},
                        "image": {"type": "string", "default": "default"}
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "terra_run",
                "description": "Run a command in a sandbox",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sandbox_name": {"type": "string"},
                        "command": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["sandbox_name", "command"]
                }
            },
            {
                "name": "terra_terminate",
                "description": "Terminate a sandbox",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sandbox_name": {"type": "string"}
                    },
                    "required": ["sandbox_name"]
                }
            }
        ]
    })
}

fn main() {
    let stdin = std::io::stdin();
    let reader = BufReader::new(stdin.lock());
    let ctrl = Controller::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse::err(None, -32700, &format!("Parse error: {e}"));
                println!("{}", serde_json::to_string(&resp).unwrap());
                continue;
            }
        };

        let resp = match req.method.as_str() {
            "initialize" => JsonRpcResponse::ok(
                req.id.clone(),
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "serverInfo": {"name": "terra-mcp", "version": "0.1.0"},
                    "capabilities": {"tools": {}}
                }),
            ),
            "tools/list" => JsonRpcResponse::ok(req.id.clone(), tools_list()),
            "tools/call" => handle_tool_call(&ctrl, req.id.clone(), &req.params),
            _ => JsonRpcResponse::err(req.id, -32601, &format!("Method not found: {}", req.method)),
        };

        println!("{}", serde_json::to_string(&resp).unwrap());
        std::io::stdout().flush().ok();
    }
}

fn handle_tool_call(ctrl: &Controller, id: Option<Value>, params: &Value) -> JsonRpcResponse {
    let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").unwrap_or(&Value::Null);

    match tool_name {
        "terra_create" => {
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            match ctrl.create(
                name,
                &std::path::PathBuf::from("target/guest/bzImage"),
                None,
                128,
                1,
            ) {
                Ok(info) => JsonRpcResponse::ok(
                    id,
                    serde_json::json!({
                        "content": [{"type": "text", "text": format!("Sandbox created: {name} (pid={})", info.pid)}]
                    }),
                ),
                Err(e) => JsonRpcResponse::err(id, -1, &format!("Create failed: {e}")),
            }
        }
        "terra_terminate" => {
            let name = args
                .get("sandbox_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match ctrl.destroy(name) {
                Ok(()) => JsonRpcResponse::ok(
                    id,
                    serde_json::json!({
                        "content": [{"type": "text", "text": format!("Sandbox terminated: {name}")}]
                    }),
                ),
                Err(e) => JsonRpcResponse::err(id, -1, &format!("Terminate failed: {e}")),
            }
        }
        "terra_run" => JsonRpcResponse::ok(
            id,
            serde_json::json!({
                "content": [{"type": "text", "text": "terra_run: command execution via sandboxd pending"}]
            }),
        ),
        _ => JsonRpcResponse::err(id, -32601, &format!("Unknown tool: {tool_name}")),
    }
}
