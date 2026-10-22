//! Raw libSystem bindings and the C struct layouts they fill. The layouts are
//! ABI-critical: the tests pin them to the headers' `offsetof()` values.

use std::ffi::c_void;
use std::os::raw::{c_char, c_int};

// ---- type aliases ---------------------------------------------------------

pub type KernReturn = c_int; // kern_return_t
pub type MachPort = u32; // mach_port_t
pub type TaskFlavor = c_int; // task_flavor_t
pub type MsgTypeNumber = u32; // mach_msg_type_number_t (natural_t)

pub const KERN_SUCCESS: KernReturn = 0;
pub const TASK_VM_INFO: TaskFlavor = 22;
// `integer_t` counts per `task_vm_info` revision (<mach/task_info.h>).
pub const TASK_VM_INFO_REV3_COUNT: MsgTypeNumber = 84;
pub const TASK_VM_INFO_REV4_COUNT: MsgTypeNumber = 86;
pub const TASK_VM_INFO_REV5_COUNT: MsgTypeNumber = 87;
pub const TASK_VM_INFO_REV6_COUNT: MsgTypeNumber = 89;
pub const HOST_VM_INFO64: c_int = 4;
// `integer_t` counts per `vm_statistics64` revision (<mach/host_info.h>).
pub const HOST_VM_INFO64_REV1_COUNT: MsgTypeNumber = 38;
pub const HOST_VM_INFO64_REV2_COUNT: MsgTypeNumber = 40;
pub const PROC_ALL_PIDS: u32 = 1;
pub const PROC_PIDTASKINFO: c_int = 4;
pub const PROC_PIDTBSDINFO: c_int = 3;
pub const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;
pub const RUSAGE_INFO_V2: c_int = 2;

// ---- FFI (libSystem, linked automatically on macOS) -----------------------

extern "C" {
    pub static mach_task_self_: MachPort;
    pub fn mach_host_self() -> MachPort;
    pub fn task_name_for_pid(target: MachPort, pid: c_int, t: *mut MachPort) -> KernReturn;
    pub fn task_info(
        task: MachPort,
        flavor: TaskFlavor,
        out: *mut c_int,
        count: *mut MsgTypeNumber,
    ) -> KernReturn;
    pub fn host_statistics64(
        host: MachPort,
        flavor: c_int,
        out: *mut c_int,
        count: *mut MsgTypeNumber,
    ) -> KernReturn;
    pub fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
    pub fn proc_listpids(t: u32, ti: u32, buf: *mut c_void, sz: c_int) -> c_int;
    pub fn proc_pidpath(pid: c_int, buf: *mut c_void, sz: u32) -> c_int;
    pub fn proc_name(pid: c_int, buf: *mut c_void, sz: u32) -> c_int;
    pub fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buf: *mut c_void, sz: c_int) -> c_int;
    pub fn proc_pid_rusage(pid: c_int, flavor: c_int, buf: *mut c_void) -> c_int;
    /// Private (libquarantine, re-exported by libSystem): the process macOS
    /// holds responsible for `pid`, as Activity Monitor groups them; -1 if none.
    pub fn responsibility_get_pid_responsible_for_pid(pid: c_int) -> c_int;
    pub fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
    pub fn getpwnam(name: *const c_char) -> *const Passwd;
    pub fn getpwuid(uid: u32) -> *const Passwd;
    pub fn isatty(fd: c_int) -> c_int;
    pub fn ioctl(fd: c_int, request: u64, ...) -> c_int;
    pub fn tcgetattr(fd: c_int, t: *mut Termios) -> c_int;
    pub fn tcsetattr(fd: c_int, action: c_int, t: *const Termios) -> c_int;
    pub fn poll(fds: *mut PollFd, nfds: u32, timeout_ms: c_int) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
    pub fn signal(sig: c_int, handler: usize) -> usize;
}

pub const SIGHUP: c_int = 1;
pub const SIGINT: c_int = 2;
pub const SIGPIPE: c_int = 13;
pub const SIGTERM: c_int = 15;
pub const SIG_DFL: usize = 0;
pub const TIOCGWINSZ: u64 = 0x4008_7468;
pub const ICANON: u64 = 0x100;
pub const ECHO: u64 = 0x8;
pub const VMIN: usize = 16;
pub const VTIME: usize = 17;
pub const TCSANOW: c_int = 0;
pub const POLLIN: i16 = 1;

// ---- struct layouts (ABI-critical) ----------------------------------------

/// `task_vm_info` truncated through rev6 (`ledger_swapins`). `task_info` copies
/// `min(count, actual)` `integer_t` (4-byte) units and returns how many it
/// wrote, so a prefix is enough and older kernels hand back an earlier rev.
/// The header is `#pragma pack(4)`: `ledger_swapins` is only 4-aligned.
#[repr(C, packed(4))]
#[derive(Default)]
pub struct TaskVmInfo {
    pub virtual_size: u64,  // off 0
    pub region_count: i32,  // off 8
    pub page_size: i32,     // off 12
    pub resident_size: u64, // off 16
    pub resident_size_peak: u64,
    pub device: u64,
    pub device_peak: u64,
    pub internal: u64,
    pub internal_peak: u64,
    pub external: u64,
    pub external_peak: u64,
    pub reusable: u64,
    pub reusable_peak: u64,
    pub purgeable_volatile_pmap: u64,
    pub purgeable_volatile_resident: u64,
    pub purgeable_volatile_virtual: u64,
    pub compressed: u64,      // off 120  <-- READ THIS
    pub compressed_peak: u64, // off 128  <-- READ THIS
    pub compressed_lifetime: u64,
    // rev1
    pub phys_footprint: u64, // off 144  <-- READ THIS
    // rev2
    pub min_address: u64,
    pub max_address: u64,
    // rev3
    pub ledger_phys_footprint_peak: i64, // off 168  <-- READ THIS
    // ledger_purgeable_* (4) + ledger_tag_{network,media,graphics,neural}_* (16)
    pub purgeable_and_tag_ledgers: [i64; 20],
    // rev4
    pub limit_bytes_remaining: u64, // off 336  <-- READ THIS
    // rev5
    pub decompressions: i32, // off 344  <-- READ THIS
    // rev6
    pub ledger_swapins: i64, // off 348  <-- READ THIS
}
// size_of == 356, /4 == 89 = TASK_VM_INFO_REV6_COUNT, the count passed to task_info.

/// `task_vm_info` rev3 ledgers as (total, compressed) pairs, in header order.
pub const LEDGER_NAMES: [&str; 10] = [
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
pub struct ProcBsdInfo {
    pub pbi_flags: u32,
    pub pbi_status: u32,
    pub pbi_xstatus: u32,
    pub pbi_pid: u32,
    pub pbi_ppid: u32, // off 16  <-- READ THIS
    pub pbi_uid: u32,  // off 20  <-- READ THIS
    pub pbi_gid: u32,
    pub pbi_ruid: u32,
    pub pbi_rgid: u32,
    pub pbi_svuid: u32,
    pub pbi_svgid: u32,
    pub pbi_rfu: u32,
    pub pbi_comm: [u8; 16],
    pub pbi_name: [u8; 32],
    pub pbi_nfiles: u32,
    pub pbi_pgid: u32,
    pub pbi_pjobc: u32,
    pub e_tdev: u32,
    pub e_tpgid: u32,
    pub pbi_nice: i32,
    pub pbi_start_tvsec: u64, // off 120  <-- READ THIS
    pub pbi_start_tvusec: u64,
}

/// `proc_taskinfo` — full struct required, like `proc_bsdinfo`.
#[repr(C)]
#[derive(Default)]
pub struct ProcTaskInfo {
    pub pti_virtual_size: u64,
    pub pti_resident_size: u64,
    pub pti_total_user: u64,
    pub pti_total_system: u64,
    pub pti_threads_user: u64,
    pub pti_threads_system: u64,
    pub pti_policy: i32,
    pub pti_faults: i32,     // off 52  <-- READ THIS
    pub pti_pageins: i32,    // off 56  <-- READ THIS
    pub pti_cow_faults: i32, // off 60  <-- READ THIS
    pub pti_messages_sent: i32,
    pub pti_messages_received: i32,
    pub pti_syscalls_mach: i32,
    pub pti_syscalls_unix: i32,
    pub pti_csw: i32,
    pub pti_threadnum: i32, // off 84  <-- READ THIS
    pub pti_numrunning: i32,
    pub pti_priority: i32,
}

/// `rusage_info_v2` — full struct required: `proc_pid_rusage` writes all of it.
#[repr(C)]
#[derive(Default)]
pub struct RusageInfoV2 {
    pub ri_uuid: [u8; 16],
    pub ri_user_time: u64,
    pub ri_system_time: u64,
    pub ri_pkg_idle_wkups: u64,
    pub ri_interrupt_wkups: u64,
    pub ri_pageins: u64,    // off 48  <-- READ THIS
    pub ri_wired_size: u64, // off 56  <-- READ THIS
    pub ri_resident_size: u64,
    pub ri_phys_footprint: u64,
    pub ri_proc_start_abstime: u64,
    pub ri_proc_exit_abstime: u64,
    pub ri_child_user_time: u64,
    pub ri_child_system_time: u64,
    pub ri_child_pkg_idle_wkups: u64,
    pub ri_child_interrupt_wkups: u64,
    pub ri_child_pageins: u64,
    pub ri_child_elapsed_abstime: u64,
    pub ri_diskio_bytesread: u64,    // off 144  <-- READ THIS
    pub ri_diskio_byteswritten: u64, // off 152  <-- READ THIS
}

/// `xsw_usage` — swap totals from `vm.swapusage`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct XswUsage {
    pub xsu_total: u64,
    pub xsu_avail: u64,
    pub xsu_used: u64, // off 16  <-- READ THIS
    pub xsu_pagesize: u32,
    pub xsu_encrypted: i32,
}

/// `vm_statistics64` truncated through rev2 (`swapped_count`); same prefix
/// rule as `task_info`: `host_statistics64` returns the count it wrote.
#[repr(C)]
#[derive(Default)]
pub struct VmStatistics64 {
    pub free_count: u32,     // off 0
    pub active_count: u32,   // off 4
    pub inactive_count: u32, // off 8
    pub wire_count: u32,     // off 12
    pub zero_fill_count: u64,
    pub reactivations: u64,
    pub pageins: u64,
    pub pageouts: u64,
    pub faults: u64,
    pub cow_faults: u64,
    pub lookups: u64,
    pub hits: u64,
    pub purges: u64,
    pub purgeable_count: u32,
    pub speculative_count: u32,
    pub decompressions: u64,        // off 96
    pub compressions: u64,          // off 104
    pub swapins: u64,               // off 112
    pub swapouts: u64,              // off 120
    pub compressor_page_count: u32, // off 128
    pub throttled_count: u32,
    pub external_page_count: u32,
    pub internal_page_count: u32,
    pub total_uncompressed_pages_in_compressor: u64, // off 144
    // rev2
    pub swapped_count: u64, // off 152
}
// size_of == 160, /4 == 40 = HOST_VM_INFO64_REV2_COUNT.

/// `struct passwd` prefix through `pw_uid`; only read through libc's pointer.
#[repr(C)]
pub struct Passwd {
    pub pw_name: *const c_char, // off 0
    pub pw_passwd: *const c_char,
    pub pw_uid: u32, // off 16
}

/// `struct termios` (tcflag_t and speed_t are `unsigned long`).
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct Termios {
    pub c_iflag: u64,
    pub c_oflag: u64,
    pub c_cflag: u64,
    pub c_lflag: u64,   // off 24
    pub c_cc: [u8; 20], // off 32
    pub c_ispeed: u64,  // off 56
    pub c_ospeed: u64,
}

#[repr(C)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub revents: i16,
}

#[repr(C)]
#[derive(Default)]
pub struct WinSize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

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
