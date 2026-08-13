//! terra-confine — Terrarium's native in-guest confinement (SandboxAdapter L2).
//!
//! Runs a command confined by:
//! - **Landlock** (static, kernel-enforced): default-deny filesystem with
//!   explicit read / read-write grants (system dirs + the session workdir).
//!   Zero per-syscall overhead — the filesystem policy costs ~1.3x vs bare.
//! - **seccomp user-notify** (supervisor): only network syscalls are
//!   intercepted, so outbound connections can be whitelisted and denials
//!   audited through the denyfd channel. Network is low-frequency, so the
//!   supervisor cost is negligible.
//! - **cgroup v2 memory.max**: per-sandbox memory cap enforced by the
//!   kernel (no per-allocation traps).
//!
//! This is the default backend; sandlock remains available as an
//! alternative via the guest-proxy backend selection.
//!
//! CLI (mirrors sandlock's shape so guest-proxy stays symmetric):
//!   terra-confine run [-r path] [-w path] [--net-allow host[:port]]
//!                     [-m <n>M] -w <workdir> -- <cmd...>

mod cgroup;
mod landlock;
mod netpolicy;
mod supervisor;

use std::process::ExitCode;

/// Exit code guest-proxy maps to a structured policy denial.
pub const SANDBOX_DENY_EXIT_CODE: i32 = 200;

#[derive(Debug, Default)]
struct Config {
    read_paths: Vec<String>,
    write_paths: Vec<String>,
    net_allow: Vec<String>,
    memory_mb: Option<u64>,
    cpu_shares: Option<u64>,
    procs: Option<u32>,
    max_open_files: Option<u64>,
    cmd: Vec<String>,
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config::default();
    let mut it = std::env::args().skip(1).peekable();
    let mut saw_run = false;
    while let Some(a) = it.next() {
        match a.as_str() {
            "run" => saw_run = true,
            "-r" => {
                let p = it.next().ok_or("missing path after -r")?;
                cfg.read_paths.push(p);
            }
            "-w" => {
                let p = it.next().ok_or("missing path after -w")?;
                cfg.write_paths.push(p);
            }
            "--net-allow" => {
                let s = it.next().ok_or("missing spec after --net-allow")?;
                cfg.net_allow.push(s);
            }
            "-m" => {
                let v = it.next().ok_or("missing value after -m")?;
                let v = v.strip_suffix('M').unwrap_or(&v);
                cfg.memory_mb = Some(v.parse::<u64>().map_err(|_| format!("bad -m value {v}"))?);
            }
            "--cpu-shares" => {
                let v = it.next().ok_or("missing value after --cpu-shares")?;
                cfg.cpu_shares = Some(
                    v.parse::<u64>()
                        .map_err(|_| format!("bad cpu_shares {v}"))?,
                );
            }
            "--max-procs" => {
                let v = it.next().ok_or("missing value after --max-procs")?;
                cfg.procs = Some(v.parse::<u32>().map_err(|_| format!("bad procs {v}"))?);
            }
            "--max-open-files" => {
                let v = it.next().ok_or("missing value after --max-open-files")?;
                cfg.max_open_files = Some(
                    v.parse::<u64>()
                        .map_err(|_| format!("bad max_open_files {v}"))?,
                );
            }
            "--" => {
                cfg.cmd = it.collect();
                break;
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    if !saw_run {
        return Err("missing 'run' subcommand".into());
    }
    if cfg.cmd.is_empty() {
        return Err("missing command after --".into());
    }
    Ok(cfg)
}

fn main() -> ExitCode {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("terra-confine: {e}");
            return ExitCode::from(2);
        }
    };
    match supervisor::run(&cfg) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("terra-confine: {e}");
            ExitCode::from(1)
        }
    }
}
