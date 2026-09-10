//! The ssh world's egress: requests leave from the remote (a real sshd on loopback, see `rig`)
//! with the remote's `curl`, against a local tokio HTTP server. Skips with a printed reason when
//! no sshd can be started or the remote has no curl.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod rig;

use std::sync::Arc;

use aho_egress::{Egress, EgressError, HostList, HttpRequest, Limits, Method};
use rig::Rig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use world_ssh::SshWorld;

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Paths: `/hello`, `/redirect` → `/hello`, `/cross` → `http://localhost:<port>/auth`, `/auth`
/// (echoes Authorization), `/big` (100 000 bytes), `/echo` (echoes method + body), `/loop`.
async fn server() -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let mut lines = head.lines();
                let mut first = lines.next().unwrap().split_whitespace();
                let (method, path) = (first.next().unwrap().to_string(), first.next().unwrap().to_string());
                let headers: Vec<(String, String)> =
                    lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
                let len: usize = headers.iter().find(|(k, _)| k == "content-length").map(|(_, v)| v.parse().unwrap()).unwrap_or(0);
                let mut body = buf[head_end + 4..].to_vec();
                while body.len() < len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                log.lock().await.push(Seen { method: method.clone(), path: path.clone(), headers: headers.clone(), body: body.clone() });
                let auth = headers.iter().find(|(k, _)| k == "authorization").map(|(_, v)| v.clone()).unwrap_or_else(|| "none".into());
                let (status, extra, payload): (&str, String, Vec<u8>) = match path.as_str() {
                    "/hello" => ("200 OK", "X-Test: yes\r\n".into(), b"hello world".to_vec()),
                    "/redirect" => ("302 Found", "Location: /hello\r\n".into(), Vec::new()),
                    "/cross" => ("302 Found", format!("Location: http://localhost:{port}/auth\r\n"), Vec::new()),
                    "/auth" => ("200 OK", String::new(), auth.into_bytes()),
                    "/big" => ("200 OK", String::new(), vec![b'x'; 100_000]),
                    "/echo" => ("200 OK", format!("X-Method: {method}\r\n"), body),
                    "/loop" => ("302 Found", "Location: /loop\r\n".into(), Vec::new()),
                    _ => ("404 Not Found", String::new(), b"nope".to_vec()),
                };
                let head = format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", payload.len());
                let _ = sock.write_all(head.as_bytes()).await;
                if method != "HEAD" {
                    let _ = sock.write_all(&payload).await;
                }
                let _ = sock.shutdown().await;
            });
        }
    });
    (port, seen)
}

fn remote_has_curl() -> bool {
    std::process::Command::new("curl").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

async fn setup() -> Option<(Rig, SshWorld, u16, Arc<Mutex<Vec<Seen>>>)> {
    if !remote_has_curl() {
        eprintln!("skipping: the (loopback) remote has no curl");
        return None;
    }
    let rig = Rig::start()?;
    let w = SshWorld::connect(rig.config("ws")).unwrap();
    let (port, seen) = server().await;
    Some((rig, w, port, seen))
}

#[tokio::test]
async fn a_get_follows_redirects_from_the_remote_and_counts_them() {
    let Some((_rig, w, port, seen)) = setup().await else { return };
    let r = w.egress().fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/redirect")).header("X-Plain", "v")).await.unwrap();
    assert_eq!((r.status, r.body.as_slice(), r.redirects, r.truncated), (200, &b"hello world"[..], 1, false));
    assert_eq!(r.final_url, format!("http://127.0.0.1:{port}/hello"));
    assert_eq!(r.header("x-test"), Some("yes"));
    let log = seen.lock().await.clone();
    assert_eq!(log.iter().map(|s| s.path.as_str()).collect::<Vec<_>>(), vec!["/redirect", "/hello"], "curl never followed on its own");
    assert!(log.iter().all(|s| s.headers.iter().any(|(k, v)| k == "x-plain" && v == "v")));
    assert!(log.iter().all(|s| !s.headers.iter().any(|(k, _)| k == "expect")));
    assert_eq!(w.egress().enforced_by(), "remote host");
}

#[tokio::test]
async fn secret_headers_stay_with_their_host_and_the_host_list_is_checked_per_hop() {
    let Some((_rig, w, port, seen)) = setup().await else { return };
    let same = w.egress().fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/auth")).secret_header("Authorization", "Bearer s3cret"));
    assert_eq!(same.await.unwrap().body, b"Bearer s3cret");
    let cross =
        w.egress().fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/cross")).secret_header("Authorization", "Bearer s3cret"));
    let r = cross.await.unwrap();
    assert_eq!((r.body.as_slice(), r.redirects), (&b"none"[..], 1), "dropped when the redirect left 127.0.0.1");

    let before = seen.lock().await.len();
    let deny = HostList::new(vec![], vec!["localhost".into()]).unwrap();
    let r = w.egress().fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/cross")).hosts(deny.clone())).await;
    assert!(matches!(r, Err(EgressError::Denied(_))), "{r:?}");
    let paths: Vec<String> = seen.lock().await[before..].iter().map(|s| s.path.clone()).collect();
    assert_eq!(paths, vec!["/cross".to_string()], "the denied hop was never sent");
    let r = w.egress().fetch(HttpRequest::get(format!("http://localhost:{port}/hello")).hosts(deny)).await;
    assert!(matches!(r, Err(EgressError::Denied(_))));
}

#[tokio::test]
async fn bodies_are_capped_posts_are_byte_exact_and_limits_hold() {
    let Some((_rig, w, port, seen)) = setup().await else { return };
    let limits = Limits { max_body_bytes: 1000, ..Limits::default() };
    let r = w.egress().fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/big")).limits(limits)).await.unwrap();
    assert_eq!((r.body.len(), r.truncated), (1000, true));

    let payload: Vec<u8> = (0u8..=255).cycle().take(70_000).collect();
    let mut req = HttpRequest::get(format!("http://127.0.0.1:{port}/echo")).header("Content-Type", "application/octet-stream");
    req.method = Method::Post;
    req.body = Some(payload.clone());
    let r = w.egress().fetch(req).await.unwrap();
    assert_eq!((r.header("x-method"), r.body == payload), (Some("POST"), true));
    let posted = seen.lock().await.last().cloned().unwrap();
    assert_eq!((posted.method.as_str(), posted.body == payload), ("POST", true));
    assert!(posted.headers.iter().any(|(k, v)| k == "content-type" && v == "application/octet-stream"));

    let mut head = HttpRequest::get(format!("http://127.0.0.1:{port}/hello"));
    head.method = Method::Head;
    let r = w.egress().fetch(head).await.unwrap();
    assert_eq!((r.status, r.body.len(), r.header("x-test")), (200, 0, Some("yes")));

    let limits = Limits { max_redirects: 2, ..Limits::default() };
    let r = w.egress().fetch(HttpRequest::get(format!("http://127.0.0.1:{port}/loop")).limits(limits)).await;
    assert!(matches!(r, Err(EgressError::TooManyRedirects { limit: 2, .. })), "{r:?}");
    let r = w.egress().fetch(HttpRequest::get("ftp://127.0.0.1/x")).await;
    assert!(matches!(r, Err(EgressError::InvalidUrl { .. })), "{r:?}");
    let r = w.egress().fetch(HttpRequest::get("http://127.0.0.1:1/unreachable")).await;
    assert!(matches!(r, Err(EgressError::Connect { .. })), "{r:?}");
}
