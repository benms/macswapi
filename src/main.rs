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
// `integer_t` counts per `task_vm_info` revision (<mach/task_info.h>).
const TASK_VM_INFO_REV3_COUNT: MsgTypeNumber = 84;
const TASK_VM_INFO_REV4_COUNT: MsgTypeNumber = 86;
const TASK_VM_INFO_REV5_COUNT: MsgTypeNumber = 87;
const TASK_VM_INFO_REV6_COUNT: MsgTypeNumber = 89;
const PROC_ALL_PIDS: u32 = 1;
const PROC_PIDTBSDINFO: c_int = 3;
const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;

// ---- FFI (libSystem, linked automatically on macOS) -----------------------

extern "C" {
    static mach_task_self_: MachPort;
    fn task_name_for_pid(target: MachPort, pid: c_int, t: *mut MachPort) -> KernReturn;
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
    fn signal(sig: c_int, handler: usize) -> usize;
}

const SIGPIPE: c_int = 13;
const SIG_DFL: usize = 0;

// ---- struct layouts (ABI-critical) ----------------------------------------

/// `task_vm_info` truncated through rev6 (`ledger_swapins`). `task_info` copies
/// `min(count, actual)` `integer_t` (4-byte) units and returns how many it
/// wrote, so a prefix is enough and older kernels hand back an earlier rev.
/// The header is `#pragma pack(4)`: `ledger_swapins` is only 4-aligned.
#[repr(C, packed(4))]
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
    compressed: u64,      // off 120  <-- READ THIS
    compressed_peak: u64, // off 128  <-- READ THIS
    compressed_lifetime: u64,
    // rev1
    phys_footprint: u64, // off 144  <-- READ THIS
    // rev2
    min_address: u64,
    max_address: u64,
    // rev3
    ledger_phys_footprint_peak: i64, // off 168  <-- READ THIS
    // ledger_purgeable_* (4) + ledger_tag_{network,media,graphics,neural}_* (16)
    purgeable_and_tag_ledgers: [i64; 20],
    // rev4
    limit_bytes_remaining: u64, // off 336  <-- READ THIS
    // rev5
    decompressions: i32, // off 344  <-- READ THIS
    // rev6
    ledger_swapins: i64, // off 348  <-- READ THIS
}
// size_of == 356, /4 == 89 = TASK_VM_INFO_REV6_COUNT, the count passed to task_info.

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

/// Per-process stats from `task_info(TASK_VM_INFO)`. Fields from later struct
/// revisions are None when the kernel returned an earlier revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VmStats {
    compressed: u64,
    compressed_peak: u64,
    footprint: u64,
    /// lifetime peak of `footprint` (rev3)
    footprint_peak: Option<u64>,
    /// bytes left under the jetsam limit; 0 = no limit, or at/over a soft one (rev4)
    limit_remaining: Option<u64>,
    /// pages decompressed over the process lifetime (rev5)
    decompressions: Option<u32>,
    /// bytes swapped in over the process lifetime (rev6)
    swapins: Option<u64>,
}

impl VmStats {
    /// `count` is what `task_info` returned: the `integer_t` units it wrote.
    fn from_info(info: &TaskVmInfo, count: MsgTypeNumber) -> VmStats {
        VmStats {
            compressed: info.compressed,
            compressed_peak: info.compressed_peak,
            footprint: info.phys_footprint,
            footprint_peak: (count >= TASK_VM_INFO_REV3_COUNT)
                .then_some(info.ledger_phys_footprint_peak.max(0) as u64),
            limit_remaining: (count >= TASK_VM_INFO_REV4_COUNT)
                .then_some(info.limit_bytes_remaining),
            decompressions: (count >= TASK_VM_INFO_REV5_COUNT)
                .then_some(info.decompressions as u32),
            swapins: (count >= TASK_VM_INFO_REV6_COUNT)
                .then_some(info.ledger_swapins.max(0) as u64),
        }
    }
}

/// Returns None if denied (another user's process without root). A task
/// *name* port is enough for `task_info`; `task_for_pid` would need root and
/// is still refused for SIP-protected procs.
fn task_vm(pid: c_int) -> Option<VmStats> {
    let mut task: MachPort = 0;
    let kr = unsafe { task_name_for_pid(mach_task_self_, pid, &mut task) };
    if kr != KERN_SUCCESS {
        return None;
    }
    let mut info = TaskVmInfo::default();
    let mut count = TASK_VM_INFO_REV6_COUNT;
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
    Some(VmStats::from_info(&info, count))
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

/// Swap bytes attributed per compressed byte, or None when the swap total is
/// unavailable or no readable process holds compressed memory.
fn swap_ratio(swap_total: Option<u64>, total_compressed: u64) -> Option<f64> {
    swap_total
        .filter(|_| total_compressed > 0)
        .map(|swap| swap as f64 / total_compressed as f64)
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

/// N/A when the kernel's `task_vm_info` revision lacks the field.
fn opt_bytes(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "N/A".to_string(), |b| human(b as f64))
}

/// The kernel reports 0 both for "no jetsam limit" and for a process at or
/// over a soft limit; show it as "-" rather than an alarming 0B.
fn headroom_str(remaining: Option<u64>) -> String {
    match remaining {
        None => "N/A".to_string(),
        Some(0) => "-".to_string(),
        Some(b) => human(b as f64),
    }
}

/// Left-aligned identity columns: PID NAME, or PID PPID NAME PARENT.
fn ident_cols(pid: &str, name: &str, parent: Option<(&str, &str)>) -> String {
    match parent {
        Some((ppid, pname)) => format!(
            "{:<7} {:<7} {:<24} {:<20}",
            pid,
            ppid,
            truncate(name, 24),
            truncate(pname, 20)
        ),
        None => format!("{:<7} {:<24}", pid, truncate(name, 24)),
    }
}

/// Right-aligned metric columns, 12 wide each.
fn metric_cols<S: AsRef<str>>(cells: &[S]) -> String {
    cells
        .iter()
        .map(|c| format!(" {:>12}", c.as_ref()))
        .collect()
}

// ---- row model ------------------------------------------------------------

struct Row {
    pid: c_int,
    vm: VmStats,
}

// ---- snapshot mode --------------------------------------------------------

fn run_snapshot(topn: usize, show_parent: bool, extended: bool) {
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
            Some(vm) => rows.push(Row { pid, vm }),
            None => denied += 1,
        }
    }

    rows.sort_by_key(|r| (Reverse(r.vm.compressed), r.pid));
    let total_compressed: u64 = rows.iter().map(|r| r.vm.compressed).sum();

    let shown = if topn == 0 {
        rows.len()
    } else {
        topn.min(rows.len())
    };

    // parent-name cache (many children share one parent).
    let mut pname_cache: HashMap<c_int, String> = HashMap::new();

    let mut header = vec!["CMPRS", "FOOTPRINT", "SWAP~", "SWAPINS"];
    if extended {
        header.extend(["FOOT_PK", "CMPRS_PK", "DECOMP", "HEADROOM"]);
    }
    println!(
        "{}{}",
        ident_cols("PID", "NAME", show_parent.then_some(("PPID", "PARENT"))),
        metric_cols(&header)
    );

    // SWAP~ estimates are proportional splits of system swap across readable
    // processes, shown as N/A if the system swap total is unavailable or the
    // denominator is 0. When denied > 0 the denominator is incomplete and one
    // visible process may absorb all system swap; warn about that below.
    let swap_per_cmprs = swap_ratio(swap_total, total_compressed);
    let swap_str =
        |cmprs: u64| swap_per_cmprs.map_or_else(|| "N/A".to_string(), |k| human(cmprs as f64 * k));

    let mut sum_compressed = 0u64;
    let mut sum_footprint = 0u64;

    // Names and parents are looked up only for shown rows.
    for r in rows.iter().take(shown) {
        sum_compressed += r.vm.compressed;
        sum_footprint += r.vm.footprint;
        let name = proc_display_name(r.pid);

        let mut cells = vec![
            human(r.vm.compressed as f64),
            human(r.vm.footprint as f64),
            swap_str(r.vm.compressed),
            opt_bytes(r.vm.swapins),
        ];
        if extended {
            cells.extend([
                opt_bytes(r.vm.footprint_peak),
                human(r.vm.compressed_peak as f64),
                r.vm.decompressions
                    .map_or_else(|| "N/A".to_string(), |n| n.to_string()),
                headroom_str(r.vm.limit_remaining),
            ]);
        }

        let ident = if show_parent {
            let ppid = ppid_of(r.pid);
            let pname = pname_cache
                .entry(ppid)
                .or_insert_with(|| proc_display_name(ppid));
            ident_cols(&r.pid.to_string(), &name, Some((&ppid.to_string(), pname)))
        } else {
            ident_cols(&r.pid.to_string(), &name, None)
        };
        println!("{ident}{}", metric_cols(&cells));
    }
    let sum_swapins: Option<u64> = rows.iter().take(shown).map(|r| r.vm.swapins).sum();

    let sudo_hint = if denied > 0 {
        " — need sudo for other users"
    } else {
        ""
    };
    println!();
    println!(
        "shown {}/{} procs  ({} denied{})",
        shown,
        rows.len(),
        denied,
        sudo_hint
    );

    println!(
        "shown totals: CMPRS {}  FOOTPRINT {}  SWAP~ {}  SWAPINS {}",
        human(sum_compressed as f64),
        human(sum_footprint as f64),
        swap_str(sum_compressed),
        opt_bytes(sum_swapins),
    );

    let swap_total_str = swap_total.map_or_else(|| "N/A".to_string(), |b| human(b as f64));
    println!("swap (sysctl vm.swapusage used): {swap_total_str}");
    if swap_total.is_some() && swap_per_cmprs.is_none() {
        println!("SWAP~ N/A: denominator is 0 — run as root for accurate estimates");
    }
    if denied > 0 && swap_per_cmprs.is_some() {
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
    vm: VmStats,
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
        if let Some(vm) = task_vm(pid) {
            let info = bsdinfo(pid);
            let start = info
                .as_ref()
                .map(|i| (i.pbi_start_tvsec, i.pbi_start_tvusec));
            let ppid = info.as_ref().map(|i| i.pbi_ppid as c_int).unwrap_or(0);
            rows.insert(pid, Sample { vm, start, ppid });
        } else {
            denied += 1;
        }
    }
    SampleSet { rows, denied }
}

fn run_watch(interval: u64, topn: usize, show_parent: bool, extended: bool) {
    let a = sample();
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs(interval.max(1)));
    let b = sample();
    let elapsed = t0.elapsed().as_secs_f64().max(1e-9);

    struct Delta {
        pid: c_int,
        ppid: c_int,
        foot0: u64,
        foot1: u64,
        delta: i128,
        swapins: Option<u64>,
        decompressions: Option<u32>,
        headroom: Option<u64>,
    }
    let mut deltas: Vec<Delta> = Vec::new();
    for (pid, b_sample) in &b.rows {
        if let Some(a_sample) = a.rows.get(pid) {
            if !same_process_start(a_sample.start, b_sample.start) {
                continue; // PID reused or start time unavailable between samples
            }
            let (va, vb) = (a_sample.vm, b_sample.vm);
            deltas.push(Delta {
                pid: *pid,
                ppid: if show_parent { b_sample.ppid } else { 0 },
                foot0: va.footprint,
                foot1: vb.footprint,
                delta: vb.footprint as i128 - va.footprint as i128,
                swapins: va.swapins.zip(vb.swapins).map(|(x, y)| y.saturating_sub(x)),
                // 32-bit lifetime counter: wrapping_sub survives a wrap.
                decompressions: va
                    .decompressions
                    .zip(vb.decompressions)
                    .map(|(x, y)| y.wrapping_sub(x)),
                headroom: vb.limit_remaining,
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

    let mut header = vec!["FOOT0", "FOOT1", "DELTA", "RATE/s", "SWAPINS"];
    if extended {
        header.extend(["DECOMP", "HEADROOM"]);
    }
    println!(
        "{}{}",
        ident_cols("PID", "NAME", show_parent.then_some(("PPID", "PARENT"))),
        metric_cols(&header)
    );
    for d in deltas.iter().take(shown) {
        let rate = d.delta as f64 / elapsed;
        let name = proc_display_name(d.pid);

        let mut cells = vec![
            human(d.foot0 as f64),
            human(d.foot1 as f64),
            human_signed(d.delta as f64),
            human_signed(rate),
            d.swapins
                .map_or_else(|| "N/A".to_string(), |b| human_signed(b as f64)),
        ];
        if extended {
            cells.extend([
                d.decompressions
                    .map_or_else(|| "N/A".to_string(), |n| format!("+{n}")),
                headroom_str(d.headroom),
            ]);
        }

        let ident = if show_parent {
            let pname = pname_cache
                .entry(d.ppid)
                .or_insert_with(|| proc_display_name(d.ppid));
            ident_cols(
                &d.pid.to_string(),
                &name,
                Some((&d.ppid.to_string(), pname)),
            )
        } else {
            ident_cols(&d.pid.to_string(), &name, None)
        };
        println!("{ident}{}", metric_cols(&cells));
    }
    let interval_swapins: Option<u64> = deltas.iter().map(|d| d.swapins).sum();
    println!();
    println!(
        "interval {:.1}s — one interval = allocation churn; trust SUSTAINED growth as leak signal.",
        elapsed
    );
    println!(
        "swapped in during interval (all procs sampled twice): {}",
        opt_bytes(interval_swapins)
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
    extended: bool,
    watch_secs: Option<u64>,
    help: bool,
}

const USAGE: &str = "\
macswapi [N] [-p|--parent] [-x|--extended] [-w|--watch|--delta [SECS]]
  N              top N rows (default 20, 0=all)
  -p --parent    add PPID + parent-name columns (works in watch mode too)
  -x --extended  add FOOT_PK, CMPRS_PK, DECOMP, HEADROOM (watch: DECOMP, HEADROOM)
  -w --watch     single-interval leak check; SECS must immediately follow the flag
     --delta     clearer alias for --watch (default interval 3s)
                 SECS must be positive; N must appear before the interval flag
                 example: macswapi 50 --delta 5
  -h --help      this help
";

fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut config = Config {
        topn: 20,
        show_parent: false,
        extended: false,
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
            "-x" | "--extended" => config.extended = true,
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
    // Restore default SIGPIPE so piping into `head`/`less` exits quietly
    // instead of panicking on a broken-pipe write (Rust ignores SIGPIPE).
    unsafe { signal(SIGPIPE, SIG_DFL) };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match parse_args(&args) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    };

    if config.help {
        print!("{USAGE}");
        return;
    }

    if let Some(watch_secs) = config.watch_secs {
        run_watch(watch_secs, config.topn, config.show_parent, config.extended);
    } else {
        run_snapshot(config.topn, config.show_parent, config.extended);
    }
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

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
    fn test_swap_ratio() {
        assert_eq!(swap_ratio(None, 100), None); // swap total unavailable
        assert_eq!(swap_ratio(Some(1000), 0), None); // zero denominator
        assert_eq!(swap_ratio(Some(1000), 100), Some(10.0)); // proportional
        assert_eq!(swap_ratio(Some(0), 100), Some(0.0)); // zero swap
    }

    #[test]
    fn test_conservation() {
        let compressed = [10u64, 25, 3, 100, 7, 512, 64, 1];
        let total: u64 = compressed.iter().sum();
        let swap = 4_294_967_296u64; // 4G
        let k = swap_ratio(Some(swap), total).unwrap();
        let sum: f64 = compressed.iter().map(|&c| c as f64 * k).sum();
        assert!((sum - swap as f64).abs() < 1e-3, "sum={sum} swap={swap}");
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
                extended: false,
                watch_secs: Some(5),
                help: false,
            }
        );
    }

    #[test]
    fn test_parse_args_extended() {
        let config = parse(&["10", "-x", "--watch", "2"]).unwrap();
        assert!(config.extended);
        assert_eq!((config.topn, config.watch_secs), (10, Some(2)));
        assert!(parse(&["--extended"]).unwrap().extended);
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
        // offsetof() values from <mach/task_info.h>; it is #pragma pack(4), so
        // ledger_swapins sits at 348, where natural alignment would give 352.
        assert_eq!(offset_of!(TaskVmInfo, compressed), 120);
        assert_eq!(offset_of!(TaskVmInfo, compressed_peak), 128);
        assert_eq!(offset_of!(TaskVmInfo, phys_footprint), 144);
        assert_eq!(offset_of!(TaskVmInfo, ledger_phys_footprint_peak), 168);
        assert_eq!(offset_of!(TaskVmInfo, limit_bytes_remaining), 336);
        assert_eq!(offset_of!(TaskVmInfo, decompressions), 344);
        assert_eq!(offset_of!(TaskVmInfo, ledger_swapins), 348);
        assert_eq!(size_of::<TaskVmInfo>(), 356);
    }

    #[test]
    fn test_task_vm_info_revision_counts() {
        // Each revision's count ends where the next revision's first field starts.
        let units = |bytes: usize| (bytes / 4) as MsgTypeNumber;
        assert_eq!(
            units(offset_of!(TaskVmInfo, limit_bytes_remaining)),
            TASK_VM_INFO_REV3_COUNT
        );
        assert_eq!(
            units(offset_of!(TaskVmInfo, decompressions)),
            TASK_VM_INFO_REV4_COUNT
        );
        assert_eq!(
            units(offset_of!(TaskVmInfo, ledger_swapins)),
            TASK_VM_INFO_REV5_COUNT
        );
        assert_eq!(units(size_of::<TaskVmInfo>()), TASK_VM_INFO_REV6_COUNT);
    }

    #[test]
    fn test_vm_stats_drops_fields_past_returned_revision() {
        let info = TaskVmInfo {
            ledger_phys_footprint_peak: 7,
            limit_bytes_remaining: 5,
            decompressions: 3,
            ledger_swapins: 2,
            ..TaskVmInfo::default()
        };
        let fields = |count| {
            let v = VmStats::from_info(&info, count);
            (
                v.footprint_peak,
                v.limit_remaining,
                v.decompressions,
                v.swapins,
            )
        };
        assert_eq!(
            fields(TASK_VM_INFO_REV6_COUNT),
            (Some(7), Some(5), Some(3), Some(2))
        );
        assert_eq!(
            fields(TASK_VM_INFO_REV5_COUNT),
            (Some(7), Some(5), Some(3), None)
        );
        assert_eq!(fields(TASK_VM_INFO_REV3_COUNT), (Some(7), None, None, None));
        assert_eq!(fields(38), (None, None, None, None)); // rev1: through phys_footprint
    }

    #[test]
    fn test_vm_stats_clamps_negative_ledgers() {
        let info = TaskVmInfo {
            ledger_phys_footprint_peak: -1,
            ledger_swapins: -1,
            ..TaskVmInfo::default()
        };
        let v = VmStats::from_info(&info, TASK_VM_INFO_REV6_COUNT);
        assert_eq!((v.footprint_peak, v.swapins), (Some(0), Some(0)));
    }

    #[test]
    fn test_headroom_str() {
        assert_eq!(headroom_str(None), "N/A");
        assert_eq!(headroom_str(Some(0)), "-"); // no limit (or at/over a soft one)
        assert_eq!(headroom_str(Some(1024)), "1.00K");
    }

    #[test]
    fn test_ident_and_metric_cols() {
        assert_eq!(
            ident_cols("1", "launchd", None),
            "1       launchd                 "
        );
        assert_eq!(
            ident_cols("9", "x", Some(("1", "launchd"))),
            "9       1       x                        launchd             "
        );
        assert_eq!(metric_cols(&["1.00K", "N/A"]), "        1.00K          N/A");
    }

    #[test]
    fn test_proc_bsdinfo_offset() {
        assert_eq!(offset_of!(ProcBsdInfo, pbi_ppid), 16);
    }

    #[test]
    fn test_xsw_usage_offset() {
        assert_eq!(offset_of!(XswUsage, xsu_used), 16);
    }
}
