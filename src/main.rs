//! macswapi — per-process memory & swap inspector for macOS.
//!
//! macOS exposes no real per-process swap counter (the compressor swaps shared
//! segments, not PIDs). We report EXACT compressed/footprint from Mach
//! `task_info(TASK_VM_INFO)` and ESTIMATE per-proc swap by splitting the real
//! `vm.swapusage` total proportional to each proc's compressed memory.

mod alert;
mod cli;
mod completions;
mod detail;
mod ffi;
mod format;
mod kernel;
mod record;
mod sample;
mod snapshot;
mod summary;
mod table;
mod term;
mod top;
mod watch;

use cli::{parse_args, Mode, USAGE};
use detail::run_detail;
use ffi::{signal, SIGPIPE, SIG_DFL};
use sample::Filter;
use snapshot::run_snapshot;
use top::run_top;
use watch::{run_once, run_watch};

fn main() {
    // Restore default SIGPIPE so piping into `head`/`less` exits quietly
    // instead of panicking on a broken-pipe write (Rust ignores SIGPIPE).
    unsafe { signal(SIGPIPE, SIG_DFL) };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match parse_args(&args) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    };

    if config.help {
        print!("{USAGE}");
        return;
    }
    if let Some(shell) = config.completions {
        print!("{}", shell.script());
        return;
    }

    let filter = match Filter::from_config(&config) {
        Ok(filter) => filter,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };

    match config.mode {
        Mode::Snapshot => run_snapshot(&config, &filter),
        Mode::Watch(secs) => run_watch(&config, &filter, secs),
        Mode::WatchOnce(secs) => run_once(&config, &filter, secs),
        Mode::Detail(pid) => run_detail(pid),
        Mode::Top(secs) => run_top(&config, &filter, secs),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_plugin_version_matches_cargo() {
        let manifest = include_str!("../.claude-plugin/plugin.json");
        let version = format!("\"version\": \"{}\"", env!("CARGO_PKG_VERSION"));
        assert!(
            manifest.contains(&version),
            "bump .claude-plugin/plugin.json"
        );
    }
}
