//! Cloud Hypervisor API socket client.
//!
//! Provides a Rust client for communicating with Cloud Hypervisor's
//! HTTP API over a Unix domain socket. Covers VM lifecycle (create, boot,
//! shutdown), dynamic resource adjustment (resize vCPUs, memory, disk),
//! and snapshot operations.

pub mod api;
pub mod client;
pub mod error;

pub use client::ChClient;
pub use error::ClientError;
