use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::{Engine, InputFuture, LoginEvent, LoginUi};

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl Request {
    pub fn form(&self) -> BTreeMap<String, String> {
        reqwest::Url::parse(&format!("http://localhost/?{}", self.body)).expect("form URL").query_pairs().into_owned().collect()
    }
}

pub struct Server {
    pub base: String,
    handle: JoinHandle<Vec<Request>>,
}

impl Server {
    pub async fn start(responses: impl FnOnce(&str) -> Vec<(u16, Value)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fixture");
        let base = format!("http://{}", listener.local_addr().expect("fixture address"));
        let responses = responses(&base);
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, response) in responses {
                let (mut socket, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
                    .await
                    .expect("fixture request timeout")
                    .expect("accept fixture request");
                let mut bytes = Vec::new();
                let mut chunk = [0; 2048];
                let end = loop {
                    let size = socket.read(&mut chunk).await.expect("read request");
                    assert!(size > 0, "closed before HTTP headers");
                    bytes.extend_from_slice(&chunk[..size]);
                    if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break position + 4;
                    }
                };
                let header = String::from_utf8_lossy(&bytes[..end]).into_owned();
                let mut lines = header.split("\r\n");
                let mut start = lines.next().expect("request line").split(' ');
                let method = start.next().expect("method").into();
                let path = start.next().expect("path").into();
                let headers: BTreeMap<String, String> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().into()))
                    .collect();
                let length = headers.get("content-length").and_then(|value| value.parse::<usize>().ok()).unwrap_or(0);
                while bytes.len() < end + length {
                    let size = socket.read(&mut chunk).await.expect("read body");
                    assert!(size > 0, "closed before HTTP body");
                    bytes.extend_from_slice(&chunk[..size]);
                }
                let body = String::from_utf8(bytes[end..end + length].to_vec()).expect("UTF-8 body");
                requests.push(Request { method, path, headers, body });
                let response = response.to_string();
                let wire = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
                socket.write_all(wire.as_bytes()).await.expect("write response");
                socket.shutdown().await.expect("close response");
            }
            requests
        });
        Self { base, handle }
    }

    pub fn engine(&self, endpoints: &[(&str, &str)]) -> Engine {
        let endpoints = endpoints.iter().map(|(canonical, path)| ((*canonical).into(), format!("{}{path}", self.base))).collect();
        Engine::with_endpoint_overrides(reqwest::Client::builder().no_proxy().build().expect("fixture client"), endpoints)
    }

    pub async fn requests(self) -> Vec<Request> {
        tokio::time::timeout(Duration::from_secs(20), self.handle).await.expect("fixture completion timeout").expect("fixture task")
    }
}

#[derive(Default)]
pub struct Ui(pub Mutex<Vec<LoginEvent>>);

impl LoginUi for Ui {
    fn event(&self, event: LoginEvent) {
        self.0.lock().expect("UI events").push(event);
    }
    fn input<'a>(&'a self, _: &'a str) -> InputFuture<'a> {
        Box::pin(std::future::pending())
    }
}
