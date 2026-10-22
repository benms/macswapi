//! Snapshot mode (the default): one sample, with SWAP~ estimates.

use std::fmt::Write as _;

use crate::cli::Config;
use crate::kernel::swap_usage;
use crate::sample::{sample, swap_ratio, Filter};
use crate::summary::{summary, SysMem};
use crate::table::{
    arrange, render_csv, render_table, total, Col, Layout, ParentNames, Row, RowSrc,
};
use crate::term::color_enabled;

pub fn run_snapshot(config: &Config, filter: &Filter) {
    let swap_total = match swap_usage() {
        Ok(x) => Some(x.xsu_used),
        Err(err) => {
            eprintln!("WARNING: could not read sysctl vm.swapusage: {err}; SWAP~ unavailable.");
            None
        }
    };
    let s = sample();

    // SWAP~ estimates are proportional splits of system swap across readable
    // processes, shown as N/A if the system swap total is unavailable or the
    // denominator is 0. When denied > 0 the denominator is incomplete and one
    // visible process may absorb all system swap; warn about that below.
    let swap_per_cmprs = swap_ratio(swap_total, s.total_compressed());

    let layout = Layout::new(config);
    let rows: Vec<Row> = s
        .procs
        .values()
        .filter(|p| filter.keeps(p))
        .map(|p| {
            layout.row(&RowSrc {
                now: p,
                before: None,
                elapsed: 0.0,
                swap_per_cmprs,
                trend: None,
            })
        })
        .collect();
    let matched = rows.len();
    let (rows, trees) = arrange(rows, &layout, config);

    let mut out = String::new();
    if config.csv {
        render_csv(&mut out, &layout, &rows, config.tree);
    } else {
        summary(&mut out, &SysMem::read(), None);
        out.push('\n');
        render_table(
            &mut out,
            &layout,
            &rows,
            &mut ParentNames::new(&s),
            config.show_parent,
            color_enabled(),
        );

        let what = if filter.active() {
            format!(
                "{}/{matched} matching procs, of {} readable",
                rows.len(),
                s.procs.len()
            )
        } else {
            format!("{}/{matched} procs", rows.len())
        };
        let trees = trees.map_or_else(String::new, |t| format!(" in {t} trees"));
        let sudo_hint = if s.denied > 0 {
            " — need sudo for other users"
        } else {
            ""
        };
        let shown_total = |col: Col| col.display(total(&layout, &rows, col));
        let _ = writeln!(out);
        let _ = writeln!(out, "shown {what}{trees}  ({} denied{sudo_hint})", s.denied);
        let _ = writeln!(
            out,
            "shown totals: CMPRS {}  FOOTPRINT {}  SWAP~ {}  SWAPINS {}",
            shown_total(Col::Cmprs),
            shown_total(Col::Foot),
            shown_total(Col::SwapEst),
            shown_total(Col::Swapins),
        );
        if swap_total.is_some() && swap_per_cmprs.is_none() {
            let _ = writeln!(
                out,
                "SWAP~ N/A: denominator is 0 — run as root for accurate estimates"
            );
        }
    }
    print!("{out}");

    if s.denied > 0 && swap_per_cmprs.is_some() {
        eprintln!(
            "WARNING: {} proc(s) denied — SWAP~ estimates cover only visible processes; \
             a single visible process may absorb all system swap. Run as root for accuracy.",
            s.denied
        );
    }
}
