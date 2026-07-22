//! sandboxd：guest 内 sandbox 守护进程（M2 Task 1-2）。
//!
//! 监听 `/run/sandboxd.sock` Unix socket，接收 JSON 命令并执行。
//! 静态 musl 编译，不依赖 guest 动态库。
//!
//! 命令面：exec / exec_sandboxed / status / terminate / logs。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;

use serde::{Deserialize, Serialize};

mod sandbox;

/// 客户端请求。
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    /// 执行命令（普通子进程，无隔离）。
    Exec {
        argv: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        #[serde(default = "default_cwd")]
        cwd: String,
    },
    /// 在隔离沙箱中执行命令（namespace + overlay + cgroup + Landlock + seccomp）。
    ExecSandboxed {
        argv: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        #[serde(default = "default_cwd")]
        cwd: String,
        sandbox: sandbox::SandboxConfigInput,
    },
    /// 查询守护进程状态。
    Status,
    /// 停止守护进程。
    Terminate,
    /// 查询沙箱日志。
    Logs,
}

fn default_cwd() -> String {
    "/".to_string()
}

/// 服务端响应。
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Response {
    Ok {
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    Error {
        message: String,
    },
}

impl Response {
    fn ok() -> Self {
        Response::Ok { data: None }
    }
    fn ok_data(data: serde_json::Value) -> Self {
        Response::Ok { data: Some(data) }
    }
    fn err(msg: impl Into<String>) -> Self {
        Response::Error {
            message: msg.into(),
        }
    }
}

fn main() {
    let socket_path = "/run/sandboxd.sock";

    // 确保 /run 目录存在（某些 initramfs 可能没有）。
    let _ = std::fs::create_dir_all("/run");
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path).expect("bind sandboxd socket");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let should_exit = handle_client(stream);
                if should_exit {
                    break;
                }
            }
            Err(e) => {
                eprintln!("sandboxd: accept error: {e}");
                break;
            }
        }
    }

    let _ = std::fs::remove_file(socket_path);
}

fn handle_client(mut stream: UnixStream) -> bool {
    let reader = BufReader::new(stream.try_clone().unwrap());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("sandboxd: read error: {e}");
                return false;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = send(&mut stream, &Response::err(format!("parse: {e}")));
                continue;
            }
        };

        match req {
            Request::Exec { argv, env, cwd } => {
                let resp = handle_exec(&argv, &env, &cwd);
                let _ = send(&mut stream, &resp);
            }
            Request::ExecSandboxed {
                argv,
                env,
                cwd,
                sandbox: sb,
            } => {
                let resp = handle_exec_sandboxed(&argv, &env, &cwd, sb);
                let _ = send(&mut stream, &resp);
            }
            Request::Status => {
                let _ = send(&mut stream, &Response::ok());
            }
            Request::Terminate => {
                let _ = send(&mut stream, &Response::ok());
                return true;
            }
            Request::Logs => {
                let _ = send(&mut stream, &Response::err("logs not implemented (Task 2)"));
            }
        }
    }
    false
}

fn handle_exec(
    argv: &[String],
    env: &std::collections::HashMap<String, String>,
    cwd: &str,
) -> Response {
    if argv.is_empty() {
        return Response::err("empty argv");
    }

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null());

    match cmd.output() {
        Ok(output) => {
            use serde_json::json;
            let data = json!({
                "exit_code": output.status.code().unwrap_or(-1),
                "stdout": base64_encode(&output.stdout),
                "stderr": base64_encode(&output.stderr),
            });
            Response::ok_data(data)
        }
        Err(e) => Response::err(format!("exec: {e}")),
    }
}

fn handle_exec_sandboxed(
    argv: &[String],
    env: &std::collections::HashMap<String, String>,
    cwd: &str,
    sb_config: sandbox::SandboxConfigInput,
) -> Response {
    use serde_json::json;
    let config: sandbox::SandboxConfig = sb_config.into();
    match sandbox::exec_in_sandbox(&config, argv, env, cwd) {
        Ok(result) => {
            let data = json!({
                "exit_code": result.exit_code,
                "stdout": base64_encode(&result.stdout),
                "stderr": base64_encode(&result.stderr),
            });
            Response::ok_data(data)
        }
        Err(e) => Response::err(format!("sandbox exec: {e}")),
    }
}

fn send(stream: &mut UnixStream, resp: &Response) -> std::io::Result<()> {
    let mut json = serde_json::to_string(resp).unwrap();
    json.push('\n');
    stream.write_all(json.as_bytes())
}

fn base64_encode(data: &[u8]) -> String {
    // 简单的 base64 编码（无外部依赖）。
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3f) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3f) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64() {
        assert_eq!("SGVsbG8=", base64_encode(b"Hello"));
        assert_eq!("dGVzdA==", base64_encode(b"test"));
        assert_eq!("YWJj", base64_encode(b"abc"));
    }

    #[test]
    fn test_exec_echo() {
        let resp = handle_exec(
            &["echo".into(), "hello".into()],
            &std::collections::HashMap::new(),
            "/",
        );
        match resp {
            Response::Ok { data: Some(ref d) } => {
                assert_eq!(0, d["exit_code"].as_i64().unwrap());
                let stdout = d["stdout"].as_str().unwrap();
                assert!(!stdout.is_empty());
            }
            _ => panic!("unexpected response"),
        }
    }

    #[test]
    fn test_exec_missing() {
        let resp = handle_exec(
            &["nonexistent_binary_xyz".into()],
            &std::collections::HashMap::new(),
            "/",
        );
        match resp {
            Response::Error { .. } => {}
            _ => panic!("expected error"),
        }
    }
}
