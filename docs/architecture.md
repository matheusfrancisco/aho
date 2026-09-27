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

1. queued inputs are appended at the request boundary;
2. dynamic contexts are logged as `Snapshot` user messages only when their text changed;
3. the request is **derived from the log**: `derive_messages(fold(log))`, plus the system
   sections and the tool list;
4. the response is appended; tool calls dispatch immediately and run concurrently; results wake
   the next step as they land.

Tool calls are async-first: the model never blocks on a running call; a call still running when
the next request goes out shows a derived placeholder, and its result lands late.

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
