# acmon

Measuring what AI coding agents actually cost on a managed macOS developer machine —
and how much of that cost is the machine's fault rather than the work's. See
[`README.md`](README.md).

**Status:** v1 implementation in progress. The crate now builds **two binaries** — `amon`,
the monitor, and `agtop`, the display. **If it measures, it is `amon`; if it draws, it is
`agtop`.** `agtop` runs and is worth running: it prints a session table and an at-risk
workspace panel in about 2.5 s. `amon` is a verb surface only so far; every verb it
advertises fails loudly rather than exiting zero having done nothing, and `amon --help`
names the ticket that will deliver each one.

**Ask GitHub which tickets are open and unblocked** — that is authoritative, and any list
written here goes stale. Two carried-forward notes that GitHub would not tell you: #13
records a #2 criterion met in effect rather than in letter, and the whole collection
currently takes ~2.5 s against a one-second fast-tier budget, which the tiering tickets own
rather than being a defect in what exists.

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
