# macswapi

A macOS per-process memory & swap inspector in Rust, with **zero external crates**.

Prints, per process:
- **exact** compressed memory (no rounding, unlike `top`)
- **exact** physical footprint
- an **estimated** swap share

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

So macswapi reports exact compressed/footprint and **estimates** swap by
splitting the real total proportional to each process's compressed memory (where
swap pressure originates). The estimates are computed so their floating-point
sum matches the real total within a small test tolerance.

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
macswapi [N] [-p|--parent] [-w|--watch|--delta [SECS]]
```

- `N` — top N rows (default 20, `0` = all)
- `-p`, `--parent` — add PPID + parent-name columns
- `-w`, `--watch [SECS]`, `--delta [SECS]` — single-interval leak check over a positive interval (default 3s); `N` must appear before the interval flag
- `-h`, `--help` — usage

```sh
./target/release/macswapi 20            # top 20 (own procs without sudo)
sudo ./target/release/macswapi 20 -p    # system-wide + parents
sudo ./target/release/macswapi -w 5     # leak watch, 5s
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

## License

MIT
