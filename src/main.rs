//! macswapi — per-process memory & swap inspector for macOS.
//!
//! macOS exposes no real per-process swap counter (the compressor swaps shared
//! segments, not PIDs). We report EXACT compressed/footprint from Mach
//! `task_info(TASK_VM_INFO)` and ESTIMATE per-proc swap by splitting the real
//! `vm.swapusage` total proportional to each proc's compressed memory.

use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::ffi::{c_void, CStr, CString};
use std::fmt::Write as _;
use std::io::Write as _;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
const HOST_VM_INFO64: c_int = 4;
// `integer_t` counts per `vm_statistics64` revision (<mach/host_info.h>).
const HOST_VM_INFO64_REV1_COUNT: MsgTypeNumber = 38;
const HOST_VM_INFO64_REV2_COUNT: MsgTypeNumber = 40;
const PROC_ALL_PIDS: u32 = 1;
const PROC_PIDTASKINFO: c_int = 4;
const PROC_PIDTBSDINFO: c_int = 3;
const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;
const RUSAGE_INFO_V2: c_int = 2;

// ---- FFI (libSystem, linked automatically on macOS) -----------------------

extern "C" {
    static mach_task_self_: MachPort;
    fn mach_host_self() -> MachPort;
    fn task_name_for_pid(target: MachPort, pid: c_int, t: *mut MachPort) -> KernReturn;
    fn task_info(
        task: MachPort,
        flavor: TaskFlavor,
        out: *mut c_int,
        count: *mut MsgTypeNumber,
    ) -> KernReturn;
    fn host_statistics64(
        host: MachPort,
        flavor: c_int,
        out: *mut c_int,
        count: *mut MsgTypeNumber,
    ) -> KernReturn;
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
    fn proc_listpids(t: u32, ti: u32, buf: *mut c_void, sz: c_int) -> c_int;
    fn proc_pidpath(pid: c_int, buf: *mut c_void, sz: u32) -> c_int;
    fn proc_name(pid: c_int, buf: *mut c_void, sz: u32) -> c_int;
    fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buf: *mut c_void, sz: c_int) -> c_int;
    fn proc_pid_rusage(pid: c_int, flavor: c_int, buf: *mut c_void) -> c_int;
    fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
    fn getpwnam(name: *const c_char) -> *const Passwd;
    fn getpwuid(uid: u32) -> *const Passwd;
    fn isatty(fd: c_int) -> c_int;
    fn ioctl(fd: c_int, request: u64, ...) -> c_int;
    fn tcgetattr(fd: c_int, t: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, action: c_int, t: *const Termios) -> c_int;
    fn poll(fds: *mut PollFd, nfds: u32, timeout_ms: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
    fn signal(sig: c_int, handler: usize) -> usize;
}

const SIGHUP: c_int = 1;
const SIGINT: c_int = 2;
const SIGPIPE: c_int = 13;
const SIGTERM: c_int = 15;
const SIG_DFL: usize = 0;
const TIOCGWINSZ: u64 = 0x4008_7468;
const ICANON: u64 = 0x100;
const ECHO: u64 = 0x8;
const VMIN: usize = 16;
const VTIME: usize = 17;
const TCSANOW: c_int = 0;
const POLLIN: i16 = 1;

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

/// `task_vm_info` rev3 ledgers as (total, compressed) pairs, in header order.
const LEDGER_NAMES: [&str; 10] = [
    "purgeable nonvolatile",
    "purgeable volatile",
    "network nonvolatile",
    "network volatile",
    "media footprint",
    "media nofootprint",
    "graphics footprint",
    "graphics nofootprint",
    "neural footprint",
    "neural nofootprint",
];

/// `proc_bsdinfo` — full struct required (proc_pidinfo returns full size or fails).
#[repr(C)]
#[derive(Default)]
struct ProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32, // off 16  <-- READ THIS
    pbi_uid: u32,  // off 20  <-- READ THIS
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
    pbi_start_tvsec: u64, // off 120  <-- READ THIS
    pbi_start_tvusec: u64,
}

/// `proc_taskinfo` — full struct required, like `proc_bsdinfo`.
#[repr(C)]
#[derive(Default)]
struct ProcTaskInfo {
    pti_virtual_size: u64,
    pti_resident_size: u64,
    pti_total_user: u64,
    pti_total_system: u64,
    pti_threads_user: u64,
    pti_threads_system: u64,
    pti_policy: i32,
    pti_faults: i32,     // off 52  <-- READ THIS
    pti_pageins: i32,    // off 56  <-- READ THIS
    pti_cow_faults: i32, // off 60  <-- READ THIS
    pti_messages_sent: i32,
    pti_messages_received: i32,
    pti_syscalls_mach: i32,
    pti_syscalls_unix: i32,
    pti_csw: i32,
    pti_threadnum: i32, // off 84  <-- READ THIS
    pti_numrunning: i32,
    pti_priority: i32,
}

/// `rusage_info_v2` — full struct required: `proc_pid_rusage` writes all of it.
#[repr(C)]
#[derive(Default)]
struct RusageInfoV2 {
    ri_uuid: [u8; 16],
    ri_user_time: u64,
    ri_system_time: u64,
    ri_pkg_idle_wkups: u64,
    ri_interrupt_wkups: u64,
    ri_pageins: u64,    // off 48  <-- READ THIS
    ri_wired_size: u64, // off 56  <-- READ THIS
    ri_resident_size: u64,
    ri_phys_footprint: u64,
    ri_proc_start_abstime: u64,
    ri_proc_exit_abstime: u64,
    ri_child_user_time: u64,
    ri_child_system_time: u64,
    ri_child_pkg_idle_wkups: u64,
    ri_child_interrupt_wkups: u64,
    ri_child_pageins: u64,
    ri_child_elapsed_abstime: u64,
    ri_diskio_bytesread: u64,    // off 144  <-- READ THIS
    ri_diskio_byteswritten: u64, // off 152  <-- READ THIS
}

/// `xsw_usage` — swap totals from `vm.swapusage`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct XswUsage {
    xsu_total: u64,
    xsu_avail: u64,
    xsu_used: u64, // off 16  <-- READ THIS
    xsu_pagesize: u32,
    xsu_encrypted: i32,
}

/// `vm_statistics64` truncated through rev2 (`swapped_count`); same prefix
/// rule as `task_info`: `host_statistics64` returns the count it wrote.
#[repr(C)]
#[derive(Default)]
struct VmStatistics64 {
    free_count: u32,     // off 0
    active_count: u32,   // off 4
    inactive_count: u32, // off 8
    wire_count: u32,     // off 12
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,        // off 96
    compressions: u64,          // off 104
    swapins: u64,               // off 112
    swapouts: u64,              // off 120
    compressor_page_count: u32, // off 128
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64, // off 144
    // rev2
    swapped_count: u64, // off 152
}
// size_of == 160, /4 == 40 = HOST_VM_INFO64_REV2_COUNT.

/// `struct passwd` prefix through `pw_uid`; only read through libc's pointer.
#[repr(C)]
struct Passwd {
    pw_name: *const c_char, // off 0
    pw_passwd: *const c_char,
    pw_uid: u32, // off 16
}

/// `struct termios` (tcflag_t and speed_t are `unsigned long`).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Termios {
    c_iflag: u64,
    c_oflag: u64,
    c_cflag: u64,
    c_lflag: u64,   // off 24
    c_cc: [u8; 20], // off 32
    c_ispeed: u64,  // off 56
    c_ospeed: u64,
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

#[repr(C)]
#[derive(Default)]
struct WinSize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

// ---- core kernel functions ------------------------------------------------

fn swap_usage() -> Result<XswUsage, std::io::Error> {
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
        Ok(xsw)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// A numeric sysctl. A narrower kernel value fills the low bytes, which is
/// right on both Mac architectures (little-endian).
fn sysctl_num<T: Default>(name: &CStr) -> Option<T> {
    let mut value = T::default();
    let mut size = std::mem::size_of::<T>();
    let rc = unsafe {
        sysctlbyname(
            name.as_ptr(),
            &mut value as *mut T as *mut c_void,
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(value)
}

fn host_vm() -> Option<(VmStatistics64, MsgTypeNumber)> {
    let host = unsafe { mach_host_self() };
    let mut info = VmStatistics64::default();
    let mut count = HOST_VM_INFO64_REV2_COUNT;
    let kr = unsafe {
        host_statistics64(
            host,
            HOST_VM_INFO64,
            &mut info as *mut _ as *mut c_int,
            &mut count,
        )
    };
    unsafe { mach_port_deallocate(mach_task_self_, host) };
    (kr == KERN_SUCCESS && count >= HOST_VM_INFO64_REV1_COUNT).then_some((info, count))
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

/// The raw struct and the count `task_info` wrote, or None if denied (another
/// user's process without root). A task *name* port is enough for
/// `task_info`; `task_for_pid` would need root and is still refused for
/// SIP-protected procs.
fn task_vm_raw(pid: c_int) -> Option<(TaskVmInfo, MsgTypeNumber)> {
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
    Some((info, count))
}

fn task_vm(pid: c_int) -> Option<VmStats> {
    task_vm_raw(pid).map(|(info, count)| VmStats::from_info(&info, count))
}

/// A fixed-size `proc_pidinfo` flavor: it returns the full size or fails.
fn pidinfo<T: Default>(pid: c_int, flavor: c_int) -> Option<T> {
    let mut info = T::default();
    let sz = std::mem::size_of::<T>() as c_int;
    let rc = unsafe { proc_pidinfo(pid, flavor, 0, &mut info as *mut T as *mut c_void, sz) };
    (rc == sz).then_some(info)
}

fn rusage(pid: c_int) -> Option<RusageInfoV2> {
    let mut info = RusageInfoV2::default();
    let rc = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V2, &mut info as *mut _ as *mut c_void) };
    (rc == 0).then_some(info)
}

fn cstr_buf(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn proc_path(pid: c_int) -> Option<String> {
    let mut buf = [0u8; PROC_PIDPATHINFO_MAXSIZE];
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if n > 0 && (n as usize) <= buf.len() {
        Some(cstr_buf(&buf[..n as usize])).filter(|p| !p.is_empty())
    } else {
        None
    }
}

fn proc_display_name(pid: c_int) -> String {
    if let Some(path) = proc_path(pid) {
        return basename(&path).to_string();
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

fn user_name(uid: u32) -> Option<String> {
    let pw = unsafe { getpwuid(uid) };
    if pw.is_null() {
        return None;
    }
    let name = unsafe { (*pw).pw_name };
    (!name.is_null()).then(|| {
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned()
    })
}

/// `--user` takes a login name or a numeric uid.
fn resolve_user(user: &str) -> Result<u32, String> {
    if let Ok(uid) = user.parse::<u32>() {
        return Ok(uid);
    }
    let name = CString::new(user).map_err(|_| format!("bad user name: {user:?}"))?;
    let pw = unsafe { getpwnam(name.as_ptr()) };
    if pw.is_null() {
        Err(format!("unknown user: {user}"))
    } else {
        Ok(unsafe { (*pw).pw_uid })
    }
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

/// Compact duration: 42s, 13m05s, 5h03m, 3d04h.
fn duration_str(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        3600..=86_399 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d{:02}h", s / 86_400, s % 86_400 / 3600),
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

/// Adds two optional values; missing on one side keeps the other.
fn sum_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x + y),
        (x, None) => x,
        (None, y) => y,
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Whole number for CSV, without a "-0".
fn csv_num(v: f64) -> String {
    let r = v.round();
    if r == 0.0 {
        "0".to_string()
    } else {
        format!("{r:.0}")
    }
}

// ---- process samples ------------------------------------------------------

type StartTime = (u64, u64);

/// One readable process at one instant.
struct Proc {
    pid: c_int,
    ppid: c_int,
    uid: Option<u32>,
    start: Option<StartTime>,
    name: String,
    vm: VmStats,
}

struct SampleSet {
    procs: HashMap<c_int, Proc>,
    denied: usize,
    at: Instant,
}

impl SampleSet {
    fn total(&self) -> usize {
        self.procs.len() + self.denied
    }

    /// The SWAP~ denominator: every readable process, whatever is filtered out.
    fn total_compressed(&self) -> u64 {
        self.procs.values().map(|p| p.vm.compressed).sum()
    }
}

fn same_process_start(a: Option<StartTime>, b: Option<StartTime>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a == b)
}

fn sample() -> SampleSet {
    let mut procs = HashMap::new();
    let mut denied = 0usize;
    for pid in list_pids() {
        let Some(vm) = task_vm(pid) else {
            denied += 1;
            continue;
        };
        let info = pidinfo::<ProcBsdInfo>(pid, PROC_PIDTBSDINFO);
        let proc = Proc {
            pid,
            ppid: info.as_ref().map_or(0, |i| i.pbi_ppid as c_int),
            uid: info.as_ref().map(|i| i.pbi_uid),
            start: info
                .as_ref()
                .map(|i| (i.pbi_start_tvsec, i.pbi_start_tvusec)),
            name: proc_display_name(pid),
            vm,
        };
        procs.insert(pid, proc);
    }
    SampleSet {
        procs,
        denied,
        at: Instant::now(),
    }
}

/// Processes that appeared or vanished between samples, by PID. A reused PID
/// counts as one exit plus one start; an unknown start time is not reuse.
fn churn<'a>(
    a: &'a SampleSet,
    b: &'a SampleSet,
    filter: &Filter,
) -> (Vec<&'a Proc>, Vec<&'a Proc>) {
    let reused = |x: &Proc, y: &Proc| matches!((x.start, y.start), (Some(s), Some(t)) if s != t);
    let mut started: Vec<&Proc> = b
        .procs
        .values()
        .filter(|p| filter.keeps(p) && a.procs.get(&p.pid).map_or(true, |q| reused(q, p)))
        .collect();
    let mut exited: Vec<&Proc> = a
        .procs
        .values()
        .filter(|p| filter.keeps(p) && b.procs.get(&p.pid).map_or(true, |q| reused(p, q)))
        .collect();
    started.sort_by_key(|p| p.pid);
    exited.sort_by_key(|p| p.pid);
    (started, exited)
}

/// Bytes swapped in between samples, over every process readable in both.
fn interval_swapins(a: &SampleSet, b: &SampleSet) -> Option<u64> {
    let mut total = None;
    for p in b.procs.values() {
        let Some(q) = a.procs.get(&p.pid) else {
            continue;
        };
        if let (true, Some(x), Some(y)) = (
            same_process_start(q.start, p.start),
            q.vm.swapins,
            p.vm.swapins,
        ) {
            total = Some(total.unwrap_or(0) + y.saturating_sub(x));
        }
    }
    total
}

/// `--name` / `--user`: which processes become rows. Never applied to the
/// SWAP~ denominator, which must cover every readable process.
struct Filter {
    name: Option<String>, // lowercase
    uid: Option<u32>,
}

impl Filter {
    fn from_config(config: &Config) -> Result<Filter, String> {
        Ok(Filter {
            name: config.name.as_ref().map(|n| n.to_lowercase()),
            uid: config.user.as_deref().map(resolve_user).transpose()?,
        })
    }

    fn active(&self) -> bool {
        self.name.is_some() || self.uid.is_some()
    }

    fn keeps(&self, p: &Proc) -> bool {
        self.name
            .as_ref()
            .map_or(true, |n| p.name.to_lowercase().contains(n.as_str()))
            && self.uid.map_or(true, |uid| p.uid == Some(uid))
    }
}

// ---- system summary -------------------------------------------------------

/// System-wide memory state for the summary header.
struct SysMem {
    at: Instant,
    total: Option<u64>,
    page: u64,
    vm: Option<(VmStatistics64, MsgTypeNumber)>,
    swap: Option<XswUsage>,
    pressure: Option<i32>,
    level: Option<i32>,
}

impl SysMem {
    fn read() -> SysMem {
        SysMem {
            at: Instant::now(),
            total: sysctl_num(c"hw.memsize"),
            page: sysctl_num(c"hw.pagesize").unwrap_or(4096),
            vm: host_vm(),
            swap: swap_usage().ok(),
            pressure: sysctl_num(c"kern.memorystatus_vm_pressure_level"),
            level: sysctl_num(c"kern.memorystatus_level"),
        }
    }
}

fn pressure_str(level: Option<i32>, available: Option<i32>) -> String {
    let name = match level {
        Some(1) => "normal".to_string(),
        Some(2) => "warn".to_string(),
        Some(4) => "critical".to_string(),
        Some(n) => n.to_string(),
        None => "?".to_string(),
    };
    match available {
        Some(pct) => format!("{name} (memorystatus level {pct}%)"),
        None => name,
    }
}

/// Header lines; with `before`, a line of rates over the time since then.
fn summary(out: &mut String, now: &SysMem, before: Option<&SysMem>) {
    let pages = |n: u64| human(n.saturating_mul(now.page) as f64);
    let total = now
        .total
        .map_or_else(|| "?".to_string(), |b| human(b as f64));
    let Some((vm, count)) = &now.vm else {
        let _ = writeln!(out, "memory {total}: host_statistics64 unavailable");
        return;
    };
    let _ = writeln!(
        out,
        "memory {total}: wired {}  active {}  inactive {}  free {}",
        pages(vm.wire_count.into()),
        pages(vm.active_count.into()),
        pages(vm.inactive_count.into()),
        pages(vm.free_count.into()),
    );
    let used = u64::from(vm.compressor_page_count);
    let held = vm.total_uncompressed_pages_in_compressor;
    let ratio = if used > 0 {
        format!(" ({:.2}x)", held as f64 / used as f64)
    } else {
        String::new()
    };
    let _ = writeln!(
        out,
        "compressor {} holding {}{ratio}  pressure {}",
        pages(used),
        pages(held),
        pressure_str(now.pressure, now.level),
    );
    let mut swap = match now.swap {
        Some(x) => format!(
            "swap {} used of {}{}",
            human(x.xsu_used as f64),
            human(x.xsu_total as f64),
            if x.xsu_encrypted != 0 {
                " (encrypted)"
            } else {
                ""
            },
        ),
        None => "swap ?".to_string(),
    };
    if *count >= HOST_VM_INFO64_REV2_COUNT {
        let _ = write!(swap, ", {} compressor pages in swap", vm.swapped_count);
    }
    let _ = writeln!(
        out,
        "{swap}  since boot: swapped in {}, out {}",
        pages(vm.swapins),
        pages(vm.swapouts),
    );

    if let Some((old, _)) = before.and_then(|b| b.vm.as_ref()) {
        let secs = now
            .at
            .duration_since(before.map_or(now.at, |b| b.at))
            .as_secs_f64()
            .max(1e-9);
        let rate = |a: u64, b: u64| b.saturating_sub(a) as f64 / secs;
        let _ = writeln!(
            out,
            "last {secs:.1}s: swap in {}/s  out {}/s  pages decompressed {:.0}/s  compressed {:.0}/s",
            human(rate(old.swapins, vm.swapins) * now.page as f64),
            human(rate(old.swapouts, vm.swapouts) * now.page as f64),
            rate(old.decompressions, vm.decompressions),
            rate(old.compressions, vm.compressions),
        );
    }
}

// ---- table model ----------------------------------------------------------

/// A metric column. Each mode carries its own set (SNAPSHOT_COLS etc.).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Col {
    /// tree mode: processes in the subtree
    Procs,
    // snapshot
    Cmprs,
    Foot,
    SwapEst,
    Swapins,
    FootPeak,
    CmprsPeak,
    Decomp,
    Headroom,
    // watch / live: change over the interval
    Foot0,
    Foot1,
    Delta,
    Rate,
    SwapinsGrowth,
    DecompGrowth,
    // live: footprint trend over the window
    Grew,
    JetsamEta,
}

// (column, shown without -x), in display order.
const SNAPSHOT_COLS: &[(Col, bool)] = &[
    (Col::Cmprs, true),
    (Col::Foot, true),
    (Col::SwapEst, true),
    (Col::Swapins, true),
    (Col::FootPeak, false),
    (Col::CmprsPeak, false),
    (Col::Decomp, false),
    (Col::Headroom, false),
];
const WATCH_COLS: &[(Col, bool)] = &[
    (Col::Foot0, true),
    (Col::Foot1, true),
    (Col::Delta, true),
    (Col::Rate, true),
    (Col::SwapinsGrowth, true),
    (Col::DecompGrowth, false),
    (Col::Headroom, false),
];
const LIVE_COLS: &[(Col, bool)] = &[
    (Col::Foot0, true),
    (Col::Foot1, true),
    (Col::Delta, true),
    (Col::Rate, true),
    (Col::SwapinsGrowth, true),
    (Col::DecompGrowth, false),
    (Col::Headroom, false),
    (Col::Grew, true),
    (Col::JetsamEta, false),
];

impl Col {
    fn header(self) -> &'static str {
        match self {
            Col::Procs => "PROCS",
            Col::Cmprs => "CMPRS",
            Col::Foot => "FOOTPRINT",
            Col::SwapEst => "SWAP~",
            Col::Swapins | Col::SwapinsGrowth => "SWAPINS",
            Col::FootPeak => "FOOT_PK",
            Col::CmprsPeak => "CMPRS_PK",
            Col::Decomp | Col::DecompGrowth => "DECOMP",
            Col::Headroom => "HEADROOM",
            Col::Foot0 => "FOOT0",
            Col::Foot1 => "FOOT1",
            Col::Delta => "DELTA",
            Col::Rate => "RATE/s",
            Col::Grew => "GREW%",
            Col::JetsamEta => "JETSAM_IN",
        }
    }

    fn csv_name(self) -> &'static str {
        match self {
            Col::Procs => "procs",
            Col::Cmprs => "cmprs",
            Col::Foot => "footprint",
            Col::SwapEst => "swap_est",
            Col::Swapins => "swapins",
            Col::FootPeak => "foot_peak",
            Col::CmprsPeak => "cmprs_peak",
            Col::Decomp => "decompressions",
            Col::Headroom => "headroom",
            Col::Foot0 => "foot0",
            Col::Foot1 => "foot1",
            Col::Delta => "delta",
            Col::Rate => "rate_per_s",
            Col::SwapinsGrowth => "swapins_delta",
            Col::DecompGrowth => "decompressions_delta",
            Col::Grew => "grew_pct",
            Col::JetsamEta => "jetsam_eta_s",
        }
    }

    /// Display text. None is N/A (the kernel lacks the field), except for the
    /// trend columns, where it means "not enough samples" and prints blank.
    fn display(self, v: Option<f64>) -> String {
        match (self, v) {
            (Col::Grew | Col::JetsamEta, None) => String::new(),
            (_, None) => "N/A".to_string(),
            (Col::Headroom, Some(b)) => headroom_str(Some(b as u64)),
            (Col::Procs | Col::Decomp, Some(n)) => format!("{n:.0}"),
            (Col::DecompGrowth, Some(n)) => format!("+{n:.0}"),
            (Col::Delta | Col::Rate | Col::SwapinsGrowth, Some(b)) => human_signed(b),
            (Col::Grew, Some(pct)) => format!("{pct:.0}%"),
            (Col::JetsamEta, Some(secs)) => duration_str(secs),
            (_, Some(b)) => human(b),
        }
    }

    /// Whether tree rollup adds this column up; peaks, limits and trends don't.
    fn sums(self) -> bool {
        !matches!(
            self,
            Col::FootPeak | Col::CmprsPeak | Col::Headroom | Col::Grew | Col::JetsamEta
        )
    }
}

/// Footprint trend of one process in live mode.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Trend {
    /// share of intervals in the window where the footprint grew, in percent
    grew_pct: Option<f64>,
    /// seconds to the jetsam limit at the window's average growth rate
    eta_secs: Option<f64>,
}

/// What a row's cells are computed from.
struct RowSrc<'a> {
    now: &'a Proc,
    /// watch/live: the same process in the previous sample
    before: Option<&'a Proc>,
    elapsed: f64,
    swap_per_cmprs: Option<f64>,
    trend: Option<Trend>,
}

fn cell(col: Col, src: &RowSrc) -> Option<f64> {
    let vm = &src.now.vm;
    let before = src.before.map(|p| &p.vm);
    let foot_delta = |a: &VmStats| vm.footprint as f64 - a.footprint as f64;
    match col {
        Col::Procs => Some(1.0),
        Col::Cmprs => Some(vm.compressed as f64),
        Col::Foot | Col::Foot1 => Some(vm.footprint as f64),
        Col::SwapEst => src.swap_per_cmprs.map(|k| vm.compressed as f64 * k),
        Col::Swapins => vm.swapins.map(|b| b as f64),
        Col::FootPeak => vm.footprint_peak.map(|b| b as f64),
        Col::CmprsPeak => Some(vm.compressed_peak as f64),
        Col::Decomp => vm.decompressions.map(f64::from),
        Col::Headroom => vm.limit_remaining.map(|b| b as f64),
        Col::Foot0 => before.map(|a| a.footprint as f64),
        Col::Delta => before.map(foot_delta),
        Col::Rate => before.map(|a| foot_delta(a) / src.elapsed),
        Col::SwapinsGrowth => {
            before.and_then(|a| Some(vm.swapins?.saturating_sub(a.swapins?) as f64))
        }
        // 32-bit lifetime counter: wrapping_sub survives a wrap.
        Col::DecompGrowth => before.and_then(|a| {
            Some(f64::from(
                vm.decompressions?.wrapping_sub(a.decompressions?),
            ))
        }),
        Col::Grew => src.trend.and_then(|t| t.grew_pct),
        Col::JetsamEta => src.trend.and_then(|t| t.eta_secs),
    }
}

struct Row {
    pid: c_int,
    ppid: c_int,
    name: String,
    /// one value per `Layout::cols`
    vals: Vec<Option<f64>>,
    /// tree indent level
    depth: usize,
    /// tree mode: values cover the whole subtree
    rollup: bool,
}

/// The columns a table carries (`cols`: all of the mode's, as CSV prints) and
/// the ones it shows as text (`visible`: indices into `cols`).
struct Layout {
    cols: Vec<Col>,
    visible: Vec<usize>,
}

impl Layout {
    /// A sort column hidden without -x is shown anyway.
    fn new(config: &Config) -> Layout {
        let mut spec = config.mode.cols().to_vec();
        if config.tree {
            spec.insert(0, (Col::Procs, true));
        }
        let sort_col = match config.sort_by() {
            SortBy::Col(c) => Some(c),
            SortBy::Pid | SortBy::Name => None,
        };
        Layout {
            cols: spec.iter().map(|&(c, _)| c).collect(),
            visible: spec
                .iter()
                .enumerate()
                .filter(|&(_, &(c, basic))| basic || config.extended || Some(c) == sort_col)
                .map(|(i, _)| i)
                .collect(),
        }
    }

    fn index(&self, col: Col) -> Option<usize> {
        self.cols.iter().position(|&c| c == col)
    }

    fn row(&self, src: &RowSrc) -> Row {
        Row {
            pid: src.now.pid,
            ppid: src.now.ppid,
            name: src.now.name.clone(),
            vals: self.cols.iter().map(|&c| cell(c, src)).collect(),
            depth: 0,
            rollup: false,
        }
    }

    /// Text of column `i` for `row`: blank where a rollup can't add up.
    fn text(&self, row: &Row, i: usize) -> Option<String> {
        let col = self.cols[i];
        (!row.rollup || col.sums()).then(|| col.display(row.vals[i]))
    }
}

/// What rows are ordered by: a column (largest first, except HEADROOM:
/// smallest first, as that is closest to a jetsam kill), PID, or name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortBy {
    Col(Col),
    Pid,
    Name,
}

/// --sort keys: (key, snapshot column, watch/live column).
const SORT_KEYS: &[(&str, Option<Col>, Option<Col>)] = &[
    ("cmprs", Some(Col::Cmprs), None),
    ("swap", Some(Col::SwapEst), None),
    ("foot", Some(Col::Foot), Some(Col::Foot1)),
    ("swapins", Some(Col::Swapins), Some(Col::SwapinsGrowth)),
    ("peak", Some(Col::FootPeak), None),
    ("cpeak", Some(Col::CmprsPeak), None),
    ("decomp", Some(Col::Decomp), Some(Col::DecompGrowth)),
    ("headroom", Some(Col::Headroom), Some(Col::Headroom)),
    ("delta", None, Some(Col::Delta)),
    ("grew", None, Some(Col::Grew)),
];

fn parse_sort(key: &str, mode: Mode) -> Result<SortBy, String> {
    match key {
        "pid" => return Ok(SortBy::Pid),
        "name" => return Ok(SortBy::Name),
        _ => {}
    }
    let &(_, snapshot, interval) = SORT_KEYS
        .iter()
        .find(|(k, ..)| *k == key)
        .ok_or_else(|| format!("unknown sort key: {key} (see --help)"))?;
    let col = match mode {
        Mode::Snapshot | Mode::Detail(_) => snapshot,
        Mode::Watch(_) | Mode::Live(_) => interval,
    };
    col.filter(|c| mode.cols().iter().any(|(m, _)| m == c))
        .map(SortBy::Col)
        .ok_or_else(|| format!("--sort {key} doesn't apply in this mode"))
}

/// Missing values last; HEADROOM ascending with 0 (no limit) last; the rest
/// descending.
fn cmp_vals(col: Col, a: Option<f64>, b: Option<f64>) -> Ordering {
    let key = |v: Option<f64>| v.filter(|&x| !(col == Col::Headroom && x == 0.0));
    match (key(a), key(b)) {
        (Some(x), Some(y)) if col == Col::Headroom => x.total_cmp(&y),
        (Some(x), Some(y)) => y.total_cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Row order for `by`; ties fall back to PID.
fn row_order(layout: &Layout, by: SortBy) -> impl Fn(&Row, &Row) -> Ordering {
    let col = match by {
        SortBy::Col(c) => layout.index(c).map(|i| (c, i)),
        SortBy::Pid | SortBy::Name => None,
    };
    move |a, b| {
        let primary = match (by, col) {
            (SortBy::Name, _) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            (SortBy::Col(_), Some((c, i))) => cmp_vals(c, a.vals[i], b.vals[i]),
            _ => Ordering::Equal,
        };
        primary.then(a.pid.cmp(&b.pid))
    }
}

/// Nests rows into process trees, each row rolled up over its subtree
/// (du-style), siblings in `order`. Children of launchd (pid 1) are roots:
/// rolling everything into launchd would bury every app. Keeps the first
/// `max_roots` trees (0 = all) and returns their rows with the tree count.
fn treeify(
    mut rows: Vec<Row>,
    layout: &Layout,
    order: &dyn Fn(&Row, &Row) -> Ordering,
    max_roots: usize,
) -> (Vec<Row>, usize) {
    let index: HashMap<c_int, usize> = rows.iter().enumerate().map(|(i, r)| (r.pid, i)).collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    let mut has_parent = vec![false; rows.len()];
    for (i, r) in rows.iter().enumerate() {
        if r.ppid <= 1 || r.ppid == r.pid {
            continue;
        }
        if let Some(&p) = index.get(&r.ppid) {
            children[p].push(i);
            has_parent[i] = true;
        }
    }

    let sums: Vec<bool> = layout.cols.iter().map(|c| c.sums()).collect();
    let mut roots: Vec<usize> = (0..rows.len()).filter(|&i| !has_parent[i]).collect();
    let mut visited = vec![false; rows.len()];
    for &r in &roots {
        rollup(r, &mut rows, &children, &sums, &mut visited);
    }
    // A PID-reuse race can leave a parent cycle no root reaches.
    for i in 0..rows.len() {
        if !visited[i] {
            roots.push(i);
            rollup(i, &mut rows, &children, &sums, &mut visited);
        }
    }

    for list in children.iter_mut() {
        list.sort_by(|&a, &b| order(&rows[a], &rows[b]));
    }
    roots.sort_by(|&a, &b| order(&rows[a], &rows[b]));
    let kept = if max_roots == 0 {
        roots.len()
    } else {
        max_roots.min(roots.len())
    };

    let mut slots: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
    let mut out = Vec::new();
    for &r in roots.iter().take(kept) {
        emit(r, 0, &children, &mut slots, &mut out);
    }
    (out, kept)
}

/// Adds each child's rolled-up values into row `i` (post-order). Columns that
/// don't add up are cleared on rows that have children.
fn rollup(
    i: usize,
    rows: &mut [Row],
    children: &[Vec<usize>],
    sums: &[bool],
    visited: &mut [bool],
) {
    visited[i] = true;
    for &c in &children[i] {
        if visited[c] {
            continue;
        }
        rollup(c, rows, children, sums, visited);
        for (k, &adds) in sums.iter().enumerate() {
            if adds {
                let v = rows[c].vals[k];
                rows[i].vals[k] = sum_opt(rows[i].vals[k], v);
            }
        }
    }
    if !children[i].is_empty() {
        rows[i].rollup = true;
        for (k, &adds) in sums.iter().enumerate() {
            if !adds {
                rows[i].vals[k] = None;
            }
        }
    }
}

fn emit(
    i: usize,
    depth: usize,
    children: &[Vec<usize>],
    slots: &mut [Option<Row>],
    out: &mut Vec<Row>,
) {
    let Some(mut row) = slots[i].take() else {
        return;
    };
    row.depth = depth;
    out.push(row);
    for &c in &children[i] {
        emit(c, depth + 1, children, slots, out);
    }
}

/// Sorts rows (tree mode: nests and rolls them up) and applies N. Returns the
/// rows to show and, in tree mode, how many trees they form.
fn arrange(mut rows: Vec<Row>, layout: &Layout, config: &Config) -> (Vec<Row>, Option<usize>) {
    let order = row_order(layout, config.sort_by());
    if config.tree {
        let (rows, trees) = treeify(rows, layout, &order, config.topn);
        return (rows, Some(trees));
    }
    rows.sort_by(&order);
    if config.topn > 0 {
        rows.truncate(config.topn);
    }
    (rows, None)
}

/// Sum of `col` over top-level rows (tree mode: over the shown trees).
fn total(layout: &Layout, rows: &[Row], col: Col) -> Option<f64> {
    let i = layout.index(col)?;
    let mut top = rows.iter().filter(|r| r.depth == 0).peekable();
    if top.peek().is_none() {
        return Some(0.0);
    }
    top.map(|r| r.vals[i]).fold(None, sum_opt)
}

/// Parent names for -p: from the sample when the parent is readable, else
/// looked up by PID (cached).
struct ParentNames<'a> {
    sample: &'a SampleSet,
    cache: HashMap<c_int, String>,
}

impl<'a> ParentNames<'a> {
    fn new(sample: &'a SampleSet) -> Self {
        ParentNames {
            sample,
            cache: HashMap::new(),
        }
    }

    fn get(&mut self, ppid: c_int) -> String {
        if let Some(p) = self.sample.procs.get(&ppid) {
            return p.name.clone();
        }
        self.cache
            .entry(ppid)
            .or_insert_with(|| proc_display_name(ppid))
            .clone()
    }
}

/// Text table; with `color`, rows that grew every interval are yellow.
fn render_table(
    out: &mut String,
    layout: &Layout,
    rows: &[Row],
    parents: &mut ParentNames,
    show_parent: bool,
    color: bool,
) {
    let headers: Vec<&str> = layout
        .visible
        .iter()
        .map(|&i| layout.cols[i].header())
        .collect();
    let _ = writeln!(
        out,
        "{}{}",
        ident_cols("PID", "NAME", show_parent.then_some(("PPID", "PARENT"))),
        metric_cols(&headers)
    );
    let grew = layout.index(Col::Grew);
    for r in rows {
        let name = format!("{}{}", "  ".repeat(r.depth), r.name);
        let pid = r.pid.to_string();
        let ident = if show_parent {
            let pname = parents.get(r.ppid);
            ident_cols(&pid, &name, Some((&r.ppid.to_string(), &pname)))
        } else {
            ident_cols(&pid, &name, None)
        };
        let cells: Vec<String> = layout
            .visible
            .iter()
            .map(|&i| layout.text(r, i).unwrap_or_default())
            .collect();
        let line = format!("{ident}{}", metric_cols(&cells));
        let line = line.trim_end(); // blank trend cells at the end
        let sustained = grew.is_some_and(|g| matches!(r.vals[g], Some(p) if p >= 100.0));
        if color && sustained {
            let _ = writeln!(out, "\x1b[33m{line}\x1b[0m");
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
}

/// CSV: every column of the mode, whole numbers, empty where N/A.
fn render_csv(out: &mut String, layout: &Layout, rows: &[Row], tree: bool) {
    let mut head = vec!["pid", "ppid", "name"];
    if tree {
        head.push("depth");
    }
    head.extend(layout.cols.iter().map(|c| c.csv_name()));
    let _ = writeln!(out, "{}", head.join(","));
    for r in rows {
        let mut fields = vec![r.pid.to_string(), r.ppid.to_string(), csv_field(&r.name)];
        if tree {
            fields.push(r.depth.to_string());
        }
        for (i, &col) in layout.cols.iter().enumerate() {
            let shown = !r.rollup || col.sums();
            fields.push(
                r.vals[i]
                    .filter(|_| shown)
                    .map_or_else(String::new, csv_num),
            );
        }
        let _ = writeln!(out, "{}", fields.join(","));
    }
}

fn color_enabled() -> bool {
    let tty = unsafe { isatty(1) } == 1;
    tty && std::env::var_os("NO_COLOR").map_or(true, |v| v.is_empty())
}

/// "started (2): foo 123, bar 456" and the like, at most 8 names each.
fn churn_lines(out: &mut String, a: &SampleSet, b: &SampleSet, filter: &Filter, color: bool) {
    const MAX: usize = 8;
    let (started, exited) = churn(a, b, filter);
    if started.is_empty() && exited.is_empty() {
        let _ = writeln!(out, "no process started or exited");
        return;
    }
    for (label, procs, ansi) in [
        ("started", &started, "\x1b[32m"),
        ("exited", &exited, "\x1b[31m"),
    ] {
        if procs.is_empty() {
            continue;
        }
        let mut names: Vec<String> = procs
            .iter()
            .take(MAX)
            .map(|p| format!("{} {}", p.name, p.pid))
            .collect();
        if procs.len() > MAX {
            names.push(format!("… +{} more", procs.len() - MAX));
        }
        let line = format!("{label} ({}): {}", procs.len(), names.join(", "));
        if color {
            let _ = writeln!(out, "{ansi}{line}\x1b[0m");
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
}

// ---- snapshot mode --------------------------------------------------------

fn run_snapshot(config: &Config, filter: &Filter) {
    let swap_total = match swap_usage() {
        Ok(x) => Some(x.xsu_used),
        Err(err) => {
            eprintln!("WARNING: could not read sysctl vm.swapusage: {err}; SWAP~ unavailable.");
            None
        }
    };
    let s = sample();

    // SWAP~ estimates are proportional splits of system swap across readable
    // processes, shown as N/A if the system swap total is unavailable or the
    // denominator is 0. When denied > 0 the denominator is incomplete and one
    // visible process may absorb all system swap; warn about that below.
    let swap_per_cmprs = swap_ratio(swap_total, s.total_compressed());

    let layout = Layout::new(config);
    let rows: Vec<Row> = s
        .procs
        .values()
        .filter(|p| filter.keeps(p))
        .map(|p| {
            layout.row(&RowSrc {
                now: p,
                before: None,
                elapsed: 0.0,
                swap_per_cmprs,
                trend: None,
            })
        })
        .collect();
    let matched = rows.len();
    let (rows, trees) = arrange(rows, &layout, config);

    let mut out = String::new();
    if config.csv {
        render_csv(&mut out, &layout, &rows, config.tree);
    } else {
        summary(&mut out, &SysMem::read(), None);
        out.push('\n');
        render_table(
            &mut out,
            &layout,
            &rows,
            &mut ParentNames::new(&s),
            config.show_parent,
            color_enabled(),
        );

        let what = if filter.active() {
            format!(
                "{}/{matched} matching procs, of {} readable",
                rows.len(),
                s.procs.len()
            )
        } else {
            format!("{}/{matched} procs", rows.len())
        };
        let trees = trees.map_or_else(String::new, |t| format!(" in {t} trees"));
        let sudo_hint = if s.denied > 0 {
            " — need sudo for other users"
        } else {
            ""
        };
        let shown_total = |col: Col| col.display(total(&layout, &rows, col));
        let _ = writeln!(out);
        let _ = writeln!(out, "shown {what}{trees}  ({} denied{sudo_hint})", s.denied);
        let _ = writeln!(
            out,
            "shown totals: CMPRS {}  FOOTPRINT {}  SWAP~ {}  SWAPINS {}",
            shown_total(Col::Cmprs),
            shown_total(Col::Foot),
            shown_total(Col::SwapEst),
            shown_total(Col::Swapins),
        );
        if swap_total.is_some() && swap_per_cmprs.is_none() {
            let _ = writeln!(
                out,
                "SWAP~ N/A: denominator is 0 — run as root for accurate estimates"
            );
        }
    }
    print!("{out}");

    if s.denied > 0 && swap_per_cmprs.is_some() {
        eprintln!(
            "WARNING: {} proc(s) denied — SWAP~ estimates cover only visible processes; \
             a single visible process may absorb all system swap. Run as root for accuracy.",
            s.denied
        );
    }
}

// ---- watch / leak mode ----------------------------------------------------

/// Rows for processes present in both samples with the same start time.
fn interval_rows(
    layout: &Layout,
    filter: &Filter,
    a: &SampleSet,
    b: &SampleSet,
    hist: Option<&History>,
) -> Vec<Row> {
    let elapsed = b.at.duration_since(a.at).as_secs_f64().max(1e-9);
    b.procs
        .values()
        .filter(|p| filter.keeps(p))
        .filter_map(|p| {
            let q = a
                .procs
                .get(&p.pid)
                .filter(|q| same_process_start(q.start, p.start))?;
            let trend = hist
                .and_then(|h| h.get(&(p.pid, p.start?)))
                .map(|h| trend(h, p.vm.limit_remaining));
            Some(layout.row(&RowSrc {
                now: p,
                before: Some(q),
                elapsed,
                swap_per_cmprs: None,
                trend,
            }))
        })
        .collect()
}

fn run_watch(config: &Config, filter: &Filter, interval: u64) {
    let sys0 = SysMem::read();
    let a = sample();
    std::thread::sleep(Duration::from_secs(interval.max(1)));
    let b = sample();
    let sys1 = SysMem::read();

    let layout = Layout::new(config);
    let (rows, _) = arrange(
        interval_rows(&layout, filter, &a, &b, None),
        &layout,
        config,
    );

    let mut out = String::new();
    if config.csv {
        render_csv(&mut out, &layout, &rows, config.tree);
        print!("{out}");
        return;
    }
    let color = color_enabled();
    summary(&mut out, &sys1, Some(&sys0));
    out.push('\n');
    render_table(
        &mut out,
        &layout,
        &rows,
        &mut ParentNames::new(&b),
        config.show_parent,
        color,
    );
    let elapsed = b.at.duration_since(a.at).as_secs_f64();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "interval {:.1}s — one interval = allocation churn; trust SUSTAINED growth as leak signal (--live tracks it).",
        elapsed
    );
    let _ = writeln!(
        out,
        "swapped in during interval (all procs sampled twice): {}",
        opt_bytes(interval_swapins(&a, &b))
    );
    churn_lines(&mut out, &a, &b, filter, color);
    let _ = writeln!(
        out,
        "sampled procs: first {}/{} readable ({} denied), second {}/{} readable ({} denied)",
        a.procs.len(),
        a.total(),
        a.denied,
        b.procs.len(),
        b.total(),
        b.denied,
    );
    print!("{out}");
}

// ---- live mode ------------------------------------------------------------

/// Live-mode footprint history window, in intervals.
const TREND_WINDOW: usize = 10;
/// Intervals needed before GREW% and JETSAM_IN are shown.
const TREND_MIN: usize = 3;

/// (seconds since start, footprint) per process, oldest first, keyed by PID
/// and start time so a reused PID starts a fresh history.
type History = HashMap<(c_int, StartTime), VecDeque<(f64, u64)>>;

fn record(hist: &mut History, s: &SampleSet, t: f64) {
    hist.retain(|(pid, start), _| s.procs.get(pid).is_some_and(|p| p.start == Some(*start)));
    for p in s.procs.values() {
        if let Some(start) = p.start {
            let h = hist.entry((p.pid, start)).or_default();
            h.push_back((t, p.vm.footprint));
            if h.len() > TREND_WINDOW + 1 {
                h.pop_front();
            }
        }
    }
}

fn trend(hist: &VecDeque<(f64, u64)>, headroom: Option<u64>) -> Trend {
    let intervals = hist.len().saturating_sub(1);
    if intervals < TREND_MIN {
        return Trend {
            grew_pct: None,
            eta_secs: None,
        };
    }
    let grew = hist
        .iter()
        .zip(hist.iter().skip(1))
        .filter(|(a, b)| b.1 > a.1)
        .count();
    let (first, last) = (hist[0], hist[intervals]);
    let rate = (last.1 as f64 - first.1 as f64) / (last.0 - first.0).max(1e-9);
    Trend {
        grew_pct: Some(100.0 * grew as f64 / intervals as f64),
        eta_secs: headroom
            .filter(|&h| h > 0 && rate > 0.0)
            .map(|h| h as f64 / rate),
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn request_stop(_sig: c_int) {
    STOP.store(true, AtomicOrdering::SeqCst);
}

fn emit_raw(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// Live mode on a terminal: alternate screen, no cursor, no line wrap (long
/// rows are clipped), and keys read unbuffered without echo when stdin is a
/// terminal too. Dropping it restores all of that.
struct Screen {
    saved: Option<Termios>,
}

impl Screen {
    fn enter(keys: bool) -> Screen {
        let mut saved = None;
        let mut t = Termios::default();
        if keys && unsafe { tcgetattr(0, &mut t) } == 0 {
            let mut raw = t;
            raw.c_lflag &= !(ICANON | ECHO);
            raw.c_cc[VMIN] = 0;
            raw.c_cc[VTIME] = 0;
            if unsafe { tcsetattr(0, TCSANOW, &raw) } == 0 {
                saved = Some(t);
            }
        }
        emit_raw("\x1b[?1049h\x1b[?25l\x1b[?7l");
        Screen { saved }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if let Some(t) = &self.saved {
            unsafe { tcsetattr(0, TCSANOW, t) };
        }
        emit_raw("\x1b[?7h\x1b[?25h\x1b[?1049l");
    }
}

fn term_rows() -> Option<usize> {
    let mut ws = WinSize::default();
    let rc = unsafe { ioctl(1, TIOCGWINSZ, &mut ws as *mut WinSize) };
    (rc == 0 && ws.ws_row > 0).then_some(ws.ws_row as usize)
}

/// The next key pressed within `timeout_ms`, if any.
fn wait_key(timeout_ms: c_int) -> Option<u8> {
    let mut fd = PollFd {
        fd: 0,
        events: POLLIN,
        revents: 0,
    };
    if unsafe { poll(&mut fd, 1, timeout_ms) } <= 0 || fd.revents & POLLIN == 0 {
        return None;
    }
    let mut key = 0u8;
    let n = unsafe { read(0, &mut key as *mut u8 as *mut c_void, 1) };
    (n == 1).then_some(key)
}

/// Draws a frame: in place (clipped to the window) on a terminal, else appended.
fn show(status: &str, frame: &str, tty: bool) {
    let mut s = String::new();
    if tty {
        let height = term_rows().unwrap_or(usize::MAX);
        s.push_str("\x1b[H");
        for line in status
            .lines()
            .chain(frame.lines())
            .take(height.saturating_sub(1))
        {
            s.push_str(line);
            s.push_str("\x1b[K\n");
        }
        s.push_str("\x1b[J");
    } else {
        let _ = write!(s, "{status}\n{frame}\n");
    }
    emit_raw(&s);
}

fn status_line(interval: u64, tick: u64, paused: bool, keys: bool) -> String {
    let state = if paused { "PAUSED" } else { "live" };
    let help = match (keys, paused) {
        (true, false) => "space: pause · q: quit",
        (true, true) => "space: resume · q: quit",
        (false, _) => "Ctrl-C: quit",
    };
    format!("macswapi {state} · every {interval}s · refresh #{tick} · {help}")
}

fn live_frame(
    config: &Config,
    filter: &Filter,
    sys: (&SysMem, &SysMem),
    a: &SampleSet,
    b: &SampleSet,
    hist: &History,
) -> String {
    let color = color_enabled();
    let layout = Layout::new(config);
    let (rows, _) = arrange(
        interval_rows(&layout, filter, a, b, Some(hist)),
        &layout,
        config,
    );
    let mut out = String::new();
    summary(&mut out, sys.1, Some(sys.0));
    out.push('\n');
    render_table(
        &mut out,
        &layout,
        &rows,
        &mut ParentNames::new(b),
        config.show_parent,
        color,
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "swapped in during interval: {}",
        opt_bytes(interval_swapins(a, b))
    );
    churn_lines(&mut out, a, b, filter, color);
    let _ = writeln!(
        out,
        "GREW%: intervals with FOOTPRINT growth, of the last {TREND_WINDOW} (shown after {TREND_MIN}); 100% = sustained"
    );
    out
}

fn run_live(config: &Config, filter: &Filter, interval: u64) {
    for sig in [SIGHUP, SIGINT, SIGTERM] {
        unsafe { signal(sig, request_stop as extern "C" fn(c_int) as usize) };
    }
    let tty = unsafe { isatty(1) } == 1;
    // Raw keys only when output is a terminal too: if a pipe closed under us,
    // SIGPIPE would kill the process with the terminal still raw.
    let keys = tty && unsafe { isatty(0) } == 1;
    let screen = tty.then(|| Screen::enter(keys));

    let t0 = Instant::now();
    let mut sys = SysMem::read();
    let mut prev = sample();
    let mut hist = History::new();
    record(&mut hist, &prev, 0.0);
    let mut frame = format!("waiting {interval}s for the first interval…\n");
    let mut paused = false;
    let mut tick = 0u64;
    show(&status_line(interval, tick, paused, keys), &frame, tty);

    'run: loop {
        let deadline = Instant::now() + Duration::from_secs(interval);
        while paused || Instant::now() < deadline {
            if STOP.load(AtomicOrdering::SeqCst) {
                break 'run;
            }
            let key = if keys {
                wait_key(100)
            } else {
                std::thread::sleep(Duration::from_millis(100));
                None
            };
            match key {
                Some(b'q' | b'Q') => break 'run,
                Some(b' ') => {
                    paused = !paused;
                    show(&status_line(interval, tick, paused, keys), &frame, tty);
                }
                _ => {}
            }
        }
        let sys_now = SysMem::read();
        let cur = sample();
        tick += 1;
        record(&mut hist, &cur, t0.elapsed().as_secs_f64());
        frame = live_frame(config, filter, (&sys, &sys_now), &prev, &cur, &hist);
        show(&status_line(interval, tick, paused, keys), &frame, tty);
        sys = sys_now;
        prev = cur;
    }

    if tty {
        drop(screen);
        // Leave the last frame on the normal screen.
        print!("{frame}");
    }
}

// ---- single-process detail ------------------------------------------------

fn kv(out: &mut String, label: &str, value: impl std::fmt::Display) {
    let _ = writeln!(out, "  {label:<30} {value}");
}

fn kv2(out: &mut String, label: &str, a: &str, b: &str) {
    let line = format!("  {label:<30} {a:>12} {b:>12}");
    let _ = writeln!(out, "{}", line.trim_end());
}

/// A section title, optionally with titles over kv2's two value columns.
fn section(out: &mut String, title: &str, cols: Option<(&str, &str)>) {
    let _ = match cols {
        Some((a, b)) => writeln!(out, "\n{title:<32} {a:>12} {b:>12}"),
        None => writeln!(out, "\n{title}"),
    };
}

fn run_detail(pid: c_int) {
    let Some((info, count)) = task_vm_raw(pid) else {
        eprintln!("pid {pid}: cannot read it (no such process, or another user's — try sudo)");
        std::process::exit(1);
    };
    let vm = VmStats::from_info(&info, count);
    let b = |v: u64| human(v as f64);
    let mut out = String::new();

    let _ = writeln!(out, "PID {pid}  {}", proc_display_name(pid));
    kv(
        &mut out,
        "path",
        proc_path(pid).unwrap_or_else(|| "?".to_string()),
    );
    if let Some(bsd) = pidinfo::<ProcBsdInfo>(pid, PROC_PIDTBSDINFO) {
        let ppid = bsd.pbi_ppid as c_int;
        kv(
            &mut out,
            "parent",
            format!("{ppid} ({})", proc_display_name(ppid)),
        );
        let user = user_name(bsd.pbi_uid).unwrap_or_else(|| "?".to_string());
        kv(&mut out, "user", format!("{user} ({})", bsd.pbi_uid));
        if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
            let running = now.as_secs_f64() - bsd.pbi_start_tvsec as f64;
            kv(&mut out, "running for", duration_str(running));
        }
    }

    section(
        &mut out,
        "memory (task_info TASK_VM_INFO)",
        Some(("now", "peak")),
    );
    kv2(
        &mut out,
        "footprint",
        &b(vm.footprint),
        &opt_bytes(vm.footprint_peak),
    );
    kv2(
        &mut out,
        "compressed",
        &b(vm.compressed),
        &b(vm.compressed_peak),
    );
    kv2(
        &mut out,
        "resident",
        &b(info.resident_size),
        &b(info.resident_size_peak),
    );
    kv2(
        &mut out,
        "internal (anonymous)",
        &b(info.internal),
        &b(info.internal_peak),
    );
    kv2(
        &mut out,
        "external (file-backed)",
        &b(info.external),
        &b(info.external_peak),
    );
    kv2(
        &mut out,
        "reusable",
        &b(info.reusable),
        &b(info.reusable_peak),
    );
    kv2(&mut out, "device", &b(info.device), &b(info.device_peak));
    kv2(&mut out, "virtual", &b(info.virtual_size), "");
    kv2(
        &mut out,
        "compressed over lifetime",
        &b(info.compressed_lifetime),
        "",
    );
    kv2(
        &mut out,
        "purgeable volatile pmap",
        &b(info.purgeable_volatile_pmap),
        "",
    );
    kv2(
        &mut out,
        "purgeable volatile resident",
        &b(info.purgeable_volatile_resident),
        "",
    );
    kv2(
        &mut out,
        "purgeable volatile virtual",
        &b(info.purgeable_volatile_virtual),
        "",
    );
    kv2(&mut out, "regions", &{ info.region_count }.to_string(), "");

    // SWAP~ needs the denominator over every readable process.
    let (mut all_compressed, mut readable, mut denied) = (0u64, 0usize, 0usize);
    for p in list_pids() {
        match task_vm(p) {
            Some(v) => {
                all_compressed += v.compressed;
                readable += 1;
            }
            None => denied += 1,
        }
    }
    let swap_used = swap_usage().ok().map(|x| x.xsu_used);
    let estimate = swap_ratio(swap_used, all_compressed)
        .map_or_else(|| "N/A".to_string(), |k| human(vm.compressed as f64 * k));
    let mut note = format!(
        "share of {} system swap over {readable} readable procs",
        opt_bytes(swap_used)
    );
    if denied > 0 {
        let _ = write!(note, "; {denied} denied, run as root for all");
    }
    section(&mut out, "swap & compressor", None);
    kv(
        &mut out,
        "SWAP~ (estimate)",
        format!("{estimate}  ({note})"),
    );
    kv(&mut out, "swapped in (lifetime)", opt_bytes(vm.swapins));
    kv(
        &mut out,
        "pages decompressed (lifetime)",
        vm.decompressions
            .map_or_else(|| "N/A".to_string(), |n| n.to_string()),
    );
    let headroom = headroom_str(vm.limit_remaining);
    let note = if vm.limit_remaining == Some(0) {
        "  (no jetsam limit, or at/over a soft one)"
    } else {
        ""
    };
    kv(&mut out, "jetsam headroom", format!("{headroom}{note}"));

    if count >= TASK_VM_INFO_REV3_COUNT {
        // Each pair is two separate ledgers, not a total and its compressed part.
        let ledgers = info.purgeable_and_tag_ledgers;
        section(
            &mut out,
            "ledgers (task_vm_info rev3)",
            Some(("bytes", "compressed")),
        );
        for (i, name) in LEDGER_NAMES.iter().enumerate() {
            let (plain, compressed) = (ledgers[2 * i], ledgers[2 * i + 1]);
            kv2(
                &mut out,
                name,
                &human(plain as f64),
                &human(compressed as f64),
            );
        }
    }

    section(
        &mut out,
        "faults & threads (proc_pidinfo PROC_PIDTASKINFO)",
        None,
    );
    match pidinfo::<ProcTaskInfo>(pid, PROC_PIDTASKINFO) {
        Some(t) => {
            kv(&mut out, "page faults", t.pti_faults as u32);
            kv(&mut out, "pageins", t.pti_pageins as u32);
            kv(&mut out, "copy-on-write faults", t.pti_cow_faults as u32);
            kv(&mut out, "threads", t.pti_threadnum);
        }
        None => kv(&mut out, "unavailable", "another user's process needs sudo"),
    }

    section(&mut out, "rusage (proc_pid_rusage v2)", None);
    match rusage(pid) {
        Some(r) => {
            kv(&mut out, "wired", b(r.ri_wired_size));
            kv(&mut out, "pageins", r.ri_pageins);
            kv(&mut out, "disk read", b(r.ri_diskio_bytesread));
            kv(&mut out, "disk written", b(r.ri_diskio_byteswritten));
        }
        None => kv(&mut out, "unavailable", "another user's process needs sudo"),
    }
    print!("{out}");
}

// ---- CLI ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Snapshot,
    Watch(u64),
    Live(u64),
    Detail(c_int),
}

impl Mode {
    fn cols(self) -> &'static [(Col, bool)] {
        match self {
            Mode::Snapshot | Mode::Detail(_) => SNAPSHOT_COLS,
            Mode::Watch(_) => WATCH_COLS,
            Mode::Live(_) => LIVE_COLS,
        }
    }
}

#[derive(Debug, PartialEq)]
struct Config {
    topn: usize,
    show_parent: bool,
    extended: bool,
    tree: bool,
    csv: bool,
    sort: Option<SortBy>,
    name: Option<String>,
    user: Option<String>,
    mode: Mode,
    help: bool,
}

impl Config {
    fn sort_by(&self) -> SortBy {
        self.sort.unwrap_or(match self.mode {
            Mode::Watch(_) | Mode::Live(_) => SortBy::Col(Col::Delta),
            Mode::Snapshot | Mode::Detail(_) => SortBy::Col(Col::Cmprs),
        })
    }
}

const USAGE: &str = "\
macswapi [N] [options] [-w|--watch|--delta [SECS] | -l|--live [SECS] | --pid PID]
  N              top N rows (default 20, 0=all); with --tree, top N trees
  -p --parent    add PPID + parent-name columns
  -x --extended  add FOOT_PK, CMPRS_PK, DECOMP, HEADROOM
                 (watch: DECOMP, HEADROOM; live: also JETSAM_IN)
  -t --tree      process trees; each row rolled up over its subtree
     --sort KEY  cmprs swap foot swapins peak cpeak decomp headroom pid name
                 (watch/live: delta foot swapins decomp headroom pid name,
                 live also grew; default cmprs, watch/live delta)
     --name STR  only processes whose name contains STR (any case)
     --user USER only processes of USER (login name or uid)
     --csv       every column of the mode as CSV, raw numbers
  -w --watch     single-interval leak check (default 3s)
     --delta     clearer alias for --watch
  -l --live      refresh every SECS (default 2s); space pauses, q quits
     --pid PID   everything macswapi can read about one process
  -h --help      this help
  SECS must be positive and immediately follow its flag; N must appear
  before the interval flag. example: macswapi 50 --delta 5
";

fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut config = Config {
        topn: 20,
        show_parent: false,
        extended: false,
        tree: false,
        csv: false,
        sort: None,
        name: None,
        user: None,
        mode: Mode::Snapshot,
        help: false,
    };
    let mut interval: Option<(bool, u64)> = None; // (live, secs)
    let mut pid: Option<c_int> = None;
    let mut sort: Option<String> = None;

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
            "-t" | "--tree" => config.tree = true,
            "--csv" => config.csv = true,
            "--sort" | "--name" | "--user" | "--pid" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| format!("{arg} needs a value"))?
                    .clone();
                i += 1;
                match arg.as_str() {
                    "--sort" => sort = Some(value),
                    "--name" => config.name = Some(value),
                    "--user" => config.user = Some(value),
                    _ => {
                        let p = value.parse::<c_int>().ok().filter(|&p| p > 0);
                        pid =
                            Some(p.ok_or_else(|| {
                                format!("--pid needs a positive PID, got {value}")
                            })?);
                    }
                }
            }
            "-w" | "--watch" | "--delta" | "-l" | "--live" => {
                if interval.is_some() {
                    return Err("only one of --watch/--delta/--live".to_string());
                }
                let live = matches!(arg.as_str(), "-l" | "--live");
                let mut secs = if live { 2 } else { 3 };
                if let Some(Ok(s)) = args.get(i + 1).map(|a| a.parse::<u64>()) {
                    if s == 0 {
                        return Err("interval must be positive".to_string());
                    }
                    secs = s;
                    i += 1;
                }
                interval = Some((live, secs));
            }
            other => {
                if let Ok(topn) = other.parse::<usize>() {
                    if interval.is_some() {
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

    config.mode = match (pid, interval) {
        (Some(_), Some(_)) => return Err("--pid can't be combined with --watch/--live".to_string()),
        (Some(p), None) => Mode::Detail(p),
        (None, Some((true, secs))) => Mode::Live(secs),
        (None, Some((false, secs))) => Mode::Watch(secs),
        (None, None) => Mode::Snapshot,
    };
    if matches!(config.mode, Mode::Detail(_))
        && (config.tree
            || config.csv
            || sort.is_some()
            || config.name.is_some()
            || config.user.is_some())
    {
        return Err(
            "--pid shows one process: no --tree, --csv, --sort, --name or --user".to_string(),
        );
    }
    if config.csv && matches!(config.mode, Mode::Live(_)) {
        return Err("--csv can't be combined with --live".to_string());
    }
    config.sort = sort.map(|key| parse_sort(&key, config.mode)).transpose()?;
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

    let filter = match Filter::from_config(&config) {
        Ok(filter) => filter,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };

    match config.mode {
        Mode::Snapshot => run_snapshot(&config, &filter),
        Mode::Watch(secs) => run_watch(&config, &filter, secs),
        Mode::Live(secs) => run_live(&config, &filter, secs),
        Mode::Detail(pid) => run_detail(pid),
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
                tree: false,
                csv: false,
                sort: None,
                name: None,
                user: None,
                mode: Mode::Watch(5),
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
        assert!(parse(&["--live", "0"]).is_err());
    }

    #[test]
    fn test_parse_args_extended() {
        let config = parse(&["10", "-x", "--watch", "2"]).unwrap();
        assert!(config.extended);
        assert_eq!((config.topn, config.mode), (10, Mode::Watch(2)));
        assert!(parse(&["--extended"]).unwrap().extended);
    }

    #[test]
    fn test_parse_args_modes() {
        assert_eq!(parse(&[]).unwrap().mode, Mode::Snapshot);
        assert_eq!(parse(&["--live"]).unwrap().mode, Mode::Live(2));
        assert_eq!(parse(&["5", "-l", "7"]).unwrap().mode, Mode::Live(7));
        assert_eq!(parse(&["--watch"]).unwrap().mode, Mode::Watch(3));
        assert_eq!(parse(&["--pid", "42"]).unwrap().mode, Mode::Detail(42));
        assert!(parse(&["--watch", "--live"]).is_err());
        assert!(parse(&["--pid", "0"]).is_err());
        assert!(parse(&["--pid", "abc"]).is_err());
        assert!(parse(&["--pid"]).is_err());
    }

    #[test]
    fn test_parse_args_filters_and_options() {
        let c = parse(&[
            "-t", "--csv", "--sort", "swapins", "--name", "Chrome", "--user", "501",
        ])
        .unwrap();
        assert!(c.tree && c.csv);
        assert_eq!(c.sort, Some(SortBy::Col(Col::Swapins)));
        assert_eq!(c.name.as_deref(), Some("Chrome"));
        assert_eq!(c.user.as_deref(), Some("501"));
        assert!(parse(&["--name"]).is_err());
    }

    #[test]
    fn test_parse_args_rejects_conflicts() {
        assert!(parse(&["--pid", "1", "--watch"]).is_err());
        assert!(parse(&["--pid", "1", "--tree"]).is_err());
        assert!(parse(&["--pid", "1", "--name", "x"]).is_err());
        assert!(parse(&["--csv", "--live"]).is_err());
        assert!(parse(&["--sort", "delta"]).is_err()); // watch/live only
        assert!(parse(&["--sort", "bogus"]).is_err());
    }

    #[test]
    fn test_parse_sort_per_mode() {
        let col = |key, mode| parse_sort(key, mode);
        assert_eq!(
            col("swapins", Mode::Snapshot),
            Ok(SortBy::Col(Col::Swapins))
        );
        assert_eq!(
            col("swapins", Mode::Watch(3)),
            Ok(SortBy::Col(Col::SwapinsGrowth))
        );
        assert_eq!(col("foot", Mode::Live(2)), Ok(SortBy::Col(Col::Foot1)));
        assert_eq!(col("grew", Mode::Live(2)), Ok(SortBy::Col(Col::Grew)));
        assert!(col("grew", Mode::Watch(3)).is_err());
        assert!(col("peak", Mode::Watch(3)).is_err());
        assert_eq!(col("name", Mode::Watch(3)), Ok(SortBy::Name));
        assert_eq!(col("pid", Mode::Snapshot), Ok(SortBy::Pid));
    }

    #[test]
    fn test_layout_shows_hidden_sort_column() {
        let plain = Layout::new(&parse(&[]).unwrap());
        assert_eq!(plain.visible.len(), 4);
        let sorted = Layout::new(&parse(&["--sort", "headroom"]).unwrap());
        let shown: Vec<Col> = sorted.visible.iter().map(|&i| sorted.cols[i]).collect();
        assert_eq!(shown.last(), Some(&Col::Headroom));
        let tree = Layout::new(&parse(&["-t", "-x"]).unwrap());
        assert_eq!(tree.cols[0], Col::Procs);
        assert_eq!(tree.visible.len(), tree.cols.len());
    }

    #[test]
    fn test_cmp_vals() {
        let mut v = vec![Some(0.0), Some(5.0), None, Some(1.0)];
        v.sort_by(|&a, &b| cmp_vals(Col::Headroom, a, b));
        assert_eq!(&v[..2], &[Some(1.0), Some(5.0)]); // closest to the limit first
        v.sort_by(|&a, &b| cmp_vals(Col::Cmprs, a, b));
        assert_eq!(v, vec![Some(5.0), Some(1.0), Some(0.0), None]);
    }

    fn row(pid: c_int, ppid: c_int, vals: &[Option<f64>]) -> Row {
        Row {
            pid,
            ppid,
            name: format!("p{pid}"),
            vals: vals.to_vec(),
            depth: 0,
            rollup: false,
        }
    }

    #[test]
    fn test_treeify_rolls_up_and_orders() {
        let layout = Layout {
            cols: vec![Col::Procs, Col::Cmprs, Col::FootPeak],
            visible: vec![0, 1, 2],
        };
        let p = |pid, ppid, cmprs: f64| row(pid, ppid, &[Some(1.0), Some(cmprs), Some(cmprs)]);
        let rows = vec![
            p(1, 0, 5.0),    // launchd: its children are roots
            p(10, 1, 10.0),  // app
            p(11, 10, 30.0), // helpers
            p(12, 10, 1.0),
            p(20, 1, 50.0),  // another app
            p(30, 999, 2.0), // parent not readable
        ];
        let order = row_order(&layout, SortBy::Col(Col::Cmprs));
        let (out, trees) = treeify(rows, &layout, &order, 0);
        let shape: Vec<(c_int, usize)> = out.iter().map(|r| (r.pid, r.depth)).collect();
        assert_eq!(
            shape,
            vec![(20, 0), (10, 0), (11, 1), (12, 1), (1, 0), (30, 0)]
        );
        assert_eq!(trees, 4);
        let app = &out[1];
        assert_eq!(app.vals, vec![Some(3.0), Some(41.0), None]); // peak can't add up
        assert!(app.rollup && !out[2].rollup);
        assert_eq!(layout.text(app, 2), None); // blank, not N/A
        assert_eq!(out[2].vals[2], Some(30.0)); // a leaf keeps its own peak

        let rows = vec![p(10, 1, 10.0), p(11, 10, 30.0), p(20, 1, 50.0)];
        let (out, trees) = treeify(rows, &layout, &order, 1);
        assert_eq!((out.len(), trees), (1, 1));
        assert_eq!(total(&layout, &out, Col::Cmprs), Some(50.0));
    }

    #[test]
    fn test_treeify_survives_parent_cycle() {
        let layout = Layout {
            cols: vec![Col::Cmprs],
            visible: vec![0],
        };
        let rows = vec![row(5, 6, &[Some(1.0)]), row(6, 5, &[Some(2.0)])];
        let order = row_order(&layout, SortBy::Pid);
        let (out, _) = treeify(rows, &layout, &order, 0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn test_render_csv() {
        let layout = Layout {
            cols: vec![Col::Cmprs, Col::Headroom, Col::Swapins],
            visible: vec![0],
        };
        let mut rollup = row(7, 1, &[Some(2048.0), None, None]);
        rollup.name = "a, \"b\"".to_string();
        rollup.rollup = true;
        let leaf = row(8, 7, &[Some(1024.4), Some(0.0), Some(-0.2)]);
        let mut out = String::new();
        render_csv(&mut out, &layout, &[rollup, leaf], false);
        assert_eq!(
            out,
            "pid,ppid,name,cmprs,headroom,swapins\n\
             7,1,\"a, \"\"b\"\"\",2048,,\n\
             8,7,p8,1024,0,0\n"
        );
    }

    #[test]
    fn test_col_display() {
        assert_eq!(Col::Headroom.display(Some(0.0)), "-");
        assert_eq!(Col::Headroom.display(None), "N/A");
        assert_eq!(Col::Grew.display(None), "");
        assert_eq!(Col::Grew.display(Some(66.6)), "67%");
        assert_eq!(Col::DecompGrowth.display(Some(3.0)), "+3");
        assert_eq!(Col::Delta.display(Some(1024.0)), "+1.00K");
        assert_eq!(Col::JetsamEta.display(Some(150.0)), "2m30s");
    }

    #[test]
    fn test_duration_str() {
        assert_eq!(duration_str(-1.0), "0s");
        assert_eq!(duration_str(42.9), "42s");
        assert_eq!(duration_str(785.0), "13m05s");
        assert_eq!(duration_str(18_180.0), "5h03m");
        assert_eq!(duration_str(273_600.0), "3d04h");
    }

    #[test]
    fn test_trend() {
        let hist = |pts: &[(f64, u64)]| pts.iter().copied().collect::<VecDeque<_>>();
        let young = trend(&hist(&[(0.0, 100), (1.0, 110), (2.0, 120)]), Some(1000));
        assert_eq!((young.grew_pct, young.eta_secs), (None, None)); // < TREND_MIN intervals
        let h = hist(&[(0.0, 100), (1.0, 110), (2.0, 105), (3.0, 120)]);
        let t = trend(&h, Some(1000));
        assert!((t.grew_pct.unwrap() - 200.0 / 3.0).abs() < 1e-9);
        assert!((t.eta_secs.unwrap() - 150.0).abs() < 1e-9); // 1000 B at 20 B / 3 s
        assert_eq!(trend(&h, Some(0)).eta_secs, None); // no limit
        assert_eq!(trend(&h, None).eta_secs, None);
        let shrinking = hist(&[(0.0, 120), (1.0, 110), (2.0, 105), (3.0, 100)]);
        assert_eq!(trend(&shrinking, Some(1000)).eta_secs, None);
    }

    fn proc_at(pid: c_int, start: u64, swapins: Option<u64>) -> Proc {
        Proc {
            pid,
            ppid: 1,
            uid: Some(501),
            start: Some((start, 0)),
            name: format!("p{pid}"),
            vm: VmStats {
                compressed: 0,
                compressed_peak: 0,
                footprint: 0,
                footprint_peak: None,
                limit_remaining: None,
                decompressions: None,
                swapins,
            },
        }
    }

    fn set(procs: Vec<Proc>) -> SampleSet {
        SampleSet {
            procs: procs.into_iter().map(|p| (p.pid, p)).collect(),
            denied: 0,
            at: Instant::now(),
        }
    }

    #[test]
    fn test_churn_and_interval_swapins() {
        let a = set(vec![
            proc_at(1, 100, Some(10)),
            proc_at(2, 200, Some(0)),
            proc_at(3, 300, Some(5)),
        ]);
        let b = set(vec![
            proc_at(1, 100, Some(40)),
            proc_at(3, 301, Some(0)), // PID reused
            proc_at(4, 400, Some(7)),
        ]);
        let all = Filter {
            name: None,
            uid: None,
        };
        let (started, exited) = churn(&a, &b, &all);
        let pids = |ps: &[&Proc]| ps.iter().map(|p| p.pid).collect::<Vec<_>>();
        assert_eq!(pids(&started), vec![3, 4]);
        assert_eq!(pids(&exited), vec![2, 3]);
        assert_eq!(interval_swapins(&a, &b), Some(30)); // only pid 1 was sampled twice
        let none = Filter {
            name: Some("nomatch".to_string()),
            uid: None,
        };
        let (started, exited) = churn(&a, &b, &none);
        assert!(started.is_empty() && exited.is_empty());
    }

    #[test]
    fn test_filter_keeps() {
        let p = proc_at(1, 100, None);
        let keeps = |name: Option<&str>, uid| {
            Filter {
                name: name.map(str::to_string),
                uid,
            }
            .keeps(&p)
        };
        assert!(keeps(None, None));
        assert!(keeps(Some("p1"), Some(501)));
        assert!(!keeps(Some("x"), None));
        assert!(!keeps(None, Some(0)));
    }

    #[test]
    fn test_csv_field_and_num() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_num(-0.4), "0");
        assert_eq!(csv_num(1.6), "2");
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
    fn test_vm_statistics64_layout() {
        // offsetof() values from <mach/vm_statistics.h>.
        assert_eq!(offset_of!(VmStatistics64, decompressions), 96);
        assert_eq!(offset_of!(VmStatistics64, swapins), 112);
        assert_eq!(offset_of!(VmStatistics64, swapouts), 120);
        assert_eq!(offset_of!(VmStatistics64, compressor_page_count), 128);
        assert_eq!(
            offset_of!(VmStatistics64, total_uncompressed_pages_in_compressor),
            144
        );
        assert_eq!(
            offset_of!(VmStatistics64, swapped_count) / 4,
            HOST_VM_INFO64_REV1_COUNT as usize
        );
        assert_eq!(
            size_of::<VmStatistics64>() / 4,
            HOST_VM_INFO64_REV2_COUNT as usize
        );
    }

    #[test]
    fn test_proc_info_layouts() {
        assert_eq!(offset_of!(ProcBsdInfo, pbi_ppid), 16);
        assert_eq!(offset_of!(ProcBsdInfo, pbi_uid), 20);
        assert_eq!(offset_of!(ProcBsdInfo, pbi_start_tvsec), 120);
        assert_eq!(size_of::<ProcBsdInfo>(), 136);
        assert_eq!(offset_of!(ProcTaskInfo, pti_faults), 52);
        assert_eq!(offset_of!(ProcTaskInfo, pti_threadnum), 84);
        assert_eq!(size_of::<ProcTaskInfo>(), 96);
        assert_eq!(offset_of!(RusageInfoV2, ri_pageins), 48);
        assert_eq!(offset_of!(RusageInfoV2, ri_wired_size), 56);
        assert_eq!(offset_of!(RusageInfoV2, ri_diskio_bytesread), 144);
        assert_eq!(size_of::<RusageInfoV2>(), 160);
    }

    #[test]
    fn test_libc_layouts() {
        assert_eq!(offset_of!(XswUsage, xsu_used), 16);
        assert_eq!(offset_of!(Passwd, pw_uid), 16);
        assert_eq!(offset_of!(Termios, c_lflag), 24);
        assert_eq!(offset_of!(Termios, c_cc), 32);
        assert_eq!(offset_of!(Termios, c_ispeed), 56);
        assert_eq!(size_of::<Termios>(), 72);
        assert_eq!(size_of::<PollFd>(), 8);
        assert_eq!(size_of::<WinSize>(), 8);
    }
}
