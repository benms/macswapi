//! Table output: aligned text, or CSV with every column of the mode.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::os::raw::c_int;

use super::{Col, Layout, Row};
use crate::format::{csv_field, csv_num, ident_cols, metric_cols};
use crate::kernel::proc_display_name;
use crate::sample::SampleSet;

/// Parent names for -p: from the sample when the parent is readable, else
/// looked up by PID (cached).
pub struct ParentNames<'a> {
    sample: &'a SampleSet,
    cache: HashMap<c_int, String>,
}

impl<'a> ParentNames<'a> {
    pub fn new(sample: &'a SampleSet) -> Self {
        ParentNames {
            sample,
            cache: HashMap::new(),
        }
    }

    fn get(&mut self, ppid: c_int) -> String {
        if let Some(p) = self.sample.procs.get(&ppid) {
            return p.name.clone();
        }
        self.cache
            .entry(ppid)
            .or_insert_with(|| proc_display_name(ppid))
            .clone()
    }
}

/// Text table; with `color`, rows that grew every interval are yellow.
pub fn render_table(
    out: &mut String,
    layout: &Layout,
    rows: &[Row],
    parents: &mut ParentNames,
    show_parent: bool,
    color: bool,
) {
    let headers: Vec<&str> = layout
        .visible
        .iter()
        .map(|&i| layout.cols[i].header())
        .collect();
    let _ = writeln!(
        out,
        "{}{}",
        ident_cols("PID", "NAME", show_parent.then_some(("PPID", "PARENT"))),
        metric_cols(&headers)
    );
    let grew = layout.index(Col::Grew);
    for r in rows {
        let name = format!("{}{}", "  ".repeat(r.depth), r.name);
        let pid = r.pid.to_string();
        let ident = if show_parent {
            let pname = parents.get(r.ppid);
            ident_cols(&pid, &name, Some((&r.ppid.to_string(), &pname)))
        } else {
            ident_cols(&pid, &name, None)
        };
        let cells: Vec<String> = layout
            .visible
            .iter()
            .map(|&i| layout.text(r, i).unwrap_or_default())
            .collect();
        let line = format!("{ident}{}", metric_cols(&cells));
        let line = line.trim_end(); // blank trend cells at the end
        let sustained = grew.is_some_and(|g| matches!(r.vals[g], Some(p) if p >= 100.0));
        if color && sustained {
            let _ = writeln!(out, "\x1b[33m{line}\x1b[0m");
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
}

/// The CSV header line: every column of the mode but TREND; `depth` in tree
/// and app modes, `time` first when streaming.
pub fn csv_header(layout: &Layout, depth: bool, time: bool) -> String {
    let mut head = Vec::new();
    if time {
        head.push("time");
    }
    head.extend(["pid", "ppid", "name"]);
    if depth {
        head.push("depth");
    }
    head.extend(
        layout
            .cols
            .iter()
            .filter(|c| c.in_csv())
            .map(|c| c.csv_name()),
    );
    format!("{}\n", head.join(","))
}

/// CSV rows, whole numbers, empty where N/A; each led by `time` if given.
pub fn render_csv_rows(
    out: &mut String,
    layout: &Layout,
    rows: &[Row],
    depth: bool,
    time: Option<&str>,
) {
    for r in rows {
        let mut fields = Vec::new();
        if let Some(t) = time {
            fields.push(t.to_string());
        }
        fields.extend([r.pid.to_string(), r.ppid.to_string(), csv_field(&r.name)]);
        if depth {
            fields.push(r.depth.to_string());
        }
        for (i, &col) in layout.cols.iter().enumerate() {
            if !col.in_csv() {
                continue;
            }
            let shown = !r.rollup || col.sums();
            fields.push(
                r.vals[i]
                    .filter(|_| shown)
                    .map_or_else(String::new, csv_num),
            );
        }
        let _ = writeln!(out, "{}", fields.join(","));
    }
}

/// CSV: the header, then the rows.
pub fn render_csv(out: &mut String, layout: &Layout, rows: &[Row], depth: bool) {
    out.push_str(&csv_header(layout, depth, false));
    render_csv_rows(out, layout, rows, depth, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::tests::row;

    #[test]
    fn test_render_csv() {
        let layout = Layout {
            cols: vec![Col::Cmprs, Col::Headroom, Col::Swapins],
            visible: vec![0],
        };
        let mut rollup = row(7, 1, &[Some(2048.0), None, None]);
        rollup.name = "a, \"b\"".to_string();
        rollup.rollup = true;
        let leaf = row(8, 7, &[Some(1024.4), Some(0.0), Some(-0.2)]);
        let mut out = String::new();
        render_csv(&mut out, &layout, &[rollup, leaf], false);
        assert_eq!(
            out,
            "pid,ppid,name,cmprs,headroom,swapins\n\
             7,1,\"a, \"\"b\"\"\",2048,,\n\
             8,7,p8,1024,0,0\n"
        );
    }

    #[test]
    fn test_csv_stream_skips_trend() {
        let layout = Layout {
            cols: vec![Col::Foot1, Col::Spark, Col::Grew],
            visible: vec![0, 1, 2],
        };
        assert_eq!(
            csv_header(&layout, true, true),
            "time,pid,ppid,name,depth,foot1,grew_pct\n"
        );
        let mut out = String::new();
        let r = row(3, 1, &[Some(10.0), None, Some(50.0)]);
        render_csv_rows(&mut out, &layout, &[r], false, Some("T"));
        assert_eq!(out, "T,3,1,p3,10,50\n");
    }
}
