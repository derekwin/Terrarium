//! CH process management — spawning, arg building, and startup helpers.

use crate::api;
use crate::client::ChClient;
use adapter_traits::{AdapterError, VmSpec};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::{sleep, Instant};

/// Next free vsock CID (0/1/2 reserved: hypervisor/host/local).
pub(crate) static NEXT_VSOCK_CID: AtomicU64 = AtomicU64::new(3);

/// Build the Cloud Hypervisor command-line arguments for a VM.
pub(crate) fn ch_args(
    spec: &VmSpec,
    socket: &str,
    fs_socket: Option<&str>,
    vsock: &str,
    tap: Option<&str>,
    snapshot_dir: &str,
) -> Vec<String> {
    let mut args = vec!["--api-socket".into(), socket.into()];
    if let Some(kernel) = &spec.kernel {
        args.push("--kernel".into());
        args.push(kernel.clone());
    }
    if let Some(ref c) = spec.cmdline {
        args.push("--cmdline".into());
        args.push(c.clone());
    }
    if let Some(ref i) = spec.initramfs {
        args.push("--initramfs".into());
        args.push(i.clone());
    }
    args.push("--cpus".into());
    args.push(format!(
        "boot={},max={}",
        spec.boot_vcpus,
        spec.max_vcpus.unwrap_or(spec.boot_vcpus)
    ));
    // vhost-user devices (virtiofs) require shared guest memory. Always
    // on: any VM may receive a hot-plugged fs later (warm pool), and
    // shared memory is also the DAX/zero-copy path — no downside.
    let shared = ",shared=on";
    if let Some(max_mem) = spec.max_memory_mb {
        args.push("--memory".into());
        args.push(format!(
            "size={}M,hotplug_method=virtio-mem,hotplug_size={}G{}",
            spec.memory_mb,
            max_mem / 1024,
            shared
        ));
    } else {
        args.push("--memory".into());
        args.push(format!("size={}M{}", spec.memory_mb, shared));
    }
    if let Some(fs_sock) = fs_socket {
        args.push("--fs".into());
        args.push(format!("tag=rootfs,socket={},num_queues=1", fs_sock));
    }
    // Free-page reporting: the guest proactively reports freed pages so
    // the host reclaims them passively (~97% RSS reclaim measured, R-M1).
    // Always on — zero-size balloon, no guest-visible semantics change.
    args.push("--balloon".into());
    args.push("size=0,free_page_reporting=on".into());
    if let Some(tap) = tap {
        args.push("--net".into());
        args.push(format!("tap={}", tap));
    }
    // Landlock whitelists only cmdline paths; CH opens /dev/net/tun to
    // attach tap devices, so it must be granted explicitly when
    // networking is enabled (otherwise CH dies right after boot).
    if tap.is_some() {
        // CH opens /dev/net/tun to create/attach taps and reads the tap
        // flags from sysfs (/sys/class/net is a symlink farm into
        // /sys/devices/virtual/net — grant both, read-only).
        args.push("--landlock-rules".into());
        args.push("path=/dev/net/tun,access=rw".into());
        args.push("--landlock-rules".into());
        args.push("path=/sys/class/net,access=r".into());
        args.push("--landlock-rules".into());
        args.push("path=/sys/devices/virtual/net,access=r".into());
    }
    // vsock for host→guest control (guest-proxy); unique CID per VM.
    let cid = NEXT_VSOCK_CID.fetch_add(1, Ordering::Relaxed);
    args.push("--vsock".into());
    args.push(format!("cid={},socket={}", cid, vsock));
    args.push("--serial".into());
    args.push("null".into());
    args.push("--console".into());
    args.push("off".into());
    // Landlock confines the CH process to the paths explicitly given on
    // the command line (kernel/initramfs/api socket/fs socket) — anything
    // the VMM might be tricked into opening outside that set is denied.
    // Snapshot destinations live under the managed snapshot_dir; without
    // a rule there CH cannot write the memory/state files (EPERM).
    args.push("--landlock-rules".into());
    args.push(format!("path={},access=rw", snapshot_dir));
    args.push("--landlock".into());
    args
}

/// Restore-only CH invocation (P1 fast reset).
///
/// The restored VM's full config (cpus, memory, fs, vsock, serial, ...)
/// comes from the snapshot's `config.json`. Passing ANY vm-config CLI
/// option would force CH's clap to require a kernel payload and boot a
/// fresh VM instead of restoring (CH v53: the vm-config arg group
/// `.requires("vm-payload")`, and `VmBoot` wins the if/else-if over
/// `VmRestore` whenever a payload is present). So the command line is
/// ONLY the api socket + `--restore`.
pub(crate) fn ch_restore_args(socket: &str, source_url: &str) -> Vec<String> {
    vec![
        "--api-socket".into(),
        socket.into(),
        "--restore".into(),
        format!("source_url={},resume=true", source_url),
    ]
}

/// Retry vm_info() up to 10 times with 500ms back-off.
/// CH API may return transient errors during startup.
pub(crate) async fn retry_get_info(client: &ChClient) -> Result<api::VmDetails, AdapterError> {
    for attempt in 0..10 {
        match client.vm_info().await {
            Ok(details) => return Ok(details),
            Err(e) => {
                if attempt == 9 {
                    return Err(AdapterError::internal(format!(
                        "vm.info failed after 10 attempts: {}",
                        e
                    )));
                }
                sleep(Duration::from_millis(500)).await;
            }
        }
    }
    unreachable!()
}

/// Spawn a Cloud Hypervisor process, redirecting stderr to a per-VM log file.
pub(crate) fn spawn_ch(
    args: &[String],
    ch_binary: &str,
    log_dir: &str,
    name: &str,
) -> Result<Child, AdapterError> {
    let _ = std::fs::create_dir_all(log_dir);
    let log_path = format!("{}/{}.log", log_dir, name);
    let log_file = std::fs::File::create(&log_path)
        .map_err(|e| AdapterError::internal(format!("create CH log {}: {}", log_path, e)))?;

    Command::new(ch_binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .spawn()
        .map_err(|e| AdapterError::internal(format!("spawn CH: {}", e)))
}

/// Wait for a Unix socket file to appear, polling every 5ms.
pub(crate) async fn wait_for_socket(
    socket_path: &str,
    timeout: Duration,
) -> Result<(), AdapterError> {
    let deadline = Instant::now() + timeout;
    loop {
        if std::path::Path::new(socket_path).exists() {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(AdapterError::internal(format!(
                "socket timeout for {}",
                socket_path
            )));
        }
        sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adapter_traits::VmName;

    fn spec(max_memory_mb: Option<u64>) -> VmSpec {
        VmSpec {
            name: VmName::new("test".to_string()).unwrap(),
            kernel: Some("/k".into()),
            cmdline: None,
            boot_vcpus: 1,
            max_vcpus: Some(4),
            memory_mb: 512,
            max_memory_mb,
            initramfs: Some("/i".into()),
            net: false,
            fs: None,
        }
    }

    fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == flag)
            .map(|i| args[i + 1].as_str())
    }

    /// Every spawn carries the FPR balloon (R-M2): zero-size balloon with
    /// free page reporting, regardless of memory hotplug configuration.
    #[test]
    fn balloon_fpr_always_present() {
        for max in [None, Some(4096)] {
            let args = ch_args(&spec(max), "/s", None, "/v", None, "/tmp/snaps");
            assert_eq!(
                arg_after(&args, "--balloon"),
                Some("size=0,free_page_reporting=on"),
                "args: {:?}",
                args
            );
        }
    }

    /// The memory arg keeps its existing shape with and without hotplug.
    #[test]
    fn memory_arg_shape() {
        let args = ch_args(&spec(Some(4096)), "/s", None, "/v", None, "/tmp/snaps");
        assert_eq!(
            arg_after(&args, "--memory"),
            Some("size=512M,hotplug_method=virtio-mem,hotplug_size=4G,shared=on")
        );
        let args = ch_args(&spec(None), "/s", None, "/v", None, "/tmp/snaps");
        assert_eq!(arg_after(&args, "--memory"), Some("size=512M,shared=on"));
    }

    /// Restore mode (P1 fast reset) is a restore-ONLY invocation: passing
    /// any vm-config option would make CH boot fresh instead (CH v53 clap
    /// quirk). The snapshot's config.json supplies everything else.
    #[test]
    fn restore_args_are_restore_only() {
        let args = ch_restore_args("/s", "file:///tmp/terra-snap-env");
        assert_eq!(arg_after(&args, "--api-socket"), Some("/s"));
        assert_eq!(
            arg_after(&args, "--restore"),
            Some("source_url=file:///tmp/terra-snap-env,resume=true")
        );
        for forbidden in [
            "--kernel", "--cpus", "--memory", "--fs", "--vsock", "--serial",
        ] {
            assert_eq!(
                arg_after(&args, forbidden),
                None,
                "restore args must not carry {}",
                forbidden
            );
        }
    }
}
