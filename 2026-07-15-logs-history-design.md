# Memory-event log history (`--logs`) — design

2026-07-15

## Problem

macswapi estimates per-process swap because macOS keeps no per-PID swap
counter. The unified log cannot improve that estimate — swap-outs happen at
compressor-segment level and are never attributed to a PID — but it does hold
swap-adjacent *events* useful for post-mortem context ("why did swap spike at
2pm?"). This feature surfaces that event history.

## Interface

```
macswapi -l [HOURS]
macswapi --logs [HOURS]
```

Queries the last N hours (default 1) of the unified log, prints a
chronological timeline of memory events, exits. Mutually exclusive with
`--watch`. (`-h` is already `--help`, hence `-l`.)

```
macswapi -f json --logs 4
macswapi --format text --logs
```

`-f` / `--format` selects `text` (default) or `json`. Passing `--format`
without `--logs` is a usage error.

Text output:

```
TIME      TYPE      DETAIL
-- 2026-10-21 --
10:14:14  jetsam    memorystatus: killing_specific_process pid 58521 [knowledgeconstructiond] (per-process-limit 0 0s rf:- type:daemon) 20192KB - memorystatus_available_pages: 1820483
20:09:19  jetsam    memorystatus: killing_idle_process pid 414 [sandboxd] (idle-exit 0 90330s rf:- type:daemon) 3168KB - memorystatus_available_pages: 789885 compressor_size:795015
```

DETAIL is the raw kernel message.

JSON output: array of objects, one per event:

```json
{"time": "2026-07-15 14:02:11", "type": "pressure|swapfile|jetsam",
 "pid": 812, "process": "Xcode", "message": "<raw kernel message>"}
```

Text output shows `HH:MM:SS` and inserts a date separator line when the day
changes (queries can span days); JSON always carries the full timestamp.

`pid`/`process` present only for jetsam events. JSON is *emitted* with
stdlib string formatting (+ escaping); we never parse JSON, preserving the
zero-crates constraint.

## Event types

| type | source | content |
|---|---|---|
| `pressure` | kernel memorystatus | pressure level transitions (normal / warn / critical) |
| `jetsam` | kernel memorystatus kills | pid, process name, footprint at kill time — the only per-process data the log has |
| `swapfile` | kernel vm | swapfile create/delete — timestamps when system-wide swap grew/shrank |

## Data flow

One subprocess:

```
log show --last <N>h --style compact --predicate '<compound>'
```

The predicate matches kernel/memorystatus senders and swap/pressure/jetsam
keywords. Each output line is keyword-classified into one of the three event
types; jetsam lines additionally get pid + process name extracted. The kernel
logs each idle kill twice (`killing (idle) NAME [PID] …` then
`killing_idle_process pid PID [NAME] …`); the first form is skipped. The
kernel's message text is otherwise passed through raw — deliberately shallow
parsing, so message-format drift across macOS versions degrades gracefully
(if pid/name extraction fails, the event still prints, minus those fields).
Events print in log order (already chronological). Text output streams as
`log show` produces lines; JSON prints once the query finishes.

Spawning `/usr/bin/log` (a platform binary) is in the same spirit as the
existing sysctl calls; no crates added.

## Error handling

- `log show` missing or non-zero exit → clear error to stderr, exit 1.
- Zero matching events → "no memory events in last N hours" (text) / `[]`
  (json), exit 0.
- Bad HOURS (0, non-numeric) → usage error, same as existing flag validation.

## Testing

Classify/extract functions are pure (line in → event out); unit-tested with
captured real log lines as fixtures, same pattern as existing pure-helper
tests in `main.rs`. During implementation, run `memory_pressure -l warn` to
capture genuine kernel messages on the current macOS version and pin the
predicate keywords + fixtures to reality. The thin `log show` wrapper stays
untested glue, matching how sysctl calls are handled today.

## Non-goals

- Improving SWAP~ estimate accuracy (logs can't).
- Live event annotations in watch mode (may come later; the classifier core
  would be reusable).
- Full parsing of every kernel message variant.
