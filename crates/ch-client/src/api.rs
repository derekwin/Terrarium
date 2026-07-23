use serde::{Deserialize, Serialize};

/// VM configuration used when creating a new VM.
#[derive(Debug, Clone, Serialize)]
pub struct VmConfig {
    /// Path to the kernel image (vmlinux.bin / bzImage).
    pub kernel: String,
    /// Kernel command line parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,
    /// vCPU configuration.
    pub cpus: CpusConfig,
    /// Memory configuration.
    pub memory: MemoryConfig,
    /// Disk configuration.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub disks: Vec<DiskConfig>,
    /// Console mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console: Option<String>,
}

/// vCPU configuration for a VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpusConfig {
    /// Number of vCPUs to boot with.
    #[serde(rename = "boot_vcpus")]
    pub boot: u8,
    /// Maximum number of vCPUs the VM can scale to.
    #[serde(rename = "max_vcpus")]
    pub max: u8,
}

/// Memory configuration for a VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Amount of RAM in bytes.
    pub size: u64,
    /// Hotplug method and size. Uses virtio-mem for dynamic resizing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotplug_size: Option<u64>,
}

/// Disk configuration for a VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskConfig {
    /// Path to the disk image.
    pub path: String,
    /// Optional disk identifier for later operations like resize-disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Resize parameters: vCPUs and/or memory.
#[derive(Debug, Clone, Serialize)]
pub struct ResizeConfig {
    /// Desired number of vCPUs (if changing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired_vcpus: Option<u8>,
    /// Desired amount of RAM in bytes (if changing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired_ram: Option<u64>,
    /// Desired balloon size in bytes (if changing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balloon_size: Option<u64>,
}

/// VM state as returned by the API.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct VmInfo {
    /// VM identifier assigned by CH.
    #[serde(default)]
    pub id: String,
    /// Current VM state (Created, Running, Shutdown).
    #[serde(default)]
    pub state: String,
}

/// VM information from vm.info endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct VmDetails {
    /// Current vCPU configuration.
    #[serde(default)]
    pub cpus: Option<CpusConfig>,
    /// Current memory configuration.
    #[serde(default)]
    pub memory: Option<MemoryConfig>,
    /// Current state.
    #[serde(default)]
    pub state: String,
}
