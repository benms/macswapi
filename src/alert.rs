//! `--alert`: rules checked against every row (not just the top N) at each
//! watch interval. A rule that holds for `--for` seconds on end fires.

use std::collections::{HashMap, HashSet};
use std::os::raw::c_int;

use crate::format::{duration_str, human};
use crate::sample::StartTime;
use crate::summary::pressure_name;
use crate::table::{Col, Layout, Row};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    /// FOOT1: footprint now
    Foot,
    /// RATE/s: footprint growth per second
    Rate,
    /// bytes swapped in per second over the interval
    Swapins,
    /// GREW%
    Grew,
    /// HEADROOM, for processes that have a limit
    Headroom,
    /// system memory pressure level
    Pressure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Gt,
    Ge,
    Lt,
    Le,
}

impl Op {
    fn holds(self, v: f64, limit: f64) -> bool {
        match self {
            Op::Gt => v > limit,
            Op::Ge => v >= limit,
            Op::Lt => v < limit,
            Op::Le => v <= limit,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub metric: Metric,
    pub op: Op,
    pub limit: f64,
    /// as given, for display
    pub text: String,
}

/// A size: a number with an optional K/M/G/T suffix (1024-based, as shown).
fn parse_size(s: &str) -> Option<f64> {
    let s = s.trim();
    let s = s.strip_suffix(['B', 'b']).unwrap_or(s);
    let (num, mult) = match s.chars().last()?.to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1024f64),
        'M' => (&s[..s.len() - 1], 1024f64.powi(2)),
        'G' => (&s[..s.len() - 1], 1024f64.powi(3)),
        'T' => (&s[..s.len() - 1], 1024f64.powi(4)),
        _ => (s, 1.0),
    };
    num.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v * mult)
}

fn parse_rule(text: &str) -> Result<Rule, String> {
    let bad = |why: &str| format!("bad --alert rule {text:?}: {why}");
    let at = text
        .find(['<', '>'])
        .ok_or_else(|| bad("needs < or >, e.g. 'foot>2G'"))?;
    let (key, rest) = (text[..at].trim(), &text[at..]);
    let (op, value) = if let Some(v) = rest.strip_prefix(">=") {
        (Op::Ge, v)
    } else if let Some(v) = rest.strip_prefix("<=") {
        (Op::Le, v)
    } else if let Some(v) = rest.strip_prefix('>') {
        (Op::Gt, v)
    } else {
        (Op::Lt, &rest[1..])
    };
    let value = value.trim();
    let (metric, limit) = match key {
        "foot" => (Metric::Foot, parse_size(value)),
        "headroom" => (Metric::Headroom, parse_size(value)),
        "rate" => (
            Metric::Rate,
            parse_size(value.strip_suffix("/s").unwrap_or(value)),
        ),
        "swapins" => (
            Metric::Swapins,
            parse_size(value.strip_suffix("/s").unwrap_or(value)),
        ),
        "grew" => (
            Metric::Grew,
            value
                .strip_suffix('%')
                .unwrap_or(value)
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite()),
        ),
        "pressure" => (
            Metric::Pressure,
            match value {
                "normal" => Some(1.0),
                "warn" => Some(2.0),
                "critical" => Some(4.0),
                _ => None,
            },
        ),
        _ => {
            return Err(bad(
                "key is one of foot rate swapins grew headroom pressure",
            ))
        }
    };
    let limit = limit.ok_or_else(|| {
        bad(match metric {
            Metric::Grew => "grew takes a percentage, e.g. grew>=100",
            Metric::Pressure => "pressure takes normal, warn or critical",
            _ => "takes a size such as 500M or 2G",
        })
    })?;
    Ok(Rule {
        metric,
        op,
        limit,
        text: text.trim().to_string(),
    })
}

/// Comma-separated rules, e.g. "foot>2G,headroom<200M,pressure>=warn".
pub fn parse_rules(spec: &str) -> Result<Vec<Rule>, String> {
    spec.split(',')
        .filter(|r| !r.trim().is_empty())
        .map(parse_rule)
        .collect::<Result<Vec<_>, _>>()
        .and_then(|rules| {
            if rules.is_empty() {
                Err("--alert needs at least one rule".to_string())
            } else {
                Ok(rules)
            }
        })
}

/// Who a rule holds for: a process (by PID and start time), or the system.
type Key = (usize, c_int, Option<StartTime>);

/// One rule holding for one process (pid 0: the system) for at least `--for`.
#[derive(Debug, PartialEq)]
pub struct Firing {
    pub rule: usize,
    pub pid: c_int,
    pub name: String,
    pub value: f64,
    /// seconds the rule has held
    pub held: f64,
    /// fired at this interval, not before
    pub new: bool,
}

pub struct Alerts {
    rules: Vec<Rule>,
    hold: f64,
    /// when each rule started holding for each key
    since: HashMap<Key, f64>,
    fired: HashSet<Key>,
}

impl Alerts {
    pub fn new(rules: &[Rule], hold_secs: u64) -> Alerts {
        Alerts {
            rules: rules.to_vec(),
            hold: hold_secs as f64,
            since: HashMap::new(),
            fired: HashSet::new(),
        }
    }

    pub fn active(&self) -> bool {
        !self.rules.is_empty()
    }

    /// Checks the rules against an interval that ended `now` seconds into the
    /// watch and lasted `elapsed`; returns what is firing, new ones first.
    pub fn update(
        &mut self,
        now: f64,
        elapsed: f64,
        layout: &Layout,
        rows: &[Row],
        pressure: Option<i32>,
    ) -> Vec<Firing> {
        let mut holding: Vec<(Key, String, f64)> = Vec::new();
        for (i, rule) in self.rules.iter().enumerate() {
            if rule.metric == Metric::Pressure {
                if let Some(level) = pressure.map(f64::from) {
                    if rule.op.holds(level, rule.limit) {
                        holding.push(((i, 0, None), "system".to_string(), level));
                    }
                }
                continue;
            }
            let col = match rule.metric {
                Metric::Foot => Col::Foot1,
                Metric::Rate => Col::Rate,
                Metric::Swapins => Col::SwapinsGrowth,
                Metric::Grew => Col::Grew,
                Metric::Headroom => Col::Headroom,
                Metric::Pressure => unreachable!(),
            };
            let Some(c) = layout.index(col) else {
                continue;
            };
            for r in rows {
                let value = match (rule.metric, r.vals[c]) {
                    (_, None) => continue,
                    (Metric::Headroom, Some(0.0)) => continue, // no limit
                    (Metric::Swapins, Some(v)) => v / elapsed.max(1e-9),
                    (_, Some(v)) => v,
                };
                if rule.op.holds(value, rule.limit) {
                    holding.push(((i, r.pid, r.start), r.name.clone(), value));
                }
            }
        }

        let keys: HashSet<Key> = holding.iter().map(|(k, ..)| *k).collect();
        self.since.retain(|k, _| keys.contains(k));
        self.fired.retain(|k| keys.contains(k));
        let mut out = Vec::new();
        for (key, name, value) in holding {
            let held = now - *self.since.entry(key).or_insert(now);
            if held + 1e-9 < self.hold {
                continue;
            }
            out.push(Firing {
                rule: key.0,
                pid: key.1,
                name,
                value,
                held,
                new: self.fired.insert(key),
            });
        }
        out.sort_by(|a, b| {
            b.new
                .cmp(&a.new)
                .then(a.rule.cmp(&b.rule))
                .then(b.held.total_cmp(&a.held))
                .then(a.pid.cmp(&b.pid))
        });
        out
    }

    /// "foot>2G: Google Chrome 21009 at 2.31G, for 1m10s"
    pub fn describe(&self, f: &Firing) -> String {
        let rule = &self.rules[f.rule];
        let value = match rule.metric {
            Metric::Foot | Metric::Headroom => human(f.value),
            Metric::Rate | Metric::Swapins => format!("{}/s", human(f.value)),
            Metric::Grew => format!("{:.0}%", f.value),
            Metric::Pressure => pressure_name(f.value as i32),
        };
        let who = if f.pid == 0 {
            f.name.clone()
        } else {
            format!("{} {}", f.name, f.pid)
        };
        let held = if f.held > 0.0 {
            format!(", for {}", duration_str(f.held))
        } else {
            String::new()
        };
        format!("{}: {who} at {value}{held}", rule.text)
    }
}

/// A Notification Center banner, through osascript. Best effort: it may not
/// show for a root process (sudo), and failures are ignored.
pub fn notify(text: &str) {
    let text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let esc = text.replace('\\', "\\\\").replace('"', "\\\"");
    let _ = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(format!(
            "display notification \"{esc}\" with title \"macswapi\""
        ))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::tests::row;

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("512"), Some(512.0));
        assert_eq!(parse_size("2G"), Some(2.0 * 1024f64.powi(3)));
        assert_eq!(parse_size("1.5m"), Some(1.5 * 1024f64.powi(2)));
        assert_eq!(parse_size("10KB"), Some(10240.0));
        assert_eq!(parse_size("-1M"), None);
        assert_eq!(parse_size("G"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn test_parse_rules() {
        let rules =
            parse_rules("foot>2G, headroom<=200M,grew>=100%,swapins>1M/s,pressure>=warn").unwrap();
        let shape: Vec<(Metric, Op, f64)> =
            rules.iter().map(|r| (r.metric, r.op, r.limit)).collect();
        assert_eq!(
            shape,
            vec![
                (Metric::Foot, Op::Gt, 2.0 * 1024f64.powi(3)),
                (Metric::Headroom, Op::Le, 200.0 * 1024f64.powi(2)),
                (Metric::Grew, Op::Ge, 100.0),
                (Metric::Swapins, Op::Gt, 1024f64.powi(2)),
                (Metric::Pressure, Op::Ge, 2.0),
            ]
        );
        assert_eq!(rules[1].text, "headroom<=200M");
        assert!(parse_rules("").is_err());
        assert!(parse_rules("foot=2G").is_err());
        assert!(parse_rules("cpu>50").is_err());
        assert!(parse_rules("foot>lots").is_err());
        assert!(parse_rules("pressure>high").is_err());
    }

    fn layout() -> Layout {
        Layout {
            cols: vec![Col::Foot1, Col::SwapinsGrowth, Col::Headroom],
            visible: vec![0, 1, 2],
        }
    }

    #[test]
    fn test_alerts_hold_then_fire_once() {
        let rules = parse_rules("foot>100").unwrap();
        let mut alerts = Alerts::new(&rules, 4);
        let big = [row(7, 1, &[Some(150.0), None, None])];
        let small = [row(7, 1, &[Some(50.0), None, None])];
        let l = layout();
        assert!(alerts.update(2.0, 2.0, &l, &big, None).is_empty()); // held 0s
        assert!(alerts.update(4.0, 2.0, &l, &big, None).is_empty()); // held 2s
        let f = alerts.update(6.0, 2.0, &l, &big, None);
        assert_eq!((f.len(), f[0].new, f[0].held), (1, true, 4.0));
        assert_eq!(alerts.describe(&f[0]), "foot>100: p7 7 at 150B, for 4s");
        let f = alerts.update(8.0, 2.0, &l, &big, None);
        assert!(!f[0].new); // still firing, not new
        assert!(alerts.update(10.0, 2.0, &l, &small, None).is_empty()); // reset
        assert!(alerts.update(12.0, 2.0, &l, &big, None).is_empty()); // starts over
    }

    #[test]
    fn test_alerts_metrics() {
        let rules = parse_rules("swapins>10,headroom<100,pressure>=warn").unwrap();
        let mut alerts = Alerts::new(&rules, 0);
        let rows = [
            row(1, 0, &[Some(1.0), Some(30.0), Some(0.0)]), // 15 B/s, no limit
            row(2, 0, &[Some(1.0), Some(10.0), Some(50.0)]), // 5 B/s, near limit
        ];
        let f = alerts.update(2.0, 2.0, &layout(), &rows, Some(2));
        let got: Vec<(usize, c_int)> = f.iter().map(|f| (f.rule, f.pid)).collect();
        assert_eq!(got, vec![(0, 1), (1, 2), (2, 0)]);
        assert_eq!(alerts.describe(&f[0]), "swapins>10: p1 1 at 15B/s");
        assert_eq!(alerts.describe(&f[2]), "pressure>=warn: system at warn");
        assert!(alerts.update(4.0, 2.0, &layout(), &[], Some(1)).is_empty());
    }
}
