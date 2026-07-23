use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::api::*;
use crate::error::{ClientError, Result};

/// Client for Cloud Hypervisor's REST API over a Unix domain socket.
///
/// Communicates via raw HTTP/1.1 requests to the CH API socket.
/// No async runtime required — uses blocking `UnixStream` I/O.
pub struct ChClient {
    socket_path: String,
    timeout: Duration,
}

impl ChClient {
    /// Create a new client connected to the given CH API socket path.
    pub fn new(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Set the timeout for API requests.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Send an HTTP request to the CH API and return the response body.
    fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
        let mut stream = UnixStream::connect(&self.socket_path)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;

        let body_str = body.unwrap_or("");
        let req = format!(
            "{} {} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            method,
            path,
            body_str.len(),
            body_str
        );

        stream.write_all(req.as_bytes())?;
        stream.flush()?;

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf)?;
        let response = String::from_utf8_lossy(&buf).into_owned();

        // Parse status code and body from HTTP response
        let (status, body) = Self::parse_http_response(&response)?;

        if status >= 400 {
            return Err(ClientError::Api(format!(
                "CH API returned HTTP {}: {}",
                status, body
            )));
        }

        Ok((status, body))
    }

    /// Parse an HTTP/1.1 response, returning (status_code, body).
    fn parse_http_response(response: &str) -> Result<(u16, String)> {
        // Split headers from body
        let parts: Vec<&str> = response.splitn(2, "\r\n\r\n").collect();
        if parts.len() < 2 {
            return Err(ClientError::HttpParse(
                "Response missing body separator".into(),
            ));
        }

        let headers = parts[0];
        let body = parts[1].to_string();

        // Parse status line: "HTTP/1.1 200 OK"
        let first_line = headers
            .lines()
            .next()
            .ok_or_else(|| ClientError::HttpParse("Empty response".into()))?;

        let status_parts: Vec<&str> = first_line.split_whitespace().collect();
        if status_parts.len() < 2 {
            return Err(ClientError::HttpParse(format!(
                "Invalid status line: {}",
                first_line
            )));
        }

        let status: u16 = status_parts[1].parse().map_err(|_| {
            ClientError::HttpParse(format!("Invalid status code: {}", status_parts[1]))
        })?;

        Ok((status, body))
    }

    // -----------------------------------------------------------------------
    // VM Lifecycle API
    // -----------------------------------------------------------------------

    /// Create a new VM with the given configuration.
    /// Corresponds to `PUT /api/v1/vm.create`.
    pub fn vm_create(&self, config: &VmConfig) -> Result<VmInfo> {
        let body = serde_json::to_string(config)?;
        let (_status, resp) = self.request("PUT", "/api/v1/vm.create", Some(&body))?;
        Ok(serde_json::from_str(&resp)?)
    }

    /// Boot the VM — starts the vCPUs.
    /// Corresponds to `PUT /api/v1/vm.boot`.
    pub fn vm_boot(&self) -> Result<()> {
        self.request("PUT", "/api/v1/vm.boot", None)?;
        Ok(())
    }

    /// Shut down the VM gracefully.
    /// Corresponds to `PUT /api/v1/vm.shutdown`.
    pub fn vm_shutdown(&self) -> Result<()> {
        self.request("PUT", "/api/v1/vm.shutdown", None)?;
        Ok(())
    }

    /// Power off the VM immediately (no guest shutdown).
    /// Corresponds to `PUT /api/v1/vm.shutdown` with poweroff body.
    pub fn vm_power_off(&self) -> Result<()> {
        let body = r#"{"action":"power_off"}"#;
        self.request("PUT", "/api/v1/vm.shutdown", Some(body))?;
        Ok(())
    }

    /// Delete the VM — remove all resources.
    /// Corresponds to `PUT /api/v1/vm.delete`.
    pub fn vm_delete(&self) -> Result<()> {
        self.request("PUT", "/api/v1/vm.delete", None)?;
        Ok(())
    }

    /// Pause the VM.
    /// Corresponds to `PUT /api/v1/vm.pause`.
    pub fn vm_pause(&self) -> Result<()> {
        self.request("PUT", "/api/v1/vm.pause", None)?;
        Ok(())
    }

    /// Resume a paused VM.
    /// Corresponds to `PUT /api/v1/vm.resume`.
    pub fn vm_resume(&self) -> Result<()> {
        self.request("PUT", "/api/v1/vm.resume", None)?;
        Ok(())
    }

    /// Get VM information.
    /// Corresponds to `GET /api/v1/vm.info`.
    pub fn vm_info(&self) -> Result<VmDetails> {
        let (_status, resp) = self.request("GET", "/api/v1/vm.info", None)?;
        Ok(serde_json::from_str(&resp)?)
    }

    // -----------------------------------------------------------------------
    // Dynamic Resource API
    // -----------------------------------------------------------------------

    /// Resize vCPUs and/or memory. Pass `None` to leave a dimension unchanged.
    /// Corresponds to `PUT /api/v1/vm.resize`.
    pub fn vm_resize(&self, desired_vcpus: Option<u8>, desired_ram: Option<u64>) -> Result<()> {
        let config = ResizeConfig {
            desired_vcpus,
            desired_ram,
            balloon_size: None,
        };
        let body = serde_json::to_string(&config)?;
        self.request("PUT", "/api/v1/vm.resize", Some(&body))?;
        Ok(())
    }

    /// Resize balloon to reclaim memory from the guest.
    /// Corresponds to `PUT /api/v1/vm.resize` with balloon_size.
    pub fn vm_balloon(&self, size: u64) -> Result<()> {
        let config = ResizeConfig {
            desired_vcpus: None,
            desired_ram: None,
            balloon_size: Some(size),
        };
        let body = serde_json::to_string(&config)?;
        self.request("PUT", "/api/v1/vm.resize", Some(&body))?;
        Ok(())
    }

    /// Resize an existing disk.
    /// Corresponds to `PUT /api/v1/vm.resize-disk`.
    pub fn vm_resize_disk(&self, disk_id: &str, size: u64) -> Result<()> {
        let body = serde_json::json!({
            "id": disk_id,
            "size": size,
        });
        self.request("PUT", "/api/v1/vm.resize-disk", Some(&body.to_string()))?;
        Ok(())
    }

    /// Hot-add a new disk to the VM.
    /// Corresponds to `PUT /api/v1/vm.add-disk`.
    pub fn vm_add_disk(&self, path: &str) -> Result<()> {
        let body = serde_json::json!({
            "path": path,
        });
        self.request("PUT", "/api/v1/vm.add-disk", Some(&body.to_string()))?;
        Ok(())
    }

    /// Remove a hot-added disk.
    /// Corresponds to `PUT /api/v1/vm.remove-device`.
    pub fn vm_remove_disk(&self, disk_id: &str) -> Result<()> {
        let body = serde_json::json!({
            "id": disk_id,
        });
        self.request("PUT", "/api/v1/vm.remove-device", Some(&body.to_string()))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Snapshot API
    // -----------------------------------------------------------------------

    /// Take a full VM snapshot.
    /// Corresponds to `PUT /api/v1/vm.snapshot`.
    pub fn vm_snapshot(&self, snapshot_path: &str) -> Result<()> {
        let body = serde_json::json!({
            "destination_url": format!("file://{}", snapshot_path),
        });
        self.request("PUT", "/api/v1/vm.snapshot", Some(&body.to_string()))?;
        Ok(())
    }

    /// Restore a VM from a snapshot.
    /// Corresponds to `PUT /api/v1/vm.restore`.
    pub fn vm_restore(&self, snapshot_path: &str) -> Result<()> {
        let body = serde_json::json!({
            "source_url": format!("file://{}", snapshot_path),
        });
        self.request("PUT", "/api/v1/vm.restore", Some(&body.to_string()))?;
        Ok(())
    }
}
