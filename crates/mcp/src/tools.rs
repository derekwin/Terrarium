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
/// "Running" before its vsock agent answers). The transport is
/// injectable so unit tests can swap the engine socket for a scripted
/// responder.
fn send_retry_with(send: &impl Fn(&Command) -> String, cmd: &Command) -> String {
    let mut resp = String::new();
    for _ in 0..8 {
        resp = send(cmd);
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
    send: &impl Fn(&Command) -> String,
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
    let resp = send_retry_with(send, &c);
    let data =
        resp_data(&resp).ok_or_else(|| format!("session '{}' create failed: {}", name, resp))?;
    let id = data["id"]
        .as_str()
        .ok_or_else(|| format!("session '{}' create: no id in response: {}", name, resp))?
        .to_string();
    sessions.insert(name.to_string(), id.clone());
    Ok(id)
}

/// True if the response is the engine's exact miss message for a dead
/// sandbox record (crates/engine/src/commands/sandbox.rs — sandbox_get
/// miss). The structured `error` field is compared with exact equality,
/// so nothing else can trigger a self-heal: ok responses carrying exec
/// stdout (even text echoing the cached id — the id is visible to the
/// agent as its `/workdir/<id>`), `VM '<name>' not found`, `Session
/// '<id>' not found`, and pool errors are all structurally distinct.
fn is_sandbox_miss(resp: &str, id: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(resp)
        .ok()
        .and_then(|v| {
            v["error"]
                .as_str()
                .map(|e| e == format!("Sandbox '{}' not found", id))
        })
        .unwrap_or(false)
}

/// Execute against a session, self-healing a stale cached id after an
/// engine restart (the engine's sandbox records are gone). When the
/// engine's structured response reports the cached sandbox as missing,
/// drop the id and retry once with a freshly created session — the new
/// sandbox gets a fresh engine-allocated id, so a second miss is a real
/// error and is returned as-is (no loop). The miss check is exact-match
/// on the error field, so exec output can never masquerade as a miss.
fn session_exec(
    sessions: &mut SessionRegistry,
    session: &str,
    layers: &[String],
    build: impl Fn(&str) -> Command,
    send: &impl Fn(&Command) -> String,
) -> String {
    let id = match ensure_session(sessions, session, layers, send) {
        Ok(id) => id,
        Err(e) => return error_json(&e),
    };
    let resp = send_retry_with(send, &build(&id));
    if is_sandbox_miss(&resp, &id) {
        sessions.remove(session);
        match ensure_session(sessions, session, layers, send) {
            Ok(new_id) => send_retry_with(send, &build(&new_id)),
            Err(e) => error_json(&e),
        }
    } else {
        resp
    }
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

    let mut final_args = argv;
    if let Some(cwd) = cwd {
        final_args = vec![
            "sh".into(),
            "-c".into(),
            format!("cd {} && {}", sh_quote(cwd), sh_join(&final_args)),
        ];
    }
    session_exec(
        sessions,
        &session,
        &layers,
        |id| {
            let mut c = Command::new("sandbox_exec")
                .with_id(id)
                .with_args(final_args.clone())
                .with_sandbox(sandboxed);
            if let Some(t) = args.get("timeout_secs").and_then(|a| a.as_u64()) {
                c = c.with_timeout_secs(t);
            }
            c
        },
        &send_to_engine,
    )
}

fn terra_session_read(args: &serde_json::Value, sessions: &mut SessionRegistry) -> String {
    let session = session_arg(args);
    let path = args.get("path").and_then(|a| a.as_str()).unwrap_or("");
    if path.is_empty() {
        return error_json("missing 'path'");
    }
    session_exec(
        sessions,
        &session,
        &[],
        |id| {
            Command::new("sandbox_exec")
                .with_id(id)
                .with_args(vec!["cat".into(), path.into()])
                .with_sandbox(true)
        },
        &send_to_engine,
    )
}

fn terra_session_write(args: &serde_json::Value, sessions: &mut SessionRegistry) -> String {
    let session = session_arg(args);
    let path = args.get("path").and_then(|a| a.as_str()).unwrap_or("");
    let content = args.get("content").and_then(|a| a.as_str()).unwrap_or("");
    if path.is_empty() {
        return error_json("missing 'path'");
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
    session_exec(
        sessions,
        &session,
        &[],
        |id| {
            Command::new("sandbox_exec")
                .with_id(id)
                .with_args(vec![
                    "sh".into(),
                    "-c".into(),
                    format!("echo {} | base64 -d > {}", b64, sh_quote(path)),
                ])
                .with_sandbox(true)
        },
        &send_to_engine,
    )
}

fn error_json(msg: &str) -> String {
    serde_json::json!({"status": "error", "error": msg}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted engine stub: replays `script` responses in order and
    /// records every command sent (as "command:id") for assertions.
    struct MockEngine {
        script: std::cell::RefCell<std::collections::VecDeque<String>>,
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl MockEngine {
        fn new(script: &[&str]) -> Self {
            Self {
                script: std::cell::RefCell::new(script.iter().map(|s| s.to_string()).collect()),
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn send(&self, cmd: &Command) -> String {
            self.calls.borrow_mut().push(format!(
                "{}:{}",
                cmd.command,
                cmd.id.clone().unwrap_or_default()
            ));
            self.script
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| r#"{"status":"error","error":"unexpected engine call"}"#.into())
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    fn exec_builder(id: &str) -> Command {
        Command::new("sandbox_exec")
            .with_id(id)
            .with_args(vec!["echo".into(), "hi".into()])
            .with_sandbox(true)
    }

    #[test]
    fn heals_stale_cached_session_after_engine_restart() {
        let mut sessions = SessionRegistry::new();
        sessions.insert("default".into(), "sb-deadbeef".into());
        let engine = MockEngine::new(&[
            r#"{"status":"error","error":"Sandbox 'sb-deadbeef' not found"}"#,
            r#"{"status":"ok","data":{"id":"sb-cafe1234","vm":"tenant-mcp","workdir":"/workdir/sb-cafe1234","pool":false}}"#,
            r#"{"status":"ok","data":{"stdout":"hello\n","stderr":"","exit_code":0}}"#,
        ]);
        let send = |c: &Command| engine.send(c);

        let resp = session_exec(&mut sessions, "default", &[], exec_builder, &send);

        assert!(
            resp.contains("\"stdout\":\"hello"),
            "exec must succeed after self-heal: {resp}"
        );
        assert_eq!(
            sessions.get("default").map(String::as_str),
            Some("sb-cafe1234")
        );
        assert_eq!(
            engine.calls(),
            vec![
                "sandbox_exec:sb-deadbeef".to_string(),
                "sandbox_create:".to_string(),
                "sandbox_exec:sb-cafe1234".to_string(),
            ]
        );
    }

    #[test]
    fn keeps_working_session_untouched() {
        let mut sessions = SessionRegistry::new();
        sessions.insert("s1".into(), "sb-alive".into());
        let engine = MockEngine::new(&[
            r#"{"status":"ok","data":{"stdout":"ok\n","stderr":"","exit_code":0}}"#,
        ]);
        let send = |c: &Command| engine.send(c);

        let resp = session_exec(&mut sessions, "s1", &[], exec_builder, &send);

        assert!(resp.contains("\"exit_code\":0"), "{resp}");
        assert_eq!(sessions.get("s1").map(String::as_str), Some("sb-alive"));
        assert_eq!(engine.calls(), vec!["sandbox_exec:sb-alive".to_string()]);
    }

    #[test]
    fn vm_missing_error_does_not_trigger_self_heal() {
        let mut sessions = SessionRegistry::new();
        sessions.insert("s1".into(), "sb-gone".into());
        let engine =
            MockEngine::new(&[r#"{"status":"error","error":"VM 'tenant-mcp' not found"}"#]);
        let send = |c: &Command| engine.send(c);

        let resp = session_exec(&mut sessions, "s1", &[], exec_builder, &send);

        assert!(resp.contains("VM 'tenant-mcp' not found"), "{resp}");
        assert_eq!(
            sessions.get("s1").map(String::as_str),
            Some("sb-gone"),
            "a VM-level error must not drop the cached sandbox id"
        );
        assert_eq!(engine.calls(), vec!["sandbox_exec:sb-gone".to_string()]);
    }

    #[test]
    fn echoed_sandbox_text_in_stdout_does_not_trigger_self_heal() {
        // The cached sandbox id is NOT secret — the agent's cwd is
        // `/workdir/<id>`, and exec stdout is embedded in ok responses.
        // A command echoing `Sandbox '<cached-id>' not found` must not be
        // mistaken for a dead record (that would drop the session, wipe
        // its workdir via re-creation, and re-execute the command).
        let mut sessions = SessionRegistry::new();
        sessions.insert("s1".into(), "sb-alive".into());
        let engine = MockEngine::new(&[
            r#"{"status":"ok","data":{"stdout":"Sandbox 'sb-alive' not found\n","stderr":"","exit_code":0}}"#,
        ]);
        let send = |c: &Command| engine.send(c);

        let resp = session_exec(&mut sessions, "s1", &[], exec_builder, &send);

        assert!(resp.contains("\"exit_code\":0"), "{resp}");
        assert_eq!(sessions.get("s1").map(String::as_str), Some("sb-alive"));
        assert_eq!(engine.calls(), vec!["sandbox_exec:sb-alive".to_string()]);
    }

    #[test]
    fn second_miss_is_returned_without_retry_loop() {
        let mut sessions = SessionRegistry::new();
        sessions.insert("s1".into(), "sb-dead".into());
        let engine = MockEngine::new(&[
            r#"{"status":"error","error":"Sandbox 'sb-dead' not found"}"#,
            r#"{"status":"ok","data":{"id":"sb-fresh","vm":"tenant-mcp","workdir":"/workdir/sb-fresh","pool":false}}"#,
            r#"{"status":"error","error":"Sandbox 'sb-fresh' not found"}"#,
        ]);
        let send = |c: &Command| engine.send(c);

        let resp = session_exec(&mut sessions, "s1", &[], exec_builder, &send);

        assert!(resp.contains("Sandbox 'sb-fresh' not found"), "{resp}");
        assert_eq!(
            sessions.get("s1").map(String::as_str),
            Some("sb-fresh"),
            "re-created id stays cached even though its exec failed"
        );
        assert_eq!(
            engine.calls(),
            vec![
                "sandbox_exec:sb-dead".to_string(),
                "sandbox_create:".to_string(),
                "sandbox_exec:sb-fresh".to_string(),
            ],
            "exactly one retry — no second self-heal"
        );
    }

    #[test]
    fn fresh_session_creation_failure_is_reported() {
        let mut sessions = SessionRegistry::new();
        let engine =
            MockEngine::new(&[r#"{"status":"error","error":"pool VM info failed: nope"}"#]);
        let send = |c: &Command| engine.send(c);

        let resp = session_exec(&mut sessions, "s1", &[], exec_builder, &send);

        assert!(resp.contains("create failed"), "{resp}");
        assert!(sessions.is_empty());
    }

    #[test]
    fn re_create_failure_after_heal_drops_stale_id() {
        // Stale id detected → re-create fails → the error is returned and
        // the stale id is gone from the registry, so the next call starts
        // fresh instead of failing against the dead sandbox again.
        let mut sessions = SessionRegistry::new();
        sessions.insert("s1".into(), "sb-dead".into());
        let engine = MockEngine::new(&[
            r#"{"status":"error","error":"Sandbox 'sb-dead' not found"}"#,
            r#"{"status":"error","error":"pool VM info failed: nope"}"#,
        ]);
        let send = |c: &Command| engine.send(c);

        let resp = session_exec(&mut sessions, "s1", &[], exec_builder, &send);

        assert!(resp.contains("create failed"), "{resp}");
        assert!(
            !sessions.contains_key("s1"),
            "stale id must be removed so the next call re-creates"
        );
        assert_eq!(
            engine.calls(),
            vec![
                "sandbox_exec:sb-dead".to_string(),
                "sandbox_create:".to_string(),
            ]
        );
    }
}
