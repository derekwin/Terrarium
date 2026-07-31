use std::collections::HashMap;

use crate::client::send_to_engine;
use base64::Engine as _;
use terrarium_protocol::Command;

/// All MCP sandbox sessions share one tenant VM ("mcp") — the VM is
/// the isolation boundary, each session is an engine sandbox inside it
/// with its own workdir, confined by sandlock on every exec. The tenant
/// is a platform concern: administrators clean it up with
/// `terra sandbox destroy-tenant mcp`.
const MCP_TENANT: &str = "mcp";

/// Session name → engine sandbox id (sb-<hex>). Lives for the MCP
/// process lifetime; sessions are created on first use and reused
/// afterwards, so agents never manage sandbox lifecycle explicitly.
pub type SessionRegistry = HashMap<String, String>;

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
            "Execute a command inside a sandbox session, confined by sandlock by default. Sessions are created on first use (per session name) and reused afterwards.",
            vec![
                (
                    "args",
                    "array",
                    "Command argv (e.g. [\"python3\",\"-c\",\"print(1)\"])",
                ),
                (
                    "session",
                    "string",
                    "Session name; omitted → the shared \"default\" session. Different names = isolated workdirs (optional)",
                ),
                (
                    "sandboxed",
                    "boolean",
                    "Run under sandlock permission isolation (default true)",
                ),
                (
                    "cwd",
                    "string",
                    "Working directory inside the session; default is the session workdir (optional)",
                ),
                (
                    "layers",
                    "array",
                    "Layer names for the session environment — only used when the session is first created (optional)",
                ),
                (
                    "timeout_secs",
                    "number",
                    "Timeout seconds (default 60, max 3600)",
                ),
            ],
        ),
        tool(
            "terra_session_read",
            "Read a file from a sandbox session.",
            vec![
                ("path", "string", "Absolute path inside the session"),
                (
                    "session",
                    "string",
                    "Session name; omitted → the shared \"default\" session (optional)",
                ),
            ],
        ),
        tool(
            "terra_session_write",
            "Write a file into a sandbox session.",
            vec![
                ("path", "string", "Absolute path inside the session"),
                ("content", "string", "File content"),
                (
                    "session",
                    "string",
                    "Session name; omitted → the shared \"default\" session (optional)",
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

pub fn call_tool(name: &str, args: &serde_json::Value, sessions: &mut SessionRegistry) -> String {
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
        "terra_exec" => terra_exec(args, sessions),
        "terra_session_read" => terra_session_read(args, sessions),
        "terra_session_write" => terra_session_write(args, sessions),
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

// ── session-scoped tools ──────────────────────────────────────────

/// Send a command, retrying transient guest-agent boot races (a VM is
/// "Running" before its vsock agent answers).
fn send_retry(cmd: &Command) -> String {
    let mut resp = String::new();
    for _ in 0..8 {
        resp = send_to_engine(cmd);
        if !resp.contains("handshake") && !resp.contains("vsock") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    resp
}

/// Resolve the sandbox id for a session name, creating the session on
/// first use via `sandbox_create` (idempotent engine-side: an existing
/// tenant VM is reused, so all MCP sessions share one VM with isolated
/// workdirs).
fn ensure_session(
    sessions: &mut SessionRegistry,
    name: &str,
    layers: &[String],
) -> Result<String, String> {
    if let Some(id) = sessions.get(name) {
        return Ok(id.clone());
    }
    let mut c = Command::new("sandbox_create").with_tenant(MCP_TENANT);
    // A cold-booted VM needs a layered rootfs to even start its guest
    // agent — default to the system base when no layers were given.
    if layers.is_empty() {
        c = c.with_layers(vec!["base".into()]);
    } else {
        c = c.with_layers(layers.to_vec());
    }
    if let Ok(k) = std::env::var("TERRA_KERNEL") {
        if !k.is_empty() {
            c = c.with_kernel(&k);
        }
    }
    if let Ok(i) = std::env::var("TERRA_INITRAMFS") {
        if !i.is_empty() {
            c = c.with_initramfs(&i);
        }
    }
    let resp = send_retry(&c);
    let data =
        resp_data(&resp).ok_or_else(|| format!("session '{}' create failed: {}", name, resp))?;
    let id = data["id"]
        .as_str()
        .ok_or_else(|| format!("session '{}' create: no id in response: {}", name, resp))?
        .to_string();
    sessions.insert(name.to_string(), id.clone());
    Ok(id)
}

/// Extract the `data` object of an ok response.
fn resp_data(resp: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(resp).ok()?;
    if v["status"].as_str() == Some("ok") {
        v.get("data").cloned()
    } else {
        None
    }
}

/// Single-quote a string for safe shell embedding.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Join argv into a single shell command line.
fn sh_join(args: &[String]) -> String {
    args.iter()
        .map(|a| sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn session_arg(args: &serde_json::Value) -> String {
    args.get("session")
        .and_then(|a| a.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string()
}

fn terra_exec(args: &serde_json::Value, sessions: &mut SessionRegistry) -> String {
    let session = session_arg(args);
    let argv: Vec<String> = args
        .get("args")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let layers: Vec<String> = args
        .get("layers")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let sandboxed = args
        .get("sandboxed")
        .and_then(|a| a.as_bool())
        .unwrap_or(true);
    let cwd = args.get("cwd").and_then(|a| a.as_str());

    let id = match ensure_session(sessions, &session, &layers) {
        Ok(id) => id,
        Err(e) => return error_json(&e),
    };
    let mut final_args = argv;
    if let Some(cwd) = cwd {
        final_args = vec![
            "sh".into(),
            "-c".into(),
            format!("cd {} && {}", sh_quote(cwd), sh_join(&final_args)),
        ];
    }
    let mut c = Command::new("sandbox_exec")
        .with_id(&id)
        .with_args(final_args)
        .with_sandbox(sandboxed);
    if let Some(t) = args.get("timeout_secs").and_then(|a| a.as_u64()) {
        c = c.with_timeout_secs(t);
    }
    send_retry(&c)
}

fn terra_session_read(args: &serde_json::Value, sessions: &mut SessionRegistry) -> String {
    let session = session_arg(args);
    let path = args.get("path").and_then(|a| a.as_str()).unwrap_or("");
    if path.is_empty() {
        return error_json("missing 'path'");
    }
    let id = match ensure_session(sessions, &session, &[]) {
        Ok(id) => id,
        Err(e) => return error_json(&e),
    };
    send_retry(
        &Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec!["cat".into(), path.into()])
            .with_sandbox(true),
    )
}

fn terra_session_write(args: &serde_json::Value, sessions: &mut SessionRegistry) -> String {
    let session = session_arg(args);
    let path = args.get("path").and_then(|a| a.as_str()).unwrap_or("");
    let content = args.get("content").and_then(|a| a.as_str()).unwrap_or("");
    if path.is_empty() {
        return error_json("missing 'path'");
    }
    let id = match ensure_session(sessions, &session, &[]) {
        Ok(id) => id,
        Err(e) => return error_json(&e),
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
    send_retry(
        &Command::new("sandbox_exec")
            .with_id(&id)
            .with_args(vec![
                "sh".into(),
                "-c".into(),
                format!("echo {} | base64 -d > {}", b64, sh_quote(path)),
            ])
            .with_sandbox(true),
    )
}

fn error_json(msg: &str) -> String {
    serde_json::json!({"status": "error", "error": msg}).to_string()
}
