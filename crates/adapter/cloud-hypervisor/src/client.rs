use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::UnixStream;

use crate::api::*;
use crate::error::{ClientError, Result};

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
        let stream = UnixStream::connect(&self.socket_path).await?;
        let (reader, mut writer) = stream.into_split();

        let body_str = body.unwrap_or("");
        let req = uds_http::build_request(method, path, body_str);

        tokio::time::timeout(self.timeout, writer.write_all(req.as_bytes()))
            .await
            .map_err(|_| ClientError::Timeout)?
            .map_err(ClientError::Io)?;

        // Read response with timeout
        let (status, body) = tokio::time::timeout(self.timeout, Self::read_response(reader))
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
        let status = uds_http::parse_status(&status_line).map_err(ClientError::HttpParse)?;

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
        let body = serde_json::json!({"payload": config});
        self.request("PUT", "/api/v1/vm.create", Some(&body.to_string()))
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

    pub async fn vm_power_off(&self) -> Result<()> {
        let body = r#"{"action":"power_off"}"#;
        self.request("PUT", "/api/v1/vm.shutdown", Some(body))
            .await?;
        Ok(())
    }

    pub async fn vm_delete(&self) -> Result<()> {
        self.request("PUT", "/api/v1/vm.delete", None).await?;
        Ok(())
    }

    pub async fn vm_pause(&self) -> Result<()> {
        self.request("PUT", "/api/v1/vm.pause", None).await?;
        Ok(())
    }

    pub async fn vm_resume(&self) -> Result<()> {
        self.request("PUT", "/api/v1/vm.resume", None).await?;
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

    pub async fn vm_resize_disk(&self, disk_id: &str, size: u64) -> Result<()> {
        let body = serde_json::json!({
            "id": disk_id,
            "desired_size": size,
        });
        self.request("PUT", "/api/v1/vm.resize-disk", Some(&body.to_string()))
            .await?;
        Ok(())
    }

    pub async fn vm_add_disk(&self, path: &str) -> Result<()> {
        let body = serde_json::json!({
            "path": path,
        });
        self.request("PUT", "/api/v1/vm.add-disk", Some(&body.to_string()))
            .await?;
        Ok(())
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

    pub async fn vm_snapshot(&self, snapshot_path: &str) -> Result<()> {
        let body = serde_json::json!({
            "destination_url": format!("file://{}", snapshot_path),
        });
        self.request("PUT", "/api/v1/vm.snapshot", Some(&body.to_string()))
            .await?;
        Ok(())
    }

    pub async fn vm_restore(&self, snapshot_path: &str) -> Result<()> {
        let body = serde_json::json!({
            "source_url": format!("file://{}", snapshot_path),
        });
        self.request("PUT", "/api/v1/vm.restore", Some(&body.to_string()))
            .await?;
        Ok(())
    }
}
