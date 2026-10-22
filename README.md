# macswapi

A macOS per-process memory & swap inspector in Rust, with **zero external crates**.

Prints, per process:
- **exact** compressed memory (no rounding, unlike `top`)
- **exact** physical footprint
- an **estimated** swap share
- **exact** bytes swapped in over the process lifetime
- with `-x`: lifetime peaks, decompression count, and jetsam-limit headroom

plus a system memory summary on top, and modes in the spirit of Process
Explorer: a process tree with subtree rollups (`-t`), a live view that flags
sustained growth (`--live`), a one-process detail page (`--pid`), sorting and
filtering, and CSV output.

## Why estimated swap?

**macOS has no per-process swap counter.** The memory compressor packs pages
from many processes into shared *segments*, then swaps whole segments to disk —
swap is accounted per-segment, never traced back to a PID. No public or private
API holds "swap bytes owned by PID X".

What *is* exact and obtainable:

| metric | source |
|---|---|
| total swap used | `sysctl vm.swapusage` → `xsu_used` |
| per-proc compressed bytes | Mach `task_info(TASK_VM_INFO)` → `compressed` |
| per-proc phys footprint | Mach `task_info(TASK_VM_INFO)` → `phys_footprint` |
| per-proc swap-in bytes | `task_info(TASK_VM_INFO)` rev6 → `ledger_swapins` |

So macswapi reports exact compressed/footprint and **estimates** swap by
splitting the real total proportional to each process's compressed memory (where
swap pressure originates). The estimates are computed so their floating-point
sum matches the real total within a small test tolerance.

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

**kernel_task excluded.** pid 0 is never queried, so kernel-compressed memory
never appears in the denominator. User
processes are therefore slightly over-attributed — the gap is small in practice
but grows under kernel memory pressure.

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

**Watch** (`-w`/`--delta [SECS]`): two samples SECS apart (default 3), sorted by
footprint growth. Adds a line of system rates over the interval, the bytes
swapped in during it, and which processes started or exited (a reused PID is
one of each).

**Live** (`-l`/`--live [SECS]`): watch, refreshed every SECS (default 2) until
`q` or Ctrl-C; space pauses. Two more columns track the last 10 intervals:

| column | meaning |
|---|---|
| `GREW%` | share of those intervals in which `FOOTPRINT` grew (blank until 3); rows at 100% are yellow on a color terminal |
| `JETSAM_IN` | with `-x`: time until `HEADROOM` runs out at the window's average growth rate |

On a terminal it draws in place on the alternate screen and leaves the last
frame behind on exit; piped, it appends one frame per refresh.

**Detail** (`--pid PID`): everything readable about one process: the full
`task_vm_info` breakdown with peaks, the purgeable and network/media/graphics/neural
ledgers, its SWAP~ share, fault and thread counts (`PROC_PIDTASKINFO`), and
wired memory and disk I/O (`proc_pid_rusage`).

### Tree (`-t`)

Rows nest under their parent process, and each row's values cover its whole
subtree, like `du`: Chrome's line holds all of its helpers, and `PROCS` counts
them. Children of launchd (pid 1) are the roots, since rolling everything into
launchd would hide every app. `N` keeps the top N trees, each shown in full.
Peaks, `HEADROOM`, and the live trend columns don't add up, so they are blank on
rows that have children.

### Sort and filter

- `--sort KEY` — snapshot: `cmprs` (default) `swap` `foot` `swapins` `peak`
  `cpeak` `decomp` `headroom`; watch/live: `delta` (default) `foot` `swapins`
  `decomp` `headroom`, live also `grew`; any mode: `pid` `name`. Largest first,
  except `headroom`: smallest first (closest to a kill), processes without a
  limit last.
- `--name STR` — names containing STR, any case.
- `--user USER` — a login name or uid.

Filters pick rows only: the SWAP~ denominator still covers every readable process.

### CSV (`--csv`)

Every column of the mode, whatever `-x` says, as whole numbers (bytes, counts,
bytes/s); empty means N/A. `headroom` 0 means no limit. With `-t`, a `depth`
column is added and rows carry subtree totals. No header or footer is printed;
warnings go to stderr. Not available with `--live`.

## Install

```sh
./install.sh                    # build and install to /usr/local/bin (uses sudo if needed)
./install.sh --prefix ~/.local  # install to ~/.local/bin instead
./install.sh --uninstall        # remove it (pass the same --prefix)
```

Needs Rust: get it from <https://rustup.rs> or `brew install rust`.

## Build

```sh
cargo build --release
cargo test  --release
```

No `[dependencies]`. All kernel APIs come via `extern "C"` from libSystem at
link time → network-free build.

## Usage

```
macswapi [N] [options] [-w|--watch|--delta [SECS] | -l|--live [SECS] | --pid PID]
```

- `N` — top N rows (default 20, `0` = all); with `-t`, top N trees
- `-p`, `--parent` — add PPID + parent-name columns
- `-x`, `--extended` — add `FOOT_PK`, `CMPRS_PK`, `DECOMP`, `HEADROOM` (watch: `DECOMP`, `HEADROOM`; live: also `JETSAM_IN`)
- `-t`, `--tree` — process trees with subtree rollups
- `--sort KEY`, `--name STR`, `--user USER` — see [Sort and filter](#sort-and-filter)
- `--csv` — CSV output
- `-w`, `--watch [SECS]`, `--delta [SECS]` — single-interval leak check (default 3s)
- `-l`, `--live [SECS]` — refresh every SECS (default 2s)
- `--pid PID` — one-process detail page
- `-h`, `--help` — usage

SECS must be positive and immediately follow its flag; `N` must appear before
the interval flag. `--pid` takes no other mode, `--tree`, `--csv`, `--sort` or
filter.

```sh
macswapi 20                         # top 20 (own procs without sudo)
sudo macswapi 20 -p                 # system-wide + parents
sudo macswapi 20 -x                 # + peaks, decompressions, jetsam headroom
sudo macswapi 10 -t                 # top 10 process trees, rolled up
macswapi --name chrome --sort foot  # Chrome processes by footprint
macswapi 5 --sort headroom          # closest to a jetsam kill
sudo macswapi -w 5                  # leak watch, 5s
macswapi 30 -t --live               # live trees; space pauses, q quits
macswapi --pid 872                  # everything about one process
macswapi 0 --csv > procs.csv        # every process, every column
```

## Privileges

Stats are read through a task *name* port (`task_name_for_pid`), the read-only
handle macOS hands out for `task_info` queries. Without `sudo`, every process
you own is read, Apple's included; other users' processes are denied, so the
swap-share denominator covers only yours (denial count is shown). Reading
another user's process needs **root**: with `sudo`, every process is read
(except kernel_task, see caveats). Compressed/footprint are exact either way.

`task_for_pid` is not used: without root it is refused even for your own
processes, and under `sudo` it is still refused for SIP-protected ones.

`--pid` on another user's process needs `sudo` too, including for its fault
counts (`PROC_PIDTASKINFO`) and `proc_pid_rusage`. Without it, `--user root`
matches nothing, because root's processes can't be read.

## License

MIT
