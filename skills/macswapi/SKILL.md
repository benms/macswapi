---
name: macswapi
description: "Answer macOS memory questions with the macswapi CLI: what is using memory, compressed memory or swap, which app or process is leaking or swapping in right now, and who is close to a jetsam kill. Use before running any `macswapi` command. Not for editing macswapi's source code."
---

# macswapi

macswapi is a read-only macOS CLI that reports per-process memory: exact
compressed memory and physical footprint, an estimated share of swap, exact
bytes swapped in, and growth over time. It never changes anything on the
machine, so every command below is safe to run without asking. Done means the
user's question is answered from macswapi's own output, with its numbers.

## 1. Check it is there

Run `macswapi --help`. If it is not found, say so and offer one of these, and
install only after the user agrees:

- `brew tap benms/macswapi https://github.com/benms/macswapi && brew install --HEAD macswapi`
- from a clone: `./install.sh --prefix ~/.local`

If a flag below is rejected, the installed version is older: trust
`macswapi --help` over this skill.

## 2. Rules

1. **Never start a watch that waits for a key.** `macswapi -w` without `--csv`
   draws a full-screen view and runs until `q`. Use one of the bounded forms:
   - `macswapi --once 5`: one 5-second interval, then exit.
   - `macswapi -T 3 | cat`: one frame of the `--top` dashboard. Piped, it
     prints after the first interval and exits; never run `-T` on a terminal.
   - `macswapi 10 -w 2 --csv | head -n 51`: streamed CSV, ended by `head`
     after 5 intervals of 10 rows (1 header line + rows × intervals). With
     `--name` or `--user` there may be fewer rows per interval, so it runs
     longer; prefer `--once` there.
   - `macswapi -w 5 --alert 'RULES' --for SECS --exit-on-alert` only when the
     user wants to wait for a condition; it can run for a long time.
2. **Don't run sudo yourself.** Without root, macswapi reads only the user's
   own processes and reports the rest as `denied`. If the answer needs other
   users' or system processes (the footer says how many were denied), give the
   user the exact `sudo macswapi …` command to run themselves.
3. **Don't kill or quit anything.** macswapi only observes. If a process looks
   like the culprit, report it and leave the decision to the user.
4. **Report estimates as estimates.** `SWAP~` is an estimate (macOS keeps no
   per-process swap counter); `CMPRS`, `FOOTPRINT` and `SWAPINS` are exact.

## 3. Questions and commands

| question | command | read |
|---|---|---|
| How is memory overall? | `macswapi -T 3 \| cat` | one dashboard frame (piped, it exits): pressure verdict, memory, compressor, swap, top apps, growing |
| What is using my memory? | `macswapi 10 -a --sort foot` | apps with their helpers rolled up: `PROCS`, `FOOTPRINT` |
| What is using swap / compressed memory? | `macswapi 10 -a` | `CMPRS`, `SWAP~`; footer: unattributed share |
| Who is swapping right now? | `macswapi --once 5` | `swapped in during interval: … — name pid %` |
| Is something leaking? | `macswapi 10 -w 3 --csv \| head -n 61` | per pid over the intervals: `delta` > 0 every time, `grew_pct` 100 |
| Which process is near a jetsam kill? | `macswapi 5 --sort headroom -x` | smallest `HEADROOM` first; `-` means no limit (or at a soft one) |
| Everything about one process | `macswapi --name NAME` → `macswapi --pid PID` | detail page: ledgers, peaks, responsible app |
| Data for a spreadsheet or script | `macswapi 0 --csv` | every column, raw bytes |

Use `-t` instead of `-a` for parent/child process trees. `-a` groups by the
app macOS holds responsible, which also brings in XPC helpers whose parent is
launchd (WebKit content processes, media services); shells started in a
terminal count toward the terminal app.

## 4. Reading the output

- The summary on top: `pressure normal|warn|critical`, compressor size and
  ratio, swap used. `swap 0B used` means no process has anything in swap, so
  every `SWAP~` is 0.
- `CMPRS` and `FOOTPRINT` overlap and are not additive: compare processes on
  one column, never add the two.
- One interval of growth is allocation churn. A leak is *sustained* growth:
  `GREW%` at 100 over several intervals, or `delta` positive in every streamed
  interval.
- `SWAPINS` is lifetime traffic read back from swap, not what sits in swap now;
  in watch mode it is the growth over the interval.
- Footer lines: how many processes were shown, matched and denied; with
  denials, the answer may be missing system processes (see rule 2).

Answer with the few rows that matter, their numbers, and what they mean for
the user's question. Mention the denied count when it could change the answer.
