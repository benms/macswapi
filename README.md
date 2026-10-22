# macswapi

[![CI](https://github.com/benms/macswapi/actions/workflows/ci.yml/badge.svg)](https://github.com/benms/macswapi/actions/workflows/ci.yml)

A macOS per-process memory & swap inspector in Rust, with **zero external crates**.

Prints, per process:
- **exact** compressed memory (no rounding, unlike `top`)
- **exact** physical footprint
- an **estimated** swap share
- **exact** bytes swapped in over the process lifetime
- with `-x`: lifetime peaks, decompression count, and jetsam-limit headroom

plus a system memory summary on top, and modes in the spirit of Process
Explorer: a process tree with subtree rollups (`-t`), apps rolled up with
their XPC helpers (`-a`), a one-screen dashboard (`-T`), a live watch that
flags sustained growth and names who is swapping in (`-w`), alert rules with exit codes and notifications
(`--alert`), a one-process detail page (`--pid`), sorting and filtering, and
CSV output, streamed or recorded to a file (`--record`).

## Why estimated swap?

**macOS has no per-process swap counter.** The memory compressor packs pages
from many processes into shared *segments*, then swaps whole segments to disk —
swap is accounted per-segment, never traced back to a PID. No public or private
API holds "swap bytes owned by PID X".

What *is* exact and obtainable:

| metric | source |
|---|---|
| total swap used | `sysctl vm.swapusage` → `xsu_used` |
| all the compressor holds | `host_statistics64` → `total_uncompressed_pages_in_compressor` |
| per-proc compressed bytes | Mach `task_info(TASK_VM_INFO)` → `compressed` |
| per-proc phys footprint | Mach `task_info(TASK_VM_INFO)` → `phys_footprint` |
| per-proc swap-in bytes | `task_info(TASK_VM_INFO)` rev6 → `ledger_swapins` |

So macswapi reports exact compressed/footprint and **estimates** swap by
splitting the real total proportional to each process's compressed memory (where
swap pressure originates). The denominator is everything the compressor holds,
system-wide: the part no readable process accounts for (kernel_task, and other
users' processes without `sudo`) is *unattributed* and keeps its share, so a
missing `sudo` doesn't inflate the estimates of the processes you can read. The
snapshot footer shows that split:

```
SWAP~ split over the compressor's 8.82G: readable procs 6.85G, unattributed 1.97G (kernel_task, 315 denied) → SWAP~ 0B
```

The estimates are computed so their floating-point sum, unattributed share
included, matches the real total within a small test tolerance. If the host
total can't be read, the denominator falls back to the readable processes alone
(with a warning when some were denied).

`SWAPINS` is the exact, complementary signal: the kernel's per-task `swapins`
ledger, in bytes. It measures swap *traffic* (data read back from swap on the
process's behalf over its lifetime), not bytes currently sitting on disk, so it
does not replace `SWAP~`. In watch mode it shows growth over the interval: a
process whose swap-ins keep rising is the one actually paying for swap.

### Estimate caveats

**Bias toward idle processes.** The compressor evicts least-recently-used
segments first, so idle processes hold a disproportionate share of on-disk swap
relative to their compressed-memory size. A busy process with a hot compressed
pool is therefore *overweighted* by this estimate.

**Compression ratio assumed uniform.** `compressed` (from `task_info`) is the
*uncompressed* size of memory the compressor holds for a process. `xsu_used`
(from `vm.swapusage`) is the *compressed* on-disk size across all processes.
The proportional split implicitly assumes every process's data compresses at the
same ratio. Processes whose data compresses poorly will be underweighted; those
that compress well will be overweighted.

**kernel_task is unattributed.** pid 0 is never queried, so kernel-compressed
memory has no row; its share of swap is in the footer's *unattributed* part,
together with processes that couldn't be read. Shared memory compressed on
behalf of several processes may be counted differently by the per-process and
host totals, so the split is approximate; the denominator is never below the
readable processes' sum.

**Columns are not additive.** `FOOTPRINT` is the process's physical footprint
ledger value. It usually includes compressed memory, but not always: `CMPRS`
can even exceed `FOOTPRINT` (one wallpaper extension showed 277M vs 15M, and
`top` agrees). `CMPRS` explains one component of pressure; do not add
`CMPRS + FOOTPRINT` as a total.

**HEADROOM `-` is ambiguous.** The kernel reports 0 bytes remaining both for a
process with no jetsam memory limit (most of them) and for one at or over a
*soft* limit. Both print as `-`.

## Extended columns (`-x`)

All come from the same `task_info(TASK_VM_INFO)` call. Older kernels return an
earlier struct revision, and fields it lacks print as `N/A`.

| column | field | meaning |
|---|---|---|
| `FOOT_PK` | `ledger_phys_footprint_peak` (rev3) | lifetime peak of `FOOTPRINT` |
| `CMPRS_PK` | `compressed_peak` | peak of `CMPRS` |
| `DECOMP` | `decompressions` (rev5) | pages decompressed over the lifetime (compressor churn, swap or not) |
| `HEADROOM` | `limit_bytes_remaining` (rev4) | bytes left before the jetsam memory limit; `-` = none (see caveats) |

Watch mode with `-x` adds `DECOMP` growth and the current `HEADROOM`, so a
leaking process can be checked against its limit. Sorting by a column that `-x`
would add (`--sort headroom`) shows that column without `-x`.

## Modes

**Snapshot** (default): the table above, sorted by `CMPRS`. A summary header
comes first, from `host_statistics64` and sysctls:

```
memory 32.00G: wired 2.84G  active 11.22G  inactive 10.93G  free 1.33G
compressor 4.82G holding 9.01G (1.87x)  pressure normal (memorystatus level 75%)
swap 0B used of 0B (encrypted), 0 compressor pages in swap  since boot: swapped in 0B, out 0B
```

The compressor ratio (uncompressed data held ÷ space it occupies) is a
system-wide average; the SWAP~ estimate implicitly gives every process this one
ratio (see caveats).

**Watch** (`-w`/`--watch [SECS]`): samples every SECS (default 2) until `q` or
Ctrl-C, sorted by footprint growth over the last interval; space pauses. Adds a
line of system rates over the interval, the bytes swapped in during it and the
processes they went to (with `-a`, the apps), and which processes started or
exited (a reused PID is one of each):

```
swapped in during interval: 120.00M — Xcode 812 74%, Slack 1044 10%, mds_stores 377 6%
```

Three more columns track the last 10 intervals:

| column | meaning |
|---|---|
| `GREW%` | share of those intervals in which `FOOTPRINT` grew (blank until 3); rows at 100% are yellow on a color terminal |
| `TREND` | `FOOTPRINT` over those intervals as a sparkline (`▁▃▅█`), scaled to the row's own range; a spread under 5% of the footprint (or 1M) draws low, so allocator noise stays flat |
| `JETSAM_IN` | with `-x`: time until `HEADROOM` runs out at the window's average growth rate |

On a terminal it draws in place on the alternate screen and leaves the last
frame behind on exit; piped, it appends one frame per refresh.

`--once [SECS]` watches a single interval, prints it and exits, without the
trend columns. `--csv` with `--watch` streams instead of drawing (see
[CSV](#csv---csv)). `-l`/`--live` and `--delta` are aliases of `--watch`.

### Top (`-T`)

`-T`/`--top [SECS]` is a one-screen dashboard in the manner of Mole's
`mo status`, refreshed every SECS (default 2) until `q`. Space pauses, `a`
switches the first process card between apps (grouped as with `-a`) and single
processes, and `?` or `h` shows a help page with the keys, the verdict order
and what each card means (any key goes back; sampling goes on behind it):

```
macswapi top · 32.00G RAM · up 2h47m · pressure ● normal (74%) · all clear

◫ Memory  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  ◧ Compressor  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
Wired  ██░░░░░░░░░░░░░░   9.5%    3.04G            Holds  10.86G in 5.53G (1.96x)
Active ██████░░░░░░░░░░  35.2%   11.25G            Size         ▁▁▁▁▁▁▁▁  17.3% of RAM
Inact  ██████░░░░░░░░░░  35.1%   11.23G            Rate   compress 0/s · decomp 2000/s
Cmprsd ███░░░░░░░░░░░░░  17.3%    5.53G            Owner  procs 8.23G · unattributed 2.63G
Free   ░░░░░░░░░░░░░░░░   0.3%   86.45M

⇅ Swap  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  ◉ Pressure  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
Used   none                                        Level  ● normal
In             ▁▁▁▁▁▁▁▁ 0B/s                       Avail         ▆▆▆▆▆▆▆▆ 74%
Out            ▁▁▁▁▁▁▁▁ 0B/s                       Boot   swapped in 0B · out 0B
Now    nobody swapping in                          Read   650 procs · 314 denied, sudo for all

❊ Apps by footprint  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  ▲ Growing  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
Google Chrome ×53      ▮▮▮▮▮    6.25G ▁▂▂▃▃▂▂▃     macswapi 98377       +210.73K/s 100% ▁▃▆▇▇▇█
Code ×31               ▮▮▮▯▯    3.64G ▄▁▁▁▁▁▁▁     Code Helper 22687     +45.16K/s 100% ▁▁▁▂▂▂▂
OrbStack Helper 87953  ▮▯▯▯▯    1.81G ▁▁▁▁▁▁▁▁     Jetsam nearest: WallpaperAgent 653, 9.44M left
Claude ×16             ▮▯▯▯▯    1.10G ▁▁▁▁▁▁▁▁

space pause · a apps/processes · ? help · q quit · every 2s · refresh #8
```

- The header ends with a verdict, the first that applies of: memory pressure
  critical, who is swapping in, a firing `--alert`, pressure warn, a process
  growing every interval faster than 1M/s, else all clear. On a narrow
  terminal the RAM size and uptime give way to it.
- **Memory**: physical memory by state, as a share of RAM. **Compressor**:
  what it holds and in how much RAM, its size over time, page rates, and how
  much of its content readable processes account for. **Swap**: use, swap-in
  and swap-out rates over time, and which apps are swapping in now.
  **Pressure**: the kernel's level and available percentage over time, swap
  traffic since boot, and how many processes were readable.
- **Apps by footprint** (or processes, with `a`): `▮` bars relative to the
  largest, and each row's `TREND`. **Growing**: processes whose footprint grew
  over the interval, those growing every interval (`GREW%` 100, yellow) first,
  then the process closest to its jetsam limit.
- Wider than 80 columns, cards go in pairs; narrower, in one column. The
  process cards shrink to fit the window (`N` fixes the apps card's rows);
  what still doesn't fit is cut at the bottom. The sparklines start with the
  first interval and keep the last 14.
- `--name` and `--user` pick the processes in the process cards. `--alert`
  rules are checked against every app and process; firing ones show as a
  yellow banner under the header and ring the bell, and `--for`,
  `--exit-on-alert` and `--notify` work as in watch mode.
- Piped, it prints one frame after the first interval and exits, without
  color.

**Detail** (`--pid PID`): everything readable about one process: its parent
and responsible process (see [Apps](#apps--a)), the full
`task_vm_info` breakdown with peaks, the purgeable and network/media/graphics/neural
ledgers, its SWAP~ share, fault and thread counts (`PROC_PIDTASKINFO`), and
wired memory and disk I/O (`proc_pid_rusage`).

### Tree (`-t`)

Rows nest under their parent process, and each row's values cover its whole
subtree, like `du`: Chrome's line holds all of its helpers, and `PROCS` counts
them. Children of launchd (pid 1) are the roots, since rolling everything into
launchd would hide every app. `N` keeps the top N trees, each shown in full.
Peaks, `HEADROOM`, `GREW%` and `JETSAM_IN` don't add up, so they are blank on
rows that have children. `TREND` does: a group's sparkline adds up its members'
histories, aligned at the latest sample (members that just started count as 0
before that, and processes that exited are no longer in it).

### Apps (`-a`)

Like `-t`, but rows nest under the process macOS holds *responsible* for them,
as Activity Monitor groups them. Many app helpers are XPC services that launchd
starts on the app's behalf, so their parent is launchd and `-t` leaves them out
of the app: Mail's `com.apple.WebKit.WebContent`, Chrome's
`MTLCompilerService`, `VTDecoderXPCService` and the like. `-a` puts them in, so
an app's line is what it really costs:

```
PID     NAME                            PROCS        CMPRS    FOOTPRINT        SWAP~      SWAPINS
21009   Google Chrome                      54        2.40G        5.47G           0B           0B
21686     Google Chrome Helper …            1      189.19M      312.88M           0B           0B
21079     MTLCompilerService                1       11.05M       55.39M           0B           0B
```

The responsible process comes from `responsibility_get_pid_responsible_for_pid`,
a private libSystem call that needs no root. A terminal is responsible for the
shells and commands started in it, so those land under the terminal app.
Processes with no answer, or whose responsible process can't be read, are
groups of their own. `N` keeps the top N apps; `--pid` shows a process's
responsible process.

### Sort and filter

- `--sort KEY` — snapshot: `cmprs` (default) `swap` `foot` `swapins` `peak`
  `cpeak` `decomp` `headroom`; watch: `delta` (default) `foot` `swapins`
  `decomp` `headroom` `grew` (not with `--once`); any mode: `pid` `name`. Largest first,
  except `headroom`: smallest first (closest to a kill), processes without a
  limit last.
- `--name STR` — names containing STR, any case.
- `--user USER` — a login name or uid.

Filters pick rows only: the SWAP~ denominator still covers everything the
compressor holds.

### Alerts (`--alert`, watch and top)

Rules checked at every interval against every row that passes the filters, not
just the top N; in `-t`/`-a` mode, against group rows too (a group's `foot` is
its total). A rule fires once it has held for `--for SECS` on end (default 0:
at once); it resets as soon as it stops holding.

| rule | holds when |
|---|---|
| `foot>2G` | `FOOT1`, the footprint now, is above 2G |
| `rate>5M/s` | the footprint grew faster than 5M/s over the interval |
| `swapins>1M/s` | the process swapped in faster than 1M/s over the interval |
| `grew>=100` | `GREW%` is at least 100 (needs 3 intervals) |
| `headroom<200M` | less than 200M left before the jetsam limit (processes with a limit only) |
| `pressure>=warn` | system memory pressure is `warn` or `critical` |

Operators are `>`, `>=`, `<`, `<=`; sizes take `K` `M` `G` `T` (1024-based, as
shown). Join rules with commas or repeat `--alert`; any rule firing counts.
Quote them, since the shell treats `<` and `>` as redirections.

Firing rules are listed under the table (`new` for those that fired at this
interval) and ring the terminal bell. With `--csv` they go to stderr instead,
one line per new firing. `--notify` also posts a Notification Center banner
for new firings (through `osascript`; under `sudo` it may not show).
`--exit-on-alert` stops at the first firing, after printing that interval, with
exit status 3, for scripts and leak tests:

```sh
macswapi -w 5 --name MyApp --alert 'grew>=100,foot>3G' --for 60 --exit-on-alert \
  || echo "MyApp kept growing for a minute, or passed 3G"
```

### Record (`--record FILE`, watch only)

Appends every interval to FILE as CSV, with the same columns as `--csv`
streaming: a `time` column, then every row that passes the filters (not just
the top N). The header is written when the file is new or empty; an existing
file must have the same columns (same mode and `-t`/`-a`), otherwise macswapi
refuses to mix them. macOS keeps no per-process memory history, so a recording
left running answers "who was growing, or swapping in, at 2pm?":

```sh
macswapi -w 30 --record ~/macswapi.csv > /dev/null &
```

### CSV (`--csv`)

Every column of the mode, whatever `-x` says, as whole numbers (bytes, counts,
bytes/s); empty means N/A. `TREND` is left out (it is a picture of the other
columns). `headroom` 0 means no limit. With `-t` or `-a`, a `depth` column is
added and rows carry group totals. No summary or footer is printed; warnings go
to stderr.

With `--watch`, CSV streams: one header line, then each interval's top N rows,
each led by a `time` column (ISO 8601, UTC), until Ctrl-C. Use `N` = 0 for every
row. `--once --csv` prints a single interval without `time`.

## Install

**Homebrew.** This repository is its own tap; it builds the latest `main`:

```sh
brew tap benms/macswapi https://github.com/benms/macswapi
brew install --HEAD macswapi
brew upgrade --fetch-HEAD macswapi   # later, to update
```

**From a clone:**

```sh
./install.sh                    # build and install to /usr/local/bin (uses sudo if needed)
./install.sh --prefix ~/.local  # install to ~/.local/bin instead
./install.sh --no-completions   # skip the shell completions
./install.sh --uninstall        # remove it (pass the same --prefix)
```

Needs Rust 1.77 or newer: get it from <https://rustup.rs> or `brew install rust`.

### Shell completions

Both installs add completions for zsh, bash and fish (`install.sh` under
`PREFIX/share`, Homebrew under its own prefix): flags, sort keys, users, PIDs,
alert rules and files. To load them by hand instead:

```sh
eval "$(macswapi --completions zsh)"     # ~/.zshrc, after compinit
eval "$(macswapi --completions bash)"    # ~/.bashrc
macswapi --completions fish | source     # ~/.config/fish/config.fish
```

With `--prefix ~/.local`, zsh needs `~/.local/share/zsh/site-functions` on
`fpath` before `compinit`.

### Agent skill

`skills/macswapi/SKILL.md` teaches coding agents to answer memory questions
with macswapi: which command fits "what is using my swap?" or "is something
leaking?", how to read the columns, and to never start a watch that waits for
a key or run `sudo` for you.

```sh
npx skills add benms/macswapi -a claude-code codex cursor -g -y   # Claude Code, Codex, Cursor
/plugin install macswapi --marketplace benms/macswapi             # or as a Claude Code plugin
```

[`llms.txt`](llms.txt) tells an agent how to install both.

## Build

```sh
cargo build --release
cargo test  --release
```

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs rustfmt,
clippy with warnings as errors, the tests, and a smoke test of every mode on
macOS 15 and 26; checks the completion scripts (`zsh -n`, `bash -n`, `fish -n`,
shellcheck) and `install.sh`; builds with Rust 1.77; and runs `brew style` on
the formula.

No `[dependencies]`. All kernel APIs come via `extern "C"` from libSystem at
link time → network-free build.

## Usage

```
macswapi [N] [options] [-w|--watch [SECS] | --once [SECS] | --pid PID]
```

- `N` — top N rows (default 20, `0` = all); with `-t`, top N trees
- `-p`, `--parent` — add PPID + parent-name columns
- `-x`, `--extended` — add `FOOT_PK`, `CMPRS_PK`, `DECOMP`, `HEADROOM` (watch: `DECOMP`, `HEADROOM`, and `JETSAM_IN` unless `--once`)
- `-t`, `--tree` — process trees with subtree rollups
- `-a`, `--app` — rows grouped under their responsible app, XPC helpers included
- `--sort KEY`, `--name STR`, `--user USER` — see [Sort and filter](#sort-and-filter)
- `--csv` — CSV output; streamed in watch mode
- `-w`, `--watch [SECS]` — leak watch, refreshed every SECS (default 2s); `-l`/`--live`/`--delta` are aliases
- `--once [SECS]` — watch a single interval and exit
- `-T`, `--top [SECS]` — one-screen dashboard, see [Top](#top--t)
- `--pid PID` — one-process detail page
- `--alert RULES`, `--for SECS`, `--exit-on-alert`, `--notify` — watch and top alerts, see [Alerts](#alerts---alert-watch-and-top)
- `--record FILE` — append every watch interval to FILE as CSV
- `--completions SHELL` — print the zsh, bash or fish completion script
- `-h`, `--help` — usage

SECS must be positive and immediately follow its flag; `N` must appear before
the interval flag. `--pid` takes no other mode, `--tree`, `--app`, `--csv`,
`--sort` or filter. `--tree` and `--app` exclude each other. `--top` takes `N`, `--name`,
`--user` and the alert options, nothing else. `--alert` needs `--watch` or
`--top`; `--record` needs `--watch` (not `--once`).

```sh
macswapi 20                         # top 20 (own procs without sudo)
sudo macswapi 20 -p                 # system-wide + parents
sudo macswapi 20 -x                 # + peaks, decompressions, jetsam headroom
sudo macswapi 10 -t                 # top 10 process trees, rolled up
macswapi 10 -a                      # top 10 apps, XPC helpers included
macswapi -T                         # the dashboard; a: apps/processes, ?: help
sudo macswapi -T 5                  # every process, refreshed every 5s
macswapi --name chrome --sort foot  # Chrome processes by footprint
macswapi 5 --sort headroom          # closest to a jetsam kill
sudo macswapi -w 5                  # leak watch every 5s; space pauses, q quits
macswapi 30 -t -w                   # watch trees
macswapi 10 -a -w                   # watch apps, with a TREND per app
macswapi -w --alert 'headroom<200M,pressure>=warn' --notify
macswapi 0 -w 10 --csv > watch.csv  # stream every process, every 10s
sudo macswapi -w 30 --record /var/tmp/mem.csv  # keep a history
sudo macswapi --once 5              # one 5s interval, then exit
macswapi --pid 872                  # everything about one process
macswapi 0 --csv > procs.csv        # every process, every column
```

## Privileges

Stats are read through a task *name* port (`task_name_for_pid`), the read-only
handle macOS hands out for `task_info` queries. Without `sudo`, every process
you own is read, Apple's included; other users' processes are denied (the count
is shown), and their share of swap goes to the unattributed part, as the
denominator comes from the host, not from the processes. Reading
another user's process needs **root**: with `sudo`, every process is read
(except kernel_task, see caveats). Compressed/footprint are exact either way.

`task_for_pid` is not used: without root it is refused even for your own
processes, and under `sudo` it is still refused for SIP-protected ones.

`--pid` on another user's process needs `sudo` too, including for its fault
counts (`PROC_PIDTASKINFO`) and `proc_pid_rusage`. Without it, `--user root`
matches nothing, because root's processes can't be read.

## License

MIT
