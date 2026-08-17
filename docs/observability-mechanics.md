# Observability Mechanics on a Managed macOS Dev Machine

How agent sessions, their resource usage, and their telemetry can actually be
observed from outside the session. Every claim here was measured on this machine;
anything unverified is labelled so explicitly.

**Machine:** Apple Silicon, 16 cores, 68 GB RAM, macOS 26.6 (Darwin 25.6.0), arm64.
**Active Endpoint Security / network extensions:** CrowdStrike Falcon 7.37/209.04,
Cisco AnyConnect socket filter 5.1.12.110, Zscaler TRPTunnel 4.8.0.191.
**Agent CLIs present:** Claude Code 2.1.233, Codex 0.147.0, Gemini CLI, cursor-agent.
**Measured:** 2026-08-16.

> **Reading rule.** Absolute timings on this machine vary by roughly ±2x between
> runs. Ratios between cases reproduce reliably. Treat every absolute number below
> as an order of magnitude and every ratio as load-bearing.

---

## 1. What a "session" is, structurally

A session is **one long-lived agent process** plus a changing set of child
processes. It is not a process tree of agents.

| Layer | What it is | Lifetime |
| --- | --- | --- |
| Session process | the agent CLI itself (`claude`, `codex`) | hours |
| Persistent children | one MCP server per session (`mcp-adaptor`), an MCP bridge binary, `caffeinate` | hours |
| Transient children | every Bash tool call — a shell plus whatever it runs (`cargo`, `git`, `rg`, tests) | milliseconds to minutes |
| Hook processes | one process per hook firing | milliseconds |

Observed live: 6 concurrent Claude sessions, 2–6 persistent descendants each,
~690 MB resident per session (~4.1 GB total).

### Subagents are NOT processes

Verified by launching a subagent and sampling the parent's descendant tree every
2 s for 80 s: **no new agent process ever appeared.** The only arrivals were brief
`zsh` / `sh` processes — the subagent's *own Bash calls*, parented to the same
session process and indistinguishable from the main thread's.

Consequences:

- A subagent's reasoning cost lands in the **session process's own CPU**.
- A subagent's tool calls land in the session's **child CPU**, mixed in with
  everything else.
- Per-subagent *OS resource* attribution is therefore **impossible**. Per-subagent
  *token and latency* attribution is possible, but only through telemetry (§3.3).

---

## 2. Resource accounting

### 2.1 The wrong way

`ps -o time=` reports a process's **own** CPU only. A parent shows `0.0s` after a
child burned real CPU and exited (verified). Any monitor built on `ps` alone
undercounts a session by up to ~20x — see §2.4.

### 2.2 The right way: `proc_pid_rusage()`

macOS exposes a per-process ledger that includes children. It is readable for
same-user processes **without root** (verified: 5/5 live sessions readable as uid 501).

```c
int proc_pid_rusage(int pid, int flavor, rusage_info_t *buffer);  // flavor 4 = RUSAGE_INFO_V4
```

Layout is `uint8_t ri_uuid[16]` followed by a run of `uint64_t` fields, in order:

```
ri_user_time              ri_system_time            ri_pkg_idle_wkups
ri_interrupt_wkups        ri_pageins                ri_wired_size
ri_resident_size          ri_phys_footprint         ri_proc_start_abstime
ri_proc_exit_abstime      ri_child_user_time        ri_child_system_time
ri_child_pkg_idle_wkups   ri_child_interrupt_wkups  ri_child_pageins
ri_child_elapsed_abstime  ri_diskio_bytesread       ri_diskio_byteswritten
ri_cpu_time_qos_default   ... (7 qos fields) ...     ri_billed_system_time
ri_serviced_system_time   ri_logical_writes         ri_lifetime_max_phys_footprint
ri_instructions           ri_cycles                 ri_billed_energy
ri_serviced_energy        ri_interval_max_phys_footprint  ri_runnable_time
```

Available per session, root-free: own CPU, **children's CPU**, current footprint,
lifetime peak footprint, disk bytes read/written, instructions, cycles.

### 2.3 UNITS — the trap

All `*_time` fields are in **mach absolute time ticks, not nanoseconds.** Convert
with `mach_timebase_info()`.

On this machine: `numer=125, denom=3` → **1 tick = 41.6667 ns**.

Reading ticks as nanoseconds makes every duration look 41.67x too small. This
error is easy to miss because the results stay internally consistent. The tell is
that *every* value is off by the *same* factor — a uniform discrepancy is a units
bug, not a finding. Cross-check against `ps -o time=` before trusting the numbers.

### 2.4 What rolls up and what is lost

| Case | Attributed to parent? | Evidence |
| --- | --- | --- |
| Child runs and is reaped | **Yes** | 0.555 s burner correctly attributed |
| Grandchild (parent → sh → burner) | **Yes, recursively** | 0.629 s attributed |
| Orphaned / double-forked (`(cmd &)`) | **No — LOST** | 0.004 s of a 0.6 s burner |
| Session's own totals after it exits | **Lost** | ledger dies with the process |

Two operational rules follow:

1. **Detached background work escapes accounting.** Frequency in real agent use is
   unmeasured.
2. **Sample before death.** A session's lifetime totals are only knowable if they
   were read while it lived. Persist the last reading.

### 2.5 These are odometers, not speedometers

Every counter is monotonic and cumulative from process start. Therefore **sampling
cadence does not affect accuracy** — read now, read later, subtract, and the work
in between is fully accounted for. There is no accuracy/cost tradeoff for these
metrics. Cheap and exact.

This does *not* apply to instantaneous values (`%cpu`, current footprint), which
are genuine point samples.

### 2.6 Measured example

Five live Claude sessions, correct units:

| pid | own CPU | children's CPU | total | footprint | peak | disk written | instructions |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 69046 | 1,669 s | **32,317 s** | 33,986 s | 482 MB | 622 MB | 166 MB | 4,135 G |
| 5333 | 2,239 s | 12,007 s | 14,246 s | 498 MB | 563 MB | 97 MB | 5,456 G |
| 74638 | 857 s | 1,046 s | 1,904 s | 445 MB | 510 MB | 45 MB | 2,054 G |
| 2880 | 222 s | 557 s | 779 s | 382 MB | 392 MB | 23 MB | 604 G |
| 264 | 637 s | 101 s | 738 s | 419 MB | 587 MB | 34 MB | 1,434 G |

Combined: **14.3 CPU-hours.**

Session 69046's children used **19.4x** more CPU than the session process itself —
builds, tests, git and hooks, not agent reasoning. Note also that sessions have
distinct *shapes*: 69046 delegates almost everything; 264 does most of its own
work. Same tool, opposite fingerprints.

### 2.7 The uid boundary — important correction

`proc_pid_rusage()` is root-free **only for processes owned by the calling user.**
System daemons run as root and are **not** readable: of 28 security-stack processes
found, **24 returned an error** (`endpointsecurityd`, `amfid`, `mds`, `xprotectd`,
`sandboxd`, `trustd`, `syspolicyd`, `ZscalerTunnel`, `acsockext`, …).

For those, `ps -o time=` **does** report cumulative CPU without sudo. So:

| Target | Use | Gives |
| --- | --- | --- |
| Your own agent sessions and children | `proc_pid_rusage` | full ledger incl. child CPU, disk I/O, instructions |
| Root-owned system/security daemons | `ps -o time=,rss=` | cumulative CPU and RSS only |

### 2.8 Root-gated extras (not needed)

`/usr/bin/taskinfo` (resource coalitions) and `powermetrics --show-process-coalition`
both refuse to run as non-root. `footprint(1)` also requires root. Falcon is a
protected ES extension: `ps` reports RSS=0 and `top` omits it entirely, so its own
memory is not scriptable at all — Activity Monitor is the only reader.

None of this is required; §2.2 plus §2.7 cover the need root-free.

---

## 3. Telemetry (OpenTelemetry)

### 3.1 Enabling it

Claude Code ships OTel support. Exporters are `console` and `otlp` **only** — there
is no Prometheus exporter, so scraping is not an option. Protocols: `grpc`,
`http/protobuf`, `http/json`. Default endpoints `localhost:4317` (grpc) and
`localhost:4318` (http), paths `/v1/metrics`, `/v1/logs`, `/v1/traces`.

**`http/json` works**, so a receiver needs no protobuf toolchain — just an HTTP
server and a JSON parser. Verified end to end against a 40-line Python listener.

```sh
CLAUDE_CODE_ENABLE_TELEMETRY=1
OTEL_METRICS_EXPORTER=otlp
OTEL_LOGS_EXPORTER=otlp
OTEL_EXPORTER_OTLP_PROTOCOL=http/json
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
OTEL_METRIC_EXPORT_INTERVAL=2000      # default is 60s
OTEL_LOGS_EXPORT_INTERVAL=2000
OTEL_METRICS_INCLUDE_SESSION_ID=true
```

Telemetry is **push**-based, so something must be listening. Nothing buffers to
disk for later collection.

### 3.2 What actually arrives

Only 4 things are **metrics**:

```
claude_code.active_time.total   claude_code.cost.usage
claude_code.session.count       claude_code.token.usage
```

Everything useful is a **log event** on `/v1/logs`. Observed catalog:

| Event | Key attributes |
| --- | --- |
| `tool_result` | `duration_ms`, `tool_name`, `success`, `session.id`, `tool_use_id`, `tool_input_size_bytes`, `tool_result_size_bytes`, `prompt.id` |
| `tool_decision` | `decision`, `tool_name`, `tool_source`, `tool_use_id`, `session.id` |
| `api_request` | `duration_ms`, `cost_usd`, `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`, `model`, `effort`, `speed`, `query_source`, `agent.name` |
| `subagent_completed` | `agent_type`, `agent.source`, `duration_ms`, `total_tokens`, `total_tool_uses`, `model`, `final_model`, `model_swapped`, `is_async` |
| `hook_execution_start` / `_complete` | `hook_name`, `hook_event`, `hook_source`, `num_hooks`, `num_blocking`, `num_cancelled` |
| `mcp_server_connection` | `duration_ms`, `transport_type`, `server_scope`, `status` |
| `user_prompt` | `prompt` (redacted), `prompt_length`, `message.uuid` |
| `assistant_response` | `response` (redacted), `response_length`, `model` |
| `hook_registered`, `plugin_loaded` | hook/plugin inventory, `plugin_id_hash` |

**Not emitted**, despite existing as strings in the binary:
`claude_code.bash.subprocess`, `claude_code.tool.execution`,
`claude_code.tool.blocked_on_user`. Never observed in any run.

This last point matters: there is **no usable direct signal for "this session is
blocked waiting on a human."**

Tested four ways, including a genuine interactive session driven through a pty (which
did start a real turn — `user_prompt`, `api_request` and `tool_result` all arrived).
`claude_code.tool.blocked_on_user` was **never emitted in any run.**

Worse for the idea, the nearest alternative does not help either. `tool_decision`
arrives *alongside* `tool_result` carrying an already-resolved value
(`decision='accept'`), so it reports what was decided, not that something is
currently pending. Even forcing a permission prompt would not give a "waiting now"
signal from it.

**Design consequence:** `WAITING` must be inferred — stale transcript, plus a
resident session process, plus no live build — for both CLIs. Record which method
produced each verdict so an inferred state is never mistaken for an asserted one. If
a direct signal appears in a future version, it slots in without redesign.

(Two earlier attempts at this test were void: the first never submitted the prompt,
the second was killed by a harness timeout. Neither was evidence of absence. Only the
run that demonstrably started a turn counts.)

Incidental observation: one `mcp_server_connection` reported `duration_ms=73341`
— 73 seconds to connect. Worth investigating separately.

### 3.3 Per-subagent accounting

| Want | Available? | How |
| --- | --- | --- |
| Subagent wall duration | **Yes** | `subagent_completed.duration_ms` |
| Subagent total tokens | **Yes** | `subagent_completed.total_tokens` |
| Subagent tool-call count | **Yes** | `subagent_completed.total_tool_uses` |
| Subagent LLM cost / tokens / latency | **Yes** | `api_request` rows where `agent.name` is set |
| Which subagent ran a given tool call | **No** | `tool_result` has no agent field |
| Subagent CPU / memory / disk | **No** | subagents are not processes (§1) |

Main-thread vs subagent LLM calls are distinguished by `query_source`:
`sdk` for the main thread, `agent:builtin:<type>` for a subagent. Measured example —
one subagent: `duration_ms=7722`, `total_tokens=20976`, `total_tool_uses=1`, and
three `api_request` rows, two carrying `agent.name=general-purpose`.

### 3.4 Turns: `prompt.id` is the natural unit of work

`prompt.id` appears on `tool_result`, `api_request`, **and** `subagent_completed` —
the same value across all three. So everything belonging to one user request can be
summed: every tool call, every LLM call, every subagent, with tokens, cost and
duration.

This is a far more useful unit than "session." A session runs for hours and does
many unrelated things; a **Turn** (one `prompt.id`) has a beginning, an end, and an
intent.

### 3.5 Commits: outcome attribution

Verified in a throwaway repo — a session that commits emits:

```
claude_code.commit.count        = 1   attrs: session.id, terminal.type, user.id
claude_code.lines_of_code.count = 2   attrs: session.id, type, model, …
claude_code.code_edit_tool.decision   (also emitted)
claude_code.pull_request.count        (not emitted unless a PR is opened)
```

**Attribution is session-level, not turn-level.** These are metrics and carry
`session.id` but **not** `prompt.id`. Linking a commit to the turn that produced it
requires correlating commit timestamps against turn windows — approximate, not free.

Git also records a durable marker independent of telemetry:

```
Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
```

This matters more than it first appears:

- It is **retrospective** — `git log` yields agent-attributable history for work done
  before any monitor existed, giving real numbers on day one on a new machine.
- It is **CLI-agnostic** — it closes the Codex outcome gap, which telemetry cannot.

**Do not treat "commit" as equivalent to "work unit complete."** The mapping is
lossy in both directions: one turn can produce several commits, one commit can span
twenty turns, and many legitimate turns produce none at all (reviews, questions,
debugging that correctly concludes "no change needed"). Using commits as the sole
completion signal scores a correct "nothing to change" identically to an hour of
thrashing.

Model outcomes as a three-way classification per Turn instead:

| Outcome | Meaning |
| --- | --- |
| **Committed** | one or more commits landed in the window |
| **Concluded** | ended normally, nothing committed |
| **Abandoned** | ended in error, or interrupted |

Only *Abandoned* is unambiguous waste. Commits can also be reverted, squashed or
rebased away, so "did it survive" is a separate later question answered by
reachability.

**Avoid `lines_of_code` as a denominator.** A refactor deleting 500 lines is good
work; cost-per-line would score it as a disaster. Count commits, show diff size as
context, let a human judge. Denominators invite gaming — including by agents.

### 3.6 Privacy

**Content is redacted by default.** Verified with a canary string planted in a
prompt: it never reached the wire. The `prompt` and `response` attribute *keys*
are present, but their values are the literal string `<REDACTED>` — both with
`OTEL_LOG_USER_PROMPTS=0` and with the variable unset entirely.

Content is therefore strictly **opt-in** via `OTEL_LOG_USER_PROMPTS`,
`OTEL_LOG_TOOL_CONTENT`, `OTEL_LOG_TOOL_DETAILS`, `OTEL_LOG_ASSISTANT_RESPONSES`.
Never set them. Assert redaction at ingest as defence in depth.

### 3.7 A separate content channel: argv

**Process argv contains prompt text.** Discovered accidentally — another agent's
prompt spilled into a `ps` listing and broke a line-based parse, because the text
contains newlines.

Two rules: never parse `ps` text output (use `libproc`/`proc_pidinfo` structurally),
and never persist or display argv. It deserves the same care as transcripts.

---

## 4. Session discovery and attribution

### 4.1 Process enumeration

An `lsof -d cwd -Fpcn` snapshot yields `pid → command → cwd` for every process in
one pass (514 of 837 processes had a readable cwd). Include a **self-sentinel**:
an all-process snapshot must contain the querying shell's own pid. If it does not,
the snapshot failed silently and its emptiness means nothing.

**`lsof` cannot identify Claude Code.** The process sets its title to its version
string, so `lsof` reports the command as `2.1.233`. Identity must come from a second
source (argv or exe path), joined by pid — which reintroduces a race.

**Atomicity matters.** Enumerating processes and then resolving cwd in a later pass
produced 6 "unreadable cwd" entries that were simply *dead processes*. Get identity
and cwd in the same pass. `libproc` returns exe path *and* cwd from one enumeration
with no subprocess at all, which avoids both the race and the spawn cost.

### 4.2 Identifying agent processes

Mechanisms differ per CLI, so detection rules must be **data, not code**:

| CLI | Real executable | Usable signal |
| --- | --- | --- |
| claude | `~/.local/share/claude/versions/<ver>` (via `~/.local/bin/claude`) | exe path — note the basename is a *version string* |
| codex | `/opt/homebrew/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex` | exe path ending `bin/codex` |
| gemini | `/opt/homebrew/lib/node_modules/@google/gemini-cli/bundle/gemini.js` | **argv only** — the exe is `node` |
| cursor-agent | `~/.local/share/cursor-agent/versions/<ver>/cursor-agent` | exe path |

Required exclusions — all matched agent-ish patterns but are **not** sessions:

```
/Applications/ChatGPT.app/.../Codex Framework.framework/...   (desktop app)
/Applications/Claude.app/.../Claude Helper                     (desktop app)
/Applications/Cursor.app/.../Cursor Helper (*)                 (editor helpers)
~/.codex/computer-use/.../SkyComputerUseClient                 (Computer Use)
```

Also: a `comm` value is **not always a path**. Cursor reports descriptive strings
such as `Cursor Helper: terminal pty-host`. Detectors must tolerate that.

On this machine both CLIs are launched through `cmux` shims in a temp directory, so
the invoked path is not the real binary. Resolve before matching.

### 4.3 Claude Code transcript paths

```
~/.claude/projects/<cwd-slug>/<session-id>.jsonl
```

The slug is the absolute cwd with characters replaced by `-`. **The replaced set is
`/`, `.`, and `_`.**

```
/Users/pmcfadin/projects/agentic_coding_monitor
  -> -Users-pmcfadin-projects-agentic-coding-monitor      EXISTS
  -> -Users-pmcfadin-projects-agentic_coding_monitor      absent
```

Corroboration: **0 of 135 namespaces on this machine contain an underscore.**

Two further hazards:

- **Case.** A namespace exists as `-Users-pmcfadin-projects-WorkforceOS` while the
  live process cwd is `/Users/pmcfadin/projects/workforceos`. APFS is
  case-insensitive but case-*preserving*, so `[ -d ]` on a constructed lowercase
  path succeeds while a string comparison against the listing fails. Compare
  case-insensitively. On a case-sensitive volume, path access would fail too.
- **Lossiness.** Because `/`, `.` and `_` all map to `-`, the slug is **not
  invertible**. Always map path → slug forward; never attempt slug → path.

### 4.4 Codex session storage

```
~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<id>.jsonl
~/.codex/session_index.jsonl        # 689 rows: {id, thread_name, updated_at}
~/.codex/archived_sessions/
```

The path encodes the date and id but **not the cwd**. `session_index.jsonl` gives
cheap liveness (`updated_at`) but carries no cwd either.

Attribution requires reading the transcript — but only **line 1**, which is always
a `session_meta` record:

```
top level : payload, timestamp, type
payload   : cwd, cli_version, context_window, git, id, model_provider,
            originator, session_id, source, thread_source, timestamp
```

`payload.cwd` is the workspace. Cost measured: index parse+sort 28 ms, locate by id
54 ms, bounded read 32 ms — **~20 ms per session**, and 3/3 resolved in 60 ms total
via `rglob` over the 7.0 GB tree (directory traversal only, no content scanning).

The index `id` appears verbatim in the filename, so lookup by id works. The file
lives in its *creation* date directory, which may differ from `updated_at`, so the
directory cannot be derived from `updated_at` alone.

Line 1 is metadata only — **no conversation content is read.**

### 4.5 Codex telemetry and hooks

No OpenTelemetry. Telemetry is Sentry-based and not externally queryable.
`~/.codex/hooks.json` defines 10 events: `PermissionRequest`, `PreToolUse`,
`PostToolUse`, `PreCompact`, `PostCompact`, `SessionStart`, `Stop`, `SubagentStart`,
`SubagentStop`, `UserPromptSubmit`. Notification env convention:
`CODEX_NOTIFY_WEBHOOK`, `CODEX_NOTIFY_NTFY_TOPIC`, `CODEX_NOTIFY_THROTTLE`,
`CODEX_NOTIFY_WEBHOOK_CATEGORIES` (`approval`, `question`, `error`, `auth`).

**Net asymmetry:** Claude Code gives per-tool-call latency and per-subagent token
accounting for free. Codex gives neither without either installing hooks or parsing
tool events out of transcripts.

---

## 5. Cost of observing

Measured under load; treat as upper bounds.

| Operation | Cost | Tier |
| --- | --- | --- |
| Read `proc_pid_rusage` for one pid | microseconds, no subprocess | fast |
| `lsof -d cwd` full snapshot | one subprocess | medium |
| Codex `session_meta` per session | ~20 ms | slow |
| `git status --porcelain --no-optional-locks` per workspace | median 59 ms, max 455 ms | slow |
| Full sweep of 34 workspaces | **2.7 s** | slow |

Use `--no-optional-locks`: plain `git status` may write the index and contend with
the very agent being observed.

**The observer competes with the observed.** Exec authorization is serialized
machine-wide (§6), so a subprocess-chatty monitor adds latency to the agents it
watches. Two mitigations: prefer `libproc` over shelling out, and record collection
cost as a first-class reported metric.

**Refuse to measure a busy machine.** During this work load average ranged 6–26.
Timing measurements taken at load 26 are meaningless. Record load with every sample
and mark low-confidence rather than publishing noise.

---

## 6. Exec authorization tax (reproduction)

Every `execve` must be authorized by each ES extension, which SHA-256s the target
binary synchronously on a serial delivery thread. Verdicts are cached per **inode**.

Re-measured, median of 11 trials, at load ~6:

| Case | This run | Prior measurement | x warm |
| --- | --- | --- | --- |
| warm exec (same inode) | 4.1 ms | 5.2 ms | 1.0x |
| **cold exec (fresh inode)** | **79.8 ms** | 79.3 ms | 19.4x |
| hardlink to authorized binary | 5.3 ms | 4.7 ms | 1.3x |
| APFS clone (`cp -c`) | 79.5 ms | 84.1 ms | 19.3x |
| fresh shell script | 162.3 ms | 237.2 ms | 39.4x |
| piped to `sh -s` | 8.4 ms | 9 ms | 2.0x |

Key ratios: `cold/warm = 19.4x`, `script/piped = 19.4x`, `clone/hardlink = 15.1x`.

Identical content at a new inode pays full price. A hardlink does not. An APFS
clone does. Authorization is machine-wide serial: 12 concurrent fresh execs on
16 cores yielded only a 1.25x speedup.

**Practical consequence for tooling:** never write a one-off script to disk and
execute it; pipe it to the interpreter's stdin instead.

### Size scaling — a correction

The relationship is often quoted as "79 ms fixed + 8.6 ms/MB". The 79 ms is the
**measured floor at 0.08 MB**, not a regression intercept. An ordinary
least-squares fit of the same data gives **280 ms + 8.3 ms/MB**. The slopes agree;
the intercepts differ by 3.5x because the curve is not cleanly linear.

Neither model fits everywhere: the quoted model predicts the 315.7 MB point within
10 ms but misses 89.3 MB by 676 ms. Note that the 89.3 MB sample's *warm* exec was
also anomalous (326 ms vs 20–54 ms elsewhere), suggesting it was taken under
different machine conditions.

Report fit coefficients **with R² and residuals**, and keep "measured floor" and
"regression intercept" as distinct fields.

**Worked example:** Claude Code's binary is 292.8 MB. Both models put its first
exec after each auto-update at **~2.6 s**, re-paid on every version change (new
inode).

### Per-extension attribution: not by tracing, but yes by correlation

**Tracing is impossible.** `eslogger` advertises 104 event types; the only
auth-shaped ones are `authentication`, `authorization_judgement`,
`authorization_petition` — all Authorization Services / securityd (admin-rights
prompts). There is **no `auth_exec`.** Apple's own ES logging tool subscribes to
NOTIFY events only and structurally cannot observe the authorization phase.
`dtrace` could attribute but requires SIP disabled, which destroys the real-world
relevance of the measurement.

**Correlation works, and needs neither root nor SIP off.** Snapshot every security
daemon's cumulative CPU (§2.7), fire a controlled burst of cold execs, snapshot
again, and read the deltas.

Measured: 20 cold execs of a fresh 6.19 MB binary (124 MB total presented for
authorization), load ~5, all 20 exits verified 0.

| Process | Role | CPU | per exec | Share |
| --- | --- | --- | --- | --- |
| **XprotectService** | XProtect | 2.64 s | 132 ms | **42%** |
| **JamfDaemon** | Jamf | 2.30 s | 115 ms | **36%** |
| syspolicyd | Gatekeeper | 0.51 s | 26 ms | 8% |
| ZscalerTunnel | Zscaler | 0.51 s | 26 ms | 8% |
| logd | logging | 0.14 s | 7 ms | 2% |
| socketfilterfw | App Firewall | 0.08 s | 4 ms | 1% |
| trustd | cert validation | 0.06 s | 3 ms | 1% |
| acsockext | Cisco AnyConnect | 0.04 s | 2 ms | 1% |
| mds_stores | Spotlight | 0.03 s | 2 ms | 0% |

Cold median 193.6 ms vs warm 5.1 ms — a **38x** penalty on this binary.

**XProtect + Jamf are 78% of the visible cost.** Cisco is negligible. Zscaler is a
surprise at 26 ms/exec — it is a network filter with no obvious interest in a
process launch, yet ties with Gatekeeper.

Three caveats, all load-bearing:

1. **Falcon does not appear, and that is not reassurance.** As a protected ES
   extension its CPU is omitted from `ps` entirely. Its contribution is
   **unmeasured, not zero.** Every share above is a share of *the visible portion*.
2. **This is correlation, not causation.** CPU rising in a process during a burst is
   strong evidence, not proof. Only SIP-off tracing would prove it.
3. **Total security CPU (6.35 s) exceeded wall-clock overhead (4.72 s) — 134%.**
   Not an error: several daemons work *concurrently*, so CPU-seconds exceed elapsed
   seconds. The authorization *decision* still serializes, which is why 12 parallel
   cold execs yield only 1.25x.

### Cost per MB — the slope holds, the fixed cost doubled

An earlier version of this section claimed **38.1 ms/MB**, "4.4x worse than
documented". **That was an arithmetic error**: it divided total burst overhead
(4.72 s) by total bytes presented (124 MB), which attributes per-exec *fixed* cost to
file *size*. Most of that overhead was fixed cost paid 20 times, not size sensitivity.

Two clean measurements, each a median of repeated trials at low load:

| Binary | Size | Cold exec | Warm exec |
| --- | --- | --- | --- |
| stripped Rust release binary | 0.49 MB | 136.4 ms | 4.8 ms |
| copy of `rg` | 6.19 MB | 193.6 ms | 5.1 ms |

Fitting those two points: **≈131 ms fixed + ≈10.0 ms/MB.**

Compared with the 79 ms + 8.6 ms/MB in `process_changes_for_perf.md`, the **slope
reproduces well** (10.0 vs 8.6) while the **fixed cost has roughly doubled**
(131 vs 79). Note also that a 12.6x increase in size bought only a 1.4x increase in
cost, which is what a dominant fixed term looks like.

**This changes the practical conclusion.** The tax is dominated by *how many times you
exec*, not by *how large the binary is*. That strengthens the case against
write-then-run scripts (each is a fresh inode paying full fixed cost regardless of
size) and further weakens the argument for a process-per-test runner, which multiplies
exec count while leaving binary count unchanged.

Two points is still a two-point fit. Report coefficients with residuals, and keep
"measured floor" and "regression intercept" distinct (see above).

### Methodology trap: verify exit codes

Two attempts at this measurement produced confident, entirely void numbers because
the test binaries never ran:

- Appending padding to a signed binary invalidates its signature.
- Copying a **SIP-protected platform binary** (`/bin/bash`, `/bin/echo`) and running
  the copy is killed by code-signing enforcement — exit code **137** (SIGKILL).

Both produced ~1 ms "cold" execs, which looks like a fast machine rather than a
failed test. Use a freely copyable binary (Homebrew builds work; `/usr/bin/true`
works) and **assert every exit code before believing any timing.**

### Extension inventory

Free and root-free:

```sh
systemextensionsctl list    # team IDs, bundle IDs, versions, activation state
```

Note that cumulative-since-boot CPU figures depend entirely on uptime — the numbers
here were taken at 3 hours' uptime and are not comparable to 10-day figures.

---

## 7. Bugs found in `agent-watchdog.sh`

| # | Bug | Effect |
| --- | --- | --- |
| 1 | `slugify()` maps only `/` and `.`, not `_` | Every underscore path (`agent_ami`, `agent_data_gateway`, `bcs_agent_sdlc`, `agentic_coding_monitor`) is invisible and reports as `NO-SESSION` |
| 2 | Slug comparison is case-sensitive | `WorkforceOS` namespace never matches `workforceos` cwd |
| 3 | `pgrep -f 'Resources/codex'` matches `/Applications/ChatGPT.app/.../Resources/codex` | False positive on the desktop app, and **misses the real CLI entirely** (its path ends `bin/codex`) |
| 4 | `pgrep -f '/\.codex/'` matches the Computer Use helper | False positive |

Bugs 3 and 4 are the serious pair. In `classify()`, a resident session downgrades a
stale lane from `STALLED` to `WAITING`. Because the ChatGPT desktop app is
effectively always running, the watchdog reports "waiting" instead of "dead" — it
goes quiet at exactly the moment a session dies, which is its entire purpose.

All four fail *open*: they produce calm-looking answers rather than errors.

### A design decision the script gets right

Getting cwd from the same `lsof` call as the process list, rather than enumerating
then enriching. Deviating from this during testing immediately produced six phantom
"unreadable cwd" entries that were merely dead processes.

---

## 8. A correction to a separate document

`process_changes_for_perf.md` §P1 advises preferring `cargo nextest` on the grounds
that it "re-execs the same binary and pays the toll once."

Under the document's own cost model this is backwards. The number of *distinct*
test binaries is set by the crate graph and is identical under both runners. What
differs is exec *count*:

- `cargo test` — one exec per binary, tests run as in-process threads
- `cargo nextest` — process-per-test, so 1 cold + (N−1) warm execs per binary

At ~4–5 ms per warm exec, a 1,000-test suite adds ~5 s of pure authorization
overhead that `cargo test` never pays. Nextest does not reduce the toll; it adds
warm execs on top of it. It may still be worth using for isolation and scheduling,
but the exec-tax justification does not hold.

Also confirmed as a dead end for this repo: `split-debuginfo`. `otool -l` on the
21 MB `wfos` binary shows segments `__TEXT __DATA __DATA_CONST __LINKEDIT
__PAGEZERO` and **no `__DWARF`** — debug info is already split.

---

## 9. Open questions

| Question | Why it matters | Status |
| --- | --- | --- |
| Does an interactive session emit `tool.blocked_on_user`? | It is the only direct "waiting on a human" signal; without it, WAITING stays inferential for both CLIs | **Unverified** — non-interactive `-p` cannot block |
| What does Falcon actually cost? | It is the prime suspect and the one process whose CPU cannot be read at all (§6) | **Unmeasurable** without Activity Monitor or IT cooperation |
| How often do agents orphan children? | Orphaned work escapes accounting entirely (§2.4) | Unmeasured |
| Do `hook_execution_*` events carry a duration? | Would price the 12 registered hook events directly | Attributes seen, duration not confirmed |
| Does commit→turn timestamp correlation work in practice? | Turn-level outcome attribution depends on it (§3.5) | Untested |
| Did a commit survive (revert / squash / rebase)? | "Committed" is output, not success | Not designed |
| ~~Why is cost/MB 38.1 rather than 8.6?~~ | Was a discrepancy against the prior model | **RESOLVED: arithmetic error on my part.** Slope is ~10.0 ms/MB (close to 8.6); the *fixed* cost doubled to ~131 ms |
| ~~Rust release binary size and its own cold-exec cost~~ | The tool pays the tax it measures | **RESOLVED:** stripped release binary 0.49 MB, cold 136.4 ms vs warm 4.8 ms (28.6x). Paid once per released version |
| Why does `mcp_server_connection` take 73 s? | Observed once; a 73 s startup stall is worth explaining | Unexplained |

---

## 10. Reproducing any of this

Every measurement above came from short scripts piped to an interpreter's stdin —
never written to disk first, per §6. For example:

```sh
# exec tax, in outline
cd "$(mktemp -d)"
cp /usr/bin/true a && chmod +x a && sync && sleep 1
time ./a          # cold, new inode   -> ~80 ms
time ./a          # warm, same inode  -> ~4 ms
ln a hard;  time ./hard   # same inode -> ~5 ms
cp -c a cl; time ./cl     # APFS clone -> ~80 ms
```

```sh
# extension inventory
systemextensionsctl list

# confirm eslogger has no auth_exec
eslogger --list-events | grep -i auth
```

For `proc_pid_rusage`, remember the mach timebase conversion (§2.3) and validate
against `ps -o time=` before trusting any duration.
