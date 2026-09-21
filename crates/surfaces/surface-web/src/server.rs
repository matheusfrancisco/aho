//! Router, middleware and handlers. `POST /api/<ns>/<method>` maps 1:1 onto the `Session` API
//! and the `aho-inspect` projections; `/api/mux` is the event stream (see `mux`);
//! `/api/session/command` runs the app's slash commands.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aho_approval::{ApprovalRequest, Decided};
use aho_compose::Planned;
use aho_log::GrantScope;
use aho_policy::Operator;
use aho_session::{Session, SessionError};
use aho_types::*;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use surface_tui::{Commands, Parsed, Reply, parse_line};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::auth::{self, COOKIE, Fence};
use crate::views;

pub const INDEX_HTML: &str = include_str!("../assets/index.html");
pub const APP_JS: &str = include_str!("../assets/app.js");
pub const APP_CSS: &str = include_str!("../assets/app.css");

pub struct Pending {
    pub view: Value,
    pub class: Consequence,
    pub preset_cut: Option<String>,
    pub reply: oneshot::Sender<Decided>,
}

pub struct AppState {
    pub session: Arc<Session>,
    pub planned: Planned,
    /// The app's slash commands (`/help` is built in).
    pub commands: Commands,
    pub label: String,
    pub token: String,
    pub cookie: String,
    pub exchanged: AtomicBool,
    pub secure: bool,
    pub fence: Fence,
    pub operator: Operator,
    pub pending: Mutex<BTreeMap<u64, Pending>>,
    pub next_card: AtomicU64,
    /// Surface-level frames (`approval`, `approval_resolved`) for every connected mux.
    pub web_events: broadcast::Sender<String>,
}

pub type App = Arc<AppState>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl AppState {
    pub fn pending_frames(&self) -> Vec<String> {
        lock(&self.pending).values().map(|p| views::frame("approval", p.view.clone())).collect()
    }

    fn resolve(&self, id: u64, outcome: &str) {
        let _ = self.web_events.send(views::frame("approval_resolved", json!({ "id": id, "outcome": outcome })));
    }
}

pub fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, axum::Json(json!({ "error": msg.into() }))).into_response()
}

fn ok(v: Value) -> Response {
    axum::Json(v).into_response()
}

fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, Response> {
    let bytes: &[u8] = if body.iter().all(|b| b.is_ascii_whitespace()) { b"{}" } else { body };
    serde_json::from_slice(bytes).map_err(|e| err(StatusCode::BAD_REQUEST, format!("the request body is not the expected JSON: {e}")))
}

/// Owns the approval receiver: every card gets an id, is kept until answered, and is announced to
/// every mux. Cards whose caller stopped waiting are withdrawn.
pub fn spawn_approval_pump(app: App, mut rx: mpsc::Receiver<ApprovalRequest>) {
    tokio::spawn(async move {
        let mut sweep = tokio::time::interval(Duration::from_millis(500));
        let mut open = true;
        loop {
            tokio::select! {
                req = rx.recv(), if open => match req {
                    Some((card, reply)) => {
                        let id = app.next_card.fetch_add(1, Ordering::Relaxed) + 1;
                        let state = app.session.fold().unwrap_or_default();
                        let view = views::approval_json(id, &card, views::tool_cap(app.session.registry(), &card.tool), &state);
                        let preset_cut = view["preset_cut"].as_str().map(str::to_string);
                        lock(&app.pending).insert(id, Pending { view: view.clone(), class: card.class, preset_cut, reply });
                        let _ = app.web_events.send(views::frame("approval", view));
                    }
                    None => open = false,
                },
                _ = sweep.tick() => {
                    let gone: Vec<u64> = lock(&app.pending).iter().filter(|(_, p)| p.reply.is_closed()).map(|(id, _)| *id).collect();
                    for id in gone {
                        lock(&app.pending).remove(&id);
                        app.resolve(id, "withdrawn");
                    }
                    if !open && lock(&app.pending).is_empty() {
                        break;
                    }
                }
            }
        }
    });
}

pub fn router(app: App) -> Router {
    let api = Router::new()
        .route("/api/session/prompt", post(prompt))
        .route("/api/session/steer", post(steer))
        .route("/api/session/command", post(command))
        .route("/api/session/abort", post(abort))
        .route("/api/session/fold", post(fold))
        .route("/api/session/jobs", post(jobs))
        .route("/api/session/rewind", post(rewind))
        .route("/api/caps/tree", post(caps_tree))
        .route("/api/caps/why", post(caps_why))
        .route("/api/caps/inventory", post(caps_inventory))
        .route("/api/approval/pending", post(approval_pending))
        .route("/api/approval/answer", post(approval_answer))
        .route("/api/ui/panels", post(ui_panels))
        .route("/api/mux", get(crate::mux::mux))
        .route_layer(middleware::from_fn_with_state(app.clone(), require_auth));
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(|| async { asset(APP_JS, "text/javascript; charset=utf-8") }))
        .route("/app.css", get(|| async { asset(APP_CSS, "text/css; charset=utf-8") }))
        .route("/auth", get(auth_link))
        .route("/api/auth/exchange", post(auth_exchange))
        .merge(api)
        .fallback(|| async { err(StatusCode::NOT_FOUND, "no such endpoint; the API is POST /api/<ns>/<method> and GET /api/mux") })
        .layer(middleware::from_fn_with_state(app.clone(), fence))
        .with_state(app)
}

fn asset(body: &'static str, ctype: &'static str) -> Response {
    let mut r = Response::new(Body::from(body));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    r
}

async fn index() -> Response {
    let mut r = asset(INDEX_HTML, "text/html; charset=utf-8");
    r.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'"),
    );
    r
}

async fn fence(State(app): State<App>, req: Request, next: Next) -> Response {
    if let Err(why) = app.fence.check(req.headers()) {
        return err(StatusCode::FORBIDDEN, why);
    }
    let mut r = next.run(req).await;
    r.headers_mut().insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    r
}

fn authorized(app: &AppState, h: &HeaderMap) -> bool {
    auth::bearer(h).is_some_and(|t| auth::ct_eq(t, &app.token)) || auth::cookie(h).is_some_and(|c| auth::ct_eq(c, &app.cookie))
}

async fn require_auth(State(app): State<App>, req: Request, next: Next) -> Response {
    if !authorized(&app, req.headers()) {
        return err(
            StatusCode::UNAUTHORIZED,
            "this API needs the process token printed when the server started: send `Authorization: Bearer <token>`, or open the printed /auth link once in this browser",
        );
    }
    next.run(req).await
}

/// The token buys one browser cookie; later exchanges are refused so a leaked link (history,
/// screen share) cannot mint a second session. Bearer auth keeps working for API clients.
fn exchange(app: &AppState, presented: &str) -> Result<HeaderValue, Response> {
    if !auth::ct_eq(presented, &app.token) {
        return Err(err(StatusCode::UNAUTHORIZED, "wrong token; use the one printed when the server started"));
    }
    if app.exchanged.swap(true, Ordering::SeqCst) {
        return Err(err(
            StatusCode::CONFLICT,
            "the token was already exchanged for a browser cookie; use that browser, or restart the server for a new token",
        ));
    }
    let secure = if app.secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!("{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/{secure}", app.cookie))
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "could not build the session cookie"))
}

#[derive(Deserialize)]
struct TokenQuery {
    token: String,
}

async fn auth_link(State(app): State<App>, Query(q): Query<TokenQuery>) -> Response {
    match exchange(&app, &q.token) {
        Ok(cookie) => {
            let mut r = Response::new(Body::empty());
            *r.status_mut() = StatusCode::SEE_OTHER;
            r.headers_mut().insert(header::SET_COOKIE, cookie);
            r.headers_mut().insert(header::LOCATION, HeaderValue::from_static("/"));
            r
        }
        Err(r) => r,
    }
}

async fn auth_exchange(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    #[derive(Deserialize, Default)]
    struct Req {
        token: Option<String>,
    }
    let req: Req = match parse(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let presented = req.token.or_else(|| auth::bearer(&headers).map(str::to_string)).unwrap_or_default();
    match exchange(&app, &presented) {
        Ok(cookie) => {
            let mut r = ok(json!({ "ok": true }));
            r.headers_mut().insert(header::SET_COOKIE, cookie);
            r
        }
        Err(r) => r,
    }
}

// ───────────────────────────── session ─────────────────────────────

async fn prompt(State(app): State<App>, body: Bytes) -> Response {
    #[derive(Deserialize)]
    struct Req {
        text: String,
        client_key: Option<String>,
    }
    let req: Req = match parse(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    if req.text.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "the prompt is empty");
    }
    // The turn runs in its own task: a dropped HTTP connection must not cancel it.
    let s = app.session.clone();
    match tokio::spawn(async move { s.prompt(req.text, req.client_key).await }).await {
        Ok(Ok(t)) => ok(json!({ "transition": t })),
        Ok(Err(SessionError::Busy)) => err(
            StatusCode::CONFLICT,
            "a turn is already running; send this as a steer (POST /api/session/steer) or wait for the idle event",
        ),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("the turn task failed: {e}")),
    }
}

async fn steer(State(app): State<App>, body: Bytes) -> Response {
    #[derive(Deserialize)]
    struct Req {
        text: String,
        #[serde(default)]
        urgent: bool,
    }
    let req: Req = match parse(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    if req.text.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "the steer text is empty");
    }
    app.session.steer(req.text, req.urgent);
    ok(json!({ "queued": true, "urgent": req.urgent }))
}

async fn abort(State(app): State<App>) -> Response {
    app.session.abort();
    ok(json!({ "ok": true }))
}

/// `{"line": "/name args…"}` → `{"command", "output"}`. An unknown name is 404 with the list of
/// commands, a refusal 422; nothing reaches the model either way. Entries the command wrote
/// directly to the log are announced to every mux, since no session event carries them.
async fn command(State(app): State<App>, body: Bytes) -> Response {
    #[derive(Deserialize)]
    struct Req {
        line: String,
    }
    let req: Req = match parse(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let (name, args) = match parse_line(&req.line) {
        Parsed::Command { name, args } => (name.to_string(), args.to_string()),
        Parsed::Prompt(_) => {
            return err(
                StatusCode::BAD_REQUEST,
                "this line does not start with a single `/`, so it is not a command and nothing was run. Send text for the model with POST /api/session/prompt",
            );
        }
    };
    let before = app.session.log().head();
    let run = app.clone();
    let (n, a) = (name.clone(), args);
    let reply = match tokio::task::spawn_blocking(move || run.commands.dispatch(&run.session, &n, &a)).await {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the `/{name}` command task failed ({e}); check the session log for what it did"),
            );
        }
    };
    for e in app.session.subscribe_from(before) {
        if app.web_events.send(views::frame("entry", views::entry_json(&e))).is_err() {
            break; // no tab is connected; each replays from the log when it connects
        }
    }
    match reply {
        Reply::Output(output) => ok(json!({ "command": name, "output": output })),
        Reply::Unknown(why) => (
            StatusCode::NOT_FOUND,
            axum::Json(
                json!({ "error": why, "commands": std::iter::once(surface_tui::HELP).chain(app.commands.names()).collect::<Vec<_>>() }),
            ),
        )
            .into_response(),
        Reply::Failed(why) => err(StatusCode::UNPROCESSABLE_ENTITY, why),
    }
}

fn branch(app: &AppState) -> Result<Vec<aho_log::Entry>, Response> {
    match app.session.log().head() {
        Some(h) => app.session.log().read_branch(h).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        None => Ok(Vec::new()),
    }
}

fn state(app: &AppState) -> Result<aho_log::State, Response> {
    app.session.fold().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("the session log does not fold: {e}")))
}

async fn fold(State(app): State<App>) -> Response {
    let (st, br) = match (state(&app), branch(&app)) {
        (Ok(s), Ok(b)) => (s, b),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    ok(views::fold_json(&app.session, &st, &br))
}

async fn jobs(State(app): State<App>) -> Response {
    ok(json!({ "jobs": views::jobs_json(app.session.ops()) }))
}

async fn rewind(State(app): State<App>, body: Bytes) -> Response {
    #[derive(Deserialize)]
    struct Req {
        checkpoint: u64,
        #[serde(default = "both")]
        mode: String,
        #[serde(default)]
        apply: bool,
    }
    fn both() -> String {
        "both".into()
    }
    let req: Req = match parse(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let Some(mode) = views::parse_mode(&req.mode) else {
        return err(StatusCode::BAD_REQUEST, format!("unknown rewind mode `{}`; use both, chat or code", req.mode));
    };
    let cp = CheckpointId(req.checkpoint);
    if !req.apply {
        return match views::rewind_plan(&app.session, cp, mode).await {
            Ok(plan) => ok(json!({ "plan": plan })),
            Err(e) => err(StatusCode::CONFLICT, e),
        };
    }
    match app.session.rewind(&app.operator, cp, mode).await {
        Ok(entry) => ok(json!({ "entry": entry, "mode": mode })),
        Err(SessionError::NotFound(e)) => err(StatusCode::NOT_FOUND, e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ───────────────────────────── caps ─────────────────────────────

async fn caps_tree(State(app): State<App>) -> Response {
    match state(&app) {
        Ok(st) => ok(views::tree_json(&app.planned, &st, &app.label)),
        Err(r) => r,
    }
}

async fn caps_why(State(app): State<App>, body: Bytes) -> Response {
    #[derive(Deserialize)]
    struct Req {
        id: String,
    }
    let req: Req = match parse(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let st = match state(&app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let nodes = views::tree_nodes(&app.planned, &st);
    match aho_inspect::why_in(&nodes, &app.planned.plan, &req.id) {
        Some(w) => ok(json!(w)),
        None => err(StatusCode::NOT_FOUND, format!("no capability `{}` in this plan; POST /api/caps/tree lists them", req.id)),
    }
}

async fn caps_inventory(State(app): State<App>, body: Bytes) -> Response {
    #[derive(Deserialize, Default)]
    struct Req {
        kind: Option<String>,
    }
    let req: Req = match parse(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let st = match state(&app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let nodes = views::tree_nodes(&app.planned, &st);
    let cards = aho_inspect::inventory_in(&nodes, &app.planned.plan, req.kind.as_deref());
    ok(json!({ "text": aho_inspect::render_inventory(&cards), "cards": cards }))
}

// ───────────────────────────── approvals ─────────────────────────────

async fn approval_pending(State(app): State<App>) -> Response {
    let cards: Vec<Value> = lock(&app.pending).values().map(|p| p.view.clone()).collect();
    ok(json!({ "pending": cards }))
}

async fn approval_answer(State(app): State<App>, body: Bytes) -> Response {
    #[derive(Deserialize)]
    struct Req {
        id: u64,
        outcome: String,
        scope: Option<String>,
    }
    let req: Req = match parse(&body) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let scope = match req.scope.as_deref() {
        None | Some("once") => GrantScope::Once,
        Some("session") => GrantScope::Session,
        Some("until_revoked") => GrantScope::UntilRevoked,
        Some(other) => return err(StatusCode::BAD_REQUEST, format!("unknown scope `{other}`; use once, session or until_revoked")),
    };
    let decided = match req.outcome.as_str() {
        "allow" => Decided::allowed(scope),
        "allow_once" => Decided::allowed(GrantScope::Once),
        "allow_session" => Decided::allowed(GrantScope::Session),
        "reject" => Decided::rejected(),
        "cancel" => Decided::cancelled(),
        "preset" => Decided::allowed(GrantScope::Session),
        other => {
            return err(
                StatusCode::BAD_REQUEST,
                format!("unknown outcome `{other}`; use allow, allow_once, allow_session, reject, cancel or preset"),
            );
        }
    };
    let Some(p) = lock(&app.pending).remove(&req.id) else {
        return err(StatusCode::NOT_FOUND, format!("no approval card {} is waiting (already answered or withdrawn)", req.id));
    };
    let mut wrote = Value::Null;
    if req.outcome == "preset" {
        let Some(preset) = p.preset_cut.clone() else {
            let msg = "this card's smallest cut is not a preset change; answer with allow, reject or cancel";
            lock(&app.pending).insert(req.id, p);
            return err(StatusCode::CONFLICT, msg);
        };
        let env = &app.planned.env;
        let file = aho_compose::apply_target(std::path::Path::new(&env.project), std::path::Path::new(&env.home));
        if let Err(e) = aho_compose::apply(&file, "policy.preset", json!(preset)) {
            lock(&app.pending).insert(req.id, p);
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("could not write the preset: {e}; the card is still open"));
        }
        wrote = json!({ "file": file.display().to_string(), "key": "policy.preset", "value": preset, "note": format!("new sessions start with preset {preset}; this session now allows {}", p.class.name()) });
    }
    let label = req.outcome.clone();
    if p.reply.send(decided).is_err() {
        app.resolve(req.id, "withdrawn");
        return err(StatusCode::GONE, "the session stopped waiting for this answer; nothing was decided");
    }
    app.resolve(req.id, &label);
    ok(json!({ "ok": true, "id": req.id, "outcome": label, "wrote": wrote }))
}

async fn ui_panels(State(app): State<App>) -> Response {
    let reg = app.session.registry();
    let mut tools: serde_json::Map<String, Value> = reg.tools.iter().map(|(c, d, _)| (d.name.to_string(), json!(c.name))).collect();
    for (c, spec, _) in &reg.dynamic_tools {
        tools.insert(spec.name.clone(), json!(c.name));
    }
    match state(&app) {
        Ok(st) => ok(json!({ "panels": views::active_slots(&reg.panels, &st), "known": crate::SLOTS, "tools": tools })),
        Err(r) => r,
    }
}

pub fn origin_scheme(tls: bool) -> &'static str {
    if tls { "https" } else { "http" }
}

pub fn display_host(bind: SocketAddr) -> String {
    if bind.ip().is_unspecified() || bind.ip().is_loopback() { format!("127.0.0.1:{}", bind.port()) } else { bind.to_string() }
}
