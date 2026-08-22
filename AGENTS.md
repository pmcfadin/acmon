# acmon

Measuring what AI coding agents actually cost on a managed macOS developer machine —
and how much of that cost is the machine's fault rather than the work's. See
[`README.md`](README.md).

**Status:** v1 implementation in progress. The crate now builds **two binaries** — `amon`,
the monitor, and `agtop`, the display. **If it measures, it is `amon`; if it draws, it is
`agtop`.** `agtop` runs and is worth running, and is now the live display: full screen on
the alternate screen by default, refreshing while open by polling `state.json` once a
second and re-reading only on an mtime change, plus `agtop --once` for one pass as plain
lines. It is read-only **in fact** — `collect` takes a role, and a display's collection
writes no state and asks no notification channel anything.

**It now draws what `amon` published, and says how old every part of it is.** The display
reads the tier payloads, so a live monitor's sessions, at-risk panel and duty cycle come off
disk rather than from a collection of the display's own. It classifies the monitor itself,
from the file alone — `FRESH` / `STALE` / `DEAD` / `ABSENT` — and the word is the first thing
on the screen. The verdict rests on **two** observations and says which: the writer pid the
file records, asked of the kernel with signal 0, and each tier's own stamp against the
cadence the monitor published. A pid that is gone is `DEAD` however young the file it left;
a tier that has missed a whole pass is `STALE` while its writer is still there, and the two
get different sentences because "alive but slow" and "gone, and everything here is a corpse"
are opposite facts. Marking is **whole-screen, never per-row**. Every tier prints two ages —
when it last ran and how old its oldest fact is — because for the slow tier those differ by
tens of minutes, and the at-risk panel is aged by the **slow** tier's evidence with a
per-workspace age on every row. It never signals, restarts or tidies up after a monitor it
has declared dead. With nothing published it still makes its own single collection (F28), and
states on screen that the figures are its own and that nothing is being recorded or alerted.

It is also laid out: a **meter row** of gauges above
the table carrying collection overhead and `amon`'s duty cycle, session rows ordered by
**child CPU descending** with no sort keybindings (deliberately — F55, N1), and a session
whose child CPU is unmeasurable listed **first** rather than last, because the cheap end of
the table is what a short terminal drops. A terminal too short drops session rows from that
end and states how many are hidden; the at-risk panel and every warning under it are not
candidates, and where the terminal cannot hold even those, the top line says the bottom is
cut. One function decides what fits — `render::fit` — and the drawing obeys it.

**`amon watch` is a real monitor.** It takes an exclusive `flock` in the state directory (a
second instance is refused by name), then drives all three tiers from **one loop** — fast
signals through `libproc`, the filesystem searches, and `git` plus Codex — each on its own
interval, idling down when no session is live and rising on the first one seen. It publishes
`state.json` per tier, each tier with its own timestamp, and it **meters itself**: own CPU,
duty cycle over the trailing minute, and per-tier pass durations, published with everything
else it measures. `SIGTERM` and `SIGINT` stop it cleanly and release the lock.

**Every launch is on the record.** `amon watch` appends one line to `starts.jsonl` before its
first state write, saying how long nothing was being recorded (measured from the previous
monitor's last `state.json` write, never from a shutdown record a killed monitor would never
have written), whether the run before it exited cleanly (from the lock's pid record, which a
clean release clears), and how long that run lasted. Three short runs in the last five launches
publishes a **cycling** verdict. The same record is republished in the fast tier's payload as
`launch`, so a display can show the restart count without reading `starts.jsonl`, and
`amon status` reports it — launchd's own run count is now reported *only* where the durable
record cannot answer. A first launch reports its previous exit as `absent`, never `clean`.

**`amon install`/`uninstall`/`status` are built** — one per-user LaunchAgent at
`~/Library/LaunchAgents/io.github.pmcfadin.acmon.plist`, which is the only file this tool
writes outside `~/.config/acmon/` and `~/.local/state/acmon/`; the load is verified with
launchd rather than assumed, and an install that could not be confirmed removes its own
plist. **No test may write to the real `~/Library/LaunchAgents` or register a job with a
real login session** — `ACMON_LAUNCH_AGENTS_DIR` relocates the directory and
`ACMON_LAUNCHCTL` replaces `launchctl`, and any verb using the latter says so in its own
output. The v2 verbs still fail loudly rather than exiting zero having done nothing, and
`amon --help` names the ticket that will deliver each one.

**Ask GitHub which tickets are open and unblocked** — that is authoritative, and any list
written here goes stale. Carried-forward notes GitHub would not tell you:

- #13 records a #2 criterion met in effect rather than in letter.
- **`ACMON_STATE_DIR` and `ACMON_CONFIG_DIR` between them now move everything a run touches**, so
  a test that spawns either binary needs no third variable to keep it off the developer's own
  files — which is why the hand-written `ACMON_STATE` redirection is gone from seams 13, 16 and 17.
  The memory file is `memory.json` in the state directory, not `state.json`: that name belongs to
  the tiered file the monitor publishes. `ACMON_STATE`, `ACMON_NOTIFY_CONFIG` and `ACMON_DETECTORS`
  still name their files outright. A machine with a pre-split `~/.acmon/` has it **read** while the
  new location has none, and the run says on screen that it did; nothing is moved, deleted, or
  written there ever. Naming either directory takes `~/.acmon/` out of play entirely, which is what
  makes a relocated run isolated rather than nearly isolated.
- The collection's ~2.5 s against a one-second budget **is fixed**, by tiering rather than by
  making anything faster: `amon watch` now measures its own cost at **0.37–0.47% of one core**
  over the trailing minute, inside NF9's 1%. A fast pass is ~49 ms, a medium pass ~207 ms, and
  the slow tier reads a budgeted slice of workspaces per pass rather than sweeping all of them.
  Three consequences to know before reading a figure off disk. A session row is up to one fast
  interval old, and its liveness rests on transcript activity read by the **medium** tier, so
  that evidence ages against the medium stamp rather than the fast one. Each workspace in the
  at-risk panel carries **its own** age, and because `git status` costs ~59 ms a full refresh of
  ~70 repositories takes 20–30 minutes inside the budget — workspaces not yet read are counted
  and published, never reported as clean. And a duty cycle read from a run shorter than the slow
  interval is inflated by the once-only first round, so a short run is not a steady-state
  measurement.
- `agtop`'s **own** collection (F28) is still one untiered pass and still costs seconds —
  measured at 4.7 s on a loaded machine, well past its own gauge's scale. Tiering is a property
  of the monitor's loop; there is nothing to tier in a single one-shot read. It is now reached
  **only** when there are no published facts to draw: no state file, a monitor that has not
  finished a fast pass, or a file that cannot be believed. With a monitor publishing, the
  display collects nothing at all.
- A tier is called `STALE` after it has missed a whole pass — `display::MISSED_A_PASS`, two
  intervals — not the instant it is late. One interval old is a tier *due now*, which is the
  ordinary state of the fast tier for most of every interval, and flipping the word on at that
  boundary would blink it every ten seconds until a reader stopped seeing it.

## Orientation

Read these before proposing anything:

| Document | What it is |
| --- | --- |
| [`docs/observability-mechanics.md`](docs/observability-mechanics.md) | The evidence base. What is observable on macOS, how, at what cost, and what is provably *not* observable. |
| [`docs/PRD.md`](docs/PRD.md) | Requirements, scope phasing (v1/v2/v3), and the decision record. |
| [Issue #1](https://github.com/pmcfadin/acmon/issues/1) | The v1 spec — scope, user stories, testing decisions. |
| [Issues #2–#12](https://github.com/pmcfadin/acmon/issues) | The v1 tickets, with native blocking edges. Ask GitHub which are unblocked rather than trusting a list here. |

## Working rules specific to this project

**Fail loud, never fail to zero.** An unmeasurable value is reported as absent with a
stated reason — never `0`, never an empty string that reads as a healthy negative
result. Every defect found in the tool this project replaces was a violation of this
rule, and each produced a calm, plausible, wrong answer instead of an error.

**Never assert absolute timings in a test.** Measurements on this class of machine
vary by roughly 2x between runs; only *ratios* and *invariants* reproduce. A test
asserting "cold exec takes 79 ms" fails for reasons unrelated to correctness.

**A cost budget is not an exception to that rule.** NF9's 1%-of-a-core budget has to be
measured against something, and the suite oversubscribes 16 cores, so the same monitor
measures 0.4% alone and over 1% while the suite runs. `tests/seam16_tiering.rs` shows the
shape that works: assert the ratio unconditionally, and judge the absolute figure only when
the recorded load average says the machine was not oversubscribed — printing what it saw
either way. Every sample carries the load it was taken under precisely so this decision can
be made afterwards rather than guessed.

**Run the suite with `cargo suite`, not `cargo test`.** `cargo test` stops at the first
failing test binary, so one red seam silently skips every later one — that hid 80 of 203
tests once, and the run still read as "123 passed, 1 failed". `cargo suite` is the alias
in [`.cargo/config.toml`](.cargo/config.toml) for `cargo test --no-fail-fast`. A partial
run is never evidence that the rest is green.

**No test may assert which checkout it is running in.** Work here happens in linked
worktrees under `.claude/worktrees/<name>`, where `.git` is a file rather than a
directory. The suite has to pass there and in the primary checkout, so a test that cares
about the difference establishes the expected value independently and asserts the
invariant — the same family as the rule about absolute timings.

**Assert success before believing a measurement.** Two measurements behind the
mechanics document were void because test binaries never actually executed while
still reporting plausible timings. Check exit codes first.

**Time values from `proc_pid_rusage` are mach ticks, not nanoseconds.** Read the
conversion from `mach_timebase_info()`. Assuming nanoseconds understates every
duration by ~41.67x on Apple Silicon while staying internally consistent, which makes
it hard to notice.

**This repo pays the tax it measures.** Every rebuild produces a new binary that must
be re-authorised by the security stack. Avoid reflexive clean builds. Never write a
one-off script to disk and execute it — pipe it to the interpreter's stdin instead.

**The tool observes; it never acts.** It must not restart, kill, or signal an agent. A
stalled session holding uncommitted work has to be inspected by a human before
anything touches it.

## Agent skills

### Issue tracker

GitHub Issues on `pmcfadin/acmon`, via the `gh` CLI. See
[`docs/agents/issue-tracker.md`](docs/agents/issue-tracker.md).

### Triage labels

The five canonical roles, label strings unchanged. See
[`docs/agents/triage-labels.md`](docs/agents/triage-labels.md).

### Domain docs

Single-context — `CONTEXT.md` and `docs/adr/` at the repo root. See
[`docs/agents/domain.md`](docs/agents/domain.md).
