//! Watch modes: footprint change over a single interval (`--once`, `--csv`),
//! or refreshed every interval with the growth trend (`--watch`).

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::cli::Config;
use crate::ffi::{isatty, signal, SIGHUP, SIGINT, SIGTERM};
use crate::format::opt_bytes;
use crate::sample::{
    churn, interval_swapins, same_process_start, sample, Filter, SampleSet, StartTime,
};
use crate::summary::{summary, SysMem};
use crate::table::{arrange, render_csv, render_table, Layout, ParentNames, Row, RowSrc, Trend};
use crate::term::{color_enabled, emit_raw, term_rows, wait_key, Screen};

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

pub fn run_once(config: &Config, filter: &Filter, interval: u64) {
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
        "interval {:.1}s — one interval = allocation churn; trust SUSTAINED growth as leak signal (watch without --once tracks it).",
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

/// Watch-mode footprint history window, in intervals.
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
    STOP.store(true, Ordering::SeqCst);
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
    let state = if paused { "PAUSED" } else { "watch" };
    let help = match (keys, paused) {
        (true, false) => "space: pause · q: quit",
        (true, true) => "space: resume · q: quit",
        (false, _) => "Ctrl-C: quit",
    };
    format!("macswapi {state} · every {interval}s · refresh #{tick} · {help}")
}

fn watch_frame(
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

pub fn run_watch(config: &Config, filter: &Filter, interval: u64) {
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
            if STOP.load(Ordering::SeqCst) {
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
        frame = watch_frame(config, filter, (&sys, &sys_now), &prev, &cur, &hist);
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
