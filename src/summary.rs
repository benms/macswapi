//! The system-wide memory summary above the process table.

use std::fmt::Write as _;
use std::time::Instant;

use crate::ffi::{MsgTypeNumber, VmStatistics64, XswUsage, HOST_VM_INFO64_REV2_COUNT};
use crate::format::human;
use crate::kernel::{host_vm, swap_usage, sysctl_num};

/// System-wide memory state for the summary header.
pub struct SysMem {
    at: Instant,
    total: Option<u64>,
    page: u64,
    vm: Option<(VmStatistics64, MsgTypeNumber)>,
    swap: Option<XswUsage>,
    pressure: Option<i32>,
    level: Option<i32>,
}

impl SysMem {
    pub fn read() -> SysMem {
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

    /// Uncompressed bytes the compressor holds, over every process and the
    /// kernel: the same unit as each process's `compressed`.
    pub fn compressor_held(&self) -> Option<u64> {
        self.vm.as_ref().map(|(vm, _)| {
            vm.total_uncompressed_pages_in_compressor
                .saturating_mul(self.page)
        })
    }

    /// `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warn, 4 critical.
    pub fn pressure(&self) -> Option<i32> {
        self.pressure
    }

    /// `kern.memorystatus_level`: percent of memory available, by the kernel's
    /// own measure.
    pub fn level(&self) -> Option<i32> {
        self.level
    }

    pub fn total(&self) -> Option<u64> {
        self.total
    }

    /// (used, total) swap bytes.
    pub fn swap(&self) -> Option<(u64, u64)> {
        self.swap.map(|x| (x.xsu_used, x.xsu_total))
    }

    /// Where physical memory is, in bytes; None if host_statistics64 failed.
    pub fn breakdown(&self) -> Option<Breakdown> {
        let (vm, _) = self.vm.as_ref()?;
        let b = |pages: u64| pages.saturating_mul(self.page);
        Some(Breakdown {
            wired: b(vm.wire_count.into()),
            active: b(vm.active_count.into()),
            inactive: b(vm.inactive_count.into()),
            free: b(vm.free_count.into()),
            compressor: b(vm.compressor_page_count.into()),
            swapped_in: b(vm.swapins),
            swapped_out: b(vm.swapouts),
        })
    }

    /// Rates over the time since `before`; None if either read failed.
    pub fn rates(&self, before: &SysMem) -> Option<Rates> {
        let ((now, _), (old, _)) = (self.vm.as_ref()?, before.vm.as_ref()?);
        let secs = self.at.duration_since(before.at).as_secs_f64().max(1e-9);
        let per_sec = |a: u64, b: u64| b.saturating_sub(a) as f64 / secs;
        Some(Rates {
            swapin_bytes: per_sec(old.swapins, now.swapins) * self.page as f64,
            swapout_bytes: per_sec(old.swapouts, now.swapouts) * self.page as f64,
            compressions: per_sec(old.compressions, now.compressions),
            decompressions: per_sec(old.decompressions, now.decompressions),
        })
    }
}

/// Physical memory by state, plus swap traffic since boot, in bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Breakdown {
    pub wired: u64,
    pub active: u64,
    pub inactive: u64,
    pub free: u64,
    /// RAM the compressor occupies (not what it holds)
    pub compressor: u64,
    pub swapped_in: u64,
    pub swapped_out: u64,
}

/// Per-second rates between two reads: swap bytes, compressor pages.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rates {
    pub swapin_bytes: f64,
    pub swapout_bytes: f64,
    pub compressions: f64,
    pub decompressions: f64,
}

/// Seconds since boot, from `kern.boottime`.
pub fn uptime_secs() -> Option<u64> {
    // struct timeval: tv_sec (i64), tv_usec (i32, padded to 8 bytes)
    let boot = sysctl_num::<[i64; 2]>(c"kern.boottime")?[0];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    u64::try_from(now - boot).ok()
}

/// A pressure level's name, or its number if unknown.
pub fn pressure_name(level: i32) -> String {
    match level {
        1 => "normal".to_string(),
        2 => "warn".to_string(),
        4 => "critical".to_string(),
        n => n.to_string(),
    }
}

fn pressure_str(level: Option<i32>, available: Option<i32>) -> String {
    let name = level.map_or_else(|| "?".to_string(), pressure_name);
    match available {
        Some(pct) => format!("{name} (memorystatus level {pct}%)"),
        None => name,
    }
}

/// Header lines; with `before`, a line of rates over the time since then.
pub fn summary(out: &mut String, now: &SysMem, before: Option<&SysMem>) {
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
