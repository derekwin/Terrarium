//! observe：guest 内观测守护进程（M2 Task 3）。
//!
//! 按沙箱粒度（cgroup）采集进程指标，经 vsock / Unix socket 上报 host。
//! M2 初始版用 /proc 文件系统采集；后续替换为 aya eBPF。
//!
//! 监听 `/run/observe.sock`，接收查询命令（`metrics`），
//! 返回按 cgroup 聚合的 syscall 计数、文件打开数、I/O 用量。

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};

use serde::Serialize;

/// 单个进程的指标快照。
#[derive(Debug, Clone, Serialize)]
struct ProcMetrics {
    pid: u32,
    comm: String,
    /// 用户态 CPU 时间（jiffies）。
    utime: u64,
    /// 内核态 CPU 时间（jiffies）。
    stime: u64,
    /// 打开的文件描述符数。
    open_fds: u32,
    /// 读字节数（/proc/pid/io rchar）。
    rchar: u64,
    /// 写字节数（/proc/pid/io wchar）。
    wchar: u64,
    /// cgroup 路径。
    cgroup: String,
}

/// 聚合后的沙箱指标。
#[derive(Debug, Serialize)]
struct SandboxMetrics {
    /// cgroup 路径 → 聚合指标。
    cgroups: HashMap<String, AggregatedMetrics>,
    /// 快照时间戳（Unix 秒）。
    timestamp: u64,
}

#[derive(Debug, Default, Serialize)]
struct AggregatedMetrics {
    process_count: u32,
    total_utime: u64,
    total_stime: u64,
    total_open_fds: u32,
    total_rchar: u64,
    total_wchar: u64,
}

/// 扫描 /proc，收集所有进程的指标。
fn collect_metrics() -> Vec<ProcMetrics> {
    let mut metrics = Vec::new();

    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Ok(pid) = name_str.parse::<u32>() {
                if let Some(m) = read_proc_pid(pid) {
                    metrics.push(m);
                }
            }
        }
    }

    metrics
}

fn read_proc_pid(pid: u32) -> Option<ProcMetrics> {
    // /proc/pid/stat
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let comm_start = stat.find('(')? + 1;
    let comm_end = stat.rfind(')')?;
    let comm = stat[comm_start..comm_end].to_string();

    // stat 格式：pid (comm) state ppid ... utime stime ...
    let after_comm = &stat[comm_end + 2..]; // skip ") "
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // utime = fields[11], stime = fields[12]
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;

    // /proc/pid/fd — count entries.
    let open_fds = fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|d| d.count() as u32)
        .unwrap_or(0);

    // /proc/pid/io
    let (rchar, wchar) = read_proc_io(pid).unwrap_or((0, 0));

    // /proc/pid/cgroup — read cgroup path.
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .unwrap_or_default()
        .lines()
        .next()
        .map(|l| l.to_string())
        .unwrap_or_default();

    Some(ProcMetrics {
        pid,
        comm,
        utime,
        stime,
        open_fds,
        rchar,
        wchar,
        cgroup,
    })
}

fn read_proc_io(pid: u32) -> Option<(u64, u64)> {
    let content = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let mut rchar = 0u64;
    let mut wchar = 0u64;
    for line in content.lines() {
        if let Some(val) = line.strip_prefix("rchar:") {
            rchar = val.trim().parse().unwrap_or(0);
        } else if let Some(val) = line.strip_prefix("wchar:") {
            wchar = val.trim().parse().unwrap_or(0);
        }
    }
    Some((rchar, wchar))
}

/// 按 cgroup 聚合指标。
fn aggregate(metrics: &[ProcMetrics]) -> SandboxMetrics {
    let mut cgroups: HashMap<String, AggregatedMetrics> = HashMap::new();

    for m in metrics {
        let entry = cgroups.entry(m.cgroup.clone()).or_default();
        entry.process_count += 1;
        entry.total_utime += m.utime;
        entry.total_stime += m.stime;
        entry.total_open_fds += m.open_fds;
        entry.total_rchar += m.rchar;
        entry.total_wchar += m.wchar;
    }

    SandboxMetrics {
        cgroups,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    }
}

fn main() {
    let socket_path = "/run/observe.sock";
    let _ = std::fs::create_dir_all("/run");
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path).expect("bind observe socket");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                handle_client(stream);
            }
            Err(e) => {
                eprintln!("observe: accept error: {e}");
                break;
            }
        }
    }
}

fn handle_client(mut stream: UnixStream) {
    let reader = BufReader::new(stream.try_clone().unwrap());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let resp = if line.trim() == "metrics" {
            let metrics = collect_metrics();
            let aggregated = aggregate(&metrics);
            serde_json::to_string(&aggregated).unwrap_or_else(|e| format!("error: {e}"))
        } else {
            "unknown command".to_string()
        };

        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.write_all(b"\n");
    }
}
