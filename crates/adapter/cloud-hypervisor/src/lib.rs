//! Cloud Hypervisor adapter — self-contained VmAdapter implementation.
//!
//! Contains the CH HTTP API client (Unix socket) and VmAdapter trait impl.
//! No external CH SDK required — users install the official CH release binary.

pub mod api;
pub mod client;
mod config;
mod error;
mod fs;
mod handle;
mod process;

use adapter_traits::{AdapterError, VmAdapter, VmHandle, VmSpec};
use async_trait::async_trait;
use std::sync::Arc;

pub use client::ChClient;
pub use config::ChConfig;
pub use error::ClientError;
pub use fs::FsStack;

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

pub struct ChAdapter {
    config: Arc<ChConfig>,
}

impl ChAdapter {
    pub fn new(ch_binary: impl Into<String>) -> Self {
        Self {
            config: Arc::new(ChConfig::from_env(ch_binary)),
        }
    }
}

#[async_trait]
impl VmAdapter for ChAdapter {
    async fn create(&self, spec: &VmSpec) -> Result<Box<dyn VmHandle>, AdapterError> {
        spec.validate().map_err(AdapterError::invalid_argument)?;
        handle::spawn_vm(spec, self.config.clone()).await
    }
}
