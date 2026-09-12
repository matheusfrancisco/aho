//! The per-child loop: find the transcript, tail it, mirror what it says, and watch the tmux
//! pane for exit, disappearance, and a frozen screen (the CLI waiting on its own dialog).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use aho_types::StopReason;
use serde_json::{Value, json};

use crate::event::{Event, Normalized, clip};
use crate::mirror::{self, TEXT_MAX, TOOL_MAX};
use crate::provider::{Child, Cli, Engine, LocateCtx, Status, Track, lock};
use crate::tail::Tail;

/// Pane checks (list-sessions, capture-pane) run at most this often.
pub const CHECK_EVERY: Duration = Duration::from_secs(1);
/// Lines of the screen kept in a `Blocked` entry and diagnostic.
pub const SCREEN_LINES: usize = 40;

/// Starts the loop for `c` unless it runs already, the child is finished, or there is no
/// tokio runtime (it is then started by the next async call).
pub(crate) fn ensure<C: Cli>(eng: &Arc<Engine<C>>, c: &Arc<Child>) {
    if matches!(lock(&c.track).status, Status::Closed | Status::Uncertain) {
        return;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
    if c.driving.swap(true, Ordering::SeqCst) {
        return;
    }
    let (eng, c) = (eng.clone(), c.clone());
    rt.spawn(async move {
        let poll = Duration::from_millis(eng.settings.poll_ms.max(10));
        while step(&eng, &c).await {
            tokio::time::sleep(poll).await;
        }
        c.driving.store(false, Ordering::SeqCst);
    });
}

/// One tick; false once the child is finished.
async fn step<C: Cli>(eng: &Arc<Engine<C>>, c: &Arc<Child>) -> bool {
    if matches!(lock(&c.track).status, Status::Closed | Status::Uncertain) {
        return false;
    }
    attach(eng, c);
    pump(eng, c);

    let due = {
        let t = lock(&c.track);
        t.last_check.is_none_or(|at| at.elapsed() >= CHECK_EVERY)
    };
    if due {
        lock(&c.track).last_check = Some(Instant::now());
        match eng.tmux.session(&c.session).await {
            Ok(None) => {
                let mut t = lock(&c.track);
                t.missing_scans += 1;
                if t.missing_scans >= 2 {
                    drop(t);
                    pump(eng, c);
                    let mut t = lock(&c.track);
                    let reason = format!("its tmux session `{}` is gone (missing on two scans)", c.session);
                    eng.close_child(c, &mut t, StopReason::Error, &reason, None);
                    return false;
                }
            }
            Ok(Some(s)) => {
                let recycled = {
                    let t = lock(&c.track);
                    t.created.is_some_and(|created| created != s.created)
                };
                if recycled {
                    let mut t = lock(&c.track);
                    t.missing_scans += 1;
                    if t.missing_scans >= 2 {
                        let reason = format!("tmux session `{}` now belongs to a different process; this child is gone", c.session);
                        eng.close_child(c, &mut t, StopReason::Error, &reason, None);
                        return false;
                    }
                } else if s.dead {
                    pump(eng, c);
                    let screen = eng.tmux.capture(&c.session).await.unwrap_or_default();
                    let cleanup = eng.tmux.kill(&c.session).await;
                    let mut t = lock(&c.track);
                    if let Err(e) = cleanup {
                        t.errors.push(e);
                    }
                    let status = s.dead_status.map(|x| x.to_string()).unwrap_or_else(|| "unknown".into());
                    let mut reason = format!("{} exited (status {status})", C::PRODUCT);
                    if c.running() && !screen.is_empty() {
                        reason.push_str(&format!(". Its last screen:\n{}", last_lines(&screen, SCREEN_LINES)));
                    }
                    eng.close_child(c, &mut t, StopReason::Error, &reason, s.dead_status);
                    return false;
                } else {
                    lock(&c.track).missing_scans = 0;
                    watch_screen(eng, c).await;
                }
            }
            Err(e) => {
                let mut t = lock(&c.track);
                if t.io_error.as_deref() != Some(e.as_str()) {
                    t.io_error = Some(e.clone());
                    eng.record(mirror::NOTE, c.id, json!({"reason": format!("tmux check failed: {e}")}), &mut t);
                }
            }
        }
    }

    let mut t = lock(&c.track);
    if let Some(at) = t.interrupt_at
        && at.elapsed() >= Duration::from_millis(eng.settings.interrupt_grace_ms)
        && c.running()
    {
        let why = format!(
            "aho pressed Escape, but {} recorded no interrupt within {} ms, so the run is closed here; it may still be working (attach: {})",
            C::PRODUCT,
            eng.settings.interrupt_grace_ms,
            eng.tmux.attach_hint(&c.session)
        );
        eng.finish_run(c, &mut t, StopReason::Aborted, Some(why));
    }
    true
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Finds the transcript while the child is launching.
fn attach<C: Cli>(eng: &Arc<Engine<C>>, c: &Arc<Child>) {
    let (launching, cli_session) = {
        let t = lock(&c.track);
        (t.tail.is_none() && t.status == Status::Launching, t.cli_session.clone())
    };
    if !launching {
        return;
    }
    let claimed = eng.claimed(c.id);
    let ctx = LocateCtx { cwd: &c.cwd, cli_session: cli_session.as_deref(), launched_ms: c.launched_ms, claimed: &claimed };
    let Some(path) = eng.cli.locate(&ctx) else { return };
    // Identity is recorded so a later replacement of the file is noticed, also after a crash.
    let ident = crate::tail::file_ident(&path);
    let mut t = lock(&c.track);
    t.status = Status::Live;
    let body = json!({
        "transcript": path.display().to_string(),
        "ident": ident.map(|(d, i)| json!([d, i])),
        "cli_session": cli_session,
        "offset": 0,
    });
    eng.record(mirror::ATTACHED, c.id, body, &mut t);
    t.tail = Some(Tail::resume(path, 0, ident));
}

/// Reads new transcript lines and mirrors them. Synchronous: holds the child's lock throughout.
pub(crate) fn pump<C: Cli>(eng: &Engine<C>, c: &Child) {
    let mut t = lock(&c.track);
    let Some(tail) = t.tail.as_mut() else { return };
    let read = match tail.poll() {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("could not read the transcript {} ({e})", tail.path().display());
            if t.io_error.as_deref() != Some(msg.as_str()) {
                t.io_error = Some(msg.clone());
                eng.record(mirror::NOTE, c.id, json!({"reason": msg}), &mut t);
            }
            return;
        }
    };
    let ident = tail.ident();
    if let Some(r) = &read.reset {
        let body = json!({"reason": r.describe(), "offset": 0, "ident": ident.map(|(d, i)| json!([d, i]))});
        eng.record(mirror::NOTE, c.id, body, &mut t);
    }
    if !read.lines.is_empty() {
        t.quiet_since = Instant::now();
        t.blocked = false;
    }
    for line in read.lines {
        let rec = eng.cli.normalize(&line.bytes);
        match rec.out {
            Normalized::Events(events) => {
                for e in events {
                    apply(eng, c, &mut t, e, line.end);
                }
            }
            Normalized::Ignored(_) => {}
            Normalized::Unknown => {
                let reason = format!(
                    "{} wrote a `{}` record that no recorded session of it has shown; it was not mirrored (the provider may need new fixtures)",
                    C::PRODUCT,
                    rec.kind
                );
                eng.record(mirror::NOTE, c.id, json!({"reason": reason, "offset": line.end}), &mut t);
            }
            Normalized::Malformed(why) => {
                eng.record(mirror::NOTE, c.id, json!({"reason": format!("unreadable transcript line: {why}"), "offset": line.end}), &mut t);
            }
        }
    }
}

fn clipped_value(v: Value) -> Value {
    let s = v.to_string();
    if s.len() <= TOOL_MAX { v } else { json!({"truncated": true, "preview": clip(&s, TOOL_MAX).0}) }
}

fn apply<C: Cli>(eng: &Engine<C>, c: &Child, t: &mut Track, e: Event, offset: u64) {
    match e {
        Event::Session { version, .. } => t.version = version.or(t.version.take()),
        Event::TurnStarted => {}
        Event::UserPrompt => match t.unconfirmed.pop_front() {
            Some(seq) => eng.record(mirror::DELIVERED, c.id, json!({"seq": seq, "offset": offset}), t),
            None => eng.record(
                mirror::NOTE,
                c.id,
                json!({"reason": "a prompt aho did not send was entered in the pane (someone attached to its tmux session)", "offset": offset}),
                t,
            ),
        },
        Event::AssistantText { text } => {
            let (kept, truncated) = clip(&text, TEXT_MAX);
            eng.record(mirror::MESSAGE, c.id, json!({"text": kept, "truncated": truncated, "offset": offset}), t);
            t.last_text = Some(text);
        }
        Event::ToolUse { id, name, input } => {
            eng.record(mirror::TOOL_USE, c.id, json!({"id": id, "name": name, "input": clipped_value(input), "offset": offset}), t);
        }
        Event::ToolResult { id, is_error, content } => {
            let (kept, truncated) = clip(&content, TOOL_MAX);
            let body = json!({"id": id, "is_error": is_error, "content": kept, "truncated": truncated, "offset": offset});
            eng.record(mirror::TOOL_RESULT, c.id, body, t);
        }
        Event::TurnEnd { stop, last_message } => {
            if let Some(m) = last_message.filter(|m| !m.trim().is_empty()) {
                t.last_text = Some(m);
            }
            // A turn that ended after an error it did not retry failed, whatever the marker says.
            let stop = if stop == StopReason::EndTurn && !t.errors.is_empty() { StopReason::Error } else { stop };
            let body =
                json!({"stop": stop, "last_message": t.last_text.as_deref().map(|m| clip(m, TEXT_MAX).0), "offset": offset});
            eng.record(mirror::TURN_END, c.id, body, t);
            eng.finish_run(c, t, stop, None);
        }
        Event::Error { message, retrying } => {
            let (kept, _) = clip(&message, mirror::DIAGNOSTIC_MAX / 2);
            eng.record(mirror::ERROR, c.id, json!({"message": kept, "retrying": retrying, "offset": offset}), t);
            if !retrying {
                t.errors.push(kept);
            }
        }
    }
}

/// A run whose transcript has been silent for `blocked_after_ms` while the screen has not
/// changed for half that is returned as blocked, with the screen, so the parent can answer.
async fn watch_screen<C: Cli>(eng: &Arc<Engine<C>>, c: &Arc<Child>) {
    let after = Duration::from_millis(eng.settings.blocked_after_ms);
    let silent = {
        let t = lock(&c.track);
        if t.blocked || !c.running() || t.interrupt_at.is_some() {
            return;
        }
        t.quiet_since.elapsed()
    };
    if silent < after / 2 {
        return;
    }
    let screen = match eng.tmux.capture(&c.session).await {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut t = lock(&c.track);
    if t.screen.as_deref() != Some(screen.as_str()) {
        t.screen = Some(screen);
        t.screen_since = Instant::now();
        return;
    }
    if t.quiet_since.elapsed() < after || t.screen_since.elapsed() < after / 2 || !c.running() {
        return;
    }
    t.blocked = true;
    let shown = last_lines(&screen, SCREEN_LINES);
    let what = if t.tail.is_none() { "has not written its transcript yet" } else { "has written nothing to its transcript" };
    let reason = format!(
        "{} {what} for {} s and its screen is not changing; it is probably waiting on one of its own dialogs",
        C::PRODUCT,
        t.quiet_since.elapsed().as_secs()
    );
    eng.record(mirror::BLOCKED, c.id, json!({"reason": reason, "screen": shown}), &mut t);
    let diag = format!(
        "{reason}. Its screen:\n{shown}\nAnswer with send_message (the text is typed into its prompt, then Enter), or stop it with interrupt_agent."
    );
    eng.finish_run(c, &mut t, StopReason::ToolUse, Some(diag));
}
