//! host 资源控制器（M2 Task 5）。
//!
//! 经 vmm-api socket 管理 terra-vmm 进程生命周期：
//! create — 派生 terra-vmm 子进程；list — 查询运行中的 VM；
//! destroy — 发送 stop 命令。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use vmm_api::{Request, Response, StatusInfo};

/// VM 实例信息。
#[derive(Debug, Clone, Serialize)]
pub struct VmInfo {
    pub id: String,
    pub pid: u32,
    pub api_socket: PathBuf,
}

/// controller 错误类型。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("VM {0}: 未找到")]
    NotFound(String),
    #[error("VM {0}: 已存在")]
    AlreadyExists(String),
    #[error("API 通信错误: {0}")]
    Api(#[from] std::io::Error),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("API 返回错误: {0}")]
    ApiError(String),
    #[error("启动 terra-vmm 失败: {0}")]
    Spawn(std::io::Error),
}

/// 用于路由 sandbox 的 Handle。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxHandle {
    pub vm_id: String,
    pub sandbox_id: String,
}

/// 资源控制器。
pub struct Controller {
    vms: Mutex<HashMap<String, VmHandle>>,
}

struct VmHandle {
    info: VmInfo,
    child: Child,
}

impl Controller {
    pub fn new() -> Self {
        Controller::default()
    }
}

impl Default for Controller {
    fn default() -> Self {
        Controller {
            vms: Mutex::new(HashMap::new()),
        }
    }
}

impl Controller {
    /// 创建 VM：启动 terra-vmm 子进程。
    pub fn create(
        &self,
        id: &str,
        kernel: &Path,
        initrd: Option<&Path>,
        mem_mib: usize,
        vcpus: u8,
    ) -> Result<VmInfo, Error> {
        let mut vms = self.vms.lock().unwrap();
        if vms.contains_key(id) {
            return Err(Error::AlreadyExists(id.to_string()));
        }

        let socket_path = format!("/tmp/terra-{id}.sock");
        let _ = std::fs::remove_file(&socket_path);

        let mut cmd = Command::new("cargo");
        cmd.args([
            "run",
            "-p",
            "vmm",
            "--",
            "--kernel",
            kernel.to_str().unwrap(),
            "--api-socket",
            &socket_path,
            "--mem",
            &mem_mib.to_string(),
            "--max-vcpus",
            &vcpus.to_string(),
        ]);
        if let Some(i) = initrd {
            cmd.args(["--initrd", i.to_str().unwrap()]);
        }
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());

        let mut child = cmd.spawn().map_err(Error::Spawn)?;
        let pid = child.id();

        // 等待 socket 出现。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !Path::new(&socket_path).exists() {
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                return Err(Error::ApiError("socket 文件未出现（超时）".into()));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // 额外等待 VM 启动完成。
        std::thread::sleep(std::time::Duration::from_millis(500));

        let info = VmInfo {
            id: id.to_string(),
            pid,
            api_socket: PathBuf::from(&socket_path),
        };

        vms.insert(
            id.to_string(),
            VmHandle {
                info: info.clone(),
                child,
            },
        );

        Ok(info)
    }

    /// 列出所有 VM。
    pub fn list(&self) -> Vec<VmInfo> {
        self.vms
            .lock()
            .unwrap()
            .values()
            .map(|h| h.info.clone())
            .collect()
    }

    /// 销毁 VM：发送 stop 命令并等待退出。
    pub fn destroy(&self, id: &str) -> Result<(), Error> {
        let mut vms = self.vms.lock().unwrap();
        let mut handle = vms
            .remove(id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?;

        // 发送 stop 命令。
        send_api(&handle.info.api_socket, &Request::Stop)?;
        let _ = handle.child.wait();
        let _ = std::fs::remove_file(&handle.info.api_socket);

        Ok(())
    }

    /// 查询 VM 状态。
    pub fn status(&self, id: &str) -> Result<StatusInfo, Error> {
        let vms = self.vms.lock().unwrap();
        let handle = vms.get(id).ok_or_else(|| Error::NotFound(id.to_string()))?;
        let resp = send_api(&handle.info.api_socket, &Request::Status)?;
        match resp {
            Response::Ok { data: Some(ref d) } => Ok(serde_json::from_value(d.clone())?),
            Response::Error { message } => Err(Error::ApiError(message)),
            _ => Err(Error::ApiError("unexpected response".into())),
        }
    }
}

/// 向 terra-vmm 发送 API 请求并读取响应。
fn send_api(socket: &Path, req: &Request) -> std::io::Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    let mut json = serde_json::to_string(req).unwrap();
    json.push('\n');
    stream.write_all(json.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(serde_json::from_str(&line).unwrap_or(Response::Error { message: line }))
}
