//! macswapi — per-process memory & swap inspector for macOS.
//!
//! macOS exposes no real per-process swap counter (the compressor swaps shared
//! segments, not PIDs). We report EXACT compressed/footprint from Mach
//! `task_info(TASK_VM_INFO)` and ESTIMATE per-proc swap by splitting the real
//! `vm.swapusage` total proportional to each proc's compressed memory.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{BufRead, BufReader, IsTerminal};
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

/// Returns (compressed, phys_footprint) or None if denied (another user's
/// process without root). A task *name* port is enough for `task_info`;
/// `task_for_pid` would need root and is still refused for SIP-protected procs.
fn task_vm(pid: c_int) -> Option<(u64, u64)> {
    let mut task: MachPort = 0;
    let kr = unsafe { task_name_for_pid(mach_task_self_, pid, &mut task) };
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

// ---- row model ------------------------------------------------------------

struct Row {
    pid: c_int,
    compressed: u64,
    footprint: u64,
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
                compressed,
                footprint,
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
        sum_compressed += r.compressed;
        sum_footprint += r.footprint;
        let name = proc_display_name(r.pid);

        if show_parent {
            let ppid = ppid_of(r.pid);
            let pname = pname_cache
                .entry(ppid)
                .or_insert_with(|| proc_display_name(ppid))
                .clone();
            println!(
                "{:<7} {:<7} {:<24} {:<20} {:>12} {:>12} {:>12}",
                r.pid,
                ppid,
                truncate(&name, 24),
                truncate(&pname, 20),
                human(r.compressed as f64),
                human(r.footprint as f64),
                swap_str(r.compressed),
            );
        } else {
            println!(
                "{:<7} {:<24} {:>12} {:>12} {:>12}",
                r.pid,
                truncate(&name, 24),
                human(r.compressed as f64),
                human(r.footprint as f64),
                swap_str(r.compressed),
            );
        }
    }

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
        "shown totals: CMPRS {}  FOOTPRINT {}  SWAP~ {}",
        human(sum_compressed as f64),
        human(sum_footprint as f64),
        swap_str(sum_compressed),
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
        let name = proc_display_name(d.pid);
        if show_parent {
            let pname = pname_cache
                .entry(d.ppid)
                .or_insert_with(|| proc_display_name(d.ppid))
                .clone();
            println!(
                "{:<7} {:<7} {:<24} {:<20} {:>12} {:>12} {:>12} {:>12}",
                d.pid,
                d.ppid,
                truncate(&name, 24),
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
                truncate(&name, 24),
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

// ---- log-event history (--logs) -------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum EventKind {
    Pressure,
    Jetsam,
    Swapfile,
}

impl EventKind {
    fn label(self) -> &'static str {
        match self {
            EventKind::Pressure => "pressure",
            EventKind::Jetsam => "jetsam",
            EventKind::Swapfile => "swapfile",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct LogEvent {
    date: String, // "2026-07-15"
    time: String, // "14:24:01" (milliseconds stripped)
    kind: EventKind,
    pid: Option<i32>,        // jetsam only
    process: Option<String>, // jetsam only
    message: String,         // raw kernel message
}

// Coarse filter applied at the `log` layer; classify_event() below is the
// authoritative typing. Scoped to kernel + the keywords we care about, and the
// memorystatus branch requires "kill" to skip the flood of benign
// "memorystatus: Denying dirty-tracking..." / "opted in..." lines.
// ponytail: kernel-only sender; widen if swapfile events from dynamic_pager/launchd get missed.
const LOG_PREDICATE: &str = "process == \"kernel\" AND (\
(eventMessage CONTAINS[c] \"memorystatus\" AND eventMessage CONTAINS[c] \"kill\") OR \
(eventMessage CONTAINS[c] \"pressure\" AND (eventMessage CONTAINS[c] \"memory\" OR eventMessage CONTAINS[c] \"vm\")) OR \
eventMessage CONTAINS[c] \"swapfile\" OR \
eventMessage CONTAINS[c] \"macx_swap\")";

/// Classify a raw kernel message. Order matters: a jetsam kill line may also
/// mention "pressure", but the kill IS the event.
fn classify_event(msg: &str) -> Option<EventKind> {
    let m = msg.to_ascii_lowercase();
    if m.contains("swapfile") || m.contains("macx_swap") {
        Some(EventKind::Swapfile)
    } else if m.contains("memorystatus: killing (idle)") {
        // The kernel logs each idle kill twice: `killing (idle) NAME [PID] ...`
        // then `killing_idle_process pid PID [NAME] ...`. Keep only the second,
        // whose layout extract_jetsam() understands.
        None
    } else if m.contains("memorystatus") && m.contains("kill") {
        Some(EventKind::Jetsam)
    } else if m.contains("pressure") && (m.contains("memory") || m.contains("vm")) {
        Some(EventKind::Pressure)
    } else {
        None
    }
}

/// Best-effort pid + process name from a jetsam message. Formats vary across
/// macOS versions; we scan for "pid N" / "process N" and the first bracketed
/// non-numeric token "[Name]". Returns whatever is found (both optional).
fn extract_jetsam(msg: &str) -> (Option<i32>, Option<String>) {
    let pid = pid_after_keyword(msg, "pid ")
        .or_else(|| pid_after_keyword(msg, "process "))
        .or_else(|| bracketed_number(msg));

    // First "[...]" whose contents are not purely a number → the process name.
    let mut process = None;
    let mut rest = msg;
    while let Some(open) = rest.find('[') {
        if let Some(close_rel) = rest[open + 1..].find(']') {
            let inner = &rest[open + 1..open + 1 + close_rel];
            if !inner.is_empty() && !inner.chars().all(|c| c.is_ascii_digit()) {
                process = Some(inner.to_string());
                break;
            }
            rest = &rest[open + 1 + close_rel + 1..];
        } else {
            break;
        }
    }
    (pid, process)
}

/// Parse the digits immediately following `keyword` (e.g. "pid ") as an i32.
fn pid_after_keyword(msg: &str, keyword: &str) -> Option<i32> {
    let start = msg.find(keyword)? + keyword.len();
    let digits: String = msg[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// First "[N]" bracketed pure-number token, as an i32.
fn bracketed_number(msg: &str) -> Option<i32> {
    let mut rest = msg;
    while let Some(open) = rest.find('[') {
        let close_rel = rest[open + 1..].find(']')?;
        let inner = &rest[open + 1..open + 1 + close_rel];
        if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()) {
            return inner.parse().ok();
        }
        rest = &rest[open + 1 + close_rel + 1..];
    }
    None
}

/// Parse one `log show --style compact` line into a LogEvent, or None for the
/// header, blank lines, and messages that don't classify.
///
/// Compact line: `2026-07-15 14:24:01.804 Df kernel[0:9a59c] <message...>`
/// timestamp = first two whitespace tokens; message = text after the first `]`
/// (which closes the `process[pid:tid]` field).
fn parse_compact_line(line: &str) -> Option<LogEvent> {
    if line.is_empty() || line.starts_with("Timestamp") {
        return None;
    }
    let mut tokens = line.splitn(3, char::is_whitespace);
    let date = tokens.next()?;
    let time_full = tokens.next()?;
    let remainder = tokens.next()?;

    // message begins after the first ']' (end of process[pid:tid]).
    let close = remainder.find(']')?;
    let message = remainder[close + 1..].trim();
    if message.is_empty() {
        return None;
    }

    let kind = classify_event(message)?;
    let (pid, process) = if kind == EventKind::Jetsam {
        extract_jetsam(message)
    } else {
        (None, None)
    };

    // Sanity-check the timestamp shape without a date library.
    if date.len() != 10 || !date.as_bytes()[4].eq(&b'-') {
        return None;
    }
    let time = time_full.split('.').next().unwrap_or(time_full);

    Some(LogEvent {
        date: date.to_string(),
        time: time.to_string(),
        kind,
        pid,
        process,
        message: message.to_string(),
    })
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn event_json(e: &LogEvent) -> String {
    let mut fields = vec![
        format!("\"time\": \"{} {}\"", e.date, json_escape(&e.time)),
        format!("\"type\": \"{}\"", e.kind.label()),
    ];
    if let Some(pid) = e.pid {
        fields.push(format!("\"pid\": {pid}"));
    }
    if let Some(p) = &e.process {
        fields.push(format!("\"process\": \"{}\"", json_escape(p)));
    }
    fields.push(format!("\"message\": \"{}\"", json_escape(&e.message)));
    format!("{{{}}}", fields.join(", "))
}

/// Spawns `log show` with its stdout piped to us; its stderr passes through.
fn spawn_log_show(hours: u64) -> std::io::Result<std::process::Child> {
    std::process::Command::new("/usr/bin/log")
        .args([
            "show",
            "--last",
            &format!("{hours}h"),
            "--style",
            "compact",
            "--predicate",
            LOG_PREDICATE,
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
}

/// Returns the process exit code. Text events print as `log show` emits them,
/// since a long query can take minutes; JSON prints once the query finishes.
fn run_logs(hours: u64, format: OutputFormat) -> i32 {
    if std::io::stderr().is_terminal() {
        eprintln!("querying unified log for the last {hours}h (can take minutes)…");
    }
    let mut child = match spawn_log_show(hours) {
        Ok(child) => child,
        Err(e) => {
            eprintln!("could not run /usr/bin/log: {e}");
            return 1;
        }
    };
    let stdout = child.stdout.take().expect("stdout is piped");
    let events = BufReader::new(stdout)
        .split(b'\n')
        .map_while(Result::ok)
        .filter_map(|line| parse_compact_line(&String::from_utf8_lossy(&line)));

    let mut json_items: Vec<String> = Vec::new();
    let mut count = 0usize;
    let mut prev_date: Option<String> = None;
    for e in events {
        count += 1;
        match format {
            OutputFormat::Json => json_items.push(event_json(&e)),
            OutputFormat::Text => {
                if count == 1 {
                    println!("{:<9} {:<9} DETAIL", "TIME", "TYPE");
                }
                if prev_date.as_deref() != Some(e.date.as_str()) {
                    println!("-- {} --", e.date);
                    prev_date = Some(e.date.clone());
                }
                println!("{:<9} {:<9} {}", e.time, e.kind.label(), e.message);
            }
        }
    }

    match child.wait() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("log show failed ({status})");
            return 1;
        }
        Err(e) => {
            eprintln!("could not wait for /usr/bin/log: {e}");
            return 1;
        }
    }
    match format {
        OutputFormat::Json => println!("[{}]", json_items.join(", ")),
        OutputFormat::Text if count == 0 => {
            let unit = if hours == 1 { "hour" } else { "hours" };
            println!("no memory events in last {hours} {unit}");
        }
        OutputFormat::Text => {}
    }
    0
}

// ---- CLI ------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, PartialEq, Eq)]
struct Config {
    topn: usize,
    show_parent: bool,
    watch_secs: Option<u64>,
    logs_hours: Option<u64>,
    format: OutputFormat,
    help: bool,
}

const USAGE: &str = "\
macswapi [N] [-p|--parent] [-w|--watch|--delta [SECS]]
macswapi -l|--logs [HOURS] [-f|--format text|json]
  N            top N rows (default 20, 0=all)
  -p --parent  add PPID + parent-name columns (works in watch mode too)
  -w --watch   single-interval leak check; SECS must immediately follow the flag
     --delta   clearer alias for --watch (default interval 3s)
               SECS must be positive; N must appear before the interval flag
               example: macswapi 50 --delta 5
  -l --logs    memory-event timeline from the unified log (default 1 hour)
               events: memory-pressure changes, jetsam kills, swapfile lifecycle
               HOURS must be positive and immediately follow the flag
  -f --format  output format for --logs: text (default) or json
  -h --help    this help
";

fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut config = Config {
        topn: 20,
        show_parent: false,
        watch_secs: None,
        logs_hours: None,
        format: OutputFormat::Text,
        help: false,
    };
    let mut saw_interval_flag = false;
    let mut saw_format_flag = false;
    let mut saw_topn = false;

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
            "-l" | "--logs" => {
                if config.logs_hours.is_some() {
                    return Err("duplicate --logs flag".to_string());
                }
                config.logs_hours = Some(1);
                if i + 1 < args.len() {
                    if let Ok(hours) = args[i + 1].parse::<u64>() {
                        if hours == 0 {
                            return Err("--logs HOURS must be positive".to_string());
                        }
                        config.logs_hours = Some(hours);
                        i += 1;
                    }
                }
            }
            "-f" | "--format" => {
                saw_format_flag = true;
                match args.get(i + 1).map(String::as_str) {
                    Some("text") => config.format = OutputFormat::Text,
                    Some("json") => config.format = OutputFormat::Json,
                    Some(other) => {
                        return Err(format!("unknown --format: {other} (want text|json)"))
                    }
                    None => return Err("--format needs a value (text|json)".to_string()),
                }
                i += 1;
            }
            other => {
                if let Ok(topn) = other.parse::<usize>() {
                    if saw_interval_flag {
                        return Err("N must appear before the interval flag".to_string());
                    }
                    config.topn = topn;
                    saw_topn = true;
                } else {
                    return Err(format!("unknown arg: {other}"));
                }
            }
        }
        i += 1;
    }

    if config.watch_secs.is_some() && config.logs_hours.is_some() {
        return Err("--logs and --watch are mutually exclusive".to_string());
    }
    if saw_format_flag && config.logs_hours.is_none() {
        return Err("--format only applies to --logs".to_string());
    }
    if config.logs_hours.is_some() && (saw_topn || config.show_parent) {
        return Err("N and --parent do not apply to --logs".to_string());
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

    if let Some(hours) = config.logs_hours {
        std::process::exit(run_logs(hours, config.format));
    } else if let Some(watch_secs) = config.watch_secs {
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
                watch_secs: Some(5),
                logs_hours: None,
                format: OutputFormat::Text,
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
    fn test_parse_args_logs_defaults_and_hours() {
        assert_eq!(parse(&["--logs"]).unwrap().logs_hours, Some(1));
        assert_eq!(parse(&["-l", "4"]).unwrap().logs_hours, Some(4));
        assert!(parse(&["--logs", "0"]).is_err());
    }

    #[test]
    fn test_parse_args_rejects_snapshot_options_with_logs() {
        assert!(parse(&["5", "--logs"]).is_err());
        assert!(parse(&["--logs", "4", "10"]).is_err()); // 4 = HOURS, 10 = N
        assert!(parse(&["--logs", "-p"]).is_err());
    }

    #[test]
    fn test_parse_args_format_requires_logs_and_valid_value() {
        assert_eq!(
            parse(&["--logs", "-f", "json"]).unwrap().format,
            OutputFormat::Json
        );
        assert!(parse(&["-f", "json"]).is_err()); // --format without --logs
        assert!(parse(&["--logs", "-f", "yaml"]).is_err()); // bad value
        assert!(parse(&["--logs", "--format"]).is_err()); // missing value
        assert!(parse(&["--logs", "--watch", "5"]).is_err()); // mutually exclusive
    }

    // Real kernel messages captured with `log show --style compact`.
    // KILL_IDLE_DUP is logged ~15ms before KILL_IDLE for the same kill.
    const KILL_SPECIFIC: &str = "memorystatus: killing_specific_process pid 58521 \
        [knowledgeconstructiond] (per-process-limit 0 0s rf:- type:daemon) 20192KB - \
        memorystatus_available_pages: 1820483";
    const KILL_IDLE: &str = "memorystatus: killing_idle_process pid 414 [sandboxd] \
        (idle-exit 0 90330s rf:- type:daemon) 3168KB - memorystatus_available_pages: 789885 \
        compressor_size:795015";
    const KILL_IDLE_DUP: &str = "memorystatus: killing (idle) sandboxd [414] due to idle-exit (9)";

    #[test]
    fn test_classify_event() {
        assert_eq!(classify_event(KILL_SPECIFIC), Some(EventKind::Jetsam));
        assert_eq!(classify_event(KILL_IDLE), Some(EventKind::Jetsam));
        // duplicate of KILL_IDLE: skipped so each kill is reported once
        assert_eq!(classify_event(KILL_IDLE_DUP), None);
        // noise: memorystatus but no kill
        assert_eq!(
            classify_event("memorystatus: Denying dirty-tracking opt-in for managed VPN [15607]"),
            None
        );
        assert_eq!(
            classify_event("vm memory pressure level changed to 2"),
            Some(EventKind::Pressure)
        );
        assert_eq!(
            classify_event("macx_swapon SUCCESS - swapfile /private/var/vm/swapfile1"),
            Some(EventKind::Swapfile)
        );
        // a kill mentioning pressure still classifies as jetsam
        assert_eq!(
            classify_event("memorystatus: killing process 5 [App] because of memory pressure"),
            Some(EventKind::Jetsam)
        );
        assert_eq!(classify_event("unrelated kernel chatter"), None);
    }

    #[test]
    fn test_extract_jetsam() {
        assert_eq!(
            extract_jetsam(KILL_SPECIFIC),
            (Some(58521), Some("knowledgeconstructiond".to_string()))
        );
        assert_eq!(
            extract_jetsam(KILL_IDLE),
            (Some(414), Some("sandboxd".to_string()))
        );
        // "process N" form, name in brackets, no "pid" token
        assert_eq!(
            extract_jetsam("Killing process 1234 [Safari] in high band FOREGROUND"),
            (Some(1234), Some("Safari".to_string()))
        );
        // graceful: unknown shape yields whatever is found
        assert_eq!(extract_jetsam("killed something"), (None, None));
    }

    #[test]
    fn test_parse_compact_line() {
        let line = format!("2026-10-21 20:09:19.943 Df kernel[0:877] {KILL_IDLE}");
        let e = parse_compact_line(&line).unwrap();
        assert_eq!(e.date, "2026-10-21");
        assert_eq!(e.time, "20:09:19"); // ms stripped
        assert_eq!(e.kind, EventKind::Jetsam);
        assert_eq!(e.pid, Some(414));
        assert_eq!(e.process.as_deref(), Some("sandboxd"));
        assert_eq!(e.message, KILL_IDLE);

        // the idle-kill duplicate yields no event
        let dup = format!("2026-10-21 20:09:19.929 Df kernel[0:877] {KILL_IDLE_DUP}");
        assert!(parse_compact_line(&dup).is_none());

        // header and noise → None
        assert!(parse_compact_line("Timestamp               Ty Process[PID:TID]").is_none());
        assert!(parse_compact_line(
            "2026-07-15 14:24:01.804 Df kernel[0:9a59c] memorystatus: Denying dirty-tracking [15607]"
        )
        .is_none());
        assert!(parse_compact_line("").is_none());
    }

    #[test]
    fn test_json_escape() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("line\nfeed"), "line\\nfeed");
        assert_eq!(json_escape("\u{1}"), "\\u0001");
    }

    #[test]
    fn test_event_json_omits_pid_for_non_jetsam() {
        let e = LogEvent {
            date: "2026-07-15".to_string(),
            time: "14:24:01".to_string(),
            kind: EventKind::Pressure,
            pid: None,
            process: None,
            message: "memory pressure level 2".to_string(),
        };
        let j = event_json(&e);
        assert!(j.contains("\"type\": \"pressure\""));
        assert!(j.contains("\"time\": \"2026-07-15 14:24:01\""));
        assert!(!j.contains("\"pid\""));
        assert!(!j.contains("\"process\""));
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
