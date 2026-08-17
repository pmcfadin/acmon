# PRD — Agentic Coding Monitor

**Status:** Draft for review
**Date:** 2026-08-16
**Name:** `acmon` — the binary, the crate, and the eventual Homebrew formula
**Evidence base:** [`observability-mechanics.md`](./observability-mechanics.md) — every
capability claimed here was measured; anything unverified is labelled.

---

## 1. Summary

A macOS tool that answers three questions about AI coding agents on a developer
machine:

1. **What are my agents doing right now, and is any of them stuck or dead?**
2. **What is each session actually costing** — CPU, memory, disk, money?
3. **How much of that cost is the machine's fault** rather than the work's?

It ships as a single Rust binary, distributable via Homebrew. It observes and
notifies; it never restarts, kills, or signals an agent.

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
- **G6** Run on a new Mac with one `brew install` and no configuration for the
  measurement half.

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
- **N6** A daemon that holds state or makes decisions.

---

## 4. Domain model

| Term | Definition |
| --- | --- |
| **Workspace** | A directory an agent works in. May or may not be a git worktree; git-ness is an *attribute*, not a prerequisite. Replaces the earlier "Lane". |
| **Session** | One agent CLI process with its own transcript. Rows in the UI are Sessions. |
| **Turn** | One user request, identified by `prompt.id`. The natural unit for cost and outcome. |
| **Attribution** | The link between a Session and a Workspace. **Many-to-many and frequently absent.** |
| **Detector** | Data describing how to recognise one agent CLI's processes. |
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
                    ┌──────────────┐
   Claude Code ────▶│   receiver   │──── spool (JSONL, size-capped)
   (OTLP/JSON)      │  :4318 only  │            │
                    └──────────────┘            ▼
                                        ┌───────────────┐
   libproc / ps ───────────────────────▶│   collector   │──▶ state.json (atomic)
   transcripts / git ──────────────────▶│  (tiered)     │──▶ registry.json
                                        └───────────────┘──▶ events.jsonl
                                                │
                    ┌───────────────┐           │
   fast cadence ───▶│      TUI      │◀──────────┘
   slow cadence ───▶│   launchd     │──▶ notify (verified) 
                    └───────────────┘
```

**Four components, deliberately unequal:**

- **receiver** — resident, dumb. Accepts OTLP `http/json` POSTs and appends to a
  spool. No logic, no state, so it can die and restart harmlessly.
- **collector** — all logic. Enumerates processes, reads ledgers, resolves
  attribution, checks git, evaluates state. Invoked, not resident.
- **TUI** — renders the last state. `ratatui` + `crossterm`.
- **launchd job** — invokes the collector on a slow cadence so notifications fire
  when the TUI is closed.

One collector, two schedulers. There is no daemon that holds state.

---

## 7. Scope

### v1 — Liveness and resources

Discovery, registry, detectors, both state machines, resources, TUI, notify.

**Proposed change to an earlier decision:** the agreed v1 slice excluded resource
metrics. I now recommend including them, because they turned out to need *no new
subsystem* — the same process enumeration that finds sessions also reads their
ledger, at microsecond cost and with no root. They are also the project's strongest
differentiator (§2.3). **This is a proposal, not a settled decision — say if you'd
rather hold v1 to liveness only.**

### v2 — Probes, findings, report

Probe suite (exec tax, security-stack attribution, repo surface, machine pressure),
findings catalog, `report` / `diff` / `trend` / `export --redact`.

### v3 — Turn-level telemetry

OTLP receiver, per-Turn cost and outcome, per-subagent accounting, hook cost,
commit correlation.

Order respects dependency: v3's latency numbers are uninterpretable without v2's
baseline.

---

## 8. Functional requirements

### Discovery (v1)

- **F1** Enumerate agent sessions from the running process set, obtaining identity
  and cwd **in one pass**. Enumerate-then-enrich is forbidden: it produced six
  phantom "unreadable cwd" entries that were merely dead processes.
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
- **F11** `WAITING` uses a direct signal where available and inference otherwise,
  and **records which method produced the verdict**, so weaker evidence reads as
  weaker.
- **F12** Never assert `STALLED` without a trustworthy process snapshot. Report
  `UNKNOWN` instead.
- **F13** Include a self-sentinel: an all-process snapshot must contain the
  collector's own pid, or it is treated as failed.

### Resources (v1, proposed)

- **F14** Report per session: own CPU, **child CPU**, current footprint, lifetime
  peak, disk read/written, instructions.
- **F15** Convert all `*_time` fields via `mach_timebase_info()`. A hardcoded
  nanosecond assumption is a 41.67x error on this hardware.
- **F16** Persist the last reading per session, so lifetime totals survive its exit.
- **F17** State plainly that orphaned/detached children are **not** counted.

### Presentation (v1)

- **F18** Rows are Sessions.
- **F19** A dedicated at-risk panel, **always visible**, listing `DIRTY-STRANDED`
  workspaces. "0 at risk" is information.
- **F20** Display collection overhead as a first-class figure.

### Notification (v1)

- **F21** Notify on transition *into* a notable state, deduped; re-notify on
  re-entry.
- **F22** Verify delivery and report channel health. **Fail closed** — an
  undelivered alert must not be recorded as sent.
- **F23** Local notification via a configurable command.

### Probes (v2)

- **F24** Exec tax: warm, cold, hardlink, APFS clone, script vs piped, size curve,
  parallel ceiling.
- **F25** **Assert every exit code before believing any timing.** Two measurements
  were void because copies of SIP-protected binaries were SIGKILLed (137) while
  reporting plausible ~1 ms timings.
- **F26** Security-stack attribution by correlation: snapshot daemon CPU, burst,
  snapshot, report deltas — presented as **correlates, not causes**, and stating
  that Falcon is unmeasured.
- **F27** Report size-curve fits with **R² and residuals**, keeping "measured floor"
  and "regression intercept" as distinct fields.
- **F28** Refuse to run timing probes on a busy machine, or stamp
  `confidence: low`. Load ranged 3–26 during development; measurements at 26 are
  meaningless.
- **F29** Findings fire on **ratios, never absolutes**.
- **F30** A finding whose inputs are missing or null renders `UNKNOWN`, never
  "no problem".
- **F31** Validate every catalog expression against the metric namespace **at load
  time**, so a typo fails at startup rather than masquerading as `UNKNOWN` forever.
- **F32** `export --redact`: hash the hostname, collapse paths to depth-1
  categories, retain all numbers and the extension inventory.

### Telemetry (v3)

- **F33** Accept OTLP `http/json` on localhost. No protobuf required.
- **F34** Treat `/v1/logs` as the primary feed. The useful data is events, not
  metrics; `bash.subprocess`, `tool.execution` and `blocked_on_user` were **never
  observed**.
- **F35** Aggregate by `prompt.id` to form Turns.
- **F36** Classify Turn outcomes three ways (§4). Never equate "no commit" with
  waste.
- **F37** Never enable `OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_TOOL_CONTENT`,
  `OTEL_LOG_TOOL_DETAILS`, `OTEL_LOG_ASSISTANT_RESPONSES`. Assert values arrive
  `<REDACTED>` at ingest; drop any record that is not.
- **F38** Do not build metrics with `lines_of_code` as a denominator.

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
- **NF6** Tiered cadence: near-free signals fast, `lsof`-class medium, git and
  Codex slow. Full 34-workspace git sweep costs 2.7 s and belongs in the slow tier.
- **NF7** Use `--no-optional-locks` for every git invocation; plain `git status` may
  write the index and contend with the agent being observed.
- **NF8** Cumulative counters are monotonic, so **sampling cadence does not affect
  their accuracy.** Do not trade fidelity for cost where none is required.

### Correctness

- **NF9** **Fail loud, never fail to zero.** An unmeasurable value is `null` plus a
  reason. This is the single most important rule; every defect found in the existing
  tooling was a violation of it.
- **NF10** Distinguish verified measurement, inference, and assumption in all
  output.

### Distribution

- **NF11** Single Rust binary, no runtime dependencies.
- **NF12** `probe` must work on a fresh managed Mac with no configuration.
- **NF13** Embedded default catalog plus `~/.config/acmon/` overrides, surviving
  `brew upgrade`.
- **NF14** No `sudo` anywhere in the core paths.

---

## 10. Decision record

| # | Decision |
| --- | --- |
| 1 | One tool; TUI default, `probe`/`report` as subcommands |
| 2 | Feeds: liveness, machine pressure, latency. Cost delegated to `ccusage` |
| 3 | Rows are Sessions |
| 4 | Dedicated at-risk panel |
| 5 | Read-only + notify; never auto-recover |
| 6 | Codex: `session_meta` only, recently-active only |
| 7 | Probes portable; monitor may assume local config |
| 8 | One collector, two schedulers |
| 9 | Tiered cadence + self-metering |
| 10 | Rust, `ratatui` + `crossterm`, Homebrew |
| 11 | Embedded catalog + user override layer |
| 12 | Dumb resident receiver + periodic collector |
| 13 | JSON + JSONL, no database |
| 14 | `evalexpr` + wrapper (null→UNKNOWN, load-time validation) |
| 15 | Process-first discovery + persistent workspace registry |
| 16 | Detectors as data |
| 17 | Direct WAITING signal where available, method labelled |
| 18 | Split Session / Workspace state machines |
| 19 | Built-in verified publish + configurable local command |
| 20 | v1 = liveness slice (**§7 proposes adding resources**) |

`ratatui` was asserted rather than chosen — flag if you disagree.

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
- **S6** Total collection overhead is displayed and under 1 s for the fast tier.
- **S7** Every probe/collector path pointed at a nonexistent target returns `null`
  plus a reason — never `0`.

**v2 ships when:** a `--redact`ed profile can be handed to IT stating cost per cold
exec, the ratio, and the ranked extension attribution with Falcon marked unmeasured.

**v3 ships when:** a Turn's total cost is reportable, classified three ways, with
per-subagent tokens and duration.

---

## 12. Risks and open questions

| Risk | Impact | Mitigation |
| --- | --- | --- |
| `blocked_on_user` may not exist in practice | `WAITING` stays inferential for both CLIs | Test interactively before building on it; inference already works |
| Orphaned children escape accounting | Per-session CPU understated by an unknown margin | Measure the rate; state the limitation in output |
| Falcon unmeasurable | Attribution covers only the visible portion | Say so explicitly wherever shares are shown |
| Commit→Turn linkage is timestamp-based | Outcome attribution is approximate | Ship session-level first (exact); flag turn-level as estimated |
| Cost/MB measured 4.4x above the prior model | Findings could mis-price the tax | Multiple runs; report residuals; never a single model |
| OTel schema is undocumented and may change | v3 breaks on a Claude Code update | Version-tag ingest; degrade to UNKNOWN on unknown fields |
| Codex has no telemetry | Turn-level data is Claude-only | Git outcomes cover Codex retrospectively |
| Dev-loop exec tax on this repo | Every `cargo build` is a fresh inode | Apply the project's own discipline; never reflexive `cargo clean` |

**Open, needing a decision:** whether to adopt resources into v1 (§7 proposes yes);
whether to pull git outcomes into v1 or wait for v3; which license.

---

## 13. Out of scope

Hooks. Config injection. Agent telemetry for Codex. Network/VPN probes. Linux. A
stateful daemon. Auto-recovery. Per-extension causal attribution. Token/cost
accounting as a product. Cross-machine catalog sync.
