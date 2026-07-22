//! VM 生命周期与 vCPU run 循环。
//!
//! 流程：`Vm::new` 完成「建 VM → 建内存 → 建 irqchip → 加载内核/initrd →
//! 写 boot params → 初始化 vCPU」；`Vm::run` 进入 vCPU 退出处理循环。

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use std::thread;

use kvm_bindings::{kvm_pit_config, kvm_userspace_memory_region};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use linux_loader::cmdline::Cmdline;
use linux_loader::configurator::{linux::LinuxBootConfigurator, BootConfigurator, BootParams};
use linux_loader::loader::{self, bzimage::BzImage, KernelLoader};
use tracing::{debug, warn};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

use crate::arch;
use crate::device::{Balloon, Blk, DeviceManager, Mem, Net, Rng, VirtioMmio, Watchdog};
use crate::rtc::{Rtc, RTC_PORT_DATA, RTC_PORT_INDEX};
use crate::serial::{Serial, SERIAL_PORT_BASE, SERIAL_PORT_SIZE};

/// 内存下限（MiB）：再小放不下内核 + initramfs。
const MIN_MEM_SIZE_MIB: usize = 64;
/// 内存上限：M0 使用单段内存布局，不跨越 3GiB 低端 MMIO hole（M1 引入 virtio-mmio
/// 设备窗口时再处理多段布局）。
const MAX_MEM_SIZE: u64 = 3 << 30;

/// VM 配置。
///
/// 为 M1「启动预创建 + 运行调整」资源模型预留扩展空间（如 `max_vcpu_count`），
/// 但 M0 只实现单 vCPU。
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// guest 内存大小（MiB），默认 128。
    pub mem_size_mib: usize,
    /// 内核 bzImage 路径。
    pub kernel_path: PathBuf,
    /// initramfs 路径（可选）。
    pub initrd_path: Option<PathBuf>,
    /// 内核命令行，默认 `console=ttyS0 reboot=k panic=-1 tsc=reliable`
    /// （tsc=reliable 跳过 PIT 校准，KVM 下宿主 TSC 可信，microVM 常规做法）。
    pub kernel_cmdline: String,
    /// vCPU 上限。M0 仅支持 1；M1 将按此上限预创建 vCPU、guest 内逻辑上下线。
    pub max_vcpu_count: u8,
    /// virtio-blk 后端磁盘路径（M1 Task 1；None 时不注册 blk 设备）。
    pub disk_path: Option<PathBuf>,
    /// virtio-mem 热插拔内存上限（MiB，M1 Task 3；None 时不注册 virtio-mem 设备）。
    pub mem_hotplug_max: Option<usize>,
    /// virtio-net 后端 fd 路径或 tap 设备名（M1.5 Task 0；None 时不注册 net 设备）。
    pub net_backend: Option<PathBuf>,
}

impl VmConfig {
    /// 以默认配置创建，只需指定内核路径。
    pub fn new(kernel_path: impl Into<PathBuf>) -> Self {
        VmConfig {
            kernel_path: kernel_path.into(),
            ..Default::default()
        }
    }
}

impl Default for VmConfig {
    fn default() -> Self {
        VmConfig {
            mem_size_mib: 128,
            kernel_path: PathBuf::new(),
            initrd_path: None,
            kernel_cmdline: "console=ttyS0 reboot=k panic=-1 tsc=reliable acpi=off".to_string(),
            max_vcpu_count: 1,
            disk_path: None,
            mem_hotplug_max: None,
            net_backend: None,
        }
    }
}

/// vmm-core 错误类型。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 打开 /dev/kvm 失败。
    #[error("打开 /dev/kvm 失败: {0}")]
    Kvm(kvm_ioctls::Error),
    /// 创建 VM 失败。
    #[error("创建 VM 失败: {0}")]
    CreateVm(kvm_ioctls::Error),
    /// 创建 guest 内存失败。
    #[error("创建 guest 内存失败: {0}")]
    GuestMemory(vm_memory::mmap::FromRangesError),
    /// 取 guest 内存宿主地址失败。
    #[error("取 guest 内存宿主地址失败: {0}")]
    HostAddress(vm_memory::GuestMemoryError),
    /// 注册 guest 内存到 KVM 失败。
    #[error("注册 guest 内存到 KVM 失败: {0}")]
    SetUserMemoryRegion(kvm_ioctls::Error),
    /// 创建 in-kernel irqchip 失败。
    #[error("创建 in-kernel irqchip 失败: {0}")]
    CreateIrqChip(kvm_ioctls::Error),
    /// 创建 in-kernel PIT 失败。
    #[error("创建 in-kernel PIT 失败: {0}")]
    CreatePit(kvm_ioctls::Error),
    /// 打开内核镜像失败。
    #[error("打开内核镜像失败: {0}")]
    OpenKernel(io::Error),
    /// 加载内核镜像失败。
    #[error("加载内核镜像失败: {0}")]
    KernelLoad(loader::Error),
    /// bzImage 缺少 setup header。
    #[error("bzImage 缺少 setup header")]
    MissingSetupHeader,
    /// 内核命令行不合法。
    #[error("内核命令行不合法: {0}")]
    Cmdline(linux_loader::cmdline::Error),
    /// 写内核命令行到 guest 内存失败。
    #[error("写内核命令行到 guest 内存失败: {0}")]
    LoadCmdline(loader::Error),
    /// 打开 initrd 失败。
    #[error("打开 initrd 失败: {0}")]
    OpenInitrd(io::Error),
    /// 写 initrd 到 guest 内存失败。
    #[error("写 initrd 到 guest 内存失败: {0}")]
    LoadInitrd(vm_memory::GuestMemoryError),
    /// 写 boot params（zero page）失败。
    #[error("写 boot params 失败: {0}")]
    BootParams(linux_loader::configurator::Error),
    /// guest 内存超出 M0 支持的范围。
    #[error("guest 内存超出 M0 支持的范围（{MIN_MEM_SIZE_MIB}MiB ~ 3GiB）: {0}MiB")]
    InvalidMemSize(usize),
    /// M0 仅支持单 vCPU。
    #[error("M0 仅支持单 vCPU（配置了 max_vcpu_count={0}）")]
    UnsupportedVcpuCount(u8),
    /// 平台相关初始化失败。
    #[error("平台相关初始化失败: {0}")]
    Arch(#[from] arch::Error),
    /// 创建 vCPU 失败。
    #[error("创建 vCPU 失败: {0}")]
    CreateVcpu(kvm_ioctls::Error),
    /// 获取 KVM 支持的 CPUID 失败。
    #[error("获取 KVM 支持的 CPUID 失败: {0}")]
    GetSupportedCpuid(kvm_ioctls::Error),
    /// 设置 vCPU CPUID 失败。
    #[error("设置 vCPU CPUID 失败: {0}")]
    SetCpuid(kvm_ioctls::Error),
    /// vCPU 运行失败。
    #[error("vCPU 运行失败: {0}")]
    VcpuRun(kvm_ioctls::Error),
    /// vCPU 入口执行失败（KVM_EXIT_FAIL_ENTRY）。
    #[error("vCPU 入口执行失败 (KVM_EXIT_FAIL_ENTRY, reason={0:#x})")]
    VcpuFailEntry(u64),
    /// vCPU 内部错误（KVM_EXIT_INTERNAL_ERROR）。
    #[error("vCPU 内部错误 (KVM_EXIT_INTERNAL_ERROR)")]
    VcpuInternalError,
    /// 串口输出失败。
    #[error("串口输出失败: {0}")]
    Serial(io::Error),
    /// 设置 IRQ 线电平失败。
    #[error("设置 IRQ 线电平失败: {0}")]
    SetIrqLine(kvm_ioctls::Error),
    /// 创建 blk 设备失败。
    #[error("创建 blk 设备失败: {0}")]
    Blk(io::Error),
    /// 设备注册失败。
    #[error("设备注册失败: {0}")]
    Device(#[from] crate::device::Error),
}

/// 一个运行中的 VM 实例。
///
/// 每个 VM 对应一个 terra-vmm 进程（见 AGENTS.md 第 1 节）；本结构体拥有
/// KVM VM fd、guest 内存与全部 vCPU fd。泛型 `W` 是串口输出去向
/// （生产为 `io::Stdout`，测试可注入缓冲）。
pub struct Vm<W: io::Write + Send + 'static = io::Stdout> {
    vm_fd: VmFd,
    #[allow(dead_code)]
    memory: GuestMemoryMmap,
    vcpus: Vec<VcpuFd>,
    serial: Arc<Mutex<Serial<W>>>,
    rtc: Arc<Mutex<Rtc>>,
    device_manager: Arc<Mutex<DeviceManager>>,
    serial_input: Arc<Mutex<VecDeque<u8>>>,
    resize_target: Option<Arc<AtomicU64>>,
    mem_config_changed: Option<Arc<AtomicBool>>,
    blk_capacity: Option<Arc<AtomicU64>>,
    blk_config_changed: Option<Arc<AtomicBool>>,
}

impl Vm<io::Stdout> {
    /// 按配置创建 VM，串口输出打到 host stdout。
    pub fn new(config: VmConfig) -> Result<Self, Error> {
        Vm::with_output(config, io::stdout())
    }
}

impl<W: io::Write + Send + 'static> Vm<W> {
    /// 按配置创建 VM 并完成启动前初始化（内核/initrd 加载、boot params、vCPU 寄存器）。
    ///
    /// `out` 是 guest 串口输出的去向。
    pub fn with_output(config: VmConfig, out: W) -> Result<Self, Error> {
        if config.max_vcpu_count == 0 {
            return Err(Error::UnsupportedVcpuCount(config.max_vcpu_count));
        }
        let mem_size = config.mem_size_mib << 20;
        if config.mem_size_mib < MIN_MEM_SIZE_MIB || mem_size as u64 >= MAX_MEM_SIZE {
            return Err(Error::InvalidMemSize(config.mem_size_mib));
        }

        let kvm = Kvm::new().map_err(Error::Kvm)?;
        let vm_fd = kvm.create_vm().map_err(Error::CreateVm)?;

        // guest 物理内存：单段匿名映射，[0, mem_size)。
        let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), mem_size)])
            .map_err(Error::GuestMemory)?;
        let host_addr = memory
            .get_host_address(GuestAddress(0))
            .map_err(Error::HostAddress)?;
        let region = kvm_userspace_memory_region {
            slot: 0,
            guest_phys_addr: 0,
            memory_size: mem_size as u64,
            userspace_addr: host_addr as u64,
            flags: 0,
        };
        // SAFETY: region 指向 memory 持有的匿名映射，地址与长度和映射一致；
        // memory 与 vm_fd 同存于 Vm，映射的生命周期覆盖该 KVM 内存槽的使用期。
        #[allow(unsafe_code)]
        unsafe { vm_fd.set_user_memory_region(region) }.map_err(Error::SetUserMemoryRegion)?;

        // in-kernel irqchip：必须在创建 vCPU 之前建好，否则 guest 收不到
        // 时钟中断，HLT 会睡死。
        vm_fd.create_irq_chip().map_err(Error::CreateIrqChip)?;
        // in-kernel PIT（i8253）：irqchip 不含 PIT；没有它，guest 对 0x40/0x43
        // 的访问全部退到用户态，早期 TSC 校准会死等计数器而走不下去。
        // KVM_PIT_SPEAKER_DUMMY 让 0x61 端口也在内核态处理：否则解压器 KASLR 的
        // i8254 熵源（GATE2 由 0x61 控制）会死等 channel 2 计数（Dragonball 同款用法）。
        vm_fd
            .create_pit2(kvm_pit_config {
                flags: kvm_bindings::KVM_PIT_SPEAKER_DUMMY,
                ..Default::default()
            })
            .map_err(Error::CreatePit)?;

        // 加载 bzImage；从 32-bit 入口 startup_32（= 内核加载地址）引导，
        // 由解压器自行切换到长模式（决策与理由见 ADR 0002）。
        let mut kernel_file = File::open(&config.kernel_path).map_err(Error::OpenKernel)?;
        let loader_result = BzImage::load(
            &memory,
            Some(GuestAddress(arch::HIMEM_START)),
            &mut kernel_file,
            None,
        )
        .map_err(Error::KernelLoad)?;
        let setup_header = loader_result
            .setup_header
            .ok_or(Error::MissingSetupHeader)?;
        let entry_addr = loader_result.kernel_load.raw_value();
        debug!(entry = entry_addr, "内核已加载");

        // MMIO 设备管理器：注册 virtio-blk（若配置了 disk_path），
        // 其余设备在后续里程碑注册。
        let mut device_manager = DeviceManager::new();
        let mut resize_target: Option<Arc<AtomicU64>> = None;
        let mut mem_config_changed: Option<Arc<AtomicBool>> = None;
        let mut blk_capacity: Option<Arc<AtomicU64>> = None;
        let mut blk_config_changed: Option<Arc<AtomicBool>> = None;

        if let Some(ref disk_path) = config.disk_path {
            let blk = Blk::new(disk_path).map_err(Error::Blk)?;
            blk_capacity = Some(blk.capacity_arc());
            blk_config_changed = Some(blk.config_changed_arc());
            let mmio = VirtioMmio::new(blk, memory.clone())?;
            device_manager.register(Box::new(mmio))?;
        }

        if let Some(hotplug_mib) = config.mem_hotplug_max {
            use crate::device::mem::MEM_HOTPLUG_BASE;
            let hotplug_bytes = (hotplug_mib as u64) << 20;
            let hotplug_mem = GuestMemoryMmap::from_ranges(&[(
                GuestAddress(MEM_HOTPLUG_BASE),
                hotplug_bytes as usize,
            )])
            .map_err(Error::GuestMemory)?;
            let mem = Mem::new(hotplug_mib, hotplug_mem);
            resize_target = Some(mem.requested_size_arc());
            mem_config_changed = Some(mem.config_changed_arc());
            let mmio = VirtioMmio::new(mem, memory.clone())?;
            device_manager.register(Box::new(mmio))?;
        }

        if let Some(ref net_path) = config.net_backend {
            use std::os::unix::io::AsRawFd;
            let f = std::fs::File::open(net_path).map_err(Error::Blk)?;
            let fd = f.as_raw_fd();
            let net = Net::new_tap(fd, fd).map_err(Error::Blk)?;
            let mmio = VirtioMmio::new(net, memory.clone())?;
            device_manager.register(Box::new(mmio))?;
        }

        // Balloon（无条件注册，无外部依赖）。
        let balloon = Balloon::new();
        let mmio = VirtioMmio::new(balloon, memory.clone())?;
        device_manager.register(Box::new(mmio))?;

        // RNG（无条件注册，从 /dev/urandom 读取熵）。
        if let Ok(rng) = Rng::new() {
            let mmio = VirtioMmio::new(rng, memory.clone())?;
            device_manager.register(Box::new(mmio))?;
        }

        // Watchdog（无条件注册）。
        {
            let wd = Watchdog::new();
            let mmio = VirtioMmio::new(wd, memory.clone())?;
            device_manager.register(Box::new(mmio))?;
        }

        // 内核命令行：先插入用户配置，再追加已注册 MMIO 设备的声明
        // （virtio_mmio.device=…；无设备时为空串，行为与 M0 一致）。
        let mut cmdline = Cmdline::new(arch::CMDLINE_MAX_SIZE).map_err(Error::Cmdline)?;
        cmdline
            .insert_str(&config.kernel_cmdline)
            .map_err(Error::Cmdline)?;
        let device_args = device_manager.cmdline_args();
        if !device_args.is_empty() {
            cmdline.insert_str(&device_args).map_err(Error::Cmdline)?;
        }
        let cmdline_size = cmdline
            .as_cstring()
            .map_err(Error::Cmdline)?
            .as_bytes_with_nul()
            .len() as u32;
        loader::load_cmdline(&memory, GuestAddress(arch::CMDLINE_START), &cmdline)
            .map_err(Error::LoadCmdline)?;

        // initrd（可选）：放在低端内存顶部、按页对齐。
        let initrd = match &config.initrd_path {
            Some(path) => {
                let mut file = File::open(path).map_err(Error::OpenInitrd)?;
                let size = file.metadata().map_err(Error::OpenInitrd)?.len();
                let addr = arch::initrd_load_addr(&memory, size)?;
                memory
                    .read_exact_volatile_from(GuestAddress(addr), &mut file, size as usize)
                    .map_err(Error::LoadInitrd)?;
                debug!(addr, size, "initrd 已加载");
                Some((addr as u32, size as u32))
            }
            None => None,
        };

        // boot params（zero page）。
        let params = arch::build_boot_params(
            setup_header,
            mem_size as u64,
            cmdline_size,
            initrd,
            config.max_vcpu_count,
        )?;
        LinuxBootConfigurator::write_bootparams(
            &BootParams::new(&params, GuestAddress(arch::ZERO_PAGE_START)),
            &memory,
        )
        .map_err(Error::BootParams)?;

        // MP table：多 vCPU 枚举（内核据此发现非 BSP 的 CPU）。
        if config.max_vcpu_count > 1 {
            arch::setup_mp_table(&memory, config.max_vcpu_count)?;
        }

        // guest CPUID：归一化后设入 vCPU（裁剪 KVM PV feature bits 确保跨宿主迁移兼容）。
        let mut cpuid = kvm
            .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
            .map_err(Error::GetSupportedCpuid)?;
        arch::normalize_cpuid(&mut cpuid);

        // vCPU 初始化（M0 单 vCPU；Vec 结构为多 vCPU 预留）。
        let mut vcpus = Vec::with_capacity(usize::from(config.max_vcpu_count));
        for id in 0..u64::from(config.max_vcpu_count) {
            let vcpu = vm_fd.create_vcpu(id).map_err(Error::CreateVcpu)?;
            vcpu.set_cpuid2(&cpuid).map_err(Error::SetCpuid)?;
            arch::setup_msrs(&vcpu)?;
            arch::setup_regs(&vcpu, entry_addr)?;
            arch::setup_fpu(&vcpu)?;
            arch::setup_sregs(&memory, &vcpu)?;
            // LAPIC LINT0=ExtINT / LINT1=NMI：否则 PIC 的定时器中断送不到 CPU。
            arch::set_lint(&vcpu)?;
            vcpus.push(vcpu);
        }

        Ok(Vm {
            vm_fd,
            memory,
            vcpus,
            serial: Arc::new(Mutex::new(Serial::new(out))),
            rtc: Arc::new(Mutex::new(Rtc::new())),
            device_manager: Arc::new(Mutex::new(device_manager)),
            serial_input: Arc::new(Mutex::new(VecDeque::new())),
            resize_target,
            mem_config_changed,
            blk_capacity,
            blk_config_changed,
        })
    }

    /// 获取串口输入缓冲（供 host 侧注入 stdin 数据）。
    pub fn serial_input(&self) -> Arc<Mutex<VecDeque<u8>>> {
        self.serial_input.clone()
    }

    /// 获取 virtio-mem resize 目标（供 API handler 写入）。
    pub fn resize_target(&self) -> Option<Arc<AtomicU64>> {
        self.resize_target.clone()
    }

    pub fn mem_config_changed(&self) -> Option<Arc<AtomicBool>> {
        self.mem_config_changed.clone()
    }

    pub fn blk_capacity(&self) -> Option<Arc<AtomicU64>> {
        self.blk_capacity.clone()
    }

    pub fn blk_config_changed(&self) -> Option<Arc<AtomicBool>> {
        self.blk_config_changed.clone()
    }

    /// 运行 vCPU 直到 guest 关机（KVM_EXIT_SHUTDOWN）。
    ///
    /// 处理的退出原因：PIO（分发给串口/RTC）、MMIO（分发给设备管理器）、
    /// HLT、SHUTDOWN；其余忽略或报错。
    pub fn run(self) -> Result<(), Error> {
        let Self {
            vcpus,
            serial,
            vm_fd,
            rtc,
            device_manager,
            serial_input,
            ..
        } = self;

        let mut vcpu_iter = vcpus.into_iter();
        let mut bsp = vcpu_iter
            .next()
            .ok_or_else(|| Error::VcpuRun(kvm_ioctls::Error::new(22)))?;

        // Spawn AP vCPU threads. Each AP runs a minimal KVM_RUN loop.
        // Device MMIO/PIO is forwarded to the shared device_manager.
        let _ap_handles: Vec<std::thread::JoinHandle<()>> = vcpu_iter
            .map(|vcpu| {
                let dm = device_manager.clone();
                let s = serial.clone();
                let r = rtc.clone();
                thread::spawn(move || {
                    let mut ap = vcpu;
                    loop {
                        match ap.run() {
                            Ok(VcpuExit::Hlt) => {}
                            Ok(VcpuExit::Shutdown) => break,
                            Ok(VcpuExit::IoOut(port, data)) => {
                                if (SERIAL_PORT_BASE..SERIAL_PORT_BASE + SERIAL_PORT_SIZE)
                                    .contains(&port)
                                {
                                    let _ = s.lock().unwrap().write(port - SERIAL_PORT_BASE, data);
                                } else if port == RTC_PORT_INDEX || port == RTC_PORT_DATA {
                                    r.lock().unwrap().write(port - RTC_PORT_INDEX, data);
                                }
                            }
                            Ok(VcpuExit::IoIn(port, data)) => {
                                for (i, byte) in data.iter_mut().enumerate() {
                                    let p = port + i as u16;
                                    *byte = if (SERIAL_PORT_BASE
                                        ..SERIAL_PORT_BASE + SERIAL_PORT_SIZE)
                                        .contains(&p)
                                    {
                                        s.lock().unwrap().read(p - SERIAL_PORT_BASE)
                                    } else if p == RTC_PORT_INDEX || p == RTC_PORT_DATA {
                                        r.lock().unwrap().read(p - RTC_PORT_INDEX)
                                    } else {
                                        0xff
                                    };
                                }
                            }
                            Ok(VcpuExit::MmioRead(addr, data)) => {
                                dm.lock().unwrap().read(addr, data);
                            }
                            Ok(VcpuExit::MmioWrite(addr, data)) => {
                                dm.lock().unwrap().write(addr, data);
                            }
                            Ok(_) => {}
                            Err(ref e)
                                if io::Error::from_raw_os_error(e.errno()).kind()
                                    == io::ErrorKind::Interrupted =>
                            {
                                continue
                            }
                            Err(e) => {
                                warn!(err=%e, "AP vCPU 出错，1s 后重试");
                                thread::sleep(std::time::Duration::from_secs(1));
                                // continue: KVM_RUN 重试
                            }
                        }
                    }
                })
            })
            .collect();

        let mut dev_mgr = device_manager.lock().unwrap();
        let mut serial = serial.lock().unwrap();
        let mut rtc = rtc.lock().unwrap();
        let mut last_irq = false;
        let mut dev_irqs: Vec<bool> = dev_mgr.irq_lines().map(|(_, l)| l).collect();

        loop {
            match bsp.run() {
                Ok(VcpuExit::IoOut(port, data)) => {
                    if (SERIAL_PORT_BASE..SERIAL_PORT_BASE + SERIAL_PORT_SIZE).contains(&port) {
                        serial
                            .write(port - SERIAL_PORT_BASE, data)
                            .map_err(Error::Serial)?;
                    } else if port == RTC_PORT_INDEX || port == RTC_PORT_DATA {
                        rtc.write(port - RTC_PORT_INDEX, data);
                    } else {
                        debug!(port, len = data.len(), "忽略 PIO 写");
                    }
                }
                Ok(VcpuExit::IoIn(port, data)) => {
                    for (i, byte) in data.iter_mut().enumerate() {
                        let p = port + i as u16;
                        *byte = if (SERIAL_PORT_BASE..SERIAL_PORT_BASE + SERIAL_PORT_SIZE)
                            .contains(&p)
                        {
                            serial.read(p - SERIAL_PORT_BASE)
                        } else if p == RTC_PORT_INDEX || p == RTC_PORT_DATA {
                            rtc.read(p - RTC_PORT_INDEX)
                        } else {
                            0xff
                        };
                    }
                }
                Ok(VcpuExit::MmioRead(addr, data)) => {
                    dev_mgr.read(addr, data);
                }
                Ok(VcpuExit::MmioWrite(addr, data)) => {
                    dev_mgr.write(addr, data);
                }
                Ok(VcpuExit::Hlt) => {}
                Ok(VcpuExit::Shutdown) => return Ok(()),
                Ok(VcpuExit::FailEntry(r, _)) => return Err(Error::VcpuFailEntry(r)),
                Ok(VcpuExit::InternalError) => return Err(Error::VcpuInternalError),
                Ok(other) => {
                    warn!(exit = ?other, "未处理退出");
                }
                Err(e) => {
                    if io::Error::from_raw_os_error(e.errno()).kind() == io::ErrorKind::Interrupted
                    {
                        continue;
                    }
                    return Err(Error::VcpuRun(e));
                }
            }

            // Refresh serial IRQ
            let level = serial.irq_level();
            if level != last_irq {
                vm_fd
                    .set_irq_line(crate::serial::SERIAL_IRQ, level)
                    .map_err(Error::SetIrqLine)?;
                last_irq = level;
            }
            // Refresh device IRQs
            for (idx, (irq, level)) in dev_mgr.irq_lines().enumerate() {
                if level != dev_irqs[idx] {
                    vm_fd.set_irq_line(irq, level).map_err(Error::SetIrqLine)?;
                    dev_irqs[idx] = level;
                }
            }
            // Drain serial input
            let mut input = serial_input.lock().unwrap();
            if !input.is_empty() {
                let data: Vec<u8> = input.drain(..).collect();
                serial.enqueue_input(&data);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vm_config_default() {
        let config = VmConfig::new("/path/to/bzImage");
        assert_eq!(128, config.mem_size_mib);
        assert_eq!(PathBuf::from("/path/to/bzImage"), config.kernel_path);
        assert_eq!(None, config.initrd_path);
        assert_eq!(1, config.max_vcpu_count);
        assert!(config.kernel_cmdline.contains("console=ttyS0"));
        assert!(config.kernel_cmdline.contains("reboot=k"));
        assert!(config.kernel_cmdline.contains("panic=-1"));
    }

    #[test]
    fn test_vm_config_rejects_zero_vcpu() {
        let config = VmConfig {
            max_vcpu_count: 0,
            ..VmConfig::new("/nonexistent")
        };
        assert!(matches!(
            Vm::new(config),
            Err(Error::UnsupportedVcpuCount(0))
        ));
    }

    #[test]
    fn test_vm_config_rejects_invalid_mem_size() {
        let small = VmConfig {
            mem_size_mib: 8,
            ..VmConfig::new("/nonexistent")
        };
        assert!(matches!(Vm::new(small), Err(Error::InvalidMemSize(8))));

        // 跨越 3GiB MMIO hole（3072MiB）。
        let large = VmConfig {
            mem_size_mib: 3072,
            ..VmConfig::new("/nonexistent")
        };
        assert!(matches!(Vm::new(large), Err(Error::InvalidMemSize(3072))));
    }

    // KVM 集成测试（需 /dev/kvm，不存在则跳过）：在 guest 里执行一小段
    // 32-bit 代码向 COM1 写两字节，验证 vCPU 初始化与串口 PIO 分发链路。
    // 代码放在 HIMEM_START（0x100000），顺带覆盖「段限须按 G 位缩放」这一
    // 历史 bug（未缩放时 ≥0x100000 取指 #GP 导致三重故障）。
    #[test]
    fn test_serial_pio_dispatch() {
        if !std::path::Path::new("/dev/kvm").exists() {
            return;
        }
        let kvm = Kvm::new().unwrap();
        let vm_fd = kvm.create_vm().unwrap();
        let mem: GuestMemoryMmap =
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 2 << 20)]).unwrap();
        let host_addr = mem.get_host_address(GuestAddress(0)).unwrap();
        let region = kvm_userspace_memory_region {
            slot: 0,
            guest_phys_addr: 0,
            memory_size: (2 << 20) as u64,
            userspace_addr: host_addr as u64,
            flags: 0,
        };
        // SAFETY: region 指向 mem 持有的匿名映射，地址与长度和映射一致；
        // mem 的生命周期覆盖本次 KVM 内存槽的使用期。
        #[allow(unsafe_code)]
        unsafe {
            vm_fd.set_user_memory_region(region).unwrap();
        }

        // mov dx,0x3f8; mov al,'H'; out dx,al; mov al,'i'; out dx,al; hlt
        let code: [u8; 12] = [
            0x66, 0xba, 0xf8, 0x03, 0xb0, b'H', 0xee, 0xb0, b'i', 0xee, 0xf4, 0x90,
        ];
        mem.write_slice(&code, GuestAddress(arch::HIMEM_START))
            .unwrap();

        let mut vcpu = vm_fd.create_vcpu(0).unwrap();
        arch::setup_msrs(&vcpu).unwrap();
        arch::setup_regs(&vcpu, arch::HIMEM_START).unwrap();
        arch::setup_fpu(&vcpu).unwrap();
        arch::setup_sregs(&mem, &vcpu).unwrap();

        let mut out = Vec::new();
        for _ in 0..8 {
            match vcpu.run().unwrap() {
                VcpuExit::IoOut(port, data) => {
                    assert_eq!(0x3f8, port);
                    out.extend_from_slice(data);
                }
                VcpuExit::Hlt => break,
                other => panic!("unexpected exit: {other:?}"),
            }
        }
        assert_eq!(b"Hi", out.as_slice());
    }
}
