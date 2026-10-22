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
