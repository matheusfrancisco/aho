//! One-shot HTTP/1.1 server on a local `TcpListener`: serves a canned response (fixture bytes,
//! written in small pieces so SSE framing straddles TCP reads) and hands back what it received.
#![allow(dead_code)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub struct Canned {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Canned {
    pub fn sse(body: &[u8]) -> Canned {
        Canned { status: 200, content_type: "text/event-stream; charset=utf-8", headers: vec![], body: body.to_vec() }
    }
    pub fn json(status: u16, body: &[u8]) -> Canned {
        Canned { status, content_type: "application/json", headers: vec![], body: body.to_vec() }
    }
    pub fn header(mut self, name: &'static str, value: &str) -> Canned {
        self.headers.push((name, value.to_string()));
        self
    }
}

#[derive(Debug)]
pub struct Captured {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Captured {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Returns the base URL (`http://127.0.0.1:<port>`) and a handle resolving to the request seen.
pub async fn serve_once(canned: Canned) -> (String, JoinHandle<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let handle = tokio::spawn(async move { exchange(&listener, canned).await });
    (base, handle)
}

pub async fn serve_sequence(responses: Vec<Canned>) -> (String, JoinHandle<Vec<Captured>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for canned in responses {
            requests.push(exchange(&listener, canned).await);
        }
        requests
    });
    (base, handle)
}

async fn exchange(listener: &TcpListener, canned: Canned) -> Captured {
    let (mut sock, _) = listener.accept().await.expect("accept");
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let n = sock.read(&mut tmp).await.expect("read");
        assert!(n > 0, "client closed before sending headers");
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split(' ');
    let method = request_line.next().unwrap_or("").to_string();
    let path = request_line.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    while buf.len() < head_end + len {
        let n = sock.read(&mut tmp).await.expect("read body");
        assert!(n > 0, "client closed mid-body");
        buf.extend_from_slice(&tmp[..n]);
    }
    let body: Value = serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(Value::Null);

    let mut response = format!("HTTP/1.1 {} X\r\ncontent-type: {}\r\nconnection: close\r\n", canned.status, canned.content_type);
    for (k, v) in &canned.headers {
        response.push_str(&format!("{k}: {v}\r\n"));
    }
    response.push_str("\r\n");
    sock.write_all(response.as_bytes()).await.expect("write head");
    for piece in canned.body.chunks(61) {
        sock.write_all(piece).await.expect("write body");
        sock.flush().await.expect("flush");
    }
    sock.shutdown().await.expect("shutdown");
    Captured { method, path, headers, body }
}
