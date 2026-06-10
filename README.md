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
swap pressure originates). The estimates sum to the real total to the byte —
enforced by a unit test.

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

**kernel_task excluded.** `task_for_pid(0)` is always denied, even under
`sudo`, so kernel-compressed memory never appears in the denominator. User
processes are therefore slightly over-attributed — the gap is small in practice
but grows under kernel memory pressure.

## Build

```sh
cargo build --release
cargo test  --release
```

No `[dependencies]`. All kernel APIs come via `extern "C"` from libSystem at
link time → network-free build.

## Usage

```
macswapi [N] [-p|--parent] [-w|--watch [SECS]]
```

- `N` — top N rows (default 20, `0` = all)
- `-p`, `--parent` — add PPID + parent-name columns
- `-w`, `--watch [SECS]` — leak mode: footprint delta + rate/s over an interval (default 3s)
- `-h`, `--help` — usage

```sh
./target/release/macswapi 20            # top 20 (own procs without sudo)
sudo ./target/release/macswapi 20 -p    # system-wide + parents
sudo ./target/release/macswapi -w 5     # leak watch, 5s
```

## Privileges

`task_for_pid` on another user's process needs **root**. Without `sudo`, only
your own processes are read, and the swap-share denominator covers only those
(denial count is shown). With `sudo`, most processes are readable
(SIP-protected ones still deny). Compressed/footprint are exact either way.

## License

MIT
