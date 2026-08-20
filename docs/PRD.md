# PRD — Agentic Coding Monitor

**Status:** Draft for review
**Date:** 2026-08-19
**Name:** `acmon` — the project, the crate, and the eventual Homebrew formula. It
installs **two binaries**: `amon`, the monitor, and `agtop`, the display.
**Evidence base:** [`observability-mechanics.md`](./observability-mechanics.md) — every
capability claimed here was measured; anything unverified is labelled.

---

## 1. Summary

A macOS tool that answers three questions about AI coding agents on a developer
machine:

1. **What are my agents doing right now, and is any of them stuck or dead?**
2. **What is each session actually costing** — CPU, memory, disk, money?
3. **How much of that cost is the machine's fault** rather than the work's?

It observes and notifies; it never restarts, kills, or signals an agent.

### 1.1 Two binaries, one rule for remembering which

**If it measures, it is `amon`. If it draws, it is `agtop`.**

- **`amon`** — the monitor. Resident under launchd. Owns every collection tier, is the
  only writer of state, and is the only thing that notifies. Also carries the one-shot
  verbs: `probe`, `report`, and its own `install`/`uninstall`/`status`.
- **`agtop`** — the display. Read-only. Polls the state `amon` writes and renders it,
  full-screen or as a single pass of lines.

The split exists for two reasons, both already on the board as open work:
notifications must fire when no terminal is open (issue #11), and the display wants a
fast refresh while the git sweep costs 2.7 s and cannot have one (issue #10). One
invocation cannot serve both.

The split is a **deployment boundary, not a code boundary**. Both binaries link the
same collection library, because `agtop` must be able to collect for itself when no
monitor is running (F28). Two names, one crate, one formula.

---

## 2. Problem

### 2.1 Sessions die silently and take work with them

An agent session cannot report its own death. Detection must live outside the
sessions being watched. Three lanes were lost in one day before any detector
existed, and a workspace holding 27 uncommitted files was deleted minutes after
sitting unflagged.

**Still true today:** of 34 git workspaces, **12 hold uncommitted changes** while
only **2** have a live session. Roughly ten workspaces are holding unsaved work
that nothing is minding.

### 2.2 The existing detector fails silently

`agent-watchdog.sh` has four measured defects (mechanics §7). Two are serious: its
Codex patterns match the ChatGPT desktop app and the Computer Use helper while
**missing the real Codex CLI entirely**. Because a resident session downgrades
`STALLED` to `WAITING`, and the ChatGPT app is effectively always running, the tool
goes quiet at exactly the moment a session dies.

All four defects fail *open* — they produce calm-looking answers, not errors.

**This section now applies to us.** A resident `amon` is a new way to produce a
calm-looking wrong answer: a monitor wedged forty minutes ago leaves a `state.json`
that reads as perfectly healthy. Everything in §6.2 and F21/F29/F30 exists because of
this paragraph, not in spite of it.

### 2.3 Cost is measured wrongly, or not at all

Existing tooling counts tokens. Nothing measures where the time actually goes.

Measured across five live sessions: **14.3 CPU-hours**, ~4.1 GB resident. Session
69046 spent 1,669 s in the agent process and **32,317 s in its children** — a
**19.4x** undercount for anything watching only the agent. Sessions also differ in
*shape*: 69046 delegates nearly everything, 264 does most of its own work.

### 2.4 The machine imposes a large, invisible tax

A newly built binary costs **193.6 ms** to first execute versus **5.1 ms** warm — a
**38x** penalty, re-paid on every rebuild because the cache is keyed on inode.
Attribution of the visible portion: **XProtect 42%, Jamf 36%**, Gatekeeper 8%,
Zscaler 8%. Falcon's contribution is unmeasurable.

Authorization is serialized machine-wide: 12 concurrent cold execs on 16 cores
yield **1.25x**. One lane's cold build stalls exec for every other agent.

---

## 3. Goals and non-goals

### Goals

- **G1** Never let a workspace holding uncommitted work sit unnoticed.
- **G2** Detect a dead or stuck session without relying on the session to report it.
- **G3** Attribute resource cost per session accurately, including child processes.
- **G4** Separate machine tax from real work, so waste is actionable.
- **G5** Produce numbers credible enough to hand to IT.
- **G6** Run on a new Mac with one `brew install` and no hand-editing of files. The
  monitor is enabled by `amon install`, which writes and loads the LaunchAgent itself.
  Editing a plist by hand is not "no configuration".
- **G7** Be answerable to its own thesis. A resident process that cannot state its own
  duty cycle is exactly what this tool would flag on someone else's machine.

### Non-goals

- **N1** Auto-recovery. A stalled session holding uncommitted work must be inspected
  *before* resuming, or the restart overwrites what the dead session left behind. An
  unattended resumer is a work-destroyer.
- **N2** Token/cost accounting as a product. `ccusage` (17.9k stars, actively
  developed) does this across both CLIs. Read it or shell out; do not rebuild it.
- **N3** Per-extension *causal* attribution. Impossible with SIP enabled.
- **N4** Modifying agent configuration. No installing hooks, no editing
  `CLAUDE.md`/`AGENTS.md`, no injecting rules.
- **N5** Linux support.
- **N6** A monitor whose truth lives only in memory, or that decides anything about the
  agents it watches. `amon` is resident, which an earlier draft of this document
  forbade outright. What that ban was protecting is kept in full: durable state is
  written to disk atomically so a crashed monitor loses nothing and anything can read
  the truth without asking a process for it, and the monitor still only observes and
  notifies (N1). What it was needlessly forbidding — a long-lived process — is now
  required, because F35's dedupe and issue #11's closed-terminal alerts cannot be done
  by a process that exits.
- **N7** A watchdog for the watchdog. launchd is the supervisor; a second job watching
  the first can die just as quietly, and then there are two silent failures instead of
  one. Gaps are made **visible** (F23) rather than prevented by another turtle.

---

## 4. Domain model

| Term | Definition |
| --- | --- |
| **Workspace** | A directory an agent works in. May or may not be a git worktree; git-ness is an *attribute*, not a prerequisite. Replaces the earlier "Lane". |
| **Session** | One agent CLI process with its own transcript. Rows in the display are Sessions. |
| **Turn** | One user request, identified by `prompt.id`. The natural unit for cost and outcome. |
| **Attribution** | The link between a Session and a Workspace. **Many-to-many and frequently absent.** |
| **Detector** | Data describing how to recognise one agent CLI's processes. |
| **Tier** | A group of signals collected on a shared cadence, because they share a cost class. Every fact in the state file belongs to exactly one Tier and carries **that Tier's** age, not the file's. |
| **Probe** | A deterministic measurement of the machine. |
| **Profile** | The output of a probe run: machine facts plus measurements, timestamped. |
| **Finding** | A catalog entry pairing a predicate over a Profile with a payoff and an action. |

`WORK-EXT` and `NO-SESSION` are **retired** — each was a symptom of describing a
Workspace in a Session's vocabulary.

### State machines (two, orthogonal)

**Session:** `ACTIVE` · `WAITING` · `STALLED` · `UNKNOWN`

**Workspace:** `CLEAN` · `DIRTY-DRIVEN` (uncommitted work, live session present) ·
`DIRTY-STRANDED` (uncommitted work, nothing driving it — the at-risk case) ·
`UNKNOWN` (git unreadable)

### Monitor presence, as seen by the display

Not a state machine of the monitor — a classification the *display* makes, from the
data alone, so that a dead monitor cannot present itself as a healthy one:

`FRESH` (age within the tier's expected cadence) · `STALE` (age beyond it; the writer
still exists) · `DEAD` (the recorded writer pid is gone) · `ABSENT` (no state file at
all; `agtop` is doing its own live read).

### Turn outcome

`COMMITTED` · `CONCLUDED` (ended normally, nothing committed) · `ABANDONED` (error or
interrupted). Only `ABANDONED` is unambiguous waste — many legitimate turns produce
no commit.

---

## 5. Data sources

| Source | Gives | Cost | Root? |
| --- | --- | --- | --- |
| `libproc` process enumeration | pid, exe path, cwd — no subprocess | microseconds | no |
| `proc_pid_rusage` | own + **child** CPU, footprint, peak, disk I/O, instructions | microseconds | no (own uid only) |
| `ps -o time=,rss=` | cumulative CPU for root-owned daemons | one subprocess | no |
| Claude transcript paths | Workspace attribution, liveness by mtime | cheap | no |
| Codex `session_meta` (line 1 only) | `cwd`, `cli_version`, `context_window`, `git` | ~20 ms/session | no |
| Codex `session_index.jsonl` | liveness (`updated_at`); **no cwd** | 28 ms | no |
| `git status --porcelain --no-optional-locks` | dirty state per Workspace | median 59 ms | no |
| `git log` + `Co-Authored-By` trailer | retrospective, CLI-agnostic outcomes | cheap | no |
| OTLP `http/json` receiver | per-tool latency, per-turn cost, per-subagent tokens, commits | push | no |
| `systemextensionsctl list` | security extension inventory | cheap | no |

**Everything needed runs unprivileged.** Root buys only `taskinfo`, `footprint`, and
`powermetrics`, none of which are required.

---

## 6. Architecture

```
                                      ┌──────── amon ─────────────────┐
   Claude Code (OTLP/JSON) ──────────▶│  listener  :4318 (v3)         │
                                      │                               │
   libproc / ps ─────────────────────▶│  one loop, all tiers          │
   transcripts / git ────────────────▶│  fast · medium · slow         │
                                      │  idles down at zero sessions  │
                                      └───────────────┬───────────────┘
                                                      │ sole writer, atomic
   launchd (KeepAlive) ──── supervises ──┘            │
   amon install ─────────── writes plist              ▼
                                        ~/.local/state/acmon/
                                          state.json    (per-tier stamps, writer pid)
                                          registry.json (workspaces, first/last seen)
                                          events.jsonl  (transitions)
                                          notified.json (dedupe, survives restart)
                                          starts.jsonl  (launches, downtime, exit)
                                                      │
                                                      │ read-only, poll by mtime
                                                      ▼
                                              ┌──── agtop ────┐
                                              │  full screen  │
                                              │  or --once    │
                                              └───────────────┘
```

### 6.1 Who does what

- **`amon watch`** — all logic, resident. Enumerates processes, reads ledgers, resolves
  attribution, checks git, evaluates state, notifies. One loop drives every tier, so
  there is exactly one writer and exactly one place a verdict came from.
- **`amon probe` / `amon report`** (v2) — one-shot verbs. They measure the machine, not
  the agents, and hold no state between runs.
- **`amon install` / `uninstall` / `status`** — LaunchAgent lifecycle. The only place
  this tool writes outside its own directories, and called out as such (F24).
- **`agtop`** — renders. Never writes, never notifies, never decides anything durable.
- **launchd** — supervises. It does not schedule collection any more; it only keeps
  `amon` alive.

An earlier draft had one collector invoked by two schedulers — the display on a fast
cadence and launchd on a slow one. That put two processes on the same state file for no
gain and split "which run decided this" across two lifetimes. One loop, one writer,
launchd demoted to a babysitter.

### 6.2 Why a resident monitor is a new hazard, and what answers it

| Hazard the split introduces | What answers it |
| --- | --- |
| `amon` wedges; `state.json` still looks healthy | Per-tier stamps plus writer pid in the data; the display classifies `FRESH`/`STALE`/`DEAD` and marks the **whole screen**, never guessing per row (F21, F29) |
| `amon` dies with no terminal open, so nothing notices | launchd `KeepAlive` restarts it; each launch records downtime and whether the last exit was clean, so a crash loop is a visible pattern rather than a silent gap (F23, N7) |
| Restart re-notifies every existing condition | Dedupe state is on disk. A restart notifies only real changes — but a transition that happened *during* the gap is still a transition and still alerts (F36) |
| Two `amon watch` processes interleave writes | Exclusive lock in the state dir; the second refuses, naming the pid that holds it (F19) |
| A process that never exits becomes the tax it measures | Budget stated as a duty cycle over a window, cadence idles down at zero sessions, and `amon` meters itself into the display (F22, F25, NF9) |
| No monitor running, so the display is blank or errors | `agtop` does its own live read, labelled `ABSENT`, and states plainly that nothing is being recorded (F28) |

---

## 7. Scope

### v1 — Liveness, resources, and the split

Discovery, registry, detectors, both state machines, resources, the `amon` loop with
its lock and install verbs, `agtop` with freshness classification and its layout
(sorting, the meter row, a screen too short), notify.

The split is **in v1**, not after it. It reshaped work already open (#10, #11, #14), so
deferring it would have meant building those twice.

`htop`'s lessons were weighed in #14 and are now settled: sorting and the meter row are
in v1 (decisions 36, 37), column hide/reflow is v2 (decision 35). The layout work lives
in its own ticket rather than in the display's, so "the display works" and "the display
is laid out well" fail separately.

Resource metrics are **in** v1 (decided). They need no new subsystem — the same
process enumeration that finds sessions also reads its ledger, at microsecond cost
and with no root — and they are the project's strongest differentiator (§2.3).

### v2 — Probes, findings, report

Probe suite (exec tax, security-stack attribution, repo surface, machine pressure),
findings catalog, `report` / `diff` / `trend` / `export --redact`, all as `amon`
subcommands.

### v3 — Turn-level telemetry

OTLP listener inside the `amon watch` loop, per-Turn cost and outcome, per-subagent
accounting, hook cost, commit correlation.

Order respects dependency: v3's latency numbers are uninterpretable without v2's
baseline.

---

## 8. Functional requirements

**Requirement ids are append-only from F53 and NF16 onward.** A new requirement takes the
next free number and is filed in the section it belongs to, even when that puts it out of
numeric order within the section. The two-binary split renumbered F18–F38 once; doing it
again would silently rot every reference in a ticket, a commit message or a code comment
written before the change. Grouping is what the sections are for; the number is only an
identifier.

### Discovery (v1)

- **F1** Enumerate agent sessions from the running process set. One logical observation,
  not one instant: pids are enumerated and each path is then read, because macOS offers no
  call that returns them together. A process that exits in between produces a record
  carrying `PathUnavailable::ProcessExited` — a reason established by asking, not assumed.
  Such a record is excluded when sessions are formed, so an exiting process is never
  reported as a session, nor as one with an unreadable field. Stated because the opposite
  was measured: six phantom "unreadable cwd" entries turned out to be dead processes, and a
  dead process reported as an unreadable one is indistinguishable from a live session we
  failed to see unless the reason is established rather than guessed.
- **F2** Recognise CLIs via **detectors as data** — exe-path glob plus optional argv
  pattern, with exclusions. Adding a CLI must not require a code change.
- **F3** Detectors MUST exclude `/Applications/ChatGPT.app/`,
  `/Applications/Claude.app/`, `/Applications/Cursor.app/`, and
  `~/.codex/computer-use/`. Regression tests required for all four.
- **F4** Tolerate a `comm` value that is not a path (Cursor reports
  `Cursor Helper: terminal pty-host`).
- **F5** Maintain a **persistent workspace registry**, unioning live process cwds
  with transcript-derived history, each entry carrying first-seen and last-seen.
- **F6** Age out registry entries only when clean *and* quiet.

### Attribution (v1)

- **F7** Map Workspace → Claude slug by replacing `/`, `.` **and `_`** with `-`, and
  compare **case-insensitively**. Never invert a slug; the mapping is lossy.
- **F8** For Codex, read **only line 1** (`session_meta`) of a transcript, and only
  for sessions the index shows as recently active. Never read message content.
- **F9** A session whose cwd falls under no known workspace renders as *unmanaged*,
  never dropped.

### State (v1)

- **F10** Classify Sessions and Workspaces independently per §4.
- **F11** `WAITING` is **inferred** (stale transcript + resident session + no live
  build) for both CLIs — no direct signal exists (measured). Every verdict records
  which method produced it, so an inferred state never reads as asserted, and a
  future direct signal slots in without redesign.
- **F12** Never assert `STALLED` without a trustworthy process snapshot. Report
  `UNKNOWN` instead.
- **F13** Include a self-sentinel: an all-process snapshot must contain the
  monitor's own pid, or it is treated as failed.

### Resources (v1)

- **F14** Report per session: own CPU, **child CPU**, current footprint, lifetime
  peak, disk read/written, instructions.
- **F15** Convert all `*_time` fields via `mach_timebase_info()`. A hardcoded
  nanosecond assumption is a 41.67x error on this hardware.
- **F16** Persist the last reading per session, so lifetime totals survive its exit.
- **F17** State plainly that orphaned/detached children are **not** counted.

### Monitor — `amon` (v1)

- **F18** `amon watch` is the **sole writer** of every state artefact and the **sole
  notifier**. Nothing else writes state; nothing else sends an alert.
- **F19** `amon watch` takes an exclusive lock in the state directory at startup. A
  second instance exits non-zero naming the pid that holds the lock. `--foreground`
  exists for debugging and is still subject to the lock, because two writers is two
  writers regardless of intent; `amon status` and the log are how you watch it work.
- **F20** All durable truth is written to disk, atomically, each pass. Memory holds
  only a cache. A `SIGKILL`ed monitor loses nothing but the pass in flight, and any
  reader can obtain the truth without asking a process for it.
- **F21** Every write stamps the **writer pid** and a timestamp **per tier** (§4,
  *Tier*). A single file-level timestamp is forbidden: it would describe the newest
  fact in the file and thereby misdescribe every older one.
- **F22** Cadence idles down when no live sessions exist, and rises on the first
  detected session. Most of the day there is nothing to poll.
- **F23** Each launch appends a start record: time started, elapsed time since the
  last state write (the **downtime**), and whether the previous exit was clean. This
  is how a crash loop becomes a visible pattern rather than a silent gap (N7).
- **F24** `amon install` writes and loads the LaunchAgent plist; `amon uninstall`
  unloads and removes it; `amon status` reports whether the job is loaded, whether a
  process is running, and the age of the last write. This is the **only** path in the
  product that writes outside `~/.config/acmon/` and `~/.local/state/acmon/`, and it
  must say what file it is creating before creating it.
- **F25** `amon` meters itself: its own CPU, its duty cycle over the trailing window,
  and its per-tier pass durations are collected and published like any other measured
  cost, so the monitor appears in the audit it performs (G7).

### Display — `agtop` (v1)

- **F26** `agtop` is **read-only**. It never writes state and never notifies. A
  notification from a foreground UI is redundant with looking at it, and a second
  writer would undo F18.
- **F27** Refresh by polling: `stat` the state file on a fixed interval (~1 s) and
  re-read only on an mtime change. No filesystem-event watcher — it would gain a
  fraction of a second and add a class of "the watcher silently stopped delivering"
  bug to a tool whose thesis is that silent background failure is the enemy.
- **F28** With no state file present, `agtop` performs its **own single collection**,
  renders it, labels the screen `ABSENT`, and states that nothing is being recorded or
  alerted. A blank screen or a refusal would be the "fail to zero" this project exists
  to eliminate, and it is the first thing a fresh `brew install` would hit.
- **F29** Classify monitor presence per §4 from the data alone, and mark the **whole
  screen** on `STALE` or `DEAD`, showing the age. Never a per-row judgement: a stale
  file is uniformly untrustworthy, and per-row marking implies some rows were verified.
- **F30** Show each tier's age, and show the at-risk panel's evidence age next to it.
  The panel is the highest-stakes thing on screen and is fed by the **slowest** tier;
  a workspace committed 50 s ago must not appear at-risk under a 1 s stamp.
- **F31** Rows are Sessions.
- **F32** A dedicated at-risk panel, **always visible**, listing `DIRTY-STRANDED`
  workspaces. "0 at risk" is information.
- **F33** Display collection overhead — and `amon`'s duty cycle (F25) — as first-class
  figures.
- **F34** Full-screen interactive rendering (`ratatui` + `crossterm`, alternate
  screen), plus `agtop --once` which emits the same content as plain lines. The
  one-shot mode is not a fallback: it is what keeps the renderer testable against a
  fixed buffer instead of a live terminal, and what keeps the output pipeable.
- **F54** A terminal too **short** for the whole screen drops session rows,
  **cheapest first**, and states how many are hidden. The at-risk panel is never
  truncated, and no number inside a row is ever shortened. Stated because the opposite
  axis was specified and this one was not: too *narrow* has refused to draw since the
  first table, while too *short* was simply assumed away — `required_height` computes
  what it needs and trusts that it gets it. With ten sessions and eleven at-risk
  workspaces that is roughly thirty rows, so a twenty-four-row pane is the common case
  rather than the edge. Refusing the whole screen there would be §2.2 aimed at our own
  display; a silent cut would be worse. The rule forbids *silent* truncation, and a
  stated count is not silent.
- **F55** Session rows are ordered by **child CPU, descending** — fixed, with no sort
  keybindings. Ordering by pid, as the first implementation did, carries no
  information; child CPU is the quantity §2.3's thesis rests on, so the default order
  is the answer to the question the tool exists to ask. The absence of keybindings is a
  requirement rather than an omission: interactive sorting places the display inside
  `htop`'s interaction model, and a reader who feels they are in `htop` reaches for F9
  — which N1 forbids this tool from ever honouring. The order must be deterministic
  including ties, because the existing collection sort exists for stable test output.
  A session whose child CPU is unmeasurable sorts to a stated position and is never
  treated as zero (NF10).

### Notification (v1)

- **F35** Notify on transition *into* a notable state, deduped; re-notify on
  re-entry.
- **F36** Dedupe state is persisted alongside the other state artefacts, so a restart
  does not re-announce conditions that have not changed. A condition that *became*
  true while the monitor was down **is** a transition and **must** notify — suppressing
  the first pass after a start would trade a real missed alert for a cosmetic one,
  which is backwards for a tool whose job is not missing things.
- **F37** Verify delivery and report channel health. **Fail closed** — an
  undelivered alert must not be recorded as sent.
- **F38** Local notification via a configurable command.

### Probes (v2)

- **F39** Exec tax: warm, cold, hardlink, APFS clone, script vs piped, size curve,
  parallel ceiling.
- **F40** **Assert every exit code before believing any timing.** Two measurements
  were void because copies of SIP-protected binaries were SIGKILLed (137) while
  reporting plausible ~1 ms timings.
- **F41** Security-stack attribution by correlation: snapshot daemon CPU, burst,
  snapshot, report deltas — presented as **correlates, not causes**, and stating
  that Falcon is unmeasured.
- **F42** Report size-curve fits with **R² and residuals**, keeping "measured floor"
  and "regression intercept" as distinct fields.
- **F43** Refuse to run timing probes on a busy machine, or stamp
  `confidence: low`. Load ranged 3–26 during development; measurements at 26 are
  meaningless. A running `amon` counts toward that load and must be disclosed in the
  profile.
- **F44** Findings fire on **ratios, never absolutes**.
- **F45** A finding whose inputs are missing or null renders `UNKNOWN`, never
  "no problem".
- **F46** Validate every catalog expression against the metric namespace **at load
  time**, so a typo fails at startup rather than masquerading as `UNKNOWN` forever.
- **F47** `export --redact`: hash the hostname, collapse paths to depth-1
  categories, retain all numbers and the extension inventory.

### Telemetry (v3)

- **F48** Accept OTLP `http/json` on localhost, as a listener inside the `amon watch`
  loop. Not a separate resident process: one job, one daemon.
- **F49** Treat `/v1/logs` as the primary feed. The useful data is events, not
  metrics; `bash.subprocess`, `tool.execution` and `blocked_on_user` were **never
  observed**.
- **F50** Aggregate by `prompt.id` to form Turns.
- **F51** Classify Turn outcomes three ways (§4). Never equate "no commit" with
  waste.
- **F52** Never enable `OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_TOOL_CONTENT`,
  `OTEL_LOG_TOOL_DETAILS`, `OTEL_LOG_ASSISTANT_RESPONSES`. Assert values arrive
  `<REDACTED>` at ingest; drop any record that is not.
- **F53** Do not build metrics with `lines_of_code` as a denominator.

---

## 9. Non-functional requirements

### Privacy

- **NF1** Never persist or display process **argv** — it contains prompt text.
- **NF2** Never parse `ps` text output; enumerate structurally. Argv newlines break
  line-based parsers (observed).
- **NF3** Never read agent conversation content. Codex is limited to line 1.
- **NF4** Spool and state files contain no prompt or response text.

### Cost

- **NF5** The observer competes with the observed on a serialized authorization
  path. Prefer `libproc` over subprocesses.
- **NF6** Tiered cadence, all tiers inside the one `amon` loop: near-free signals
  fast, `lsof`-class medium, git and Codex slow. A full 34-workspace git sweep costs
  2.7 s and belongs in the slow tier.
- **NF7** Use `--no-optional-locks` for every git invocation; plain `git status` may
  write the index and contend with the agent being observed.
- **NF8** Cumulative counters are monotonic, so **sampling cadence does not affect
  their accuracy.** Do not trade fidelity for cost where none is required.
- **NF9** `amon`'s cost is budgeted as a **duty cycle**, not a per-pass wall clock:
  under **1% of one core averaged over a minute** with sessions live, and effectively
  nil when idled down. A per-pass bound says nothing about the bill — 0.9 s every 2 s
  is 45% of a core, forever. The figure is measured and published (F25), not asserted.

### Correctness

- **NF10** **Fail loud, never fail to zero.** An unmeasurable value is `null` plus a
  reason. This is the single most important rule; every defect found in the existing
  tooling was a violation of it.
- **NF11** Distinguish verified measurement, inference, and assumption in all output —
  **and, equally, age**. A fact of unknown or excessive age is a fact of unknown
  reliability, and presenting an old one at a young timestamp produces exactly the
  calm, plausible, wrong answer of §2.2.

### Distribution

- **NF12** Two Rust binaries (`amon`, `agtop`) from one crate, one Homebrew formula,
  no runtime dependencies.
- **NF13** `amon probe` must work on a fresh managed Mac with no configuration.
- **NF14** Configuration lives in `~/.config/acmon/` (`detectors.toml`,
  `notify.toml`), as an override layer over an embedded default catalog, surviving
  `brew upgrade`.
- **NF15** Mutable state lives separately, in `~/.local/state/acmon/`. Split by
  mutability so that "keep my config in dotfiles" and "delete the state directory to
  recover" are both safe, obvious instructions. Deleting the state directory must lose
  history and nothing else.
- **NF16** No `sudo` anywhere, including `amon install` — a per-user LaunchAgent needs
  none.

---

## 10. Decision record

| # | Decision |
| --- | --- |
| 1 | **Two binaries from one crate:** `amon` measures, `agtop` draws. One formula |
| 2 | Feeds: liveness, machine pressure, latency. Cost delegated to `ccusage` |
| 3 | Rows are Sessions |
| 4 | Dedicated at-risk panel |
| 5 | Read-only + notify; never auto-recover |
| 6 | Codex: `session_meta` only, recently-active only |
| 7 | Probes portable; monitor may assume local config |
| 8 | **One loop, all tiers, inside `amon`. launchd supervises, it does not schedule** |
| 9 | Tiered cadence + self-metering |
| 10 | Rust, `ratatui` + `crossterm` (both, for real — full screen plus `--once`), Homebrew |
| 11 | Embedded catalog + user override layer |
| 12 | **The OTLP listener lives inside the `amon` loop; no second resident process** |
| 13 | JSON + JSONL, no database |
| 14 | `evalexpr` + wrapper (null→UNKNOWN, load-time validation) |
| 15 | Process-first discovery + persistent workspace registry |
| 16 | Detectors as data |
| 17 | Direct WAITING signal where available, method labelled |
| 18 | Split Session / Workspace state machines |
| 19 | Built-in verified publish + configurable local command |
| 20 | v1 = liveness **+ per-session resources** |
| 21 | `amon` is resident, but its truth is on disk and written atomically. N6 reworded, not deleted |
| 22 | Single writer, enforced by an exclusive lock — not merely documented |
| 23 | `agtop` is read-only and never notifies |
| 24 | `agtop` polls by mtime; no filesystem-event watcher |
| 25 | `agtop` falls back to its own live read when no monitor is present, labelled `ABSENT` |
| 26 | Freshness is a property **of the data**: per-tier stamps plus writer pid, and the display judges it. No heartbeat file — a heartbeat can be fresh while the write it was meant to prove has failed |
| 27 | Notification dedupe persisted, so a restart does not storm; a gap transition still alerts |
| 28 | `amon install` owns the LaunchAgent; the only write outside our own directories |
| 29 | Config `~/.config/acmon/`, state `~/.local/state/acmon/`, split by mutability |
| 30 | Budget as a duty cycle, with idle-down and self-metering |
| 31 | launchd `KeepAlive` is the whole supervision story; gaps made visible, no watchdog-of-the-watchdog |
| 32 | The split is a deployment boundary; both binaries link the same collection library |
| 33 | Apache-2.0 (already in `LICENSE` and `Cargo.toml`) |
| 34 | Git outcomes stay in **v3**. They are retrospective, so deferring loses nothing but a wait — at the stated cost that Codex has no outcome data until then |
| 35 | Column hide/reflow deferred to **v2**. v1 keeps refusing to draw below the minimum width rather than truncating a number — the only one of `htop`'s three lessons where doing nothing is defensible rather than a defect |
| 36 | Rows sorted by **child CPU, fixed** (F55). No sort keybindings, deliberately: interactive sorting buys flexibility and buys `htop`'s interaction model with it, and F9 is the one thing this tool must never do |
| 37 | A **meter row** above the table in v1, carrying `amon`'s duty cycle and collection overhead (F33). Chosen over a line of text so v2's machine-tax gauges move into it rather than forcing a redesign of the top of the screen |
| 38 | A too-short screen **truncates the session table with a stated count** (F54), never the at-risk panel. Refusing the whole screen would fire in the common case, not the edge |

---

## 11. Success criteria

**v1 ships when:**

- **S1** All 6 live Claude sessions and any live Codex session appear, with correct
  workspace attribution — including underscore paths (`agentic_coding_monitor`) and
  mixed-case paths (`WorkforceOS`), both of which the current watchdog misses.
- **S2** Zero false positives from ChatGPT.app, Claude.app, Cursor.app, or Computer
  Use, verified by test.
- **S3** The ~10 currently `DIRTY-STRANDED` workspaces are all listed.
- **S4** Per-session child CPU is reported and matches `ps` cross-check after
  timebase conversion.
- **S5** A killed session is reported `STALLED` within the stall threshold, and its
  last resource reading is retained.
- **S6** `amon`'s duty cycle is measured, displayed, and within NF9's budget with
  sessions live; and the cadence is verified to idle down at zero sessions. Stated as a
  ratio over a window, never an absolute per-pass timing (see the project rule on
  absolute timings in tests).
- **S7** Every probe/monitor path pointed at a nonexistent target returns `null`
  plus a reason — never `0`.
- **S8** `SIGKILL` the monitor with `agtop` open: the screen becomes `STALE` within
  one cadence and `DEAD` once the pid is reaped, showing the age. At no point are dead
  facts rendered as current.
- **S9** Restart the monitor with conditions unchanged: **no** notification fires.
  Create a `DIRTY-STRANDED` workspace while the monitor is down, then start it: the
  alert **does** fire.
- **S10** A second `amon watch` exits non-zero naming the pid holding the lock, and
  `state.json` shows a single writer pid throughout.
- **S11** From `brew install` plus `amon install` and nothing else, an alert is
  delivered with no terminal open — issue #11's condition, verified end to end.
- **S12** `agtop` with no monitor present renders a live read labelled `ABSENT` and
  states that nothing is being recorded. It never shows a blank screen or an error as
  its whole output.
- **S13** The at-risk panel displays its own evidence age, and that age tracks the
  slow tier's cadence rather than the file's newest write.

**v2 ships when:** a `--redact`ed profile can be handed to IT stating cost per cold
exec, the ratio, and the ranked extension attribution with Falcon marked unmeasured.

**v3 ships when:** a Turn's total cost is reportable, classified three ways, with
per-subagent tokens and duration.

---

## 12. Risks and open questions

| Risk | Impact | Mitigation |
| --- | --- | --- |
| ~~`blocked_on_user` may not exist~~ **confirmed absent** | `WAITING` is inferential for both CLIs | Closed by test. Inference works; F11 labels the method so inferred never reads as asserted |
| **`amon` wedges while `state.json` looks healthy** | §2.2 reproduced inside our own tool — the worst available outcome | Per-tier stamps + writer pid in the data; display classifies and marks the whole screen (F21, F29); S8 tests it |
| **`amon` dies overnight with nothing watching** | G1 and G2 silently stop holding | launchd `KeepAlive`; downtime and unclean exits recorded per launch and surfaced as a restart count (F23). Explicitly **not** solved with a second watchdog (N7) |
| **Alert storm on every reboot** | Users learn to ignore alerts, which defeats the product | Dedupe persisted to disk (F36); S9 tests both directions |
| **The monitor becomes the tax it measures** | The thesis of §2.4 turned on us | Duty-cycle budget (NF9), idle-down (F22), self-metering into the display (F25) |
| **Two writers on the state file** | Torn state and duplicate alerts, hard to see from outside | Exclusive lock, second instance refuses (F19); S10 |
| `amon install` fails or the job never loads | Silent non-monitoring that looks like installation success | `amon status` reports loaded/running/last-write-age; install verifies the load rather than assuming it |
| Orphaned children escape accounting | Per-session CPU understated by an unknown margin | Measure the rate; state the limitation in output |
| Falcon unmeasurable | Attribution covers only the visible portion | Say so explicitly wherever shares are shown |
| Commit→Turn linkage is timestamp-based | Outcome attribution is approximate | Ship session-level first (exact); flag turn-level as estimated |
| ~~Cost/MB 4.4x above the prior model~~ **was my arithmetic error** | Slope reproduces (~10 ms/MB); fixed cost doubled to ~131 ms | Tax is driven by exec *count*, not binary size. Report residuals; never a single model |
| OTel schema is undocumented and may change | v3 breaks on a Claude Code update | Version-tag ingest; degrade to UNKNOWN on unknown fields |
| Codex has no telemetry | Turn-level data is Claude-only | Git outcomes cover Codex retrospectively |
| Dev-loop exec tax on this repo | Every `cargo build` is a fresh inode — now two binaries per build | Apply the project's own discipline; never reflexive `cargo clean`. Keep both binaries thin over one library so a change rebuilds one crate, not two |

**Nothing open.**

**Closed since the last draft:** git outcomes stay in **v3** (decision 34) — v1 is already
the split and nine tickets, and git outcomes are retrospective, so nothing is lost by
waiting. The consequence to state plainly: until v3, Codex sessions have no outcome data at
all, because Codex has no telemetry and git was going to be its only source; the license
question (Apache-2.0, decision 33); whether
`ratatui` was chosen or merely asserted (chosen, with `crossterm`, decision 10 — and
`crossterm` becomes a real dependency rather than a claim, since today's code uses
`ratatui`'s `TestBackend` only).

---

## 13. Out of scope

Hooks. Config injection. Agent telemetry for Codex. Network/VPN probes. Linux.
Auto-recovery. Per-extension causal attribution. Token/cost accounting as a product.
Cross-machine catalog sync. A monitor whose truth lives only in memory. A second
resident process for telemetry ingest. A watchdog watching the watchdog. Any write by
`agtop`.
