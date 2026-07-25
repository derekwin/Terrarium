//! guest-proxy — host→guest command relay.
//!
//! Listens on Unix socket for command requests from host-side adapters.
//! Executes them locally and returns stdout/stderr/exit_code.
//! Not a sandbox — just a command forwarder.

mod sandbox;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;

const SOCKET_PATH: &str = "/tmp/sandboxd.sock";

fn main() {
    // For M2: Unix socket (guest-local). M3: vsock (host→guest).
    let _ = std::fs::remove_file(SOCKET_PATH);
    let listener = UnixListener::bind(SOCKET_PATH).expect("bind sandboxd socket");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(|| handle(stream));
            }
            Err(e) => eprintln!("accept: {}", e),
        }
    }
}

fn handle(mut stream: UnixStream) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }

    let cmd: serde_json::Value = match serde_json::from_str(line.trim()) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(
                stream,
                r#"{{"status":"error","message":"invalid json: {}"}}"#,
                e
            );
            return;
        }
    };

    let command = cmd["command"].as_str().unwrap_or("");
    match command {
        "exec" => exec_cmd(&mut stream, &cmd),
        "ping" => {
            let _ = writeln!(stream, r#"{{"status":"ok","message":"pong"}}"#);
        }
        _ => {
            let _ = writeln!(
                stream,
                r#"{{"status":"error","message":"unknown command: {}"}}"#,
                command
            );
        }
    }
}

fn exec_cmd(stream: &mut UnixStream, cmd: &serde_json::Value) {
    let args: Vec<String> = match cmd["args"].as_array() {
        Some(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => {
            let _ = writeln!(stream, r#"{{"status":"error","message":"missing args"}}"#);
            return;
        }
    };
    if args.is_empty() {
        let _ = writeln!(stream, r#"{{"status":"error","message":"empty args"}}"#);
        return;
    }

    let work_dir = cmd["work_dir"].as_str().unwrap_or("/tmp");

    match sandbox::exec_isolated(&args[0], &args, work_dir) {
        Ok(o) => {
            let resp = serde_json::json!({
                "status": "ok",
                "message": "command executed",
                "data": {
                    "stdout": o.stdout,
                    "stderr": o.stderr,
                    "exit_code": o.exit_code,
                }
            });
            let _ = writeln!(stream, "{}", resp);
        }
        Err(e) => {
            let _ = writeln!(stream, r#"{{"status":"error","message":"{}"}}"#, e);
        }
    }
}
