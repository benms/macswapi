//! Command line: the modes, options and their parsing.

use std::os::raw::c_int;

use crate::table::{Col, SortBy, ONCE_COLS, SNAPSHOT_COLS, WATCH_COLS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Snapshot,
    /// refresh every SECS until quit
    Watch(u64),
    /// --once: a single interval
    WatchOnce(u64),
    Detail(c_int),
}

impl Mode {
    pub fn cols(self) -> &'static [(Col, bool)] {
        match self {
            Mode::Snapshot | Mode::Detail(_) => SNAPSHOT_COLS,
            Mode::Watch(_) => WATCH_COLS,
            Mode::WatchOnce(_) => ONCE_COLS,
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Config {
    pub topn: usize,
    pub show_parent: bool,
    pub extended: bool,
    pub tree: bool,
    pub csv: bool,
    pub sort: Option<SortBy>,
    pub name: Option<String>,
    pub user: Option<String>,
    pub mode: Mode,
    pub help: bool,
}

impl Config {
    pub fn sort_by(&self) -> SortBy {
        self.sort.unwrap_or(match self.mode {
            Mode::Watch(_) | Mode::WatchOnce(_) => SortBy::Col(Col::Delta),
            Mode::Snapshot | Mode::Detail(_) => SortBy::Col(Col::Cmprs),
        })
    }
}

/// --sort keys: (key, snapshot column, watch column).
const SORT_KEYS: &[(&str, Option<Col>, Option<Col>)] = &[
    ("cmprs", Some(Col::Cmprs), None),
    ("swap", Some(Col::SwapEst), None),
    ("foot", Some(Col::Foot), Some(Col::Foot1)),
    ("swapins", Some(Col::Swapins), Some(Col::SwapinsGrowth)),
    ("peak", Some(Col::FootPeak), None),
    ("cpeak", Some(Col::CmprsPeak), None),
    ("decomp", Some(Col::Decomp), Some(Col::DecompGrowth)),
    ("headroom", Some(Col::Headroom), Some(Col::Headroom)),
    ("delta", None, Some(Col::Delta)),
    ("grew", None, Some(Col::Grew)),
];

fn parse_sort(key: &str, mode: Mode) -> Result<SortBy, String> {
    match key {
        "pid" => return Ok(SortBy::Pid),
        "name" => return Ok(SortBy::Name),
        _ => {}
    }
    let &(_, snapshot, interval) = SORT_KEYS
        .iter()
        .find(|(k, ..)| *k == key)
        .ok_or_else(|| format!("unknown sort key: {key} (see --help)"))?;
    let col = match mode {
        Mode::Snapshot | Mode::Detail(_) => snapshot,
        Mode::Watch(_) | Mode::WatchOnce(_) => interval,
    };
    col.filter(|c| mode.cols().iter().any(|(m, _)| m == c))
        .map(SortBy::Col)
        .ok_or_else(|| format!("--sort {key} doesn't apply in this mode"))
}

pub const USAGE: &str = "\
macswapi [N] [options] [-w|--watch [SECS] | --once [SECS] | --pid PID]
  N              top N rows (default 20, 0=all); with --tree, top N trees
  -p --parent    add PPID + parent-name columns
  -x --extended  add FOOT_PK, CMPRS_PK, DECOMP, HEADROOM
                 (watch: DECOMP, HEADROOM, and JETSAM_IN unless --once)
  -t --tree      process trees; each row rolled up over its subtree
     --sort KEY  cmprs swap foot swapins peak cpeak decomp headroom pid name
                 (watch: delta foot swapins decomp headroom grew pid name,
                 grew not with --once; default cmprs, watch delta)
     --name STR  only processes whose name contains STR (any case)
     --user USER only processes of USER (login name or uid)
     --csv       every column of the mode as CSV, raw numbers
                 (watch: a single interval, as with --once)
  -w --watch     leak watch: refresh every SECS (default 2s), GREW% flags
                 sustained growth; space pauses, q quits
                 (-l --live and --delta are aliases)
     --once      watch a single interval (default 2s), print it and exit
     --pid PID   everything macswapi can read about one process
  -h --help      this help
  SECS must be positive and immediately follow its flag; N must appear
  before the interval flag. example: macswapi 50 --watch 5
";

/// --watch / --once interval when SECS is not given.
const WATCH_SECS: u64 = 2;

pub fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut config = Config {
        topn: 20,
        show_parent: false,
        extended: false,
        tree: false,
        csv: false,
        sort: None,
        name: None,
        user: None,
        mode: Mode::Snapshot,
        help: false,
    };
    let (mut watch, mut once) = (false, false);
    let mut interval: Option<u64> = None;
    let mut pid: Option<c_int> = None;
    let mut sort: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-h" | "--help" => {
                config.help = true;
                return Ok(config);
            }
            "-p" | "--parent" => config.show_parent = true,
            "-x" | "--extended" => config.extended = true,
            "-t" | "--tree" => config.tree = true,
            "--csv" => config.csv = true,
            "--sort" | "--name" | "--user" | "--pid" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| format!("{arg} needs a value"))?
                    .clone();
                i += 1;
                match arg.as_str() {
                    "--sort" => sort = Some(value),
                    "--name" => config.name = Some(value),
                    "--user" => config.user = Some(value),
                    _ => {
                        let p = value.parse::<c_int>().ok().filter(|&p| p > 0);
                        pid =
                            Some(p.ok_or_else(|| {
                                format!("--pid needs a positive PID, got {value}")
                            })?);
                    }
                }
            }
            "-w" | "--watch" | "--delta" | "-l" | "--live" | "--once" => {
                let seen = if arg == "--once" {
                    &mut once
                } else {
                    &mut watch
                };
                if *seen {
                    return Err(format!(
                        "{arg} given twice (--delta and --live are --watch)"
                    ));
                }
                *seen = true;
                if let Some(Ok(s)) = args.get(i + 1).map(|a| a.parse::<u64>()) {
                    if s == 0 {
                        return Err("interval must be positive".to_string());
                    }
                    if interval.is_some() {
                        return Err("SECS given twice".to_string());
                    }
                    interval = Some(s);
                    i += 1;
                }
            }
            other => {
                if let Ok(topn) = other.parse::<usize>() {
                    if watch || once {
                        return Err("N must appear before the interval flag".to_string());
                    }
                    config.topn = topn;
                } else {
                    return Err(format!("unknown arg: {other}"));
                }
            }
        }
        i += 1;
    }

    let secs = interval.unwrap_or(WATCH_SECS);
    config.mode = match (pid, watch || once) {
        (Some(_), true) => return Err("--pid can't be combined with --watch/--once".to_string()),
        (Some(p), false) => Mode::Detail(p),
        // CSV is a single table, so it takes a single interval.
        (None, true) if once || config.csv => Mode::WatchOnce(secs),
        (None, true) => Mode::Watch(secs),
        (None, false) => Mode::Snapshot,
    };
    if matches!(config.mode, Mode::Detail(_))
        && (config.tree
            || config.csv
            || sort.is_some()
            || config.name.is_some()
            || config.user.is_some())
    {
        return Err(
            "--pid shows one process: no --tree, --csv, --sort, --name or --user".to_string(),
        );
    }
    config.sort = sort.map(|key| parse_sort(&key, config.mode)).transpose()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::Layout;

    fn parse(words: &[&str]) -> Result<Config, String> {
        let args: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        parse_args(&args)
    }

    #[test]
    fn test_parse_args_accepts_strict_interval_order() {
        assert_eq!(
            parse(&["20", "--delta", "5"]).unwrap(),
            Config {
                topn: 20,
                show_parent: false,
                extended: false,
                tree: false,
                csv: false,
                sort: None,
                name: None,
                user: None,
                mode: Mode::Watch(5),
                help: false,
            }
        );
    }

    #[test]
    fn test_parse_args_rejects_n_after_interval_flag() {
        assert!(parse(&["--delta", "5", "20"]).is_err());
    }

    #[test]
    fn test_parse_args_rejects_zero_interval() {
        assert!(parse(&["--watch", "0"]).is_err());
        assert!(parse(&["--live", "0"]).is_err());
    }

    #[test]
    fn test_parse_args_extended() {
        let config = parse(&["10", "-x", "--watch", "2"]).unwrap();
        assert!(config.extended);
        assert_eq!((config.topn, config.mode), (10, Mode::Watch(2)));
        assert!(parse(&["--extended"]).unwrap().extended);
    }

    #[test]
    fn test_parse_args_modes() {
        assert_eq!(parse(&[]).unwrap().mode, Mode::Snapshot);
        assert_eq!(parse(&["--watch"]).unwrap().mode, Mode::Watch(2));
        assert_eq!(parse(&["--live"]).unwrap().mode, Mode::Watch(2));
        assert_eq!(parse(&["5", "-l", "7"]).unwrap().mode, Mode::Watch(7));
        assert_eq!(parse(&["--once"]).unwrap().mode, Mode::WatchOnce(2));
        assert_eq!(
            parse(&["-w", "5", "--once"]).unwrap().mode,
            Mode::WatchOnce(5)
        );
        assert_eq!(
            parse(&["--once", "-w", "5"]).unwrap().mode,
            Mode::WatchOnce(5)
        );
        assert_eq!(parse(&["--csv", "-w"]).unwrap().mode, Mode::WatchOnce(2));
        assert_eq!(parse(&["--once", "5"]).unwrap().mode, Mode::WatchOnce(5));
        assert_eq!(parse(&["--csv"]).unwrap().mode, Mode::Snapshot);
        assert!(parse(&["-w", "3", "--once", "5"]).is_err());
        assert!(parse(&["--once", "--once"]).is_err());
        assert!(parse(&["--once", "0"]).is_err());
        assert!(parse(&["--once", "--sort", "foot", "20"]).is_err());
        assert_eq!(parse(&["--pid", "42"]).unwrap().mode, Mode::Detail(42));
        assert!(parse(&["--watch", "--live"]).is_err());
        assert!(parse(&["--pid", "0"]).is_err());
        assert!(parse(&["--pid", "abc"]).is_err());
        assert!(parse(&["--pid"]).is_err());
    }

    #[test]
    fn test_parse_args_filters_and_options() {
        let c = parse(&[
            "-t", "--csv", "--sort", "swapins", "--name", "Chrome", "--user", "501",
        ])
        .unwrap();
        assert!(c.tree && c.csv);
        assert_eq!(c.sort, Some(SortBy::Col(Col::Swapins)));
        assert_eq!(c.name.as_deref(), Some("Chrome"));
        assert_eq!(c.user.as_deref(), Some("501"));
        assert!(parse(&["--name"]).is_err());
    }

    #[test]
    fn test_parse_args_rejects_conflicts() {
        assert!(parse(&["--pid", "1", "--watch"]).is_err());
        assert!(parse(&["--pid", "1", "--tree"]).is_err());
        assert!(parse(&["--pid", "1", "--name", "x"]).is_err());
        assert!(parse(&["--pid", "1", "--once"]).is_err());
        assert!(parse(&["--sort", "delta"]).is_err()); // watch only
        assert!(parse(&["--sort", "bogus"]).is_err());
    }

    #[test]
    fn test_parse_sort_per_mode() {
        let col = |key, mode| parse_sort(key, mode);
        assert_eq!(
            col("swapins", Mode::Snapshot),
            Ok(SortBy::Col(Col::Swapins))
        );
        assert_eq!(
            col("swapins", Mode::Watch(3)),
            Ok(SortBy::Col(Col::SwapinsGrowth))
        );
        assert_eq!(col("foot", Mode::Watch(2)), Ok(SortBy::Col(Col::Foot1)));
        assert_eq!(col("grew", Mode::Watch(2)), Ok(SortBy::Col(Col::Grew)));
        assert!(col("grew", Mode::WatchOnce(3)).is_err());
        assert!(col("peak", Mode::Watch(3)).is_err());
        assert_eq!(col("name", Mode::Watch(3)), Ok(SortBy::Name));
        assert_eq!(col("pid", Mode::Snapshot), Ok(SortBy::Pid));
    }

    #[test]
    fn test_layout_shows_hidden_sort_column() {
        let plain = Layout::new(&parse(&[]).unwrap());
        assert_eq!(plain.visible.len(), 4);
        let sorted = Layout::new(&parse(&["--sort", "headroom"]).unwrap());
        let shown: Vec<Col> = sorted.visible.iter().map(|&i| sorted.cols[i]).collect();
        assert_eq!(shown.last(), Some(&Col::Headroom));
        let tree = Layout::new(&parse(&["-t", "-x"]).unwrap());
        assert_eq!(tree.cols[0], Col::Procs);
        assert_eq!(tree.visible.len(), tree.cols.len());
    }
}
