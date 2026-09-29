//! Telemetry flushes against a local HTTP sink through world-host's real egress.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aho_cap::BuildError;
use aho_egress::{EgressHandle, HostList};
use aho_log::{EntryKind, LogStore, MemoryLog, ToolOutcome};
use aho_types::{CallId, Consequence, Principal, Timestamp, TurnId, Value};
use telemetry::{EnvProbe, LATCH_FILE, Telemetry, TelemetryConfig, TelemetryDeps, payload};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use world_host::HostEgress;

/// `(request head, request body)` per accepted exchange.
type Captured = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

struct Sink {
    port: u16,
    hits: Arc<AtomicUsize>,
    requests: Captured,
}

/// Local HTTP/1.1 sink answering every request with `status` and an empty body.
async fn sink(status: &'static str) -> Sink {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (h, r) = (hits.clone(), requests.clone());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            h.fetch_add(1, Ordering::SeqCst);
            let r = r.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok()).flatten()
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                r.lock().await.push((head, buf[head_end..].to_vec()));
                let resp = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                sock.write_all(resp.as_bytes()).await.ok();
                sock.shutdown().await.ok();
            });
        }
    });
    Sink { port, hits, requests }
}

fn probe() -> EnvProbe {
    EnvProbe { do_not_track: false, is_ci: false, ran_from_cargo: true }
}

fn deps(log: &Arc<MemoryLog>, home: &tempfile::TempDir) -> TelemetryDeps {
    TelemetryDeps {
        egress: EgressHandle(Arc::new(HostEgress::new().unwrap())),
        log: Some(log.clone() as Arc<dyn LogStore>),
        home: Some(home.path().display().to_string()),
    }
}

fn cfg(port: u16, queue_max: usize) -> TelemetryConfig {
    TelemetryConfig { endpoint: Some(format!("http://127.0.0.1:{port}/ingest")), queue_max, hosts: HostList::default() }
}

/// One completed model turn with `tools` tool calls; tool names are deliberately third-party.
fn turn(log: &MemoryLog, turn_no: u64, tools: usize) {
    let by = Principal::system("test");
    log.append(None, by.clone(), EntryKind::TurnStart { turn: TurnId(turn_no) }).unwrap();
    for i in 0..tools {
        let call = CallId(format!("t{turn_no}c{i}"));
        log.append(
            None,
            by.clone(),
            EntryKind::ToolCall {
                call: call.clone(),
                tool: format!("acme_secret_tool_{i}"),
                args: Value::Null,
                class: Consequence::ReadOnly,
            },
        )
        .unwrap();
        log.append(
            None,
            by.clone(),
            EntryKind::ToolResult {
                call,
                outcome: ToolOutcome::Ok,
                value: Value::Null,
                content: Vec::new(),
                spill_ref: None,
                meta: None,
                late: false,
            },
        )
        .unwrap();
    }
    log.append(None, by, EntryKind::TurnEnd { turn: TurnId(turn_no) }).unwrap();
}

fn branch_events(log: &MemoryLog) -> Vec<aho_inspect::Event> {
    aho_inspect::events(&log.read_branch(log.head().unwrap()).unwrap())
}

fn collect_strings(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        serde_json::Value::Object(m) => m.iter().for_each(|(k, x)| {
            out.push(k.clone());
            collect_strings(x, out);
        }),
        _ => {}
    }
}

#[tokio::test]
async fn a_turn_end_flush_posts_exactly_the_derived_payload_and_only_vocabulary_tokens() {
    let s = sink("200 OK").await;
    let home = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::new());
    turn(&log, 1, 2);
    let t = Telemetry::with_probe(deps(&log, &home), cfg(s.port, 256), probe()).unwrap();

    t.flush(false).await;
    assert_eq!(s.hits.load(Ordering::SeqCst), 1);

    let (head, body) = s.requests.lock().await[0].clone();
    assert!(head.starts_with("POST /ingest"), "{head}");
    assert!(head.to_ascii_lowercase().contains("content-type: application/json"), "{head}");

    let got: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let expected_events = branch_events(&log);
    assert_eq!(expected_events.len(), 3, "two tool_finished + one turn_ended");
    let mut expected = payload(&aho_inspect::envelope(Timestamp::now(), false, true), &expected_events);
    expected["envelope"]["hw"] = got["envelope"]["hw"].clone();
    assert_eq!(got, expected, "the wire payload is exactly payload(envelope, events)");

    // Privacy: every string in the events — tags, field names, values — is vocabulary; the
    // third-party tool name `acme_secret_tool_*` is unrepresentable, not filtered.
    let vocab = aho_inspect::vocab();
    let mut strings = Vec::new();
    collect_strings(&got["events"], &mut strings);
    assert!(!strings.is_empty());
    for s in &strings {
        assert!(vocab.contains(&s.as_str()), "`{s}` is not in the closed vocabulary");
    }

    // Nothing new since the last flush: no second request is made.
    t.flush(false).await;
    assert_eq!(s.hits.load(Ordering::SeqCst), 1);

    // A session-end flush over fresh entries appends the whole-branch snapshot.
    turn(&log, 2, 0);
    t.flush(true).await;
    assert_eq!(s.hits.load(Ordering::SeqCst), 2);
    let (_, body) = s.requests.lock().await[1].clone();
    let got: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let events = got["events"].as_array().unwrap();
    assert_eq!(events.last().unwrap()["e"], "session_snapshot");
}

#[tokio::test]
async fn a_4xx_latches_the_endpoint_and_nothing_is_sent_again() {
    let s = sink("404 Not Found").await;
    let home = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::new());
    turn(&log, 1, 1);
    let t = Telemetry::with_probe(deps(&log, &home), cfg(s.port, 256), probe()).unwrap();

    t.flush(false).await;
    assert_eq!(s.hits.load(Ordering::SeqCst), 1);
    assert!(t.latched());
    let latch = std::fs::read_to_string(home.path().join(LATCH_FILE)).unwrap();
    assert_eq!(latch, format!("http://127.0.0.1:{}/ingest", s.port));

    turn(&log, 2, 1);
    t.flush(false).await;
    assert_eq!(s.hits.load(Ordering::SeqCst), 1, "a latched endpoint is never contacted again");

    // A fresh build against the same endpoint honors the latch from the very start.
    let again = Telemetry::with_probe(deps(&log, &home), cfg(s.port, 256), probe()).unwrap();
    assert!(again.latched());
    again.flush(true).await;
    assert_eq!(s.hits.load(Ordering::SeqCst), 1);

    // Pointing telemetry somewhere else clears the way: the old latch no longer matches.
    let s2 = sink("200 OK").await;
    let moved = Telemetry::with_probe(deps(&log, &home), cfg(s2.port, 256), probe()).unwrap();
    assert!(!moved.latched());
}

#[tokio::test]
async fn do_not_track_refuses_the_build_without_touching_the_process_environment() {
    let s = sink("200 OK").await;
    let home = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::new());
    let refused = Telemetry::with_probe(deps(&log, &home), cfg(s.port, 256), EnvProbe { do_not_track: true, ..EnvProbe::default() });
    match refused {
        Err(BuildError::Unsupported(m)) => assert!(m.contains("DO_NOT_TRACK"), "{m}"),
        other => panic!("expected Unsupported, got {:?}", other.err()),
    }
    assert_eq!(s.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_queue_never_exceeds_queue_max_and_drops_the_oldest_first() {
    let s = sink("200 OK").await;
    let home = tempfile::tempdir().unwrap();
    let log = Arc::new(MemoryLog::new());
    turn(&log, 1, 5); // five tool_finished + one turn_ended = six events
    let t = Telemetry::with_probe(deps(&log, &home), cfg(s.port, 3), probe()).unwrap();

    t.flush(false).await;
    assert_eq!(s.hits.load(Ordering::SeqCst), 1);
    let (_, body) = s.requests.lock().await[0].clone();
    let got: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let all = branch_events(&log);
    assert_eq!(all.len(), 6);
    let expected = serde_json::to_value(&all[3..]).unwrap();
    assert_eq!(got["events"], expected, "the newest queue_max events survive, oldest are dropped");
}
