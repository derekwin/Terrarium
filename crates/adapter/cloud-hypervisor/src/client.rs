use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::UnixStream;

use crate::api::*;
use crate::error::{ClientError, Result};

// -----------------------------------------------------------------------
// HTTP/1.1 helpers (inlined from the former uds-http crate)
// -----------------------------------------------------------------------

/// Parse an HTTP status line like "HTTP/1.1 200 OK\r\n" to extract the status code.
fn parse_status(status_line: &str) -> std::result::Result<u16, String> {
    let parts: Vec<&str> = status_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(format!("invalid status line: {}", status_line.trim()));
    }
    parts[1]
        .parse()
        .map_err(|_| format!("invalid status code: {}", parts[1]))
}

/// Build an HTTP/1.1 request string.
fn build_request(method: &str, path: &str, body: &str) -> String {
    format!(
        "{} {} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {}",
        method,
        path,
        body.len(),
        body
    )
}

/// Client for Cloud Hypervisor's REST API over a Unix domain socket.
///
/// Communicates via raw HTTP/1.1 requests to the CH API socket.
/// Uses `tokio::net::UnixStream` for async I/O.
pub struct ChClient {
    socket_path: String,
    timeout: Duration,
}

impl ChClient {
    pub fn new(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Send an HTTP request to the CH API and return the response body.
    async fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
        self.request_with_timeout(self.timeout, method, path, body)
            .await
    }

    /// Send an HTTP request with an explicit timeout.
    ///
    /// Long-running operations (snapshot, restore) need a much larger
    /// budget than interactive ones (info, resize).
    async fn request_with_timeout(
        &self,
        timeout: Duration,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String)> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        let (reader, mut writer) = stream.into_split();

        let body_str = body.unwrap_or("");
        let req = build_request(method, path, body_str);

        tokio::time::timeout(timeout, writer.write_all(req.as_bytes()))
            .await
            .map_err(|_| ClientError::Timeout)?
            .map_err(ClientError::Io)?;

        // Read response with timeout
        let (status, body) = tokio::time::timeout(timeout, Self::read_response(reader))
            .await
            .map_err(|_| ClientError::Timeout)??;

        if status >= 400 {
            return Err(ClientError::Api(format!(
                "CH API returned HTTP {}: {}",
                status, body
            )));
        }

        Ok((status, body))
    }

    async fn read_response(reader: OwnedReadHalf) -> Result<(u16, String)> {
        let mut buf_reader = BufReader::new(reader);

        // Read status line
        let mut status_line = String::new();
        buf_reader.read_line(&mut status_line).await?;
        let status = parse_status(&status_line).map_err(ClientError::HttpParse)?;

        // Read headers, tracking Content-Length
        let mut content_length: usize = 0;
        loop {
            let mut line = String::new();
            buf_reader.read_line(&mut line).await?;
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(val) = line
                .to_lowercase()
                .strip_prefix("content-length:")
                .map(|s| s.trim().to_string())
            {
                content_length = val.parse().unwrap_or(0);
            }
        }

        // Read body
        let mut resp_body = vec![0u8; content_length];
        if content_length > 0 {
            buf_reader.read_exact(&mut resp_body).await?;
        }
        let body = String::from_utf8_lossy(&resp_body).into_owned();

        Ok((status, body))
    }

    // -----------------------------------------------------------------------
    // VM Lifecycle API
    // -----------------------------------------------------------------------

    pub async fn vm_create(&self, config: &VmConfig) -> Result<()> {
        let body = serde_json::to_string(config)?;
        self.request("PUT", "/api/v1/vm.create", Some(&body))
            .await?;
        Ok(())
    }

    pub async fn vm_boot(&self) -> Result<()> {
        self.request("PUT", "/api/v1/vm.boot", None).await?;
        Ok(())
    }

    pub async fn vm_shutdown(&self) -> Result<()> {
        self.request("PUT", "/api/v1/vm.shutdown", None).await?;
        Ok(())
    }

    pub async fn vm_delete(&self) -> Result<()> {
        self.request("PUT", "/api/v1/vm.delete", None).await?;
        Ok(())
    }

    pub async fn vm_info(&self) -> Result<VmDetails> {
        let (_status, resp) = self.request("GET", "/api/v1/vm.info", None).await?;
        Ok(serde_json::from_str(&resp)?)
    }

    // -----------------------------------------------------------------------
    // Dynamic Resource API
    // -----------------------------------------------------------------------

    pub async fn vm_resize(
        &self,
        desired_vcpus: Option<u8>,
        desired_ram: Option<u64>,
    ) -> Result<()> {
        let config = ResizeConfig {
            desired_vcpus,
            desired_ram,
            balloon_size: None,
        };
        let body = serde_json::to_string(&config)?;
        self.request("PUT", "/api/v1/vm.resize", Some(&body))
            .await?;
        Ok(())
    }

    pub async fn vm_balloon(&self, size: u64) -> Result<()> {
        let config = ResizeConfig {
            desired_vcpus: None,
            desired_ram: None,
            balloon_size: Some(size),
        };
        let body = serde_json::to_string(&config)?;
        self.request("PUT", "/api/v1/vm.resize", Some(&body))
            .await?;
        Ok(())
    }

    /// Hot-plug a virtiofs device backed by an already-running virtiofsd.
    /// Returns the device id reported by CH (needed for remove-device).
    pub async fn vm_add_fs(&self, tag: &str, socket: &str) -> Result<String> {
        let body = serde_json::json!({
            "tag": tag,
            "socket": socket,
        });
        let (_status, resp) = self
            .request("PUT", "/api/v1/vm.add-fs", Some(&body.to_string()))
            .await?;
        // CH returns PciDeviceInfo{"id": "..."} on success.
        let id = serde_json::from_str::<serde_json::Value>(&resp)
            .ok()
            .and_then(|v| v["id"].as_str().map(String::from))
            .unwrap_or_else(|| format!("_fs{}", tag));
        Ok(id)
    }

    pub async fn vm_remove_disk(&self, disk_id: &str) -> Result<()> {
        let body = serde_json::json!({
            "id": disk_id,
        });
        self.request("PUT", "/api/v1/vm.remove-device", Some(&body.to_string()))
            .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Snapshot API
    // -----------------------------------------------------------------------

    /// Snapshot/restore can take minutes for large-memory VMs — far beyond
    /// the interactive default timeout.
    const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(600);

    pub async fn vm_snapshot(&self, snapshot_path: &str) -> Result<()> {
        let body = serde_json::json!({
            "destination_url": format!("file://{}", snapshot_path),
        });
        self.request_with_timeout(
            Self::SNAPSHOT_TIMEOUT,
            "PUT",
            "/api/v1/vm.snapshot",
            Some(&body.to_string()),
        )
        .await?;
        Ok(())
    }
}
