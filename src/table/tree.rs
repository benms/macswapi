//! Tree mode: rows nested into process trees and rolled up over each subtree.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::os::raw::c_int;

use super::{sum_opt, Layout, Row};

/// Nests rows into process trees, each row rolled up over its subtree
/// (du-style), siblings in `order`. Children of launchd (pid 1) are roots:
/// rolling everything into launchd would bury every app. Keeps the first
/// `max_roots` trees (0 = all) and returns their rows with the tree count.
pub fn treeify(
    mut rows: Vec<Row>,
    layout: &Layout,
    order: &dyn Fn(&Row, &Row) -> Ordering,
    max_roots: usize,
) -> (Vec<Row>, usize) {
    let index: HashMap<c_int, usize> = rows.iter().enumerate().map(|(i, r)| (r.pid, i)).collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    let mut has_parent = vec![false; rows.len()];
    for (i, r) in rows.iter().enumerate() {
        if r.ppid <= 1 || r.ppid == r.pid {
            continue;
        }
        if let Some(&p) = index.get(&r.ppid) {
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
    let kept = if max_roots == 0 {
        roots.len()
    } else {
        max_roots.min(roots.len())
    };

    let mut slots: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
    let mut out = Vec::new();
    for &r in roots.iter().take(kept) {
        emit(r, 0, &children, &mut slots, &mut out);
    }
    (out, kept)
}

/// Adds each child's rolled-up values into row `i` (post-order). Columns that
/// don't add up are cleared on rows that have children.
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
    use crate::table::{row_order, tests::row, total, Col, SortBy};

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
        let (out, trees) = treeify(rows, &layout, &order, 0);
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
        let (out, trees) = treeify(rows, &layout, &order, 1);
        assert_eq!((out.len(), trees), (1, 1));
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
        let (out, _) = treeify(rows, &layout, &order, 0);
        assert_eq!(out.len(), 2);
    }
}
