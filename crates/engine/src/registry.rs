//! Rootfs registry — maps capabilities to pre-built rootfs images.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootfsEntry {
    pub name: String,
    pub path: String,
    pub capabilities: Vec<String>,
    pub size_mb: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Registry {
    pub images: Vec<RootfsEntry>,
}

impl Registry {
    /// Load from the default path or env var.
    pub fn load() -> Result<Self, String> {
        let path = std::env::var("TERRA_REGISTRY").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            format!("{}/.terra/registry.json", home)
        });

        if !std::path::Path::new(&path).exists() {
            return Ok(Self {
                images: vec![RootfsEntry {
                    name: "default".into(),
                    path: "target/guest/alpine-python.cpio".into(),
                    capabilities: vec!["python".into()],
                    size_mb: 50,
                }],
            });
        }

        let data = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {}", path, e))?;
        serde_json::from_str(&data).map_err(|e| format!("parse {}: {}", path, e))
    }

    /// Find the minimal rootfs that satisfies all requested capabilities.
    pub fn resolve(&self, required: &[String]) -> Result<&RootfsEntry, String> {
        let required_set: HashSet<&str> = required.iter().map(|s| s.as_str()).collect();

        let mut best: Option<&RootfsEntry> = None;
        for entry in &self.images {
            let caps: HashSet<&str> = entry.capabilities.iter().map(|s| s.as_str()).collect();
            if required_set.is_subset(&caps) {
                match best {
                    None => best = Some(entry),
                    Some(b) if caps.len() < b.capabilities.len() => best = Some(entry),
                    _ => {}
                }
            }
        }

        best.ok_or_else(|| {
            format!(
                "No rootfs satisfies capabilities: {:?}. Available: {:?}",
                required,
                self.images
                    .iter()
                    .map(|e| &e.capabilities)
                    .collect::<Vec<_>>()
            )
        })
    }

    /// List all registered images.
    pub fn list(&self) -> &[RootfsEntry] {
        &self.images
    }
}
