//! controller ↔ terra-vmm 的 API socket 协议。
//!
//! M1 Task 2 定义：Unix seqpacket 传输，serde_json 文本帧（每帧一条完整 JSON，
//! 以换行分隔）。选择理由见 ADR 0004。
//!
//! M1 命令面：`stop`、`status`、`resize_mem`。

use serde::{Deserialize, Serialize};

/// 客户端发往 terra-vmm 的请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// 干净停止 VMM 进程（返回 Ok 即开始退出序列）。
    Stop,
    /// 查询 VM 运行状态。
    Status,
    /// 调整 guest 内存大小（字节），M1 Task 3 接入。
    ResizeMem {
        /// 新的内存字节数。
        bytes: u64,
    },
    /// 调整 guest 磁盘容量（字节）。
    ResizeDisk { bytes: u64 },
}

/// terra-vmm 返回给客户端的响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// 请求成功完成。
    Ok {
        /// 可选负载（如 Status 查询结果）。
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    /// 请求执行出错。
    Error {
        /// 错误描述。
        message: String,
    },
}

impl Response {
    /// 构建成功响应（无负载）。
    pub fn ok() -> Self {
        Response::Ok { data: None }
    }

    /// 构建成功响应（带负载）。
    pub fn ok_with(data: serde_json::Value) -> Self {
        Response::Ok { data: Some(data) }
    }

    /// 构建错误响应。
    pub fn error(msg: impl Into<String>) -> Self {
        Response::Error {
            message: msg.into(),
        }
    }
}

/// `status` 命令的查询结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    /// 所配置的 guest 内存（MiB）。
    pub mem_size_mib: usize,
    /// vCPU 数量上限。
    pub max_vcpu_count: u8,
    /// 是否挂载了磁盘。
    pub disk_attached: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_stop_roundtrip() {
        let req = Request::Stop;
        let json = serde_json::to_string(&req).unwrap();
        let decoded: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, Request::Stop));
    }

    #[test]
    fn test_request_status_roundtrip() {
        let req = Request::Status;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(r#"{"cmd":"status"}"#, json);
        let decoded: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, Request::Status));
    }

    #[test]
    fn test_request_resize_roundtrip() {
        let req = Request::ResizeMem {
            bytes: 256 * 1024 * 1024,
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: Request = serde_json::from_str(&json).unwrap();
        match decoded {
            Request::ResizeMem { bytes } => assert_eq!(256 * 1024 * 1024, bytes),
            _ => panic!("期望 ResizeMem"),
        }
    }

    #[test]
    fn test_response_ok_roundtrip() {
        let resp = Response::ok();
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: Response = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, Response::Ok { data: None }));
    }

    #[test]
    fn test_response_error_roundtrip() {
        let resp = Response::error("test error");
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: Response = serde_json::from_str(&json).unwrap();
        match decoded {
            Response::Error { message } => assert_eq!("test error", message),
            _ => panic!("期望 Error"),
        }
    }
}
