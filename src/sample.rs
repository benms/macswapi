//! Process samples: every readable process at one instant, what changed
//! between two samples, and the `--name` / `--user` filter.

use std::collections::HashMap;
use std::os::raw::c_int;
use std::time::Instant;

use crate::cli::Config;
use crate::ffi::{ProcBsdInfo, PROC_PIDTBSDINFO};
use crate::kernel::{
    list_pids, pidinfo, proc_display_name, resolve_user, responsible_pid, task_vm, VmStats,
};

pub type StartTime = (u64, u64);

/// One readable process at one instant.
pub struct Proc {
    pub pid: c_int,
    pub ppid: c_int,
    pub uid: Option<u32>,
    pub start: Option<StartTime>,
    /// the process macOS holds responsible for this one (`--app` groups by it)
    pub resp: Option<c_int>,
    pub name: String,
    pub vm: VmStats,
}

pub struct SampleSet {
    pub procs: HashMap<c_int, Proc>,
    pub denied: usize,
    pub at: Instant,
}

impl SampleSet {
    pub fn total(&self) -> usize {
        self.procs.len() + self.denied
    }

    /// Compressed memory over every readable process, whatever is filtered out.
    pub fn total_compressed(&self) -> u64 {
        self.procs.values().map(|p| p.vm.compressed).sum()
    }
}

/// Swap bytes attributed per compressed byte, or None when the swap total is
/// unavailable or no readable process holds compressed memory.
pub fn swap_ratio(swap_total: Option<u64>, total_compressed: u64) -> Option<f64> {
    swap_total
        .filter(|_| total_compressed > 0)
        .map(|swap| swap as f64 / total_compressed as f64)
}

/// The SWAP~ denominator: what the compressor holds system-wide (`held`, from
/// `host_statistics64`) when the host reports it, so kernel_task and processes
/// we can't read keep their share instead of it landing on the readable ones.
/// Never below the readable sum, as the two are read at different instants.
pub fn swap_denominator(held: Option<u64>, readable: u64) -> u64 {
    held.map_or(readable, |h| h.max(readable))
}

pub fn same_process_start(a: Option<StartTime>, b: Option<StartTime>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a == b)
}

pub fn sample() -> SampleSet {
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
            resp: responsible_pid(pid),
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
pub fn churn<'a>(
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
pub fn interval_swapins(a: &SampleSet, b: &SampleSet) -> Option<u64> {
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

/// Bytes each process swapped in between samples, largest first, zeros left
/// out; over every process readable in both, as `interval_swapins`.
pub fn swapins_by_proc<'a>(a: &SampleSet, b: &'a SampleSet) -> Vec<(&'a Proc, u64)> {
    let mut out: Vec<(&Proc, u64)> = b
        .procs
        .values()
        .filter_map(|p| {
            let q = a.procs.get(&p.pid)?;
            if !same_process_start(q.start, p.start) {
                return None;
            }
            Some((p, p.vm.swapins?.saturating_sub(q.vm.swapins?)))
        })
        .filter(|&(_, n)| n > 0)
        .collect();
    out.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.pid.cmp(&y.0.pid)));
    out
}

/// `--name` / `--user`: which processes become rows. Never applied to the
/// SWAP~ denominator, which must cover every readable process.
pub struct Filter {
    name: Option<String>, // lowercase
    uid: Option<u32>,
}

impl Filter {
    pub fn from_config(config: &Config) -> Result<Filter, String> {
        Ok(Filter {
            name: config.name.as_ref().map(|n| n.to_lowercase()),
            uid: config.user.as_deref().map(resolve_user).transpose()?,
        })
    }

    pub fn active(&self) -> bool {
        self.name.is_some() || self.uid.is_some()
    }

    pub fn keeps(&self, p: &Proc) -> bool {
        self.name
            .as_ref()
            .map_or(true, |n| p.name.to_lowercase().contains(n.as_str()))
            && self.uid.map_or(true, |uid| p.uid == Some(uid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_swap_denominator() {
        assert_eq!(swap_denominator(None, 5), 5); // host total unavailable
        assert_eq!(swap_denominator(Some(10), 5), 10); // the rest is unattributed
        assert_eq!(swap_denominator(Some(3), 5), 5); // read a moment apart
    }

    #[test]
    fn test_same_process_start_fails_closed() {
        assert!(same_process_start(Some((10, 20)), Some((10, 20))));
        assert!(!same_process_start(Some((10, 20)), Some((10, 21))));
        assert!(!same_process_start(None, Some((10, 20))));
        assert!(!same_process_start(Some((10, 20)), None));
        assert!(!same_process_start(None, None));
    }

    fn proc_at(pid: c_int, start: u64, swapins: Option<u64>) -> Proc {
        Proc {
            pid,
            ppid: 1,
            uid: Some(501),
            start: Some((start, 0)),
            resp: None,
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
        let by_proc: Vec<(c_int, u64)> = swapins_by_proc(&a, &b)
            .iter()
            .map(|(p, n)| (p.pid, *n))
            .collect();
        assert_eq!(by_proc, vec![(1, 30)]);
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
}
