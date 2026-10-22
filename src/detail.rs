//! `--pid`: everything macswapi can read about one process.

use std::fmt::Write as _;
use std::os::raw::c_int;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ffi::{
    ProcBsdInfo, ProcTaskInfo, LEDGER_NAMES, PROC_PIDTASKINFO, PROC_PIDTBSDINFO,
    TASK_VM_INFO_REV3_COUNT,
};
use crate::format::{duration_str, headroom_str, human, opt_bytes};
use crate::kernel::{
    list_pids, pidinfo, proc_display_name, proc_path, responsible_pid, rusage, swap_usage, task_vm,
    task_vm_raw, user_name, VmStats,
};
use crate::sample::{swap_denominator, swap_ratio};
use crate::summary::SysMem;

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

pub fn run_detail(pid: c_int) {
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
        let resp = match responsible_pid(pid) {
            Some(r) if r == pid => "itself".to_string(),
            Some(r) => format!("{r} ({})", proc_display_name(r)),
            None => "?".to_string(),
        };
        kv(&mut out, "responsible process", resp);
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

    // SWAP~ needs the denominator: the compressor's whole content, or failing
    // that, every readable process.
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
    let held = SysMem::read().compressor_held();
    let denominator = swap_denominator(held, all_compressed);
    let estimate = swap_ratio(swap_used, denominator)
        .map_or_else(|| "N/A".to_string(), |k| human(vm.compressed as f64 * k));
    let mut note = format!("share of {} system swap", opt_bytes(swap_used));
    if held.is_some() {
        let _ = write!(
            note,
            " over the compressor's {}, {} of it in {readable} readable procs",
            human(denominator as f64),
            human(all_compressed as f64),
        );
    } else {
        let _ = write!(note, " over {readable} readable procs");
        if denied > 0 {
            let _ = write!(note, "; {denied} denied, run as root for all");
        }
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
