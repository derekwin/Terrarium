//! VM specification used by the controller to define and launch VMs.

use serde::{Deserialize, Serialize};

use adapter_traits::VmName;

/// Full specification for creating and booting a VM.
///
/// This is the controller's canonical VM definition. It maps to
/// ch-client's VmConfig but adds controller-level metadata (name, labels).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSpec {
    /// Human-readable name for this VM. Must be unique within a manager.
    pub name: VmName,

    /// Path to the kernel image (bzImage / vmlinux.bin).
    pub kernel: String,

    /// Kernel command line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,

    /// Number of vCPUs to boot with.
    #[serde(default = "default_boot_vcpus")]
    pub boot_vcpus: u8,

    /// Maximum vCPUs this VM can scale to. None = fixed, no hotplug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vcpus: Option<u8>,

    /// Boot memory in megabytes.
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u64,

    /// Maximum memory in MB (enables hotplug). None = fixed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<u64>,

    /// Path to the Cloud Hypervisor binary.
    #[serde(default = "default_ch_binary")]
    pub ch_binary: String,

    /// API socket path for this VM's CH instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_socket: Option<String>,

    /// Path to initramfs image (cpio). If set, CH boots with --initramfs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initramfs: Option<String>,

    /// Disk images to attach (each generates a --disk flag).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<String>,

    /// Base disk image path for qcow2 overlay (shared, read-only layer).
    /// When set, a per-VM qcow2 overlay is created with this as backing file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_disk: Option<String>,

    /// Virtual disk size for overlay in GB.
    #[serde(default = "default_disk_size_gb")]
    pub disk_size_gb: u64,

    /// Optional labels for categorization / filtering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<(String, String)>,
}

fn default_boot_vcpus() -> u8 {
    2
}
fn default_memory_mb() -> u64 {
    512
}
fn default_ch_binary() -> String {
    "cloud-hypervisor".to_string()
}
fn default_disk_size_gb() -> u64 {
    20
}

impl VmSpec {
    /// Create a minimal spec with just a name and kernel path.
    pub fn new(name: impl Into<String>, kernel: impl Into<String>) -> Self {
        let name = VmName::new(name).expect("VmSpec::new called with invalid name");
        Self {
            name,
            kernel: kernel.into(),
            cmdline: Some("console=ttyS0 quiet init=/init".into()),
            boot_vcpus: default_boot_vcpus(),
            max_vcpus: Some(16),
            memory_mb: default_memory_mb(),
            max_memory_mb: None,
            ch_binary: default_ch_binary(),
            api_socket: None,
            initramfs: None,
            disks: vec![],
            base_disk: None,
            disk_size_gb: default_disk_size_gb(),
            labels: vec![],
        }
    }

    /// Set the kernel command line.
    pub fn cmdline(mut self, cmdline: impl Into<String>) -> Self {
        self.cmdline = Some(cmdline.into());
        self
    }

    /// Set vCPU range. boot is required, max can be None (no hotplug).
    pub fn cpus(mut self, boot: u8, max: Option<u8>) -> Self {
        self.boot_vcpus = boot;
        self.max_vcpus = max;
        self
    }

    /// Set boot memory + optional hotplug ceiling in MB.
    pub fn memory_range(mut self, boot_mb: u64, max_mb: Option<u64>) -> Self {
        self.memory_mb = boot_mb;
        self.max_memory_mb = max_mb;
        self
    }

    /// Set boot memory in MB (no hotplug).
    pub fn memory_mb(mut self, mb: u64) -> Self {
        self.memory_mb = mb;
        self
    }

    /// Enable memory hotplug with a ceiling in GB (convenience wrapper).
    pub fn hotplug_memory_gb(mut self, gb: u64) -> Self {
        self.max_memory_mb = Some(gb * 1024);
        self
    }

    /// Set the CH binary path.
    pub fn ch_binary(mut self, path: impl Into<String>) -> Self {
        self.ch_binary = path.into();
        self
    }

    /// Set a custom API socket path. If not set, a default is generated from the name.
    #[allow(dead_code)]
    pub fn api_socket(mut self, path: impl Into<String>) -> Self {
        self.api_socket = Some(path.into());
        self
    }

    /// Set the initramfs path (cpio archive).
    pub fn initramfs(mut self, path: impl Into<String>) -> Self {
        self.initramfs = Some(path.into());
        self
    }

    /// Attach a disk image.
    pub fn disk(mut self, path: impl Into<String>) -> Self {
        self.disks.push(path.into());
        self
    }

    /// Set base disk for qcow2 overlay. Each VM gets a per-instance qcow2
    /// with this base as the backing file (COW semantics).
    pub fn base_disk(mut self, path: impl Into<String>) -> Self {
        self.base_disk = Some(path.into());
        self
    }

    /// Set the virtual disk size for the qcow2 overlay in GB.
    /// The overlay file grows dynamically via COW up to this ceiling.
    #[allow(dead_code)]
    pub fn disk_size_gb(mut self, gb: u64) -> Self {
        self.disk_size_gb = gb;
        self
    }

    /// Return the API socket path, using a default if not explicitly set.
    pub fn api_socket_path(&self) -> String {
        self.api_socket
            .clone()
            .unwrap_or_else(|| format!("/tmp/terra-{}.sock", self.name))
    }

    /// Build the CH command-line arguments for this spec.
    pub fn to_ch_args(&self) -> Vec<String> {
        let socket = self.api_socket_path();
        let mut args = vec![
            "--api-socket".to_string(),
            socket,
            "--kernel".to_string(),
            self.kernel.clone(),
            "--cpus".to_string(),
            format!(
                "boot={},max={}",
                self.boot_vcpus,
                self.max_vcpus.unwrap_or(self.boot_vcpus)
            ),
        ];

        if let Some(ref cmdline) = self.cmdline {
            args.push("--cmdline".to_string());
            args.push(cmdline.clone());
        }

        if let Some(ref initramfs) = self.initramfs {
            args.push("--initramfs".to_string());
            args.push(initramfs.clone());
        }

        for disk in &self.disks {
            args.push("--disk".to_string());
            args.push(format!("path={}", disk));
        }

        if let Some(max_mb) = self.max_memory_mb {
            args.push("--memory".to_string());
            args.push(format!(
                "size={}M,hotplug_method=virtio-mem,hotplug_size={}G",
                self.memory_mb,
                max_mb / 1024
            ));
        } else {
            args.push("--memory".to_string());
            args.push(format!("size={}M", self.memory_mb));
        }

        args.push("--serial".to_string());
        args.push("tty".to_string());
        args.push("--console".to_string());
        args.push("off".to_string());

        args
    }
}
