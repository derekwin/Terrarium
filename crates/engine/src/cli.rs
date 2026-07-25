//! CLI client: connects to the daemon socket, sends a JSON command,
//! and prints the response.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

/// Send a JSON command to the daemon and return the parsed response.
pub fn send_command(
    socket_path: &str,
    cmd: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut stream = UnixStream::connect(socket_path).map_err(|e| {
        format!(
            "Failed to connect to daemon at {}: {} — is the daemon running?",
            socket_path, e
        )
    })?;

    let json = serde_json::to_string(&cmd).map_err(|e| format!("Serialize error: {}", e))?;
    writeln!(stream, "{}", json).map_err(|e| format!("Write error: {}", e))?;
    stream.flush().map_err(|e| format!("Flush error: {}", e))?;

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("Read error: {}", e))?;

    serde_json::from_str(line.trim()).map_err(|e| format!("Parse error: {} (raw: {})", e, line))
}

/// Build a JSON command from CLI arguments (args[0] = command name).
pub fn build_command(args: &[String]) -> serde_json::Value {
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("list");

    let mut cmd = serde_json::json!({"command": command});

    if let Some(name) = args.get(2) {
        cmd["name"] = serde_json::json!(name);
    }

    let flag = |name: &str| -> Option<String> {
        let mut i = 0;
        while i < args.len() {
            if args[i] == name && i + 1 < args.len() {
                return Some(args[i + 1].clone());
            }
            i += 1;
        }
        None
    };

    if let Some(v) = flag("--kernel") {
        cmd["kernel"] = serde_json::json!(v);
    }
    if let Some(v) = flag("--initramfs") {
        cmd["initramfs"] = serde_json::json!(v);
    }
    // Collect all --disk flags (repeatable)
    {
        let mut disks: Vec<serde_json::Value> = Vec::new();
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--disk" && i + 1 < args.len() {
                disks.push(serde_json::json!(args[i + 1]));
                i += 2;
            } else {
                i += 1;
            }
        }
        if !disks.is_empty() {
            cmd["disks"] = serde_json::json!(disks);
        }
    }
    if let Some(v) = flag("--cmdline") {
        cmd["cmdline"] = serde_json::json!(v);
    }
    if let Some(v) = flag("--cpus") {
        if let Ok(n) = v.parse::<u8>() {
            cmd["cpus"] = serde_json::json!(n);
        }
    }
    if let Some(v) = flag("--max-cpus") {
        if let Ok(n) = v.parse::<u8>() {
            cmd["max_cpus"] = serde_json::json!(n);
        }
    }
    if let Some(v) = flag("--memory") {
        if let Ok(n) = v.parse::<u64>() {
            cmd["memory_mb"] = serde_json::json!(n);
        }
    }
    if let Some(v) = flag("--memory-bytes") {
        if let Ok(n) = v.parse::<u64>() {
            cmd["memory_bytes"] = serde_json::json!(n);
        }
    }
    if let Some(v) = flag("--hotplug-memory") {
        if let Ok(n) = v.parse::<u64>() {
            cmd["hotplug_memory_gb"] = serde_json::json!(n);
        }
    }
    if let Some(v) = flag("--ch-binary") {
        cmd["ch_binary"] = serde_json::json!(v);
    }
    if let Some(v) = flag("--base-disk") {
        cmd["base_disk"] = serde_json::json!(v);
    }
    if let Some(v) = flag("--disk-size") {
        if let Ok(n) = v.parse::<u64>() {
            cmd["disk_size_gb"] = serde_json::json!(n);
        }
    }

    cmd
}

/// Print the daemon response in a human-friendly way.
pub fn print_response(cmd_name: &str, response: serde_json::Value) {
    let status = response["status"].as_str().unwrap_or("unknown");

    if status == "error" {
        let err = response["error"].as_str().unwrap_or("unknown error");
        eprintln!("ERROR: {}", err);
        std::process::exit(1);
    }

    let data = &response["data"];
    match cmd_name {
        "list" => {
            let count = data["count"].as_u64().unwrap_or(0);
            if count == 0 {
                println!("No running VMs.");
            } else {
                println!("Running VMs ({}):", count);
                if let Some(vms) = data["vms"].as_array() {
                    for vm in vms {
                        println!(
                            "  {}  pid={}  state={}",
                            vm["name"].as_str().unwrap_or("?"),
                            vm["pid"].as_u64().unwrap_or(0),
                            vm["state"].as_str().unwrap_or("?"),
                        );
                    }
                }
            }
        }
        "info" => {
            println!("VM: {}", data["name"].as_str().unwrap_or("?"));
            println!("  PID:    {}", data["pid"].as_u64().unwrap_or(0));
            println!("  State:  {}", data["state"].as_str().unwrap_or("?"));
            if let Some(cpus) = data["cpus"].as_object() {
                println!(
                    "  vCPUs:  boot={}, max={}",
                    cpus["boot"].as_u64().unwrap_or(0),
                    cpus["max"].as_u64().unwrap_or(0),
                );
            }
            if let Some(mem) = data["memory"].as_u64() {
                println!("  Memory: {} MB", mem / 1024 / 1024);
            }
        }
        _ => {
            // Generic: print message or full data
            if let Some(msg) = data["message"].as_str() {
                println!("{}", msg);
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&response).unwrap_or_default()
                );
            }
        }
    }
}
