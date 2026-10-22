//! Tree and app modes: rows nested into process trees, or under the app
//! responsible for them, and rolled up over each group.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::os::raw::c_int;

use super::{sum_opt, Layout, Row};

/// Nests rows, each rolled up over the rows under it (du-style), siblings in
/// `order`. Tree mode nests by parent; children of launchd (pid 1) are roots,
/// as rolling everything into launchd would bury every app. App mode
/// (`by_app`) nests under the responsible process instead, which brings in
/// the XPC helpers launchd started for an app. A row whose parent isn't
/// readable is a root.
pub fn treeify(
    mut rows: Vec<Row>,
    layout: &Layout,
    order: &dyn Fn(&Row, &Row) -> Ordering,
    by_app: bool,
) -> Vec<Row> {
    let index: HashMap<c_int, usize> = rows.iter().enumerate().map(|(i, r)| (r.pid, i)).collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    let mut has_parent = vec![false; rows.len()];
    for (i, r) in rows.iter().enumerate() {
        let parent = if by_app {
            r.resp
        } else {
            Some(r.ppid).filter(|&p| p > 1)
        };
        let Some(parent) = parent.filter(|&p| p != r.pid) else {
            continue;
        };
        if let Some(&p) = index.get(&parent) {
            children[p].push(i);
            has_parent[i] = true;
        }
    }

    let sums: Vec<bool> = layout.cols.iter().map(|c| c.sums()).collect();
    let mut roots: Vec<usize> = (0..rows.len()).filter(|&i| !has_parent[i]).collect();
    let mut visited = vec![false; rows.len()];
    for &r in &roots {
        rollup(r, &mut rows, &children, &sums, &mut visited);
    }
    // A PID-reuse race can leave a parent cycle no root reaches.
    for i in 0..rows.len() {
        if !visited[i] {
            roots.push(i);
            rollup(i, &mut rows, &children, &sums, &mut visited);
        }
    }

    for list in children.iter_mut() {
        list.sort_by(|&a, &b| order(&rows[a], &rows[b]));
    }
    roots.sort_by(|&a, &b| order(&rows[a], &rows[b]));

    let mut slots: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
    let mut out = Vec::new();
    for &r in &roots {
        emit(r, 0, &children, &mut slots, &mut out);
    }
    out
}

/// Adds `src` into `dst`, the two aligned at their ends (the latest sample),
/// `dst` padded with zeros in front if shorter.
fn add_aligned(dst: &mut Vec<u64>, src: &[u64]) {
    if src.len() > dst.len() {
        let pad = src.len() - dst.len();
        dst.splice(0..0, std::iter::repeat(0).take(pad));
    }
    let off = dst.len() - src.len();
    for (d, &v) in dst[off..].iter_mut().zip(src) {
        *d = d.saturating_add(v);
    }
}

/// Adds each child's rolled-up values and footprint history into row `i`
/// (post-order). Columns that don't add up are cleared on rows that have
/// children.
fn rollup(
    i: usize,
    rows: &mut [Row],
    children: &[Vec<usize>],
    sums: &[bool],
    visited: &mut [bool],
) {
    visited[i] = true;
    for &c in &children[i] {
        if visited[c] {
            continue;
        }
        rollup(c, rows, children, sums, visited);
        for (k, &adds) in sums.iter().enumerate() {
            if adds {
                let v = rows[c].vals[k];
                rows[i].vals[k] = sum_opt(rows[i].vals[k], v);
            }
        }
        let hist = std::mem::take(&mut rows[c].hist);
        add_aligned(&mut rows[i].hist, &hist);
        rows[c].hist = hist;
    }
    if !children[i].is_empty() {
        rows[i].rollup = true;
        for (k, &adds) in sums.iter().enumerate() {
            if !adds {
                rows[i].vals[k] = None;
            }
        }
    }
}

fn emit(
    i: usize,
    depth: usize,
    children: &[Vec<usize>],
    slots: &mut [Option<Row>],
    out: &mut Vec<Row>,
) {
    let Some(mut row) = slots[i].take() else {
        return;
    };
    row.depth = depth;
    out.push(row);
    for &c in &children[i] {
        emit(c, depth + 1, children, slots, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{cut, groups, row_order, tests::row, total, Col, SortBy};

    #[test]
    fn test_treeify_rolls_up_and_orders() {
        let layout = Layout {
            cols: vec![Col::Procs, Col::Cmprs, Col::FootPeak],
            visible: vec![0, 1, 2],
        };
        let p = |pid, ppid, cmprs: f64| row(pid, ppid, &[Some(1.0), Some(cmprs), Some(cmprs)]);
        let rows = vec![
            p(1, 0, 5.0),    // launchd: its children are roots
            p(10, 1, 10.0),  // app
            p(11, 10, 30.0), // helpers
            p(12, 10, 1.0),
            p(20, 1, 50.0),  // another app
            p(30, 999, 2.0), // parent not readable
        ];
        let order = row_order(&layout, SortBy::Col(Col::Cmprs));
        let out = treeify(rows, &layout, &order, false);
        let trees = groups(&out);
        let shape: Vec<(c_int, usize)> = out.iter().map(|r| (r.pid, r.depth)).collect();
        assert_eq!(
            shape,
            vec![(20, 0), (10, 0), (11, 1), (12, 1), (1, 0), (30, 0)]
        );
        assert_eq!(trees, 4);
        let app = &out[1];
        assert_eq!(app.vals, vec![Some(3.0), Some(41.0), None]); // peak can't add up
        assert!(app.rollup && !out[2].rollup);
        assert_eq!(layout.text(app, 2), None); // blank, not N/A
        assert_eq!(out[2].vals[2], Some(30.0)); // a leaf keeps its own peak

        let rows = vec![p(10, 1, 10.0), p(11, 10, 30.0), p(20, 1, 50.0)];
        let out = cut(treeify(rows, &layout, &order, false), 1);
        assert_eq!((out.len(), groups(&out)), (1, 1));
        assert_eq!(total(&layout, &out, Col::Cmprs), Some(50.0));
    }

    #[test]
    fn test_treeify_survives_parent_cycle() {
        let layout = Layout {
            cols: vec![Col::Cmprs],
            visible: vec![0],
        };
        let rows = vec![row(5, 6, &[Some(1.0)]), row(6, 5, &[Some(2.0)])];
        let order = row_order(&layout, SortBy::Pid);
        let out = treeify(rows, &layout, &order, false);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn test_treeify_by_app() {
        let layout = Layout {
            cols: vec![Col::Procs, Col::Foot1, Col::Spark],
            visible: vec![0, 1, 2],
        };
        let p = |pid, ppid, resp: Option<c_int>, foot: f64, hist: &[u64]| {
            let mut r = row(pid, ppid, &[Some(1.0), Some(foot), None]);
            r.resp = resp;
            r.hist = hist.to_vec();
            r
        };
        let rows = vec![
            p(10, 1, Some(10), 5.0, &[1, 2, 3]), // the app, responsible for itself
            p(11, 10, Some(10), 7.0, &[5, 5, 5]), // its child
            p(12, 1, Some(10), 9.0, &[4, 4]),    // XPC helper: parent launchd
            p(20, 1, None, 1.0, &[]),            // no answer: a root
            p(30, 1, Some(99), 2.0, &[]),        // responsible not readable
        ];
        let order = row_order(&layout, SortBy::Col(Col::Foot1));
        let out = treeify(rows, &layout, &order, true);
        let shape: Vec<(c_int, usize)> = out.iter().map(|r| (r.pid, r.depth)).collect();
        assert_eq!(shape, vec![(10, 0), (12, 1), (11, 1), (30, 0), (20, 0)]);
        assert_eq!(out[0].vals[..2], [Some(3.0), Some(21.0)]);
        assert_eq!(out[0].hist, vec![6, 11, 12]); // aligned at the latest sample
        assert_eq!(out[1].hist, vec![4, 4]); // members keep their own
                                             // tree mode leaves the helper out of the app
        let rows = vec![p(10, 1, Some(10), 5.0, &[]), p(12, 1, Some(10), 9.0, &[])];
        assert_eq!(groups(&treeify(rows, &layout, &order, false)), 2);
    }

    #[test]
    fn test_add_aligned() {
        let mut d = vec![1, 1];
        add_aligned(&mut d, &[5, 6, 7]);
        assert_eq!(d, vec![5, 7, 8]);
        add_aligned(&mut d, &[1]);
        assert_eq!(d, vec![5, 7, 9]);
    }
}
