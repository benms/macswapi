//! Snapshot mode (the default): one sample, with SWAP~ estimates.

use std::fmt::Write as _;

use crate::cli::{Config, Group};
use crate::format::human;
use crate::kernel::swap_usage;
use crate::sample::{sample, swap_denominator, swap_ratio, Filter};
use crate::summary::{summary, SysMem};
use crate::table::{
    arrange, cut, groups, render_csv, render_table, total, Col, Layout, ParentNames, Row, RowSrc,
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
    let sys = SysMem::read();

    // SWAP~ splits system swap in proportion to compressed memory, over all
    // the compressor holds (kernel_task and denied processes included), so a
    // missing sudo doesn't push their share onto the readable processes. N/A
    // if the swap total is unavailable or the denominator is 0. Without the
    // host's total the denominator is the readable processes alone, and one
    // of them may absorb all system swap; warn about that below.
    let readable = s.total_compressed();
    let held = sys.compressor_held();
    let denominator = swap_denominator(held, readable);
    let swap_per_cmprs = swap_ratio(swap_total, denominator);

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
    let rows = cut(arrange(rows, &layout, config), config.topn);

    let mut out = String::new();
    if config.csv {
        render_csv(&mut out, &layout, &rows, config.group != Group::Flat);
    } else {
        summary(&mut out, &sys, None);
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
        let trees = match config.group {
            Group::Flat => String::new(),
            Group::Tree => format!(" in {} trees", groups(&rows)),
            Group::App => format!(" in {} apps", groups(&rows)),
        };
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
        if held.is_some() {
            let rest = denominator - readable;
            let swap = swap_per_cmprs.map_or_else(|| "N/A".to_string(), |k| human(rest as f64 * k));
            let _ = writeln!(
                out,
                "SWAP~ split over the compressor's {}: readable procs {}, unattributed {} \
                 (kernel_task, {} denied) → SWAP~ {swap}",
                human(denominator as f64),
                human(readable as f64),
                human(rest as f64),
                s.denied,
            );
        }
        if swap_total.is_some() && swap_per_cmprs.is_none() {
            let why = if held.is_some() {
                "the compressor is empty"
            } else {
                "denominator is 0 — run as root for accurate estimates"
            };
            let _ = writeln!(out, "SWAP~ N/A: {why}");
        }
    }
    print!("{out}");

    if s.denied > 0 && swap_per_cmprs.is_some() && held.is_none() {
        eprintln!(
            "WARNING: {} proc(s) denied — SWAP~ estimates cover only visible processes; \
             a single visible process may absorb all system swap. Run as root for accuracy.",
            s.denied
        );
    }
}
