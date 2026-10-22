//! Safe wrappers over the `ffi` calls: system memory totals, the process list,
//! and per-process VM stats, names and users.

use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_int;
use std::ptr;

use crate::ffi::*;

pub fn swap_usage() -> Result<XswUsage, std::io::Error> {
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
pub fn sysctl_num<T: Default>(name: &CStr) -> Option<T> {
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

pub fn host_vm() -> Option<(VmStatistics64, MsgTypeNumber)> {
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

pub fn list_pids() -> Vec<c_int> {
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
pub struct VmStats {
    pub compressed: u64,
    pub compressed_peak: u64,
    pub footprint: u64,
    /// lifetime peak of `footprint` (rev3)
    pub footprint_peak: Option<u64>,
    /// bytes left under the jetsam limit; 0 = no limit, or at/over a soft one (rev4)
    pub limit_remaining: Option<u64>,
    /// pages decompressed over the process lifetime (rev5)
    pub decompressions: Option<u32>,
    /// bytes swapped in over the process lifetime (rev6)
    pub swapins: Option<u64>,
}

impl VmStats {
    /// `count` is what `task_info` returned: the `integer_t` units it wrote.
    pub fn from_info(info: &TaskVmInfo, count: MsgTypeNumber) -> VmStats {
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
pub fn task_vm_raw(pid: c_int) -> Option<(TaskVmInfo, MsgTypeNumber)> {
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

pub fn task_vm(pid: c_int) -> Option<VmStats> {
    task_vm_raw(pid).map(|(info, count)| VmStats::from_info(&info, count))
}

/// A fixed-size `proc_pidinfo` flavor: it returns the full size or fails.
pub fn pidinfo<T: Default>(pid: c_int, flavor: c_int) -> Option<T> {
    let mut info = T::default();
    let sz = std::mem::size_of::<T>() as c_int;
    let rc = unsafe { proc_pidinfo(pid, flavor, 0, &mut info as *mut T as *mut c_void, sz) };
    (rc == sz).then_some(info)
}

pub fn rusage(pid: c_int) -> Option<RusageInfoV2> {
    let mut info = RusageInfoV2::default();
    let rc = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V2, &mut info as *mut _ as *mut c_void) };
    (rc == 0).then_some(info)
}

fn cstr_buf(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

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

pub fn proc_path(pid: c_int) -> Option<String> {
    let mut buf = [0u8; PROC_PIDPATHINFO_MAXSIZE];
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if n > 0 && (n as usize) <= buf.len() {
        Some(cstr_buf(&buf[..n as usize])).filter(|p| !p.is_empty())
    } else {
        None
    }
}

pub fn proc_display_name(pid: c_int) -> String {
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

pub fn user_name(uid: u32) -> Option<String> {
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
pub fn resolve_user(user: &str) -> Result<u32, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basename() {
        assert_eq!(basename("/usr/bin/node"), "node");
        assert_eq!(basename("node"), "node");
        assert_eq!(basename("/usr/bin/"), "/usr/bin/"); // trailing slash → whole
        assert_eq!(basename(""), "");
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
}
