//! `--top` / `-T`: a one-screen memory dashboard in the manner of `mo status`:
//! memory, compressor, swap and pressure cards over the biggest apps and the
//! growing processes, two columns on a wide terminal, refreshed every interval.

use std::collections::{HashSet, VecDeque};
use std::os::raw::c_int;
use std::time::{Duration, Instant};

use crate::alert::{Alerts, Firing};
use crate::cli::{Config, Group, Mode};
use crate::ffi::isatty;
use crate::format::{duration_str, human, human_signed, sparkline};
use crate::sample::{sample, swap_denominator, Filter, SampleSet};
use crate::summary::{pressure_name, uptime_secs, SysMem};
use crate::table::{arrange, Col, Layout, Row, SortBy};
use crate::term::{color_enabled, emit_raw, term_cols, term_rows, Key, Screen};
use crate::watch::{
    catch_stop_signals, draw, interval_rows, notify_new, record, swapin_culprits, wait_until,
    History, Wake, ALERT_EXIT,
};

/// Points in the system sparklines.
const SPARK: usize = 14;
/// Wider than this, cards go in two columns.
const WIDE: usize = 80;
/// Rows the apps card starts from before shrinking to the window.
const APP_ROWS: usize = 10;
const GROW_ROWS: usize = 4;
/// Most processes listed under an opened app.
const OPEN_ROWS: usize = 8;
/// Sustained growth slower than this (bytes/s) stays out of the header.
const GROWER_MIN: f64 = 1024.0 * 1024.0;

const RESET: &str = "\x1b[0m";
const TITLE: &str = "\x1b[1;35m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BANNER: &str = "\x1b[1;30;43m";
const REVERSE: &str = "\x1b[7m";

/// Wraps text in an SGR color when color is on.
#[derive(Clone, Copy)]
struct Paint(bool);

impl Paint {
    fn p(self, code: &str, s: &str) -> String {
        if self.0 {
            format!("{code}{s}{RESET}")
        } else {
            s.to_string()
        }
    }
}

/// Visible width: characters outside escape sequences. Every glyph drawn here
/// takes one column.
fn width(s: &str) -> usize {
    let mut n = 0;
    let mut esc = false;
    for c in s.chars() {
        if esc {
            esc = !c.is_ascii_alphabetic();
        } else if c == '\x1b' {
            esc = true;
        } else {
            n += 1;
        }
    }
    n
}

/// `s` cut or padded to exactly `w` columns, escape sequences kept; a cut
/// line ends in "…".
fn fit(s: &str, w: usize) -> String {
    let n = width(s);
    if n <= w {
        return format!("{s}{}", " ".repeat(w - n));
    }
    let mut out = String::new();
    let (mut shown, mut esc, mut styled) = (0, false, false);
    for c in s.chars() {
        if esc {
            out.push(c);
            esc = !c.is_ascii_alphabetic();
        } else if c == '\x1b' {
            out.push(c);
            esc = true;
            styled = true;
        } else if shown + 1 < w {
            out.push(c);
            shown += 1;
        }
    }
    if w > 0 {
        out.push('…');
    }
    if styled {
        out.push_str(RESET);
    }
    out
}

/// "name pid" in `w` columns, the name cut first so the PID stays readable.
fn name_pid(name: &str, pid: i32, w: usize) -> String {
    let tail = format!(" {pid}");
    let room = w.saturating_sub(tail.len());
    let name = if width(name) > room {
        fit(name, room)
    } else {
        name.to_string()
    };
    fit(&format!("{name}{tail}"), w)
}

/// `frac` of `n` cells filled.
fn bar(frac: f64, n: usize, full: char, empty: char) -> String {
    let filled = ((frac.clamp(0.0, 1.0) * n as f64).round() as usize).min(n);
    let mut s: String = std::iter::repeat(full).take(filled).collect();
    s.extend(std::iter::repeat(empty).take(n - filled));
    s
}

/// Rates as block characters on a scale from 0 to their peak (at least
/// `floor`, so a trickle stays flat), right-aligned in `n` columns.
fn spark_zero(points: &VecDeque<f64>, floor: f64, n: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let peak = points.iter().copied().fold(floor, f64::max);
    let s: String = points
        .iter()
        .rev()
        .take(n)
        .rev()
        .map(|&v| BARS[((v.max(0.0) / peak * 7.0).round() as usize).min(7)])
        .collect();
    format!("{}{s}", " ".repeat(n.saturating_sub(s.chars().count())))
}

fn level_color(level: Option<i32>) -> &'static str {
    match level {
        Some(1) => GREEN,
        Some(2) => YELLOW,
        Some(_) => RED,
        None => DIM,
    }
}

/// Color for a share that is fine below `warn`, bad from `bad`.
fn share_color(frac: f64, warn: f64, bad: f64) -> &'static str {
    if frac >= bad {
        RED
    } else if frac >= warn {
        YELLOW
    } else {
        GREEN
    }
}

fn push<T>(q: &mut VecDeque<T>, v: T) {
    q.push_back(v);
    if q.len() > SPARK {
        q.pop_front();
    }
}

/// System history for the cards' sparklines, one point per interval.
#[derive(Default)]
struct SysHist {
    compressor: VecDeque<u64>,
    swapin: VecDeque<f64>,
    swapout: VecDeque<f64>,
    level: VecDeque<f64>,
}

impl SysHist {
    fn record(&mut self, before: &SysMem, now: &SysMem) {
        if let Some(b) = now.breakdown() {
            push(&mut self.compressor, b.compressor);
        }
        if let Some(r) = now.rates(before) {
            push(&mut self.swapin, r.swapin_bytes);
            push(&mut self.swapout, r.swapout_bytes);
        }
        if let Some(l) = now.level() {
            push(&mut self.level, f64::from(l));
        }
    }
}

/// PIDs of the apps (top-level rows) in `apps`, in order.
fn roots(apps: &[Row]) -> Vec<c_int> {
    apps.iter()
        .filter(|r| r.depth == 0)
        .map(|r| r.pid)
        .collect()
}

/// The app selected in the apps card and the apps opened to show their
/// processes, kept by PID so they follow the apps as rows re-sort.
#[derive(Default)]
struct Pick {
    /// the selected app, while a selection is on
    sel: Option<c_int>,
    /// its index when last seen, the selection's place once the app exits
    at: usize,
    open: HashSet<c_int>,
}

impl Pick {
    /// Index of the selected app in `roots`, or of the one now in its place.
    fn index(&self, roots: &[c_int]) -> Option<usize> {
        let pid = self.sel?;
        let last = roots.len().checked_sub(1)?;
        Some(
            roots
                .iter()
                .position(|&p| p == pid)
                .unwrap_or(self.at.min(last)),
        )
    }

    /// Follows the apps in a new sample: the selection moves to whichever app
    /// took the place of one that exited, and exited apps close.
    fn sync(&mut self, roots: &[c_int]) {
        if let Some(i) = self.index(roots) {
            self.sel = Some(roots[i]);
            self.at = i;
        }
        self.open.retain(|p| roots.contains(p));
    }

    /// Acts on a selection key; false if `k` isn't one.
    fn key(&mut self, k: Key, roots: &[c_int]) -> bool {
        self.sync(roots);
        let i = self.index(roots);
        match k {
            Key::Down | Key::Char(b'j') | Key::Up | Key::Char(b'k') => {
                let Some(last) = roots.len().checked_sub(1) else {
                    return true;
                };
                let down = matches!(k, Key::Down | Key::Char(b'j'));
                let j = match i {
                    None => 0,
                    Some(i) if down => (i + 1).min(last),
                    Some(i) => i.saturating_sub(1),
                };
                self.sel = Some(roots[j]);
                self.at = j;
            }
            Key::Char(b'\n' | b'\r') | Key::Right | Key::Left => {
                if let Some(i) = i {
                    let pid = roots[i];
                    match k {
                        Key::Right => {
                            self.open.insert(pid);
                        }
                        Key::Left => {
                            self.open.remove(&pid);
                        }
                        _ => {
                            if !self.open.remove(&pid) {
                                self.open.insert(pid);
                            }
                        }
                    }
                }
            }
            Key::Esc => self.sel = None,
            _ => return false,
        }
        true
    }
}

/// One interval's samples and the rows made from them.
struct Snap {
    sys0: SysMem,
    sys1: SysMem,
    a: SampleSet,
    b: SampleSet,
    /// rows grouped by app, largest footprint first
    apps: Vec<Row>,
    /// rows per process, largest footprint first
    procs: Vec<Row>,
    firing: Vec<Firing>,
}

/// What a frame is drawn with besides the samples.
struct Ctx<'a> {
    paint: Paint,
    width: usize,
    height: usize,
    layout: &'a Layout,
    hist: &'a SysHist,
    alerts: &'a Alerts,
    show_apps: bool,
    pick: &'a Pick,
    /// fixed apps-card rows (N given), else sized to the window
    rows: Option<usize>,
    interval: u64,
    tick: u64,
    paused: bool,
    keys: bool,
}

/// The parts of a Ctx that stay the same for the whole run.
struct Fixed<'a> {
    paint: Paint,
    tty: bool,
    layout: &'a Layout,
    rows: Option<usize>,
    interval: u64,
    keys: bool,
}

impl Fixed<'_> {
    /// A Ctx for now: the window is measured each time, so a resize shows at
    /// the next refresh. Piped, the frame is 100 columns and unclipped.
    fn ctx<'b>(
        &'b self,
        hist: &'b SysHist,
        alerts: &'b Alerts,
        show_apps: bool,
        pick: &'b Pick,
        tick: u64,
        paused: bool,
    ) -> Ctx<'b> {
        let (width, height) = if self.tty {
            (
                term_cols().unwrap_or(100),
                term_rows().unwrap_or(usize::MAX),
            )
        } else {
            (100, usize::MAX)
        };
        Ctx {
            paint: self.paint,
            width,
            height,
            layout: self.layout,
            hist,
            alerts,
            show_apps,
            pick,
            rows: self.rows,
            interval: self.interval,
            tick,
            paused,
            keys: self.keys,
        }
    }
}

struct Card {
    icon: &'static str,
    title: String,
    lines: Vec<String>,
}

fn render_card(c: &Card, w: usize, paint: Paint) -> Vec<String> {
    let head = format!("{} {}", c.icon, c.title);
    let rule = w.saturating_sub(width(&head) + 2);
    let mut out = vec![fit(
        &format!(
            "{}  {}",
            paint.p(TITLE, &head),
            paint.p(DIM, &"╌".repeat(rule))
        ),
        w,
    )];
    out.extend(c.lines.iter().map(|l| fit(l, w)));
    out
}

/// Cards in pairs side by side when the terminal is wide, else one per row;
/// a blank line between rows of cards.
fn lay_out(cards: &[Card], total: usize, paint: Paint) -> Vec<String> {
    let mut out = Vec::new();
    if total > WIDE {
        let cw = (total - 2) / 2;
        for (i, pair) in cards.chunks(2).enumerate() {
            if i > 0 {
                out.push(String::new());
            }
            let left = render_card(&pair[0], cw, paint);
            let right = pair
                .get(1)
                .map_or_else(Vec::new, |c| render_card(c, cw, paint));
            for k in 0..left.len().max(right.len()) {
                let l = left.get(k).cloned().unwrap_or_else(|| " ".repeat(cw));
                let r = right.get(k).map_or("", String::as_str);
                out.push(format!("{l}  {r}").trim_end().to_string());
            }
        }
    } else {
        for (i, c) in cards.iter().enumerate() {
            if i > 0 {
                out.push(String::new());
            }
            out.extend(
                render_card(c, total, paint)
                    .into_iter()
                    .map(|l| l.trim_end().to_string()),
            );
        }
    }
    out
}

/// The header's verdict: the most pressing thing, in this order.
fn diagnosis(
    pressure: Option<i32>,
    swapping: Option<&str>,
    alert: Option<&str>,
    grower: Option<(&str, f64)>,
) -> (String, &'static str) {
    if pressure == Some(4) {
        return ("memory pressure critical".to_string(), RED);
    }
    if let Some(who) = swapping {
        return (format!("swapping in: {who}"), YELLOW);
    }
    if let Some(a) = alert {
        return (format!("alert {a}"), RED);
    }
    if pressure == Some(2) {
        return ("memory pressure warn".to_string(), YELLOW);
    }
    if let Some((name, rate)) = grower {
        return (format!("{name} growing {}/s", human(rate)), YELLOW);
    }
    ("all clear".to_string(), GREEN)
}

/// Value of `col` in `row`, if the layout has it.
fn val(layout: &Layout, row: &Row, col: Col) -> Option<f64> {
    layout.index(col).and_then(|i| row.vals[i])
}

/// Processes that grew over the interval: sustained ones (GREW%) first, then
/// by rate.
fn growing<'r>(layout: &Layout, procs: &'r [Row]) -> Vec<&'r Row> {
    let mut out: Vec<&Row> = procs
        .iter()
        .filter(|r| val(layout, r, Col::Delta).is_some_and(|d| d > 0.0))
        .collect();
    let key = |r: &Row| {
        (
            val(layout, r, Col::Grew).unwrap_or(-1.0),
            val(layout, r, Col::Rate).unwrap_or(0.0),
        )
    };
    out.sort_by(|x, y| {
        let (gx, rx) = key(x);
        let (gy, ry) = key(y);
        gy.total_cmp(&gx).then(ry.total_cmp(&rx))
    });
    out
}

fn memory_card(sys: &SysMem, cw: usize, paint: Paint) -> Card {
    let mut lines = Vec::new();
    match sys.breakdown() {
        Some(bd) => {
            let parts = [
                ("Wired", bd.wired),
                ("Active", bd.active),
                ("Inact", bd.inactive),
                ("Cmprsd", bd.compressor),
                ("Free", bd.free),
            ];
            let total = sys
                .total()
                .unwrap_or_else(|| parts.iter().map(|p| p.1).sum())
                .max(1) as f64;
            let bw = cw.saturating_sub(23).clamp(6, 16);
            for (label, bytes) in parts {
                let frac = bytes as f64 / total;
                lines.push(format!(
                    "{label:<6} {} {:>5.1}% {:>8}",
                    paint.p(CYAN, &bar(frac, bw, '█', '░')),
                    100.0 * frac,
                    human(bytes as f64)
                ));
            }
        }
        None => lines.push("host_statistics64 unavailable".to_string()),
    }
    Card {
        icon: "◫",
        title: "Memory".to_string(),
        lines,
    }
}

fn compressor_card(snap: &Snap, hist: &SysHist, paint: Paint) -> Card {
    let sys = &snap.sys1;
    let mut lines = Vec::new();
    if let (Some(bd), Some(held)) = (sys.breakdown(), sys.compressor_held()) {
        let ratio = if bd.compressor > 0 {
            format!(" ({:.2}x)", held as f64 / bd.compressor as f64)
        } else {
            String::new()
        };
        lines.push(format!(
            "Holds  {} in {}{ratio}",
            human(held as f64),
            human(bd.compressor as f64)
        ));
        let pts: Vec<u64> = hist.compressor.iter().copied().collect();
        let spark = sparkline(&pts);
        let frac = bd.compressor as f64 / sys.total().unwrap_or(1).max(1) as f64;
        lines.push(format!(
            "Size   {}{spark} {} of RAM",
            " ".repeat(SPARK.saturating_sub(width(&spark))),
            paint.p(
                share_color(frac, 0.3, 0.5),
                &format!("{:.1}%", 100.0 * frac)
            )
        ));
        if let Some(r) = sys.rates(&snap.sys0) {
            lines.push(format!(
                "Rate   compress {:.0}/s · decomp {:.0}/s",
                r.compressions, r.decompressions
            ));
        }
        let readable = snap.b.total_compressed();
        let rest = swap_denominator(Some(held), readable) - readable;
        lines.push(format!(
            "Owner  procs {} · unattributed {}",
            human(readable as f64),
            human(rest as f64)
        ));
    } else {
        lines.push("host_statistics64 unavailable".to_string());
    }
    Card {
        icon: "◧",
        title: "Compressor".to_string(),
        lines,
    }
}

fn swap_card(snap: &Snap, hist: &SysHist, cw: usize, paint: Paint) -> Card {
    let sys = &snap.sys1;
    let mut lines = Vec::new();
    match sys.swap() {
        Some((0, 0)) => lines.push(format!("Used   {}", paint.p(GREEN, "none"))),
        Some((used, total)) => {
            let frac = used as f64 / total.max(1) as f64;
            let bw = cw.saturating_sub(30).clamp(6, 16);
            lines.push(format!(
                "Used   {} {:>5.1}% {}/{}",
                paint.p(share_color(frac, 0.5, 0.8), &bar(frac, bw, '█', '░')),
                100.0 * frac,
                human(used as f64),
                human(total as f64)
            ));
        }
        None => lines.push("Used   ?".to_string()),
    }
    let rates = sys.rates(&snap.sys0);
    for (label, q, now) in [
        ("In", &hist.swapin, rates.map(|r| r.swapin_bytes)),
        ("Out", &hist.swapout, rates.map(|r| r.swapout_bytes)),
    ] {
        let now = now.unwrap_or(0.0);
        let color = share_color(now, 1.0, 10.0 * 1024.0 * 1024.0);
        lines.push(format!(
            "{label:<6} {} {}",
            paint.p(color, &spark_zero(q, 1024.0 * 1024.0, SPARK)),
            paint.p(color, &format!("{}/s", human(now)))
        ));
    }
    let (_, who) = swapin_culprits(&snap.a, &snap.b, true, 2);
    lines.push(if who.is_empty() {
        format!("Now    {}", paint.p(DIM, "nobody swapping in"))
    } else {
        format!("Now    {}", paint.p(YELLOW, &who.join(", ")))
    });
    Card {
        icon: "⇅",
        title: "Swap".to_string(),
        lines,
    }
}

fn pressure_card(snap: &Snap, hist: &SysHist, paint: Paint) -> Card {
    let sys = &snap.sys1;
    let level = sys.pressure();
    let mut lines = vec![format!(
        "Level  {}",
        paint.p(
            level_color(level),
            &format!("● {}", level.map_or_else(|| "?".to_string(), pressure_name))
        )
    )];
    if let Some(avail) = sys.level() {
        let frac = f64::from(100 - avail.clamp(0, 100)) / 100.0;
        lines.push(format!(
            "Avail  {} {}",
            spark_zero(&hist.level, 100.0, SPARK),
            paint.p(share_color(frac, 0.7, 0.85), &format!("{avail}%"))
        ));
    }
    if let Some(bd) = sys.breakdown() {
        lines.push(format!(
            "Boot   swapped in {} · out {}",
            human(bd.swapped_in as f64),
            human(bd.swapped_out as f64)
        ));
    }
    let denied = if snap.b.denied > 0 {
        format!(" · {} denied, sudo for all", snap.b.denied)
    } else {
        String::new()
    };
    lines.push(format!("Read   {} procs{denied}", snap.b.procs.len()));
    Card {
        icon: "◉",
        title: "Pressure".to_string(),
        lines,
    }
}

/// One row of the apps card: name (with ×N for a group, else the PID),
/// footprint bar against `peak`, footprint and trend, in `w` columns.
fn app_line(layout: &Layout, r: &Row, peak: f64, indent: usize, w: usize, paint: Paint) -> String {
    let foot = val(layout, r, Col::Foot1).unwrap_or(0.0);
    let procs = val(layout, r, Col::Procs).unwrap_or(1.0);
    let right = format!(
        " {} {:>8} {}",
        paint.p(CYAN, &bar(foot / peak.max(1.0), 5, '▮', '▯')),
        human(foot),
        fit(&sparkline(&r.hist), 11)
    );
    let nw = w.saturating_sub(width(&right));
    let pad = "  ".repeat(indent);
    let name = if procs > 1.0 {
        fit(&format!("{pad}{} ×{procs:.0}", r.name), nw)
    } else {
        name_pid(&format!("{pad}{}", r.name), r.pid, nw)
    };
    format!("{name}{right}")
}

/// The apps card's rows from the selected app's: reversed with color,
/// else marked with "›".
fn selected_line(layout: &Layout, r: &Row, peak: f64, w: usize, paint: Paint) -> String {
    if paint.0 {
        let line = app_line(layout, r, peak, 0, w, Paint(false));
        paint.p(REVERSE, &fit(&line, w))
    } else {
        format!(
            "› {}",
            app_line(layout, r, peak, 0, w.saturating_sub(2), paint)
        )
    }
}

/// Apps (or processes) by footprint. In apps mode the list scrolls to keep
/// the selected app in view, and an opened app lists its processes under it,
/// their bars a share of the app.
fn apps_card(snap: &Snap, cx: &Ctx, cw: usize, n: usize) -> Card {
    let layout = cx.layout;
    let foot = |r: &Row| val(layout, r, Col::Foot1).unwrap_or(0.0);
    let mut lines = Vec::new();
    let mut title = String::from("Processes by footprint");
    if cx.show_apps {
        title = String::from("Apps by footprint");
        let apps = &snap.apps;
        let roots: Vec<usize> = (0..apps.len()).filter(|&i| apps[i].depth == 0).collect();
        let pids: Vec<c_int> = roots.iter().map(|&i| apps[i].pid).collect();
        let sel = cx.pick.index(&pids);
        let start = sel.map_or(0, |i| (i + 1).saturating_sub(n));
        let shown = &roots[start..(start + n).min(roots.len())];
        if start > 0 {
            title += &format!(
                " · {}–{} of {}",
                start + 1,
                start + shown.len(),
                roots.len()
            );
        }
        let peak = roots.first().map_or(1.0, |&i| foot(&apps[i]));
        for (k, &i) in shown.iter().enumerate() {
            let r = &apps[i];
            lines.push(if sel == Some(start + k) {
                selected_line(layout, r, peak, cw, cx.paint)
            } else {
                app_line(layout, r, peak, 0, cw, cx.paint)
            });
            if !cx.pick.open.contains(&r.pid) {
                continue;
            }
            let kids = apps[i + 1..].iter().take_while(|c| c.depth > 0);
            let total = kids.clone().count();
            for c in kids.take(OPEN_ROWS) {
                lines.push(app_line(layout, c, foot(r), c.depth, cw, cx.paint));
            }
            if total > OPEN_ROWS {
                let more = format!("  … +{} more", total - OPEN_ROWS);
                lines.push(cx.paint.p(DIM, &more));
            }
        }
    } else {
        let rows = &snap.procs[..n.min(snap.procs.len())];
        let peak = rows.first().map_or(1.0, foot);
        for r in rows {
            lines.push(app_line(layout, r, peak, 0, cw, cx.paint));
        }
    }
    if lines.is_empty() {
        lines.push(cx.paint.p(DIM, "no process matches"));
    }
    Card {
        icon: "❊",
        title,
        lines,
    }
}

fn growing_card(snap: &Snap, cx: &Ctx, cw: usize, n: usize) -> Card {
    let layout = cx.layout;
    let mut lines = Vec::new();
    for r in growing(layout, &snap.procs).into_iter().take(n) {
        let rate = val(layout, r, Col::Rate).unwrap_or(0.0);
        let grew = val(layout, r, Col::Grew).map_or_else(String::new, |g| format!("{g:.0}%"));
        let sustained = val(layout, r, Col::Grew).is_some_and(|g| g >= 100.0);
        let right = format!(
            " {:>10} {:>4} {}",
            format!("{}/s", human_signed(rate)),
            grew,
            fit(&sparkline(&r.hist), 11)
        );
        let left = name_pid(&r.name, r.pid, cw.saturating_sub(width(&right)));
        let line = format!("{left}{right}");
        lines.push(if sustained {
            cx.paint.p(YELLOW, &line)
        } else {
            line
        });
    }
    if lines.is_empty() {
        lines.push(cx.paint.p(DIM, "nothing grew this interval"));
    }
    let nearest = snap
        .procs
        .iter()
        .filter_map(|r| {
            val(layout, r, Col::Headroom)
                .filter(|&h| h > 0.0)
                .map(|h| (r, h))
        })
        .min_by(|x, y| x.1.total_cmp(&y.1));
    lines.push(match nearest {
        Some((r, h)) => format!("Jetsam nearest: {} {}, {} left", r.name, r.pid, human(h)),
        None => cx.paint.p(DIM, "Jetsam nearest: none with a limit"),
    });
    Card {
        icon: "▲",
        title: "Growing".to_string(),
        lines,
    }
}

fn header(snap: &Snap, cx: &Ctx) -> String {
    let sys = &snap.sys1;
    let p = cx.paint;
    // RAM and uptime go first when the line is too long: the verdict matters.
    let ram = sys.total().map(|t| format!("{} RAM", human(t as f64)));
    let up = uptime_secs().map(|u| format!("up {}", duration_str(u as f64)));
    let level = sys.pressure();
    let avail = sys.level().map_or_else(String::new, |l| format!(" ({l}%)"));
    let pressure = format!(
        "pressure {}{avail}",
        p.p(
            level_color(level),
            &format!("● {}", level.map_or_else(|| "?".to_string(), pressure_name))
        )
    );
    let (_, who) = swapin_culprits(&snap.a, &snap.b, true, 1);
    let alert = snap.firing.first().map(|f| cx.alerts.describe(f));
    let grow = growing(cx.layout, &snap.procs);
    let grower = grow
        .iter()
        .find(|r| {
            val(cx.layout, r, Col::Grew).is_some_and(|g| g >= 100.0)
                && val(cx.layout, r, Col::Rate).is_some_and(|v| v >= GROWER_MIN)
        })
        .map(|r| (r.name.as_str(), val(cx.layout, r, Col::Rate).unwrap_or(0.0)));
    let (text, color) = diagnosis(
        level,
        who.first().map(String::as_str),
        alert.as_deref(),
        grower,
    );
    let verdict = p.p(color, &text);
    let title = p.p(BOLD, "macswapi top");
    let line = |extra: &[&Option<String>]| {
        let mut parts = vec![title.clone()];
        parts.extend(extra.iter().filter_map(|x| x.as_ref()).cloned());
        parts.push(pressure.clone());
        parts.push(verdict.clone());
        parts.join(" · ")
    };
    [line(&[&ram, &up]), line(&[&ram]), line(&[])]
        .into_iter()
        .find(|l| width(l) <= cx.width)
        .unwrap_or_else(|| fit(&line(&[]), cx.width).trim_end().to_string())
}

fn footer(cx: &Ctx) -> String {
    let keys = match (cx.keys, cx.paused) {
        (true, false) => "space pause",
        (true, true) => "PAUSED · space resume",
        (false, _) => "Ctrl-C quit",
    };
    let more = match (cx.keys, cx.show_apps) {
        (true, true) => " · ↑↓ select · ⏎ open · a apps/processes · ? help · q quit",
        (true, false) => " · a apps/processes · ? help · q quit",
        (false, _) => "",
    };
    cx.paint.p(
        DIM,
        &format!(
            "{keys}{more} · every {}s · refresh #{}",
            cx.interval, cx.tick
        ),
    )
}

/// The `?` page: keys, the header's verdict, and what each card shows.
fn help_page(paint: Paint, width: usize) -> String {
    let head = |s: &str| paint.p(TITLE, s);
    let lines = [
        format!(
            "{}   {}",
            paint.p(BOLD, "macswapi top · help"),
            paint.p(DIM, "any key: back · q: quit")
        ),
        String::new(),
        head("Keys"),
        "  space  pause, resume      a  apps / processes      ? h  this help      q  quit"
            .to_string(),
        "  ↑↓ j k  select an app     ⏎ → ←  open, close its processes      esc  unselect"
            .to_string(),
        String::new(),
        head("Header verdict: the first that applies"),
        "  memory pressure critical › swapping in: who › an --alert firing ›".to_string(),
        "  pressure warn › a process growing every interval over 1M/s › all clear".to_string(),
        String::new(),
        head("Cards"),
        "  Memory      RAM by state. Low Free is normal: macOS keeps idle RAM as cache".to_string(),
        "  Compressor  what it holds, in how much RAM. Owner: readable processes, and".to_string(),
        "              unattributed (kernel_task, other users' processes without sudo)".to_string(),
        "  Swap        swap in use, swap-in and swap-out rates; Now: who swapped in".to_string(),
        "  Pressure    the kernel's level and % available over time; swap since boot".to_string(),
        "  Apps        footprint of apps with their helpers (×N processes); a: per proc"
            .to_string(),
        "              an opened app lists its processes, bars as a share of the app".to_string(),
        "  Growing     footprint growth this interval; yellow: grew every interval".to_string(),
        "              Jetsam nearest: least memory left before macOS kills a process".to_string(),
        String::new(),
        head("Marks"),
        "  ▮▮▯▯▯  footprint against the largest row    ▁▃▅█  last intervals, oldest left"
            .to_string(),
        "  ⚠ banner  an --alert rule firing; --exit-on-alert then exits with status 3".to_string(),
        String::new(),
        paint.p(
            DIM,
            "Without sudo only your own processes are read: sudo macswapi -T sees all.",
        ),
    ];
    lines
        .iter()
        .map(|l| fit(l, width).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The whole frame with `n` apps rows and `g` growing rows.
fn frame_with(snap: &Snap, cx: &Ctx, n: usize, g: usize) -> Vec<String> {
    let cw = if cx.width > WIDE {
        (cx.width - 2) / 2
    } else {
        cx.width
    };
    let mut out = vec![header(snap, cx)];
    if let Some(first) = snap.firing.first() {
        let more = if snap.firing.len() > 1 {
            format!(" (+{} more)", snap.firing.len() - 1)
        } else {
            String::new()
        };
        let text = format!(" ⚠ ALERT {}{more} ", cx.alerts.describe(first));
        out.push(cx.paint.p(BANNER, &fit(&text, width(&text).min(cx.width))));
    }
    out.push(String::new());
    let cards = [
        memory_card(&snap.sys1, cw, cx.paint),
        compressor_card(snap, cx.hist, cx.paint),
        swap_card(snap, cx.hist, cw, cx.paint),
        pressure_card(snap, cx.hist, cx.paint),
        apps_card(snap, cx, cw, n),
        growing_card(snap, cx, cw, g),
    ];
    out.extend(lay_out(&cards, cx.width, cx.paint));
    out.push(String::new());
    out.push(footer(cx));
    out
}

/// The frame, its process cards shrunk until it fits the window.
fn frame(snap: &Snap, cx: &Ctx) -> String {
    let (mut n, mut g) = (cx.rows.unwrap_or(APP_ROWS), GROW_ROWS);
    loop {
        let lines = frame_with(snap, cx, n, g);
        let fits = lines.len() < cx.height;
        let can_shrink_apps = cx.rows.is_none() && n > 2;
        if fits || (!can_shrink_apps && g <= 1) {
            return lines.join("\n");
        }
        if can_shrink_apps && (n > g + 2 || g <= 1) {
            n -= 1;
        } else {
            g -= 1;
        }
    }
}

pub fn run_top(config: &Config, filter: &Filter, interval: u64) {
    catch_stop_signals();
    let tty = unsafe { isatty(1) } == 1;
    let keys = tty && unsafe { isatty(0) } == 1;
    let paint = Paint(color_enabled());
    let by = |group| Config {
        group,
        sort: Some(SortBy::Col(Col::Foot1)),
        mode: Mode::Top(interval),
        ..Config::default()
    };
    let (app_cfg, flat_cfg) = (by(Group::App), by(Group::Flat));
    // Grouped, so it carries PROCS; per-process rows read 1 there.
    let layout = Layout::new(&app_cfg);
    let mut alerts = Alerts::new(&config.alerts, config.alert_for);
    let rows = config.topn_given.then_some(config.topn).filter(|&n| n > 0);

    let screen = tty.then(|| Screen::enter(keys));
    let t0 = Instant::now();
    let mut first = Some((SysMem::read(), sample()));
    let mut hist = History::new();
    if let Some((_, s)) = &first {
        record(&mut hist, s, 0.0);
    }
    let mut sys_hist = SysHist::default();
    let mut last: Option<Snap> = None;
    let (mut paused, mut show_apps, mut help, mut tick) = (false, true, false, 0u64);
    let mut pick = Pick::default();
    let mut text = format!("macswapi top · waiting {interval}s for the first interval…");
    if tty {
        draw(&text, tty);
    }
    let mut status = 0;

    let fixed = Fixed {
        paint,
        tty,
        layout: &layout,
        rows,
        interval,
        keys,
    };

    'run: loop {
        let deadline = Instant::now() + Duration::from_secs(interval);
        loop {
            match wait_until(deadline, paused, keys) {
                Wake::Tick => break,
                Wake::Quit => break 'run,
                Wake::Key(k) => {
                    match k {
                        // On the help page any key goes back (q quits).
                        _ if help => help = false,
                        Key::Char(b'?' | b'h' | b'H') => help = true,
                        Key::Char(b' ') => paused = !paused,
                        Key::Char(b'a' | b'A') => show_apps = !show_apps,
                        k if show_apps
                            && last.as_ref().is_some_and(|s| pick.key(k, &roots(&s.apps))) => {}
                        _ => continue,
                    }
                    if help {
                        let width = fixed
                            .ctx(&sys_hist, &alerts, show_apps, &pick, tick, paused)
                            .width;
                        draw(&help_page(paint, width), tty);
                    } else if let Some(snap) = &last {
                        let cx = fixed.ctx(&sys_hist, &alerts, show_apps, &pick, tick, paused);
                        text = frame(snap, &cx);
                        draw(&text, tty);
                    } else {
                        draw(&text, tty);
                    }
                }
            }
        }

        let (sys0, a) = match (last.take(), first.take()) {
            (Some(s), _) => (s.sys1, s.b),
            (None, Some(f)) => f,
            (None, None) => unreachable!(),
        };
        let sys1 = SysMem::read();
        let b = sample();
        tick += 1;
        let t = t0.elapsed().as_secs_f64();
        record(&mut hist, &b, t);
        sys_hist.record(&sys0, &sys1);
        let apps = arrange(
            interval_rows(&layout, filter, &a, &b, Some(&hist)),
            &layout,
            &app_cfg,
        );
        let procs = arrange(
            interval_rows(&layout, filter, &a, &b, Some(&hist)),
            &layout,
            &flat_cfg,
        );
        pick.sync(&roots(&apps));
        let elapsed = b.at.duration_since(a.at).as_secs_f64();
        let firing = alerts.update(t, elapsed, &layout, &apps, sys1.pressure());
        let snap = Snap {
            sys0,
            sys1,
            a,
            b,
            apps,
            procs,
            firing,
        };
        let cx = fixed.ctx(&sys_hist, &alerts, show_apps, &pick, tick, paused);
        text = frame(&snap, &cx);
        // Samples go on behind the help page, so the cards are current on return.
        if help {
            draw(&help_page(paint, cx.width), tty);
        } else {
            draw(&text, tty);
        }
        if tty && snap.firing.iter().any(|f| f.new) {
            emit_raw("\x07");
        }
        if config.notify {
            notify_new(&alerts, &snap.firing);
        }
        let fired = !snap.firing.is_empty();
        last = Some(snap);
        if config.exit_on_alert && fired {
            status = ALERT_EXIT;
            break;
        }
        if !tty {
            break; // piped: one frame
        }
    }

    if tty {
        drop(screen);
        // Leave the last frame on the normal screen.
        emit_raw(&format!("{text}\n"));
    }
    if status != 0 {
        std::process::exit(status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::raw::c_int;

    #[test]
    fn test_width_and_fit() {
        let red = Paint(true).p(RED, "abc");
        assert_eq!(width(&red), 3);
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abcdef", 0), "");
        let cut = fit(&format!("{red}def"), 4);
        assert_eq!(width(&cut), 4);
        assert!(cut.ends_with(RESET)); // color never bleeds past a cut
        assert_eq!(fit("█▁●", 3), "█▁●");
    }

    #[test]
    fn test_name_pid_keeps_pid() {
        assert_eq!(
            name_pid("Google Chrome Helper", 21686, 16),
            "Google Ch… 21686"
        );
        assert_eq!(width(&name_pid("Google Chrome Helper", 21686, 16)), 16);
        assert_eq!(name_pid("zsh", 7, 8), "zsh 7   ");
    }

    #[test]
    fn test_help_page_fits_80_columns() {
        let page = help_page(Paint(false), 200);
        assert!(page.lines().count() < 30, "fits a 30-row window");
        for line in page.lines() {
            assert!(width(line) <= 80, "{} columns: {line}", width(line));
        }
        assert!(page.contains("? h  this help"));
        // narrower terminals cut lines instead of wrapping them
        assert!(help_page(Paint(true), 40).lines().all(|l| width(l) <= 40));
    }

    #[test]
    fn test_bars() {
        assert_eq!(bar(0.5, 4, '█', '░'), "██░░");
        assert_eq!(bar(1.7, 3, '▮', '▯'), "▮▮▮");
        assert_eq!(bar(-1.0, 2, '▮', '▯'), "▯▯");
        let q: VecDeque<f64> = [0.0, 50.0, 100.0].into_iter().collect();
        assert_eq!(spark_zero(&q, 100.0, 5), "  ▁▅█");
        let trickle: VecDeque<f64> = [0.0, 10.0].into_iter().collect();
        assert_eq!(spark_zero(&trickle, 1e6, 2), "▁▁"); // under the floor
    }

    #[test]
    fn test_diagnosis_order() {
        let d = |p, s, a, g| diagnosis(p, s, a, g).0;
        assert_eq!(
            d(Some(4), Some("Xcode 1 90%"), Some("x"), None),
            "memory pressure critical"
        );
        assert_eq!(
            d(Some(2), Some("Xcode 1 90%"), Some("x"), None),
            "swapping in: Xcode 1 90%"
        );
        assert_eq!(
            d(Some(2), None, Some("foot>1G: a"), None),
            "alert foot>1G: a"
        );
        assert_eq!(
            d(Some(2), None, None, Some(("Code", 1024.0))),
            "memory pressure warn"
        );
        assert_eq!(
            d(Some(1), None, None, Some(("Code", 1024.0))),
            "Code growing 1.00K/s"
        );
        assert_eq!(d(Some(1), None, None, None), "all clear");
    }

    fn card(title: &str, n: usize) -> Card {
        Card {
            icon: "◫",
            title: title.to_string(),
            lines: (0..n).map(|i| format!("line {i}")).collect(),
        }
    }

    #[test]
    fn test_lay_out_columns() {
        let cards = [card("A", 2), card("B", 1), card("C", 1)];
        let wide = lay_out(&cards, 100, Paint(false));
        // A|B side by side (3 lines), blank, C alone (2 lines)
        assert_eq!(wide.len(), 6);
        assert!(wide[0].starts_with("◫ A") && wide[0].contains("◫ B"));
        assert!(wide[2].starts_with("line 1") && !wide[2].contains("line 0"));
        assert!(wide.iter().all(|l| width(l) <= 100));
        let narrow = lay_out(&cards, 60, Paint(false));
        assert_eq!(narrow.len(), 3 + 1 + 2 + 1 + 2); // one column
        assert!(narrow.iter().all(|l| width(l) <= 60));
    }

    #[test]
    fn test_pick_follows_apps() {
        let mut pick = Pick::default();
        assert!(pick.key(Key::Down, &[10, 20, 30])); // first move: top app
        assert_eq!(pick.sel, Some(10));
        pick.key(Key::Char(b'j'), &[10, 20, 30]);
        pick.key(Key::Down, &[10, 20, 30]);
        pick.key(Key::Down, &[10, 20, 30]); // stays on the last
        assert_eq!(pick.sel, Some(30));
        pick.key(Key::Char(b'\n'), &[10, 20, 30]);
        assert!(pick.open.contains(&30));
        // re-sorted: the selection follows the app
        assert_eq!(pick.index(&[30, 10, 20]), Some(0));
        pick.sync(&[30, 10, 20]);
        pick.key(Key::Up, &[30, 10, 20]);
        assert_eq!(pick.sel, Some(30)); // stays on the first
                                        // the app exits: its place is selected and it closes
        pick.sync(&[10, 20]);
        assert_eq!((pick.sel, pick.open.len()), (Some(10), 0));
        pick.key(Key::Right, &[10, 20]);
        assert!(pick.open.contains(&10));
        pick.key(Key::Left, &[10, 20]);
        assert!(pick.open.is_empty());
        pick.key(Key::Esc, &[10, 20]);
        assert_eq!(pick.index(&[10, 20]), None);
        assert!(!pick.key(Key::Char(b'x'), &[10, 20]));
    }

    #[test]
    fn test_apps_card_opens_and_scrolls() {
        let layout = Layout {
            cols: vec![Col::Procs, Col::Foot1],
            visible: vec![0, 1],
        };
        let r = |pid, depth, procs: f64, foot: f64| {
            let mut row = crate::table::tests::row(pid, 1, &[Some(procs), Some(foot)]);
            row.depth = depth;
            row
        };
        let mut apps = vec![r(1, 0, 11.0, 1100.0)];
        apps.extend((0..10).map(|k| r(100 + k, 1, 1.0, 100.0)));
        apps.extend((2..6).map(|pid| r(pid, 0, 1.0, 10.0)));
        let sys = SysMem::read();
        let set = || SampleSet {
            procs: std::collections::HashMap::new(),
            denied: 0,
            at: Instant::now(),
        };
        let snap = Snap {
            sys0: SysMem::read(),
            sys1: sys,
            a: set(),
            b: set(),
            apps,
            procs: Vec::new(),
            firing: Vec::new(),
        };
        let (hist, alerts) = (SysHist::default(), Alerts::new(&[], 0));
        let mut pick = Pick::default();
        let card_of = |pick: &Pick| {
            let cx = Ctx {
                paint: Paint(false),
                width: 60,
                height: usize::MAX,
                layout: &layout,
                hist: &hist,
                alerts: &alerts,
                show_apps: true,
                pick,
                rows: None,
                interval: 2,
                tick: 1,
                paused: false,
                keys: true,
            };
            apps_card(&snap, &cx, 60, 3)
        };
        let card = card_of(&pick);
        assert_eq!(card.lines.len(), 3);
        assert!(card.lines[0].starts_with("p1 ×11"));

        let roots = roots(&snap.apps);
        pick.key(Key::Down, &roots);
        pick.key(Key::Char(b'\n'), &roots);
        let card = card_of(&pick);
        assert!(card.lines[0].starts_with("› p1 ×11"));
        assert!(card.lines[1].starts_with("  p100 100"));
        assert_eq!(card.lines[OPEN_ROWS + 1], "  … +2 more");
        assert!(card.lines[OPEN_ROWS + 2].starts_with("p2 2"));
        assert_eq!(card.lines.len(), 1 + OPEN_ROWS + 1 + 2);
        assert!(card.lines.iter().all(|l| width(l) <= 60));

        // the list scrolls to keep the selection in view
        for _ in 0..4 {
            pick.key(Key::Down, &roots);
        }
        let card = card_of(&pick);
        assert_eq!(card.title, "Apps by footprint · 3–5 of 5");
        assert!(card.lines[2].starts_with("› p5 5"));
    }

    #[test]
    fn test_growing_order() {
        let layout = Layout {
            cols: vec![Col::Delta, Col::Rate, Col::Grew],
            visible: vec![0, 1, 2],
        };
        let r = |pid, delta: f64, rate: f64, grew: Option<f64>| {
            crate::table::tests::row(pid, 1, &[Some(delta), Some(rate), grew])
        };
        let rows = [
            r(1, 10.0, 5.0, Some(50.0)),
            r(2, 10.0, 9.0, None),         // too young for GREW%
            r(3, -4.0, -2.0, Some(100.0)), // shrank: left out
            r(4, 1.0, 1.0, Some(100.0)),
        ];
        let pids: Vec<c_int> = growing(&layout, &rows).iter().map(|r| r.pid).collect();
        assert_eq!(pids, vec![4, 1, 2]);
    }
}
