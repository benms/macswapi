//! Watch modes: footprint change over a single interval (`--once`), or
//! refreshed every interval with the growth trend (`--watch`), drawn on the
//! terminal or streamed as CSV, with `--alert` rules and `--record`.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::alert::{notify, Alerts, Firing};
use crate::cli::{Config, Group};
use crate::ffi::{isatty, signal, SIGHUP, SIGINT, SIGTERM};
use crate::format::{iso8601_utc, opt_bytes};
use crate::kernel::proc_display_name;
use crate::record::Recorder;
use crate::sample::{
    churn, interval_swapins, same_process_start, sample, swapins_by_proc, Filter, SampleSet,
    StartTime,
};
use crate::summary::{summary, SysMem};
use crate::table::{
    arrange, csv_header, cut, render_csv, render_csv_rows, render_table, Layout, ParentNames, Row,
    RowSrc, Trend,
};
use crate::term::{color_enabled, emit_raw, term_rows, wait_key, Screen};

/// Exit status of `--exit-on-alert` when a rule fires.
pub(crate) const ALERT_EXIT: i32 = 3;

/// Rows for processes present in both samples with the same start time.
pub(crate) fn interval_rows(
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
            let points = hist.and_then(|h| h.get(&(p.pid, p.start?)));
            let mut row = layout.row(&RowSrc {
                now: p,
                before: Some(q),
                elapsed,
                swap_per_cmprs: None,
                trend: points.map(|h| trend(h, p.vm.limit_remaining)),
            });
            if let Some(h) = points {
                row.hist = h.iter().map(|&(_, foot)| foot).collect();
            }
            Some(row)
        })
        .collect()
}

/// Bytes swapped in between samples, over every process sampled twice, and
/// the `top` processes (with `by_app`, apps) they went to, as "Xcode 812 74%".
pub(crate) fn swapin_culprits(
    a: &SampleSet,
    b: &SampleSet,
    by_app: bool,
    top: usize,
) -> (Option<u64>, Vec<String>) {
    let total = interval_swapins(a, b);
    let Some(sum) = total.filter(|&t| t > 0) else {
        return (total, Vec::new());
    };
    let mut by: Vec<(c_int, u64)> = Vec::new();
    for (p, n) in swapins_by_proc(a, b) {
        let key = if by_app {
            p.resp.unwrap_or(p.pid)
        } else {
            p.pid
        };
        match by.iter_mut().find(|(k, _)| *k == key) {
            Some((_, acc)) => *acc += n,
            None => by.push((key, n)),
        }
    }
    by.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
    let names = by
        .iter()
        .take(top)
        .map(|&(pid, n)| {
            let name = b
                .procs
                .get(&pid)
                .map_or_else(|| proc_display_name(pid), |p| p.name.clone());
            format!("{name} {pid} {:.0}%", 100.0 * n as f64 / sum as f64)
        })
        .collect();
    (total, names)
}

/// "swapped in during interval: 120.00M — Xcode 812 74%, Slack 1044 10%".
fn swapin_line(out: &mut String, label: &str, a: &SampleSet, b: &SampleSet, by_app: bool) {
    let (total, names) = swapin_culprits(a, b, by_app, 3);
    let _ = write!(out, "{label}: {}", opt_bytes(total));
    if !names.is_empty() {
        let _ = write!(out, " — {}", names.join(", "));
    }
    out.push('\n');
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
    let rows = cut(
        arrange(
            interval_rows(&layout, filter, &a, &b, None),
            &layout,
            config,
        ),
        config.topn,
    );

    let mut out = String::new();
    if config.csv {
        render_csv(&mut out, &layout, &rows, config.group != Group::Flat);
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
    swapin_line(
        &mut out,
        "swapped in during interval (all procs sampled twice)",
        &a,
        &b,
        config.group == Group::App,
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
pub(crate) const TREND_WINDOW: usize = 10;
/// Intervals needed before GREW% and JETSAM_IN are shown.
const TREND_MIN: usize = 3;

/// (seconds since start, footprint) per process, oldest first, keyed by PID
/// and start time so a reused PID starts a fresh history.
pub(crate) type History = HashMap<(c_int, StartTime), VecDeque<(f64, u64)>>;

pub(crate) fn record(hist: &mut History, s: &SampleSet, t: f64) {
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

/// Ctrl-C, SIGTERM and SIGHUP end the loop cleanly, so the terminal is restored.
pub(crate) fn catch_stop_signals() {
    for sig in [SIGHUP, SIGINT, SIGTERM] {
        unsafe { signal(sig, request_stop as extern "C" fn(c_int) as usize) };
    }
}

/// What ended a wait.
pub(crate) enum Wake {
    /// the interval is over
    Tick,
    /// q, or a stop signal
    Quit,
    /// any other key
    Key(u8),
}

/// Waits until `deadline` (for ever while `paused`), watching for keys when
/// `keys` and for stop signals.
pub(crate) fn wait_until(deadline: Instant, paused: bool, keys: bool) -> Wake {
    while paused || Instant::now() < deadline {
        if STOP.load(Ordering::SeqCst) {
            return Wake::Quit;
        }
        let key = if keys {
            wait_key(100)
        } else {
            std::thread::sleep(Duration::from_millis(100));
            None
        };
        match key {
            Some(b'q' | b'Q') => return Wake::Quit,
            Some(k) => return Wake::Key(k),
            None => {}
        }
    }
    Wake::Tick
}

/// Draws text: in place (clipped to the window) on a terminal, else appended.
pub(crate) fn draw(text: &str, tty: bool) {
    let mut s = String::new();
    if tty {
        let height = term_rows().unwrap_or(usize::MAX);
        s.push_str("\x1b[H");
        for line in text.lines().take(height.saturating_sub(1)) {
            s.push_str(line);
            s.push_str("\x1b[K\n");
        }
        s.push_str("\x1b[J");
    } else {
        let _ = writeln!(s, "{text}");
    }
    emit_raw(&s);
}

/// Draws a frame under its status line.
fn show(status: &str, frame: &str, tty: bool) {
    draw(&format!("{status}\n{frame}"), tty);
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

/// What watch_frame draws besides the samples.
struct FrameExtras<'a> {
    rows: &'a [Row],
    alerts: &'a Alerts,
    firing: &'a [Firing],
    color: bool,
}

fn watch_frame(
    config: &Config,
    layout: &Layout,
    filter: &Filter,
    sys: (&SysMem, &SysMem),
    a: &SampleSet,
    b: &SampleSet,
    x: &FrameExtras,
) -> String {
    let mut out = String::new();
    summary(&mut out, sys.1, Some(sys.0));
    out.push('\n');
    render_table(
        &mut out,
        layout,
        x.rows,
        &mut ParentNames::new(b),
        config.show_parent,
        x.color,
    );
    let _ = writeln!(out);
    if x.alerts.active() {
        alert_lines(&mut out, config, x.alerts, x.firing, x.color);
    }
    swapin_line(
        &mut out,
        "swapped in during interval",
        a,
        b,
        config.group == Group::App,
    );
    churn_lines(&mut out, a, b, filter, x.color);
    let _ = writeln!(
        out,
        "GREW%: intervals with FOOTPRINT growth, of the last {TREND_WINDOW} (shown after {TREND_MIN}); 100% = sustained"
    );
    out
}

/// The firing rules, at most 8, in red on a color terminal.
fn alert_lines(out: &mut String, config: &Config, alerts: &Alerts, firing: &[Firing], color: bool) {
    const MAX: usize = 8;
    let hold = if config.alert_for > 0 {
        format!(", held {}s", config.alert_for)
    } else {
        String::new()
    };
    if firing.is_empty() {
        let _ = writeln!(out, "alerts: none firing{hold}");
        return;
    }
    let mut lines = vec![format!("ALERTS ({}){hold}:", firing.len())];
    for f in firing.iter().take(MAX) {
        let mark = if f.new { "new " } else { "" };
        lines.push(format!("  {mark}{}", alerts.describe(f)));
    }
    if firing.len() > MAX {
        lines.push(format!("  … +{} more", firing.len() - MAX));
    }
    for line in lines {
        if color {
            let _ = writeln!(out, "\x1b[31m{line}\x1b[0m");
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
}

/// One banner for this interval's new firings.
pub(crate) fn notify_new(alerts: &Alerts, firing: &[Firing]) {
    let new: Vec<String> = firing
        .iter()
        .filter(|f| f.new)
        .map(|f| alerts.describe(f))
        .collect();
    match new.len() {
        0 => {}
        1 => notify(&new[0]),
        n => notify(&format!("{n} alerts: {}", new.join("; "))),
    }
}

pub(crate) fn now_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    iso8601_utc(secs)
}

pub fn run_watch(config: &Config, filter: &Filter, interval: u64) {
    catch_stop_signals();
    let layout = Layout::new(config);
    let depth = config.group != Group::Flat;
    let mut recorder = config.record.as_deref().map(|path| {
        Recorder::open(path, &csv_header(&layout, depth, true)).unwrap_or_else(|err| {
            eprintln!("{err}");
            std::process::exit(2);
        })
    });
    let mut alerts = Alerts::new(&config.alerts, config.alert_for);

    // CSV streams to stdout as it is, terminal or not.
    let tty = !config.csv && unsafe { isatty(1) } == 1;
    // Raw keys only when output is a terminal too: if a pipe closed under us,
    // SIGPIPE would kill the process with the terminal still raw.
    let keys = tty && unsafe { isatty(0) } == 1;
    let screen = tty.then(|| Screen::enter(keys));
    let color = !config.csv && color_enabled();

    let t0 = Instant::now();
    let mut sys = SysMem::read();
    let mut prev = sample();
    let mut hist = History::new();
    record(&mut hist, &prev, 0.0);
    let mut frame = format!("waiting {interval}s for the first interval…\n");
    let mut paused = false;
    let mut tick = 0u64;
    if config.csv {
        emit_raw(&csv_header(&layout, depth, true));
    } else {
        show(&status_line(interval, tick, paused, keys), &frame, tty);
    }
    let mut status = 0;

    'run: loop {
        let deadline = Instant::now() + Duration::from_secs(interval);
        loop {
            match wait_until(deadline, paused, keys) {
                Wake::Tick => break,
                Wake::Quit => break 'run,
                Wake::Key(b' ') => {
                    paused = !paused;
                    show(&status_line(interval, tick, paused, keys), &frame, tty);
                }
                Wake::Key(_) => {}
            }
        }
        let sys_now = SysMem::read();
        let cur = sample();
        tick += 1;
        let t = t0.elapsed().as_secs_f64();
        record(&mut hist, &cur, t);

        // Every row: alerts and --record cover all of them, the screen the top N.
        let rows = arrange(
            interval_rows(&layout, filter, &prev, &cur, Some(&hist)),
            &layout,
            config,
        );
        let elapsed = cur.at.duration_since(prev.at).as_secs_f64();
        let firing = alerts.update(t, elapsed, &layout, &rows, sys_now.pressure());
        let stamp = now_utc();
        if let Some(rec) = recorder.as_mut() {
            let mut out = String::new();
            render_csv_rows(&mut out, &layout, &rows, depth, Some(&stamp));
            if let Err(err) = rec.write(&out) {
                eprintln!("{err}; recording stopped");
                recorder = None;
            }
        }
        let rows = cut(rows, config.topn);

        if config.csv {
            let mut out = String::new();
            render_csv_rows(&mut out, &layout, &rows, depth, Some(&stamp));
            emit_raw(&out);
            for f in firing.iter().filter(|f| f.new) {
                eprintln!("macswapi {stamp} ALERT {}", alerts.describe(f));
            }
        } else {
            let extras = FrameExtras {
                rows: &rows,
                alerts: &alerts,
                firing: &firing,
                color,
            };
            frame = watch_frame(
                config,
                &layout,
                filter,
                (&sys, &sys_now),
                &prev,
                &cur,
                &extras,
            );
            if tty && firing.iter().any(|f| f.new) {
                emit_raw("\x07");
            }
            show(&status_line(interval, tick, paused, keys), &frame, tty);
        }
        if config.notify {
            notify_new(&alerts, &firing);
        }
        sys = sys_now;
        prev = cur;
        if config.exit_on_alert && !firing.is_empty() {
            status = ALERT_EXIT;
            break 'run;
        }
    }

    if tty {
        drop(screen);
        // Leave the last frame on the normal screen.
        emit_raw(&frame);
    }
    if status != 0 {
        std::process::exit(status);
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
