//! Shared HTTP/1.1 parsing utilities for Unix domain socket clients.
//!
//! Used by VM adapters (Cloud Hypervisor, Firecracker) that communicate
//! with their respective VMM APIs over HTTP on UDS.

/// Parse an HTTP status line like "HTTP/1.1 200 OK\r\n" to extract the status code.
pub fn parse_status(status_line: &str) -> Result<u16, String> {
    let parts: Vec<&str> = status_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(format!("invalid status line: {}", status_line.trim()));
    }
    parts[1]
        .parse()
        .map_err(|_| format!("invalid status code: {}", parts[1]))
}

/// Parse HTTP headers to extract Content-Length.
/// Returns (content_length, headers_parsed).
pub fn parse_content_length<'a>(lines: impl Iterator<Item = &'a str>) -> usize {
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(val) = trimmed
            .to_lowercase()
            .strip_prefix("content-length:")
            .map(|s| s.trim().to_string())
        {
            return val.parse().unwrap_or(0);
        }
    }
    0
}

/// Build an HTTP/1.1 request string.
pub fn build_request(method: &str, path: &str, body: &str) -> String {
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
