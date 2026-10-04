# Surfaces

Every surface (print, rpc, tui, web, webhook, acp) drives one `Session`; the CLI and the daemon
are clients of the same calls. This page covers what the operator sees of aborts and session
jobs ([architecture § Jobs](architecture.md#jobs)) on each of them.

## Aborts and jobs

`Session::abort_scope` takes a scope:

| scope | cancels | jobs |
|---|---|---|
| `turn` (what `abort()` and every "stop" key without a scope do) | the running turn and its ops | keep running |
| `all` | the turn, every running op, every job | cancelled |
| `op` | one operation | — |

`Session::jobs()` lists open jobs on the head branch: running (`status` null) or finished and
waiting for delivery at the next request boundary. `Session::kill_job(op)` cancels one running
job and refuses a turn op; `Session::detach(op, Operator)` moves a running turn call into the
background. A job that finishes while no turn runs emits `SessionEvent::JobFinished`.

## rpc (`aho rpc`, the SDKs)

| method | params | result |
|---|---|---|
| `session.abort` | `{scope?: "turn" \| "all" \| "op", op?}`; none = `turn` | `{}`; an `op` that does not run is an error |
| `job.list` | — | `{jobs: [{op, call, tool, cause, status, started}]}` |
| `job.kill` | `{op}` | `{}`; refused for a turn op or a finished job |
| `job.detach` | `{op}` | `{}`; refused when the tool cannot run as a job |

Event `job_finished{op}`: a job finished while the session was idle. The Rust client has
`abort_scope`, `list_jobs`, `kill_job`, `detach_job` and `Event::JobFinished`; the TS/Python
clients are generated (`cargo run -p aho-sdk -- gen`).

The daemon (`aho serve`) answers `session.abort{session, scope?, op?}`, `job.list{session}`,
`job.kill{session, op}`, `job.detach{session, op}` and pushes `job.finished{session, op}`;
`aho rpc` bridges them 1:1. Abort, kill and detach need the connection that drives the session
(the same owner check as `session.steer`); `job.list` only reads, so a watcher may call it.

## TUI

| key | default | does |
|---|---|---|
| `tui.keys.abort_turn` | `"esc"` | aborts the running turn (`turn`); jobs keep running |
| `tui.keys.abort_all` | `"ctrl+c"` | aborts the turn and every job (`all`), anywhere, even over the card or an overlay |

A value is `[ctrl+][alt+][shift+]<key>`: a named key (`esc`, `enter`, `tab`, `backspace`,
`delete`, `insert`, arrows, `home`, `end`, `pageup`, `pagedown`, `space`), `f1`–`f24`, or one
character (shift only with named keys); anything else refuses to start and names the key. The
abort-turn key acts last: on the approval card Esc cancels the card, with an overlay it closes
it, with the `/` menu open it dismisses the menu; only when none used it and a turn runs does it
abort the turn. Idle with nothing open, Esc does nothing. Notices say what was cancelled
("abort: this turn's running operations are being cancelled; N background job(s) keep running",
"abort all: the turn and N background job(s) are being cancelled"). Hints and `/help` name the
configured keys.

While a job runs, a panel sits between the transcript and the prompt: a header (`jobs · N
running`) and one row per running job, `op<N> · <tool> · <elapsed> · <label>` (label from the
call's `label`, `description`, `name`, `agent` or `command`), at most 4 rows plus `…and K more`;
hidden when none runs. `aho tui --headless` reports it as `jobs: {running, rows}` (null when
hidden). `/jobs` lists every op with a scope column, `turn` (cancelled with its turn) or `job`,
next to its status. The list refreshes on `OpDetached`, `JobDelivered`, `JobFinished` and ticks.

## Web

The jobs tab (`POST /api/session/jobs`) lists every op in the session's op table, oldest first:
`op, call, tool, kind, scope, state, status, started`. `scope` is `job` for a detached op and
`turn` otherwise; `state` is `running` until it finishes, then `done | failed | cancelled |
uncertain` (`status` is the raw `OpStatus`). Rows carry a `job`/`turn` badge. The tab reloads on
`op_started`, `op_detached`, `op_done`, `job_delivered`, `tool_result` entries, on the mux's
`job_finished{op}` frame, and every 2 s while open.

## CLI

```
aho jobs [--session ID] [--json] list
aho jobs kill <op>        # op7 or 7
aho jobs detach <op>
```

When the daemon holds the session the verbs go through it; `kill`/`detach` attach for the
command only and are refused while another client drives the session there. A session another
process drives is refused (its own `/jobs` shows them). In a session nothing runs, no job runs:
`list` folds the log (open jobs are re-run or delivered when it next starts, `live: false`) and
`kill`/`detach` have nothing to act on. `aho attach`'s Ctrl+C sends the default (`turn`) abort.

## ACP

`session/cancel` (and a dropped prompt call) is `abort()`: scope `turn`. Jobs keep running.
