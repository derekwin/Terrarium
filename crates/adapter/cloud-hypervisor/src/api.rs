use serde::{Deserialize, Serialize};

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
    /// Hotpluggable memory size in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotplug_size: Option<u64>,
    /// Hotplug method: "Acpi" or "VirtioMem". CH defaults to "Acpi", so
    /// virtio-mem setups must set this explicitly alongside hotplug_size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotplug_method: Option<String>,
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
}

/// VM information from vm.info endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct VmDetails {
    /// VM configuration.
    #[serde(default)]
    pub config: Option<VmInfoConfig>,
    /// Current state.
    #[serde(default)]
    pub state: String,
    /// Actual memory size reported by the VMM.
    #[serde(default)]
    pub memory_actual_size: Option<u64>,
}

/// The config section of a vm.info response.
#[derive(Debug, Clone, Deserialize)]
pub struct VmInfoConfig {
    /// Current vCPU configuration.
    #[serde(default)]
    pub cpus: Option<CpusConfig>,
    /// Current memory configuration.
    #[serde(default)]
    pub memory: Option<MemoryConfig>,
}
