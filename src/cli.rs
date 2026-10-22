//! Command line: the modes, options and their parsing.

use std::os::raw::c_int;

use crate::alert::{parse_rules, Rule};
use crate::completions::Shell;
use crate::table::{Col, SortBy, ONCE_COLS, SNAPSHOT_COLS, WATCH_COLS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Snapshot,
    /// refresh every SECS until quit
    Watch(u64),
    /// --once: a single interval
    WatchOnce(u64),
    Detail(c_int),
    /// --top: the dashboard, refreshed every SECS
    Top(u64),
}

impl Mode {
    pub fn cols(self) -> &'static [(Col, bool)] {
        match self {
            Mode::Snapshot | Mode::Detail(_) => SNAPSHOT_COLS,
            Mode::Watch(_) | Mode::Top(_) => WATCH_COLS,
            Mode::WatchOnce(_) => ONCE_COLS,
        }
    }
}

/// How rows nest: not at all, by parent (`--tree`) or by responsible app
/// (`--app`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Flat,
    Tree,
    App,
}

#[derive(Debug, PartialEq)]
pub struct Config {
    pub topn: usize,
    /// whether N was given (`--top` sizes its cards to the window otherwise)
    pub topn_given: bool,
    pub show_parent: bool,
    pub extended: bool,
    pub group: Group,
    pub csv: bool,
    pub sort: Option<SortBy>,
    pub name: Option<String>,
    pub user: Option<String>,
    pub mode: Mode,
    pub help: bool,
    /// `--completions SHELL`: print that script and exit
    pub completions: Option<Shell>,
    /// watch: `--alert` rules, and how long one must hold (`--for`)
    pub alerts: Vec<Rule>,
    pub alert_for: u64,
    pub exit_on_alert: bool,
    pub notify: bool,
    /// watch: append every interval's rows as CSV to this file
    pub record: Option<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            topn: 20,
            topn_given: false,
            show_parent: false,
            extended: false,
            group: Group::Flat,
            csv: false,
            sort: None,
            name: None,
            user: None,
            mode: Mode::Snapshot,
            help: false,
            completions: None,
            alerts: Vec::new(),
            alert_for: 0,
            exit_on_alert: false,
            notify: false,
            record: None,
        }
    }
}

impl Config {
    pub fn sort_by(&self) -> SortBy {
        self.sort.unwrap_or(match self.mode {
            Mode::Watch(_) | Mode::WatchOnce(_) | Mode::Top(_) => SortBy::Col(Col::Delta),
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
        Mode::Watch(_) | Mode::WatchOnce(_) | Mode::Top(_) => interval,
    };
    col.filter(|c| mode.cols().iter().any(|(m, _)| m == c))
        .map(SortBy::Col)
        .ok_or_else(|| format!("--sort {key} doesn't apply in this mode"))
}

pub const USAGE: &str = "\
macswapi [N] [options] [-w|--watch [SECS] | --once [SECS] | -T|--top [SECS] | --pid PID]
  N              top N rows (default 20, 0=all); with --tree/--app, top N groups;
                 with --top, rows in the apps card
  -p --parent    add PPID + parent-name columns
  -x --extended  add FOOT_PK, CMPRS_PK, DECOMP, HEADROOM
                 (watch: DECOMP, HEADROOM, and JETSAM_IN unless --once)
  -t --tree      process trees; each row rolled up over its subtree
  -a --app       group under the responsible app, XPC helpers included;
                 each app rolled up over its processes
     --sort KEY  cmprs swap foot swapins peak cpeak decomp headroom pid name
                 (watch: delta foot swapins decomp headroom grew pid name,
                 grew not with --once; default cmprs, watch delta)
     --name STR  only processes whose name contains STR (any case)
     --user USER only processes of USER (login name or uid)
     --csv       every column of the mode as CSV, raw numbers
                 (watch: streamed, a time column first)
  -w --watch     leak watch: refresh every SECS (default 2s), GREW% and
                 TREND show sustained growth; space pauses, q quits
                 (-l --live and --delta are aliases)
     --once      watch a single interval (default 2s), print it and exit
  -T --top       one-screen dashboard: memory, compressor, swap and pressure
                 cards, apps and growing processes; refreshed every SECS
                 (default 2s); space pauses, a: apps/processes, ? or h:
                 help, q quits
     --pid PID   everything macswapi can read about one process
     --completions SHELL  print a zsh, bash or fish completion script
  watch and top only:
     --alert RULES  fire when a rule holds for any process, e.g.
                 'foot>2G,headroom<200M,grew>=100,swapins>1M/s,rate>5M/s'
                 or 'pressure>=warn'; quote it for the shell
     --for SECS  a rule must hold this long to fire (default 0)
     --exit-on-alert  exit with status 3 at the first firing
     --notify    a Notification Center banner for new firings
     --record FILE  append every interval's rows (all of them) as CSV
                 (watch only)
  -h --help      this help
  SECS must be positive and immediately follow its flag; N must appear
  before the interval flag. example: macswapi 50 --watch 5
";

/// --watch / --once interval when SECS is not given.
const WATCH_SECS: u64 = 2;

pub fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut config = Config::default();
    let (mut watch, mut once, mut top) = (false, false, false);
    let (mut tree, mut app) = (false, false);
    let mut alert_for: Option<u64> = None;
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
            "--completions" => {
                let shell = args
                    .get(i + 1)
                    .ok_or("--completions needs zsh, bash or fish")?;
                config.completions = Some(Shell::parse(shell)?);
                return Ok(config);
            }
            "-p" | "--parent" => config.show_parent = true,
            "-x" | "--extended" => config.extended = true,
            "-t" | "--tree" => tree = true,
            "-a" | "--app" => app = true,
            "--csv" => config.csv = true,
            "--exit-on-alert" => config.exit_on_alert = true,
            "--notify" => config.notify = true,
            "--sort" | "--name" | "--user" | "--pid" | "--alert" | "--for" | "--record" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| format!("{arg} needs a value"))?
                    .clone();
                i += 1;
                match arg.as_str() {
                    "--sort" => sort = Some(value),
                    "--name" => config.name = Some(value),
                    "--user" => config.user = Some(value),
                    "--alert" => config.alerts.extend(parse_rules(&value)?),
                    "--for" => {
                        alert_for = Some(
                            value
                                .parse::<u64>()
                                .map_err(|_| format!("--for needs whole seconds, got {value}"))?,
                        );
                    }
                    "--record" => config.record = Some(value),
                    _ => {
                        let p = value.parse::<c_int>().ok().filter(|&p| p > 0);
                        pid =
                            Some(p.ok_or_else(|| {
                                format!("--pid needs a positive PID, got {value}")
                            })?);
                    }
                }
            }
            "-w" | "--watch" | "--delta" | "-l" | "--live" | "--once" | "-T" | "--top" => {
                let seen = match arg.as_str() {
                    "--once" => &mut once,
                    "-T" | "--top" => &mut top,
                    _ => &mut watch,
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
                    if watch || once || top {
                        return Err("N must appear before the interval flag".to_string());
                    }
                    config.topn = topn;
                    config.topn_given = true;
                } else {
                    return Err(format!("unknown arg: {other}"));
                }
            }
        }
        i += 1;
    }

    config.group = match (tree, app) {
        (true, true) => return Err("--tree and --app are two ways to group: pick one".to_string()),
        (true, false) => Group::Tree,
        (false, true) => Group::App,
        (false, false) => Group::Flat,
    };
    let secs = interval.unwrap_or(WATCH_SECS);
    if top && (watch || once || pid.is_some()) {
        return Err("--top is its own mode: no --watch, --once or --pid".to_string());
    }
    config.mode = match (pid, watch || once) {
        (Some(_), true) => return Err("--pid can't be combined with --watch/--once".to_string()),
        (Some(p), false) => Mode::Detail(p),
        (None, true) if once => Mode::WatchOnce(secs),
        (None, true) => Mode::Watch(secs),
        (None, false) if top => Mode::Top(secs),
        (None, false) => Mode::Snapshot,
    };
    if top
        && (config.show_parent
            || config.extended
            || config.group != Group::Flat
            || config.csv
            || sort.is_some()
            || config.record.is_some())
    {
        return Err(
            "--top draws its own cards: no -p, -x, --tree, --app, --csv, --sort or --record \
             (press a in it to switch apps/processes)"
                .to_string(),
        );
    }
    if matches!(config.mode, Mode::Detail(_))
        && (config.group != Group::Flat
            || config.csv
            || sort.is_some()
            || config.name.is_some()
            || config.user.is_some())
    {
        return Err(
            "--pid shows one process: no --tree, --app, --csv, --sort, --name or --user"
                .to_string(),
        );
    }
    if !config.alerts.is_empty() && !matches!(config.mode, Mode::Watch(_) | Mode::Top(_)) {
        return Err("--alert needs --watch or --top (not --once)".to_string());
    }
    if config.record.is_some() && !matches!(config.mode, Mode::Watch(_)) {
        return Err("--record needs --watch (not --once)".to_string());
    }
    if config.alerts.is_empty() && (alert_for.is_some() || config.exit_on_alert || config.notify) {
        return Err("--for, --exit-on-alert and --notify need --alert".to_string());
    }
    config.alert_for = alert_for.unwrap_or(0);
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
                topn_given: true,
                mode: Mode::Watch(5),
                ..Config::default()
            }
        );
    }

    #[test]
    fn test_parse_args_top() {
        assert_eq!(parse(&["-T"]).unwrap().mode, Mode::Top(2));
        assert_eq!(parse(&["--top", "5"]).unwrap().mode, Mode::Top(5));
        let c = parse(&["8", "-T", "--name", "chrome", "--alert", "foot>1G"]).unwrap();
        assert_eq!((c.topn, c.topn_given, c.mode), (8, true, Mode::Top(2)));
        assert!(!parse(&["-T"]).unwrap().topn_given);
        assert!(parse(&["-t"]).unwrap().mode == Mode::Snapshot); // -t stays --tree
        for bad in [
            &["-T", "-w"][..],
            &["-T", "--once"],
            &["-T", "--pid", "1"],
            &["-T", "-t"],
            &["-T", "-a"],
            &["-T", "--csv"],
            &["-T", "--sort", "foot"],
            &["-T", "-x"],
            &["-T", "-p"],
            &["-T", "--record", "x.csv"],
            &["-T", "0"],
            &["-T", "-T"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn test_parse_args_completions() {
        assert_eq!(
            parse(&["--completions", "fish"]).unwrap().completions,
            Some(Shell::Fish)
        );
        assert!(parse(&["--completions"]).is_err());
        assert!(parse(&["--completions", "tcsh"]).is_err());
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
        assert_eq!(parse(&["--csv", "-w"]).unwrap().mode, Mode::Watch(2)); // streams
        assert_eq!(
            parse(&["--csv", "--once"]).unwrap().mode,
            Mode::WatchOnce(2)
        );
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
        assert!(c.group == Group::Tree && c.csv);
        assert_eq!(parse(&["-a"]).unwrap().group, Group::App);
        assert_eq!(parse(&["--app", "-w"]).unwrap().group, Group::App);
        assert!(parse(&["-a", "-t"]).is_err());
        assert_eq!(c.sort, Some(SortBy::Col(Col::Swapins)));
        assert_eq!(c.name.as_deref(), Some("Chrome"));
        assert_eq!(c.user.as_deref(), Some("501"));
        assert!(parse(&["--name"]).is_err());
    }

    #[test]
    fn test_parse_args_rejects_conflicts() {
        assert!(parse(&["--pid", "1", "--watch"]).is_err());
        assert!(parse(&["--pid", "1", "--tree"]).is_err());
        assert!(parse(&["--pid", "1", "--app"]).is_err());
        assert!(parse(&["--pid", "1", "--name", "x"]).is_err());
        assert!(parse(&["--pid", "1", "--once"]).is_err());
        assert!(parse(&["--sort", "delta"]).is_err()); // watch only
        assert!(parse(&["--sort", "bogus"]).is_err());
    }

    #[test]
    fn test_parse_args_alerts_and_record() {
        let c = parse(&[
            "-w",
            "--alert",
            "foot>2G",
            "--alert",
            "grew>=100,pressure>=warn",
            "--for",
            "30",
            "--exit-on-alert",
            "--notify",
            "--record",
            "/tmp/x.csv",
        ])
        .unwrap();
        assert_eq!(c.alerts.len(), 3);
        assert_eq!(c.alert_for, 30);
        assert!(c.exit_on_alert && c.notify);
        assert_eq!(c.record.as_deref(), Some("/tmp/x.csv"));
        assert!(parse(&["--alert", "foot>2G"]).is_err()); // snapshot
        assert!(parse(&["-T", "--alert", "foot>2G", "--exit-on-alert"]).is_ok());
        assert!(parse(&["--once", "--alert", "foot>2G"]).is_err());
        assert!(parse(&["--record", "x.csv"]).is_err());
        assert!(parse(&["-w", "--for", "5"]).is_err()); // no rule
        assert!(parse(&["-w", "--notify"]).is_err());
        assert!(parse(&["-w", "--alert", "foot>2G", "--for", "-1"]).is_err());
        assert!(parse(&["-w", "--alert", "bogus"]).is_err());
        assert!(parse(&["--pid", "3", "--alert", "foot>1G"]).is_err());
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
