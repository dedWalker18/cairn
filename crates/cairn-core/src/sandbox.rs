//! Process hardening. The point is that a bug in Cairn *cannot* modify user
//! data: the kernel refuses, not our code.
//!
//! * Landlock (unprivileged LSM): every write-type filesystem access is
//!   denied except beneath explicitly writable directories. TCP bind/connect
//!   are denied entirely (airgap guarantee for the daemon and workers).
//! * SCHED_IDLE + idle I/O class for background threads and workers.
//! * Resource limits and parent-death signal for workers.

use std::path::Path;

use anyhow::Result;
use landlock::{
    path_beneath_rules, Access, AccessFs, AccessNet, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    Full,
    Partial,
    Unsupported,
}

/// Deny all filesystem writes except beneath `writable`, and all TCP.
/// Applies to the calling process and everything it later spawns.
pub fn restrict_writes(writable: &[&Path]) -> Result<Enforcement> {
    let abi = ABI::V5;
    let status = Ruleset::default()
        .handle_access(AccessFs::from_write(abi))?
        .handle_access(AccessNet::from_all(abi))?
        .create()?
        .add_rules(path_beneath_rules(writable, AccessFs::from_write(abi)))?
        .restrict_self()?;
    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced => Enforcement::Full,
        RulesetStatus::PartiallyEnforced => Enforcement::Partial,
        RulesetStatus::NotEnforced => Enforcement::Unsupported,
    })
}

/// Lowest CPU and I/O priority for the calling *thread* (Linux applies both
/// per thread when given tid 0).
pub fn set_thread_idle() {
    unsafe {
        let param = libc::sched_param { sched_priority: 0 };
        libc::sched_setscheduler(0, libc::SCHED_IDLE, &param);
        // ioprio_set(IOPRIO_WHO_PROCESS, 0, IOPRIO_PRIO_VALUE(IOPRIO_CLASS_IDLE, 0))
        const IOPRIO_WHO_PROCESS: libc::c_int = 1;
        const IOPRIO_CLASS_IDLE: libc::c_int = 3;
        const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT);
    }
}

pub fn set_rlimit(resource: libc::__rlimit_resource_t, value: u64) {
    let lim = libc::rlimit { rlim_cur: value, rlim_max: value };
    unsafe {
        libc::setrlimit(resource, &lim);
    }
}

/// Worker hardening: die with the parent, cap memory/CPU/files, no core dumps.
pub fn harden_worker(max_mem_bytes: u64) {
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        // If the parent already died between fork and prctl, exit now.
        if libc::getppid() == 1 {
            libc::_exit(0);
        }
    }
    set_rlimit(libc::RLIMIT_AS, max_mem_bytes);
    set_rlimit(libc::RLIMIT_CORE, 0);
    set_rlimit(libc::RLIMIT_NOFILE, 256);
    set_rlimit(libc::RLIMIT_FSIZE, 0); // cannot grow any file, belt and braces
}

/// Pressure Stall Information: `avg10` of the "some" line, in percent.
pub fn psi_avg10(resource: &str) -> Option<f32> {
    let s = std::fs::read_to_string(format!("/proc/pressure/{resource}")).ok()?;
    let line = s.lines().find(|l| l.starts_with("some"))?;
    let v = line.split_whitespace().find_map(|kv| kv.strip_prefix("avg10="))?;
    v.parse().ok()
}

/// True when running on battery (any power supply reports Discharging).
pub fn on_battery() -> bool {
    let Ok(rd) = std::fs::read_dir("/sys/class/power_supply") else { return false };
    for e in rd.flatten() {
        let p = e.path();
        let is_battery = std::fs::read_to_string(p.join("type")).map(|t| t.trim() == "Battery").unwrap_or(false);
        if is_battery {
            if let Ok(s) = std::fs::read_to_string(p.join("status")) {
                if s.trim() == "Discharging" {
                    return true;
                }
            }
        }
    }
    false
}

/// MemAvailable in bytes.
pub fn mem_available() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = s.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Landlock ABI version supported by the running kernel (0 = unavailable).
pub fn landlock_abi() -> i64 {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    let r = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<libc::c_void>(), 0usize, LANDLOCK_CREATE_RULESET_VERSION) };
    r.max(0)
}
