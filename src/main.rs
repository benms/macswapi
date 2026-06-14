//! macswapi — per-process memory & swap inspector for macOS.
//!
//! macOS exposes no real per-process swap counter (the compressor swaps shared
//! segments, not PIDs). We report EXACT compressed/footprint from Mach
//! `task_info(TASK_VM_INFO)` and ESTIMATE per-proc swap by splitting the real
//! `vm.swapusage` total proportional to each proc's compressed memory.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::time::{Duration, Instant};

// ---- type aliases ---------------------------------------------------------

type KernReturn = c_int; // kern_return_t
type MachPort = u32; // mach_port_t
type TaskFlavor = c_int; // task_flavor_t
type MsgTypeNumber = u32; // mach_msg_type_number_t (natural_t)

const KERN_SUCCESS: KernReturn = 0;
const TASK_VM_INFO: TaskFlavor = 22;
const PROC_ALL_PIDS: u32 = 1;
const PROC_PIDTBSDINFO: c_int = 3;
#[allow(dead_code)]
const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;

// ---- FFI (libSystem, linked automatically on macOS) -----------------------

extern "C" {
    static mach_task_self_: MachPort;
    fn task_for_pid(target: MachPort, pid: c_int, t: *mut MachPort) -> KernReturn;
    fn task_info(
        task: MachPort,
        flavor: TaskFlavor,
        out: *mut c_int,
        count: *mut MsgTypeNumber,
    ) -> KernReturn;
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
    fn proc_listpids(t: u32, ti: u32, buf: *mut c_void, sz: c_int) -> c_int;
    fn proc_pidpath(pid: c_int, buf: *mut c_void, sz: u32) -> c_int;
    fn proc_name(pid: c_int, buf: *mut c_void, sz: u32) -> c_int;
    fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buf: *mut c_void, sz: c_int) -> c_int;
    fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
}

// ---- struct layouts (ABI-critical) ----------------------------------------

/// `task_vm_info` truncated through `phys_footprint`. `task_info` copies
/// `min(count, actual)` `integer_t` (4-byte) units, so a prefix is enough.
#[repr(C)]
#[derive(Default)]
struct TaskVmInfo {
    virtual_size: u64,  // off 0
    region_count: i32,  // off 8
    page_size: i32,     // off 12
    resident_size: u64, // off 16
    resident_size_peak: u64,
    device: u64,
    device_peak: u64,
    internal: u64,
    internal_peak: u64,
    external: u64,
    external_peak: u64,
    reusable: u64,
    reusable_peak: u64,
    purgeable_volatile_pmap: u64,
    purgeable_volatile_resident: u64,
    purgeable_volatile_virtual: u64,
    compressed: u64, // off 120  <-- READ THIS
    compressed_peak: u64,
    compressed_lifetime: u64,
    phys_footprint: u64, // off 144  <-- READ THIS
}
// size_of == 152, /4 == 38 = the count passed to task_info.

/// `proc_bsdinfo` — full struct required (proc_pidinfo returns full size or fails).
#[repr(C)]
#[derive(Default)]
struct ProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32, // off 16  <-- READ THIS
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    pbi_rfu: u32,
    pbi_comm: [u8; 16],
    pbi_name: [u8; 32],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

/// `xsw_usage` — for total swap from `vm.swapusage`.
#[repr(C)]
#[derive(Default)]
struct XswUsage {
    xsu_total: u64,
    xsu_avail: u64,
    xsu_used: u64, // off 16  <-- READ THIS
    xsu_pagesize: u32,
    xsu_encrypted: i32,
}

// ---- core kernel functions ------------------------------------------------

fn swap_used_bytes() -> Result<u64, std::io::Error> {
    let mut xsw = XswUsage::default();
    let mut size = std::mem::size_of::<XswUsage>();
    let rc = unsafe {
        sysctlbyname(
            c"vm.swapusage".as_ptr(),
            &mut xsw as *mut _ as *mut c_void,
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        Ok(xsw.xsu_used)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn list_pids() -> Vec<c_int> {
    // First call with null buf → bytes needed.
    let needed = unsafe { proc_listpids(PROC_ALL_PIDS, 0, ptr::null_mut(), 0) };
    if needed <= 0 {
        return Vec::new();
    }
    // Slack for procs spawned between the two calls.
    let mut count = (needed as usize / std::mem::size_of::<c_int>()) + 32;
    let mut buf: Vec<c_int> = vec![0; count];
    let got = unsafe {
        proc_listpids(
            PROC_ALL_PIDS,
            0,
            buf.as_mut_ptr() as *mut c_void,
            (count * std::mem::size_of::<c_int>()) as c_int,
        )
    };
    if got <= 0 {
        return Vec::new();
    }
    count = got as usize / std::mem::size_of::<c_int>();
    buf.truncate(count);
    buf.into_iter().filter(|&p| p > 0).collect()
}

/// Returns (compressed, phys_footprint) or None if denied (needs root / SIP).
fn task_vm(pid: c_int) -> Option<(u64, u64)> {
    let mut task: MachPort = 0;
    let kr = unsafe { task_for_pid(mach_task_self_, pid, &mut task) };
    if kr != KERN_SUCCESS {
        return None;
    }
    let mut info = TaskVmInfo::default();
    let mut count = (std::mem::size_of::<TaskVmInfo>() / 4) as MsgTypeNumber;
    let kr = unsafe {
        task_info(
            task,
            TASK_VM_INFO,
            &mut info as *mut _ as *mut c_int,
            &mut count,
        )
    };
    unsafe { mach_port_deallocate(mach_task_self_, task) };
    if kr != KERN_SUCCESS {
        return None;
    }
    Some((info.compressed, info.phys_footprint))
}

fn bsdinfo(pid: c_int) -> Option<ProcBsdInfo> {
    let mut info = ProcBsdInfo::default();
    let sz = std::mem::size_of::<ProcBsdInfo>() as c_int;
    let rc = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            sz,
        )
    };
    if rc == sz {
        Some(info)
    } else {
        None
    }
}

fn ppid_of(pid: c_int) -> c_int {
    bsdinfo(pid).map(|i| i.pbi_ppid as c_int).unwrap_or(0)
}

fn cstr_buf(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn proc_display_name(pid: c_int) -> String {
    let mut buf = [0u8; PROC_PIDPATHINFO_MAXSIZE];
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if n > 0 && (n as usize) <= buf.len() {
        let path = cstr_buf(&buf[..n as usize]);
        if !path.is_empty() {
            return basename(&path).to_string();
        }
    }
    let mut nb = [0u8; 256];
    let n = unsafe { proc_name(pid, nb.as_mut_ptr() as *mut c_void, nb.len() as u32) };
    if n > 0 && (n as usize) <= nb.len() {
        let name = cstr_buf(&nb[..n as usize]);
        if !name.is_empty() {
            return name;
        }
    }
    format!("pid {pid}")
}

// ---- pure helpers (unit-tested) -------------------------------------------

fn basename(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => {
            let leaf = &path[i + 1..];
            if leaf.is_empty() {
                path
            } else {
                leaf
            }
        }
        None => path,
    }
}

fn swap_share(compressed: f64, total_compressed: f64, swap_total: f64) -> f64 {
    if total_compressed == 0.0 {
        0.0
    } else {
        swap_total * (compressed / total_compressed)
    }
}

fn swap_estimates_available(swap_total: Option<u64>, total_compressed: u64) -> bool {
    swap_total.is_some() && total_compressed > 0
}

fn human(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = bytes;
    let mut i = 0;
    while v.abs() >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{}{}", v as i64, UNITS[i])
    } else {
        format!("{:.2}{}", v, UNITS[i])
    }
}

fn human_signed(delta: f64) -> String {
    let sign = if delta < 0.0 { "-" } else { "+" };
    format!("{}{}", sign, human(delta.abs()))
}

// ---- row model ------------------------------------------------------------

struct Row {
    pid: c_int,
    name: String,
    compressed: u64,
    footprint: u64,
    ppid: c_int,
}

// ---- snapshot mode --------------------------------------------------------

fn run_snapshot(topn: usize, show_parent: bool) {
    let swap_total = match swap_used_bytes() {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            eprintln!("WARNING: could not read sysctl vm.swapusage: {err}; SWAP~ unavailable.");
            None
        }
    };
    let mut rows: Vec<Row> = Vec::new();
    let mut denied = 0usize;

    for pid in list_pids() {
        match task_vm(pid) {
            Some((compressed, footprint)) => rows.push(Row {
                pid,
                name: proc_display_name(pid),
                compressed,
                footprint,
                ppid: if show_parent { ppid_of(pid) } else { 0 },
            }),
            None => denied += 1,
        }
    }

    rows.sort_by_key(|r| (Reverse(r.compressed), r.pid));
    let total_compressed: u64 = rows.iter().map(|r| r.compressed).sum();

    let shown = if topn == 0 {
        rows.len()
    } else {
        topn.min(rows.len())
    };

    // parent-name cache (many children share one parent).
    let mut pname_cache: HashMap<c_int, String> = HashMap::new();

    if show_parent {
        println!(
            "{:<7} {:<7} {:<24} {:<20} {:>12} {:>12} {:>12}",
            "PID", "PPID", "NAME", "PARENT", "CMPRS", "FOOTPRINT", "SWAP~"
        );
    } else {
        println!(
            "{:<7} {:<24} {:>12} {:>12} {:>12}",
            "PID", "NAME", "CMPRS", "FOOTPRINT", "SWAP~"
        );
    }

    // SWAP~ estimates are proportional splits of system swap across readable
    // processes.  When denied > 0 the denominator is incomplete:
    //   - total_compressed == 0 → every estimate is 0 despite real swap
    //   - total_compressed > 0  → one visible process absorbs all system swap
    // Show N/A if the system swap total is unavailable or the denominator is 0;
    // warn whenever denied > 0.
    let swap_denom_zero = total_compressed == 0;
    let swap_est_unavailable = !swap_estimates_available(swap_total, total_compressed);

    let mut sum_est = 0.0f64;
    let mut sum_compressed = 0u64;
    let mut sum_footprint = 0u64;

    for r in rows.iter().take(shown) {
        let est = if swap_est_unavailable {
            0.0
        } else {
            let swap_total =
                swap_total.expect("swap_total is available when estimates are available");
            swap_share(
                r.compressed as f64,
                total_compressed as f64,
                swap_total as f64,
            )
        };
        if !swap_est_unavailable {
            sum_est += est;
        }
        sum_compressed += r.compressed;
        sum_footprint += r.footprint;

        let swap_col = if swap_est_unavailable {
            "N/A".to_string()
        } else {
            human(est)
        };

        if show_parent {
            let pname = pname_cache
                .entry(r.ppid)
                .or_insert_with(|| proc_display_name(r.ppid))
                .clone();
            println!(
                "{:<7} {:<7} {:<24} {:<20} {:>12} {:>12} {:>12}",
                r.pid,
                r.ppid,
                truncate(&r.name, 24),
                truncate(&pname, 20),
                human(r.compressed as f64),
                human(r.footprint as f64),
                swap_col,
            );
        } else {
            println!(
                "{:<7} {:<24} {:>12} {:>12} {:>12}",
                r.pid,
                truncate(&r.name, 24),
                human(r.compressed as f64),
                human(r.footprint as f64),
                swap_col,
            );
        }
    }

    println!();
    println!(
        "shown {}/{} procs  ({} denied — need sudo for other users)",
        shown,
        rows.len(),
        denied
    );

    let swap_shown_str = if swap_est_unavailable {
        "N/A".to_string()
    } else {
        human(sum_est)
    };
    println!(
        "shown totals: CMPRS {}  FOOTPRINT {}  SWAP~ {}",
        human(sum_compressed as f64),
        human(sum_footprint as f64),
        swap_shown_str,
    );

    let full_est: f64 = if swap_est_unavailable {
        0.0
    } else {
        let swap_total = swap_total.expect("swap_total is available when estimates are available");
        rows.iter()
            .map(|r| {
                swap_share(
                    r.compressed as f64,
                    total_compressed as f64,
                    swap_total as f64,
                )
            })
            .sum()
    };
    let swap_total_str = swap_total
        .map(|bytes| human(bytes as f64))
        .unwrap_or_else(|| "N/A".to_string());
    let full_est_str = if swap_total.is_none() {
        "N/A (vm.swapusage unavailable)".to_string()
    } else if swap_denom_zero {
        "N/A (denominator is 0 — run as root for accurate estimates)".to_string()
    } else {
        human(full_est)
    };
    println!(
        "swap (sysctl vm.swapusage used): {}   Σ est over readable procs: {}",
        swap_total_str, full_est_str,
    );
    if denied > 0 && !swap_est_unavailable {
        eprintln!(
            "WARNING: {} proc(s) denied — SWAP~ estimates cover only visible processes; \
             a single visible process may absorb all system swap. Run as root for accuracy.",
            denied
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        String::new()
    } else if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

// ---- watch / leak mode ----------------------------------------------------

type StartTime = (u64, u64);

#[derive(Clone, Copy)]
struct Sample {
    footprint: u64,
    start: Option<StartTime>,
    ppid: c_int,
}

struct SampleSet {
    rows: HashMap<c_int, Sample>,
    denied: usize,
}

impl SampleSet {
    fn total(&self) -> usize {
        self.rows.len() + self.denied
    }
}

fn same_process_start(a: Option<StartTime>, b: Option<StartTime>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a == b)
}

fn sample() -> SampleSet {
    let mut rows = HashMap::new();
    let mut denied = 0usize;
    for pid in list_pids() {
        if let Some((_compressed, footprint)) = task_vm(pid) {
            let info = bsdinfo(pid);
            let start = info
                .as_ref()
                .map(|i| (i.pbi_start_tvsec, i.pbi_start_tvusec));
            let ppid = info.as_ref().map(|i| i.pbi_ppid as c_int).unwrap_or(0);
            rows.insert(
                pid,
                Sample {
                    footprint,
                    start,
                    ppid,
                },
            );
        } else {
            denied += 1;
        }
    }
    SampleSet { rows, denied }
}

fn run_watch(interval: u64, topn: usize, show_parent: bool) {
    let a = sample();
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs(interval.max(1)));
    let b = sample();
    let elapsed = t0.elapsed().as_secs_f64().max(1e-9);

    struct Delta {
        pid: c_int,
        ppid: c_int,
        name: String,
        foot0: u64,
        foot1: u64,
        delta: i128,
    }
    let mut deltas: Vec<Delta> = Vec::new();
    for (pid, b_sample) in &b.rows {
        if let Some(a_sample) = a.rows.get(pid) {
            if !same_process_start(a_sample.start, b_sample.start) {
                continue; // PID reused or start time unavailable between samples
            }
            deltas.push(Delta {
                pid: *pid,
                ppid: if show_parent { b_sample.ppid } else { 0 },
                name: proc_display_name(*pid),
                foot0: a_sample.footprint,
                foot1: b_sample.footprint,
                delta: b_sample.footprint as i128 - a_sample.footprint as i128,
            });
        }
    }
    deltas.sort_by_key(|d| (Reverse(d.delta), d.pid));

    let shown = if topn == 0 {
        deltas.len()
    } else {
        topn.min(deltas.len())
    };

    let mut pname_cache: HashMap<c_int, String> = HashMap::new();

    if show_parent {
        println!(
            "{:<7} {:<7} {:<24} {:<20} {:>12} {:>12} {:>12} {:>12}",
            "PID", "PPID", "NAME", "PARENT", "FOOT0", "FOOT1", "DELTA", "RATE/s"
        );
    } else {
        println!(
            "{:<7} {:<24} {:>12} {:>12} {:>12} {:>12}",
            "PID", "NAME", "FOOT0", "FOOT1", "DELTA", "RATE/s"
        );
    }
    for d in deltas.iter().take(shown) {
        let rate = d.delta as f64 / elapsed;
        if show_parent {
            let pname = pname_cache
                .entry(d.ppid)
                .or_insert_with(|| proc_display_name(d.ppid))
                .clone();
            println!(
                "{:<7} {:<7} {:<24} {:<20} {:>12} {:>12} {:>12} {:>12}",
                d.pid,
                d.ppid,
                truncate(&d.name, 24),
                truncate(&pname, 20),
                human(d.foot0 as f64),
                human(d.foot1 as f64),
                human_signed(d.delta as f64),
                human_signed(rate),
            );
        } else {
            println!(
                "{:<7} {:<24} {:>12} {:>12} {:>12} {:>12}",
                d.pid,
                truncate(&d.name, 24),
                human(d.foot0 as f64),
                human(d.foot1 as f64),
                human_signed(d.delta as f64),
                human_signed(rate),
            );
        }
    }
    println!();
    println!(
        "interval {:.1}s — one interval = allocation churn; trust SUSTAINED growth as leak signal.",
        elapsed
    );
    println!(
        "sampled procs: first {}/{} readable ({} denied), second {}/{} readable ({} denied)",
        a.rows.len(),
        a.total(),
        a.denied,
        b.rows.len(),
        b.total(),
        b.denied,
    );
}

// ---- CLI ------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct Config {
    topn: usize,
    show_parent: bool,
    watch_secs: Option<u64>,
    help: bool,
}

fn usage() {
    println!("macswapi [N] [-p|--parent] [-w|--watch|--delta [SECS]]");
    println!("  N            top N rows (default 20, 0=all)");
    println!("  -p --parent  add PPID + parent-name columns (works in watch mode too)");
    println!("  -w --watch   single-interval leak check; SECS must immediately follow the flag");
    println!("     --delta   clearer alias for --watch (default interval 3s)");
    println!("               SECS must be positive; N must appear before the interval flag");
    println!("               example: macswapi 50 --delta 5");
    println!("  -h --help    this help");
}

fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut config = Config {
        topn: 20,
        show_parent: false,
        watch_secs: None,
        help: false,
    };
    let mut saw_interval_flag = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-h" | "--help" => {
                config.help = true;
                return Ok(config);
            }
            "-p" | "--parent" => config.show_parent = true,
            "-w" | "--watch" | "--delta" => {
                if saw_interval_flag {
                    return Err("duplicate interval flag".to_string());
                }
                saw_interval_flag = true;
                config.watch_secs = Some(3);
                if i + 1 < args.len() {
                    if let Ok(secs) = args[i + 1].parse::<u64>() {
                        if secs == 0 {
                            return Err("watch interval must be positive".to_string());
                        }
                        config.watch_secs = Some(secs);
                        i += 1;
                    }
                }
            }
            other => {
                if let Ok(topn) = other.parse::<usize>() {
                    if saw_interval_flag {
                        return Err("N must appear before the interval flag".to_string());
                    }
                    config.topn = topn;
                } else {
                    return Err(format!("unknown arg: {other}"));
                }
            }
        }
        i += 1;
    }

    Ok(config)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match parse_args(&args) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            usage();
            std::process::exit(2);
        }
    };

    if config.help {
        usage();
        return;
    }

    if let Some(watch_secs) = config.watch_secs {
        run_watch(watch_secs, config.topn, config.show_parent);
    } else {
        run_snapshot(config.topn, config.show_parent);
    }
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn field_offset<T, F>(base: &T, field: &F) -> usize {
        (field as *const F as usize) - (base as *const T as usize)
    }

    #[test]
    fn test_human() {
        assert_eq!(human(0.0), "0B");
        assert_eq!(human(1024.0), "1.00K");
        assert_eq!(human(1048576.0), "1.00M");
        assert_eq!(human(1073741824.0), "1.00G");
        assert_eq!(human(8_978_432.0), "8.56M");
    }

    #[test]
    fn test_human_signed() {
        assert_eq!(human_signed(0.0), "+0B");
        assert_eq!(human_signed(1024.0), "+1.00K");
        assert_eq!(human_signed(-1048576.0), "-1.00M");
    }

    #[test]
    fn test_swap_share() {
        assert_eq!(swap_share(50.0, 0.0, 1000.0), 0.0); // zero total
        assert_eq!(swap_share(50.0, 100.0, 1000.0), 500.0); // proportional
        assert_eq!(swap_share(50.0, 100.0, 0.0), 0.0); // zero swap
    }

    #[test]
    fn test_swap_estimates_available_requires_total_and_denominator() {
        assert!(!swap_estimates_available(None, 100));
        assert!(!swap_estimates_available(Some(1000), 0));
        assert!(swap_estimates_available(Some(1000), 1));
    }

    #[test]
    fn test_conservation() {
        let compressed = [10u64, 25, 3, 100, 7, 512, 64, 1];
        let total: u64 = compressed.iter().sum();
        let swap = 4_294_967_296.0f64; // 4G
        let sum: f64 = compressed
            .iter()
            .map(|&c| swap_share(c as f64, total as f64, swap))
            .sum();
        assert!((sum - swap).abs() < 1e-3, "sum={sum} swap={swap}");
    }

    #[test]
    fn test_basename() {
        assert_eq!(basename("/usr/bin/node"), "node");
        assert_eq!(basename("node"), "node");
        assert_eq!(basename("/usr/bin/"), "/usr/bin/"); // trailing slash → whole
        assert_eq!(basename(""), "");
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("abcdef", 0), "");
        assert_eq!(truncate("abcdef", 1), "…");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
    }

    #[test]
    fn test_same_process_start_fails_closed() {
        assert!(same_process_start(Some((10, 20)), Some((10, 20))));
        assert!(!same_process_start(Some((10, 20)), Some((10, 21))));
        assert!(!same_process_start(None, Some((10, 20))));
        assert!(!same_process_start(Some((10, 20)), None));
        assert!(!same_process_start(None, None));
    }

    fn parse(words: &[&str]) -> Result<Config, String> {
        let args: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        parse_args(&args)
    }

    #[test]
    fn test_parse_args_accepts_strict_interval_order() {
        assert_eq!(
            parse(&["20", "--delta", "5"]).unwrap(),
            Config {
                topn: 20,
                show_parent: false,
                watch_secs: Some(5),
                help: false,
            }
        );
    }

    #[test]
    fn test_parse_args_rejects_n_after_interval_flag() {
        assert!(parse(&["--delta", "5", "20"]).is_err());
    }

    #[test]
    fn test_parse_args_rejects_zero_interval() {
        assert!(parse(&["--watch", "0"]).is_err());
    }

    #[test]
    fn test_task_vm_info_offsets() {
        let t = TaskVmInfo::default();
        assert_eq!(field_offset(&t, &t.compressed), 120);
        assert_eq!(field_offset(&t, &t.phys_footprint), 144);
        assert_eq!(std::mem::size_of::<TaskVmInfo>(), 152);
        assert_eq!(std::mem::size_of::<TaskVmInfo>() / 4, 38);
    }

    #[test]
    fn test_proc_bsdinfo_offset() {
        let p = ProcBsdInfo::default();
        assert_eq!(field_offset(&p, &p.pbi_ppid), 16);
    }

    #[test]
    fn test_xsw_usage_offset() {
        let x = XswUsage::default();
        assert_eq!(field_offset(&x, &x.xsu_used), 16);
    }
}
