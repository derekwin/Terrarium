use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use terrarium_protocol::Command;

pub fn engine_socket() -> String {
    std::env::var("TERRA_SOCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/tmp/terra.sock".into())
}

pub fn send_to_engine(cmd: &Command) -> String {
    let addr = engine_socket();
    if let Some(tcp) = addr.strip_prefix("tcp://") {
        return match std::net::TcpStream::connect(tcp) {
            Ok(mut stream) => {
                // Remote servers may require TERRA_TOKEN as the first line.
                if let Ok(token) = std::env::var("TERRA_TOKEN") {
                    let _ = writeln!(stream, "{}", token);
                    let _ = stream.flush();
                }
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
        };
    }
    match UnixStream::connect(addr) {
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
