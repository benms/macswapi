//! `--completions SHELL`: tab-completion scripts for zsh, bash and fish.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

impl Shell {
    pub fn parse(name: &str) -> Result<Shell, String> {
        match name {
            "zsh" => Ok(Shell::Zsh),
            "bash" => Ok(Shell::Bash),
            "fish" => Ok(Shell::Fish),
            _ => Err(format!("--completions takes zsh, bash or fish, got {name}")),
        }
    }

    pub fn script(self) -> &'static str {
        match self {
            Shell::Zsh => ZSH,
            Shell::Bash => BASH,
            Shell::Fish => FISH,
        }
    }
}

// Sort keys over every mode, and alert rule stems; the tests keep both in step
// with the parser.

const ZSH: &str = r#"#compdef macswapi
# zsh completion for macswapi: put it on $fpath as _macswapi, or
#   eval "$(macswapi --completions zsh)"

_macswapi_alert() {
    compadd -S '' -- 'foot>' 'rate>' 'swapins>' 'grew>=' 'headroom<' 'pressure>='
}

_macswapi() {
    _arguments -s -S \
        '(- *)'{-h,--help}'[show usage]' \
        '(- *)--completions[print a shell completion script]:shell:(zsh bash fish)' \
        '(-p --parent)'{-p,--parent}'[add PPID and parent-name columns]' \
        '(-x --extended)'{-x,--extended}'[add peaks, decompressions and jetsam headroom]' \
        '(-t --tree -a --app)'{-t,--tree}'[process trees, rolled up over each subtree]' \
        '(-t --tree -a --app)'{-a,--app}'[group under the responsible app, XPC helpers included]' \
        '--sort[sort key]:key:(cmprs swap foot swapins peak cpeak decomp headroom delta grew pid name)' \
        '--name[only names containing STR]:name: ' \
        '--user[only processes of USER]:user:_users' \
        '--csv[CSV output, streamed in watch mode]' \
        '(-w --watch -l --live --delta --once --pid -T --top)'{-w,--watch,-l,--live,--delta}'[leak watch, refreshed every SECS]' \
        '(-w --watch -l --live --delta --pid -T --top)--once[watch a single interval and exit]' \
        '(-w --watch -l --live --delta --once --pid -T --top -t --tree -a --app --csv --sort -p --parent -x --extended --record)'{-T,--top}'[one-screen memory dashboard, refreshed every SECS]' \
        '(-w --watch -l --live --delta --once -T --top)--pid[everything about one process]:pid:_pids' \
        '*--alert[alert rules, watch and top]:rules:_macswapi_alert' \
        '--for[seconds a rule must hold to fire]:seconds: ' \
        '--exit-on-alert[exit with status 3 at the first firing]' \
        '--notify[Notification Center banner for new firings]' \
        '--record[append every watch interval to FILE as CSV]:file:_files' \
        '*:N rows or SECS: '
}

if [ "$funcstack[1]" = "_macswapi" ]; then
    _macswapi "$@"
else
    compdef _macswapi macswapi
fi
"#;

const BASH: &str = r#"# bash completion for macswapi: source it, or
#   eval "$(macswapi --completions bash)"
# Works with the bash 3.2 macOS ships.

_macswapi() {
    local cur prev k
    cur=${COMP_WORDS[COMP_CWORD]}
    prev=${COMP_WORDS[COMP_CWORD-1]}
    COMPREPLY=()
    case $prev in
        --sort)
            # shellcheck disable=SC2207
            COMPREPLY=($(compgen -W "cmprs swap foot swapins peak cpeak decomp headroom delta grew pid name" -- "$cur"))
            return ;;
        --user)
            # shellcheck disable=SC2207
            COMPREPLY=($(compgen -u -- "$cur"))
            return ;;
        --pid)
            # shellcheck disable=SC2207
            COMPREPLY=($(compgen -W "$(ps -axo pid=)" -- "$cur"))
            return ;;
        --record)
            # shellcheck disable=SC2207
            COMPREPLY=($(compgen -f -- "$cur"))
            return ;;
        --completions)
            # shellcheck disable=SC2207
            COMPREPLY=($(compgen -W "zsh bash fish" -- "$cur"))
            return ;;
        --alert)
            # The stems hold < and >, so they go in quoted.
            for k in 'foot>' 'rate>' 'swapins>' 'grew>=' 'headroom<' 'pressure>='; do
                case $k in "$cur"*) COMPREPLY+=("$(printf '%q' "$k")") ;; esac
            done
            if type compopt > /dev/null 2>&1; then compopt -o nospace; fi
            return ;;
        --name | --for)
            return ;;
    esac
    case $cur in
        -*)
            # shellcheck disable=SC2207
            COMPREPLY=($(compgen -W "-h --help --completions -p --parent -x --extended -t --tree -a --app --sort --name --user --csv -w --watch -l --live --delta --once -T --top --pid --alert --for --exit-on-alert --notify --record" -- "$cur"))
            ;;
    esac
}

complete -F _macswapi macswapi
"#;

const FISH: &str = r#"# fish completion for macswapi: save it as
# ~/.config/fish/completions/macswapi.fish, or
#   macswapi --completions fish | source

complete -c macswapi -f
complete -c macswapi -s h -l help -d 'Show usage'
complete -c macswapi -l completions -x -a 'zsh bash fish' -d 'Print a shell completion script'
complete -c macswapi -s p -l parent -d 'Add PPID and parent-name columns'
complete -c macswapi -s x -l extended -d 'Add peaks, decompressions and jetsam headroom'
complete -c macswapi -s t -l tree -d 'Process trees, rolled up over each subtree'
complete -c macswapi -s a -l app -d 'Group under the responsible app, XPC helpers included'
complete -c macswapi -l sort -x -a 'cmprs swap foot swapins peak cpeak decomp headroom delta grew pid name' -d 'Sort key'
complete -c macswapi -l name -x -d 'Only names containing STR'
complete -c macswapi -l user -x -a '(__fish_complete_users)' -d 'Only processes of USER'
complete -c macswapi -l csv -d 'CSV output, streamed in watch mode'
complete -c macswapi -s w -l watch -d 'Leak watch, refreshed every SECS'
complete -c macswapi -s l -l live -d 'Alias of --watch'
complete -c macswapi -l delta -d 'Alias of --watch'
complete -c macswapi -l once -d 'Watch a single interval and exit'
complete -c macswapi -s T -l top -d 'One-screen memory dashboard, refreshed every SECS'
complete -c macswapi -l pid -x -a '(__fish_complete_pids)' -d 'Everything about one process'
complete -c macswapi -l alert -x -a "'foot>' 'rate>' 'swapins>' 'grew>=' 'headroom<' 'pressure>='" -d 'Alert rules, watch and top'
complete -c macswapi -l for -x -d 'Seconds a rule must hold to fire'
complete -c macswapi -l exit-on-alert -d 'Exit with status 3 at the first firing'
complete -c macswapi -l notify -d 'Notification Center banner for new firings'
complete -c macswapi -l record -r -F -d 'Append every watch interval to FILE as CSV'
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::parse_rules;
    use crate::cli::{parse_args, USAGE};

    /// Every `--flag` the usage text names.
    fn usage_flags() -> Vec<String> {
        let mut flags: Vec<String> = USAGE
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .filter(|w| w.starts_with("--") && w.len() > 2)
            .map(str::to_string)
            .collect();
        flags.sort();
        flags.dedup();
        flags
    }

    #[test]
    fn test_scripts_cover_every_flag() {
        let flags = usage_flags();
        assert!(flags.len() >= 20, "{flags:?}");
        for shell in [Shell::Zsh, Shell::Bash, Shell::Fish] {
            let script = shell.script();
            for flag in &flags {
                let found = match shell {
                    Shell::Fish => script.contains(&format!("-l {}", &flag[2..])),
                    _ => script.contains(flag.as_str()),
                };
                assert!(found, "{shell:?} completion lacks {flag}");
            }
        }
    }

    #[test]
    fn test_scripts_offer_valid_sort_keys_and_rule_stems() {
        const KEYS: &str = "cmprs swap foot swapins peak cpeak decomp headroom delta grew pid name";
        for key in KEYS.split(' ') {
            // each key is accepted in at least one mode
            let ok = ["", "-w"].iter().any(|mode| {
                let mut args = vec!["--sort".to_string(), key.to_string()];
                if !mode.is_empty() {
                    args.push(mode.to_string());
                }
                parse_args(&args).is_ok()
            });
            assert!(ok, "sort key {key} rejected");
        }
        for shell in [Shell::Zsh, Shell::Bash, Shell::Fish] {
            assert!(shell.script().contains(KEYS), "{shell:?} sort keys");
        }
        for (stem, value) in [
            ("foot>", "1G"),
            ("rate>", "1M"),
            ("swapins>", "1M"),
            ("grew>=", "100"),
            ("headroom<", "1M"),
            ("pressure>=", "warn"),
        ] {
            assert!(parse_rules(&format!("{stem}{value}")).is_ok(), "{stem}");
            for shell in [Shell::Zsh, Shell::Bash, Shell::Fish] {
                assert!(
                    shell.script().contains(&format!("'{stem}'")),
                    "{shell:?} {stem}"
                );
            }
        }
    }

    #[test]
    fn test_shell_parse() {
        assert_eq!(Shell::parse("zsh"), Ok(Shell::Zsh));
        assert!(Shell::parse("tcsh").is_err());
    }
}
