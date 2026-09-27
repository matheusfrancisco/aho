//! `POST /github`: size fence → signature → headers → subscription → dedupe → queue. The
//! response never waits on the model: an accepted delivery answers 202 and a single worker hands
//! prompts to the sink in arrival order. Log lines carry status, event, delivery id, and peer,
//! never the body or a header value that failed validation.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aho_policy::Untrusted;
use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;

use crate::github::{self, DELIVERY_HEADER, EVENT_HEADER, SIGNATURE_HEADER};
use crate::tls::Peer;
use crate::{PromptSink, WebhookOptions, WebhookPrompt, template};

/// Delivery ids remembered in memory; the session log's `client_key` covers older ones.
const RECENT_DELIVERIES: usize = 4096;
/// Accepted deliveries waiting for the sink; beyond this GitHub gets 503 and may redeliver.
pub const QUEUE_DEPTH: usize = 64;
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

pub const PATH: &str = "/github";

pub struct AppState {
    pub opts: WebhookOptions,
    recent: Mutex<Recent>,
    queue: mpsc::Sender<WebhookPrompt>,
}

impl AppState {
    pub fn new(opts: WebhookOptions, queue: mpsc::Sender<WebhookPrompt>) -> AppState {
        AppState { opts, recent: Mutex::new(Recent::default()), queue }
    }

    fn recent(&self) -> std::sync::MutexGuard<'_, Recent> {
        // A panic while holding this lock cannot leave the set half-updated in a way that matters.
        self.recent.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Default)]
struct Recent {
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl Recent {
    /// `false` when `id` was already accepted.
    fn insert(&mut self, id: &str) -> bool {
        if !self.seen.insert(id.to_string()) {
            return false;
        }
        self.order.push_back(id.to_string());
        if self.order.len() > RECENT_DELIVERIES
            && let Some(old) = self.order.pop_front()
        {
            self.seen.remove(&old);
        }
        true
    }

    /// A delivery that never reached the session may be redelivered.
    fn forget(&mut self, id: &str) {
        if self.seen.remove(id) {
            self.order.retain(|x| x != id);
        }
    }
}

pub fn router(app: Arc<AppState>) -> axum::Router {
    axum::Router::new().route(PATH, post(github_hook)).with_state(app)
}

/// Hands queued prompts to the sink one at a time. A failed delivery is forgotten by the
/// dedupe set so GitHub's "Redeliver" button works.
pub fn spawn_worker(app: Arc<AppState>, mut rx: mpsc::Receiver<WebhookPrompt>, sink: Arc<dyn PromptSink>) {
    tokio::spawn(async move {
        while let Some(p) = rx.recv().await {
            let (delivery, event) = (p.delivery.clone(), p.event.clone());
            match sink.deliver(p).await {
                Ok(()) => eprintln!("aho webhook: delivery {delivery} ({event}) prompted the session"),
                Err(e) => {
                    app.recent().forget(&delivery);
                    eprintln!(
                        "aho webhook: delivery {delivery} ({event}) could not prompt the session: {e}. It was dropped; redeliver it from the GitHub hook page once the session is reachable."
                    );
                }
            }
        }
    });
}

fn reply(status: StatusCode, body: serde_json::Value) -> Response {
    (status, Json(body)).into_response()
}

fn refuse(status: StatusCode, peer: SocketAddr, why: &str) -> Response {
    eprintln!("aho webhook: {} from {peer}: {why}", status.as_u16());
    reply(status, json!({ "error": why }))
}

async fn read_body(body: Body, max: usize, declared: Option<u64>) -> Result<Vec<u8>, (StatusCode, String)> {
    let too_large = || {
        (StatusCode::PAYLOAD_TOO_LARGE, format!("the body is larger than {max} bytes (surface.webhook.max_body); nothing was delivered"))
    };
    if declared.is_some_and(|n| n > max as u64) {
        return Err(too_large());
    }
    let collect = async {
        let mut buf = Vec::with_capacity(declared.map_or(0, |n| n as usize));
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| (StatusCode::BAD_REQUEST, "the body could not be read; nothing was delivered".to_string()))?;
            if buf.len() + chunk.len() > max {
                return Err(too_large());
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    };
    tokio::time::timeout(BODY_TIMEOUT, collect)
        .await
        .map_err(|_| (StatusCode::REQUEST_TIMEOUT, "the body did not arrive within 30 s; nothing was delivered".to_string()))?
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

async fn github_hook(State(app): State<Arc<AppState>>, ConnectInfo(Peer(peer)): ConnectInfo<Peer>, h: HeaderMap, body: Body) -> Response {
    let declared = header_str(&h, header::CONTENT_LENGTH.as_str()).and_then(|v| v.parse().ok());
    let body = match read_body(body, app.opts.max_body, declared).await {
        Ok(b) => b,
        Err((status, why)) => return refuse(status, peer, &why),
    };
    if let Err(e) = github::verify_signature(app.opts.secret.expose().as_bytes(), &body, header_str(&h, SIGNATURE_HEADER)) {
        return refuse(StatusCode::UNAUTHORIZED, peer, e.message());
    }
    let Some(event) = header_str(&h, EVENT_HEADER).filter(|e| github::valid_event(e)) else {
        return refuse(StatusCode::BAD_REQUEST, peer, "the X-GitHub-Event header is missing or not an event name; nothing was delivered");
    };
    let Some(delivery) = header_str(&h, DELIVERY_HEADER).filter(|d| github::valid_delivery(d)) else {
        return refuse(StatusCode::BAD_REQUEST, peer, "the X-GitHub-Delivery header is missing or not a GUID; nothing was delivered");
    };
    let payload = match github::payload(header_str(&h, header::CONTENT_TYPE.as_str()), &body) {
        Ok(p) => p,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, peer, &format!("{why}; nothing was delivered")),
    };
    if event == "ping" {
        return reply(StatusCode::OK, json!({ "status": "pong" }));
    }
    let action = payload.get("action").and_then(|a| a.as_str());
    let Some((key, tmpl)) = app.opts.subscription(event, action) else {
        eprintln!("aho webhook: 204 delivery {delivery}: `{event}` is not in surface.webhook.events");
        return StatusCode::NO_CONTENT.into_response();
    };
    if event == "issue_comment" {
        let text = payload.pointer("/comment/body").and_then(|b| b.as_str()).unwrap_or_default();
        if !github::mentions(text, &app.opts.mention) {
            eprintln!("aho webhook: 204 delivery {delivery}: the comment does not mention `{}`", app.opts.mention);
            return StatusCode::NO_CONTENT.into_response();
        }
    }
    if !app.recent().insert(delivery) {
        eprintln!("aho webhook: 200 delivery {delivery} was already accepted; the replay was ignored");
        return reply(StatusCode::OK, json!({ "status": "duplicate", "delivery": delivery }));
    }
    let prompt = WebhookPrompt::new(delivery.to_string(), key.to_string(), Untrusted::new(template::render(tmpl, &payload)));
    if app.queue.try_send(prompt).is_err() {
        app.recent().forget(delivery);
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            peer,
            "the session is behind on webhook prompts; this delivery was not queued. GitHub may redeliver it.",
        );
    }
    eprintln!("aho webhook: 202 delivery {delivery} ({key}) queued");
    reply(StatusCode::ACCEPTED, json!({ "status": "accepted", "delivery": delivery, "event": key }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_forgets_the_oldest_past_capacity_and_on_request() {
        let mut r = Recent::default();
        assert!(r.insert("a"));
        assert!(!r.insert("a"));
        r.forget("a");
        assert!(r.insert("a"), "a forgotten delivery is accepted again");
        for i in 0..RECENT_DELIVERIES {
            assert!(r.insert(&format!("d{i}")));
        }
        assert!(r.insert("a"), "the oldest id fell out of the window");
        assert_eq!(r.order.len(), RECENT_DELIVERIES);
    }
}
