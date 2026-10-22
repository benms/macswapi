//! The process table: its columns, rows computed from samples, sort order and
//! the cut to the top N. Tree and app modes nest rows in `tree`; `render`
//! prints them.

mod render;
mod tree;

pub use render::{csv_header, render_csv, render_csv_rows, render_table, ParentNames};

use std::cmp::Ordering;
use std::os::raw::c_int;

use crate::cli::{Config, Group};
use crate::format::{duration_str, headroom_str, human, human_signed, sparkline};
use crate::kernel::VmStats;
use crate::sample::{Proc, StartTime};
use tree::treeify;

/// A metric column. Each mode carries its own set (SNAPSHOT_COLS etc.).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Col {
    /// tree and app modes: processes in the group
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
    // watch: change over the interval
    Foot0,
    Foot1,
    Delta,
    Rate,
    SwapinsGrowth,
    DecompGrowth,
    // watch (not --once): footprint trend over the window
    Grew,
    JetsamEta,
    /// footprint over the window as a sparkline (text only, not in CSV)
    Spark,
}

// (column, shown without -x), in display order.
pub const SNAPSHOT_COLS: &[(Col, bool)] = &[
    (Col::Cmprs, true),
    (Col::Foot, true),
    (Col::SwapEst, true),
    (Col::Swapins, true),
    (Col::FootPeak, false),
    (Col::CmprsPeak, false),
    (Col::Decomp, false),
    (Col::Headroom, false),
];
pub const ONCE_COLS: &[(Col, bool)] = &[
    (Col::Foot0, true),
    (Col::Foot1, true),
    (Col::Delta, true),
    (Col::Rate, true),
    (Col::SwapinsGrowth, true),
    (Col::DecompGrowth, false),
    (Col::Headroom, false),
];
pub const WATCH_COLS: &[(Col, bool)] = &[
    (Col::Foot0, true),
    (Col::Foot1, true),
    (Col::Delta, true),
    (Col::Rate, true),
    (Col::SwapinsGrowth, true),
    (Col::DecompGrowth, false),
    (Col::Headroom, false),
    (Col::Grew, true),
    (Col::Spark, true),
    (Col::JetsamEta, false),
];

impl Col {
    pub fn header(self) -> &'static str {
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
            Col::Spark => "TREND",
        }
    }

    pub fn csv_name(self) -> &'static str {
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
            Col::Spark => "trend",
        }
    }

    /// Display text. None is N/A (the kernel lacks the field), except for the
    /// trend columns, where it means "not enough samples" and prints blank.
    pub fn display(self, v: Option<f64>) -> String {
        match (self, v) {
            (Col::Grew | Col::JetsamEta | Col::Spark, None) => String::new(),
            (_, None) => "N/A".to_string(),
            (Col::Headroom, Some(b)) => headroom_str(Some(b as u64)),
            (Col::Procs | Col::Decomp, Some(n)) => format!("{n:.0}"),
            (Col::DecompGrowth, Some(n)) => format!("+{n:.0}"),
            (Col::Delta | Col::Rate | Col::SwapinsGrowth, Some(b)) => human_signed(b),
            (Col::Grew, Some(pct)) => format!("{pct:.0}%"),
            (Col::JetsamEta, Some(secs)) => duration_str(secs),
            (Col::Spark, Some(_)) => String::new(),
            (_, Some(b)) => human(b),
        }
    }

    /// Whether tree rollup adds this column up; peaks, limits and trends don't.
    /// TREND does: a group's sparkline is its members' histories added up.
    pub fn sums(self) -> bool {
        !matches!(
            self,
            Col::FootPeak | Col::CmprsPeak | Col::Headroom | Col::Grew | Col::JetsamEta
        )
    }

    /// Whether CSV carries this column; the sparkline is text only.
    pub fn in_csv(self) -> bool {
        self != Col::Spark
    }
}

/// Footprint trend of one process in watch mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Trend {
    /// share of intervals in the window where the footprint grew, in percent
    pub grew_pct: Option<f64>,
    /// seconds to the jetsam limit at the window's average growth rate
    pub eta_secs: Option<f64>,
}

/// What a row's cells are computed from.
pub struct RowSrc<'a> {
    pub now: &'a Proc,
    /// watch: the same process in the previous sample
    pub before: Option<&'a Proc>,
    pub elapsed: f64,
    pub swap_per_cmprs: Option<f64>,
    pub trend: Option<Trend>,
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
        Col::Spark => None, // drawn from Row::hist
    }
}

pub struct Row {
    pub pid: c_int,
    pub ppid: c_int,
    pub start: Option<StartTime>,
    /// responsible process, which `--app` groups under
    pub resp: Option<c_int>,
    pub name: String,
    /// one value per `Layout::cols`
    pub vals: Vec<Option<f64>>,
    /// tree indent level
    pub depth: usize,
    /// tree and app modes: values cover the whole group
    pub rollup: bool,
    /// watch: footprint over the trend window, oldest first, for TREND
    pub hist: Vec<u64>,
}

/// The columns a table carries (`cols`: all of the mode's, as CSV prints) and
/// the ones it shows as text (`visible`: indices into `cols`).
pub struct Layout {
    pub cols: Vec<Col>,
    pub visible: Vec<usize>,
}

impl Layout {
    /// A sort column hidden without -x is shown anyway.
    pub fn new(config: &Config) -> Layout {
        let mut spec = config.mode.cols().to_vec();
        if config.group != Group::Flat {
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

    pub fn index(&self, col: Col) -> Option<usize> {
        self.cols.iter().position(|&c| c == col)
    }

    pub fn row(&self, src: &RowSrc) -> Row {
        Row {
            pid: src.now.pid,
            ppid: src.now.ppid,
            start: src.now.start,
            resp: src.now.resp,
            name: src.now.name.clone(),
            vals: self.cols.iter().map(|&c| cell(c, src)).collect(),
            depth: 0,
            rollup: false,
            hist: Vec::new(),
        }
    }

    /// Text of column `i` for `row`: blank where a rollup can't add up.
    pub fn text(&self, row: &Row, i: usize) -> Option<String> {
        let col = self.cols[i];
        if col == Col::Spark {
            return Some(sparkline(&row.hist));
        }
        (!row.rollup || col.sums()).then(|| col.display(row.vals[i]))
    }
}

/// What rows are ordered by: a column (largest first, except HEADROOM:
/// smallest first, as that is closest to a jetsam kill), PID, or name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortBy {
    Col(Col),
    Pid,
    Name,
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

/// Sorts rows; in tree and app modes, nests and rolls them up. Every row is
/// kept: `cut` takes the top N to show.
pub fn arrange(mut rows: Vec<Row>, layout: &Layout, config: &Config) -> Vec<Row> {
    let order = row_order(layout, config.sort_by());
    match config.group {
        Group::Flat => {
            rows.sort_by(&order);
            rows
        }
        Group::Tree => treeify(rows, layout, &order, false),
        Group::App => treeify(rows, layout, &order, true),
    }
}

/// The first `topn` rows (0 = all); nested rows go with their top-level row,
/// so in tree and app modes it keeps `topn` whole groups.
pub fn cut(mut rows: Vec<Row>, topn: usize) -> Vec<Row> {
    if topn > 0 {
        let mut top = 0;
        let end = rows
            .iter()
            .position(|r| {
                top += usize::from(r.depth == 0);
                top > topn
            })
            .unwrap_or(rows.len());
        rows.truncate(end);
    }
    rows
}

/// How many groups (top-level rows) `rows` holds.
pub fn groups(rows: &[Row]) -> usize {
    rows.iter().filter(|r| r.depth == 0).count()
}

/// Sum of `col` over top-level rows (tree mode: over the shown trees).
pub fn total(layout: &Layout, rows: &[Row], col: Col) -> Option<f64> {
    let i = layout.index(col)?;
    let mut top = rows.iter().filter(|r| r.depth == 0).peekable();
    if top.peek().is_none() {
        return Some(0.0);
    }
    top.map(|r| r.vals[i]).fold(None, sum_opt)
}

/// Adds two optional values; missing on one side keeps the other.
fn sum_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x + y),
        (x, None) => x,
        (None, y) => y,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn test_cmp_vals() {
        let mut v = vec![Some(0.0), Some(5.0), None, Some(1.0)];
        v.sort_by(|&a, &b| cmp_vals(Col::Headroom, a, b));
        assert_eq!(&v[..2], &[Some(1.0), Some(5.0)]); // closest to the limit first
        v.sort_by(|&a, &b| cmp_vals(Col::Cmprs, a, b));
        assert_eq!(v, vec![Some(5.0), Some(1.0), Some(0.0), None]);
    }

    pub(crate) fn row(pid: c_int, ppid: c_int, vals: &[Option<f64>]) -> Row {
        Row {
            pid,
            ppid,
            start: Some((pid as u64, 0)),
            resp: None,
            name: format!("p{pid}"),
            vals: vals.to_vec(),
            depth: 0,
            rollup: false,
            hist: Vec::new(),
        }
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
        assert_eq!(Col::Spark.display(None), "");
    }

    #[test]
    fn test_cut_keeps_whole_groups() {
        let mut rows: Vec<Row> = [(1, 0), (2, 1), (3, 1), (4, 0), (5, 0)]
            .iter()
            .map(|&(pid, depth)| {
                let mut r = row(pid, 0, &[]);
                r.depth = depth;
                r
            })
            .collect();
        assert_eq!(groups(&rows), 3);
        rows = cut(rows, 2);
        assert_eq!(rows.iter().map(|r| r.pid).collect::<Vec<_>>(), [1, 2, 3, 4]);
        assert_eq!(cut(rows, 0).len(), 4);
    }
}
