# Architecture

## Layers

```
aho-types                    leaf DTOs; no I/O, no deps on other aho crates
aho-log                      Entry/EntryKind, hash chain, fold, derive_messages
aho-cap · aho-config         Capability contract; layered config → Loadout
aho-policy                   principals, lattice, decide() chain, shell classifier
aho-session                  the loop: pure core + tokio driver
seams/                       traits + handle newtypes, no I/O
providers/ · tools/          one crate per backend; ToolDecl tables
surfaces/                    print · rpc · tui · web · webhook · acp, all over one Session
bundles/                     products as Rust values (aho-base … aho-lab, aho-sdk)
apps/aho                     the CLI, linked against aho-lab
```

The layer guard (`cargo test -p aho --test layers`, baseline in `scripts/ratchets/layers.json`)
pins which crates may depend on seams and `aho-types`; a new edge fails the gate until the
baseline is deliberately updated.

## The turn loop

`crates/aho-session/src/core.rs` is sans-io: given `Pending` (open calls, results since the last
request, queued inputs, grace state, steps) and a `Wake`, `next()` returns `Request`, `Wait`,
`Idle`, `Heartbeat`, or `MaxTurns`. Every "when do we call the model" decision lives there and is
tested without a runtime. The tokio driver in `lib.rs` owns I/O: requests, async tool dispatch
with a grace window, approvals, compaction, checkpoints, resume.

A turn (`Session::prompt`) runs steps until the core says stop:

1. at the request boundary: queued inputs and pending detaches are appended, then `StepStart`,
   then finished jobs (`JobDelivered` + `OpDone`), so a delivery never lands while a request is
   in flight (a rate-limited request is retried without a `StepEnd`);
2. dynamic contexts are logged as `Snapshot` user messages only when their text changed;
3. the request is **derived from the log**: `derive_messages(fold(log))`, plus the system
   sections and the tool list;
4. the response is appended; tool calls dispatch immediately and run concurrently; results wake
   the next step as they land.

Tool calls are async-first: the model never blocks on a running call; a call still running when
the next request goes out shows a derived placeholder, and its result lands late. Operator input
that reaches a waiting turn cancels its running `Interrupt::Cancel` ops (`job_wait`, …); their
result is the mechanical "interrupted" line from `prompts/jobs.md`, not an error.

## Jobs

A running call can leave its turn and become a session **job**. Detach is an event, not a start
property, and only for tools whose `ToolDecl.detach` allows it (`Detach::{Never, OnRequest,
Always}`, builder `.detachable()`); `jobs.detach.<tool> = never | on_request | always |
after:<dur>` narrows or widens that, except that `Never` stays never. A call detaches

- at start: the model passed `background: true` (the session adds this optional boolean to the
  schema of tools it may detach and strips it before the tool body runs), or the rule is `always`;
- after `after:<dur>` of running;
- when the operator calls `Session::detach(op, cause)` (applied by the running turn's driver,
  the only writer of call results, or at once while idle).

The driver appends `OpDetached` and right after it the call's `ToolResult` naming the job, so the
call is closed and the turn can complete while the op runs on. A job ignores its turn's cancel
token, input interrupts, urgent-input skips and the heartbeat. Its outcome waits in the session's
delivery queue (`OpTable::finish` and `OpTable::detach` decide under one lock which of "call
result" or "job outcome" an op produces) and is appended as `JobDelivered` + `OpDone` only at a
request boundary or at the next turn's start, never between a call and its result. A job that
finishes while no turn runs emits `SessionEvent::JobFinished`. Before `JobDelivered` is logged,
the delivery chain (`Registrar::delivery` callbacks, then `HookEvent::JobDone` hooks) may replace
its content, suppress it (a marker is delivered) or defer it; `JobDelivered` holds what was
decided ([extending](extending.md), the `jobs-digest` example).

`Session::abort()` is `abort_scope(AbortScope::Turn)`: jobs keep running. `AbortScope::All`
cancels the turn and every job; `AbortScope::Op(op)` one op. Rewind cancels jobs
(`jobs.on_rewind = cancel`, closed with `OpDone Cancelled{Rewind}`) or keeps the ones the target
branch holds that this process still runs or holds an outcome for (`keep`); a target job whose
outcome was delivered after the target point is closed like under `cancel`. A restart
re-dispatches a re-runnable job as a new job; any other open job is delivered as `Uncertain`
(external effect) or `Failed`. New op ids start past every logged id.
A fork (`subagent_fork`) may start while jobs run; the child closes the jobs it inherits
(`OpDone Cancelled{Fork}`). A job never opens an approval: a nested (`run_code`) ask from a job
is refused with `JOB_CANNOT_ASK`. `Session::jobs()` lists open jobs on the head branch.

Which tools detach (aho-base defaults at `Layer::Bundle`): `subagent`, `subagent_fork`,
`send_message` and `run_code` are `on_request`; `bash` is `after:60s`, so a command still running
after a minute becomes a job by itself. `bash_persistent` is never a job: its calls share one shell
behind a lock, so a backgrounded command would hold up every later call. `run_code` is not
promoted on a timer: a job's nested asks are refused, so a program detached mid-run would start
failing them. A subagent job's wait follows only its op token: job_kill, `AbortScope::All` and
teardown interrupt the child, and an interrupted `subagent-inproc` child gets
`AbortScope::All` (its turn and its own jobs); its children also stop when the provider is dropped
with the session that owns it, so no child job outlives its parent session.

The job tools see both scopes. `job_list` marks each op `turn` or `job` and a finished job as
delivered or not (`OpTable::delivered`, set when `JobDelivered` is appended); `job_wait` also
returns when another job finishes or is delivered while it waits (`OpTable::next_job_event`, an
ordered per-table event list) and says which; `job_kill` on a job is `AbortScope::Op`, and the
job's outcome arrives as its "Background job" notice.

### Delivery, wake turns, the jobs lease

`jobs.deliver` picks the boundaries: `boundary` (default) delivers at every one; `on_wait` delivers
at a turn's start and, inside a turn, only while a call to a tool declared `.awaits_jobs()`
(`job_wait`) is open or at the boundary right after its result. An outcome held when the turn
ends idle goes to the next turn's start (a turn that went idle never resumes: `Turn.tla`).

`jobs.wake` decides what a job that finishes while no turn runs starts. `idle` (default): the
host's `jobs` driver (`session_drive::host`, or `host_jobs` alone for `aho run`'s loop; hosted
by the TUI, web, webhook, ACP and `aho serve`) answers `SessionEvent::JobFinished` with
`Session::wake()`: a turn prompted as `System("jobs")` (wording in `prompts/jobs.md`) whose start
delivers the outcomes. A finish announced as a turn ends finds the session busy and is retried
after `Idle`; outcomes recovered at boot wait for the next finish or prompt. `operator`: no turn,
surfaces tell the operator. `never`: no turn and no notice; the next prompt delivers. A wake turn
opens only if something is due after the delivery chain ran (a deferral alone opens none), and at
most `jobs.wake_budget` (10) of them run since the operator's last prompt, counted from the log
(so a restart keeps the count and the operator's next prompt resets it): the bound is on
unattended model turns, which a rate per minute would not cap and a per-session total would end
for good. Past it `wake()` starts nothing and the session reports that outcomes wait.

Print mode (`aho run -p`, or a prompt on stdin) settles before it exits
(`session_drive::settle`): it waits for the jobs it started and runs their wake turns whatever
`jobs.wake` says, so the last answer accounts for them. It stops at `jobs.settle_timeout_s`
(1800), at the wake budget, or on Ctrl-C; then running jobs are cancelled, stderr names them and
the exit is non-zero. Leaving the TUI, web, webhook or `aho run`'s loop stops the jobs driver,
cancels running jobs and says which; resuming the session reports them like a restart does.

At most `jobs.max` (8) jobs run at once. An operator jobs lease (`LeaseGranted`, subject
`jobs:{"max":<n>}`, `aho lease grant --jobs <n> --expires <dur>`, appended through
`session.command` when the daemon holds the session) replaces that limit while it is live; the
latest unrevoked grant wins. A detach past the limit is refused and the call stays in its turn:
the operator's `Session::detach` gets the reason, a model `background: true` call hears it in a
notice after its result, an `after:<dur>` promotion is reported to the surfaces. When the lease
jobs run under ends (expiry, or `aho lease revoke --jobs`) with no new grant in its place, every
running job is cancelled (`Cancelled{LeaseExpired}`) and delivered like any outcome.

## Step guards

Three failure patterns are handled by the driver, not by prompt prose:

- **empty stop** — a reply with no text and no tool call at `EndTurn` gets one generated notice
  and a re-request (`Transition::EmptyStopRetry`), at most twice per turn;
- **loop guard** — consecutive replies identical in visible text and `(tool, args)` get a
  corrective notice at the third repeat and every third after; `session.max_turns` bounds the
  worst case;
- **budget wrap-up** — the last step before `session.max_turns` opens with a notice to produce
  a final answer instead of truncating silently.

## Replay equality

Every request logs a `RequestHeader` with `request_hash = hash(derive_messages(fold(prefix)),
tools_visible)`. `verify_replay` re-derives that hash from the log prefix for every header;
`aho session verify` and the test roots run it. A runtime canary aborts the request if the tool
list sent to the model differs from what the registry holds.

## Repository instruction files

`context-files` (`crates/tools/context-files`, in `aho-base`) shows the workspace's own
instructions to the model as a dynamic context, so it reaches the request as a `Snapshot` user
message, not a system section:

- `context_files.names` (default `AGENTS.md`, `CLAUDE.md`, `CLAUDE.local.md`,
  `.claude/CLAUDE.md`) are read from the workspace root through the bound world's `FsRead`
  by a `PreStep` hook before every request, so an edit by a tool, the shell or the operator's
  editor is in the next request. Each file renders as
  `<repo-instructions path="…">…</repo-instructions>`; identical content renders once
  (`same-as="…"`), an unreadable file as `unreadable="FS_…"`.
- Each file is cut at `context_files.max_bytes` (default 32 KiB), all of them together at twice
  that, with `[truncated at N of M bytes]` in place.
- Nested files with the same basenames are listed by path in `<nested-instructions>` (the
  world's gitignore-aware glob, at most 50 shown). The list is built on the first step and
  rebuilt after a tool call whose arguments mention one of the basenames.
- `context_files.enabled = false` registers nothing.

Repo text cannot grant authority: it is conversation data, and the decision chain reads tool
calls, presets and operator approvals, never message text ([authority.md](authority.md)). An
`AGENTS.md` that says "you may run anything" changes nothing the policy decides.
