# acmon

Measuring what AI coding agents actually cost on a managed macOS developer machine —
and how much of that cost is the machine's fault rather than the work's.

It installs **two binaries**, and one rule tells you which you want:

**If it measures, it is `amon`. If it draws, it is `agtop`.**

| | |
| --- | --- |
| **`amon`** | The monitor. Resident. Owns every collection tier, is the only writer of state, and is the only thing that notifies. |
| **`agtop`** | The display. Read-only. Renders what `amon` recorded — and collects once for itself, saying so, when no monitor is running. |

Two names, one crate, one Homebrew formula. The split exists because notifications must
fire when no terminal is open, and a display wanting a one-second refresh cannot also be
running a git sweep that costs 2.7 s.

## Why

Three things turned out to be true on a corporate-managed Mac, all measured:

**1. Executing a newly built binary is ~38x slower than executing a known one.**
Endpoint Security extensions SHA-256 every new file before the kernel will run it,
and the verdict is cached per *inode* — so every rebuild re-pays. A 6 MB binary
costs ~194 ms cold versus ~5 ms warm. Authorization is serialized machine-wide, so
running more work in parallel does not hide it: 12 concurrent cold execs on 16 cores
yield 1.25x.

**2. Almost all of an agent's resource cost is in its child processes.** One session
spent 1,669 s of CPU in the agent process and **32,317 s in its children** — a 19.4x
undercount for any tool watching only the agent. Those children are shell commands,
builds, tests, and hooks, not agent reasoning.

**3. Sessions die silently, and take unsaved work with them.** An agent cannot report
its own death, so detection has to live outside the sessions being watched. At the
time of writing, 12 of 34 git workspaces on this machine held uncommitted changes
while only 2 had a live session.

## Documents

| Document | What it is |
| --- | --- |
| [`docs/observability-mechanics.md`](docs/observability-mechanics.md) | Reference: how agent sessions, their resource usage, and their telemetry can be observed externally on macOS. Every claim measured; unverified items labelled. |
| [`docs/PRD.md`](docs/PRD.md) | Product requirements for a monitor built on those findings. |

## Selected findings

Details, evidence and caveats are in the mechanics document.

- **Child CPU is recoverable.** `proc_pid_rusage()`'s `ri_child_user_time` /
  `ri_child_system_time` attribute exited children — recursively through
  grandchildren — without root. Orphaned/detached processes are the exception and
  are lost.
- **Those counters are cumulative**, so sampling cadence does not affect their
  accuracy. Read now, read later, subtract. There is no fidelity-versus-cost
  tradeoff, which is unusual for a monitor.
- **Time fields are mach ticks, not nanoseconds** (41.67 ns each on Apple Silicon).
  Reading them as nanoseconds understates everything by 41.67x while looking
  internally consistent.
- **Subagents are not OS processes.** Per-subagent resource attribution is
  impossible; per-subagent *token and latency* attribution is available through
  telemetry.
- **`prompt.id` is a per-turn unit of work**, appearing on tool, LLM, and subagent
  events alike — a far more useful unit than "session".
- **Per-extension attribution works by correlation, not tracing.** `eslogger` has no
  `auth_exec` event and `dtrace` needs SIP disabled, but snapshotting each security
  daemon's CPU around a controlled exec burst ranks them: XProtect 42%, Jamf 36%,
  Gatekeeper 8%, Zscaler 8% of the *visible* cost. CrowdStrike Falcon's CPU cannot be
  read at all, so it is unmeasured rather than absent.
- **Verify exit codes before believing any timing.** Two measurements here were void
  because copies of SIP-protected binaries were SIGKILLed while reporting plausible
  ~1 ms "cold" execs.

## Method

Measurements were taken on one machine (Apple Silicon, 16 cores, 68 GB, macOS 26.6)
running CrowdStrike Falcon, Cisco AnyConnect's socket filter, and Zscaler, alongside
Apple's XProtect, Gatekeeper, and application firewall, under Jamf management.

Absolute timings on such a machine vary by roughly ±2x between runs; **ratios between
cases reproduce reliably.** Treat every absolute number as an order of magnitude and
every ratio as load-bearing. Machine load is recorded with each measurement, because
a sample taken at load 26 means nothing.

Where a claim could not be verified, it says so.

## Status

v1 in progress. `agtop` runs and is worth running: it prints a session table and an
at-risk workspace panel. `amon` exists as a verb surface only — every verb it advertises
reports that it is not built yet rather than exiting zero having done nothing.

The one assumption still unverified is whether an interactive session emits a direct
"blocked waiting on a human" signal.

## License

[Apache License 2.0](LICENSE). Copyright 2026 Patrick McFadin.
